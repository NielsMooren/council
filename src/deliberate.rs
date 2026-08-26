//! The deliberation engine.
//!
//! Round shape matters and is not arbitrary:
//!   R1  independent opening positions, NO peer input. Showing peers early
//!       causes premature convergence and destroys the diversity you paid for.
//!   R2+ cross-examination: each member sees ALL positions and must state where
//!       they were wrong and who changed their mind.
//!   Rn  commitment: a decision even when in the minority.
//!   Chair synthesis, explicitly instructed not to fake consensus.

use crate::config::{Config, ResolvedPanel};
use crate::provider::Request;
use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Deliberation {
    pub question: String,
    pub context: Option<String>,
    pub rounds: u8,
    /// The roster to run. Built either from a named config panel or from
    /// runtime model handles - the engine does not care which.
    pub panel: ResolvedPanel,
    /// Per-member token ceiling, overriding config/registry defaults.
    pub max_tokens: Option<u32>,
    /// Reuse cached member responses for an identical question+panel+round.
    pub resume: bool,
}

pub struct Outcome {
    pub transcript: String,
    pub consensus: String,
    pub cache_dir: PathBuf,
    pub failures: Vec<String>,
}

const RULES: &str = "\
You are on a technical review panel. The other members are peers, not an audience.

Rules of engagement:
- You are entitled to your own opinion. Disagree openly when you disagree.
- Be specific and technical. No hedging, no restating the question back.
- Call out anything you believe is WRONG or UNVERIFIED, and say what would settle it.
- Brevity is respected. No padding, no pleasantries.
- Favour what actually works over what sounds good.
- If a peer changes your mind, say so explicitly and credit them. Updating on \
evidence is a win, not a loss.";

fn round_prompt(round: u8, total: u8, transcript: &str) -> String {
    if round == 1 {
        return "ROUND 1 - OPENING POSITION. You have no peer input yet; this is your \
independent read.\n\n\
1. POSITION: your answer, stated plainly.\n\
2. REASONING: why, with the load-bearing assumptions named.\n\
3. STRONGEST OBJECTION to your own position.\n\
4. WHAT WOULD CHANGE YOUR MIND: the specific evidence.\n\n\
Under 600 words."
            .to_string();
    }
    if round == total {
        return format!(
            "FINAL ROUND - COMMITMENT. Full transcript:\n\n{transcript}\n\n\
1. FINAL ANSWER: one paragraph. Commit even if you are in the minority - say so if you are.\n\
2. WHAT CHANGED: what you revised during deliberation, and who convinced you.\n\
3. REMAINING DISAGREEMENT: what you still reject, and why.\n\
4. RED LINE: what must NOT be done.\n\n\
Under 500 words."
        );
    }
    format!(
        "ROUND {round} - CROSS-EXAMINATION. All positions so far:\n\n{transcript}\n\n\
1. WHERE YOU WERE WRONG: what you now think you got wrong, and who convinced you. \
If nothing, say so and defend your position.\n\
2. STRONGEST DISAGREEMENT: name the peer and the claim. Argue it concretely.\n\
3. WHAT THE PANEL IS MISSING: a risk or option nobody raised.\n\
4. CONVERGENCE: what is settled, what is genuinely still open?\n\n\
Under 600 words."
    )
}

const SYNTH: &str = "\
You are the panel chair. Write the CONSENSUS DOCUMENT.

Be ruthless about separating genuine agreement from disagreement. Do NOT \
manufacture consensus that is not there - a false consensus is worse than an \
honest split, because it hides the decision the reader actually has to make.

## Consensus reached
Note unanimous vs majority, and name dissenters.

## Unresolved disagreements
Each side's best argument, and what evidence would settle it. Do not paper over these.

## Decision
The recommended course of action, with a confidence level and the reasoning.

## Red lines
What must not be done.

## What changed during deliberation
Positions that actually moved, and why. This is the highest-signal section - be specific.";

impl Deliberation {
    fn cache_key(&self, panel: &ResolvedPanel) -> String {
        let mut h = Sha256::new();
        h.update(self.question.as_bytes());
        h.update(self.context.as_deref().unwrap_or("").as_bytes());
        h.update(panel.name.as_bytes());
        h.update([self.rounds]);
        for m in &panel.members {
            h.update(m.name.as_bytes());
            h.update(m.provider.as_bytes());
            h.update(m.model.as_bytes());
        }
        if let Some(mt) = self.max_tokens {
            h.update(mt.to_le_bytes());
        }
        format!("{:x}", h.finalize()).chars().take(16).collect()
    }

    pub async fn run(&self, cfg: &Config) -> Result<Outcome> {
        let panel = &self.panel;
        let http = Arc::new(
            reqwest::Client::builder()
                // Successful streamed calls return in well under this. A request
                // still open at 10 min is wedged, not slow.
                .timeout(std::time::Duration::from_secs(600))
                .build()?,
        );

        let dir = cfg.data_dir().join("runs").join(self.cache_key(panel));
        tokio::fs::create_dir_all(&dir).await.ok();

        let question = &self.question;
        let system_base = self.context.as_ref().map_or_else(
            || format!("{RULES}\n\n=== QUESTION ===\n{question}"),
            |c| format!("{RULES}\n\n=== QUESTION ===\n{question}\n\n=== CONTEXT ===\n{c}"),
        );

        let mut transcript = String::new();
        let mut failures = Vec::new();

        for round in 1..=self.rounds {
            let round_out = self
                .run_round(
                    cfg,
                    panel,
                    &http,
                    &dir,
                    &system_base,
                    round,
                    &transcript,
                    &mut failures,
                )
                .await?;
            let _ = writeln!(transcript, "\n########## ROUND {round} ##########");
            for (name, text) in &round_out {
                let _ = writeln!(transcript, "\n===== {name} =====\n{text}");
            }
        }

        // Chair synthesis.
        let chair = panel
            .chair
            .as_deref()
            .and_then(|c| panel.members.iter().find(|m| m.name == c))
            .or_else(|| panel.members.last())
            .context("panel has no members")?;
        let provider = cfg.provider(&chair.provider)?;
        let consensus_path = dir.join("consensus.md");

        let consensus = match tokio::fs::read_to_string(&consensus_path).await {
            // Any non-trivial cached synthesis counts. A length threshold here
            // silently re-bills the chair on every resume for terse models.
            Ok(c) if self.resume && c.trim().len() > 40 => c,
            _ => {
                let text = provider
                    .complete(
                        &http,
                        Request {
                            model: &chair.model,
                            // The chair must see the question too - synthesising a
                            // transcript without knowing what was asked produces a
                            // summary, not a decision.
                            system: &format!(
                                "You are a rigorous panel chair. You do not manufacture \
                                 agreement. You separate fact from opinion.\n\n{system_base}"
                            ),
                            user: &format!("{SYNTH}\n\n=== TRANSCRIPT ===\n{transcript}"),
                            max_tokens: chair
                                .max_tokens
                                .or(self.max_tokens)
                                .unwrap_or(cfg.max_tokens)
                                .max(8000),
                        },
                    )
                    .await
                    .context("chair synthesis failed")?;
                let _ = tokio::fs::write(&consensus_path, &text).await;
                text
            }
        };

        let _ = tokio::fs::write(dir.join("transcript.md"), &transcript).await;
        Ok(Outcome {
            transcript,
            consensus,
            cache_dir: dir,
            failures,
        })
    }

    /// One round: every member in parallel, cached, failures collected.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal helper, all fields needed"
    )]
    async fn run_round(
        &self,
        cfg: &Config,
        panel: &ResolvedPanel,
        http: &Arc<reqwest::Client>,
        dir: &std::path::Path,
        system_base: &str,
        round: u8,
        transcript: &str,
        failures: &mut Vec<String>,
    ) -> Result<Vec<(String, String)>> {
        let prompt = Arc::new(round_prompt(round, self.rounds, transcript));
        let mut tasks = FuturesUnordered::new();

        for member in &panel.members {
            let (http, prompt, dir) = (http.clone(), prompt.clone(), dir.to_path_buf());
            let provider = cfg.provider(&member.provider)?.clone();
            let member = member.clone();
            let max_tokens = member
                .max_tokens
                .or(self.max_tokens)
                .unwrap_or(cfg.max_tokens);
            let system = member.persona.as_ref().map_or_else(
                || system_base.to_owned(),
                |p| format!("{system_base}\n\n=== YOUR PERSPECTIVE ===\n{p}"),
            );

            tasks.push(async move {
                let path = dir.join(format!("r{round}_{}.md", member.name));
                // Resume: a killed run must not re-pay for finished work.
                if let Ok(cached) = tokio::fs::read_to_string(&path).await {
                    if cached.len() > 200 {
                        return (member.name, Ok(cached));
                    }
                }
                let out = provider
                    .complete(
                        &http,
                        Request {
                            model: &member.model,
                            system: &system,
                            user: &prompt,
                            max_tokens,
                        },
                    )
                    .await;
                if let Ok(text) = &out {
                    let _ = tokio::fs::write(&path, text).await;
                }
                (member.name, out)
            });
        }

        let mut round_out: Vec<(String, String)> = Vec::new();
        while let Some((name, res)) = tasks.next().await {
            match res {
                Ok(text) => round_out.push((name, text)),
                Err(e) => {
                    // One dead model must not kill the panel; note it and go on.
                    tracing::warn!("{name} failed in round {round}: {e:#}");
                    failures.push(format!("{name} (round {round}): {e}"));
                }
            }
        }
        if round_out.is_empty() {
            anyhow::bail!(
                "every member failed in round {round}; first error: {}",
                failures.first().map_or("unknown", String::as_str)
            );
        }
        // Stable order regardless of completion order, so the transcript is
        // reproducible and diffable across runs.
        round_out.sort_by_key(|(n, _)| {
            panel
                .members
                .iter()
                .position(|m| &m.name == n)
                .unwrap_or(usize::MAX)
        });
        Ok(round_out)
    }
}

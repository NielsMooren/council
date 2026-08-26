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
use crate::tools::Toolbox;
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
    /// Research tools panellists may use to answer their own questions.
    /// Default (empty) means text-only reasoning.
    pub tools: Toolbox,
    /// Reuse cached member responses for an identical question+panel+round.
    pub resume: bool,
}

/// Everything needed to audit one panellist's turn after the fact: what it was
/// asked, what it looked up, what each lookup returned, and what it concluded.
#[derive(serde::Serialize)]
struct ResearchLog {
    round: u8,
    member: String,
    provider: String,
    model: String,
    /// The exact system prompt, so a protocol change is visible in the record.
    system: String,
    prompt: String,
    answer: String,
    research: Vec<crate::provider::ToolRecord>,
}

pub struct Outcome {
    pub transcript: String,
    pub consensus: String,
    pub cache_dir: PathBuf,
    pub failures: Vec<String>,
}

/// Appended to the system prompt only when the panellist actually has tools.
/// Without this they do not know to use them, and a model that speculates when
/// it could have checked is the failure mode this whole feature exists to fix.
const TOOL_RULES: &str = "\
\n\nYOU HAVE RESEARCH TOOLS. Use them before asserting anything you could verify:\n\
- Do not speculate about code you can read. Read it.\n\
- Do not claim an API behaves a certain way without checking.\n\
- When a peer makes a load-bearing claim, verify it rather than accepting it.\n\
- Quote what you found (file:line, or the URL) so peers can check your work.\n\
- If a tool returns nothing useful, say so plainly instead of guessing.\n\
Prefer three targeted lookups over ten broad ones; you have a bounded budget.";

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
If you have research tools, verify your load-bearing assumptions BEFORE writing.\n\n\
1. POSITION: your answer, stated plainly.\n\
2. REASONING: why, with the load-bearing assumptions named. Cite anything you \
verified (file:line or URL) and mark anything you could not.\n\
3. STRONGEST OBJECTION to your own position.\n\
4. WHAT WOULD CHANGE YOUR MIND: the specific evidence.\n\n\
Under 600 words."
            .to_owned();
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
2. STRONGEST DISAGREEMENT: name the peer and the claim. Argue it concretely. If \
you have tools and the claim is checkable, CHECK IT and report what you found.\n\
3. UNVERIFIED CLAIMS: any assertion a peer made that nobody has evidence for.\n\
4. WHAT THE PANEL IS MISSING: a risk or option nobody raised.\n\
5. CONVERGENCE: what is settled, what is genuinely still open?\n\n\
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
An explicit 'insufficient evidence to decide' is a VALID and sometimes correct answer - say what is missing and what would settle it. Do not manufacture a recommendation just to fill this heading.

## Red lines
What must not be done.

## What changed during deliberation
Positions that actually moved, and why. This is the highest-signal section - be specific.";

impl Deliberation {
    /// Cache key for a run.
    ///
    /// Must cover EVERYTHING that changes what a member is asked, or a warm
    /// cache silently serves output produced under a different protocol. This
    /// was verified as a live bug: two runs differing only in persona text
    /// collided on one key, so run 2 replayed run 1's answers.
    ///
    /// `PROTOCOL_VERSION` is the manual escape hatch - bump it whenever the
    /// prompts in this file change, since their text is not otherwise hashed.
    fn cache_key(&self, panel: &ResolvedPanel) -> String {
        /// Bump on any change to `RULES`, `TOOL_RULES`, `round_prompt` or `SYNTH`.
        const PROTOCOL_VERSION: u32 = 2;

        let mut h = Sha256::new();
        h.update(PROTOCOL_VERSION.to_le_bytes());
        h.update(self.question.as_bytes());
        h.update(self.context.as_deref().unwrap_or("").as_bytes());
        h.update(panel.name.as_bytes());
        h.update([self.rounds]);
        // The chair authors consensus.md; a different chair is a different run.
        h.update(panel.chair.as_deref().unwrap_or("<last-member>").as_bytes());
        for m in &panel.members {
            h.update(m.name.as_bytes());
            h.update(m.provider.as_bytes());
            h.update(m.model.as_bytes());
            // Persona is part of the system prompt, so it changes the question.
            h.update(m.persona.as_deref().unwrap_or("").as_bytes());
            // Per-member ceilings can truncate an answer.
            h.update(m.max_tokens.unwrap_or(0).to_le_bytes());
        }
        if let Some(mt) = self.max_tokens {
            h.update(mt.to_le_bytes());
        }
        // A run with code access is not the same run as one without.
        for root in &self.tools.roots {
            h.update(root.as_os_str().as_encoded_bytes());
        }
        h.update([u8::from(self.tools.web)]);
        format!("{:x}", h.finalize()).chars().take(16).collect()
    }

    pub async fn run(&self, cfg: &Config) -> Result<Outcome> {
        let panel = &self.panel;
        let http = Arc::new(
            reqwest::Client::builder()
                // Successful streamed calls return in well under this. A request
                // still open at 10 min is wedged, not slow.
                .timeout(std::time::Duration::from_secs(600))
                // Advertise compression so fetched pages are not sent
                // uncompressed. Costs the server nothing and saves bandwidth.
                // NOTE: the feature flag alone does nothing - reqwest requires
                // the builder call too, and silently sends no Accept-Encoding
                // without it.
                .gzip(true)
                .brotli(true)
                // Redirects are followed MANUALLY in tools.rs so the rate
                // limiter can charge every hop. Automatic following would make
                // hops 2..10 invisible to it.
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        );

        let dir = cfg.data_dir().join("runs").join(self.cache_key(panel));
        tokio::fs::create_dir_all(&dir).await.ok();

        let question = &self.question;
        // Tool instructions only when tools are actually available, so a
        // text-only run is not told about capabilities it does not have.
        let rules = if self.tools.is_empty() {
            RULES.to_owned()
        } else {
            format!("{RULES}{TOOL_RULES}")
        };
        let system_base = self.context.as_ref().map_or_else(
            || format!("{rules}\n\n=== QUESTION ===\n{question}"),
            |c| format!("{rules}\n\n=== QUESTION ===\n{question}\n\n=== CONTEXT ===\n{c}"),
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
            // Absent members are named in the transcript. Without this, later
            // rounds and the chair reason as if the panel were complete - a
            // 2-of-4 panel reads identically to a 4-of-4 one.
            for (name, text) in &round_out {
                let _ = writeln!(transcript, "\n===== {name} =====\n{text}");
            }
            let absent: Vec<&str> = panel
                .members
                .iter()
                .map(|m| m.name.as_str())
                .filter(|n| !round_out.iter().any(|(got, _)| got == n))
                .collect();
            if !absent.is_empty() {
                let _ = writeln!(
                    transcript,
                    "\n===== ABSENT THIS ROUND =====\n{} did not respond. Weigh the \
                     remaining positions accordingly; this was not a full panel.",
                    absent.join(", ")
                );
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
                            // The chair synthesises what was argued; it must not
                            // introduce fresh evidence nobody debated.
                            tools: &Toolbox::default(),
                        },
                    )
                    .await
                    .context("chair synthesis failed")?
                    .text;
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
            let tools = self.tools.clone();
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
                            tools: &tools,
                        },
                    )
                    .await;
                match out {
                    Ok(done) => {
                        let _ = tokio::fs::write(&path, &done.text).await;
                        // Full provenance beside the prose. The transcript keeps
                        // one summary line per lookup; this keeps the arguments
                        // AND the results, which is what makes a past
                        // deliberation auditable at all.
                        if !done.research.is_empty() {
                            let meta = ResearchLog {
                                round,
                                member: member.name.clone(),
                                provider: member.provider.clone(),
                                model: member.model.clone(),
                                system: system.clone(),
                                prompt: prompt.to_string(),
                                answer: done.text.clone(),
                                research: done.research,
                            };
                            if let Ok(json) = serde_json::to_vec_pretty(&meta) {
                                let jpath =
                                    dir.join(format!("r{round}_{}.research.json", member.name));
                                let _ = tokio::fs::write(jpath, json).await;
                            }
                        }
                        (member.name, Ok(done.text))
                    }
                    Err(e) => (member.name, Err(e)),
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Member;

    fn member(name: &str, persona: Option<&str>) -> Member {
        Member {
            name: name.to_owned(),
            provider: "p".to_owned(),
            model: "m".to_owned(),
            max_tokens: None,
            persona: persona.map(str::to_owned),
        }
    }

    fn deliberation() -> Deliberation {
        Deliberation {
            question: "same question".to_owned(),
            context: None,
            rounds: 1,
            panel: ResolvedPanel {
                name: "p".to_owned(),
                members: vec![member("A", None), member("B", None)],
                chair: Some("A".to_owned()),
            },
            max_tokens: None,
            tools: Toolbox::default(),
            resume: true,
        }
    }

    /// Regression: a run differing only in persona text collided on one cache
    /// key, so the second run replayed the first run's answers. Verified as a
    /// live bug before the fix.
    #[test]
    fn cache_key_separates_personas() {
        let base = deliberation();
        let mut changed = deliberation();
        changed.panel.members[0] = member("A", Some("argue for speed"));
        assert_ne!(
            base.cache_key(&base.panel),
            changed.cache_key(&changed.panel),
            "persona is part of the system prompt and must change the key"
        );
    }

    #[test]
    fn cache_key_separates_chairs() {
        let base = deliberation();
        let mut changed = deliberation();
        changed.panel.chair = Some("B".to_owned());
        assert_ne!(
            base.cache_key(&base.panel),
            changed.cache_key(&changed.panel),
            "the chair authors consensus.md, so it must change the key"
        );
    }

    #[test]
    fn cache_key_separates_member_token_ceilings() {
        let base = deliberation();
        let mut changed = deliberation();
        changed.panel.members[0].max_tokens = Some(500);
        assert_ne!(
            base.cache_key(&base.panel),
            changed.cache_key(&changed.panel),
            "a per-member ceiling can truncate an answer"
        );
    }

    #[test]
    fn cache_key_is_stable_for_identical_input() {
        let a = deliberation();
        let b = deliberation();
        assert_eq!(
            a.cache_key(&a.panel),
            b.cache_key(&b.panel),
            "resume depends on the key being deterministic"
        );
    }
}

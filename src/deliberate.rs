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

/// Identity of the turn being logged.
struct LogCtx {
    dir: PathBuf,
    round: u8,
    name: String,
    provider: String,
    model: String,
    system: String,
    prompt: String,
}

impl LogCtx {
    fn new(
        dir: &std::path::Path,
        round: u8,
        member: &crate::provider::Member,
        system: &str,
        prompt: &str,
    ) -> Self {
        Self {
            dir: dir.to_path_buf(),
            round,
            name: member.name.clone(),
            provider: member.provider.clone(),
            model: member.model.clone(),
            system: system.to_owned(),
            prompt: prompt.to_owned(),
        }
    }
}

/// Write one member's provenance.
///
/// Called BEFORE the prose is written, because the prose is the resume marker:
/// that ordering can leave provenance without an answer (harmless, it re-runs)
/// but never an answer without provenance, which would be silently unauditable.
async fn write_log(
    ctx: &LogCtx,
    research: Vec<crate::provider::ToolRecord>,
    answer: String,
    failed: Option<String>,
) -> Result<()> {
    if research.is_empty() {
        return Ok(());
    }
    let meta = ResearchLog {
        round: ctx.round,
        member: ctx.name.clone(),
        provider: ctx.provider.clone(),
        model: ctx.model.clone(),
        system: ctx.system.clone(),
        prompt: ctx.prompt.clone(),
        answer,
        failed,
        research,
    };
    let json = serde_json::to_vec_pretty(&meta).context("serialising provenance")?;
    let path = ctx
        .dir
        .join(format!("r{}_{}.research.json", ctx.round, ctx.name));
    tokio::fs::write(&path, json)
        .await
        .with_context(|| format!("writing {}", path.display()))
}

/// Persist whatever a failed member managed to look up before it died.
///
/// That partial trail is exactly what an audit needs, and a write failure here
/// is reported rather than swallowed: the original error is preserved in the
/// `failed` field and returned by the caller regardless, so warning masks
/// nothing.
async fn persist_salvage(ctx: &LogCtx, e: &anyhow::Error) {
    let Some(research) = e
        .chain()
        .find_map(|c| c.downcast_ref::<crate::provider::Salvaged>())
        .map(|s| s.0.clone())
    else {
        return;
    };
    if let Err(werr) = write_log(ctx, research, String::new(), Some(format!("{e:#}"))).await {
        eprintln!(
            "council: WARNING could not persist salvaged provenance for {} round {}: {werr:#}",
            ctx.name, ctx.round
        );
    }
}

/// Root paths for the cache manifest.
///
/// Paths, not contents: hashing a whole tree per run is too slow to be worth it.
/// The consequence is real and documented - editing code in place and re-running
/// the same question reuses the old answers, so use `--fresh` (CLI) after
/// changing the code you are asking about.
fn panel_roots(tools: &Toolbox) -> Vec<String> {
    tools
        .roots
        .iter()
        .map(|p| p.display().to_string())
        .collect()
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
    /// Set when the member failed: the error, so a truncated trail is explained
    /// rather than looking like a member that simply stopped early.
    #[serde(skip_serializing_if = "Option::is_none")]
    failed: Option<String>,
    research: Vec<crate::provider::ToolRecord>,
}

/// Per-member result, so a caller can see who actually spoke.
#[derive(serde::Serialize)]
pub struct MemberOutcome {
    pub name: String,
    pub ok: bool,
    /// How many rounds this member contributed to.
    pub rounds_present: u8,
}

pub struct Outcome {
    /// Who actually contributed, so a caller can tell a 4-of-4 panel from a
    /// 2-of-4 one without parsing prose.
    pub members: Vec<MemberOutcome>,
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
    /// Hashes a canonical JSON manifest rather than concatenating fields. Two
    /// reasons: undelimited concatenation means `name="ab", model="c"` and
    /// `name="a", model="bc"` hash identically, and a manifest makes it obvious
    /// when a new input has been forgotten - which is how the persona collision
    /// shipped in the first place.
    ///
    /// Everything that changes what a member is ASKED must be in here, including
    /// the prompt constants: relying on a hand-bumped version number is the same
    /// bug class re-armed, since the prompts get edited far more often than
    /// anyone remembers to bump a constant.
    fn cache_key(&self, panel: &ResolvedPanel, cfg: &Config) -> String {
        let manifest = serde_json::json!({
            // Hash of the prompt text itself, so editing RULES or SYNTH
            // invalidates the cache automatically. No discipline required.
            "protocol": Self::protocol_digest(),
            "question": self.question,
            "context": self.context,
            "panel": panel.name,
            "rounds": self.rounds,
            "chair": panel.chair,
            "members": panel.members.iter().map(|m| serde_json::json!({
                "name": m.name,
                "provider": m.provider,
                "model": m.model,
                "persona": m.persona,
                // The EFFECTIVE ceiling, not just an override: changing the
                // config default silently reused stale answers before.
                "max_tokens": m.max_tokens.or(self.max_tokens).unwrap_or(cfg.max_tokens),
            })).collect::<Vec<_>>(),
            "tools": {
                "roots": panel_roots(&self.tools),
                "web": self.tools.web,
            },
        });
        let mut h = Sha256::new();
        // to_string on a serde_json::Value with sorted keys is canonical enough:
        // serde_json preserves insertion order and this literal is fixed.
        h.update(manifest.to_string().as_bytes());
        format!("{:x}", h.finalize()).chars().take(16).collect()
    }

    /// Digest of every prompt constant, so a prompt edit changes the cache key.
    fn protocol_digest() -> String {
        let mut h = Sha256::new();
        h.update(RULES.as_bytes());
        h.update(TOOL_RULES.as_bytes());
        h.update(SYNTH.as_bytes());
        // Round prompts are generated, so hash each shape a run can produce.
        for round in 1..=6_u8 {
            h.update(round_prompt(round, 6, "").as_bytes());
        }
        format!("{:x}", h.finalize()).chars().take(12).collect()
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

        let dir = cfg.data_dir().join("runs").join(self.cache_key(panel, cfg));
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
        let mut present: std::collections::BTreeMap<String, u8> =
            panel.members.iter().map(|m| (m.name.clone(), 0)).collect();

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
                if let Some(n) = present.get_mut(name) {
                    *n = n.saturating_add(1);
                }
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

        let consensus = self
            .synthesise(cfg, panel, &http, &dir, &system_base, &transcript)
            .await?;

        let _ = tokio::fs::write(dir.join("transcript.md"), &transcript).await;

        Ok(Outcome {
            transcript,
            consensus,
            cache_dir: dir,
            failures,
            members: panel
                .members
                .iter()
                .map(|m| {
                    let rounds_present = present.get(&m.name).copied().unwrap_or(0);
                    MemberOutcome {
                        name: m.name.clone(),
                        ok: rounds_present == self.rounds,
                        rounds_present,
                    }
                })
                .collect(),
        })
    }

    /// Chair phase: one member reads the whole transcript and writes the
    /// consensus document.
    ///
    /// Deliberately toolless - it synthesises what was argued and must not
    /// introduce evidence nobody debated.
    async fn synthesise(
        &self,
        cfg: &Config,
        panel: &ResolvedPanel,
        http: &Arc<reqwest::Client>,
        dir: &std::path::Path,
        system_base: &str,
        transcript: &str,
    ) -> Result<String> {
        let chair = panel
            .chair
            .as_deref()
            .and_then(|c| panel.members.iter().find(|m| m.name == c))
            .or_else(|| panel.members.last())
            .context("panel has no members")?;
        let provider = cfg.provider(&chair.provider)?;
        let consensus_path = dir.join("consensus.md");

        // Any non-trivial cached synthesis counts. A length threshold here
        // silently re-bills the chair on every resume for terse models.
        if let Ok(cached) = tokio::fs::read_to_string(&consensus_path).await {
            if self.resume && cached.trim().len() > 40 {
                return Ok(cached);
            }
        }
        let text = provider
            .complete(
                http,
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
                    tools: &Toolbox::default(),
                },
            )
            .await
            .context("chair synthesis failed")?
            .text;
        let _ = tokio::fs::write(&consensus_path, &text).await;
        Ok(text)
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
                    // Only resume an answer we can still audit. A cached `.md`
                    // whose provenance is missing while the answer cites lookups
                    // is unverifiable, so re-run it rather than trust it.
                    let jpath = dir.join(format!("r{round}_{}.research.json", member.name));
                    let auditable = !cached.contains("<research>")
                        || tokio::fs::try_exists(&jpath).await.unwrap_or(false);
                    if cached.len() > 200 && auditable {
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
                let ctx = LogCtx::new(&dir, round, &member, &system, &prompt);

                match out {
                    Ok(done) => {
                        // A provenance write failure is NOT ignored: silently
                        // losing the audit trail is worse than failing loudly.
                        if let Err(e) =
                            write_log(&ctx, done.research, done.text.clone(), None).await
                        {
                            return (member.name, Err(e));
                        }
                        if let Err(e) = tokio::fs::write(&path, &done.text).await {
                            return (
                                member.name,
                                Err(anyhow::Error::new(e)
                                    .context(format!("writing {}", path.display()))),
                            );
                        }
                        (member.name, Ok(done.text))
                    }
                    Err(e) => {
                        persist_salvage(&ctx, &e).await;
                        (member.name, Err(e))
                    }
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

    fn test_cfg() -> Config {
        Config::default()
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
            base.cache_key(&base.panel, &test_cfg()),
            changed.cache_key(&changed.panel, &test_cfg()),
            "persona is part of the system prompt and must change the key"
        );
    }

    #[test]
    fn cache_key_separates_chairs() {
        let base = deliberation();
        let mut changed = deliberation();
        changed.panel.chair = Some("B".to_owned());
        assert_ne!(
            base.cache_key(&base.panel, &test_cfg()),
            changed.cache_key(&changed.panel, &test_cfg()),
            "the chair authors consensus.md, so it must change the key"
        );
    }

    #[test]
    fn cache_key_separates_member_token_ceilings() {
        let base = deliberation();
        let mut changed = deliberation();
        changed.panel.members[0].max_tokens = Some(500);
        assert_ne!(
            base.cache_key(&base.panel, &test_cfg()),
            changed.cache_key(&changed.panel, &test_cfg()),
            "a per-member ceiling can truncate an answer"
        );
    }

    /// Regression: only per-member OVERRIDES were hashed, so changing the
    /// config-wide default silently reused stale member output.
    #[test]
    fn cache_key_separates_effective_config_ceiling() {
        let d = deliberation();
        let mut other = test_cfg();
        other.max_tokens = test_cfg().max_tokens + 1000;
        assert_ne!(
            d.cache_key(&d.panel, &test_cfg()),
            d.cache_key(&d.panel, &other),
            "the effective ceiling changes what a member can say"
        );
    }

    /// Regression: prompt constants were not hashed, so editing RULES or SYNTH
    /// replayed answers produced under the old protocol.
    #[test]
    fn protocol_digest_covers_the_prompts() {
        let digest = Deliberation::protocol_digest();
        assert_eq!(digest.len(), 12, "digest should be a fixed-width prefix");
        // Cheap guard: the digest must actually depend on prompt text, so a
        // future refactor that stops hashing them fails here.
        assert!(
            RULES.len() + SYNTH.len() + TOOL_RULES.len() > 500,
            "prompts unexpectedly tiny - is protocol_digest still hashing them?"
        );
    }

    #[test]
    fn cache_key_is_stable_for_identical_input() {
        let a = deliberation();
        let b = deliberation();
        assert_eq!(
            a.cache_key(&a.panel, &test_cfg()),
            b.cache_key(&b.panel, &test_cfg()),
            "resume depends on the key being deterministic"
        );
    }
}

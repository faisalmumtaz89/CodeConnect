//! The hand-authored script the fictional fleet plays.
//!
//! The content lives in `script/fleet.json` and `script/sample.diff` and is
//! embedded at build time, so the binary carries its own fleet and reads nothing
//! from a filesystem at any point in its life.
//!
//! What the script may **not** carry is anything derivable. A card's
//! `payload_hash` and its risk class are computed from `tool_name` and
//! `tool_input` by the `protocol` crate at the moment the card is raised, so an
//! authored card cannot disagree with the hash the phone verifies its answer
//! against — the one failure that would make every approval in the demo refuse.

use anyhow::{bail, Context, Result};
use protocol::event::{EventKind, Source};
use serde::Deserialize;

const FLEET_JSON: &str = include_str!("../script/fleet.json");

/// The tree every `get_diff` is answered with, whichever run was asked about.
/// One diff rather than five: it is the same fiction either way, and five would
/// be four more things to keep honest.
pub const SAMPLE_DIFF: &str = include_str!("../script/sample.diff");

#[derive(Debug, Deserialize)]
pub struct Script {
    pub runs: Vec<Run>,
}

#[derive(Debug, Deserialize)]
pub struct Run {
    /// The tmux name a real run would carry. Reused by the next run in this
    /// slot, exactly as `ccd` reuses `cc-1` — which is why the identity of a run
    /// is its uid and this is only a label.
    pub name: String,
    pub cwd: String,
    /// Played once, in order, when the run starts.
    pub opening: Vec<Step>,
    /// Cycled, one card at a time. Empty means this run never blocks.
    #[serde(default)]
    pub approvals: Vec<Approval>,
    /// Played after an allow-shaped decision, behind the tool call and the tool
    /// result the engine derives from the card itself.
    #[serde(default)]
    pub allowed: Vec<Step>,
    /// Played after a deny. Nothing runs first, because nothing ran.
    #[serde(default)]
    pub denied: Vec<Step>,
    /// Cycled for as long as the process lives. Empty means the run is finished
    /// and says nothing more.
    #[serde(default)]
    pub progress: Vec<Step>,
}

#[derive(Debug, Deserialize)]
pub struct Step {
    /// Milliseconds after the step before this one. Relative on purpose: no part
    /// of the script's content or timing depends on a wall clock, so the fleet
    /// reads the same on the day it is reviewed as on the day it was written.
    pub after_ms: u64,
    pub kind: String,
    pub source: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct Approval {
    /// Stem of the request id. The cycle number is appended when the card is
    /// raised, so a respawned card is a *new* request rather than a retry of the
    /// settled one — which is what lets the ledger stay first-answer-wins while
    /// the demo repeats for the next reviewer.
    pub request_id: String,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    /// What the tool reports when it is allowed to run.
    pub tool_response: serde_json::Value,
    /// How long the fictional command takes, and what the derived `PostToolUse`
    /// reports as its `duration_ms`. One number, so the pause the reviewer sees
    /// and the number they read cannot disagree.
    pub duration_ms: u64,
}

impl Step {
    pub fn kind(&self) -> EventKind {
        EventKind::from_str_lossy(&self.kind)
    }

    pub fn source(&self) -> Source {
        // Validated at load, so the fallback is unreachable rather than a
        // silent reinterpretation of a typo.
        serde_json::from_value(serde_json::Value::String(self.source.clone()))
            .unwrap_or(Source::Daemon)
    }
}

/// Parse and validate the embedded script.
///
/// Every failure here is an authoring mistake in a file that ships inside the
/// binary, so it is raised before the listener binds: a fleet that is wrong is
/// worth less than no fleet at all, and a reviewer must never be the one who
/// finds out.
pub fn load() -> Result<Script> {
    let script: Script = serde_json::from_str(FLEET_JSON).context("parsing script/fleet.json")?;
    if script.runs.is_empty() {
        bail!("the script has no runs");
    }
    for run in &script.runs {
        validate(run).with_context(|| format!("run {}", run.name))?;
    }
    Ok(script)
}

fn validate(run: &Run) -> Result<()> {
    if run.opening.is_empty() {
        bail!("has no opening: a run has to announce itself before anything else can be said");
    }
    // The final component of `cwd` is this run's project label. `ccd` also
    // strips control characters and bounds the length, because a real cwd comes
    // off a user's disk; every cwd here is authored in the file next door and
    // checked at load, so the final component is the whole of the rule.
    if project_label(&run.cwd).is_empty() {
        bail!("cwd {:?} names no project", run.cwd);
    }
    for step in run
        .opening
        .iter()
        .chain(&run.allowed)
        .chain(&run.denied)
        .chain(&run.progress)
    {
        if matches!(step.kind(), EventKind::Other(_)) {
            bail!("{:?} is not an event kind this protocol knows", step.kind);
        }
        if serde_json::from_value::<Source>(serde_json::Value::String(step.source.clone())).is_err()
        {
            bail!(
                "{:?} is not an event source this protocol knows",
                step.source
            );
        }
    }
    // A card with no aftermath would leave the run blocked for ever the moment
    // somebody answered it, which is the one thing this fleet exists to show.
    if !run.approvals.is_empty() && (run.allowed.is_empty() || run.denied.is_empty()) {
        bail!("raises approvals but has no `allowed` or no `denied` to play afterwards");
    }
    if run.approvals.is_empty() && !(run.allowed.is_empty() && run.denied.is_empty()) {
        bail!("has an aftermath but no approval that could reach it");
    }
    if !run.approvals.is_empty() && !run.progress.is_empty() {
        bail!("both blocks and loops: a run that is waiting for a human is not also working");
    }
    Ok(())
}

/// What to call this run out loud: the final component of its `cwd`.
pub fn project_label(cwd: &str) -> &str {
    cwd.rsplit('/').find(|part| !part.is_empty()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_script_loads_and_covers_the_fleet_the_app_was_built_against() {
        let script = load().expect("the shipped script must parse");
        assert_eq!(
            script.runs.len(),
            5,
            "five runs, one per band the app draws"
        );

        let blocked: Vec<&Run> = script
            .runs
            .iter()
            .filter(|run| !run.approvals.is_empty())
            .collect();
        assert_eq!(blocked.len(), 3, "one card per risk class");
        assert_eq!(
            script
                .runs
                .iter()
                .filter(|run| !run.progress.is_empty())
                .count(),
            1,
            "exactly one run is working, so the fleet grows a Running band"
        );

        // The risk classes are the `protocol` classifier's, never the script's:
        // this is the assertion that the demo shows a HIGH, a MEDIUM and a LOW
        // because the real rules say so, not because a fixture claimed it.
        use protocol::risk::{classify, RiskClass};
        let classes: Vec<RiskClass> = blocked
            .iter()
            .map(|run| classify(&run.approvals[0].tool_name, &run.approvals[0].tool_input).class)
            .collect();
        assert!(classes.contains(&RiskClass::High), "{classes:?}");
        assert!(classes.contains(&RiskClass::Medium), "{classes:?}");
        assert!(classes.contains(&RiskClass::Low), "{classes:?}");

        let high = classify("Bash", &blocked[0].approvals[0].tool_input);
        assert_eq!(high.class, RiskClass::High);
        assert_eq!(
            high.matched_pattern.as_deref(),
            Some("git push --force main")
        );
    }

    /// Every respawn has to be a new request, and a new request has to be a new
    /// *hash* — otherwise the second card is byte-identical to the settled one
    /// and the ledger answers a tap that was never applied.
    #[test]
    fn no_run_offers_the_same_card_twice_in_its_cycle() {
        for run in load().unwrap().runs {
            let mut seen = Vec::new();
            for approval in &run.approvals {
                let hash = protocol::hash::approval_payload_hash(
                    &approval.tool_name,
                    &approval.tool_input,
                );
                assert!(!seen.contains(&hash), "{} repeats a card", run.name);
                assert!(
                    !seen.contains(&approval.request_id),
                    "{} repeats a request id stem",
                    run.name
                );
                seen.push(hash);
                seen.push(approval.request_id.clone());
            }
        }
    }

    #[test]
    fn the_sample_diff_is_a_diff() {
        assert!(SAMPLE_DIFF.starts_with("diff --git "), "{SAMPLE_DIFF}");
        assert!(SAMPLE_DIFF.contains("@@ -1,14 +1,15 @@"));
        assert!(SAMPLE_DIFF.len() < protocol::ws::MAX_DIFF_BYTES);
    }

    #[test]
    fn a_project_label_is_the_last_component_of_the_path() {
        assert_eq!(project_label("/Users/dev/app-1"), "app-1");
        assert_eq!(project_label("/Users/dev/app-1/"), "app-1");
        assert_eq!(project_label("/"), "");
        assert_eq!(project_label(""), "");
    }
}

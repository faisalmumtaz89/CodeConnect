//! `ccsoak` — the chaos gauntlet.
//!
//! ```sh
//! ccsoak all                 # every scenario against the newest live session
//! ccsoak kill --rounds 5     # just the kill storm
//! ccsoak tmuxfreeze          # freeze the tmux server; sends must stay bounded
//! ccsoak --session cc-1 all  # against a named run (uid or tmux name)
//! ```
//!
//! It attacks the **installed** daemon, not a fixture: same socket, same
//! database, same WebSocket server. That is the whole point — a harness that
//! stands up its own daemon proves the code paths work in a harness.
//!
//! Exit code is 0 only if every scenario passed or was skipped for an
//! environmental reason — there is no tmux on this machine, the session was
//! busy — because "we did not test this" and "this works" must never look the
//! same. A scenario that ran and could not reach its claim is a third thing
//! again: it is not a failure of the daemon and it is not a clean run either,
//! so it is reported as UNMEASURED and exits non-zero.

mod env;
mod scenarios;
mod ws;

use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::scenarios::Target;

/// What one scenario concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    /// The scenario ran and never reached the claim it exists for, so nothing
    /// was learned about it.
    ///
    /// Distinct from [`Verdict::Skip`], and the distinction is the exit code.
    /// A skip is the environment answering — there is no tmux here, the session
    /// is busy — and the operator can do nothing about it, so the run is clean.
    /// This is the measurement being *displaced*: the duplicate-attach scenario
    /// asks a second connection for the per-session lease and the Mac's global
    /// terminal cap answers instead, which is a true sentence about the Mac and
    /// no statement at all about the lease. That used to return a skip, and a
    /// release invocation therefore exited 0 having never exercised the lease
    /// once — "nothing was learned" taking the value that passes, which is the
    /// exact failure mode this harness exists to prevent.
    Unmeasured,
    /// Not run, with the reason. Never counted as a pass.
    Skip,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub verdict: Verdict,
    pub summary: String,
    /// The numbers. A gauntlet report without them cannot be compared to the
    /// previous run, which is the only thing a soak is for.
    pub notes: Vec<String>,
    pub elapsed: Duration,
}

impl Outcome {
    pub fn passed(summary: impl Into<String>) -> Outcome {
        Outcome::new(Verdict::Pass, summary)
    }

    pub fn failed(summary: impl Into<String>) -> Outcome {
        Outcome::new(Verdict::Fail, summary)
    }

    pub fn skipped(summary: impl Into<String>) -> Outcome {
        Outcome::new(Verdict::Skip, summary)
    }

    /// The scenario ran and its claim was never reached. See
    /// [`Verdict::Unmeasured`] for why this is not a skip.
    pub fn unmeasured(summary: impl Into<String>) -> Outcome {
        Outcome::new(Verdict::Unmeasured, summary)
    }

    fn new(verdict: Verdict, summary: impl Into<String>) -> Outcome {
        Outcome {
            verdict,
            summary: summary.into(),
            notes: Vec::new(),
            elapsed: Duration::default(),
        }
    }

    pub fn with_notes(mut self, notes: Vec<String>) -> Outcome {
        self.notes.extend(notes);
        self
    }

    fn label(&self) -> &'static str {
        match self.verdict {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Unmeasured => "UNMEASURED",
            Verdict::Skip => "SKIP",
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut session_ref: Option<String> = None;
    let mut rounds: Option<u32> = None;
    let mut which: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--session" => session_ref = it.next().cloned(),
            "--rounds" => rounds = Some(parse_rounds(it.next().map(String::as_str))?),
            "--help" | "-h" => {
                usage();
                return Ok(());
            }
            other if other.starts_with("--") => bail!("unknown flag {other}"),
            other => which = Some(other.to_string()),
        }
    }
    let which = which.unwrap_or_else(|| "all".to_string());

    let info = env::daemon_info().map_err(|err| {
        anyhow::anyhow!(
            "ccd is not answering {} ({err:#}). Start it with `codeconnect daemon install`.",
            protocol::socket_path().display()
        )
    })?;
    println!(
        "daemon   pid {} · version {} · protocol {}.{} · {}",
        info.pid,
        info.version,
        info.protocol_version,
        info.protocol_minor,
        if info.is_launchd_managed() {
            "launchd-managed"
        } else {
            "NOT launchd-managed — the kill storm cannot recover"
        }
    );
    // Against the daemon that is *running*, not the one that was just built.
    //
    // `install.sh` replaces the binaries on disk; launchd keeps executing the
    // process it already started. Without this check a gauntlet could report
    // seven passes for code that was never loaded — which is the one thing this
    // harness exists not to do, since "we did not test this" and "this works"
    // must never look the same.
    if info.protocol_minor < protocol::PROTOCOL_MINOR {
        bail!(
            "the running daemon speaks protocol minor {} and this harness was built against {}. \
             It is an older ccd still in memory — `codeconnect daemon restart` picks up the new binaries.",
            info.protocol_minor,
            protocol::PROTOCOL_MINOR
        );
    }

    let session = scenarios::choose_target(session_ref.as_deref())?;
    println!(
        "target   {} ({}) · {} · last_seq {}\n",
        session.session_id, session.session_uid, session.cwd, session.last_seq
    );
    // Loopback rather than the tailnet address the phone uses.
    //
    // Reaching one's own tailnet IP leaves the machine through the utun
    // interface, where the operator's network filter gets a vote — and a filter
    // that cannot identify a freshly built harness binary holds the connection
    // open and silent (measured here: Little Snitch, 70s to a reset). The
    // daemon serves the identical listener on 127.0.0.1 for exactly this, and
    // `--host` is there for anyone who wants to prove the tailnet path instead.
    let host = std::env::var("CCSOAK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let target = Target {
        session,
        host,
        port: info.endpoint_port,
        token: env::token()?,
        pairing: Default::default(),
    };
    println!(
        "wire     ws://{}:{} (the phone uses {}:{})\n",
        target.host, target.port, info.endpoint_host, info.endpoint_port
    );

    let mut results: Vec<(&str, Outcome)> = Vec::new();
    let run_all = which == "all";
    if run_all || which == "hookstorm" {
        results.push(
            timed(
                "b duplicate hooks",
                scenarios::duplicate_hooks(&target, replays(rounds)),
            )
            .await,
        );
    }
    if run_all || which == "answerstorm" {
        results.push(
            timed(
                "c answer storm",
                scenarios::answer_storm(&target, taps(rounds)),
            )
            .await,
        );
    }
    if run_all || which == "wsflap" {
        results.push(timed("d wss flap", scenarios::ws_flap(&target, flaps(rounds))).await);
    }
    if run_all || which == "commands" {
        results.push(timed("h slash recovery", scenarios::slash_commands(&target)).await);
    }
    if run_all || which == "tmuxfreeze" {
        results.push(timed("i tmux freeze", scenarios::tmux_freeze(&target)).await);
    }
    // The live terminal, before the two that interrupt the daemon and after the
    // freeze: an attach against a stopped tmux server would fail for a reason
    // that has nothing to do with the carrier.
    let run_terminal = run_all || which == "terminal";
    if run_terminal || which == "terminal_roundtrip" {
        results.push(
            timed(
                "j terminal round trip",
                scenarios::terminal_roundtrip(&target),
            )
            .await,
        );
    }
    if run_terminal || which == "terminal_starvation" {
        results.push(
            timed(
                "k terminal starvation",
                scenarios::terminal_starvation(&target),
            )
            .await,
        );
    }
    if run_terminal || which == "terminal_flap" {
        results.push(
            timed(
                "l terminal flap",
                scenarios::terminal_flap(&target, reattaches(rounds)),
            )
            .await,
        );
    }
    if run_terminal || which == "terminal_duplicate" {
        results.push(
            timed(
                "m terminal duplicate",
                scenarios::terminal_duplicate(&target),
            )
            .await,
        );
    }
    if run_terminal || which == "terminal_exit" {
        results.push(timed("n terminal exit", scenarios::terminal_exit(&target)).await);
    }
    if run_all || which == "tailtorture" {
        results.push(timed("e tailer torture", scenarios::tail_torture(&target)).await);
    }
    if run_all || which == "commitorder" {
        results.push(
            timed(
                "g commit order",
                scenarios::concurrent_commits(&target, writers(rounds)),
            )
            .await,
        );
    }
    // The two that interrupt the daemon run last, and the ingest kill runs
    // before the general storm: everything above should be measured against a
    // daemon that has not just restarted.
    if run_all || which == "ingestkill" {
        results.push(
            timed(
                "f kill mid-ingest",
                scenarios::kill_during_ingest(&target, kills(rounds)),
            )
            .await,
        );
    }
    if run_all || which == "kill" {
        results.push(
            timed(
                "a kill -9 storm",
                scenarios::kill_storm(&target, kills(rounds)),
            )
            .await,
        );
    }
    if results.is_empty() {
        usage();
        bail!("unknown scenario {which:?}");
    }

    report(&results);
    // The terminal scenarios pair this run as a device, which is a standing
    // grant of shell-equivalent authority. It goes back whatever the verdicts
    // were, and after the report rather than before it, because revoking is not
    // one of the measurements.
    let release = scenarios::release_device(&target);
    for said in &release.said {
        println!("{said}");
    }
    let failures = results
        .iter()
        .filter(|(_, o)| o.verdict == Verdict::Fail)
        .count();
    let unmeasured: Vec<&str> = results
        .iter()
        .filter(|(_, o)| o.verdict == Verdict::Unmeasured)
        .map(|(name, _)| *name)
        .collect();
    if let Some(complaint) = exit_complaint(failures, &unmeasured, &release.standing) {
        bail!("{complaint}");
    }
    Ok(())
}

/// The one sentence the process exits non-zero on, or `None` for a clean run.
///
/// Three conditions, and every one of them was at some point exiting 0.
///
/// A failed revoke counts: the exit code used to key on scenario verdicts
/// alone, so a run that could not hand back the credential it minted printed
/// the warning and exited 0 — a CI job read "clean run" over a standing
/// shell-equivalent grant on the operator's Mac. An **unmeasured** claim counts
/// too, and for the same reason: only `Verdict::Fail` was counted, so a
/// scenario whose measurement was displaced (see [`Verdict::Unmeasured`])
/// exited 0 having proved nothing about the invariant it is named for. An
/// environmental skip is not in this list and must not be — "there is no tmux
/// on this machine" is a true, clean run.
///
/// All of them are reported together, because a run that failed *and* left a
/// claim unmeasured *and* leaked must say all three rather than the first one
/// to be noticed.
fn exit_complaint(failures: usize, unmeasured: &[&str], standing: &[String]) -> Option<String> {
    let mut complaints = Vec::new();
    if failures > 0 {
        complaints.push("the gauntlet found something".to_string());
    }
    if !unmeasured.is_empty() {
        complaints.push(format!(
            "{} scenario(s) never reached the claim they exist to measure: {}",
            unmeasured.len(),
            unmeasured.join(", ")
        ));
    }
    if !standing.is_empty() {
        complaints.push(format!(
            "{} shell-equivalent device grant(s) this run created are still standing: {}",
            standing.len(),
            standing.join(", ")
        ));
    }
    if complaints.is_empty() {
        None
    } else {
        Some(complaints.join("; and "))
    }
}

/// One `--rounds N`.
///
/// Rejected rather than defaulted, on both counts. A value that does not parse
/// used to fall back to the scenario default, so `--rounds abc` ran fifty
/// replays and said nothing; and zero is not a smaller run but a run that
/// proves nothing while every verdict still reads as satisfied —
/// `reattaches(Some(0))` made the terminal flap iterate zero times, drop zero
/// sockets, and PASS. This is the single choke point: every scenario's count
/// comes from this one flag, so flooring it here floors all of them.
fn parse_rounds(value: Option<&str>) -> Result<u32> {
    let Some(value) = value else {
        bail!("--rounds needs a number");
    };
    let rounds: u32 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("--rounds {value:?} is not a number"))?;
    if rounds == 0 {
        bail!("--rounds 0 would run every scenario's attack zero times and report a pass for it");
    }
    Ok(rounds)
}

fn replays(override_value: Option<u32>) -> u32 {
    override_value.unwrap_or(50)
}
fn taps(override_value: Option<u32>) -> u32 {
    override_value.unwrap_or(20)
}
fn flaps(override_value: Option<u32>) -> u32 {
    override_value.unwrap_or(30)
}
fn kills(override_value: Option<u32>) -> u32 {
    override_value.unwrap_or(5)
}
fn writers(override_value: Option<u32>) -> u32 {
    override_value.unwrap_or(40)
}
/// Terminal flaps. Fewer than the socket flap's thirty: each round spawns a
/// tmux client, floods a pane and waits for the carrier to be reaped, so the
/// interesting behaviour repeats in three rounds and thirty would only make the
/// gauntlet slower.
fn reattaches(override_value: Option<u32>) -> u32 {
    override_value.unwrap_or(3)
}

async fn timed<F>(name: &'static str, future: F) -> (&'static str, Outcome)
where
    F: std::future::Future<Output = Outcome>,
{
    println!("running  {name} …");
    let start = Instant::now();
    let mut outcome = future.await;
    outcome.elapsed = start.elapsed();
    println!("         {} — {}", outcome.label(), outcome.summary);
    for note in &outcome.notes {
        println!("         · {note}");
    }
    println!();
    (name, outcome)
}

fn report(results: &[(&str, Outcome)]) {
    let width = results
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(10);
    // The state column is as wide as the longest label, so an UNMEASURED row
    // lines its result up with the rest rather than shunting it right, where a
    // reader skimming the column would take it for a wrapped line.
    println!("{:-<1$}", "", width + 64);
    println!(
        "{:<width$}  {:<10} {:>7}  RESULT",
        "SCENARIO", "STATE", "TIME"
    );
    println!("{:-<1$}", "", width + 64);
    for (name, outcome) in results {
        println!(
            "{:<width$}  {:<10} {:>6.1}s  {}",
            name,
            outcome.label(),
            outcome.elapsed.as_secs_f64(),
            outcome.summary
        );
    }
    println!("{:-<1$}", "", width + 64);
    let count = |want: Verdict| results.iter().filter(|(_, o)| o.verdict == want).count();
    println!(
        "{} passed, {} failed, {} unmeasured, {} skipped",
        count(Verdict::Pass),
        count(Verdict::Fail),
        count(Verdict::Unmeasured),
        count(Verdict::Skip)
    );
}

fn usage() {
    eprintln!(
        "\
ccsoak — chaos gauntlet against the running ccd

  ccsoak [--session <uid|name>] [--rounds N] <scenario>

  all           every scenario (default)
  kill          (a) kill -9 ccd during traffic; assert no lost or duplicated seq
  hookstorm     (b) replay one hook payload N times; assert exactly one event
  answerstorm   (c) N concurrent answers for one request; assert one applied
  wsflap        (d) N connect/subscribe/disconnect cycles; assert gap-free replay
  tailtorture   (e) truncate and rewrite a transcript; assert no duplicate ingest
  ingestkill    (f) kill -9 between a transcript write and its ingest; assert
                    every line is still in the log exactly once
  commitorder   (g) N concurrent commits on one session; assert no socket ever
                    skips a seq
  commands      (h) /status, /usage, /cost from the phone path; assert each is
                    composer_recovered with its pane and an ordinary send lands
                    immediately after — the slash-command release criterion
  tmuxfreeze    (i) SIGSTOP the tmux server mid-send; assert a bounded refusal
                    naming the tmux deadline, then thaw and prove the same
                    request id types fresh — the claim was released

  terminal      (j–n) every live-terminal scenario below
  terminal_roundtrip
                (j) attach, assert the snapshot repaints, type a marker and read
                    its echo, resize, detach — and the operator's run is still
                    listed afterwards
  terminal_starvation
                (k) attach with a small window, flood the pane, never replenish;
                    assert slow_consumer inside the 30s deadline, a surviving
                    connection and a working re-attach
  terminal_flap (l) N drops of the whole socket mid-stream; assert every
                    reattach repaints and that tmux clients never accumulate
  terminal_duplicate
                (m) a second attach from another connection takes the session's
                    terminal over and the incumbent is told superseded; a second
                    attach on one connection does the same down one socket
  terminal_exit (n) kill the session under its viewer; assert session_exited and
                    a daemon that is exactly as healthy as before

The terminal scenarios pair the harness as a device (a terminal is refused to
the static token) against a scratch tmux session of their own, and revoke the
device at the end.

The kill scenarios need the LaunchAgent installed (`codeconnect daemon install`), because
something has to bring the daemon back."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skip_is_never_reported_as_a_pass() {
        // The distinction the whole report rests on: "we did not test this" and
        // "this works" must not collapse into one label.
        let skipped = Outcome::skipped("session busy");
        assert_eq!(skipped.verdict, Verdict::Skip);
        assert_eq!(skipped.label(), "SKIP");
        assert_ne!(skipped.label(), Outcome::passed("x").label());
    }

    #[test]
    fn an_environment_skip_and_a_displaced_measurement_are_different_states() {
        // "There is no tmux on this machine" is the environment answering, and
        // a clean run. "The Mac's global cap answered the contender, so the
        // per-session lease was never exercised" is the measurement being
        // displaced, and proves nothing about the claim the scenario is named
        // for. Both used to be a skip, so the second one exited 0.
        let environment = Outcome::skipped("no tmux on this machine");
        let displaced = Outcome::unmeasured("the global cap answered the contender");
        assert_ne!(environment.verdict, displaced.verdict);
        assert_ne!(environment.label(), displaced.label());
        assert_eq!(displaced.label(), "UNMEASURED");
        // And neither of them is a pass.
        assert_ne!(displaced.verdict, Verdict::Pass);
        assert_ne!(displaced.label(), Outcome::passed("x").label());
    }

    #[test]
    fn notes_survive_a_failure() {
        // A failing scenario's numbers are the most useful ones there are, so
        // they must not be dropped on the way to the report.
        let outcome = Outcome::failed("gaps").with_notes(vec!["5 kills".into(), "1 gap".into()]);
        assert_eq!(outcome.verdict, Verdict::Fail);
        assert_eq!(outcome.notes.len(), 2);
    }

    #[test]
    fn the_scenario_defaults_are_stable_and_overridable() {
        assert_eq!(kills(None), 5);
        assert_eq!(replays(None), 50);
        assert_eq!(taps(None), 20);
        assert_eq!(flaps(None), 30);
        assert_eq!(writers(None), 40);
        assert_eq!(reattaches(None), 3);
        assert_eq!(kills(Some(2)), 2);
        assert_eq!(reattaches(Some(7)), 7);
        // Every count comes from the one `--rounds` flag, so the floor below is
        // what keeps `Some(0)` out of all of them.
        for count in [replays, taps, flaps, kills, writers, reattaches] {
            assert!(count(None) >= 1);
            assert_eq!(count(Some(1)), 1);
        }
    }

    #[test]
    fn a_round_count_that_would_measure_nothing_is_refused() {
        assert_eq!(parse_rounds(Some("3")).unwrap(), 3);
        // Zero iterates every attack loop zero times and leaves every verdict
        // at its passing value: the terminal flap would print "0 drops
        // mid-stream" and report PASS beside it.
        assert!(parse_rounds(Some("0")).is_err());
        // A bad argument is an error, not a silent fall back to the default —
        // `--rounds abc` used to run the standard fifty replays and say
        // nothing about it.
        assert!(parse_rounds(Some("abc")).is_err());
        assert!(parse_rounds(Some("-1")).is_err());
        assert!(parse_rounds(None).is_err());
    }

    #[test]
    fn a_grant_left_standing_is_a_failed_run() {
        // The branch the exit code used to miss entirely: every scenario
        // passed, the credential could not be handed back, and the process
        // exited 0 with a warning nobody's CI reads.
        assert_eq!(exit_complaint(0, &[], &[]), None);
        let leaked = exit_complaint(0, &[], &["ccsoak-terminal-4 (d7f2)".to_string()])
            .expect("a standing grant is a failure");
        assert!(leaked.contains("ccsoak-terminal-4"), "{leaked}");
        // And a run that both failed and leaked says both, rather than the
        // first one to be noticed.
        let both = exit_complaint(2, &[], &["ccsoak-terminal-4 (d7f2)".to_string()])
            .expect("failures are still a failure");
        assert!(both.contains("the gauntlet found something"), "{both}");
        assert!(both.contains("ccsoak-terminal-4"), "{both}");
        assert!(exit_complaint(1, &[], &[]).is_some());
    }

    #[test]
    fn a_claim_that_was_never_measured_is_not_a_clean_run() {
        // The measured defect: the Mac's global terminal cap answers the
        // duplicate-attach contender, the per-session lease is never exercised,
        // and the invocation exits 0 having proved nothing about it.
        let unmeasured = exit_complaint(0, &["m terminal duplicate"], &[])
            .expect("a claim nobody measured is not a clean run");
        // The sentence names what went unmeasured, or an operator reading CI
        // has no idea which invariant is uncovered.
        assert!(unmeasured.contains("m terminal duplicate"), "{unmeasured}");
        assert!(unmeasured.contains("never reached"), "{unmeasured}");

        // It composes with the other two rather than displacing them.
        let everything = exit_complaint(
            1,
            &["m terminal duplicate", "n terminal exit"],
            &["ccsoak-terminal-4 (d7f2)".to_string()],
        )
        .expect("three complaints are still a complaint");
        assert!(
            everything.contains("the gauntlet found something"),
            "{everything}"
        );
        assert!(everything.contains("m terminal duplicate"), "{everything}");
        assert!(everything.contains("n terminal exit"), "{everything}");
        assert!(everything.contains("ccsoak-terminal-4"), "{everything}");

        // And an environmental skip never reaches this at all: it is not in the
        // list, so a machine with no tmux still exits 0.
        assert_eq!(exit_complaint(0, &[], &[]), None);
    }
}

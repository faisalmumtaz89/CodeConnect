//! `ccsoak` — the chaos gauntlet.
//!
//! ```sh
//! ccsoak all                 # every scenario against the newest live session
//! ccsoak kill --rounds 5     # just the kill storm
//! ccsoak --session cc-1 all  # against a named run (uid or tmux name)
//! ```
//!
//! It attacks the **installed** daemon, not a fixture: same socket, same
//! database, same WebSocket server. That is the whole point — a harness that
//! stands up its own daemon proves the code paths work in a harness.
//!
//! Exit code is 0 only if every scenario passed. A skipped scenario (the
//! session was busy, so the measurement would have been meaningless) is not a
//! failure but is reported as its own state, because "we did not test this" and
//! "this works" must never look the same.

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
            "--rounds" => rounds = it.next().and_then(|value| value.parse().ok()),
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
    if results.iter().any(|(_, o)| o.verdict == Verdict::Fail) {
        bail!("the gauntlet found something");
    }
    Ok(())
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
    println!("{:-<1$}", "", width + 58);
    println!(
        "{:<width$}  {:<5} {:>7}  RESULT",
        "SCENARIO", "STATE", "TIME"
    );
    println!("{:-<1$}", "", width + 58);
    for (name, outcome) in results {
        println!(
            "{:<width$}  {:<5} {:>6.1}s  {}",
            name,
            outcome.label(),
            outcome.elapsed.as_secs_f64(),
            outcome.summary
        );
    }
    println!("{:-<1$}", "", width + 58);
    let passed = results
        .iter()
        .filter(|(_, o)| o.verdict == Verdict::Pass)
        .count();
    let failed = results
        .iter()
        .filter(|(_, o)| o.verdict == Verdict::Fail)
        .count();
    let skipped = results
        .iter()
        .filter(|(_, o)| o.verdict == Verdict::Skip)
        .count();
    println!("{passed} passed, {failed} failed, {skipped} skipped");
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
        assert_eq!(kills(Some(2)), 2);
    }
}

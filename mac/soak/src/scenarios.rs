//! The gauntlet.
//!
//! Each scenario attacks one invariant the event log claims, and each one
//! reports *numbers* rather than a verdict: "5 kills, 0 gaps, 0 duplicates,
//! recovery 0.4–1.2s" is something a human can compare against the next run,
//! and "PASS" is not.
//!
//! Two of the seven run against synthetic sessions (the tailer torture and the
//! kill-during-ingest run). That is not a shortcut — a synthetic transcript is
//! the only way to truncate and rewrite a JSONL file without corrupting a real
//! agent's history, and the daemon cannot tell the difference: a synthetic
//! session is adopted through exactly the hook path a real one uses. The other
//! five drive the live `codeconnect claude` session.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use protocol::event::SessionSummary;
use protocol::ipc::HookPost;
use protocol::ws::{AnswerDecision, AnswerResult};

use crate::env;
use crate::ws::{check_replay, Phone};
use crate::Outcome;

/// How long to give the tailer and the ingest path to catch up. Generous
/// against a 250ms poll plus FSEvents debounce, so a slow machine is not a
/// failure.
const SETTLE: Duration = Duration::from_secs(20);

pub struct Target {
    pub session: SessionSummary,
    pub host: String,
    pub port: u16,
    pub token: String,
}

impl Target {
    pub async fn phone(&self) -> Result<Phone> {
        Phone::connect(&self.host, self.port, &self.token).await
    }
}

// --------------------------------------------------------------- (a) kill -9

/// `kill -9 ccd` five times, at random moments, while events are flowing.
///
/// The claim under test is the one the whole architecture rests on: the daemon
/// is not the agents' parent, so losing it costs a reconnect and nothing else.
/// "Nothing else" is measured here as: the sequence is still gap-free, no fact
/// was recorded twice, and the supervisor came back.
pub async fn kill_storm(target: &Target, rounds: u32) -> Outcome {
    let mut notes = Vec::new();
    let uid = target.session.session_uid.clone();
    let mut recoveries = Vec::new();
    let mut posted = 0u32;
    let mut dropped = 0u32;

    // Give the real agent something cheap to do, so the daemon is killed while
    // a transcript is being written and hooks are firing — not while it idles.
    // A refusal is not fatal: the composer may be busy, and the synthetic
    // traffic below is enough on its own.
    let takeover = format!("soak-kill-{}", protocol::time::now_unix_ms());
    match target.phone().await {
        Ok(mut phone) => {
            let text = "reply with exactly: ok";
            match phone.send_text(&uid, text, Some(&takeover)).await {
                Ok(protocol::ws::SendTextResult::Sent { matched }) => {
                    notes.push(format!("prompted the live agent (matched {matched:?})"));
                    // The same mutation again, exactly as a phone on a flaky
                    // link would retry it. It must replay rather than type a
                    // second prompt into a live agent.
                    match phone.send_text(&uid, text, Some(&takeover)).await {
                        Ok(protocol::ws::SendTextResult::Duplicate { .. }) => {
                            notes.push("a retried takeover replayed instead of retyping".into());
                        }
                        Ok(other) => {
                            return Outcome::failed(format!(
                                "a retried send_text with the same identity must not act again: \
                                 {other:?}"
                            ))
                            .with_notes(notes)
                        }
                        Err(err) => {
                            return Outcome::failed(format!("retrying the takeover: {err:#}"))
                                .with_notes(notes)
                        }
                    }
                }
                Ok(other) => {
                    notes.push(format!(
                        "agent not prompted ({other:?}); synthetic traffic only"
                    ));
                }
                Err(err) => notes.push(format!(
                    "agent not prompted ({err:#}); synthetic traffic only"
                )),
            }
        }
        Err(err) => notes.push(format!("no phone connection for the prompt: {err:#}")),
    }

    for round in 0..rounds {
        let before = env::open_db()
            .and_then(|c| env::max_seq(&c, &uid))
            .unwrap_or(0);
        let info = match env::daemon_info() {
            Ok(info) => info,
            Err(err) => return Outcome::failed(format!("daemon unreachable before kill: {err:#}")),
        };

        // Traffic in flight when the axe falls: some of these posts land, some
        // hit a socket that is already gone. Both are correct behaviour, and
        // the point is that neither corrupts the log.
        let load = tokio::task::spawn_blocking({
            let uid = uid.clone();
            let name = target.session.session_id.clone();
            move || {
                let mut posted = 0;
                let mut dropped = 0;
                for i in 0..40 {
                    let ok = env::post_hook(&HookPost {
                        session_id: name.clone(),
                        session_uid: Some(uid.clone()),
                        event: "PreToolUse".into(),
                        payload: serde_json::json!({
                            "hook_event_name": "PreToolUse",
                            "tool_name": "Bash",
                            "tool_input": {"command": "echo soak"},
                            "tool_use_id": format!("soak-kill-{round}-{i}"),
                        }),
                        wait: false,
                    });
                    if ok {
                        posted += 1;
                    } else {
                        dropped += 1;
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                (posted, dropped)
            }
        });

        // Somewhere inside the burst, not at a fixed offset: a kill that always
        // lands at the same point only ever tests one interleaving.
        let delay = 80 + (protocol::time::now_unix_ms() as u64 % 400);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        if let Err(err) = env::kill_9(info.pid) {
            return Outcome::failed(format!("could not kill pid {}: {err:#}", info.pid));
        }

        let (round_posted, round_dropped) = load.await.unwrap_or((0, 0));
        posted += round_posted;
        dropped += round_dropped;

        match env::wait_for_daemon(env::RECOVERY_TIMEOUT) {
            Ok((info, took)) => {
                recoveries.push(took);
                if !info.is_launchd_managed() {
                    notes.push(format!(
                        "round {round}: the daemon came back but does not report our \
                         launchd label ({:?})",
                        info.launchd_label
                    ));
                }
            }
            Err(err) => return Outcome::failed(format!("round {round}: {err:#}")),
        }

        let after = env::open_db()
            .and_then(|c| env::max_seq(&c, &uid))
            .unwrap_or(0);
        if after < before {
            return Outcome::failed(format!(
                "round {round}: max_seq went backwards ({before} -> {after})"
            ));
        }
    }

    // The supervisor has to have reconnected, or the session is alive but
    // unreachable — which would be a silent half-failure.
    let reattached = env::wait_until(SETTLE, |_| {
        Ok(env::sessions()?
            .iter()
            .any(|s| s.session_uid == uid && s.link == protocol::event::Link::Attached))
    });
    match reattached {
        Ok(took) => notes.push(format!(
            "supervisor re-attached in {:.1}s",
            took.as_secs_f64()
        )),
        Err(err) => return Outcome::failed(format!("supervisor never re-attached: {err:#}")),
    }

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let integrity = match env::check_integrity(&conn) {
        Ok(integrity) => integrity,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };

    let slowest = recoveries.iter().max().copied().unwrap_or_default();
    let fastest = recoveries.iter().min().copied().unwrap_or_default();
    notes.push(format!(
        "{rounds} kills · {posted} hooks landed · {dropped} refused while down \
         (fail-open) · recovery {:.1}s–{:.1}s · {} runs, {} events checked",
        fastest.as_secs_f64(),
        slowest.as_secs_f64(),
        integrity.runs,
        integrity.events
    ));
    if !integrity.is_clean() {
        return Outcome::failed(format!(
            "log damaged — gaps: {:?}, duplicates: {:?}",
            integrity.gaps, integrity.duplicates
        ))
        .with_notes(notes);
    }
    Outcome::passed("no gaps, no duplicates").with_notes(notes)
}

// -------------------------------------------------- (b) duplicate-hook storm

/// The same hook payload, fifty times. Exactly one event.
///
/// This is the property that lets every other recovery path be careless: a
/// re-scan after a restart, a hook retried by a shell wrapper and a transcript
/// line seen twice all reduce to "the same fact arrived again", and the answer
/// has to be one row and one `seq`.
pub async fn duplicate_hooks(target: &Target, replays: u32) -> Outcome {
    let uid = target.session.session_uid.clone();
    let tool_use_id = format!("soak-dup-{}", protocol::time::now_unix_ms());
    let source_event_id = format!("pre:{tool_use_id}");

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let seq_before = env::max_seq(&conn, &uid).unwrap_or(0);
    let count_before = env::count_events(&conn, &uid).unwrap_or(0);
    drop(conn);

    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "echo duplicate-storm"},
        "tool_use_id": tool_use_id,
    });
    let post = HookPost {
        session_id: target.session.session_id.clone(),
        session_uid: Some(uid.clone()),
        event: "PreToolUse".into(),
        payload,
        wait: false,
    };

    // Concurrently, not in a loop: serial replays would be absorbed by any
    // in-memory cache, whereas fifty at once is the case where two ingests race
    // for the same `seq` and only the transaction can arbitrate.
    let mut tasks = Vec::new();
    for _ in 0..replays {
        let post = post.clone();
        tasks.push(tokio::task::spawn_blocking(move || env::post_hook(&post)));
    }
    let mut delivered = 0;
    for task in tasks {
        if task.await.unwrap_or(false) {
            delivered += 1;
        }
    }

    if let Err(err) = env::wait_until(SETTLE, |conn| {
        Ok(env::count_by_source_event_id(conn, &uid, &source_event_id)? >= 1)
    }) {
        return Outcome::failed(format!("the replayed hook was never recorded: {err:#}"));
    }

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let stored = env::count_by_source_event_id(&conn, &uid, &source_event_id).unwrap_or(0);
    let seq_after = env::max_seq(&conn, &uid).unwrap_or(0);
    let count_after = env::count_events(&conn, &uid).unwrap_or(0);
    let notes = vec![format!(
        "{delivered}/{replays} posts delivered · {stored} event(s) stored · \
         seq {seq_before}->{seq_after} · count {count_before}->{count_after}"
    )];

    if stored != 1 {
        return Outcome::failed(format!("{stored} events for one fact")).with_notes(notes);
    }
    // A dropped duplicate must not burn a sequence number either: the log's
    // gap-free promise is exactly "seq counts facts, not attempts".
    if seq_after != count_after {
        return Outcome::failed(format!(
            "max_seq {seq_after} != count {count_after}: a duplicate burnt a number"
        ))
        .with_notes(notes);
    }
    Outcome::passed("exactly one event, no burnt sequence").with_notes(notes)
}

// --------------------------------------------------- (c) answer replay storm

/// Twenty phones tapping the same card at the same moment.
///
/// A duplicate must return the *original outcome*, never a rejection. The
/// interesting case is not a retry a minute later — the ledger handles that —
/// but a retry that arrives while the first answer is still being typed into the
/// TTY. That used to come back as a rejection.
pub async fn answer_storm(target: &Target, taps: u32) -> Outcome {
    let uid = target.session.session_uid.clone();
    let prompt_id = format!("soak-{}", protocol::time::now_unix_ms());
    let tool_name = "Bash";
    let tool_input = serde_json::json!({"command": "echo soak-approval"});
    let payload_hash = protocol::hash::approval_payload_hash(tool_name, &tool_input);
    // The id the daemon derives when a PermissionRequest arrives with no
    // preceding PreToolUse to correlate against. Recomputed here rather than
    // read back, so a change to that derivation fails this scenario loudly.
    let request_id = format!("pr-{prompt_id}-{}", &payload_hash[..16]);

    // The composer has to be accepting input, or every answer is legitimately
    // refused and the scenario measures nothing.
    let mut phone = match target.phone().await {
        Ok(phone) => phone,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    match phone.capture(&uid, 40).await {
        Ok(pane) => {
            let ready = protocol::ipc::PromptPresence::InputBox
                .find_match(&pane, None)
                .is_some();
            if !ready {
                return Outcome::skipped(
                    "the session's composer is busy; an answer would be refused for the \
                     right reason and prove nothing",
                );
            }
        }
        Err(err) => return Outcome::failed(format!("could not read the pane: {err:#}")),
    }

    let posted = env::post_hook(&HookPost {
        session_id: target.session.session_id.clone(),
        session_uid: Some(uid.clone()),
        event: "PermissionRequest".into(),
        payload: serde_json::json!({
            "hook_event_name": "PermissionRequest",
            "prompt_id": prompt_id,
            "tool_name": tool_name,
            "tool_input": tool_input,
        }),
        wait: false,
    });
    if !posted {
        return Outcome::failed("could not post the approval");
    }
    if let Err(err) = env::wait_until(SETTLE, |_| {
        Ok(env::sessions()?
            .iter()
            .any(|s| s.session_uid == uid && s.blocked_on.iter().any(|id| id == &request_id)))
    }) {
        return Outcome::failed(format!("the approval never became answerable: {err:#}"));
    }

    // One connection each, so the taps are genuinely concurrent rather than
    // pipelined down a single socket in order.
    let mut tasks = Vec::new();
    for _ in 0..taps {
        let (host, port, token) = (target.host.clone(), target.port, target.token.clone());
        let (request_id, payload_hash, uid) =
            (request_id.clone(), payload_hash.clone(), uid.clone());
        tasks.push(tokio::spawn(async move {
            let mut phone = Phone::connect(&host, port, &token).await?;
            phone
                .answer(
                    &request_id,
                    &payload_hash,
                    // Typed into the composer, which is where a real answer for
                    // this build lands too. The text is a cheap prompt.
                    AnswerDecision::Text {
                        text: "reply with exactly: ok".into(),
                    },
                    &uid,
                )
                .await
        }));
    }

    let mut applied = Vec::new();
    let mut duplicates = Vec::new();
    let mut rejected = Vec::new();
    let mut errors = Vec::new();
    for task in tasks {
        match task.await {
            Ok(Ok(AnswerResult::Applied { outcome })) => applied.push(outcome),
            Ok(Ok(AnswerResult::Duplicate { outcome, .. })) => duplicates.push(outcome),
            Ok(Ok(AnswerResult::Rejected { reason })) => rejected.push(reason),
            Ok(Err(err)) => errors.push(format!("{err:#}")),
            Err(err) => errors.push(format!("task failed: {err}")),
        }
    }

    let mut notes = vec![format!(
        "{taps} concurrent taps · {} applied · {} duplicate · {} rejected · {} errored",
        applied.len(),
        duplicates.len(),
        rejected.len(),
        errors.len()
    )];
    if !rejected.is_empty() {
        notes.push(format!("rejections: {:?}", dedup(&rejected)));
    }
    if !errors.is_empty() {
        notes.push(format!("errors: {:?}", dedup(&errors)));
    }

    if applied.len() != 1 {
        return Outcome::failed(format!(
            "{} answers applied; exactly one must reach the agent",
            applied.len()
        ))
        .with_notes(notes);
    }
    let original = &applied[0];
    if duplicates.len() as u32 != taps - 1 {
        return Outcome::failed(format!(
            "{} of {} retries came back as duplicates; the rest were not idempotent",
            duplicates.len(),
            taps - 1
        ))
        .with_notes(notes);
    }
    if let Some(wrong) = duplicates.iter().find(|o| o.decision != original.decision) {
        return Outcome::failed(format!(
            "a duplicate returned a different decision: {:?} vs {:?}",
            wrong.decision, original.decision
        ))
        .with_notes(notes);
    }
    if duplicates
        .iter()
        .any(|o| o.resolved_at != original.resolved_at)
    {
        return Outcome::failed("a duplicate returned a different resolution time")
            .with_notes(notes);
    }
    Outcome::passed("one applied, the rest replayed the original outcome").with_notes(notes)
}

fn dedup(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = values.to_vec();
    out.sort();
    out.dedup();
    out
}

// --------------------------------------------------------------- (d) WSS flap

/// Connect, subscribe from a random point, read, disconnect. Thirty times.
///
/// A phone on a train does this all day. The invariant is that a replay from
/// *any* watermark is contiguous and monotonic — not merely that it eventually
/// converges, because a client cannot see a gap it was never told about.
pub async fn ws_flap(target: &Target, rounds: u32) -> Outcome {
    let uid = target.session.session_uid.clone();
    let mut total_events = 0usize;
    let mut watermarks = Vec::new();
    let start = Instant::now();

    for round in 0..rounds {
        let max = env::open_db()
            .and_then(|conn| env::max_seq(&conn, &uid))
            .unwrap_or(0);
        // Deterministic spread rather than a random-number dependency: every
        // watermark from 0 to max is visited, including both ends.
        let after_seq = if max == 0 {
            0
        } else {
            (round as u64 * (max + 1) / rounds.max(1) as u64).min(max)
        };
        watermarks.push(after_seq);

        let mut phone = match target.phone().await {
            Ok(phone) => phone,
            Err(err) => return Outcome::failed(format!("round {round}: connect: {err:#}")),
        };
        // Every reconnect re-reads the fleet, as the app does on foreground.
        // The run has to still be findable *and* still carry both identities,
        // or a phone that reconnected would be subscribing to a guess.
        match phone.sessions().await {
            Ok(sessions) => {
                let Some(found) = sessions.iter().find(|s| s.session_uid == uid) else {
                    return Outcome::failed(format!(
                        "round {round}: {uid} vanished from the session list"
                    ));
                };
                if found.session_id != target.session.session_id {
                    return Outcome::failed(format!(
                        "round {round}: {uid} is now called {}, was {}",
                        found.session_id, target.session.session_id
                    ));
                }
            }
            Err(err) => return Outcome::failed(format!("round {round}: sessions: {err:#}")),
        }
        let events = match phone
            .subscribe_and_drain(&uid, after_seq, Duration::from_millis(700))
            .await
        {
            Ok(events) => events,
            Err(err) => return Outcome::failed(format!("round {round}: {err:#}")),
        };
        if let Err(err) = check_replay(&events, after_seq, &uid) {
            return Outcome::failed(format!("round {round}: {err:#}"));
        }
        total_events += events.len();
        // Dropped without a close frame on odd rounds: a phone losing signal
        // does not say goodbye, and the server must not care.
        if round % 2 == 0 {
            drop(phone);
        }
    }

    let notes = vec![format!(
        "{rounds} connect/disconnect cycles in {:.1}s · watermarks {}–{} · \
         {total_events} events replayed, all contiguous",
        start.elapsed().as_secs_f64(),
        watermarks.iter().min().copied().unwrap_or(0),
        watermarks.iter().max().copied().unwrap_or(0),
    )];
    Outcome::passed("every replay gap-free and monotonic").with_notes(notes)
}

// ----------------------------------------------------------- (e) tail torture

/// Truncate and rewrite a transcript underneath the tailer.
///
/// The cursor claims "I have consumed up to byte N of this file". An editor, a
/// crash or a `--resume` can make that claim false while the file's identity is
/// unchanged, which is why the cursor also hashes the last line it consumed.
/// This scenario makes the claim false in the three ways that happen in
/// practice and checks that the recovery costs a re-scan and nothing else.
pub async fn tail_torture(target: &Target) -> Outcome {
    let dir = match env::scratch_dir("tail") {
        Ok(dir) => dir,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let transcript = dir.join("soak.jsonl");
    let uid = match protocol::uid::new() {
        Ok(uid) => uid,
        Err(err) => return Outcome::failed(format!("minting a uid: {err:#}")),
    };
    let name = format!("soak-{}", &uid[uid.len() - 6..]);

    let announce = |lines_written: &str| HookPost {
        session_id: name.clone(),
        session_uid: Some(uid.clone()),
        event: "SessionStart".into(),
        payload: serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": lines_written,
            "cwd": dir.to_string_lossy(),
            "transcript_path": transcript.to_string_lossy(),
        }),
        wait: false,
    };

    // Phase 1 — a normal tail.
    let first: Vec<String> = (0..6).map(|i| line(&format!("{uid}-a{i}"))).collect();
    if let Err(err) = std::fs::write(&transcript, first.join("")) {
        return Outcome::failed(format!("writing the transcript: {err}"));
    }
    if !env::post_hook(&announce("phase-1")) {
        return Outcome::failed("could not register the synthetic session");
    }
    if let Err(err) = env::wait_until(SETTLE, |conn| Ok(env::count_events(conn, &uid)? >= 7)) {
        return Outcome::failed(format!(
            "the tailer never picked the transcript up: {err:#}"
        ));
    }

    // Phase 2 — truncate to nothing and rewrite with *different* content. The
    // file keeps its inode, so identity checks pass and only the last-line hash
    // can tell that the cursor's claim is now false.
    let rewritten: Vec<String> = (0..4).map(|i| line(&format!("{uid}-b{i}"))).collect();
    if let Err(err) = std::fs::write(&transcript, rewritten.join("")) {
        return Outcome::failed(format!("rewriting the transcript: {err}"));
    }
    if let Err(err) = env::wait_until(SETTLE, |conn| {
        Ok(env::count_by_source_event_id(conn, &uid, &format!("{uid}-b3"))? == 1)
    }) {
        return Outcome::failed(format!(
            "the tailer did not recover from a rewrite: {err:#}"
        ));
    }

    // Phase 3 — truncate to a *prefix* of the rewritten file and grow again.
    // The replayed prefix must dedup away and only the genuinely new line count.
    let mut regrown = rewritten[..2].to_vec();
    regrown.push(line(&format!("{uid}-c0")));
    if let Err(err) = std::fs::write(&transcript, regrown.join("")) {
        return Outcome::failed(format!("regrowing the transcript: {err}"));
    }
    if let Err(err) = env::wait_until(SETTLE, |conn| {
        Ok(env::count_by_source_event_id(conn, &uid, &format!("{uid}-c0"))? == 1)
    }) {
        return Outcome::failed(format!(
            "the tailer did not recover from a truncation: {err:#}"
        ));
    }

    // Phase 4 — a torn write. Half a JSON object must never be ingested.
    if let Err(err) = std::fs::write(
        &transcript,
        format!("{}{{\"type\":\"user\",\"uu", regrown.join("")),
    ) {
        return Outcome::failed(format!("tearing the transcript: {err}"));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    // Every uuid ever written, exactly once each, plus the session_start the
    // hook itself produced.
    let expected_ids: Vec<String> = (0..6)
        .map(|i| format!("{uid}-a{i}"))
        .chain((0..4).map(|i| format!("{uid}-b{i}")))
        .chain(std::iter::once(format!("{uid}-c0")))
        .collect();
    let mut wrong = Vec::new();
    for id in &expected_ids {
        match env::count_by_source_event_id(&conn, &uid, id) {
            Ok(1) => {}
            Ok(n) => wrong.push(format!("{id} x{n}")),
            Err(err) => wrong.push(format!("{id}: {err}")),
        }
    }
    let total = env::count_events(&conn, &uid).unwrap_or(0);
    let max = env::max_seq(&conn, &uid).unwrap_or(0);

    let notes = vec![format!(
        "{} transcript lines across 4 rewrites · {total} events · max_seq {max} · \
         scratch {}",
        expected_ids.len(),
        transcript.display()
    )];

    let _ = std::fs::remove_dir_all(&dir);
    let _ = target; // the live session is untouched by this scenario, by design

    if !wrong.is_empty() {
        return Outcome::failed(format!(
            "lines ingested the wrong number of times: {wrong:?}"
        ))
        .with_notes(notes);
    }
    if total != max {
        return Outcome::failed(format!(
            "max_seq {max} != count {total}: the rescan burnt sequence numbers"
        ))
        .with_notes(notes);
    }
    Outcome::passed("cursor recovered from truncate, rewrite and a torn line").with_notes(notes)
}

fn line(uuid: &str) -> String {
    format!("{{\"type\":\"user\",\"uuid\":\"{uuid}\"}}\n")
}

// -------------------------------------------------- (f) kill during ingest

/// Write transcript lines, then `kill -9` the daemon *while it is reading them*.
///
/// The exact window that matters. The cursor used to be saved before the events
/// reached the log, so a death in between left a cursor claiming bytes that had
/// never been ingested — and nothing ever re-read them, because the cursor said they
/// were done. The loss is silent and permanent: no gap in `seq`, no duplicate,
/// nothing for the other scenarios to notice. Only "is every line I wrote in the
/// log?" can see it.
///
/// The kill is timed at the poll rather than at a fixed offset, and repeated, so
/// across rounds it lands at different points inside the read-then-ingest path.
pub async fn kill_during_ingest(target: &Target, rounds: u32) -> Outcome {
    let dir = match env::scratch_dir("ingest") {
        Ok(dir) => dir,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let transcript = dir.join("ingest.jsonl");
    let uid = match protocol::uid::new() {
        Ok(uid) => uid,
        Err(err) => return Outcome::failed(format!("minting a uid: {err:#}")),
    };
    let name = format!("soak-{}", &uid[uid.len() - 6..]);

    let announce = HookPost {
        session_id: name.clone(),
        session_uid: Some(uid.clone()),
        event: "SessionStart".into(),
        payload: serde_json::json!({
            "hook_event_name": "SessionStart",
            "cwd": dir.to_string_lossy(),
            "transcript_path": transcript.to_string_lossy(),
        }),
        wait: false,
    };
    if let Err(err) = std::fs::write(&transcript, "") {
        return Outcome::failed(format!("creating the transcript: {err}"));
    }
    if !env::post_hook(&announce) {
        return Outcome::failed("could not register the synthetic session");
    }
    if let Err(err) = env::wait_until(SETTLE, |conn| Ok(env::count_events(conn, &uid)? >= 1)) {
        return Outcome::failed(format!("the session never registered: {err:#}"));
    }

    let mut written: Vec<String> = Vec::new();
    let mut notes = Vec::new();
    let mut recoveries = Vec::new();

    for round in 0..rounds {
        let info = match env::daemon_info() {
            Ok(info) => info,
            Err(err) => return Outcome::failed(format!("daemon unreachable: {err:#}")),
        };

        // Appended in one write, so the tailer sees whole lines and the only
        // thing under test is whether it survives being killed after reading
        // them and before recording them.
        let batch: Vec<String> = (0..8).map(|i| format!("{uid}-r{round}i{i}")).collect();
        let bytes: String = batch.iter().map(|id| line(id)).collect();
        if let Err(err) = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .and_then(|mut file| std::io::Write::write_all(&mut file, bytes.as_bytes()))
        {
            return Outcome::failed(format!("appending to the transcript: {err}"));
        }
        written.extend(batch);

        // Inside the tail poll (250ms by default, and FSEvents fires sooner), so
        // the axe falls while the scan is in flight rather than long after it.
        let delay = 5 + (protocol::time::now_unix_ms() as u64 % 240);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        if let Err(err) = env::kill_9(info.pid) {
            return Outcome::failed(format!("could not kill pid {}: {err:#}", info.pid));
        }
        match env::wait_for_daemon(env::RECOVERY_TIMEOUT) {
            Ok((_, took)) => recoveries.push(took),
            Err(err) => return Outcome::failed(format!("round {round}: {err:#}")),
        }
    }

    // Every line, exactly once. The daemon has had every chance to re-read them.
    if let Err(err) = env::wait_until(SETTLE, |conn| {
        let last = written.last().cloned().unwrap_or_default();
        Ok(env::count_by_source_event_id(conn, &uid, &last)? == 1)
    }) {
        return Outcome::failed(format!("the last batch never landed: {err:#}"));
    }

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let mut missing = Vec::new();
    let mut duplicated = Vec::new();
    for id in &written {
        match env::count_by_source_event_id(&conn, &uid, id) {
            Ok(1) => {}
            Ok(0) => missing.push(id.clone()),
            Ok(n) => duplicated.push(format!("{id} x{n}")),
            Err(err) => missing.push(format!("{id}: {err}")),
        }
    }
    let total = env::count_events(&conn, &uid).unwrap_or(0);
    let max = env::max_seq(&conn, &uid).unwrap_or(0);
    let slowest = recoveries.iter().max().copied().unwrap_or_default();
    notes.push(format!(
        "{rounds} kills timed inside the tail poll · {} lines written · {total} events · \
         max_seq {max} · slowest recovery {:.1}s",
        written.len(),
        slowest.as_secs_f64()
    ));

    let _ = std::fs::remove_dir_all(&dir);
    let _ = target;

    if !missing.is_empty() {
        return Outcome::failed(format!(
            "{} transcript line(s) were skipped permanently — the cursor claimed bytes that \
             never reached the log: {:?}",
            missing.len(),
            &missing[..missing.len().min(5)]
        ))
        .with_notes(notes);
    }
    if !duplicated.is_empty() {
        return Outcome::failed(format!("lines ingested twice: {duplicated:?}")).with_notes(notes);
    }
    if total != max {
        return Outcome::failed(format!("max_seq {max} != count {total}")).with_notes(notes);
    }
    Outcome::passed("every transcript line survived a kill mid-ingest").with_notes(notes)
}

// ------------------------------------------------- (g) concurrent commits

/// Hammer one session from many connections at once and watch a socket read it.
///
/// Committing a `seq` and publishing it are two steps; without a gate
/// around both, a task holding seq 1 can be overtaken by one holding seq 2 —
/// and a socket that has accepted 2 can never accept 1, so the event is dropped
/// on that connection with nothing said. The subscriber here asserts strict
/// succession from its own watermark, which is the only vantage point the defect
/// is visible from: the database is perfectly consistent either way.
pub async fn concurrent_commits(target: &Target, writers: u32) -> Outcome {
    let uid = target.session.session_uid.clone();
    let name = target.session.session_id.clone();
    let tag = protocol::time::now_unix_ms();

    let mut phone = match target.phone().await {
        Ok(phone) => phone,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let from = env::open_db()
        .and_then(|conn| env::max_seq(&conn, &uid))
        .unwrap_or(0);
    if let Err(err) = phone
        .subscribe_and_drain(&uid, from, Duration::from_millis(500))
        .await
    {
        return Outcome::failed(format!("subscribe: {err:#}"));
    }

    // Concurrent, from separate blocking threads and separate sockets, so the
    // ingests genuinely race rather than being pipelined in order.
    let mut tasks = Vec::new();
    for i in 0..writers {
        let (uid, name) = (uid.clone(), name.clone());
        tasks.push(tokio::task::spawn_blocking(move || {
            env::post_hook(&HookPost {
                session_id: name,
                session_uid: Some(uid),
                event: "PreToolUse".into(),
                payload: serde_json::json!({
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Bash",
                    "tool_input": {"command": "echo order"},
                    "tool_use_id": format!("soak-order-{tag}-{i}"),
                }),
                wait: false,
            })
        }));
    }
    let mut posted = 0u32;
    for task in tasks {
        if task.await.unwrap_or(false) {
            posted += 1;
        }
    }

    // Drain what the socket actually received, in the order it received it.
    let mut received: Vec<u64> = Vec::new();
    let mut resyncs = 0usize;
    loop {
        match phone.next_message(Duration::from_millis(1_500)).await {
            Ok(protocol::ws::ServerMessage::Event { event }) => {
                if event.session_uid != uid {
                    continue;
                }
                if event.kind == protocol::event::EventKind::Resync {
                    resyncs += 1;
                    continue;
                }
                received.push(event.seq);
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    let notes = vec![format!(
        "{posted}/{writers} concurrent commits · {} events delivered live from seq {from} · \
         {resyncs} resync marker(s)",
        received.len()
    )];

    // Strictly successive from the watermark: no reordering, no repeats, and —
    // the failure this exists for — nothing skipped.
    let mut expected = from + 1;
    for seq in &received {
        if *seq != expected {
            return Outcome::failed(format!(
                "a socket saw seq {seq} where {expected} was due: an event was published out \
                 of order and would have been lost on this connection"
            ))
            .with_notes(notes);
        }
        expected += 1;
    }
    if resyncs > 0 {
        return Outcome::failed(format!(
            "{resyncs} resync marker(s) during live delivery: the daemon published out of order \
             and had to re-read the log to recover"
        ))
        .with_notes(notes);
    }
    if received.is_empty() {
        return Outcome::skipped("no live events arrived; nothing was measured").with_notes(notes);
    }
    Outcome::passed("every commit reached the socket in sequence order").with_notes(notes)
}

/// Pick the run to attack: the newest attached one, or a named reference.
///
/// A **name** has to be resolved the way the daemon resolves it — attached
/// first, then newest — because `cc-1` may name several runs and the dead ones
/// have no supervisor. Picking the first match by list order is how the first
/// version of this harness spent two minutes proving that a session which
/// exited last week does not answer.
pub fn choose_target(reference: Option<&str>) -> Result<SessionSummary> {
    let mut sessions = env::sessions().context("asking ccd for its sessions")?;
    // Attached before detached, then newest first. A ULID sorts by mint time,
    // so the identity is its own tiebreak.
    sessions.sort_by(|a, b| {
        let attached = |s: &SessionSummary| s.link == protocol::event::Link::Attached;
        attached(b)
            .cmp(&attached(a))
            .then_with(|| b.session_uid.cmp(&a.session_uid))
    });

    if let Some(reference) = reference {
        return sessions
            .into_iter()
            .find(|s| s.session_uid == reference || s.session_id == reference)
            .ok_or_else(|| anyhow::anyhow!("no session matches {reference:?}"));
    }
    match sessions
        .into_iter()
        .find(|s| s.link == protocol::event::Link::Attached)
    {
        Some(session) => Ok(session),
        None => bail!(
            "no attached session to soak against; start one with \
             `codeconnect claude` (or soak/run.sh, which starts its own)"
        ),
    }
}

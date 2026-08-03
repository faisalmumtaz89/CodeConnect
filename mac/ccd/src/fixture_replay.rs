//! Replay tests against recorded payloads from live `claude` sessions.
//!
//! Fixtures over mocks: these are the exact bytes Claude Code produced on a real
//! machine, driven through the production ingest path. Their job is to fail
//! loudly when a Claude Code release changes a payload shape. That is the single
//! largest standing risk to this design — everything here is built on a format
//! nobody else promises to keep stable — and it is the failure that silently
//! killed agentapi. A mock would encode our belief about the format; a fixture
//! encodes what the format actually was.

use std::path::PathBuf;

use protocol::event::{EventKind, Source};
use protocol::hook::{HookEventName, HookInput};

use crate::store::Store;

fn fixture(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(relative)
}

fn load_hook_payloads(relative: &str) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(fixture(relative))
        .unwrap_or_else(|err| panic!("reading {relative}: {err}"));
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("fixture line must be JSON"))
        .collect()
}

/// The run every fixture is replayed into. A real one, minted the same way a
/// live session's is, so the replay exercises the production key shape.
fn run() -> protocol::event::SessionKey {
    protocol::event::SessionKey::new(protocol::uid::new().unwrap(), "cc-1")
}

fn temp_store() -> Store {
    // The counter is load-bearing: tests run in parallel threads, and a
    // millisecond timestamp alone lets two of them share one database.
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "ccd-fixture-{}-{}-{}.db",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        protocol::time::now_unix_ms()
    ));
    let _ = std::fs::remove_file(&path);
    Store::open(&path).unwrap()
}

fn seed_session(store: &Store, run: &protocol::event::SessionKey) {
    let now = protocol::time::now_rfc3339();
    store
        .upsert_session(&crate::store::SessionRow {
            session_uid: run.uid.clone(),
            session_id: run.name.clone(),
            tmux_session: run.name.clone(),
            tmux_socket: protocol::TMUX_SOCKET_NAME.into(),
            cwd: "/tmp".into(),
            claude_session_id: None,
            transcript_path: None,
            lifecycle: protocol::event::Lifecycle::Live,
            created_at: now.clone(),
            updated_at: now,
        })
        .unwrap()
        .assert_present();
}

#[test]
fn every_recorded_hook_payload_parses() {
    let payloads = load_hook_payloads("hooks/acceptance-run.jsonl");
    assert!(payloads.len() >= 8, "fixture looks truncated");

    let mut seen = Vec::new();
    for payload in &payloads {
        let input: HookInput =
            serde_json::from_value(payload.clone()).expect("HookInput must tolerate real payloads");
        let name = input.event_name();
        assert!(
            !matches!(name, HookEventName::Other(_)),
            "unrecognised hook event {:?} — Claude Code changed its schema",
            input.hook_event_name
        );
        // Every payload must identify its session and transcript, or the daemon
        // cannot attribute the fact or find the JSONL to tail.
        assert!(input.session_id.is_some(), "missing session_id: {payload}");
        assert!(input.transcript_path.is_some(), "missing transcript_path");
        seen.push(name);
    }

    for required in [
        HookEventName::SessionStart,
        HookEventName::PreToolUse,
        HookEventName::PermissionRequest,
        HookEventName::PostToolUse,
        HookEventName::Notification,
        HookEventName::Stop,
    ] {
        assert!(seen.contains(&required), "fixture is missing {required:?}");
    }
}

#[test]
fn permission_request_still_lacks_a_tool_use_id() {
    // The premise of the correlation logic. If a future Claude Code starts
    // sending one, this test fails and the correlation can be simplified.
    let payloads = load_hook_payloads("hooks/live-hook-payloads.jsonl");
    let mut checked = 0;
    for payload in payloads {
        let input: HookInput = serde_json::from_value(payload).unwrap();
        if input.event_name() == HookEventName::PermissionRequest {
            assert_eq!(
                input.tool_use_id, None,
                "PermissionRequest now carries tool_use_id — simplify the correlation"
            );
            assert!(input.tool_name.is_some());
            assert!(input.tool_input.is_some());
            checked += 1;
        }
    }
    assert!(checked > 0, "no PermissionRequest payloads in the fixture");
}

#[test]
fn permission_request_correlates_to_its_pretooluse() {
    let payloads = load_hook_payloads("hooks/acceptance-run.jsonl");
    let mut pre_key = None;
    let mut pre_id = None;
    let mut perm_key = None;

    for payload in payloads {
        let input: HookInput = serde_json::from_value(payload).unwrap();
        match input.event_name() {
            HookEventName::PreToolUse => {
                pre_key = crate::state::correlation_key("cc-1", &input);
                pre_id = input.tool_use_id.clone();
            }
            HookEventName::PermissionRequest => {
                perm_key = crate::state::correlation_key("cc-1", &input);
            }
            _ => {}
        }
    }

    assert!(pre_id.is_some(), "PreToolUse must carry a tool_use_id");
    assert_eq!(
        pre_key, perm_key,
        "the approval must join to its tool call, or idempotency has no key"
    );
    assert!(perm_key.is_some());
}

#[test]
fn notification_carries_the_push_trigger() {
    let payloads = load_hook_payloads("hooks/acceptance-run.jsonl");
    let types: Vec<String> = payloads
        .iter()
        .filter_map(|payload| {
            let input: HookInput = serde_json::from_value(payload.clone()).ok()?;
            (input.event_name() == HookEventName::Notification)
                .then_some(input.notification_type)?
        })
        .collect();
    assert!(
        types.iter().any(|kind| kind == "permission_prompt"),
        "no permission_prompt notification in the fixture: {types:?}"
    );
}

#[test]
fn recorded_approvals_classify_from_their_real_payload_shape() {
    // The classifier reads `tool_input.command`. That field name is Claude
    // Code's, not ours, so it is checked against recorded payloads rather than
    // against a hand-written input: if a release renames it, the classifier
    // would silently start calling every Bash command `medium` and this fails
    // instead.
    use protocol::risk::{classify, RiskClass};

    let mut classified = 0;
    for payload in load_hook_payloads("hooks/live-hook-payloads.jsonl") {
        let input: HookInput = serde_json::from_value(payload).unwrap();
        if input.event_name() != HookEventName::PermissionRequest {
            continue;
        }
        let tool_name = input.tool_name.clone().unwrap();
        let tool_input = input.tool_input.clone().unwrap();
        let assessment = classify(&tool_name, &tool_input);

        // Every recorded approval is a plain `touch`: not destructive, and not
        // a read-only tool either.
        assert_eq!(
            assessment.class,
            RiskClass::Medium,
            "{tool_name} {tool_input} classified {assessment:?}"
        );
        assert_eq!(assessment.matched_pattern, None);

        // The command really did reach the classifier, rather than the payload
        // silently presenting as an unscannable shape.
        assert!(
            tool_input.get("command").and_then(|v| v.as_str()).is_some(),
            "PermissionRequest no longer carries tool_input.command: {tool_input}"
        );
        classified += 1;
    }
    assert!(
        classified > 0,
        "no PermissionRequest payloads in the fixture"
    );

    // And the same recorded shape, with only the command swapped for a
    // destructive one, must come out high — proving the path is live.
    let hostile = serde_json::json!({
        "command": "rm -rf /private/tmp/x",
        "description": "Create empty file",
    });
    let assessment = classify("Bash", &hostile);
    assert_eq!(assessment.class, RiskClass::High);
    assert_eq!(assessment.matched_pattern.as_deref(), Some("rm -rf"));
}

#[test]
fn replaying_hook_payloads_twice_is_idempotent() {
    let store = temp_store();
    let run = run();
    seed_session(&store, &run);
    let payloads = load_hook_payloads("hooks/acceptance-run.jsonl");

    let build = || -> Vec<protocol::event::PendingEvent> {
        payloads
            .iter()
            .map(|payload| {
                let input: HookInput = serde_json::from_value(payload.clone()).unwrap();
                crate::state::hook_event(&run, &input.event_name(), payload, &input)
            })
            .collect()
    };

    let first = store.append_batch(&build()).unwrap();
    assert_eq!(first.len(), payloads.len(), "first pass must ingest all");

    // A restart re-delivering the same facts must add nothing and renumber
    // nothing. Events without a natural id (notifications) legitimately repeat,
    // so only the identified ones are expected to dedup.
    let second = store.append_batch(&build()).unwrap();
    let identified = build()
        .iter()
        .filter(|event| event.source_event_id.is_some())
        .count();
    assert_eq!(
        second.len(),
        payloads.len() - identified,
        "identified facts must dedup on replay"
    );

    let seqs: Vec<u64> = store
        .events_after(&run.uid, 0, 1000)
        .unwrap()
        .iter()
        .map(|event| event.seq)
        .collect();
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "seq must stay gap-free across a replay");
}

#[test]
fn real_transcript_maps_to_known_kinds_and_dedups() {
    let store = temp_store();
    let run = run();
    // The row first: `append_batch_with_cursor` refuses a batch for a session
    // that is not in `sessions`, so that a tail poll still in flight when a run
    // is deleted cannot file events under a uid nobody can name.
    seed_session(&store, &run);
    let path = fixture("transcript/session-sample.jsonl");
    let path = path.to_str().unwrap();

    let scan = crate::tailer::scan_file(&store, &run, path)
        .unwrap()
        .expect("the sample transcript must yield events");
    assert!(scan.events.len() >= 10, "only {} events", scan.events.len());

    // Through the production path: events and cursor in one transaction.
    let ingested = store
        .append_batch_with_cursor(&run.uid, &scan.events, &scan.cursor)
        .unwrap();
    assert_eq!(ingested.len(), scan.events.len());

    let kinds: Vec<EventKind> = store
        .events_after(&run.uid, 0, 1000)
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(kinds.contains(&EventKind::UserMessage), "{kinds:?}");
    assert!(kinds.contains(&EventKind::AgentMessage), "{kinds:?}");
    // Unknown entry types must survive as `Other`, never be dropped.
    assert!(
        kinds.iter().any(|kind| matches!(kind, EventKind::Other(_))),
        "unknown transcript entries must be preserved: {kinds:?}"
    );

    for event in &scan.events {
        assert_eq!(event.source, Source::Transcript);
        assert!(event.source_event_id.is_some(), "every fact needs identity");
    }

    // A rescan from the same cursor yields nothing new.
    assert!(crate::tailer::scan_file(&store, &run, path)
        .unwrap()
        .is_none());
}

#[test]
fn transcript_and_hooks_coexist_without_collapsing_each_other() {
    // The same tool call is reported by both planes. They must both survive:
    // the hook is timely, the transcript carries the content.
    let store = temp_store();
    let run = run();

    let hooks: Vec<_> = load_hook_payloads("hooks/acceptance-run.jsonl")
        .iter()
        .map(|payload| {
            let input: HookInput = serde_json::from_value(payload.clone()).unwrap();
            crate::state::hook_event(&run, &input.event_name(), payload, &input)
        })
        .collect();
    let hook_count = store.append_batch(&hooks).unwrap().len();

    let path = fixture("transcript/session-sample.jsonl");
    let scan = crate::tailer::scan_file(&store, &run, path.to_str().unwrap())
        .unwrap()
        .unwrap();
    let transcript_count = store.append_batch(&scan.events).unwrap().len();

    assert_eq!(
        store.max_seq(&run.uid).unwrap() as usize,
        hook_count + transcript_count,
        "the two planes must not dedup against each other"
    );
}

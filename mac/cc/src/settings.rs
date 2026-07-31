//! Generated per-session `--settings` that installs the control plane.
//!
//! Two properties matter more than anything else here:
//!
//! * **The user's own settings are untouched.** `--settings` is additive, so
//!   `bypassPermissions` and everything else in `~/.claude/settings.json` keeps
//!   working. CodeConnect adds hooks; it does not have a permission model.
//! * **Matchers are omitted, not `"*"`.** Claude Code treats a matcher as an
//!   unanchored JS regex unless it is purely `[A-Za-z0-9_\- ,|]`, so `"*"` is a
//!   malformed quantifier rather than a wildcard. Omitting the field is the
//!   documented match-everything form.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use protocol::config::Config;
use serde_json::json;

/// Every event CodeConnect wires. Whichever one is configured as the gate is
/// installed with `--gate` instead; the rest are fire-and-forget observers.
///
/// `PermissionRequest` is in this list rather than only being installed when it
/// is the gate: it is the single event that means "a human is about to be
/// asked", and it carries the exact `tool_input` and `permission_suggestions`
/// the phone needs to render a card. Losing it because the gate moved would
/// silently remove the product's whole reason to exist.
///
/// `PreToolUse` is observability by default: it fires on *every* call including
/// auto-approved ones, so gating on it would put a phone round-trip in front of
/// every file read. It also fails open on timeout (measured), which makes it the
/// wrong place for a decision even when one is wanted.
const OBSERVE_EVENTS: &[&str] = &[
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "SessionStart",
    "Stop",
];

pub struct SettingsPlan {
    pub path: PathBuf,
}

/// Locate `cc-hook` next to this binary, which is correct for both a cargo
/// target directory and an installed prefix.
pub fn hook_binary() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("CODECONNECT_HOOK_BIN") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
    }
    let current = std::env::current_exe().context("locating the cc binary")?;
    if let Some(dir) = current.parent() {
        let candidate = dir.join("cc-hook");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let installed = protocol::root_dir().join("bin").join("cc-hook");
    if installed.is_file() {
        return Ok(installed);
    }
    anyhow::bail!(
        "cc-hook not found next to {} or in {}; \
         build the workspace or set CODECONNECT_HOOK_BIN",
        current.display(),
        installed.display()
    )
}

/// Write the session's settings file and return where it landed.
///
/// The file lives under the session's *uid*, not its tmux name. `cc-1` is
/// reused, and a settings file overwritten by the next session would silently
/// re-point a still-running agent's hooks at another run's identity.
pub fn write_for_session(
    session_id: &str,
    session_uid: &str,
    config: &Config,
) -> Result<SettingsPlan> {
    let hook_bin = hook_binary()?;
    let dir = protocol::sessions_dir().join(format!("{session_id}-{session_uid}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("settings.json");

    let document = build(session_id, session_uid, config, &hook_bin);
    let encoded = serde_json::to_string_pretty(&document)?;
    std::fs::write(&path, encoded).with_context(|| format!("writing {}", path.display()))?;
    Ok(SettingsPlan { path })
}

fn build(
    session_id: &str,
    session_uid: &str,
    config: &Config,
    hook_bin: &Path,
) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();

    // The gate: the one event where the agent waits for us.
    if let Some(gate) = config.gate_event() {
        let gate_name = gate.as_str().to_string();
        // Claude kills the hook at this timeout; cc-hook self-bounds strictly
        // earlier, so the timeout is a backstop that should never be reached.
        let timeout_secs = config.gate_timeout_ms.div_ceil(1000) + 10;
        hooks.insert(
            gate_name.clone(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": gate_command(session_id, session_uid, &gate_name, config, hook_bin),
                    "timeout": timeout_secs,
                }]
            }]),
        );
    }

    for event in OBSERVE_EVENTS {
        if hooks.contains_key(*event) {
            continue; // already installed as the gate
        }
        hooks.insert(
            (*event).to_string(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": observe_command(session_id, session_uid, event, config, hook_bin),
                    // Fire-and-forget: cc-hook returns in well under a second,
                    // and a short timeout bounds the damage if it ever does not.
                    "timeout": 10,
                }]
            }]),
        );
    }

    json!({ "hooks": hooks })
}

fn gate_command(
    session_id: &str,
    session_uid: &str,
    event: &str,
    config: &Config,
    hook_bin: &Path,
) -> String {
    let mut parts = vec![
        shell_quote(&hook_bin.to_string_lossy()),
        "--session".into(),
        shell_quote(session_id),
        "--session-uid".into(),
        shell_quote(session_uid),
        "--event".into(),
        shell_quote(event),
        "--gate".into(),
        "--gate-timeout-ms".into(),
        config.gate_timeout_ms.to_string(),
        "--connect-timeout-ms".into(),
        config.connect_timeout_ms.to_string(),
    ];
    if config.unreachable_ask {
        parts.push("--unreachable-ask".into());
    }
    parts.join(" ")
}

fn observe_command(
    session_id: &str,
    session_uid: &str,
    event: &str,
    config: &Config,
    hook_bin: &Path,
) -> String {
    [
        shell_quote(&hook_bin.to_string_lossy()),
        "--session".into(),
        shell_quote(session_id),
        "--session-uid".into(),
        shell_quote(session_uid),
        "--event".into(),
        shell_quote(event),
        "--connect-timeout-ms".into(),
        config.connect_timeout_ms.to_string(),
    ]
    .join(" ")
}

/// Single-quote for `/bin/sh`. Claude runs hook commands through a shell, and
/// the paths involved are user-controlled (`$HOME`, a cargo target directory),
/// so quoting is not optional.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "01K1B3XQ8ZC0DE5FGH7JKMNPQR";

    fn plan(config: &Config) -> serde_json::Value {
        build("cc-1", UID, config, Path::new("/opt/cc/bin/cc-hook"))
    }

    #[test]
    fn wires_gate_and_observability_without_touching_user_settings() {
        let document = plan(&Config::default());
        let hooks = document["hooks"].as_object().unwrap();
        for event in [
            "PermissionRequest",
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "SessionStart",
            "Stop",
        ] {
            assert!(hooks.contains_key(event), "missing {event}");
        }
        // Nothing but hooks: no permission rules, no mode overrides.
        assert_eq!(document.as_object().unwrap().len(), 1);
    }

    #[test]
    fn only_the_gate_event_waits() {
        let document = plan(&Config::default());
        let gate = document["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(gate.contains("--gate"), "{gate}");

        let observe = document["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            !observe.contains("--gate"),
            "PreToolUse must never block the agent: {observe}"
        );
    }

    #[test]
    fn matchers_are_omitted_rather_than_a_bogus_wildcard() {
        let document = plan(&Config::default());
        let encoded = document.to_string();
        assert!(
            !encoded.contains("matcher"),
            "an omitted matcher is the match-all form; \"*\" is an invalid regex: {encoded}"
        );
    }

    #[test]
    fn claude_timeout_is_a_backstop_beyond_our_own_deadline() {
        let config = Config {
            gate_timeout_ms: 120_000,
            ..Config::default()
        };
        let document = plan(&config);
        let timeout = document["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"]
            .as_u64()
            .unwrap();
        assert!(
            timeout > 120,
            "claude must not kill the hook first: {timeout}"
        );
    }

    #[test]
    fn gate_can_be_switched_to_pretooluse() {
        let config = Config {
            gate_hook: "PreToolUse".into(),
            ..Config::default()
        };
        let document = plan(&config);
        let command = document["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("--gate"), "{command}");
        // And it must not also be installed as a plain observer.
        assert_eq!(document["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn gate_can_be_disabled_entirely_without_losing_the_signal() {
        let config = Config {
            gate_hook: "none".into(),
            ..Config::default()
        };
        let document = plan(&config);
        // Still wired, just not blocking: the approval card still reaches the
        // phone, it simply cannot be answered by holding the hook.
        let command = document["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(!command.contains("--gate"), "{command}");
        assert!(document["hooks"].get("PreToolUse").is_some());
    }

    #[test]
    fn permission_request_is_wired_even_when_the_gate_moves() {
        let config = Config {
            gate_hook: "PreToolUse".into(),
            ..Config::default()
        };
        let document = plan(&config);
        let command = document["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("--event 'PermissionRequest'"), "{command}");
        assert!(
            !command.contains("--gate"),
            "only one event may gate: {command}"
        );
    }

    #[test]
    fn unreachable_ask_reaches_the_generated_command() {
        let config = Config {
            unreachable_ask: true,
            ..Config::default()
        };
        let document = plan(&config);
        let command = document["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("--unreachable-ask"), "{command}");
    }

    #[test]
    fn paths_with_spaces_and_quotes_survive_the_shell() {
        let document = build(
            "cc-1",
            UID,
            &Config::default(),
            Path::new("/Users/some one/it's/cc-hook"),
        );
        let command = document["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            command.starts_with(r#"'/Users/some one/it'\''s/cc-hook'"#),
            "{command}"
        );
    }

    #[test]
    fn session_id_is_passed_explicitly_not_left_to_the_environment() {
        let document = plan(&Config::default());
        let command = document["hooks"]["Notification"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("--session 'cc-1'"), "{command}");
    }

    #[test]
    fn every_wired_hook_carries_the_run_s_identity() {
        // The settings file outlives the process that wrote it: a session
        // started today keeps invoking these commands after the daemon has been
        // restarted and the name reassigned, so the uid has to be baked in
        // rather than looked up at hook time.
        let document = plan(&Config::default());
        for (event, entry) in document["hooks"].as_object().unwrap() {
            let command = entry[0]["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.contains(&format!("--session-uid '{UID}'")),
                "{event} does not carry the session uid: {command}"
            );
        }
    }

    #[test]
    fn generated_document_is_valid_json_for_claude() {
        let encoded = serde_json::to_string(&plan(&Config::default())).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert!(reparsed["hooks"]["Stop"][0]["hooks"][0]["type"] == "command");
    }
}

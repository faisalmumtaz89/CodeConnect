//! The installed Claude Code's slash-command inventory, read from the binary
//! itself.
//!
//! The phone needs to know which `/commands` exist so it can label them
//! honestly and refuse — before anything is typed — the picker-shaped ones
//! that would open a dialog on the Mac's screen and lock the composer. A
//! hand-maintained list would rot with every Claude Code release; the binary
//! already answers the question about itself. Measured on 2.1.221: `claude -p
//! --input-format stream-json --output-format stream-json --verbose --bare`
//! emits a `system`/`init` line carrying `slash_commands` *before* any API
//! call, and `--bare` reads no OAuth and runs no hooks, so the probe observes
//! without side effects and without cost. The turn that would follow the init
//! line fails on auth in bare mode; the child is killed after the first line
//! and never gets there anyway.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// How many leading lines may precede the init message before the probe gives
/// up. Measured: init is the *first* line — the allowance exists so a stray
/// runtime warning cannot break the probe, while a firehose that never says
/// `init` is abandoned after a screenful instead of being read until the
/// deadline.
const MAX_PRELUDE_LINES: usize = 10;

/// The stdin that makes init flush. Measured: with stdin closed and empty the
/// binary exits without emitting anything; with one user message queued it
/// emits init first. The message content never reaches a model — the child
/// dies after line one.
const PROBE_STDIN: &str = concat!(
    r#"{"type":"user","message":{"role":"user","content":"#,
    r#"[{"type":"text","text":"catalog probe"}]}}"#,
    "\n"
);

/// One binary's answer, plus when it actually said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    /// Names exactly as emitted — no leading slash, no additions, no edits.
    pub commands: Vec<String>,
    pub claude_version: Option<String>,
    pub probed_at: String,
}

/// The one line this module trusts, deserialized narrowly: `system`/`init`
/// and the two fields the product uses. Everything else the child prints is
/// ignored, which is what makes this a trust boundary rather than a parser.
#[derive(Debug, Deserialize)]
struct InitLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    subtype: String,
    /// `Option`, not defaulted: an init message that no longer carries the
    /// field is a schema change, and answering it with an empty-but-Available
    /// list would fail open — the phone would wave through every command as
    /// "unknown, probably custom". Absent must become `Unavailable`; a
    /// present-and-empty list (e.g. `--disable-slash-commands`) is a real
    /// answer and stays Available.
    slash_commands: Option<Vec<String>>,
    #[serde(default)]
    claude_code_version: Option<String>,
}

/// Identity of a binary on disk: path, size, and mtime. An upgrade in place
/// changes size or mtime; hashing tens of megabytes on every palette open
/// would be pure cost for the same answer.
pub fn fingerprint(path: &Path) -> Result<String> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("could not stat {}", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
        .unwrap_or_else(|| "unknown-mtime".into());
    Ok(format!("{}:{}:{}", path.display(), meta.len(), mtime))
}

/// The deadline comes from config (`catalog_probe_ms`): the measured probe
/// answers in well under a second on an idle machine, but a Mac mid-compile
/// can stretch even a shell's startup, and a hardcoded budget would turn
/// machine load into a phantom "unavailable".
pub async fn probe_with(claude_bin: &Path, cwd: &Path, timeout: Duration) -> Result<Catalog> {
    let mut child = Command::new(claude_bin)
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--bare",
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("could not run {}", claude_bin.display()))?;

    let outcome = tokio::time::timeout(timeout, async {
        if let Some(mut stdin) = child.stdin.take() {
            // Closed right after the write: the child must never be left
            // believing more input is coming.
            let _ = stdin.write_all(PROBE_STDIN.as_bytes()).await;
        }
        let stdout = child
            .stdout
            .take()
            .context("probe child has no stdout handle")?;
        let mut lines = BufReader::new(stdout).lines();
        for _ in 0..MAX_PRELUDE_LINES {
            match lines.next_line().await? {
                Some(line) => match parse_init_line(&line) {
                    InitRead::Catalog(catalog) => return Ok(catalog),
                    InitRead::MissingInventory => anyhow::bail!(
                        "the init message carried no slash_commands field; \
                         the schema has moved and this probe cannot answer"
                    ),
                    InitRead::NotInit => {}
                },
                None => anyhow::bail!("the binary ended its output without an init message"),
            }
        }
        anyhow::bail!("no init message in the first {MAX_PRELUDE_LINES} lines of output")
    })
    .await;

    // The child is done the moment we have our line — or the moment we gave
    // up. Killed and reaped on every path; `kill_on_drop` is the backstop,
    // not the plan.
    let _ = child.start_kill();
    let _ = child.wait().await;

    match outcome {
        Ok(result) => result,
        Err(_) => anyhow::bail!(
            "the binary said nothing usable within {}ms",
            timeout.as_millis()
        ),
    }
}

/// What one output line contributes: not the init line at all, an init line
/// missing its inventory (a schema change — a hard failure, never an empty
/// success), or the catalog.
enum InitRead {
    NotInit,
    MissingInventory,
    Catalog(Catalog),
}

fn parse_init_line(line: &str) -> InitRead {
    let Ok(parsed) = serde_json::from_str::<InitLine>(line) else {
        return InitRead::NotInit;
    };
    if parsed.kind != "system" || parsed.subtype != "init" {
        return InitRead::NotInit;
    }
    let Some(commands) = parsed.slash_commands else {
        return InitRead::MissingInventory;
    };
    InitRead::Catalog(Catalog {
        commands,
        claude_version: parsed.claude_code_version,
        probed_at: protocol::time::now_rfc3339(),
    })
}

/// A fake Claude Code for tests: a script with a scripted answer. The probe's
/// machinery — spawn, stdin, first-line read, kill, timeout — is exercised
/// for real; only the eighty-megabyte binary is stubbed. Shared with the
/// state tests, which prove the cache in front of this module.
#[cfg(test)]
pub mod test_bin {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// The real init line the installed 2.1.221 emitted. Structure and the
    /// slash-command inventory are verbatim; path- and id-shaped values
    /// (cwd, session/uuid, plugin paths) are sanitized — they carried the
    /// probe machine's own paths, which do not belong in a repository.
    pub const MEASURED_INIT: &str =
        include_str!("../../../fixtures/catalog/claude-2.1.221-init.jsonl");

    pub fn fake_binary(body: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ccd-catalog-{}-{}-{}",
            std::process::id(),
            n,
            protocol::time::now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claude");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A fake that emits the measured init line and then hangs the way the
    /// real binary would attempt its (auth-failing) turn.
    pub fn answering_binary() -> std::path::PathBuf {
        fake_binary(&format!(
            "cat <<'CCEOF'\n{}\nCCEOF\nsleep 30\n",
            MEASURED_INIT.trim_end()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::test_bin::{fake_binary, MEASURED_INIT};
    use super::*;

    #[tokio::test]
    async fn the_measured_init_line_parses_and_the_child_is_killed() {
        // `sleep 30` stands in for the errored turn the real binary would
        // attempt next; the probe must not wait for it.
        let bin = fake_binary(&format!(
            "cat <<'CCEOF'\n{}\nCCEOF\nsleep 30\n",
            MEASURED_INIT.trim_end()
        ));
        let started = std::time::Instant::now();
        let catalog = probe_with(&bin, bin.parent().unwrap(), Duration::from_secs(15))
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the probe must kill the child, not wait out its sleep"
        );
        assert!(
            catalog.commands.iter().any(|c| c == "model"),
            "the measured inventory names /model: {:?}",
            catalog.commands
        );
        assert!(
            catalog.commands.iter().any(|c| c == "clear"),
            "the measured inventory names /clear"
        );
        assert_eq!(catalog.claude_version.as_deref(), Some("2.1.221"));
    }

    #[tokio::test]
    async fn a_prelude_line_is_tolerated_before_init() {
        let bin = fake_binary(&format!(
            "echo 'some runtime warning'\ncat <<'CCEOF'\n{}\nCCEOF\n",
            MEASURED_INIT.trim_end()
        ));
        let catalog = probe_with(&bin, bin.parent().unwrap(), Duration::from_secs(15))
            .await
            .unwrap();
        assert!(catalog.commands.iter().any(|c| c == "model"));
    }

    #[tokio::test]
    async fn a_silent_binary_is_abandoned_at_the_deadline() {
        let bin = fake_binary("sleep 30\n");
        let err = probe_with(&bin, bin.parent().unwrap(), Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("nothing usable"),
            "the deadline names itself: {err}"
        );
    }

    #[tokio::test]
    async fn a_binary_that_exits_without_init_is_a_complete_failure() {
        let bin = fake_binary("exit 0\n");
        let err = probe_with(&bin, bin.parent().unwrap(), Duration::from_secs(15))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("without an init message"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_firehose_that_never_says_init_is_abandoned_by_line_count() {
        let bin = fake_binary("i=0; while [ $i -lt 50 ]; do echo '{\"type\":\"noise\"}'; i=$((i+1)); done\nsleep 30\n");
        let err = probe_with(&bin, bin.parent().unwrap(), Duration::from_secs(15))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("first 10 lines"), "{err:#}");
    }

    #[test]
    fn a_fingerprint_changes_when_the_binary_does() {
        let bin = fake_binary("exit 0\n");
        let before = fingerprint(&bin).unwrap();
        assert_eq!(before, fingerprint(&bin).unwrap(), "stat is deterministic");
        std::fs::write(&bin, "#!/bin/sh\nexit 1\n# longer\n").unwrap();
        assert_ne!(before, fingerprint(&bin).unwrap());
    }

    #[test]
    fn only_a_system_init_line_parses() {
        assert!(matches!(parse_init_line("not json"), InitRead::NotInit));
        assert!(matches!(
            parse_init_line(r#"{"type":"assistant"}"#),
            InitRead::NotInit
        ));
        assert!(matches!(
            parse_init_line(r#"{"type":"system","subtype":"turn_duration"}"#),
            InitRead::NotInit
        ));
        let minimal = r#"{"type":"system","subtype":"init","slash_commands":["model"]}"#;
        let InitRead::Catalog(catalog) = parse_init_line(minimal) else {
            panic!("a complete init line parses");
        };
        assert_eq!(catalog.commands, vec!["model".to_string()]);
        assert_eq!(catalog.claude_version, None);
    }

    /// The fail-open trap: an init line that *dropped* the inventory field is
    /// a schema change and must be a hard failure — an empty Available would
    /// wave every command through as "unknown, probably custom". An empty
    /// list that is genuinely present stays a real answer.
    #[test]
    fn a_missing_inventory_is_a_failure_and_an_empty_one_is_an_answer() {
        assert!(matches!(
            parse_init_line(r#"{"type":"system","subtype":"init"}"#),
            InitRead::MissingInventory
        ));
        let empty = r#"{"type":"system","subtype":"init","slash_commands":[]}"#;
        let InitRead::Catalog(catalog) = parse_init_line(empty) else {
            panic!("present-and-empty is an answer");
        };
        assert!(catalog.commands.is_empty());
    }

    #[tokio::test]
    async fn a_schema_drifted_binary_probes_as_a_failure() {
        let bin = fake_binary(concat!(
            r#"echo '{"type":"system","subtype":"init"}'"#,
            "\nsleep 30\n"
        ));
        let err = probe_with(&bin, bin.parent().unwrap(), Duration::from_secs(15))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no slash_commands"), "{err:#}");
    }
}

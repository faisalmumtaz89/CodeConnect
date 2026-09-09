//! The measurement instrument: a verbatim record of every frame crossing the broker.
//!
//! # Why this exists at all
//!
//! CodeConnect's whole codex posture is "pin to what you guard, and prove the pin
//! against a measured wire". Every such proof — the captured `turn/start` shape, the
//! `/new` switch ordering, the resume-adds-a-subscription finding — rests on a capture.
//! And until now the *capturing* was done by a throwaway harness that was never
//! committed: `fixtures/README.md` cites `captures/subscribed/` and three siblings that
//! do not exist in this repository and never did.
//!
//! That is a real gap rather than an untidiness. The audit sink
//! ([`crate::relay::EventSink`]) cannot stand in for it: [`crate::redact`] exists
//! precisely to guarantee that a decision line carries fixed vocabulary, counts and
//! shape classes and **never client-chosen text**, so `broker.log` is structurally
//! incapable of telling anyone *which* new parameter a new codex sent. Re-grounding
//! against a new codex needs exactly that, so the instrument has to live here.
//!
//! # Not in a shipping build, and that is a COMPILE-TIME fact
//!
//! The tee is behind the `frame-tee` cargo feature, off by default. Without it
//! [`FrameTee::from_env`] does not read `CC_CODEX_FRAME_TEE` — the read is not compiled —
//! and since the enabled state is a private field with [`FrameTee::off`] as its only
//! other constructor, a default build cannot hold an enabled recorder at all. Setting the
//! variable in front of the `codeconnect` a user runs does nothing.
//!
//! That gate replaced an argument, and the argument was too weak. The tee used to be
//! "enabled by an environment variable the shipping launcher never sets", asserted by a
//! test scanning the launcher's sources. But the launcher is not the only parent a
//! `codeconnect` process has: a shell, an IDE, a LaunchAgent or a hostile parent can set
//! any variable it likes, and what this one turns on is a verbatim, unredacted copy of
//! every frame — prompts included — written to a path the setter chose. "Our code does
//! not set it" is not a containment property. The source scan is kept as well, because a
//! capture build should still not have a launcher that switches the recorder on by
//! itself; it is now the second line rather than the only one.
//!
//! A capture build is deliberate:
//!
//! ```sh
//! cargo build -p codeconnect --features frame-tee
//! CC_CODEX_FRAME_TEE=/tmp/capture.jsonl <that binary> codex
//! ```
//!
//! # What it writes
//!
//! One JSON object per line:
//! `{"run":…,"seq":…,"conn":…,"dir":"c2s"|"s2c","raw":…,"frame":…}`.
//!
//! * `raw` is the frame **exactly as it crossed the wire**, byte for byte, as a JSON
//!   string. This is the record. An earlier version stored only the parsed value, which
//!   silently normalised away whitespace, key order, number spelling and duplicate keys —
//!   the very details a capture is taken to settle. (Duplicate keys never reach a real
//!   upstream: [`crate::message::classify_shape`] rejects them outright, so a frame
//!   carrying one appears here and forwards zero bytes.)
//! * `frame` is the parsed view, present only when the raw text parses, so a capture stays
//!   directly comparable to `fixtures/codex/thread-switch.jsonl` and its siblings.
//! * `run` and `seq` say which run a line belongs to and where it sat in the order.
//!   The file is opened in append mode, so without them two runs into one path would be
//!   indistinguishable. A run also writes an `open` and a `close` marker: a capture with
//!   no `close` for its run id was truncated, which is the difference between "the frame
//!   was not sent" and "the recorder stopped".
//!
//! The frame is written verbatim: this is the one place in the broker that deliberately
//! does not redact, because a redacted capture cannot answer the question captures are
//! taken to answer. That is also why it is not production-reachable.
//!
//! A capture therefore contains whatever the session contained, prompts included, and
//! is **not** publishable as-is. Sanitising one for `fixtures/` — scrubbing absolute
//! paths to `/work/…`, replacing personal identifiers with length-preserving
//! placeholders — is a deliberate step, exactly as `fixtures/README.md` describes for
//! every capture already committed.
//!
//! # What it costs the thing it observes
//!
//! The write is synchronous, on the relay task, before classification. That is a
//! deliberate trade and it is stated rather than hidden: a queue with a bound would drop
//! frames under load, and a capture that is silently missing the frame you were looking
//! for is worse than one that is complete and slightly slower. The specific way a
//! synchronous write can block *forever* — a FIFO or a device with no reader — is
//! removed instead: the target must be a regular file, opened without following a
//! symlink at the final component.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// The environment variable that turns the tee on, naming the file to append to.
///
/// Read only in a `frame-tee` build — see the module header.
pub const FRAME_TEE_ENV: &str = "CC_CODEX_FRAME_TEE";

/// A verbatim frame recorder. `None` inside means disabled, which is the production
/// state and the default everywhere else.
#[derive(Debug)]
pub struct FrameTee(Option<Mutex<TeeFile>>);

#[derive(Debug)]
pub struct TeeFile {
    path: PathBuf,
    file: std::fs::File,
    /// This run's id, stamped on every line so two runs appended to one path stay
    /// separable.
    run: u64,
    seq: u64,
    /// Set once a write or flush has failed. A capture that lost a line must not read as
    /// a capture that saw no line.
    failed: bool,
}

impl FrameTee {
    /// Disabled. The only constructor a shipping build can reach.
    pub fn off() -> FrameTee {
        FrameTee(None)
    }

    /// Disabled, always: the shipping build does not compile the env read.
    #[cfg(not(feature = "frame-tee"))]
    pub fn from_env() -> Result<FrameTee, String> {
        Ok(FrameTee::off())
    }

    /// Enabled iff [`FRAME_TEE_ENV`] names a path — `frame-tee` builds only.
    ///
    /// A named path that cannot be opened is a hard error rather than a silent
    /// fallback to off: a harness that asked for a capture and received an empty file
    /// would draw conclusions from frames that were never recorded, which is worse than
    /// no capture at all.
    ///
    /// The target must be a **regular file**, and the final component is opened with
    /// `O_NOFOLLOW`. A FIFO or a character device would let a write block forever on the
    /// relay task, and a symlink would let whoever planted it choose where a verbatim
    /// transcript of the session lands.
    #[cfg(feature = "frame-tee")]
    pub fn from_env() -> Result<FrameTee, String> {
        let Some(path) = std::env::var_os(FRAME_TEE_ENV) else {
            return Ok(FrameTee::off());
        };
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Ok(FrameTee::off());
        }
        Ok(FrameTee(Some(Mutex::new(TeeFile::open(path)?))))
    }

    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// The capture file, when on.
    pub fn path(&self) -> Option<PathBuf> {
        self.0
            .as_ref()
            .and_then(|m| m.lock().ok().map(|f| f.path.clone()))
    }

    /// Append one frame, verbatim.
    ///
    /// `dir` is `"c2s"` or `"s2c"`; `conn` is the relay-minted per-connection instance
    /// id, so a capture says *who got what* — the property that made the `/new` switch
    /// measurable at all.
    ///
    /// Best-effort in the sense that it never fails the session it is observing, but
    /// **not silent**: a failed write marks the run and is reported on stderr, so a
    /// harness reading a short capture can tell a frame that was never sent from a line
    /// that was never written.
    pub fn record(&self, conn: u64, dir: &str, frame: &str) {
        let Some(file) = self.0.as_ref() else {
            return;
        };
        let Ok(mut guard) = file.lock() else {
            return;
        };
        guard.write_frame(conn, dir, frame);
    }
}

impl TeeFile {
    #[cfg(feature = "frame-tee")]
    fn open(path: PathBuf) -> Result<TeeFile, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let not_a_file = |what: &str| {
            format!(
                "{FRAME_TEE_ENV} names {}, which is {what}. The capture target must be a \
                 regular file: a pipe or device would let a write block the relay task \
                 forever, and a symlink would let whoever planted it choose where a \
                 verbatim transcript of the session lands.",
                path.display()
            )
        };
        // Checked BEFORE the open, because opening a FIFO for writing blocks until a
        // reader appears — a check that runs after the open would never run at all.
        // `symlink_metadata` does not follow the final component, so a symlink is caught
        // here rather than silently resolved.
        match std::fs::symlink_metadata(&path) {
            Ok(md) if md.file_type().is_file() => {}
            Ok(md) if md.file_type().is_symlink() => return Err(not_a_file("a symlink")),
            Ok(_) => return Err(not_a_file("not a regular file")),
            // It does not exist yet, which is the ordinary case: the tee creates it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "{FRAME_TEE_ENV}: {} cannot be stat'd: {e}",
                    path.display()
                ))
            }
        }
        // The two checks are what decide; these flags close the window BETWEEN them and
        // this open, which no test can drive because winning that race is the whole
        // premise. A symlink swapped in is refused by the kernel rather than followed,
        // and a FIFO swapped in cannot block the open itself. The `fstat` below is then
        // authoritative about the handle actually held, whatever the path became.
        //
        // `mode(0o600)` applies to CREATION, and it is not a detail: what lands in this
        // file is a deliberately unredacted transcript — prompts, file contents, tool
        // output. Without a mode, creation is `0666 & umask`, commonly `0644`, i.e. a
        // world-readable copy of the session. An EXISTING file keeps its own mode, which
        // is why the permissions are checked below rather than assumed from this line.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| {
                format!(
                    "{FRAME_TEE_ENV} names {} which cannot be opened: {e}",
                    path.display()
                )
            })?;
        let meta = file
            .metadata()
            .map_err(|e| format!("{FRAME_TEE_ENV}: {} cannot be stat'd: {e}", path.display()))?;
        if !meta.file_type().is_file() {
            return Err(not_a_file("not a regular file"));
        }
        // An existing target keeps whatever mode it already had, so it is checked rather
        // than repaired: silently `chmod`-ing somebody else's file is a worse answer than
        // refusing, and a capture is a deliberate act whose target the operator chose.
        let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{FRAME_TEE_ENV} names {}, which is mode {mode:04o}. A capture is an \
                 unredacted transcript of the session — prompts, file contents and tool \
                 output — so its file must not be readable by group or other. Remove it, or \
                 `chmod 600` it, and run again.",
                path.display()
            ));
        }
        let run = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (std::process::id() as u64) << 32;
        Ok(TeeFile::new(path, file, run))
    }

    /// One recording run over an already-open handle, bracketed from the first line.
    ///
    /// Compiled only where a run can actually begin: a shipping build has no constructor
    /// that reaches it, which is the containment stated as a type rather than a comment.
    #[cfg(any(test, feature = "frame-tee"))]
    fn new(path: PathBuf, file: std::fs::File, run: u64) -> TeeFile {
        let mut tee = TeeFile {
            path,
            file,
            run,
            seq: 0,
            failed: false,
        };
        tee.marker("open");
        tee
    }

    /// One `open`/`close` line, so a truncated capture is visible as one.
    ///
    /// The close marker carries the run's OUTCOME as well as its identity: `frames` (the
    /// final sequence number) and `failed`. Without them a run whose LAST frame write
    /// failed would still be bracketed by a clean `open`/`close` pair with no sequence
    /// gap after it, and would read as complete — the one shape "absence is not evidence"
    /// has to be able to rule out.
    fn marker(&mut self, what: &str) {
        let (run, frames, failed) = (self.run, self.seq, self.failed);
        self.emit(serde_json::json!(
            {"run": run, "tee": what, "frames": frames, "failed": failed}
        ));
    }

    fn write_frame(&mut self, conn: u64, dir: &str, frame: &str) {
        self.seq += 1;
        let mut line = serde_json::json!({
            "run": self.run,
            "seq": self.seq,
            "conn": conn,
            "dir": dir,
            // VERBATIM: the bytes as they crossed, before any parse can normalise them.
            "raw": frame,
        });
        // The parsed view alongside, when there is one, so a capture stays directly
        // comparable to the committed fixtures. A frame that does not parse has `raw`
        // and nothing else — and those are exactly the interesting ones.
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(frame) {
            line["frame"] = parsed;
        }
        self.emit(line);
    }

    fn emit(&mut self, line: serde_json::Value) {
        let outcome = writeln!(self.file, "{line}").and_then(|()| self.file.flush());
        if let Err(e) = outcome {
            if !self.failed {
                self.failed = true;
                eprintln!(
                    "codeconnect: frame tee: {} is no longer recording: {e}. Every line after \
                     this point is missing from the capture; do not read its absence as \
                     evidence.",
                    self.path.display()
                );
            }
        }
    }
}

impl Drop for TeeFile {
    /// The `close` marker. A capture whose run id has an `open` and no `close` stopped
    /// early — which is the one thing "the frame is not in the file" must not be allowed
    /// to mean by default.
    fn drop(&mut self) {
        self.marker("close");
    }
}

impl Default for FrameTee {
    fn default() -> Self {
        FrameTee::off()
    }
}

/// Monotonic ids for the test constructor, so two tees in one process do not collide.
#[cfg(test)]
static TEST_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(test)]
impl FrameTee {
    /// An enabled tee over an already-open file, for the tests below. Deliberately not
    /// `pub`: the only way to enable one outside this module is [`FrameTee::from_env`],
    /// which a shipping build does not compile.
    fn enabled(path: PathBuf, file: std::fs::File) -> FrameTee {
        let run = TEST_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        FrameTee(Some(Mutex::new(TeeFile::new(path, file, run))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cc-tee-{name}-{}-{}",
            std::process::id(),
            TEST_RUN.load(std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn open_append(path: &PathBuf) -> std::fs::File {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn off_by_default_and_records_nothing() {
        let tee = FrameTee::off();
        assert!(!tee.is_on());
        assert_eq!(tee.path(), None);
        // Must not panic, must not write anywhere.
        tee.record(1, "c2s", r#"{"method":"turn/start"}"#);
    }

    /// **The shipping build cannot be switched on by its environment.** In a default
    /// build the env read is not compiled at all, so the variable names nothing.
    ///
    /// Deliberately asserted from the other side too — under `frame-tee` the same call
    /// DOES enable — so this test states a difference rather than a tautology.
    #[test]
    fn the_environment_can_only_enable_the_tee_in_a_capture_build() {
        let dir = scratch("env");
        let path = dir.join("cap.jsonl");
        std::env::set_var(FRAME_TEE_ENV, &path);
        let tee = FrameTee::from_env().expect("from_env must not fail on a writable path");
        if cfg!(feature = "frame-tee") {
            assert!(tee.is_on(), "a capture build must honour the variable");
        } else {
            assert!(
                !tee.is_on(),
                "a shipping build must ignore {FRAME_TEE_ENV} entirely"
            );
            assert_eq!(tee.path(), None);
        }
        std::env::remove_var(FRAME_TEE_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The record is the RAW bytes; the parsed view rides alongside it.
    ///
    /// The two cases that separate them are the point: a frame whose key order and
    /// spacing a parse would normalise, and a frame carrying a duplicate key, which
    /// `serde_json` collapses. Both must be recoverable byte for byte from the capture.
    #[test]
    fn a_frame_is_recorded_verbatim_with_its_parsed_view_beside_it() {
        let dir = scratch("verbatim");
        let path = dir.join("cap.jsonl");
        let _ = std::fs::remove_file(&path);
        let tee = FrameTee::enabled(path.clone(), open_append(&path));

        let spaced = "{ \"method\" : \"turn/start\" ,\n  \"params\" : { \"a\" : 1 } }";
        let duped = r#"{"method":"thread/read","params":{"threadId":"a","threadId":"b"}}"#;
        tee.record(3, "c2s", spaced);
        tee.record(3, "s2c", r#"{"result":{"ok":true}}"#);
        tee.record(4, "c2s", "not json at all");
        tee.record(5, "c2s", duped);
        drop(tee);

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let frames: Vec<&serde_json::Value> =
            lines.iter().filter(|l| l.get("tee").is_none()).collect();
        assert_eq!(frames.len(), 4);

        // Verbatim, including the whitespace and key order a parse would have lost.
        assert_eq!(frames[0]["raw"], spaced);
        assert_eq!(frames[0]["conn"], 3);
        assert_eq!(frames[0]["dir"], "c2s");
        assert_eq!(frames[0]["seq"], 1);
        // …and the parsed view is there too, so a capture is still fixture-shaped.
        assert_eq!(frames[0]["frame"]["method"], "turn/start");
        assert_eq!(frames[0]["frame"]["params"]["a"], 1);

        assert_eq!(frames[1]["dir"], "s2c");
        // A malformed frame is kept, not dropped — those are the interesting ones — and
        // it has no parsed view to be mistaken for one.
        assert_eq!(frames[2]["raw"], "not json at all");
        assert!(frames[2].get("frame").is_none());

        // The duplicate key survives in `raw` while the parsed view collapses it. The
        // capture must be able to show that they differed.
        assert_eq!(frames[3]["raw"], duped);
        assert_eq!(frames[3]["frame"]["params"]["threadId"], "b");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run is bracketed, so a capture that stopped early is visible as one rather than
    /// reading as a session that sent nothing more.
    #[test]
    fn a_run_is_bracketed_and_sequenced() {
        let dir = scratch("run");
        let path = dir.join("cap.jsonl");
        let _ = std::fs::remove_file(&path);

        for _ in 0..2 {
            let tee = FrameTee::enabled(path.clone(), open_append(&path));
            tee.record(1, "c2s", "{}");
            drop(tee);
        }

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let runs: std::collections::BTreeSet<u64> =
            lines.iter().filter_map(|l| l["run"].as_u64()).collect();
        assert_eq!(runs.len(), 2, "two appended runs must stay separable");
        for run in runs {
            let mine: Vec<&serde_json::Value> = lines.iter().filter(|l| l["run"] == run).collect();
            assert_eq!(mine.first().unwrap()["tee"], "open");
            assert_eq!(mine.last().unwrap()["tee"], "close");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// VERBATIM, deliberately: the instrument must record what `redact` would strip,
    /// because the question a capture answers is "which parameter did this codex send?"
    /// and redaction exists to make that unanswerable in the audit log.
    #[test]
    fn the_tee_does_not_redact() {
        let dir = scratch("noredact");
        let path = dir.join("cap.jsonl");
        let tee = FrameTee::enabled(path.clone(), open_append(&path));
        tee.record(
            1,
            "c2s",
            r#"{"method":"turn/start","params":{"toolOutput":null,"turnTrigger":"userInput"}}"#,
        );
        drop(tee);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("toolOutput") && text.contains("turnTrigger"),
            "the tee must record parameter NAMES; that is the whole point: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A run that lost its LAST frame must not read as complete.**
    ///
    /// `failed` living only in memory is not enough: if the final frame write fails and
    /// the close write then succeeds, the file has an `open`, a `close`, and no sequence
    /// gap after the last surviving line. Nothing in it says a line is missing. So the
    /// close marker carries the run's outcome — the final sequence and `failed` — which is
    /// the only place that fact can be recorded.
    #[test]
    fn the_close_marker_carries_the_runs_outcome() {
        let dir = scratch("outcome");
        let path = dir.join("cap.jsonl");
        let _ = std::fs::remove_file(&path);
        let tee = FrameTee::enabled(path.clone(), open_append(&path));
        tee.record(1, "c2s", "{}");
        tee.record(1, "c2s", "{}");
        drop(tee);
        let text = std::fs::read_to_string(&path).unwrap();
        let close: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(close["tee"], "close");
        assert_eq!(close["frames"], 2, "the close marker counts the frames");
        assert_eq!(close["failed"], false);

        // **The shape that used to read as complete.** A frame write fails, and the close
        // write then SUCCEEDS — so the file ends with an ordinary `open`…`close` pair and
        // no sequence gap after the last surviving line. Nothing in it would say a line is
        // missing, unless the close marker carries the outcome.
        //
        // Staged by breaking the sink for one frame and repairing it before the run ends,
        // which is what a transient write failure looks like from the file's side.
        let path2 = dir.join("cap2.jsonl");
        let tee = FrameTee::enabled(path2.clone(), open_append(&path2));
        tee.record(1, "c2s", "{}");
        if let Some(m) = tee.0.as_ref() {
            let mut g = m.lock().unwrap();
            let good = std::mem::replace(
                &mut g.file,
                std::fs::File::open(std::env::temp_dir()).unwrap(),
            );
            g.write_frame(1, "c2s", "{}");
            assert!(g.failed, "a failed frame write must mark the run");
            g.file = good;
        }
        drop(tee);

        let text = std::fs::read_to_string(&path2).unwrap();
        let close: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(close["tee"], "close");
        assert_eq!(
            close["failed"], true,
            "the close marker must carry the run's failure, or a capture that lost a \
             frame is indistinguishable from a complete one"
        );
        assert_eq!(
            close["frames"], 2,
            "the sequence counts the frame that was attempted, so the gap is visible too"
        );
        // …and the file really does look complete otherwise: two markers, and only ONE
        // frame line survived.
        let frames = text.lines().filter(|l| !l.contains("\"tee\"")).count();
        assert_eq!(
            frames, 1,
            "one frame line was lost, leaving no gap of its own"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A write that fails marks the run and says so, once. Silence would let a harness
    /// read a short capture as a short session.
    #[test]
    fn a_failed_write_marks_the_run_instead_of_vanishing() {
        let dir = scratch("failed");
        let path = dir.join("cap.jsonl");
        // A read-only handle: every write fails, and none of them may panic or block.
        let file = std::fs::File::open(std::env::temp_dir()).unwrap();
        let tee = FrameTee::enabled(path.clone(), file);
        tee.record(1, "c2s", "{}");
        tee.record(1, "c2s", "{}");
        let failed = tee
            .0
            .as_ref()
            .map(|m| m.lock().unwrap().failed)
            .unwrap_or(false);
        assert!(failed, "a failed write must be recorded on the run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A capture target that is not a regular file is refused, and each arm is refused by
    /// a different part of the check — which is why all three are here.
    ///
    /// * a **symlink** — `O_NOFOLLOW`, plus the pre-open `symlink_metadata`;
    /// * a **FIFO with no reader** — `O_NONBLOCK` turns a blocking open into `ENXIO`;
    /// * a **FIFO with a live reader** — the open SUCCEEDS and nothing about the flags
    ///   objects, so only the file-type check refuses it. This is the arm that matters
    ///   most: a reader that stops reading fills the pipe buffer and the next frame blocks
    ///   the relay task forever.
    #[cfg(feature = "frame-tee")]
    #[test]
    fn a_non_regular_or_symlinked_target_is_refused() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let dir = scratch("target");

        let mkfifo = |path: &PathBuf| {
            let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
        };

        let lonely = dir.join("lonely-pipe");
        mkfifo(&lonely);
        assert!(
            TeeFile::open(lonely).is_err(),
            "a FIFO with no reader must be refused"
        );

        let watched = dir.join("watched-pipe");
        mkfifo(&watched);
        // Held open for the length of the assertion, so the write-side open succeeds and
        // the file-type check is the only thing left to refuse it.
        let _reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&watched)
            .expect("a FIFO opens read-only without blocking");
        assert!(
            TeeFile::open(watched).is_err(),
            "a FIFO with a live reader opens cleanly and must still be refused: a reader \
             that stops reading blocks the relay on the next frame"
        );

        let real = dir.join("real.jsonl");
        std::fs::write(&real, b"").unwrap();
        // 0600, or the permission rule refuses it before the symlink rule is reached and
        // this arm would pass for the wrong reason.
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(TeeFile::open(link).is_err(), "a symlink must be refused");

        // …and an ordinary path, existing or not, is still accepted, or this test would
        // pass against a tee that refused everything.
        assert!(TeeFile::open(dir.join("new.jsonl")).is_ok());
        assert!(TeeFile::open(real).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A capture is created 0600, and a target others can read is refused.**
    ///
    /// What lands in this file is a deliberately unredacted transcript — prompts, file
    /// contents, tool output. `OpenOptions` with no mode creates `0666 & umask`, commonly
    /// `0644`, i.e. a world-readable copy of the session.
    #[cfg(feature = "frame-tee")]
    #[test]
    fn a_capture_is_created_private_and_a_readable_target_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");

        let fresh = dir.join("fresh.jsonl");
        let tee = TeeFile::open(fresh.clone()).expect("a fresh capture opens");
        drop(tee);
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a capture must be created private");

        // An EXISTING file keeps its own mode, so it is checked rather than assumed.
        for bad in [0o644, 0o604, 0o666] {
            let loose = dir.join(format!("loose-{bad:o}.jsonl"));
            std::fs::write(&loose, b"").unwrap();
            std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(bad)).unwrap();
            let why = TeeFile::open(loose).expect_err("a group/other-readable target refuses");
            assert!(why.contains("group or other"), "{why}");
        }
        // …and an existing PRIVATE file is still accepted, so this is not a blanket
        // refusal of everything that already exists.
        let tight = dir.join("tight.jsonl");
        std::fs::write(&tight, b"").unwrap();
        std::fs::set_permissions(&tight, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(TeeFile::open(tight).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

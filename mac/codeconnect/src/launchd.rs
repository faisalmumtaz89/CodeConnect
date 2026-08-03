//! `codeconnect daemon install | uninstall | status | restart` — the LaunchAgent.
//!
//! ## Why launchd at all
//!
//! `ccd` is deliberately not the agents' parent, so losing it costs a reconnect
//! and nothing else. That is exactly what makes automatic restart worth having:
//! there is no downside to being restarted, and every minute the daemon is dead
//! is a minute the phone is blind. launchd is the only thing on macOS that will
//! restart it after a crash, a logout or a reboot without a login item, a shell
//! and a `nohup`.
//!
//! ## The three things that cost real debugging time
//!
//! 1. **launchd hands a process no shell PATH.** Not a short one — none. Every
//!    external binary is therefore resolved to an absolute path *at install
//!    time* and written into the plist's `EnvironmentVariables`, and the daemon
//!    additionally resolves `tmux`, `git` and `tailscale` from candidate lists
//!    in code. Belt and braces, because the two failures look identical from
//!    the outside (a daemon that starts and then quietly cannot do anything).
//! 2. **A plist naming a relative program silently never runs.** The binary path
//!    is resolved and *checked* here, before the job is bootstrapped, so a typo
//!    is an error at the terminal rather than a job that flaps forever.
//! 3. **launchd holds the log files open.** Rotating by rename would leave it
//!    writing to an unlinked inode, so the daemon truncates in place instead
//!    (see `ccd::logrotate`) and the paths are shared through `protocol` so the
//!    plist and the rotator can never point at different files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use protocol::ipc::{ClientFrame, DaemonFrame, DaemonInfo};

/// How long to wait for the daemon to answer its socket after a bootstrap or a
/// takeover. Generous: a cold start opens SQLite and may shell out to
/// `tailscale cert`.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// launchd will not restart a job more often than this. Five seconds is the
/// documented minimum that still stops a crash-looping job from spinning a core;
/// below it launchd applies its own floor anyway.
const THROTTLE_INTERVAL_SECS: u32 = 5;

pub fn command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(),
        Some("status") => status(),
        Some("restart") => restart(),
        Some(other) => {
            usage();
            bail!("unknown daemon command {other:?}")
        }
        None => {
            usage();
            Ok(())
        }
    }
}

fn usage() {
    eprintln!(
        "\
codeconnect daemon — the ccd LaunchAgent

  codeconnect daemon install [--no-takeover]   write the LaunchAgent and start it
  codeconnect daemon status                    plist, launchd job and live daemon
  codeconnect daemon restart                   restart the managed daemon
  codeconnect daemon uninstall                 stop it and remove the LaunchAgent

Installing stops whatever ccd is running — its own job included, because the
plist cannot be replaced under a live one. Sessions survive it: they live in
tmux and their supervisors reconnect. `--no-takeover` refuses instead of
stopping anything; to pick up new binaries without rewriting the plist, use
`codeconnect daemon restart`."
    );
}

// ------------------------------------------------------------------ commands

fn install(args: &[String]) -> Result<()> {
    let takeover = !args.iter().any(|arg| arg == "--no-takeover");
    let ccd = resolve_ccd()?;
    let uid = user_id()?;
    let plist = plist_path();

    // Deal with whatever is already running *before* writing anything, so a
    // refusal leaves the machine exactly as it was.
    //
    // `--no-takeover` applies to *any* running daemon, including one this same
    // LaunchAgent is managing. Installing necessarily stops what is running —
    // the plist cannot be replaced under a live job — so exempting "our own"
    // daemon would make the flag mean something other than what it says on the
    // single most common invocation of the command.
    if let Some(info) = daemon_info() {
        let how = match &info.launchd_label {
            Some(label) if info.is_launchd_managed() => format!("this LaunchAgent, {label}"),
            Some(label) => format!("launchd job {label}"),
            None => "started by hand".to_string(),
        };
        if !takeover {
            bail!(
                "ccd is already running as pid {} ({how}), and installing has to stop \
                 it — the plist cannot be replaced under a live job.\n  \
                 To pick up new binaries without rewriting the plist: `codeconnect daemon restart`.\n  \
                 To go ahead anyway: drop --no-takeover.\n\
                 Sessions survive either way; they live in tmux and reconnect.",
                info.pid
            );
        }
        if info.is_launchd_managed() {
            println!(
                "stopping the running ccd (pid {}) to reinstall over it",
                info.pid
            );
        } else {
            println!("taking over from ccd pid {} ({how})", info.pid);
            stop_daemon(info.pid)?;
        }
    }

    // An old job must be booted out before the file under it changes, or
    // launchd keeps running the previous program.
    //
    // And it must be *gone* before the new one starts. Two daemons briefly
    // sharing one `~/.codeconnect` is the one situation where both could open
    // the event log at once, which is exactly the state the schema migration
    // must never be run from. `bootout` returns before launchd has finished
    // reaping, so the socket is what gets waited on.
    let booted = launchctl(&["bootout".into(), service_target(uid)])?;
    if booted.ok {
        wait_for_daemon_to_stop(Duration::from_secs(10))
            .context("the previous daemon did not release its socket after `launchctl bootout`")?;
    }

    let document = plist_document(&ccd, &path_value());
    write_atomically(&plist, &document).with_context(|| format!("writing {}", plist.display()))?;
    println!("wrote {}", plist.display());

    // `enable` clears a disable recorded by an earlier `bootout -w`-style flow;
    // it is a no-op otherwise and must precede the bootstrap.
    let _ = launchctl(&["enable".into(), service_target(uid)]);
    let bootstrap = launchctl(&[
        "bootstrap".into(),
        format!("gui/{uid}"),
        plist.to_string_lossy().into_owned(),
    ])?;
    if !bootstrap.ok {
        bail!(
            "launchctl bootstrap failed ({}): {}",
            bootstrap.status,
            bootstrap.stderr.trim()
        );
    }

    match wait_for_daemon(READY_TIMEOUT) {
        Some(info) => {
            println!(
                "ccd is running under launchd: pid {}, version {}, protocol {}.{}",
                info.pid, info.version, info.protocol_version, info.protocol_minor
            );
            if !info.is_launchd_managed() {
                println!(
                    "note: the running daemon does not report our label \
                     ({:?}); it may be an older binary",
                    info.launchd_label
                );
            }
            Ok(())
        }
        None => bail!(
            "the LaunchAgent was installed but ccd did not answer {} within {}s; \
             check {}",
            protocol::socket_path().display(),
            READY_TIMEOUT.as_secs(),
            protocol::daemon_stderr_log().display()
        ),
    }
}

fn uninstall() -> Result<()> {
    let uid = user_id()?;
    let plist = plist_path();
    let booted = launchctl(&["bootout".into(), service_target(uid)])?;
    if booted.ok {
        println!("stopped {}", protocol::LAUNCHD_LABEL);
    } else if booted.stderr.contains("No such process")
        || booted.stderr.contains("not find")
        || booted.stderr.contains("no such")
    {
        println!("{} was not loaded", protocol::LAUNCHD_LABEL);
    } else {
        // Stop here rather than delete the plist anyway. An unrecognised
        // failure means the job may well still be loaded, and removing its
        // definition would leave a daemon running that nothing can manage,
        // restart or stop — and no file to point `launchctl` at to find out.
        // Reporting and leaving everything in place is recoverable; that is not.
        bail!(
            "`launchctl bootout` failed ({}): {}\n\
             {} was left in place, because removing it while the job may still be \
             loaded would leave a daemon nothing can manage. Investigate with \
             `launchctl print {}`, then run this again.",
            booted.status,
            booted.stderr.trim(),
            plist.display(),
            service_target(uid),
        );
    }

    match std::fs::remove_file(&plist) {
        Ok(()) => println!("removed {}", plist.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("{} was not installed", plist.display());
        }
        Err(err) => return Err(err).context(format!("removing {}", plist.display())),
    }

    // Honest rather than optimistic: a daemon somebody started by hand is not
    // affected by any of the above, and saying "stopped" would be a lie.
    if let Some(info) = daemon_info() {
        println!(
            "note: a ccd is still running (pid {}, {}); it was not started by \
             this LaunchAgent, so it was left alone",
            info.pid,
            info.launchd_label
                .clone()
                .unwrap_or_else(|| "started by hand".into())
        );
    }
    Ok(())
}

fn restart() -> Result<()> {
    let uid = user_id()?;
    if !plist_path().exists() {
        bail!(
            "{} is not installed; run `codeconnect daemon install` first",
            plist_path().display()
        );
    }
    let result = launchctl(&["kickstart".into(), "-k".into(), service_target(uid)])?;
    if !result.ok {
        bail!(
            "launchctl kickstart failed ({}): {}",
            result.status,
            result.stderr.trim()
        );
    }
    match wait_for_daemon(READY_TIMEOUT) {
        Some(info) => {
            println!("ccd restarted: pid {}, version {}", info.pid, info.version);
            Ok(())
        }
        None => bail!(
            "kickstart returned but ccd did not answer within {}s; check {}",
            READY_TIMEOUT.as_secs(),
            protocol::daemon_stderr_log().display()
        ),
    }
}

fn status() -> Result<()> {
    let plist = plist_path();
    println!("plist    {}", describe_plist(&plist));

    let job = job_state()?;
    println!("launchd  {job}");

    match daemon_info() {
        // `endpoint_port == 0` is the probe's marker for "something owns the
        // socket but could not describe itself". Printing `ws://:0 · 0 sessions`
        // for that would be inventing facts about a daemon we never reached.
        Some(info) if info.endpoint_port == 0 => {
            println!(
                "daemon   pid {} · {} — it does not answer `daemon_info`",
                info.pid, info.version
            );
            println!("managed  unknown; `codeconnect daemon install` will take over from it");
        }
        Some(info) => {
            println!(
                "daemon   pid {} · version {} · protocol {}.{} · up since {}",
                info.pid, info.version, info.protocol_version, info.protocol_minor, info.started_at
            );
            println!(
                "         {}://{}:{} · {} session(s) attached",
                if info.tls { "wss" } else { "ws" },
                info.endpoint_host,
                info.endpoint_port,
                info.sessions
            );
            println!(
                "managed  {}",
                match &info.launchd_label {
                    Some(label) if info.is_launchd_managed() => format!("yes ({label})"),
                    Some(other) => format!("by a different job ({other})"),
                    None =>
                        "no — started by hand; `codeconnect daemon install` takes over".to_string(),
                }
            );
        }
        None => println!(
            "daemon   not answering {}",
            protocol::socket_path().display()
        ),
    }
    println!("logs     {}", protocol::logs_dir().display());
    Ok(())
}

// ------------------------------------------------------------------- helpers

fn describe_plist(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => format!("{} ({} bytes)", path.display(), meta.len()),
        Err(_) => format!("{} (not installed)", path.display()),
    }
}

/// What launchd thinks of the job, from `launchctl list`.
///
/// `launchctl list` rather than `launchctl print`: its output has been the same
/// plist-ish dict for a decade, whereas `print` is a human-readable dump that
/// has changed shape between macOS releases.
fn job_state() -> Result<String> {
    let result = launchctl(&["list".into(), protocol::LAUNCHD_LABEL.into()])?;
    if !result.ok {
        return Ok(format!("{} is not loaded", protocol::LAUNCHD_LABEL));
    }
    let pid = plist_field(&result.stdout, "PID");
    let last_exit = plist_field(&result.stdout, "LastExitStatus");
    Ok(match (pid, last_exit) {
        (Some(pid), _) => format!("loaded, running as pid {pid}"),
        // No PID means launchd is holding it between restarts; the last exit
        // status is the only clue as to why, so it is not swallowed.
        (None, Some(status)) => {
            format!("loaded but not running (last exit status {status})")
        }
        (None, None) => "loaded".to_string(),
    })
}

/// Pull `"Key" = value;` out of `launchctl list` output.
fn plist_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\" = ");
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with(&needle))?;
    let value = line
        .trim()
        .strip_prefix(&needle)?
        .trim_end_matches(';')
        .trim();
    Some(value.trim_matches('"').to_string())
}

pub fn plist_path() -> PathBuf {
    protocol::home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", protocol::LAUNCHD_LABEL))
}

fn service_target(uid: u32) -> String {
    format!("gui/{uid}/{}", protocol::LAUNCHD_LABEL)
}

/// The real user id, asked of the system rather than guessed from `$HOME`.
///
/// `gui/<uid>` is the domain a LaunchAgent lives in; getting it wrong means
/// bootstrapping into somebody else's session, which fails with a permission
/// error that reads like a bug in this program.
fn user_id() -> Result<u32> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .stdin(Stdio::null())
        .output()
        .context("running /usr/bin/id -u")?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("parsing the output of /usr/bin/id -u")
}

struct Run {
    ok: bool,
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn launchctl(args: &[String]) -> Result<Run> {
    let output = Command::new("/bin/launchctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running /bin/launchctl")?;
    Ok(Run {
        ok: output.status.success(),
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The installed `ccd`, as an absolute path that exists.
///
/// The installed prefix wins over the one next to this binary: `codeconnect daemon
/// install` run out of a cargo target directory should still point launchd at
/// the *installed* daemon, or the job would break the next time the target
/// directory is cleaned.
fn resolve_ccd() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(explicit) = std::env::var_os("CODECONNECT_CCD_BIN") {
        candidates.push(PathBuf::from(explicit));
    }
    candidates.push(protocol::root_dir().join("bin/ccd"));
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            candidates.push(dir.join("ccd"));
        }
    }
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let absolute = candidate.canonicalize().unwrap_or(candidate);
        if !is_executable(&absolute) {
            continue;
        }
        return Ok(absolute);
    }
    bail!(
        "could not find an executable ccd; run ./install.sh first (looked in {} \
         and next to this binary)",
        protocol::root_dir().join("bin").display()
    )
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The `PATH` launchd will hand the daemon.
///
/// Built from where the tools actually are on *this* machine rather than from a
/// fixed list, because that is the whole point of resolving at install time: a
/// Homebrew prefix, a volume-mounted toolchain or a `~/.local/bin` install are
/// all invisible to launchd otherwise.
fn path_value() -> String {
    let mut dirs: Vec<PathBuf> = vec![protocol::root_dir().join("bin")];
    for tool in ["tmux", "git", "tailscale", "claude", "sh"] {
        if let Some(found) = locate(tool) {
            if let Some(dir) = found.parent() {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    for fallback in [
        "/usr/local/bin",
        "/opt/homebrew/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        dirs.push(PathBuf::from(fallback));
    }

    let mut seen = BTreeSet::new();
    dirs.into_iter()
        .filter(|dir| seen.insert(dir.clone()))
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

/// Where a tool is, checking the usual install prefixes before `PATH`.
///
/// `PATH` is consulted last on purpose: this runs in the operator's shell, which
/// may have a `PATH` entry that launchd will never see, so a hit from a known
/// absolute location is the more durable answer.
fn locate(tool: &str) -> Option<PathBuf> {
    let home = protocol::home_dir();
    let candidates = [
        home.join(".local/bin").join(tool),
        home.join(".claude/local").join(tool),
        PathBuf::from("/opt/homebrew/bin").join(tool),
        PathBuf::from("/usr/local/bin").join(tool),
        PathBuf::from("/usr/bin").join(tool),
        PathBuf::from("/bin").join(tool),
        PathBuf::from("/Applications/Tailscale.app/Contents/MacOS").join(tool),
    ];
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .or_else(|| crate::tmux::search_path(tool))
}

/// Ask the running daemon who it is. `None` means nothing is answering.
fn daemon_info() -> Option<DaemonInfo> {
    match crate::daemon::request(&ClientFrame::DaemonInfo) {
        Ok(DaemonFrame::Daemon(info)) => Some(info),
        // A daemon too old to know the frame answers with an error rather than
        // silence, and that is still proof that something owns the socket.
        Ok(_) | Err(_) => legacy_daemon_probe(),
    }
}

/// Is *something* holding the socket, even if it cannot describe itself?
///
/// A daemon predating `daemon_info` does not answer the frame, and treating that
/// as "no daemon" would bootstrap a second one straight into a socket conflict —
/// which is precisely the upgrade path every existing installation takes exactly
/// once. The pid comes from `lsof`, which answers the same question the frame
/// would have ("who owns this socket?") from the outside.
fn legacy_daemon_probe() -> Option<DaemonInfo> {
    std::os::unix::net::UnixStream::connect(protocol::socket_path()).ok()?;
    Some(DaemonInfo {
        pid: socket_owner().unwrap_or(0),
        version: "unknown (predates `codeconnect daemon`)".into(),
        protocol_version: 0,
        protocol_minor: 0,
        started_at: String::new(),
        launchd_label: None,
        endpoint_host: String::new(),
        bind_ip: None,
        endpoint_port: 0,
        tls: false,
        sessions: 0,
    })
}

/// The pid holding the daemon socket open, per `lsof`.
///
/// Ambiguity is `None`, not a guess: this pid is about to be sent a signal, and
/// signalling the wrong process is not a recoverable mistake. A caller that gets
/// `None` tells the operator to stop the daemon by hand instead.
fn socket_owner() -> Option<u32> {
    let output = Command::new("/usr/sbin/lsof")
        .arg("-t")
        .arg(protocol::socket_path())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let pids: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();
    match pids.as_slice() {
        [pid] => Some(*pid),
        _ => None,
    }
}

fn wait_for_daemon(timeout: Duration) -> Option<DaemonInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(info) = daemon_info() {
            return Some(info);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Wait until nothing is answering the daemon socket.
///
/// The inverse of [`wait_for_daemon`], and the more important half: starting a
/// second daemon while the first is still alive would put two processes on one
/// SQLite file, and the migration is not a thing to run twice concurrently.
fn wait_for_daemon_to_stop(timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if daemon_info().is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "something is still answering {} after {}s",
                protocol::socket_path().display(),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ask a hand-started daemon to exit, and wait until its socket is free.
///
/// SIGTERM, not SIGKILL: `ccd` handles it, removes its socket and exits
/// cleanly, and nothing it owns needs a forced kill — the sessions are in tmux
/// and the log is in SQLite. If it will not go, that is reported rather than
/// escalated, because a daemon that ignores SIGTERM is a bug worth seeing.
fn stop_daemon(pid: u32) -> Result<()> {
    if pid == 0 {
        bail!(
            "something is holding {} but is too old to say which process it is; \
             stop it by hand (`pkill -f ccd`) and run this again",
            protocol::socket_path().display()
        );
    }
    let killed = Command::new("/bin/kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .status()
        .context("running /bin/kill")?;
    if !killed.success() {
        bail!("could not signal ccd pid {pid} ({killed})");
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if daemon_info().is_none() {
            println!("previous ccd stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("ccd pid {pid} did not exit after SIGTERM; stop it by hand and retry")
}

fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.tmp", file_name(path)));
    std::fs::write(&temp, contents)?;
    // Rename rather than truncate-and-write: launchd may be reading the file at
    // this instant, and half a plist parses as no plist.
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plist".to_string())
}

/// Generate the LaunchAgent.
///
/// Generated rather than shipped as a file with a `__HOME__` placeholder: the
/// binary path and `PATH` are resolved on the machine being installed onto, and
/// a template can only ever encode one guess at where things are.
pub(crate) fn plist_document(ccd: &Path, path_value: &str) -> String {
    let home = protocol::home_dir();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!--
  Generated by `codeconnect daemon install`. Edit that command, not this file: a
  reinstall overwrites it, and the paths below were resolved on this machine.

  * LaunchAgent, not LaunchDaemon: ccd needs the user's login session for the
    keychain (APNs .p8) and for the tailnet identity.
  * KeepAlive/SuccessfulExit=false restarts ccd after a crash but respects a
    clean exit, so `launchctl bootout` does not fight a restart loop.
  * ccd is deliberately NOT the parent of any agent. Sessions live in the
    `tmux -L codeconnect` server; restarting this job costs a reconnect.
  * The log files are truncated in place by ccd itself once they pass its cap.
    launchd holds them open, so rotating by rename would leave it appending to
    an unlinked inode.
-->
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>

    <key>ProgramArguments</key>
    <array>
        <string>{ccd}</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>

    <key>ThrottleInterval</key>
    <integer>{throttle}</integer>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
        <key>HOME</key>
        <string>{home}</string>
        <key>CODECONNECT_LOG</key>
        <string>info</string>
    </dict>

    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>

    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        label = xml_escape(protocol::LAUNCHD_LABEL),
        ccd = xml_escape(&ccd.to_string_lossy()),
        throttle = THROTTLE_INTERVAL_SECS,
        path = xml_escape(path_value),
        home = xml_escape(&home.to_string_lossy()),
        stdout = xml_escape(&protocol::daemon_stdout_log().to_string_lossy()),
        stderr = xml_escape(&protocol::daemon_stderr_log().to_string_lossy()),
    )
}

/// Escape the five XML entities.
///
/// Not paranoia: `$HOME` is user-controlled and a directory named `A&B` is
/// perfectly legal on macOS. An unescaped `&` makes the whole plist unparseable,
/// and launchd's diagnostic for that is a job that simply never starts.
fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_plist_is_valid_and_names_absolute_paths() {
        let document = plist_document(
            Path::new("/Users/someone/.codeconnect/bin/ccd"),
            "/opt/homebrew/bin:/usr/bin",
        );
        assert!(document.contains("<string>com.codeconnect.ccd</string>"));
        assert!(document.contains("/Users/someone/.codeconnect/bin/ccd"));
        assert!(document.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(document.contains("<key>SuccessfulExit</key>"));
        assert!(document.contains("<key>ThrottleInterval</key>"));
        assert!(document.contains("<integer>5</integer>"));
        // Every path in ProgramArguments and PATH must be absolute: launchd has
        // no working directory the operator would recognise and no shell PATH.
        for line in document.lines() {
            if let Some(value) = line.trim().strip_prefix("<string>") {
                let value = value.trim_end_matches("</string>");
                if value.contains('/') && !value.starts_with("http") {
                    assert!(
                        value.starts_with('/') || value.starts_with("gui/"),
                        "relative path in the plist: {value}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_plist_parses_as_a_property_list() {
        // The failure this catches is silent: launchd rejects a malformed plist
        // by never starting the job, with nothing in any log the operator reads.
        let document = plist_document(Path::new("/tmp/ccd"), "/usr/bin");
        let path = std::env::temp_dir().join(format!(
            "cc-plist-test-{}-{}.plist",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::write(&path, &document).unwrap();
        let output = Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .expect("plutil must exist on macOS");
        let _ = std::fs::remove_file(&path);
        assert!(
            output.status.success(),
            "plutil rejected the plist: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_home_with_xml_metacharacters_still_produces_a_valid_plist() {
        let document = plist_document(Path::new("/Users/a&b/<c>/ccd"), "/usr/bin");
        assert!(
            document.contains("/Users/a&amp;b/&lt;c&gt;/ccd"),
            "{document}"
        );
        assert!(!document.contains("/Users/a&b/"), "raw ampersand survived");
    }

    #[test]
    fn xml_escape_covers_every_entity() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(xml_escape("plain"), "plain");
        assert_eq!(xml_escape(""), "");
    }

    #[test]
    fn launchctl_list_output_is_parsed_for_pid_and_exit_status() {
        let sample = "{\n\
            \t\"LimitLoadToSessionType\" = \"Aqua\";\n\
            \t\"Label\" = \"com.codeconnect.ccd\";\n\
            \t\"OnDemand\" = false;\n\
            \t\"LastExitStatus\" = 0;\n\
            \t\"PID\" = 15714;\n\
            };\n";
        assert_eq!(plist_field(sample, "PID").as_deref(), Some("15714"));
        assert_eq!(plist_field(sample, "LastExitStatus").as_deref(), Some("0"));
        assert_eq!(
            plist_field(sample, "Label").as_deref(),
            Some("com.codeconnect.ccd")
        );
        assert_eq!(plist_field(sample, "Nonexistent"), None);

        // The between-restarts shape: launchd reports the job with no PID.
        let stopped =
            "{\n\t\"Label\" = \"com.codeconnect.ccd\";\n\t\"LastExitStatus\" = 256;\n};\n";
        assert_eq!(plist_field(stopped, "PID"), None);
        assert_eq!(
            plist_field(stopped, "LastExitStatus").as_deref(),
            Some("256")
        );
    }

    #[test]
    fn the_path_is_absolute_deduplicated_and_covers_the_tools_we_shell_out_to() {
        let path = path_value();
        assert!(!path.is_empty());
        let dirs: Vec<&str> = path.split(':').collect();
        for dir in &dirs {
            assert!(dir.starts_with('/'), "relative PATH entry: {dir}");
        }
        let mut unique = dirs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), dirs.len(), "PATH repeats a directory: {path}");
        assert!(dirs.contains(&"/usr/bin"), "{path}");
        assert!(
            path.contains(
                &protocol::root_dir()
                    .join("bin")
                    .to_string_lossy()
                    .to_string()
            ),
            "the installed prefix must be on the daemon's PATH: {path}"
        );
    }

    #[test]
    fn the_plist_and_the_rotator_agree_on_the_log_paths() {
        // Two files whose only relationship is that they must name the same
        // paths. If they diverge, the log grows without bound and nothing says
        // so, which is why this is asserted rather than commented.
        let document = plist_document(Path::new("/tmp/ccd"), "/usr/bin");
        assert!(document.contains(&protocol::daemon_stdout_log().to_string_lossy().to_string()));
        assert!(document.contains(&protocol::daemon_stderr_log().to_string_lossy().to_string()));
    }

    #[test]
    fn the_service_target_names_the_gui_domain() {
        assert_eq!(service_target(501), "gui/501/com.codeconnect.ccd");
    }

    #[test]
    fn the_plist_lands_in_the_user_launch_agents_directory() {
        let path = plist_path();
        assert!(
            path.ends_with("Library/LaunchAgents/com.codeconnect.ccd.plist"),
            "{path:?}"
        );
        assert!(path.is_absolute());
    }
}

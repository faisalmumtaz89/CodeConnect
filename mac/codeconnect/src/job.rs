//! Job control for the agent in a pane, so Ctrl+Z stops it as it does in a shell.
//!
//! A pane's own process leads a session whose parent, the tmux server, is in another
//! one, so its process group is orphaned and the kernel discards the SIGTSTP an agent
//! sends itself. The agent therefore runs as a job of the pane's process: in its own
//! group, in the terminal's foreground. When it stops, the pane's process takes the
//! terminal back, marks the pane stopped (`@codeconnect-stopped`) and writes
//! [`STOP_MARKER`] after everything the agent printed, which tells each viewer to
//! suspend itself in the user's shell. A viewer resumed with `fg` sends SIGUSR1, and the
//! agent gets the terminal back and SIGCONT. When the last viewer goes away while the
//! agent is stopped, or the session does, it gets SIGHUP and SIGCONT, as a shell's
//! stopped job does when its terminal closes. A stop with no viewer at the Mac attached
//! is dropped: the phone's terminal cannot `fg`.
//!
//! Claude's pane runs [`run`] (`codeconnect internal-job`); the Codex host is its TUI's
//! parent already and calls [`watch`] itself.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};

pub const SUBCOMMAND: &str = "internal-job";

/// Written to the pane after the agent stopped; the viewer drops it.
pub const STOP_MARKER: &[u8] = b"\x1bP=codeconnect-stopped\x1b\\";

const STOPPED_OPTION: &str = "@codeconnect-stopped";

/// The row a resuming viewer's cursor is on, where the agent resumes.
pub const CURSOR_OPTION: &str = "@codeconnect-cursor";

/// How often a stopped agent's pane asks whether a viewer is still attached.
const VIEWER_POLL: Duration = Duration::from_secs(1);

static AGENT: AtomicI32 = AtomicI32::new(0);
static RESUME: [AtomicI32; 2] = [AtomicI32::new(-1), AtomicI32::new(-1)];

extern "C" fn resume_requested(_: libc::c_int) {
    let fd = RESUME[1].load(Ordering::SeqCst);
    if fd >= 0 {
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }
}

extern "C" fn forward(signal: libc::c_int) {
    let agent = AGENT.load(Ordering::SeqCst);
    if agent > 0 {
        unsafe {
            libc::killpg(agent, signal);
            libc::killpg(agent, libc::SIGCONT);
        }
    }
}

/// Ignore SIGTTOU, so this process can hand the terminal back and forth, and hear a
/// viewer's SIGUSR1. Before the agent is spawned.
pub fn prepare() -> Result<()> {
    let mut pipe = [0; 2];
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("creating the resume pipe");
    }
    for fd in pipe {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    unsafe { libc::fcntl(pipe[1], libc::F_SETFL, libc::O_NONBLOCK) };
    RESUME[0].store(pipe[0], Ordering::SeqCst);
    RESUME[1].store(pipe[1], Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(
            libc::SIGUSR1,
            resume_requested as *const () as libc::sighandler_t,
        );
    }
    Ok(())
}

/// Spawn `command` as the terminal's foreground job, with the terminal signals a
/// shell gives its jobs. A tmux pane starts with SIGTTIN and SIGTTOU ignored, and a
/// job that ignores SIGTTIN has a read it restarts after being continued fail with EIO.
pub fn in_foreground(command: &mut Command) {
    command.process_group(0);
    unsafe {
        command.pre_exec(|| {
            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid());
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            libc::signal(libc::SIGTTIN, libc::SIG_DFL);
            Ok(())
        });
    }
}

/// `codeconnect internal-job PROGRAM [ARG...]`: run `PROGRAM` as this pane's job and
/// end as it ends.
pub fn run(args: &[OsString]) -> Result<()> {
    let [program, rest @ ..] = args else {
        bail!("usage: codeconnect {SUBCOMMAND} PROGRAM [ARG...]");
    };
    prepare()?;
    let mut command = Command::new(program);
    command.args(rest);
    in_foreground(&mut command);
    let agent = command
        .spawn()
        .with_context(|| format!("starting {}", program.to_string_lossy()))?
        .id() as libc::pid_t;
    AGENT.store(agent, Ordering::SeqCst);
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::signal(signal, forward as *const () as libc::sighandler_t) };
    }
    unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, agent) };
    loop {
        let mut status = 0;
        if unsafe { libc::waitpid(agent, &mut status, libc::WUNTRACED) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("waiting for the agent");
        }
        if libc::WIFSTOPPED(status) {
            hold_while_stopped(agent, &|_| {});
        } else if libc::WIFSIGNALED(status) {
            let signal = libc::WTERMSIG(status);
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
            std::process::exit(128 + signal);
        } else {
            std::process::exit(libc::WEXITSTATUS(status));
        }
    }
}

/// Hold each stop of `agent`, a child of this process leading its own group, until
/// it has exited. `pause` is told when the agent is held stopped and when it is let go,
/// so what runs beside it can stop and continue with it.
pub fn watch(agent: libc::pid_t, pause: impl Fn(bool) + Send + 'static) {
    AGENT.store(agent, Ordering::SeqCst);
    std::thread::spawn(move || loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe { libc::waitid(libc::P_PID, agent as libc::id_t, &mut info, libc::WSTOPPED) } != 0
        {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        hold_while_stopped(agent, &pause);
    });
}

/// How a stop ends.
#[derive(Debug, PartialEq)]
enum Release {
    /// No viewer at the Mac was attached to hold it: nobody can `fg`, so the stop is
    /// dropped, as the kernel drops one for a job nobody can resume.
    Unheld,
    /// A viewer came back with `fg`.
    Resumed,
    /// Every viewer at the Mac went away: a hangup, as a shell gives its stopped jobs
    /// when its terminal goes.
    Hangup,
    /// The pane's session or server is gone: a hangup, and nothing more is asked of a
    /// tmux that may now be another one.
    Gone,
    /// The agent ended while stopped.
    Exited,
}

/// Take the terminal from the stopped `agent`, tell the viewers, and give it back once
/// a viewer resumes it, with SIGHUP first if the viewers or the session went away.
fn hold_while_stopped(agent: libc::pid_t, pause: &dyn Fn(bool)) {
    let release = match viewers() {
        Some(0) => Release::Unheld,
        Some(_) => hold(agent, pause),
        None => Release::Gone,
    };
    unsafe {
        match release {
            Release::Exited => {}
            Release::Hangup | Release::Gone => {
                libc::killpg(agent, libc::SIGHUP);
                libc::killpg(agent, libc::SIGCONT);
            }
            Release::Unheld | Release::Resumed => {
                libc::killpg(agent, libc::SIGCONT);
            }
        }
    }
}

fn hold(agent: libc::pid_t, pause: &dyn Fn(bool)) -> Release {
    let mut modes: libc::termios = unsafe { std::mem::zeroed() };
    unsafe {
        libc::tcgetattr(libc::STDIN_FILENO, &mut modes);
        libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
        let mut quiet = modes;
        quiet.c_lflag &= !(libc::ISIG | libc::ICANON | libc::ECHO);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet);
    }
    let resume = RESUME[0].load(Ordering::SeqCst);
    drain(resume);
    pause(true);
    set_stopped(true);
    write_pane(STOP_MARKER);
    let release = loop {
        let mut poll = libc::pollfd {
            fd: resume,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, VIEWER_POLL.as_millis() as i32) } > 0 {
            drain(resume);
            break Release::Resumed;
        }
        if exited(agent) {
            break Release::Exited;
        }
        match viewers() {
            Some(0) => break Release::Hangup,
            None => break Release::Gone,
            Some(_) => {}
        }
    };
    if release != Release::Gone {
        set_stopped(false);
    }
    if release == Release::Resumed {
        // The viewer's cursor, so the agent resumes where the user's shell left it.
        let row = tmux(&[
            "display-message",
            "-p",
            "-t",
            &pane(),
            &format!("#{{{CURSOR_OPTION}}}"),
        ]);
        if let Some(row) =
            row.filter(|row| !row.is_empty() && row.bytes().all(|b| b.is_ascii_digit()))
        {
            write_pane(format!("\x1b[{row};1H").as_bytes());
        }
        tmux(&["set-option", "-p", "-t", &pane(), "-u", CURSOR_OPTION]);
    }
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &modes);
        libc::tcsetpgrp(libc::STDIN_FILENO, agent);
    }
    pause(false);
    release
}

/// Whether the stopped `agent` has ended, without collecting its status: its waiter
/// still reaps it.
fn exited(agent: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let waited = unsafe {
        libc::waitid(
            libc::P_PID,
            agent as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    waited == 0 && unsafe { info.si_pid() } == agent
}

/// How many viewers at the Mac the pane's session has, if its tmux server answers:
/// `None` once the session or the server is gone. The phone's terminal is attached as
/// a client too, flagged `ignore-size`; it is not a viewer, since nobody can `fg` from it.
fn viewers() -> Option<usize> {
    let answer = tmux(&["list-clients", "-t", &pane(), "-F", "#{client_flags}"])?;
    Some(
        answer
            .lines()
            .filter(|flags| !flags.split(',').any(|flag| flag == "ignore-size"))
            .count(),
    )
}

fn write_pane(bytes: &[u8]) {
    unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
}

fn drain(fd: libc::c_int) {
    let mut byte = [0u8; 64];
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    while unsafe { libc::poll(&mut poll, 1, 0) } > 0 {
        unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
    }
}

fn set_stopped(stopped: bool) {
    let pane = pane();
    let mut args = vec!["set-option", "-p", "-t", pane.as_str()];
    args.extend(if stopped {
        [STOPPED_OPTION, "1"].as_slice()
    } else {
        ["-u", STOPPED_OPTION].as_slice()
    });
    tmux(&args);
}

fn pane() -> String {
    std::env::var("TMUX_PANE").unwrap_or_default()
}

fn tmux(args: &[&str]) -> Option<String> {
    let socket = std::env::var("TMUX").ok()?;
    let socket = socket.split(',').next()?;
    let output = Command::new(crate::tmux::tmux_bin().ok()?)
        .arg("-S")
        .arg(socket)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

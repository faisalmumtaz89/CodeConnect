//! The caller's environment, carried into a tmux pane.
//!
//! A pane's processes start from the tmux server's global environment — that of
//! whichever shell first started the server, possibly days ago — plus the `-e`
//! pairs `new-session` names. An agent run directly gets the caller's environment
//! instead. So the launcher writes the caller's environment to a private file and
//! prefixes the pane command with `codeconnect internal-caller-environment FILE`,
//! which reads and deletes the file, replaces its own environment with the file's
//! and execs the rest of the pane command.
//!
//! The values travel in the file, never in argv: argv is readable by every process
//! on the machine, and tmux refuses a command longer than about 16 KB.
//!
//! The file is NUL-separated `NAME=VALUE` records, so any value an environment can
//! hold (empty, `=`, newlines, non-UTF-8) survives unchanged.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub const SUBCOMMAND: &str = "internal-caller-environment";

/// Set by tmux for the pane itself, and describing the pane rather than the
/// caller's terminal: the pane's values are kept and the caller's are not carried.
const SET_BY_TMUX: [&str; 3] = ["TERM", "TMUX", "TMUX_PANE"];

/// The file [`write`] hands the pane, in the private directory `dir`.
pub fn file(dir: &Path) -> PathBuf {
    dir.join("environment")
}

/// Write `caller` to a `0600` file in the private directory `dir`, with `own` —
/// the variables CodeConnect sets for the pane — at CodeConnect's values.
pub fn write(
    dir: &Path,
    caller: impl IntoIterator<Item = (OsString, OsString)>,
    own: &[(String, String)],
) -> Result<PathBuf> {
    protocol::fsperm::private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = file(dir);
    protocol::fsperm::create_private(&path)
        .and_then(|mut file| file.write_all(&encode(caller, own)))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The pane-command prefix that hands `file` to the pane: `exe` is this binary.
pub fn pane_prefix(exe: &Path, file: &Path) -> [OsString; 3] {
    [
        exe.as_os_str().to_owned(),
        SUBCOMMAND.into(),
        file.as_os_str().to_owned(),
    ]
}

/// `codeconnect internal-caller-environment FILE PROGRAM [ARG...]`: exec
/// `PROGRAM` with the environment in `FILE` plus the pane's own [`SET_BY_TMUX`]
/// variables, and nothing else.
pub fn run(args: &[OsString]) -> Result<()> {
    let [file, program, rest @ ..] = args else {
        bail!("usage: codeconnect {SUBCOMMAND} FILE PROGRAM [ARG...]");
    };
    let vars = load(Path::new(file))?;
    let mut command = std::process::Command::new(program);
    command.args(rest).env_clear();
    for name in SET_BY_TMUX {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.envs(vars);
    Err(command.exec()).with_context(|| format!("starting {}", Path::new(program).display()))
}

/// Read and delete the file. Deleted before anything else can fail, so the
/// caller's values never outlive the pane's start.
fn load(path: &Path) -> Result<Vec<(OsString, OsString)>> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "reading the caller's environment from {}; the session was not started",
            path.display()
        )
    })?;
    std::fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))?;
    decode(&bytes).with_context(|| format!("reading {}", path.display()))
}

fn encode(
    caller: impl IntoIterator<Item = (OsString, OsString)>,
    own: &[(String, String)],
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut put = |name: &OsStr, value: &OsStr| {
        out.extend_from_slice(name.as_bytes());
        out.push(b'=');
        out.extend_from_slice(value.as_bytes());
        out.push(0);
    };
    for (name, value) in caller {
        let carried = !SET_BY_TMUX.iter().any(|tmux| name == *tmux)
            && !own.iter().any(|(ours, _)| name == ours.as_str());
        if carried {
            put(&name, &value);
        }
    }
    for (name, value) in own {
        put(name.as_ref(), value.as_ref());
    }
    out
}

fn decode(bytes: &[u8]) -> Result<Vec<(OsString, OsString)>> {
    let Some(body) = bytes.strip_suffix(&[0]) else {
        bail!("the last record is not terminated");
    };
    body.split(|&byte| byte == 0)
        .map(
            |record| match record.iter().position(|&byte| byte == b'=') {
                Some(at) => Ok((
                    OsString::from_vec(record[..at].to_vec()),
                    OsString::from_vec(record[at + 1..].to_vec()),
                )),
                None => bail!("a record has no '='"),
            },
        )
        .collect()
}

/// The `codeconnect` binary cargo builds beside this test binary, so a test pane
/// can run the real subcommand.
#[cfg(test)]
pub fn built_binary() -> PathBuf {
    let bin = std::env::current_exe()
        .expect("the test binary has a path")
        .parent()
        .and_then(Path::parent)
        .expect("the test binary sits in target/<profile>/deps")
        .join("codeconnect");
    assert!(
        bin.is_file(),
        "{} is built by `cargo test -p codeconnect`; a `--bin`-only run needs \
         `cargo build -p codeconnect` first",
        bin.display()
    );
    bin
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn pair(name: &str, value: &[u8]) -> (OsString, OsString) {
        (name.into(), OsString::from_vec(value.to_vec()))
    }

    fn scratch() -> PathBuf {
        std::env::temp_dir()
            .join(format!("cc-caller-env-{}", protocol::uid::new().unwrap()))
            .join("session")
    }

    #[test]
    fn every_value_survives_the_file() {
        let caller = vec![
            pair("EMPTY", b""),
            pair("EQUALS", b"a=b==c"),
            pair("NEWLINE", b"one\ntwo\n"),
            pair("SPACES", b"  a b  "),
            pair("UNICODE", "ünï ☃ 日本".as_bytes()),
            pair("RAW", b"\xff\xfe-'$(x)"),
        ];
        assert_eq!(decode(&encode(caller.clone(), &[])).unwrap(), caller);
    }

    #[test]
    fn tmux_names_stay_behind_and_codeconnect_names_win() {
        let caller = vec![
            pair("TERM", b"xterm-ghostty"),
            pair("TMUX", b"/tmp/other,1,0"),
            pair("TMUX_PANE", b"%3"),
            pair("CODECONNECT_SESSION_UID", b"the caller's"),
            pair("KEPT", b"yes"),
        ];
        let own = [("CODECONNECT_SESSION_UID".to_string(), "ours".to_string())];
        assert_eq!(
            decode(&encode(caller, &own)).unwrap(),
            vec![
                pair("KEPT", b"yes"),
                pair("CODECONNECT_SESSION_UID", b"ours")
            ]
        );
    }

    #[test]
    fn the_file_is_private_and_gone_once_read() {
        let dir = scratch();
        let file = write(&dir, vec![pair("A", b"1")], &[]).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&dir), mode(&file)), (0o700, 0o600));
        assert_eq!(load(&file).unwrap(), vec![pair("A", b"1")]);
        assert!(!file.exists(), "the values do not outlive the pane's start");
        let missing = load(&file).unwrap_err();
        assert!(
            format!("{missing:#}").contains("the session was not started"),
            "{missing:#}"
        );
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_subcommand_replaces_the_environment_and_execs() {
        let dir = scratch();
        let file = write(
            &dir,
            vec![
                pair("FRESH", b"new"),
                pair("TERM", b"caller"),
                pair("RAW", b"\xff"),
            ],
            &[],
        )
        .unwrap();
        let output = std::process::Command::new(built_binary())
            .args([OsStr::new(SUBCOMMAND), file.as_os_str()])
            .args(["/usr/bin/env", "-0"])
            .env_clear()
            .envs([("STALE", "old"), ("TERM", "tmux-256color"), ("TMUX", "t")])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let mut seen: Vec<&[u8]> = output.stdout.split(|&b| b == 0).collect();
        seen.sort();
        assert_eq!(
            seen,
            [
                &b""[..],
                b"FRESH=new",
                b"RAW=\xff",
                b"TERM=tmux-256color",
                b"TMUX=t"
            ]
        );
        assert!(!file.exists());

        let failed = std::process::Command::new(built_binary())
            .args([OsStr::new(SUBCOMMAND), file.as_os_str()])
            .args(["/usr/bin/true"])
            .output()
            .unwrap();
        assert!(!failed.status.success(), "a missing file never launches");
        assert!(String::from_utf8_lossy(&failed.stderr).contains("the session was not started"));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}

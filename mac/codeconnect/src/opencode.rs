//! `codeconnect opencode`: what an argv becomes, decided before anything exists.
//!
//! Every argv is one of three things ([`classify`]): **hosted** — the interactive
//! TUI, `opencode <args unchanged>` in the private tmux server; **direct** — help,
//! version and shell completions, which are OpenCode's own answers and run
//! `opencode` itself in this terminal with no session; or **refused**, with a
//! sentence naming the cause and the fix ([`OpencodeRefusal`]).
//!
//! The walk mirrors how OpenCode 1.18.34 reads its argv (yargs, measured in
//! `fixtures/opencode/s10-launch-argv-1.18.34.json`). Subcommands dispatch in a root
//! parse that knows only `--print-logs`, `--log-level`, `--pure`, `-h` and `-v`, so
//! every other flag there takes the next word that is not flag-shaped as its value.
//! The walk consumes at most what that root parse consumes, so every token that
//! could dispatch a subcommand reaches the walk as a bare word and is judged.
//! A short option takes a following `--` as its value, as yargs does, so the walk
//! goes on judging the words after it; a `--` the walk reaches as its own word ends
//! the walk, and nothing after it is examined: OpenCode ignores it.
//!
//! Precedence is refused, then direct, then hosted: `--port 5 --help` is refused,
//! not answered with help.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use protocol::agent::AgentKind;
use protocol::config::Config;

use crate::codex::{candidates_for, first_native, resolve_native_binary, ResolvedBinary, OPENCODE};

/// Reads one environment variable. The process environment in production; a table
/// in tests, so a classification never depends on the machine running it.
pub(crate) type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// The `codeconnect opencode` command entry point.
///
/// Classifies the argv; a direct argv runs `opencode` itself. A hosted one resolves
/// the pinned native binary, then refuses a disabled CodeConnect plugin, a daemon
/// socket path too long to dial, and a daemon that cannot host OpenCode — every
/// refusal before a session identity, a tmux name or a file exists — and then hands
/// the launch to [`crate::start_agent`], which plans it with [`plan_launch`].
pub fn start(args: &[String]) -> Result<()> {
    let config = Config::load();
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let env = |key: &str| std::env::var_os(key);

    let folder = match classify(args, &env, &cwd).map_err(|refusal| anyhow!("{refusal}"))? {
        // OpenCode's own answer, printed in this terminal as native `opencode`
        // prints it: the first native candidate, unhashed, because nothing is
        // hosted. The same choice `codeconnect codex --help` makes.
        Launch::Direct => {
            use std::os::unix::process::CommandExt;
            let opencode = first_native(
                &OPENCODE,
                candidates_for(&OPENCODE, config.opencode_bin.as_deref()),
            )?;
            let err = Command::new(&opencode).args(args).exec();
            return Err(err).with_context(|| format!("running {}", opencode.display()));
        }
        Launch::Hosted { folder } => folder,
    };

    // Binary first, as for Claude and Codex: a missing or unusable executable
    // surfaces before any other check.
    let binary = resolve_native_binary(&OPENCODE, config.opencode_bin.as_deref())?;
    if let Some(path) = plugin_disabled(&folder, &env, &protocol::home_dir()) {
        bail!("{}", OpencodeRefusal::PluginDisabled { path });
    }
    let socket = protocol::socket_path();
    socket_fits(&socket).map_err(|refusal| anyhow!("{refusal}"))?;
    crate::daemon::refuse_unless_hostable(crate::daemon::agent_support(&AgentKind::Opencode))?;

    crate::start_agent(
        &config,
        crate::Agent::Opencode(Hosted {
            binary,
            folder,
            socket,
        }),
        args,
    )
}

/// A hosted launch that passed every check: the pinned executable, the folder
/// OpenCode opens, and the daemon socket its plugin dials.
pub(crate) struct Hosted {
    binary: ResolvedBinary,
    folder: PathBuf,
    socket: PathBuf,
}

/// The CodeConnect plugin, as OpenCode loads it from the session directory.
pub(crate) const PLUGIN_FILE: &str = "codeconnect-opencode.js";

/// OpenCode's TUI settings for the session: the plugin and its options.
pub(crate) const TUI_CONFIG_FILE: &str = "tui.json";

/// The agent's pid and start time, published by the pane's job once OpenCode runs,
/// and read by the plugin beside it.
pub(crate) const AGENT_FILE: &str = "agent.json";

/// The session directory's OpenCode files, which end with the session.
pub(crate) const SESSION_FILES: [&str; 3] = [PLUGIN_FILE, TUI_CONFIG_FILE, AGENT_FILE];

const PLUGIN_SOURCE: &str = include_str!("../opencode-plugin/codeconnect-opencode.js");

/// Plan a hosted launch: write the session's plugin and settings, and run the
/// pinned `opencode` with the argv unchanged.
///
/// The pane gets one variable, `OPENCODE_TUI_CONFIG`, naming the settings that
/// load the plugin. Its job verifies the binary again right before starting it
/// and publishes the agent's identity in the session directory. The supervisor
/// registers the run under the plugin's nonce and the folder OpenCode opens,
/// which is the folder the plugin reports.
pub(crate) fn plan_launch(
    hosted: &Hosted,
    session_id: &str,
    session_uid: &str,
    passthrough: &[String],
) -> Result<crate::AgentLaunchPlan> {
    let dir = protocol::session_dir(session_id, session_uid);
    let (plan, tui_config) = plan_in(hosted, &dir, &mint_nonce()?, passthrough)?;
    write_session_files(&dir, &tui_config)?;
    Ok(plan)
}

/// The plan for a session directory `dir` and plugin `nonce`, and the `tui.json`
/// to write there. Every path is checked to be text before anything is written.
fn plan_in(
    hosted: &Hosted,
    dir: &Path,
    nonce: &str,
    passthrough: &[String],
) -> Result<(crate::AgentLaunchPlan, String)> {
    let binary = utf8(&hosted.binary.path)?;
    let tui_config = tui_config(utf8(&hosted.socket)?, nonce);
    let mut argv = vec![binary.to_string()];
    argv.extend(passthrough.iter().cloned());
    let os = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
    let plan = crate::AgentLaunchPlan {
        argv,
        env: vec![(
            "OPENCODE_TUI_CONFIG".to_string(),
            utf8(&dir.join(TUI_CONFIG_FILE))?.to_string(),
        )],
        job_args: os(&[
            "--opencode-dir",
            utf8(dir)?,
            "--verify",
            binary,
            &hosted.binary.sha256,
        ]),
        supervisor_seat: os(&["--opencode-bin", binary, "--opencode-nonce", nonce]),
        registered_cwd: Some(utf8(&hosted.folder)?.to_string()),
        session_dir: dir.to_path_buf(),
    };
    Ok((plan, tui_config))
}

/// A path that has to travel as text: in an argv the pane reads back, in the
/// environment, or in JSON.
fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

/// The plugin's nonce: 128 random bits as 32 lowercase hex digits. It names the
/// run to the daemon; it is not a secret.
fn mint_nonce() -> Result<String> {
    let bytes = protocol::secret::random_bytes::<16>().context("reading random bytes")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The session's `tui.json`: the plugin, named relative to the file so OpenCode
/// loads it from the session directory, with the daemon socket and the nonce as
/// its options.
fn tui_config(socket: &str, nonce: &str) -> String {
    let text = |value: &str| serde_json::Value::from(value).to_string();
    format!(
        r#"{{"plugin":[[{},{{"socket":{},"nonce":{}}}]]}}"#,
        text(&format!("./{PLUGIN_FILE}")),
        text(socket),
        text(nonce)
    )
}

/// Write the plugin and `tui_config` into the private directory `dir`. Each file is
/// created new and owner-only; one already there refuses the launch.
fn write_session_files(dir: &Path, tui_config: &str) -> Result<()> {
    use std::io::Write;
    protocol::fsperm::private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, contents) in [(PLUGIN_FILE, PLUGIN_SOURCE), (TUI_CONFIG_FILE, tui_config)] {
        let path = dir.join(name);
        protocol::fsperm::create_private_new(&path)
            .and_then(|mut file| file.write_all(contents.as_bytes()))
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// What `codeconnect opencode <args>` does with an argv that is not refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Launch {
    /// Run `opencode` itself with the argv unchanged, in this terminal, with no
    /// session: help, version, shell completions.
    Direct,
    /// Host `opencode <args unchanged>`. `folder` is the directory OpenCode opens:
    /// `[project]` resolved as OpenCode resolves it, or the current directory.
    Hosted { folder: PathBuf },
}

/// Why `codeconnect opencode` refused. Each sentence names the cause, and the fix
/// where one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpencodeRefusal {
    /// A subcommand name or alias, as a bare word anywhere the walk judges. Only the
    /// interactive TUI is hosted; `attach`, `serve`, `web` and `acp` are servers or
    /// transports, `run` is non-interactive, and `pr` starts a second `opencode`.
    Subcommand { name: String },
    /// `--port`, `--hostname`, `--mdns`, `--mdns-domain`, `--cors`, in any spelling.
    /// `--port`, `--hostname` and `--mdns` open a TCP server with no authentication
    /// (the fixture records each listener); the five-name group is refused by name,
    /// as OpenCode itself treats it as one group.
    Transport { flag: String },
    /// `--mini` and `--pure`, in any spelling: either one starts OpenCode without the
    /// CodeConnect plugin.
    NoPlugin { flag: String },
    /// `--help` or `--version` given a value (`--help=…`, `--help false`, `-h true`)
    /// or negated alongside itself (`--help --no-help`): some of these print, others
    /// launch the TUI, so neither direct nor hosted is honest.
    HelpValue,
    /// `[project]` names no directory. OpenCode prints an error and exits 0, which
    /// reads as a clean run. An empty `[project]` is no project, as OpenCode reads it.
    NotADirectory { project: String },
    /// A short group with a non-letter in it, other than one `=` right after `m` or
    /// `s`. yargs attaches the rest of such a group to a value letter, so the next
    /// word is a bare word to OpenCode: `-m/x run` runs `opencode run`.
    ShortShape,
    /// `OPENCODE_PURE` is true: OpenCode loads no TUI plugin.
    PureEnv,
    /// `OPENCODE_TUI_CONFIG` is set. OpenCode reads one such file, and CodeConnect
    /// names its own plugin there.
    TuiConfigEnv,
    /// OpenCode's merged TUI settings, or its saved plugin state, disable the
    /// CodeConnect plugin; `path` is the file whose value wins.
    PluginDisabled { path: PathBuf },
    /// The daemon's socket path is too long for a unix socket address.
    SocketPath { path: PathBuf, len: usize },
}

impl std::fmt::Display for OpencodeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpencodeRefusal::Subcommand { name } => write!(
                f,
                "`{name}` is the name of an OpenCode subcommand, and `codeconnect opencode` \
                 hosts only the interactive TUI, so it refuses that word: run \
                 `opencode {name}` directly, or write a folder of that name as `./{name}`"
            ),
            OpencodeRefusal::Transport { flag } => write!(
                f,
                "`{flag}` is one of OpenCode's network-server options (`--port`, `--hostname`, \
                 `--mdns`, `--mdns-domain`, `--cors`), which `codeconnect opencode` refuses in \
                 any spelling: drop it"
            ),
            OpencodeRefusal::NoPlugin { flag } => write!(
                f,
                "`{flag}` is a spelling of OpenCode's `--pure` or `--mini`, either of which \
                 starts OpenCode without the CodeConnect plugin, so `codeconnect opencode` \
                 refuses them in any spelling: drop it"
            ),
            OpencodeRefusal::HelpValue => f.write_str(
                "`--help` or `--version` with a value, a `--no-` form, or `--version` after \
                 `-c --` is refused: OpenCode prints for some of these spellings and starts the \
                 TUI for others. Write the option alone, or drop it",
            ),
            OpencodeRefusal::NotADirectory { project } => write!(
                f,
                "`{project}` is not a directory; OpenCode opens `[project]` as a folder, \
                 resolved from `$PWD`: pass a folder"
            ),
            OpencodeRefusal::ShortShape => f.write_str(
                "write `-m value` or `--model=value`: a short option followed by a non-letter \
                 is parsed differently by OpenCode",
            ),
            OpencodeRefusal::PureEnv => f.write_str(
                "OPENCODE_PURE is set to true, so OpenCode would start without the CodeConnect \
                 plugin: unset OPENCODE_PURE and launch again",
            ),
            OpencodeRefusal::TuiConfigEnv => f.write_str(
                "OPENCODE_TUI_CONFIG is set; CodeConnect needs that setting for its own plugin. \
                 Unset it and launch again",
            ),
            OpencodeRefusal::PluginDisabled { path } => write!(
                f,
                "the CodeConnect plugin is disabled in {}; enable it there and launch again",
                path.display()
            ),
            OpencodeRefusal::SocketPath { path, len } => write!(
                f,
                "the daemon socket path {} is {len} bytes; a unix socket path must be shorter \
                 than {}",
                path.display(),
                crate::codex_host::SUN_LEN_LIMIT
            ),
        }
    }
}

// ------------------------------------------------------------------ the argv

/// Every subcommand and alias OpenCode 1.18.34 dispatches, hidden ones included
/// (`generate`, `console`; `auth` is `providers`, `plug` is `plugin`).
const SUBCOMMANDS: [&str; 25] = [
    "completion",
    "acp",
    "mcp",
    "attach",
    "run",
    "debug",
    "providers",
    "auth",
    "agent",
    "upgrade",
    "uninstall",
    "serve",
    "web",
    "models",
    "stats",
    "export",
    "import",
    "github",
    "pr",
    "session",
    "plugin",
    "plug",
    "db",
    "generate",
    "console",
];

/// The network options, by kebab name.
const TRANSPORT: [&str; 5] = ["port", "hostname", "mdns", "mdns-domain", "cors"];

/// Long options that take the next word as their value when it is not flag-shaped.
const VALUE_LONGS: [&str; 6] = [
    "model",
    "session",
    "prompt",
    "agent",
    "log-level",
    "replay-limit",
];

/// Boolean long options: a following literal `true` or `false` is their value.
const BOOL_LONGS: [&str; 8] = [
    "continue",
    "fork",
    "auto",
    "yolo",
    "dangerously-skip-permissions",
    "print-logs",
    "replay",
    "demo",
];

/// What an option takes from the word after it.
enum Takes {
    Nothing,
    /// The next word, unless it is flag-shaped.
    Value,
    /// The next word, if it is literally `true` or `false`.
    Bool,
    /// `--help`, `--version`, or a short group ending in `h` or `v`: a following
    /// `true` or `false` is their value, which is refused.
    HelpBool,
}

/// Help (`0`) and version (`1`): which were asked for by a flag, and which negated
/// by `--no-help`/`--no-version`.
#[derive(Default)]
struct HelpFlags {
    asked: [bool; 2],
    negated: [bool; 2],
}

/// What `codeconnect opencode <args>` does: [`Launch`] or a refusal.
///
/// `env` supplies `OPENCODE_PURE`, `OPENCODE_TUI_CONFIG` and `PWD`; `cwd` is the
/// launcher's working directory, where the pane starts.
pub(crate) fn classify(args: &[String], env: Env, cwd: &Path) -> Result<Launch, OpencodeRefusal> {
    if env("OPENCODE_PURE").is_some_and(|value| truthy(&value)) {
        return Err(OpencodeRefusal::PureEnv);
    }
    if env("OPENCODE_TUI_CONFIG").is_some_and(|value| !value.is_empty()) {
        return Err(OpencodeRefusal::TuiConfigEnv);
    }

    let mut direct = false;
    let mut help = HelpFlags::default();
    let mut project: Option<&String> = None;
    // Set once `-c` has taken a `--`: the TUI reads `-c` as a boolean and that `--`
    // as the end of its options, so no later word is `[project]`.
    let mut project_closed = false;
    let mut tokens = args.iter().peekable();
    while let Some(token) = tokens.next() {
        if token == "--" {
            break;
        }
        // After `-c --` the TUI parses the rest as its own options, where version is
        // not a print-and-exit flag: `-c -- --version` and `-c -- -v` start the TUI.
        // Help still prints there.
        let version_before = help.asked[1];
        let takes = if let Some(long) = token.strip_prefix("--") {
            long_option(token, long, &mut direct, &mut help)?
        } else if let Some(group) = token.strip_prefix('-').filter(|group| !group.is_empty()) {
            let takes = short_group(group, &mut direct, &mut help)?;
            // yargs takes a word as a short option's value unless it is `-x` or
            // `--x`, so `--` itself is a value; only `h` and `v`, the root parse's
            // booleans, decline it.
            let last = group.chars().last();
            if !group.contains('=')
                && !matches!(last, Some('h' | 'v'))
                && tokens.next_if(|next| *next == "--").is_some()
            {
                project_closed |= last == Some('c');
                continue;
            }
            takes
        } else if SUBCOMMANDS.contains(&token.as_str()) {
            return Err(OpencodeRefusal::Subcommand {
                name: token.clone(),
            });
        } else {
            // `help` as any bare word prints OpenCode's help.
            if token == "help" {
                direct = true;
            }
            if !project_closed {
                project.get_or_insert(token);
            }
            Takes::Nothing
        };
        if project_closed && help.asked[1] && !version_before {
            return Err(OpencodeRefusal::HelpValue);
        }
        let boolean = |next: &&String| *next == "true" || *next == "false";
        match takes {
            Takes::Nothing => {}
            Takes::Value => {
                tokens.next_if(|next| !next.starts_with('-'));
            }
            Takes::Bool => {
                tokens.next_if(boolean);
            }
            Takes::HelpBool => {
                if tokens.peek().is_some_and(boolean) {
                    return Err(OpencodeRefusal::HelpValue);
                }
            }
        }
    }
    // `--help --no-help` launches the TUI: the negation wins over the request.
    if (0..2).any(|i| help.asked[i] && help.negated[i]) {
        return Err(OpencodeRefusal::HelpValue);
    }
    if direct {
        return Ok(Launch::Direct);
    }

    // OpenCode tests `[project]` for truthiness: an empty one is no project.
    let Some(project) = project.filter(|project| !project.is_empty()) else {
        return Ok(Launch::Hosted {
            folder: resolve(cwd),
        });
    };
    // OpenCode: `[project]` joined onto `$PWD`, both resolved — lexically, then
    // through symlinks.
    let base = resolve(&pane_pwd(env, cwd));
    let folder = resolve(&base.join(project));
    if !folder.is_dir() {
        return Err(OpencodeRefusal::NotADirectory {
            project: project.clone(),
        });
    }
    Ok(Launch::Hosted { folder })
}

/// The `$PWD` OpenCode sees in the pane, whose `/bin/sh` keeps the caller's only
/// when it is absolute and names the same directory as `cwd`, and otherwise sets it
/// to `cwd`.
fn pane_pwd(env: Env, cwd: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let same_directory = |pwd: &Path| match (std::fs::metadata(pwd), std::fs::metadata(cwd)) {
        (Ok(pwd), Ok(cwd)) => pwd.dev() == cwd.dev() && pwd.ino() == cwd.ino(),
        _ => false,
    };
    env("PWD")
        .map(PathBuf::from)
        .filter(|pwd| pwd.is_absolute() && same_directory(pwd))
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// One `--name[=value]` option. The name is compared kebab-folded (`--mdnsDomain` is
/// `--mdns-domain`); the refused groups also match the `--no-` form and a dotted
/// name (`--no-mdns`, `--port.x=1`).
///
/// A dotted help or version name (`--help.x`, `--no-help.x=1`) makes the option an
/// object, which yargs reads as true: it prints. Shell completion runs for any
/// spelling yargs sets the key with, `--no-get-yargs-completions` included.
fn long_option(
    token: &str,
    long: &str,
    direct: &mut bool,
    help: &mut HelpFlags,
) -> Result<Takes, OpencodeRefusal> {
    let (name, value) = match long.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (long, None),
    };
    let name = kebab(name);
    let positive = name
        .strip_prefix("no-")
        .unwrap_or(&name)
        .trim_start_matches('-');
    let root = positive.split('.').next().unwrap_or(positive);
    if TRANSPORT.contains(&root) {
        return Err(OpencodeRefusal::Transport {
            flag: token.to_string(),
        });
    }
    if matches!(root, "mini" | "pure") {
        return Err(OpencodeRefusal::NoPlugin {
            flag: token.to_string(),
        });
    }
    if root == "get-yargs-completions" {
        *direct = true;
        return Ok(Takes::Nothing);
    }
    if let Some(i) = ["help", "version"].iter().position(|flag| *flag == root) {
        if name == format!("no-{root}") {
            help.negated[i] = true;
            return match value {
                Some(_) => Err(OpencodeRefusal::HelpValue),
                None => Ok(Takes::Nothing),
            };
        }
        help.asked[i] = true;
        if name == root {
            return match value {
                Some(_) => Err(OpencodeRefusal::HelpValue),
                None => {
                    *direct = true;
                    Ok(Takes::HelpBool)
                }
            };
        }
        // Dotted, so an object: printed whatever its value.
        *direct = true;
        return Ok(Takes::Nothing);
    }
    match name.as_str() {
        // A `--no-` form is yargs' negation: it sets `false` and takes no value.
        _ if value.is_some() || name.starts_with("no-") => Ok(Takes::Nothing),
        _ if VALUE_LONGS.contains(&root) => Ok(Takes::Value),
        _ if BOOL_LONGS.contains(&root) => Ok(Takes::Bool),
        _ => Ok(Takes::Nothing),
    }
}

/// One short group, `-abc` or `-m=value` (`group` is what follows the `-`).
///
/// Letters only, or one `=` right after a final `m`/`s` (the value letters): any
/// other shape is read by yargs differently from how it looks and is refused. `h`
/// or `v` among the letters asks for help or version, and a final `h` or `v` takes
/// `true` or `false` as its value. Otherwise the final letter decides what the next
/// word is: `m`/`s` take it as a value, `c` takes `true` or `false`.
fn short_group(
    group: &str,
    direct: &mut bool,
    help: &mut HelpFlags,
) -> Result<Takes, OpencodeRefusal> {
    let (letters, attached) = match group.split_once('=') {
        Some((letters, _)) => (letters, true),
        None => (group, false),
    };
    let last = letters.chars().last();
    let shaped = !letters.is_empty()
        && letters.bytes().all(|b| b.is_ascii_alphabetic())
        && (!attached || matches!(last, Some('m' | 's')));
    if !shaped {
        return Err(OpencodeRefusal::ShortShape);
    }
    help.asked[0] |= letters.contains('h');
    help.asked[1] |= letters.contains('v');
    if letters.contains(['h', 'v']) {
        *direct = true;
        return Ok(match last {
            Some('h' | 'v') => Takes::HelpBool,
            _ => Takes::Nothing,
        });
    }
    Ok(match (attached, last) {
        (false, Some('m' | 's')) => Takes::Value,
        (false, Some('c')) => Takes::Bool,
        _ => Takes::Nothing,
    })
}

/// A long option name folded to kebab case: `printLogs` is `print-logs`. Leading
/// dashes past the first two are dropped.
fn kebab(name: &str) -> String {
    let mut folded = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            folded.push('-');
            folded.push(c.to_ascii_lowercase());
        } else {
            folded.push(c);
        }
    }
    folded.trim_start_matches('-').to_string()
}

/// OpenCode's reading of a boolean environment variable: `1` or `true`, any case.
fn truthy(value: &OsString) -> bool {
    value
        .to_str()
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// A path as OpenCode resolves one: `.` and `..` lexically, then symlinks. A path
/// that cannot be resolved through the filesystem stays lexical.
fn resolve(path: &Path) -> PathBuf {
    let mut lexical = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                lexical.pop();
            }
            other => lexical.push(other),
        }
    }
    std::fs::canonicalize(&lexical).unwrap_or(lexical)
}

// ---------------------------------------------------------------- the plugin

/// The CodeConnect plugin's id, the key OpenCode's `plugin_enabled` maps use.
const PLUGIN_ID: &str = "codeconnect";

/// The file that disables the CodeConnect plugin for a session in `folder`, if one
/// does.
///
/// OpenCode decides from `plugin_enabled[id]`: its TUI config files merged in its
/// order — the global config directory, then `tui.json(c)` from the filesystem root
/// down to `folder`, then each `.opencode` directory walking up from `folder`, then
/// `~/.opencode` and `OPENCODE_CONFIG_DIR` (read after OpenCode has changed into
/// `folder`, so a relative one is relative to it) — and then its saved state
/// (`kv.json`), which wins. A file that cannot be read or parsed is skipped, as OpenCode skips
/// it. `{env:…}`/`{file:…}` substitution is not applied.
fn plugin_disabled(folder: &Path, env: Env, home: &Path) -> Option<PathBuf> {
    let set = |key: &str| env(key).filter(|value| !value.is_empty());
    let global = set("XDG_CONFIG_HOME")
        .map_or_else(|| home.join(".config"), PathBuf::from)
        .join("opencode");
    let project_config = !env("OPENCODE_DISABLE_PROJECT_CONFIG").is_some_and(|v| truthy(&v));
    let config_dir = set("OPENCODE_CONFIG_DIR").map(|dir| folder.join(dir));

    let mut files = tui_files(&global).to_vec();
    if project_config {
        let mut nearest_first = Vec::new();
        for dir in folder.ancestors() {
            for name in ["tui.jsonc", "tui.json"] {
                let file = dir.join(name);
                if file.exists() {
                    nearest_first.push(file);
                }
            }
        }
        files.extend(nearest_first.into_iter().rev());
    }
    let mut dirs = vec![global];
    if project_config {
        dirs.extend(
            folder
                .ancestors()
                .map(|dir| dir.join(".opencode"))
                .filter(|dir| dir.exists()),
        );
    }
    dirs.extend(Some(home.join(".opencode")).filter(|dir| dir.exists()));
    dirs.extend(config_dir.clone());
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if seen.contains(&dir) {
            continue;
        }
        if dir.to_string_lossy().ends_with(".opencode") || Some(&dir) == config_dir.as_ref() {
            files.extend(tui_files(&dir));
        }
        seen.push(dir);
    }

    let mut decided: Option<(bool, PathBuf)> = None;
    for file in files {
        if let Some(enabled) = config_file_enables(&file) {
            decided = Some((enabled, file));
        }
    }
    let kv = set("XDG_STATE_HOME")
        .map_or_else(|| home.join(".local/state"), PathBuf::from)
        .join("opencode/kv.json");
    if let Some(enabled) = kv_enables(&kv) {
        decided = Some((enabled, kv));
    }
    match decided {
        Some((false, path)) => Some(path),
        _ => None,
    }
}

fn tui_files(dir: &Path) -> [PathBuf; 2] {
    [dir.join("tui.json"), dir.join("tui.jsonc")]
}

/// `plugin_enabled[id]` in one TUI config file. A top-level `plugin_enabled`
/// replaces one nested under `tui`; a map holding anything but booleans fails
/// OpenCode's schema, which skips the whole file.
fn config_file_enables(file: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(file).ok()?;
    let data: serde_json::Value = serde_json::from_str(&strip_jsonc(&text)).ok()?;
    let data = data.as_object()?;
    let map = match data.get("plugin_enabled") {
        Some(map) => map,
        None => data.get("tui")?.as_object()?.get("plugin_enabled")?,
    };
    let map = map.as_object()?;
    if !map.values().all(serde_json::Value::is_boolean) {
        return None;
    }
    map.get(PLUGIN_ID)?.as_bool()
}

/// `plugin_enabled[id]` in OpenCode's saved state, where a non-boolean entry is
/// ignored.
fn kv_enables(kv: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(kv).ok()?;
    let state: serde_json::Value = serde_json::from_str(&text).ok()?;
    state.get("plugin_enabled")?.get(PLUGIN_ID)?.as_bool()
}

/// JSONC as OpenCode accepts it, made JSON: comments removed and trailing commas
/// dropped, outside strings.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                while chars.next_if(|&next| next != '\n').is_some() {}
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = ' ';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                out.push(' ');
            }
            '}' | ']' => {
                let kept = out.trim_end().len();
                if out[..kept].ends_with(',') {
                    out.truncate(kept - 1);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Refuse a daemon socket path a unix socket address cannot hold: the plugin dials
/// it from inside the session.
fn socket_fits(path: &Path) -> Result<(), OpencodeRefusal> {
    let len = path.as_os_str().len();
    if len >= crate::codex_host::SUN_LEN_LIMIT {
        return Err(OpencodeRefusal::SocketPath {
            path: path.to_path_buf(),
            len,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const LAUNCH_ARGV: &str =
        include_str!("../../../fixtures/opencode/s10-launch-argv-1.18.34.json");

    /// A private temp directory, canonical (so a symlinked temp root such as macOS's
    /// `/var` → `/private/var` cannot make two spellings of one folder differ),
    /// removed when it drops.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "codeconnect-opencode-test-{}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
                protocol::time::now_unix_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir.canonicalize().unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn table(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// Classify with exactly the variables in `env`.
    fn classify_with(
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<Launch, OpencodeRefusal> {
        let lookup = |key: &str| env.get(key).map(OsString::from);
        classify(args, &lookup, cwd)
    }

    fn classify_in(parts: &[&str], cwd: &Path) -> Result<Launch, OpencodeRefusal> {
        classify(&argv(parts), &no_env, cwd)
    }

    /// **Every row of the measured launch table.** The fixture's launch folder, its
    /// directories, file and symlink are rebuilt under a temp directory, and each
    /// row's argv and environment are classified there: an `H` row must be hosted,
    /// and in the folder OpenCode opened where the row records one (rows where
    /// OpenCode exited record none); a `D` row direct; and an `R` row refused with
    /// the row's own sentence. Every mismatch is reported, not only the first.
    #[test]
    fn every_measured_launch_row_gets_its_verdict() {
        let fixture: serde_json::Value = serde_json::from_str(LAUNCH_ARGV).unwrap();
        let context = &fixture["context"];
        let launch_folder = Path::new(context["launch_folder"].as_str().unwrap());
        let prefix = launch_folder.parent().unwrap().to_str().unwrap();
        let scratch = Scratch::new();
        let map = |value: &str| match value.strip_prefix(prefix) {
            Some(rest) => format!("{}{rest}", scratch.0.display()),
            None => value.to_string(),
        };

        let cwd = PathBuf::from(map(launch_folder.to_str().unwrap()));
        std::fs::create_dir_all(&cwd).unwrap();
        for dir in context["directories"].as_array().unwrap() {
            std::fs::create_dir_all(cwd.join(dir.as_str().unwrap())).unwrap();
        }
        for file in context["files"].as_array().unwrap() {
            std::fs::write(cwd.join(file.as_str().unwrap()), b"").unwrap();
        }
        for (link, target) in context["symlink"].as_object().unwrap() {
            std::os::unix::fs::symlink(map(target.as_str().unwrap()), map(link)).unwrap();
        }

        let rows = fixture["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 195, "the measured table has 195 rows");
        let mut mismatches = Vec::new();
        for row in rows {
            let case = row["case"].as_str().unwrap();
            let args: Vec<String> = row["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|token| map(token.as_str().unwrap()))
                .collect();
            let env: BTreeMap<String, String> = row
                .get("env")
                .and_then(|env| env.as_object())
                .map(|env| {
                    env.iter()
                        .map(|(key, value)| (key.clone(), map(value.as_str().unwrap())))
                        .collect()
                })
                .unwrap_or_default();
            let got = classify_with(&args, &env, &cwd);
            let verdict = row["verdict"].as_str().unwrap();
            let ok = match (verdict, &got) {
                ("H", Ok(Launch::Hosted { folder })) => {
                    match row["observed"]["session_folder"].as_str() {
                        Some(opened) => folder.as_path() == Path::new(&map(opened)),
                        None => true,
                    }
                }
                ("D", Ok(Launch::Direct)) => true,
                ("R", Err(refusal)) => refusal.to_string() == row["reason"].as_str().unwrap(),
                _ => false,
            };
            if !ok {
                mismatches.push(format!("{case} {args:?}: expected {verdict}, got {got:?}"));
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    /// **The environment refuses first**, before any argv is read, and reads the
    /// variables as OpenCode does: `OPENCODE_PURE` only when `1` or `true` in any
    /// case, `OPENCODE_TUI_CONFIG` whenever it names a file.
    #[test]
    fn the_environment_refuses_before_the_argv_is_read() {
        let scratch = Scratch::new();
        for value in ["1", "true", "TRUE", "True"] {
            assert_eq!(
                classify_with(
                    &argv(&["--version"]),
                    &table(&[("OPENCODE_PURE", value)]),
                    &scratch.0
                ),
                Err(OpencodeRefusal::PureEnv),
                "OPENCODE_PURE={value}"
            );
        }
        for value in ["0", "false", "yes", ""] {
            assert!(
                classify_with(&[], &table(&[("OPENCODE_PURE", value)]), &scratch.0).is_ok(),
                "OPENCODE_PURE={value} is not true to OpenCode"
            );
        }
        let refused = classify_with(
            &argv(&["--help"]),
            &table(&[("OPENCODE_TUI_CONFIG", "/elsewhere/tui.json")]),
            &scratch.0,
        )
        .unwrap_err();
        assert_eq!(refused, OpencodeRefusal::TuiConfigEnv);
        assert_eq!(
            refused.to_string(),
            "OPENCODE_TUI_CONFIG is set; CodeConnect needs that setting for its own plugin. \
             Unset it and launch again"
        );
        assert!(
            classify_with(&[], &table(&[("OPENCODE_TUI_CONFIG", "")]), &scratch.0).is_ok(),
            "an empty OPENCODE_TUI_CONFIG names no file to OpenCode"
        );
    }

    /// **Help is direct only in the spellings that print.** `--no-help` alone sets
    /// help to `false` and launches the TUI, so it is hosted; a dotted name makes
    /// help an object, which prints, so it is direct; a negation beside a request
    /// launches the TUI and is refused. A refused group stays refused in every
    /// spelling, capitalised and extra-dashed included.
    #[test]
    fn spellings_neither_widen_direct_nor_narrow_a_refusal() {
        let scratch = Scratch::new();
        let hosted = Ok(Launch::Hosted {
            folder: scratch.0.clone(),
        });
        for parts in [&["--no-help"][..], &["--no-version"]] {
            assert_eq!(classify_in(parts, &scratch.0), hosted, "{parts:?}");
        }
        for parts in [
            &["--help.x"][..],
            &["--no-version.x=1"],
            &["--noGetYargsCompletions"],
        ] {
            assert_eq!(
                classify_in(parts, &scratch.0),
                Ok(Launch::Direct),
                "{parts:?}"
            );
        }
        for parts in [
            &["-vh", "true"][..],
            &["--no-version", "-cv"],
            &["--version=1"],
        ] {
            assert_eq!(
                classify_in(parts, &scratch.0),
                Err(OpencodeRefusal::HelpValue),
                "{parts:?}"
            );
        }
        for flag in [
            "--Port",
            "---port",
            "--no-Cors",
            "--mdnsDomain=x",
            "--hostname.a=b",
        ] {
            assert_eq!(
                classify_in(&[flag], &scratch.0),
                Err(OpencodeRefusal::Transport {
                    flag: flag.to_string()
                }),
                "{flag}"
            );
        }
        for flag in ["--Mini", "--no-mini", "--pure.x"] {
            assert_eq!(
                classify_in(&[flag], &scratch.0),
                Err(OpencodeRefusal::NoPlugin {
                    flag: flag.to_string()
                }),
                "{flag}"
            );
        }
    }

    /// **The walk takes no more than yargs' root parse.** A `--no-` form takes no
    /// value (yargs' negation), so the word after it is judged; a dotted value name
    /// takes its value as the plain name does.
    #[test]
    fn a_negated_option_takes_no_value_and_a_dotted_one_does() {
        let scratch = Scratch::new();
        assert_eq!(
            classify_in(&["--no-replay", "run"], &scratch.0),
            Err(OpencodeRefusal::Subcommand { name: "run".into() })
        );
        assert_eq!(
            classify_in(&["--no-continue", "true"], &scratch.0),
            Err(OpencodeRefusal::NotADirectory {
                project: "true".into()
            })
        );
        assert_eq!(
            classify_in(&["--model.x", "help"], &scratch.0),
            Ok(Launch::Hosted {
                folder: scratch.0.clone()
            })
        );
        // A bool letter that is not last takes nothing: `help` is a bare word.
        assert_eq!(
            classify_in(&["-mc", "help"], &scratch.0),
            Ok(Launch::Direct)
        );
        // A bare `-` is a word, and a word that names no directory is refused.
        assert_eq!(
            classify_in(&["-"], &scratch.0),
            Err(OpencodeRefusal::NotADirectory {
                project: "-".into()
            })
        );
    }

    /// **`[project]` resolves as OpenCode resolves it**: `$PWD` through its symlinks
    /// first, then the project joined lexically. With `$PWD` a symlink to `sub`,
    /// `..` is `sub`'s parent, not the link's.
    #[test]
    fn the_project_is_joined_onto_the_resolved_pwd() {
        let scratch = Scratch::new();
        let project = scratch.0.join("project");
        let sub = project.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::os::unix::fs::symlink(&sub, scratch.0.join("link")).unwrap();
        let link = scratch.0.join("link");
        let env = table(&[("PWD", link.to_str().unwrap())]);
        assert_eq!(
            classify_with(&argv(&[".."]), &env, &sub),
            Ok(Launch::Hosted {
                folder: project.clone()
            })
        );
        // No project: the working directory, whatever `$PWD` says.
        assert_eq!(
            classify_with(&[], &env, &project),
            Ok(Launch::Hosted { folder: project })
        );
    }

    /// **The pane's `$PWD` is the one `/bin/sh` hands OpenCode**: the caller's when it
    /// is absolute and names the working directory, a symlink alias included; the
    /// working directory otherwise.
    #[test]
    fn the_pane_keeps_pwd_only_when_it_names_the_working_directory() {
        let scratch = Scratch::new();
        let sub = scratch.0.join("project/sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(scratch.0.join("elsewhere")).unwrap();
        let link = scratch.0.join("link");
        std::os::unix::fs::symlink(&sub, &link).unwrap();
        let with_pwd = |pwd: &Path| {
            let pwd = pwd.as_os_str().to_owned();
            move |key: &str| (key == "PWD").then(|| pwd.clone())
        };
        assert_eq!(pane_pwd(&with_pwd(&link), &sub), link);
        for pwd in [
            scratch.0.join("elsewhere"),
            scratch.0.join("missing"),
            PathBuf::from("project/sub"),
            PathBuf::new(),
        ] {
            assert_eq!(pane_pwd(&with_pwd(&pwd), &sub), sub, "PWD={pwd:?}");
        }
        assert_eq!(pane_pwd(&no_env, &sub), sub);
    }

    /// **A stale `$PWD` does not move `[project]`**: the pane's shell replaces it with
    /// the working directory, so `.` and a relative project resolve from there.
    #[test]
    fn a_stale_pwd_resolves_the_project_from_the_working_directory() {
        let scratch = Scratch::new();
        let project = scratch.0.join("project");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::create_dir_all(scratch.0.join("elsewhere/sub")).unwrap();
        let env = table(&[("PWD", scratch.0.join("elsewhere").to_str().unwrap())]);
        assert_eq!(
            classify_with(&argv(&["."]), &env, &project),
            Ok(Launch::Hosted {
                folder: project.clone()
            })
        );
        assert_eq!(
            classify_with(&argv(&["sub"]), &env, &project),
            Ok(Launch::Hosted {
                folder: project.join("sub")
            })
        );
    }

    /// The three sentences the measured table has no row for, verbatim.
    #[test]
    fn the_launch_refusals_name_the_cause_and_the_fix() {
        assert_eq!(
            OpencodeRefusal::PluginDisabled {
                path: PathBuf::from("/Users/u/.local/state/opencode/kv.json")
            }
            .to_string(),
            "the CodeConnect plugin is disabled in /Users/u/.local/state/opencode/kv.json; \
             enable it there and launch again"
        );
        let fits = PathBuf::from(format!("/{}", "s".repeat(102)));
        assert_eq!(fits.as_os_str().len(), 103);
        assert_eq!(socket_fits(&fits), Ok(()));
        let long = PathBuf::from(format!("/{}", "s".repeat(103)));
        let refused = socket_fits(&long).unwrap_err();
        assert_eq!(
            refused.to_string(),
            format!(
                "the daemon socket path {} is 104 bytes; a unix socket path must be shorter \
                 than 104",
                long.display()
            )
        );
    }

    /// Write `text` at `path`, creating its directory.
    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const DISABLED: &str = r#"{"plugin_enabled":{"codeconnect":false}}"#;
    const ENABLED: &str = r#"{"plugin_enabled":{"codeconnect":true}}"#;

    /// **The plugin's enabled state is OpenCode's own merge.** Each later source
    /// overrides an earlier one: the global config, the project's `tui.json(c)`
    /// root-first, `.opencode` directories, `OPENCODE_CONFIG_DIR` (a relative one
    /// from the session folder), and last the saved state. The refusal names the
    /// file whose value won.
    ///
    /// The ancestor walk goes past the scratch directory to the filesystem root, so
    /// a `tui.json(c)` or `.opencode` in a real ancestor of the temp directory takes
    /// part in the merge too.
    #[test]
    fn the_plugin_is_disabled_by_the_source_that_wins_the_merge() {
        let scratch = Scratch::new();
        let home = scratch.0.join("home");
        let folder = home.join("work/project");
        std::fs::create_dir_all(&folder).unwrap();
        let global = home.join(".config/opencode/tui.json");
        let project = folder.join("tui.jsonc");
        let dot = folder.join(".opencode/tui.json");
        let kv = home.join(".local/state/opencode/kv.json");
        let disabled = |env: &BTreeMap<String, String>| {
            let lookup = |key: &str| env.get(key).map(OsString::from);
            plugin_disabled(&folder, &lookup, &home)
        };
        let none = BTreeMap::new();

        assert_eq!(disabled(&none), None, "nothing says anything: enabled");

        write(&global, DISABLED);
        assert_eq!(disabled(&none), Some(global.clone()));

        // Comments and trailing commas, as OpenCode accepts them.
        write(
            &project,
            "// project\n{\"plugin_enabled\": {/* on */ \"codeconnect\": true,},}\n",
        );
        assert_eq!(
            disabled(&none),
            None,
            "the project overrides the global file"
        );
        assert_eq!(
            disabled(&table(&[("OPENCODE_DISABLE_PROJECT_CONFIG", "1")])),
            Some(global.clone()),
            "without project config the global file decides"
        );

        write(&dot, DISABLED);
        assert_eq!(
            disabled(&none),
            Some(dot.clone()),
            "`.opencode` comes after"
        );

        let config_dir = scratch.0.join("config-dir");
        write(&config_dir.join("tui.json"), ENABLED);
        assert_eq!(
            disabled(&table(&[(
                "OPENCODE_CONFIG_DIR",
                config_dir.to_str().unwrap()
            )])),
            None,
            "OPENCODE_CONFIG_DIR comes after `.opencode`"
        );
        write(&folder.join("relative-dir/tui.json"), ENABLED);
        assert_eq!(
            disabled(&table(&[("OPENCODE_CONFIG_DIR", "relative-dir")])),
            None,
            "a relative OPENCODE_CONFIG_DIR is read from the session folder"
        );

        write(&kv, ENABLED);
        assert_eq!(disabled(&none), None, "the saved state wins");
        write(&kv, DISABLED);
        assert_eq!(disabled(&none), Some(kv.clone()));
        let state = scratch.0.join("state");
        write(&state.join("opencode/kv.json"), r#"{"plugin_enabled":{}}"#);
        assert_eq!(
            disabled(&table(&[("XDG_STATE_HOME", state.to_str().unwrap())])),
            Some(dot),
            "XDG_STATE_HOME moves the saved state"
        );
    }

    /// **A file OpenCode skips is skipped here.** Unparseable text, and a map whose
    /// values are not all booleans (OpenCode's schema refuses the whole file), decide
    /// nothing; a map nested under `tui` counts unless a top-level one replaces it.
    #[test]
    fn a_file_opencode_skips_decides_nothing() {
        let scratch = Scratch::new();
        let home = scratch.0.join("home");
        let folder = home.join("project");
        std::fs::create_dir_all(&folder).unwrap();
        let global = home.join(".config/opencode/tui.json");
        let decide = || plugin_disabled(&folder, &no_env, &home);

        write(&global, "{\"plugin_enabled\": {\"codeconnect\": false");
        assert_eq!(decide(), None, "unparseable");
        write(
            &global,
            r#"{"plugin_enabled":{"codeconnect":false,"other":"no"}}"#,
        );
        assert_eq!(decide(), None, "fails the schema");
        write(
            &global,
            r#"{"tui":{"plugin_enabled":{"codeconnect":false}}}"#,
        );
        assert_eq!(decide(), Some(global.clone()), "nested under `tui`");
        write(
            &global,
            r#"{"tui":{"plugin_enabled":{"codeconnect":false}},"plugin_enabled":{}}"#,
        );
        assert_eq!(decide(), None, "a top-level map replaces the nested one");
    }

    #[test]
    fn jsonc_comments_inside_strings_are_text() {
        assert_eq!(
            strip_jsonc("{\"a\": \"//x\", \"b\": \"/*y*/\", \"c\": \"\\\"//\"} // end"),
            "{\"a\": \"//x\", \"b\": \"/*y*/\", \"c\": \"\\\"//\"} "
        );
        assert_eq!(strip_jsonc("[1, 2 ,\n]"), "[1, 2 ]");
    }

    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn hosted(folder: &Path) -> Hosted {
        Hosted {
            binary: ResolvedBinary {
                path: PathBuf::from("/opt/homebrew/lib/node_modules/opencode-ai/bin/opencode.exe"),
                sha256: SHA256.to_string(),
            },
            folder: folder.to_path_buf(),
            socket: PathBuf::from("/Users/ada/.codeconnect/ccd.sock"),
        }
    }

    /// **The settings OpenCode is pointed at, exactly.** One plugin, named relative
    /// to the file so OpenCode loads it from the session directory, with the
    /// daemon socket and the nonce as its options. A path that needs escaping is
    /// escaped, so the file stays the JSON it says.
    #[test]
    fn the_tui_settings_name_the_plugin_beside_them_with_its_options() {
        assert_eq!(
            tui_config("/Users/ada/.codeconnect/ccd.sock", NONCE),
            r#"{"plugin":[["./codeconnect-opencode.js",{"socket":"/Users/ada/.codeconnect/ccd.sock","nonce":"0123456789abcdef0123456789abcdef"}]]}"#
        );
        let odd = "/Users/a \"b\"\\c/ccd.sock";
        let parsed: serde_json::Value = serde_json::from_str(&tui_config(odd, NONCE)).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({"plugin": [["./codeconnect-opencode.js", {"socket": odd, "nonce": NONCE}]]})
        );
    }

    /// The nonce is 128 bits, in the shape the daemon accepts: 32 lowercase hex.
    #[test]
    fn a_nonce_is_32_lowercase_hex_digits_and_fresh_each_time() {
        let nonce = mint_nonce().unwrap();
        assert_eq!(nonce.len(), 32);
        assert!(nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(nonce, mint_nonce().unwrap());
    }

    /// **The session files are private and never written over.** The directory is
    /// `0700`, each file `0600` and created new: a second launch into the same
    /// directory, or a file or link already at a name, refuses rather than
    /// writing through it.
    #[test]
    fn the_session_files_are_private_and_never_written_over() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new();
        let dir = scratch.0.join("sessions/cc-1-UID");
        let settings = tui_config("/s/ccd.sock", NONCE);
        write_session_files(&dir, &settings).unwrap();
        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(PLUGIN_FILE)), 0o600);
        assert_eq!(mode(&dir.join(TUI_CONFIG_FILE)), 0o600);
        assert_eq!(
            std::fs::read_to_string(dir.join(PLUGIN_FILE)).unwrap(),
            PLUGIN_SOURCE
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(TUI_CONFIG_FILE)).unwrap(),
            settings
        );

        let again = write_session_files(&dir, &tui_config("/s/ccd.sock", &"f".repeat(32)));
        assert!(again.is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(TUI_CONFIG_FILE)).unwrap(),
            settings
        );

        let planted = scratch.0.join("planted");
        let target = scratch.0.join("target");
        std::fs::write(&target, "theirs").unwrap();
        std::fs::create_dir(&planted).unwrap();
        std::os::unix::fs::symlink(&target, planted.join(TUI_CONFIG_FILE)).unwrap();
        let refused = write_session_files(&planted, &settings).unwrap_err();
        assert_eq!(
            refused
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "theirs");
    }

    /// **The pane runs the pinned binary with the argv unchanged**, and is given one
    /// variable. Its job is told the session directory and the binary's identity;
    /// the supervisor is told the binary and the nonce; the run registers the
    /// folder OpenCode opens, which is the folder its plugin reports.
    #[test]
    fn the_plan_runs_the_pinned_binary_with_the_argv_unchanged() {
        let folder = Path::new("/Users/ada/project");
        let dir = Path::new("/Users/ada/.codeconnect/sessions/cc-2-UID");
        let passthrough = argv(&["-m", "mock/model", "--", "run"]);
        let (plan, settings) = plan_in(&hosted(folder), dir, NONCE, &passthrough).unwrap();
        let binary = "/opt/homebrew/lib/node_modules/opencode-ai/bin/opencode.exe";
        assert_eq!(plan.argv, argv(&[binary, "-m", "mock/model", "--", "run"]));
        assert_eq!(
            plan.env,
            vec![(
                "OPENCODE_TUI_CONFIG".to_string(),
                "/Users/ada/.codeconnect/sessions/cc-2-UID/tui.json".to_string()
            )]
        );
        let os = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            plan.job_args,
            os(&[
                "--opencode-dir",
                "/Users/ada/.codeconnect/sessions/cc-2-UID",
                "--verify",
                binary,
                SHA256
            ])
        );
        assert_eq!(
            plan.supervisor_seat,
            os(&["--opencode-bin", binary, "--opencode-nonce", NONCE])
        );
        assert_eq!(plan.registered_cwd.as_deref(), Some("/Users/ada/project"));
        assert_eq!(plan.session_dir, dir);
        assert_eq!(
            settings,
            tui_config("/Users/ada/.codeconnect/ccd.sock", NONCE)
        );
    }

    /// A path that cannot travel as text is refused before a file is written.
    #[test]
    fn a_path_that_is_not_text_is_refused_before_anything_is_written() {
        use std::os::unix::ffi::OsStrExt;
        let folder = PathBuf::from(std::ffi::OsStr::from_bytes(b"/Users/ada/\xff"));
        let dir = Path::new("/nonexistent/sessions/cc-3-UID");
        assert!(plan_in(&hosted(&folder), dir, NONCE, &[]).is_err());
        assert!(!dir.exists());
    }

    /// **Every refusal lands before anything exists.** `start` classifies, resolves
    /// the binary, checks the plugin, the socket and the daemon, in that order, and
    /// only then reaches the launch that mints an identity; and a direct argv runs
    /// OpenCode before any of those checks.
    ///
    /// Read from the source, as `codex::the_preflight_refuses_before_a_uid_or_a_tmux_name_is_taken`
    /// does: the order of side effects against a live daemon and tmux is not reachable
    /// from a unit test.
    #[test]
    fn the_refusals_run_in_order_before_the_launch() {
        let source = crate::codex::production_source(include_str!("opencode.rs"));
        let start = &source[source
            .find("pub fn start(args: &[String]) -> Result<()> {")
            .expect("start exists")..];
        let start = &start[..start.find("\n}\n").expect("a closed body")];
        let order = [
            "classify(",
            ".exec()",
            "resolve_native_binary(",
            "plugin_disabled(",
            "socket_fits(",
            "refuse_unless_hostable(",
            "crate::start_agent(",
        ];
        let at: Vec<usize> = order
            .iter()
            .map(|needle| start.find(needle).unwrap_or_else(|| panic!("{needle}")))
            .collect();
        assert!(
            at.windows(2).all(|pair| pair[0] < pair[1]),
            "{order:?} must run in this order: {at:?}"
        );
        for minter in ["uid::new(", "next_session_name("] {
            assert!(
                !start.contains(minter),
                "{minter} must stay behind the checks"
            );
        }
    }
}

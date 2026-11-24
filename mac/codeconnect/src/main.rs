//! `codeconnect` — CodeConnect's command line.
//!
//! It installs and runs the daemon, pairs phones, lists and prunes the daemon's
//! sessions, serves the mail tools an agent calls (`mcp`), and installs updates.
//! Agents are started from the apps, and the daemon runs them.

mod daemon;
mod mail_trust;
mod mcp;
mod pair;
mod service;
mod sessions;
mod update_check;
mod update_install;
mod update_release;

use anyhow::{bail, Context, Result};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = args
        .split_first()
        .map(|(head, tail)| (head.as_str(), tail))
        .unwrap_or(("help", &[]));

    // A phone whose app predates the retirement of SSH still tells its owner to
    // run one of two commands here. That instruction ships inside an app already
    // installed on phones, so it cannot be corrected where it is written — it is
    // corrected here, at the moment it is followed. Consulted ahead of the match
    // so both spellings are answered by one explanation rather than by whichever
    // "unknown" refusal they happen to land on.
    if let Some(retired) = retired_command(command, rest) {
        explain_retired(retired);
    }

    // A command that prints and exits ends quietly when its reader goes away —
    // `codeconnect daemon status | head -3` — as other command-line tools do;
    // with SIGPIPE ignored, Rust panics at the next print instead. `update` and
    // `install-staged` keep it ignored, so a reader going away never ends an
    // install partway through.
    if matches!(
        command,
        "sessions"
            | "token"
            | "pair"
            | "devices"
            | "revoke"
            | "unblock"
            | "daemon"
            | "--version"
            | "version"
    ) {
        // SAFETY: called before any thread is spawned; restores the default
        // disposition of one signal.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }

    match command {
        // `mail-trust` is the verb's name before CodeConnect Messages, still taken and no longer in the help.
        "msg-act" | "mail-trust" => mail_trust::command(rest),
        "sessions" => sessions::command(rest),
        "token" => token(rest),
        "pair" => pair::pair(rest),
        "devices" => pair::devices(rest),
        "revoke" => pair::revoke(rest),
        "unblock" => pair::unblock(rest),
        "daemon" => service::command(rest),
        // Hidden: the MCP server a hosted agent messages other agents with, started
        // by the agent itself. Machinery, never typed by a human.
        "mcp" => mcp::run(rest),
        "update" => {
            no_arguments(rest, "codeconnect update").and_then(|()| update_check::run_update())
        }
        // Hidden: run by the website installer as the `codeconnect` it just
        // downloaded and verified, to install that release the way `update`
        // does. Machinery, never typed by a human.
        "install-staged" => update_check::run_install_staged(rest),
        "--version" | "version" => {
            println!(
                "{}",
                protocol::build_identity::version_line("codeconnect", env!("CARGO_PKG_VERSION"))
            );
            Ok(())
        }
        "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        // Anything else is a mistake, and a mistake exits non-zero.
        //
        // This binary was called `cc` until the collision was measured: `cc` is
        // the traditional name of the C compiler, the install prepends its
        // directory to PATH, and so `cc file.c -o out` reached *this* program,
        // printed usage, and **exited 0** — which a Makefile or a configure
        // script reads as a successful compile of nothing. The rename removed
        // the collision; exiting non-zero stays, because a command nobody
        // recognised is not a command that succeeded.
        other => {
            usage();
            anyhow::bail!("unknown command {other:?}");
        }
    }
}

fn usage() {
    eprintln!("{}", usage_text());
}

/// The banner, as text rather than as a side effect.
///
/// Split out so it can be held to the same bar as the retirement notice. This
/// is the other place a claim about SSH could plausibly be written — it is the
/// list of what this CLI does, so a line reinstating `pair --ssh`, or
/// reassuring the reader that keys were dealt with, belongs here if it belongs
/// anywhere. A banner is also the text nobody rereads, which is exactly why the
/// guard has to reach it rather than the author having to remember.
fn usage_text() -> &'static str {
    "\
codeconnect — host coding agents your iPhone can see and answer

  codeconnect sessions            list every session the daemon knows, live or ended
  codeconnect sessions prune      remove ended sessions and their history (--dry-run first)

  codeconnect msg-act on|off      let your sessions act on each other's CodeConnect messages, or not

  codeconnect update              install the latest release (the daemon restarts)
  codeconnect --version           print the installed version

  codeconnect daemon install      install the daemon and keep it running
  codeconnect daemon status       show whether the daemon is installed and running
  codeconnect daemon restart      restart the daemon
  codeconnect daemon uninstall    stop the daemon and remove it

  codeconnect pair                show a QR code that pairs a phone (single use, 5 min)
  codeconnect devices             list paired devices
  codeconnect revoke <device>     revoke a device, and every other on its Tailscale device
  codeconnect unblock <device>    let a revoked device sign in again
  codeconnect token               print the computer's permanent token (instead of a pairing code)
"
}

// ------------------------------------------------------- retired commands

/// A command this CLI no longer has, still named on a phone screen.
///
/// CodeConnect does not use SSH, so nothing here manages SSH keys. An app from
/// before SSH was retired asks its owner to authorise a key
/// with `codeconnect pair --ssh`, and to withdraw one with
/// `codeconnect ssh-revoke`. Both are answered rather than refused: the person
/// typing them is doing exactly what their screen told them to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetiredCommand {
    /// A pairing that also files a key in `~/.ssh/authorized_keys`.
    SshPairing,
    /// The withdrawal of such a key.
    SshRevocation,
}

/// The status a retired command leaves with.
///
/// Non-zero, and deliberately the same `1` an unrecognised command leaves
/// with: a retired command performed no part of what it was asked, so a script
/// that runs `codeconnect pair --ssh` and reads success would be reading a
/// pairing that never happened. One code rather than a second one invented
/// here, because "this did not work" is the whole of what a caller needs and
/// nothing consumes a finer distinction.
const RETIRED_EXIT_CODE: i32 = 1;

/// Which retired command this argv names, or `None` for one that still exists.
///
/// Pure, so the routing and the wording it selects are both pinned by tests
/// with no process to spawn.
///
/// Three spellings reach the same two answers, because all three were reachable
/// when the commands existed and any of them may be typed from memory: the
/// `ssh-revoke` command word, with or without the device it once took; `--ssh`
/// on `pair`, wherever it sits among the arguments; and `--ssh` on `revoke`,
/// which is the half-remembered form of `ssh-revoke`. That last one earns its
/// place by what it does otherwise — `revoke` reads only its first argument, so
/// `codeconnect revoke iPhone --ssh` would revoke the device's token outright
/// while its author believed they were removing a key.
///
/// The flag is looked for only under those two commands, so this dispatch never
/// swallows an argument another command takes.
fn retired_command(command: &str, args: &[String]) -> Option<RetiredCommand> {
    let ssh_flag = || args.iter().any(|arg| arg == "--ssh");
    match command {
        "ssh-revoke" => Some(RetiredCommand::SshRevocation),
        "pair" if ssh_flag() => Some(RetiredCommand::SshPairing),
        "revoke" if ssh_flag() => Some(RetiredCommand::SshRevocation),
        _ => None,
    }
}

/// What a retired command says for itself. Pure, so the facts it has to carry
/// are pinned by tests rather than by reading it.
///
/// The shared advisory grammar: a heading, the explanation, a remedy with its
/// commands isolated on their own lines, the check that settles the rest, and a
/// closing. Bold rather than the impairment colour this file paints an
/// unreachable phone with: nothing here is a fault report. The command was real
/// once, the reader typed what their screen told them to type, and what is out
/// of date is the app that told them.
///
/// **Why this describes the sweep instead of declaring it done.** `ccd`'s
/// `purge_authorized_keys` is best-effort by contract: an unresolvable `$HOME`
/// skips `~/.ssh/authorized_keys` outright, and an `~/.ssh` that cannot be
/// rewritten leaves every entry where it is — both a warning in the daemon log
/// rather than a daemon that refuses to start or a revocation that reports
/// failure. Two callers reach it now, startup and `Daemon::revoke`, and a
/// second best effort is still a best effort: what the notice may say about
/// either is that it runs. This CLI also answers on a Mac whose upgraded daemon
/// has never run, and, being pure, reads neither that file nor that log — nor
/// the count a revocation deliberately does not carry back over IPC. So the
/// notice states what the daemon does and hands over the one-line check, which
/// is worth more to a reader worried about a stale grant than a reassurance
/// nothing here can see.
fn retirement_notice(retired: RetiredCommand, style: update_check::Style) -> String {
    let heading = match retired {
        RetiredCommand::SshPairing => "codeconnect pair --ssh is retired",
        RetiredCommand::SshRevocation => "codeconnect ssh-revoke is retired",
    };

    // Both answers rest on the same facts, so they are stated once. The last of
    // them is the daemon's behaviour, not this Mac's state: the sweep is
    // allowed to fail, so where it reports failure is part of the fact.
    //
    // **Only the install half is denied, because only the install half is
    // true.** This used to read "CodeConnect neither installs an SSH key nor
    // revokes one" — immediately above two sentences describing the daemon
    // removing keys, which is what revoking one is. `ccd`'s
    // `legacy_credentials::purge_authorized_keys` runs at startup and again
    // from `Daemon::revoke`, and takes out every marker-and-key pair an earlier
    // release wrote. That the IPC reply carries `ssh_key_removed: AlwaysFalse`
    // is a fact about what a revocation reports back, not about what it does to
    // the file, and reading it as the latter is how the denial got written.
    //
    // What survives is the claim this CLI can still answer for: `pair.rs` sends
    // `allow_ssh: false`, so nothing in this project installs a key. What
    // replaces the rest is the sweep itself and the hedge it needs — a grant
    // survives silently whenever the key sits outside the single hardcoded
    // `$HOME/.ssh/authorized_keys` or on any line outside the marker-and-key
    // pair, which is what "best-effort" and the check below are carrying.
    let explanation = "CodeConnect does not use SSH, so it never installs an SSH key — and it\n\
                       does take one back. At startup the daemon removes the entries an\n\
                       earlier release wrote into ~/.ssh/authorized_keys, and it sweeps that\n\
                       file again on every revocation. It is best-effort either way, and\n\
                       warns in its log when it cannot.";
    let out_of_date = "The phone that asked for this is running an older version of the app.";

    // Where the two part company: one reader wanted to grant access, the other
    // to take it back, and each needs the command that now serves that intent.
    //
    // The revocation answer says what that command does to the file as well,
    // because this reader arrived meaning to remove a key and would otherwise
    // read `codeconnect revoke` as touching only the token. It says the sweep
    // runs and stops there: `Daemon::revoke` carries no count back, five of the
    // sweep's outcomes remove nothing, and the check below is what settles
    // which of them this Mac saw.
    let remedy = match retired {
        RetiredCommand::SshPairing => format!(
            "{out_of_date}\n\
             Update the app: it then connects over the paired link\n\
             with nothing to authorise.\n\n\
             Pairing a phone is unchanged:\n\n\
             {}",
            emphasized("codeconnect pair", style)
        ),
        RetiredCommand::SshRevocation => format!(
            "{out_of_date}\n\
             Update the app: it then connects over the paired link, and\n\
             nothing in CodeConnect has a use for the key it holds. The check\n\
             below shows what is still in that file.\n\n\
             To stop a phone reaching CodeConnect, revoke its device token:\n\n\
             {}\n\
             {}\n\n\
             Revoking also asks the daemon to sweep ~/.ssh/authorized_keys\n\
             again, on the same best effort it makes at startup.",
            emphasized("codeconnect devices", style),
            emphasized("codeconnect revoke <device>", style)
        ),
    };

    // The reader who arrived worried about a stale grant is served by a check
    // they can run over a reassurance this function is in no position to give.
    // `codeconnect:` is the tag `ccd` matches on, carried by both lines of an
    // installed entry, so the same tag is what finds one still in place.
    //
    // The pair is spelled out because the tag alone is a wider net than the
    // daemon's own rule: `legacy_credentials` removes a key line only as the
    // second half of its marker's pair, and leaves a lone key line alone
    // however its comment field reads. Advice to delete whatever matches would
    // aim the operator at somebody else's key with only their memory to stop
    // them, so what is safe to remove is described by its shape instead.
    let check = format!(
        "To see what is still in that file on this computer, search for the tag\n\
         those entries carry:\n\n\
         {}\n\n\
         Nothing printed means nothing there carries the tag; grep saying\n\
         there is no such file means the same thing. What it prints is one of\n\
         ours where two adjacent lines carry the same tag: a marker comment\n\
         beginning # codeconnect:<id>, and directly beneath it an ssh-ed25519\n\
         line whose last field is that same tag. Delete that pair by hand.\n\
         A tagged key line with no such marker directly above it is one this\n\
         daemon leaves alone, because nothing in the file identifies whose it\n\
         is — removing it is your call rather than its.",
        emphasized("grep codeconnect: ~/.ssh/authorized_keys", style)
    );

    // The reader's part is observable from here — the spelling they typed is
    // one only an older app hands out. This Mac's part is not, so it is the
    // check above that speaks to it and not this line.
    let closing =
        "Nothing you typed was wrong \u{2014} the app on the phone is what is out of date.";

    let text = format!(
        "{}\n{explanation}\n\n{remedy}\n\n{check}\n\n{closing}",
        emphasized(heading, style)
    );
    match style {
        update_check::Style::Ascii => asciify(&text),
        _ => text,
    }
}

/// Print `retired`'s explanation and leave.
///
/// **Why this leaves through `process::exit` rather than through the `Result`
/// every other command returns.** anyhow prints a failure as `Error: {msg}`,
/// which is the right frame for a mistake — and this is not one. The reader
/// typed what their phone told them to type, and a paragraph explaining that,
/// prefixed `Error:`, reads as a crash they caused. The status must still be
/// non-zero, so returning `Ok(())` is not available either. Writing the notice
/// and choosing the code directly is what satisfies both.
///
/// Safe at this point specifically: nothing has been opened, spawned or
/// written, so the destructors `exit` skips have nothing to release, and
/// stderr is unbuffered — the notice is out before the call.
fn explain_retired(retired: RetiredCommand) -> ! {
    use std::io::IsTerminal;
    // stderr, like every other advisory here: this is a refusal rather than the
    // product of a command, and a reader who redirected stdout expecting a QR
    // still sees why they did not get one.
    let style = update_check::style_for_stream(std::io::stderr().is_terminal());
    eprintln!("{}", retirement_notice(retired, style));
    std::process::exit(RETIRED_EXIT_CODE);
}

/// Bold when styling is on; bare otherwise. For an advisory's heading, and for
/// an isolated command or URL line — the thing the reader is meant to act on.
fn emphasized(line: &str, style: update_check::Style) -> String {
    match style {
        update_check::Style::Styled => format!("\u{1b}[1m{line}\u{1b}[0m"),
        update_check::Style::PlainUnicode | update_check::Style::Ascii => line.to_string(),
    }
}

/// The ASCII degradation for logs, pipes and `TERM=dumb`: typography maps
/// to its plain equivalents, so no escape or multi-byte character survives.
fn asciify(text: &str) -> String {
    text.replace('\u{2014}', "--")
        .replace('\u{2192}', "->")
        .replace('\u{b7}', ":")
        .replace('\u{2026}', "...")
}

/// Refuse what follows a command that takes nothing more. An argument it does not
/// know — a `--help` it does not have, a `--dry-run` it does not offer — is never
/// read as a go-ahead: the command it rides on may install, update, revoke, or
/// print a credential.
pub(crate) fn no_arguments(args: &[String], usage: &str) -> Result<()> {
    if let Some(first) = args.first() {
        bail!("unexpected argument {first:?}; usage: {usage}");
    }
    Ok(())
}

fn token(args: &[String]) -> Result<()> {
    no_arguments(args, "codeconnect token")?;
    let path = protocol::token_path();
    let token = match std::fs::read_to_string(&path) {
        Ok(token) => token,
        // The daemon writes it the first time it starts; until then there is
        // nothing to read and one thing to do.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => bail!(
            "there is no token yet: the daemon writes it when it first starts. Start it with \
             `codeconnect daemon install`"
        ),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    println!("{}", token.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An argv as the dispatch receives it, so a test reads like the command
    /// a person typed.
    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    /// The pairing flag an app from before the retirement still names. It has
    /// to be recognised wherever it sits in the arguments, because the reader
    /// is retyping a command from a phone screen and may add to it.
    #[test]
    fn the_retired_ssh_pairing_flag_explains_itself_instead_of_failing_as_an_unknown_option() {
        for words in [
            argv(&["--ssh"]),
            argv(&["--ssh", "--qr"]),
            argv(&["--qr", "--ssh"]),
        ] {
            assert_eq!(
                retired_command("pair", &words),
                Some(RetiredCommand::SshPairing),
                "{words:?} must reach the explanation, not `unknown option`"
            );
        }
    }

    /// The revocation an app from before the retirement still names — bare, as
    /// that app spells it, and with the device the usage block spelled it with.
    ///
    /// `revoke <device> --ssh` is the same intent typed from memory, and it is
    /// the spelling with teeth: `revoke` reads only its first argument, so
    /// without this it revokes the device's token outright while its author
    /// believes they are removing a key.
    #[test]
    fn the_retired_ssh_revocation_explains_itself_instead_of_failing_as_an_unknown_command() {
        for (command, words) in [
            ("ssh-revoke", argv(&[])),
            ("ssh-revoke", argv(&["iPhone"])),
            ("revoke", argv(&["iPhone", "--ssh"])),
            ("revoke", argv(&["--ssh", "iPhone"])),
        ] {
            assert_eq!(
                retired_command(command, &words),
                Some(RetiredCommand::SshRevocation),
                "`codeconnect {command} {words:?}` must reach the explanation"
            );
        }
    }

    /// **The help names CodeConnect Messages' verb**, `msg-act`, and not the one before it, which is still taken.
    #[test]
    fn the_help_names_msg_act_and_not_mail_trust() {
        assert!(
            usage_text().contains("codeconnect msg-act on|off"),
            "{}",
            usage_text()
        );
        assert!(!usage_text().contains("mail-trust"), "{}", usage_text());
    }

    /// The diversion is surgical: every command that still exists runs.
    #[test]
    fn the_commands_that_still_exist_are_never_diverted() {
        for (command, words) in [
            ("pair", argv(&[])),
            ("revoke", argv(&["iPhone"])),
            ("devices", argv(&[])),
            ("token", argv(&[])),
            ("update", argv(&[])),
            ("help", argv(&[])),
        ] {
            assert_eq!(
                retired_command(command, &words),
                None,
                "`codeconnect {command} {words:?}` still has work to do"
            );
        }
    }

    /// The rule this diversion must not erode: a command nobody recognises is
    /// not a command that succeeded. Nothing here is retired, so all of it
    /// falls through to the refusals that were already there — including the
    /// `cc file.c` shape that is the reason those refusals exit non-zero.
    #[test]
    fn a_command_nobody_recognises_is_still_nobodys_command() {
        for (command, words) in [
            ("frobnicate", argv(&[])),
            ("ssh", argv(&[])),
            ("ssh-install", argv(&[])),
            ("sshrevoke", argv(&[])),
            ("pair", argv(&["--sshh"])),
            ("pair", argv(&["--nope"])),
            ("file.c", argv(&["-o", "out"])),
        ] {
            assert_eq!(
                retired_command(command, &words),
                None,
                "`codeconnect {command} {words:?}` is not retired, it is unknown"
            );
        }

        // And the refusal it falls through to is intact: a mistyped option on
        // `pair` is still an error naming the usage, never a QR code.
        let err = pair::pair(&argv(&["--nope"])).expect_err("an unknown option must not pair");
        assert!(
            err.to_string().contains("unexpected argument"),
            "the pre-existing refusal has to survive the diversion: {err}"
        );
    }

    /// Every fact the reader needs, in both answers: that SSH is gone, what the
    /// daemon does to `authorized_keys` — at startup, and again on every
    /// revocation — and where it says so when it cannot, the one-line check that
    /// settles what is actually left there, that the app is what is out of date —
    /// and the command that now serves the intent they arrived with.
    #[test]
    fn the_retirement_notice_names_the_facts_that_matter() {
        use update_check::Style;
        let pairing = retirement_notice(RetiredCommand::SshPairing, Style::Ascii);
        let revocation = retirement_notice(RetiredCommand::SshRevocation, Style::Ascii);

        for notice in [&pairing, &revocation] {
            assert!(notice.contains("does not use SSH"), "{notice}");
            assert!(
                notice.contains("At startup the daemon removes"),
                "the sweep is what the daemon does, and it must be stated: {notice}"
            );
            assert!(notice.contains("~/.ssh/authorized_keys"), "{notice}");
            assert!(
                notice.contains("again on every revocation"),
                "the startup sweep is not the only one, and a reader told only \
                 about that one has no reason to run `codeconnect revoke` again \
                 as the retry for a sweep that failed: {notice}"
            );
            assert!(
                notice.contains("best-effort"),
                "a sweep the daemon is allowed to fail at must be described as \
                 one, or `the daemon removes the entries` reads as a \
                 guarantee: {notice}"
            );
            assert!(
                notice.contains("warns in its log"),
                "a sweep that is allowed to fail has to name where it says so, \
                 or the reader cannot tell a quiet success from a quiet one: {notice}"
            );
            assert!(
                notice.contains("\ngrep codeconnect: ~/.ssh/authorized_keys\n"),
                "a reader worried about a stale grant is owed a check they can \
                 run, isolated on its own line: {notice}"
            );
            // A check whose hits the reader cannot classify is a check that
            // aims them at somebody else's key: `legacy_credentials` keeps a
            // lone key line however its comment field reads, so the notice
            // has to describe the pair rather than the tag alone.
            assert!(
                notice.contains("two adjacent lines carry the same tag"),
                "the reader needs the shape that is safe to remove, not just \
                 the tag that finds candidates: {notice}"
            );
            // The hit the reader cannot classify from the file: a tagged key
            // with no marker over it. `legacy_credentials` declines to touch it
            // precisely because nothing there says whose it is, so the notice
            // must hand that uncertainty over rather than resolve it. Calling
            // it somebody else's is the one answer the file cannot support, and
            // the answer that files a live grant as a colleague's.
            assert!(
                notice.contains("nothing in the file identifies whose it"),
                "an unlabelled grant has to be described as unidentified: {notice}"
            );
            assert!(
                !notice.contains("somebody else"),
                "the notice must attribute an unlabelled key to nobody: {notice}"
            );
            assert!(
                notice.contains("older version of the app"),
                "the reader has to learn why their phone asked: {notice}"
            );
            assert!(notice.contains("Update the app"), "{notice}");
        }

        // Each names the command that serves the intent it was reached with,
        // and neither offers the other's.
        assert!(pairing.starts_with("codeconnect pair --ssh is retired\n"));
        assert!(pairing.contains("\ncodeconnect pair\n"));
        assert!(
            !pairing.contains("codeconnect revoke"),
            "a reader who came to grant access is not sent to revoke: {pairing}"
        );

        assert!(revocation.starts_with("codeconnect ssh-revoke is retired\n"));
        assert!(revocation.contains("\ncodeconnect revoke <device>\n"));
        assert!(revocation.contains("\ncodeconnect devices\n"));
        // And the promise attached to that command is the one it can keep.
        // `ccd` does sweep `~/.ssh/authorized_keys` on a revocation now, but it
        // answers with `ssh_key_removed: AlwaysFalse` and carries no count
        // back, because five of that sweep's outcomes remove nothing. So what
        // the token buys back is CodeConnect's own grant, and what happened to
        // the file is a matter for the daemon log and the check.
        assert!(
            revocation.contains("To stop a phone reaching CodeConnect, revoke its device token"),
            "revoking a token takes back what CodeConnect granted, which is \
             less than everything: {revocation}"
        );
    }

    /// The notice obeys the same rendering contract as every other advisory
    /// here: pure ASCII with no escapes when styling is off, inside 80 columns
    /// so it survives a narrow terminal, and bold on the heading and on each
    /// command the reader is meant to act on when styling is on.
    #[test]
    fn the_retirement_notice_renders_within_the_advisory_grammar() {
        use update_check::Style;
        for retired in [RetiredCommand::SshPairing, RetiredCommand::SshRevocation] {
            let plain = retirement_notice(retired, Style::Ascii);
            assert!(plain.is_ascii(), "non-ascii survived: {plain}");
            assert!(!plain.contains('\u{1b}'), "an escape survived: {plain}");
            for line in plain.lines() {
                assert!(line.len() < 80, "over 80 cols: {line}");
                assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
            }

            let styled = retirement_notice(retired, Style::Styled);
            assert!(styled.starts_with("\u{1b}[1m"), "{styled}");
            // Every command line is emphasised, and every emphasis is closed —
            // an unreset attribute bleeds across the rest of the terminal.
            assert_eq!(
                styled.matches("\u{1b}[1m").count(),
                styled.matches("\u{1b}[0m").count(),
                "{styled}"
            );
        }
    }

    // --------------------------------------------- adversarial: the diversion
    //
    // Written against the diversion rather than with it: each of these is an
    // attempt to make it swallow something it has no business swallowing, or
    // to make it let go of something it has to catch.

    /// Every command word `main`'s dispatch still answers. Enumerated so the
    /// scoping test below fails the moment an arm is added without a decision
    /// about whether the flag scan reaches it.
    const LIVE_COMMANDS: &[&str] = &[
        "sessions",
        "token",
        "pair",
        "devices",
        "revoke",
        "daemon",
        "update",
        "install-staged",
        "--version",
        "version",
        "help",
        "--help",
        "-h",
    ];

    /// The scan reaches exactly two command words and no others.
    ///
    /// Widening the scan by a single arm would let this dispatch eat an argument
    /// another command takes — so the scoping is asserted against the whole
    /// dispatch table, not against a sample of it.
    #[test]
    fn only_pair_and_revoke_are_scanned_for_the_flag() {
        for command in LIVE_COMMANDS {
            assert_eq!(
                retired_command(command, &argv(&["--ssh"])).is_some(),
                matches!(*command, "pair" | "revoke"),
                "`codeconnect {command} --ssh` is scoped wrong"
            );
        }
    }

    /// The flag is matched whole and exactly. Everything that merely resembles
    /// it is a mistyped option, and has to keep falling through to the refusal
    /// that was already there rather than collecting an explanation it has not
    /// earned.
    #[test]
    fn only_the_exact_flag_and_the_exact_command_word_divert() {
        for near in [
            "--sshh",
            "-ssh",
            "ssh",
            "--SSH",
            "--Ssh",
            "--ssh=1",
            "--ssh=true",
            "--no-ssh",
            "---ssh",
            "--ssh ",
            " --ssh",
            // A Cyrillic dze where the first `s` belongs: identical on screen,
            // a different string, and not this command.
            "--\u{0455}sh",
        ] {
            assert_eq!(
                retired_command("pair", &argv(&[near])),
                None,
                "`codeconnect pair {near}` is a mistyped option, not a retired one"
            );
            assert_eq!(
                retired_command("revoke", &argv(&["iPhone", near])),
                None,
                "`codeconnect revoke iPhone {near}` is a mistyped option"
            );
        }

        for near in [
            "sshrevoke",
            "ssh_revoke",
            "ssh-revoke-all",
            "SSH-REVOKE",
            "Ssh-Revoke",
            "ssh-revoke ",
            "ssh",
            "ssh-install",
            "ssh-pair",
            "revoke-ssh",
        ] {
            assert_eq!(
                retired_command(near, &argv(&[])),
                None,
                "`codeconnect {near}` is unknown, not retired"
            );
        }
    }

    /// The flag on its own is not a command. `codeconnect --ssh` names nothing,
    /// and must land on the refusal every unknown command word lands on —
    /// which is the rule that exits non-zero.
    #[test]
    fn the_flag_alone_is_not_a_retired_command() {
        assert_eq!(retired_command("--ssh", &argv(&[])), None);
        assert_eq!(retired_command("--ssh", &argv(&["pair"])), None);
        // What `main` substitutes for an empty argv.
        assert_eq!(retired_command("help", &argv(&[])), None);
    }

    /// Position is irrelevant under the two commands that are scanned: the
    /// reader is retyping from a phone screen and may put the flag anywhere,
    /// and may add to the line. `ssh-revoke` is the command word itself, so it
    /// diverts with whatever follows — including the device it once took.
    #[test]
    fn the_flag_is_found_at_every_position_under_the_scanned_commands() {
        for slot in 0..5 {
            let mut words: Vec<String> = (0..5).map(|i| format!("arg{i}")).collect();
            words[slot] = "--ssh".to_string();
            assert_eq!(
                retired_command("pair", &words),
                Some(RetiredCommand::SshPairing),
                "position {slot}: {words:?}"
            );
            assert_eq!(
                retired_command("revoke", &words),
                Some(RetiredCommand::SshRevocation),
                "position {slot}: {words:?}"
            );
        }

        for words in [
            argv(&[]),
            argv(&["iPhone"]),
            argv(&["--help"]),
            argv(&["iPhone", "--ssh"]),
        ] {
            assert_eq!(
                retired_command("ssh-revoke", &words),
                Some(RetiredCommand::SshRevocation),
                "`codeconnect ssh-revoke {words:?}`"
            );
        }
    }

    /// The judgement call, pinned so a later change to it is deliberate:
    /// `codeconnect revoke --ssh` with no device is explained rather than
    /// looked up. Without the diversion `--ssh` *is* the device argument —
    /// `revoke` reads only the first — and the reader gets `no device matches
    /// "--ssh"`, which explains nothing about why their phone asked. No device
    /// is ever named `--ssh`, so nothing legitimate is taken from anyone.
    #[test]
    fn a_deviceless_ssh_revoke_is_explained_rather_than_looked_up() {
        assert_eq!(
            retired_command("revoke", &argv(&["--ssh"])),
            Some(RetiredCommand::SshRevocation)
        );
    }

    /// The status a retired command leaves with is the one an unperformed
    /// command has to leave with: non-zero, and the same `1` the unknown-command
    /// refusal gets from anyhow's `Termination`. A script that runs
    /// `codeconnect pair --ssh` and reads success would be reading a pairing
    /// that never happened.
    #[test]
    fn a_retired_command_never_reports_success() {
        assert_ne!(RETIRED_EXIT_CODE, 0, "nothing was performed");
        assert_eq!(RETIRED_EXIT_CODE, 1);
    }

    /// The third style, which the shipped tests left unpinned. `NO_COLOR` on a
    /// real terminal disables SGR and says nothing about characters, so the
    /// notice must carry no escape at all while keeping its typography.
    #[test]
    fn the_notice_under_no_color_drops_every_escape_and_keeps_its_typography() {
        for retired in [RetiredCommand::SshPairing, RetiredCommand::SshRevocation] {
            let plain = retirement_notice(retired, update_check::Style::PlainUnicode);
            assert!(
                !plain.contains('\u{1b}'),
                "an escape survived NO_COLOR: {plain}"
            );
            assert!(
                plain.contains('\u{2014}'),
                "NO_COLOR disables colour, not characters: {plain}"
            );
            for line in plain.lines() {
                assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
                assert!(line.chars().count() < 80, "over 80 cols: {line}");
            }
        }
    }

    /// **The invariant the notice tests exist to hold: the notice may say the
    /// sweep runs, and may never say the file is now clean.**
    ///
    /// `ccd`'s `legacy_credentials::purge_authorized_keys` is best-effort by
    /// contract. It removes nothing and warns when there is no absolute `$HOME`,
    /// when the file cannot be read, when the replacement cannot be written, when
    /// the file changed underneath the sweep, and when a tagged line is not the
    /// marker-and-key pair earlier releases wrote.
    /// Two callers reach it — startup and `Daemon::revoke` — and a second best
    /// effort is still a best effort. This CLI also answers on a Mac whose
    /// upgraded daemon has never run and, being pure, reads neither that file nor
    /// that log. So the notice states what the daemon *does* and hands over the
    /// `grep`; a reader who is told the removal already happened has been given a
    /// reassurance nothing in this tree can support, and the one they are
    /// likeliest to act on.
    ///
    /// This used to be defended by a few hundred lines that parsed the notice —
    /// and `mac/README.md` — into sentences and swept them for a blocklist of
    /// words. It did not catch the false "CodeConnect neither installs an SSH key
    /// nor revokes one"; it whitelisted that exact sentence as a fix. What
    /// replaces it is a golden snapshot per notice, so any rewording lands in a
    /// diff a human has to look at, plus literal assertions for the facts a
    /// reader is owed. Neither can be argued with, and neither certifies prose it
    /// never understood.
    #[test]
    fn the_notice_never_reports_the_removal_as_done() {
        // Each of these was, or is one keystroke from, a sentence claiming an
        // outcome nothing here observed. Literal and short on purpose: this is
        // an assertion about wording that shipped, not a theory of English.
        const NEVER: &[&str] = &[
            "already",
            "no longer",
            "nothing left",
            "is clean",
            "has been removed",
            "was removed",
            "were removed",
            "removed it",
            "nothing to revoke",
            "no key",
            "cannot log in",
            "will refuse",
        ];
        for retired in [RetiredCommand::SshPairing, RetiredCommand::SshRevocation] {
            for style in [
                update_check::Style::Ascii,
                update_check::Style::PlainUnicode,
                update_check::Style::Styled,
            ] {
                let notice = retirement_notice(retired, style).to_lowercase();
                for claim in NEVER {
                    assert!(
                        !notice.contains(claim),
                        "{claim:?} reports an outcome nothing here observed; the \
                         reader gets the check to run instead: {notice}"
                    );
                }
            }
        }
    }

    /// **The golden each notice is held to, byte for byte.**
    ///
    /// A snapshot rather than a property. Any edit to this wording — a
    /// tightening, a fact added, a hedge dropped — lands in this test's diff
    /// beside the notice's, where a human has to read both and decide the new
    /// sentence is true. That is the whole mechanism, and it is the one thing
    /// the parser this replaced could not be: prose is checked by a reader, and
    /// what a test can do is guarantee a reader is asked.
    ///
    /// `PlainUnicode` because it is the style with neither escapes nor
    /// transliteration, so what is pinned is the wording itself. The other two
    /// styles are pinned as renderings of it by
    /// [`the_notice_under_no_color_drops_every_escape_and_keeps_its_typography`]
    /// and [`the_notice_only_ever_offers_commands_that_still_exist`].
    #[test]
    fn the_pairing_notice_reads_exactly_this() {
        assert_eq!(
            retirement_notice(
                RetiredCommand::SshPairing,
                update_check::Style::PlainUnicode
            ),
            concat!(
                "codeconnect pair --ssh is retired\n",
                "CodeConnect does not use SSH, so it never installs an SSH key — and it\n",
                "does take one back. At startup the daemon removes the entries an\n",
                "earlier release wrote into ~/.ssh/authorized_keys, and it sweeps that\n",
                "file again on every revocation. It is best-effort either way, and\n",
                "warns in its log when it cannot.\n",
                "\n",
                "The phone that asked for this is running an older version of the app.\n",
                "Update the app: it then connects over the paired link\n",
                "with nothing to authorise.\n",
                "\n",
                "Pairing a phone is unchanged:\n",
                "\n",
                "codeconnect pair\n",
                "\n",
                "To see what is still in that file on this computer, search for the tag\n",
                "those entries carry:\n",
                "\n",
                "grep codeconnect: ~/.ssh/authorized_keys\n",
                "\n",
                "Nothing printed means nothing there carries the tag; grep saying\n",
                "there is no such file means the same thing. What it prints is one of\n",
                "ours where two adjacent lines carry the same tag: a marker comment\n",
                "beginning # codeconnect:<id>, and directly beneath it an ssh-ed25519\n",
                "line whose last field is that same tag. Delete that pair by hand.\n",
                "A tagged key line with no such marker directly above it is one this\n",
                "daemon leaves alone, because nothing in the file identifies whose it\n",
                "is — removing it is your call rather than its.\n",
                "\n",
                "Nothing you typed was wrong — the app on the phone is what is out of date.",
            )
        );
    }

    /// The golden for the other answer. See
    /// [`the_pairing_notice_reads_exactly_this`] for why this is a snapshot.
    #[test]
    fn the_revocation_notice_reads_exactly_this() {
        assert_eq!(
            retirement_notice(
                RetiredCommand::SshRevocation,
                update_check::Style::PlainUnicode
            ),
            concat!(
                "codeconnect ssh-revoke is retired\n",
                "CodeConnect does not use SSH, so it never installs an SSH key — and it\n",
                "does take one back. At startup the daemon removes the entries an\n",
                "earlier release wrote into ~/.ssh/authorized_keys, and it sweeps that\n",
                "file again on every revocation. It is best-effort either way, and\n",
                "warns in its log when it cannot.\n",
                "\n",
                "The phone that asked for this is running an older version of the app.\n",
                "Update the app: it then connects over the paired link, and\n",
                "nothing in CodeConnect has a use for the key it holds. The check\n",
                "below shows what is still in that file.\n",
                "\n",
                "To stop a phone reaching CodeConnect, revoke its device token:\n",
                "\n",
                "codeconnect devices\n",
                "codeconnect revoke <device>\n",
                "\n",
                "Revoking also asks the daemon to sweep ~/.ssh/authorized_keys\n",
                "again, on the same best effort it makes at startup.\n",
                "\n",
                "To see what is still in that file on this computer, search for the tag\n",
                "those entries carry:\n",
                "\n",
                "grep codeconnect: ~/.ssh/authorized_keys\n",
                "\n",
                "Nothing printed means nothing there carries the tag; grep saying\n",
                "there is no such file means the same thing. What it prints is one of\n",
                "ours where two adjacent lines carry the same tag: a marker comment\n",
                "beginning # codeconnect:<id>, and directly beneath it an ssh-ed25519\n",
                "line whose last field is that same tag. Delete that pair by hand.\n",
                "A tagged key line with no such marker directly above it is one this\n",
                "daemon leaves alone, because nothing in the file identifies whose it\n",
                "is — removing it is your call rather than its.\n",
                "\n",
                "Nothing you typed was wrong — the app on the phone is what is out of date.",
            )
        );
    }

    /// SSH is retired, so the list of what this CLI does must not offer it.
    ///
    /// A banner is the text nobody rereads, which is how a line reinstating
    /// `pair --ssh` would ship green.
    #[test]
    fn the_usage_banner_offers_no_ssh() {
        assert!(
            !usage_text().to_lowercase().contains("ssh"),
            "SSH is retired, so the list of what this CLI does must not offer \
             it: {}",
            usage_text()
        );
    }

    /// A notice that sent its reader to a command this binary does not have
    /// would replace one dead end with another. Every `codeconnect …` line the
    /// remedy isolates has to name a live dispatch arm, and none of them may be
    /// a retired spelling — the heading is the only place a retired spelling
    /// belongs, because there it is the thing being explained.
    #[test]
    fn the_notice_only_ever_offers_commands_that_still_exist() {
        for retired in [RetiredCommand::SshPairing, RetiredCommand::SshRevocation] {
            let notice = retirement_notice(retired, update_check::Style::Ascii);
            let mut offered = 0;
            for line in notice.lines().skip(1) {
                let Some(rest) = line.strip_prefix("codeconnect ") else {
                    continue;
                };
                offered += 1;
                let word = rest.split_whitespace().next().unwrap_or_default();
                assert!(
                    LIVE_COMMANDS.contains(&word),
                    "the notice offers `codeconnect {word}`, which this binary does not have"
                );
                assert!(
                    !line.contains("--ssh") && !line.contains("ssh-revoke"),
                    "a retired spelling must never be the remedy: {line}"
                );
            }
            assert!(
                offered > 0,
                "a remedy with no command to run is not a remedy: {notice}"
            );
        }
    }
}

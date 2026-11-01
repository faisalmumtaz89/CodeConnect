//! **The mail switch: whether agents act on messages from the person's other agents.**
//!
//! One answer per computer, kept in CodeConnect's own folder and nowhere else. On, the
//! `codeconnect` tool's `read_messages` tells the agent to handle each message as its
//! sender asks, as a teammate's request within the session's own permissions; off,
//! it tells the agent to show the messages to the person and not act on them.
//! Never asked, it is off. CodeConnect asks once, at the first `codeconnect claude` or
//! `codeconnect codex` that starts a session in a terminal, and
//! `codeconnect mail-trust on|off` changes the answer later.
//!
//! It is guidance to the agent, written into what it reads, not enforcement: the
//! agent's own permission mode is what limits what it can do.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The answer's file, in CodeConnect's folder `root`.
fn answer_path(root: &Path) -> PathBuf {
    root.join(protocol::ipc::MAIL_TRUST_FILE)
}

/// **Ask once per computer**, in this terminal, before a session starts. Nothing is
/// asked when an answer is kept. Whatever goes wrong here is said and the session
/// starts all the same.
pub fn ask_once() {
    let root = protocol::root_dir();
    if answer_path(&root).exists() {
        return;
    }
    eprint!(
        "\nLet your agents act on messages from your other agents? With yes, an agent handles \
         each message as a teammate's request, within its own permissions; with no, it shows \
         them to you and does not act on them. CodeConnect asks only this once; \
         `codeconnect mail-trust on` or `off` changes it. [y/N] "
    );
    let mut reply = String::new();
    if std::io::stdin().lock().read_line(&mut reply).is_err() {
        return;
    }
    let on = matches!(reply.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if let Err(err) = keep(on, &root) {
        eprintln!("codeconnect: the answer was not kept: {err:#}");
    }
}

/// `codeconnect mail-trust on|off`.
pub fn command(args: &[String]) -> Result<()> {
    let on = match args {
        [word] if word == "on" => true,
        [word] if word == "off" => false,
        _ => bail!("usage: codeconnect mail-trust on|off"),
    };
    keep(on, &protocol::root_dir())?;
    println!(
        "{}",
        if on {
            "Your agents act on messages from your other agents, within their own permissions."
        } else {
            "Your agents show messages from your other agents to you, and do not act on them."
        }
    );
    Ok(())
}

/// Keep the answer in `root`.
fn keep(on: bool, root: &Path) -> Result<()> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let path = answer_path(root);
    std::fs::write(&path, if on { "on\n" } else { "off\n" })
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Only an answer of on says agents act**; off, and no answer at all, say they
    /// do not.
    #[test]
    fn agents_act_only_when_the_answer_is_on() {
        let root = std::env::temp_dir().join(format!(
            "cc-mail-trust-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let acts = protocol::ipc::mail_acts;
        assert!(!acts(&root), "never asked");
        keep(true, &root).unwrap();
        assert!(acts(&root));
        keep(false, &root).unwrap();
        assert!(!acts(&root));
        let _ = std::fs::remove_dir_all(&root);
    }
}

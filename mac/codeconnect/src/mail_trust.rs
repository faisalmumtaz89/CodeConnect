//! **The act-on-messages switch: whether sessions act on CodeConnect messages from the person's other sessions.**
//!
//! One answer per computer, kept in CodeConnect's own folder and nowhere else. On, the
//! `codeconnect` tool's `msg_read` tells the session to handle each message as its
//! sender asks, as a teammate's request within the session's own permissions; off,
//! it tells the agent to show the messages to the person and not act on them.
//! Never set, sessions act; `codeconnect msg-act on|off` sets it (`mail-trust`, the verb's name before CodeConnect
//! Messages, is still taken).
//!
//! It is guidance to the agent, written into what it reads, not enforcement: the
//! agent's own permission mode is what limits what it can do.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The answer's file, in CodeConnect's folder `root`.
fn answer_path(root: &Path) -> PathBuf {
    root.join(protocol::ipc::MAIL_TRUST_FILE)
}

/// `codeconnect msg-act on|off`, or `mail-trust`, its name before.
pub fn command(args: &[String]) -> Result<()> {
    let on = match args {
        [word] if word == "on" => true,
        [word] if word == "off" => false,
        _ => bail!("usage: codeconnect msg-act on|off"),
    };
    keep(on, &protocol::root_dir())?;
    println!(
        "{}",
        if on {
            "Your sessions act on CodeConnect messages from your other sessions, within their own permissions."
        } else {
            "Your sessions show CodeConnect messages from your other sessions to you, and do not act on them."
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

    /// **Sessions act unless the answer is off**: no answer at all, and on, say they act; off says they do not.
    #[test]
    fn sessions_act_unless_the_answer_is_off() {
        let root = std::env::temp_dir().join(format!(
            "cc-mail-trust-{}-{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        let acts = protocol::ipc::mail_acts;
        assert!(acts(&root), "never asked");
        keep(true, &root).unwrap();
        assert!(acts(&root));
        keep(false, &root).unwrap();
        assert!(!acts(&root));
        let _ = std::fs::remove_dir_all(&root);
    }
}

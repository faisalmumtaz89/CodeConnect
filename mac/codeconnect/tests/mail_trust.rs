//! **The mail switch writes only CodeConnect's own folder.** Claude Code's and Codex's
//! instruction folders are never touched, however the answer is changed. The real
//! `codeconnect`, in a home of its own under the temp folder.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Every file under `dir`, with its contents.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.push((path, bytes));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn the_mail_switch_writes_only_codeconnects_own_folder() {
    let home = std::env::temp_dir().join(format!(
        "cc-mail-trust-{}",
        protocol::uid::new().expect("a unique test directory")
    ));
    let (claude, codex, own) = (home.join(".claude"), home.join(".codex"), home.join("cc"));
    for dir in [&claude, &codex] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(claude.join("CLAUDE.md"), "# Mine\n").unwrap();
    let before = (files(&claude), files(&codex));

    for (word, answer) in [("on", "on\n"), ("off", "off\n"), ("on", "on\n")] {
        let run = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .args(["mail-trust", word])
            .env("HOME", &home)
            .env("CODECONNECT_HOME", &own)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", &codex)
            .output()
            .unwrap();
        assert!(run.status.success(), "{word}: {run:?}");
        assert_eq!(
            std::fs::read_to_string(own.join("mail-trust")).unwrap(),
            answer
        );
        assert_eq!(
            (files(&claude), files(&codex)),
            before,
            "{word}: an agent's folder changed"
        );
    }
    let refused = Command::new(env!("CARGO_BIN_EXE_codeconnect"))
        .args(["mail-trust", "maybe"])
        .env("HOME", &home)
        .env("CODECONNECT_HOME", &own)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    std::fs::remove_dir_all(&home).unwrap();
}

/// **`codeconnect msg-act on|off` sets the switch, and the verb before it still does.** Each answer is said in
/// CodeConnect Messages' words; a word that is neither is refused with the verb's usage.
#[test]
fn msg_act_sets_the_switch_and_mail_trust_still_does() {
    let home = std::env::temp_dir().join(format!(
        "cc-msg-act-{}",
        protocol::uid::new().expect("a unique test directory")
    ));
    let own = home.join("cc");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_codeconnect"))
            .args(args)
            .env("HOME", &home)
            .env("CODECONNECT_HOME", &own)
            .output()
            .unwrap()
    };
    for (args, answer, said) in [
        (
            ["msg-act", "on"],
            "on\n",
            "Your sessions act on CodeConnect messages from your other sessions, within their own permissions.\n",
        ),
        (
            ["msg-act", "off"],
            "off\n",
            "Your sessions show CodeConnect messages from your other sessions to you, and do not act on them.\n",
        ),
        (
            ["mail-trust", "on"],
            "on\n",
            "Your sessions act on CodeConnect messages from your other sessions, within their own permissions.\n",
        ),
    ] {
        let out = run(&args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert_eq!(std::fs::read_to_string(own.join("mail-trust")).unwrap(), answer, "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), said, "{args:?}");
    }
    let refused = run(&["msg-act", "maybe"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("usage: codeconnect msg-act on|off"),
        "{refused:?}"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

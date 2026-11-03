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

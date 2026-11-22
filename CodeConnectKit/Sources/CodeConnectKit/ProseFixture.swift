import Foundation

#if DEBUG
    /// Hand-written agent replies for renders of how the apps set an agent's text (`-CC_FIXTURE_PROSE <name>`):
    /// `review` has every block an agent writes in one message; `plain`, `wide`, `code` and `nested` each push one block
    /// to its limit. The content is invented.
    enum ProseFixture {
        static let prompt = "Review the installer change and tell me what is left before it ships."

        static let reply = """
            ## Installer review

            I read `setup.sh`, `lib/paths.sh` and the two new tests, ran the suite twice, and tried the script on a clean \
            account. Most of it holds; three items are still open.

            | Item | Status | Evidence / Notes |
            | --- | --- | --- |
            | Syntax | Done | `bash -n setup.sh` exits 0; `shellcheck` reports nothing at the default level. |
            | Clean install | Done | A fresh account ran it start to finish in 41 s; `~/.local/bin/tool` is on `PATH` afterwards. |
            | Re-run | Done | A second run changes nothing: the checksum of every file it writes is the same, and it prints "already installed". |
            | Upgrade from 1.4 | Blocked | **(major)** The old `config.toml` keeps its `cache_dir` key, which 2.0 no longer reads, so the cache moves to the default and the old one stays on disk. |
            | Uninstall | In progress | `setup.sh --remove` deletes the binary and the launch agent; the cache is left on purpose and the message says so. |
            | Paths with spaces | Done | Tested with `HOME="/tmp/a b"`; every expansion is quoted. |
            | Offline | Discarded | It needs the network only for the download, which already fails with a clear message. |
            | Permissions | Done | Files are 0644, the binary 0755; nothing is written outside the home folder. |
            | Logging | In progress | Errors go to stderr, but the progress lines do too, so `2>/dev/null` hides them. **(minor)** |
            | Docs | Done | The README's install section matches the flags in `--help`. |

            The one that matters is the upgrade. Anyone on 1.4 with a custom cache folder loses the setting silently, and \
            because the old folder is never removed they also keep paying for its disk space without knowing why. The fix \
            is small: read `cache_dir` once during the upgrade, write it under the new key, and say in one line that it moved.

            Everything else is either done or a choice we can defend. I would ship after the upgrade fix and the logging \
            change, and leave the offline case as it is, since the failure it produces is already the right one.

            What I changed while reviewing:

            - `lib/paths.sh`
              - quoted the two expansions in `join_path`
              - removed the unused `old_prefix` variable
            - `tests/upgrade.bats`
              - added the 1.4 config case, which fails today
            - nothing in `setup.sh` itself

            The failing case, so you can run it:

            ```bash
            HOME="$(mktemp -d)" ./setup.sh --version 1.4
            echo 'cache_dir = "/tmp/old-cache"' >> "$HOME/.config/tool/config.toml"
            HOME="$HOME" ./setup.sh
            grep cache_dir "$HOME/.config/tool/config.toml"   # expected: the old path, found: nothing
            ```

            > The upgrade path is the only one users never test by hand, so it is the one most worth a test.

            Left to do:

            - [x] syntax and lint
            - [ ] carry `cache_dir` over on upgrade
            - [ ] send progress lines to stdout

            ---

            Details of the 2.0 config format are in the [migration notes](https://example.com/tool/migrating-to-2).
            """

        static func reply(_ name: String) -> String {
            switch name {
            case "plain": plain
            case "wide": wide
            case "code": code
            case "nested": nested
            default: reply
            }
        }

        static let plain = """
            The upgrade fix is in. I read the old `cache_dir` once, write it under the new key, and print one line that \
            says where the cache now lives. Nothing else in the installer changed.

            I ran the suite twice and the new upgrade test passes both times. A clean install and a re-run still behave \
            as before: the first writes everything, the second changes nothing and says so.

            If you want to see it yourself, run the upgrade test on its own; it takes about four seconds.
            """

        static let wide = """
            Every platform the installer was tried on:

            | Platform | Shell | Install | Re-run | Upgrade from 1.4 | Uninstall | Time | Notes |
            | --- | --- | --- | --- | --- | --- | ---: | --- |
            | macOS 26 (arm64) | zsh 5.9 | Done | Done | Blocked by `cache_dir` | Done | 41 s | The launch agent loads at once; `launchctl print` shows it running. Cache at `~/.cache/tool/objects/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08`. |
            | macOS 15 (x86_64) | bash 3.2 | Done | Done | Blocked by `cache_dir` | Done | 58 s | Old bash: arrays avoided on purpose, every expansion quoted. |
            | Ubuntu 24.04 | bash 5.2 | Done | Done | Blocked by `cache_dir` | Done | 22 s | Uses a systemd user unit instead of a launch agent. |
            | Debian 13 | dash | Done | Done | Blocked by `cache_dir` | Done | 25 s | `/bin/sh` is dash; the script re-executes itself under bash first. |
            | Alpine 3.21 | ash | Failed | n/a | n/a | n/a | 3 s | **(major)** No `curl` by default; the download step needs `wget` as a fallback. |
            """

        static let code = """
            The whole check script, so you can run it by hand:

            ```bash
            step_01() { echo "step 01: checking the config"; }
            step_02() { echo "step 02: checking the binary"; }
            step_03() { echo "step 03: checking the launch agent"; }
            step_04() { echo "step 04: checking the permissions"; }
            step_05() { echo "step 05: checking the cache"; }
            step_06() { echo "step 06: checking the config"; }
            step_07() { echo "step 07: checking the binary"; }
            step_08() { echo "step 08: checking the launch agent"; }
            step_09() { echo "step 09: checking the permissions"; }
            step_10() { echo "step 10: checking the cache"; }
            step_11() { echo "step 11: checking the config"; }
            step_12() { echo "step 12: checking the binary"; }
            step_13() { echo "step 13: checking the launch agent"; }
            step_14() { echo "step 14: checking the permissions"; }
            step_15() { echo "step 15: checking the cache"; }
            step_16() { echo "step 16: checking the config"; }
            step_17() { echo "step 17: checking the binary"; }
            step_18() { echo "step 18: checking the launch agent"; }
            step_19() { echo "step 19: checking the permissions"; }
            step_20() { echo "step 20: checking the cache"; }
            step_21() { echo "step 21: checking the config"; }
            step_22() { echo "step 22: checking the binary"; }
            step_23() { echo "step 23: checking the launch agent"; }
            step_24() { echo "step 24: checking the permissions"; }
            step_25() { echo "step 25: checking the cache"; }
            step_26() { echo "step 26: checking the config"; }
            step_27() { echo "step 27: checking the binary"; }
            step_28() { echo "step 28: checking the launch agent"; }
            step_29() { echo "step 29: checking the permissions"; }
            step_30() { echo "step 30: checking the cache"; }
            step_31() { echo "step 31: checking the config"; }
            step_32() { echo "step 32: checking the binary"; }
            step_33() { echo "step 33: checking the launch agent"; }
            step_34() { echo "step 34: checking the permissions"; }
            step_35() { echo "step 35: checking the cache"; }
            step_36() { echo "step 36: checking the config"; }
            step_37() { echo "step 37: checking the binary"; }
            step_38() { echo "step 38: checking the launch agent"; }
            step_39() { echo "step 39: checking the permissions"; }
            step_40() { echo "step 40: checking the cache"; }
            step_41() { echo "step 41: checking the config"; }
            step_42() { echo "step 42: checking the binary"; }
            step_43() { echo "step 43: checking the launch agent"; }
            step_44() { echo "step 44: checking the permissions"; }
            step_45() { echo "step 45: checking the cache"; }
            step_46() { echo "step 46: checking the config"; }
            step_47() { echo "step 47: checking the binary"; }
            step_48() { echo "step 48: checking the launch agent"; }
            step_49() { echo "step 49: checking the permissions"; }
            step_50() { echo "step 50: checking the cache"; }
            step_51() { echo "step 51: checking the config"; }
            step_52() { echo "step 52: checking the binary"; }
            step_53() { echo "step 53: checking the launch agent"; }
            step_54() { echo "step 54: checking the permissions"; }
            step_55() { echo "step 55: checking the cache"; }
            step_56() { echo "step 56: checking the config"; }
            step_57() { echo "step 57: checking the binary"; }
            step_58() { echo "step 58: checking the launch agent"; }
            step_59() { echo "step 59: checking the permissions"; }
            step_60() { echo "step 60: checking the cache"; }
            ```

            Each step prints one line; the first failure stops the run.
            """

        static let nested = """
            The release checklist, as it stands:

            - Installer
              - Upgrade path
                - read the old `cache_dir` once
                - write it under the new key
                - print where the cache now lives
              - Logging
                - progress lines to stdout
                - errors stay on stderr
            - Tests
              1. upgrade from 1.4 with a custom cache
              2. upgrade from 1.4 with the default cache
              3. clean install on a fresh account
            - Docs
              - [x] README install section
              - [ ] migration notes for 2.0
            """
    }
#endif

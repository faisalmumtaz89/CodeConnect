//! `codeconnect pair`, `codeconnect devices`, `codeconnect revoke`.
//!
//! ## Why the QR is drawn with explicit colours
//!
//! A QR code is only scannable if its dark modules are actually darker than its
//! light ones. Terminal art that relies on the default foreground colour gets
//! this backwards on a dark theme — which is what most developer terminals use
//! — producing an *inverted* code that some scanners read, some read slowly,
//! and some refuse. Since the whole point is "point your phone at the screen
//! and it works", every module is drawn with an explicit black-on-white SGR
//! pair, and the result looks the same under any terminal theme.
//!
//! Half-block glyphs pack two module rows into one character row, so the code
//! stays about 45 columns wide and 25 rows tall — small enough to fit a normal
//! terminal without scrolling, which matters because a QR split across a scroll
//! boundary cannot be scanned at all.

use std::io::IsTerminal;
use std::net::IpAddr;

use anyhow::{bail, Result};
use protocol::ipc::{ClientFrame, DaemonFrame};
use protocol::pairing::{format_for_display, unreachable_host, QrPayload, PAIRING_TTL_SECS};
use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;

use crate::daemon;

/// Black foreground on a white background. Applied per line, and reset at the
/// end of each, so a terminal that wraps the output cannot bleed the colour
/// across the rest of the screen.
const INK: &str = "\u{1b}[30;47m";
const RESET: &str = "\u{1b}[0m";

pub fn pair(args: &[String]) -> Result<()> {
    crate::no_arguments(args, "codeconnect pair")?;
    // Where the daemon listens comes from this file; one it could not read is
    // why it found no `ws_bind`.
    let config = protocol::config::Config::load();

    let reply = daemon::request(&ClientFrame::CreatePairing {
        ttl_secs: PAIRING_TTL_SECS,
        allow_ssh: false,
    })?;
    let DaemonFrame::Pairing {
        code,
        host,
        port,
        tls,
        ..
    } = reply
    else {
        bail!("unexpected reply from ccd: {reply:?}");
    };

    // **A code the phone cannot reach is worse than no code.**
    //
    // `ccd` resolves its bind address once, at startup, and prints whatever it
    // resolved into every code minted afterwards. When Tailscale is not up at that
    // moment it binds loopback and says so in a log nobody reads — and this
    // command used to take that host verbatim, draw a perfectly scannable QR
    // around it, and exit 0. Every visible sign was success: the daemon was
    // installed, running and answering; only the code was dead.
    //
    // The check belongs here rather than in the daemon because this is the moment
    // a human is asking for something to hand to a phone, and it is the last point
    // at which refusing costs nothing.
    if let Some(problem) = unreachable_host(&host) {
        let cause = bind_cause(host_address(&host), configured_ws_bind(&config));
        bail!("{}", unreachable_bind_message(&host, problem, cause));
    }

    let payload = QrPayload::new(&host, port, &code);
    let json = payload.to_json();

    println!();
    print_qr(&json)?;
    println!();
    println!("  scan with the CodeConnect app, or enter it by hand:");
    println!();
    println!("    address   {}", typed_address(&host, port));
    println!("    code      {}", format_for_display(&code));
    println!();

    let advisory = transport_advisory(tls, &host);
    if !advisory.is_empty() {
        for line in advisory {
            println!("{line}");
        }
        println!();
    }

    println!(
        "  the code is single-use and expires in {} minutes.",
        PAIRING_TTL_SECS / 60
    );
    println!();
    Ok(())
}

/// The port the app's manual field assumes when the address leaves it out.
const APP_DEFAULT_PORT: u16 = 8787;

/// What chose an unreachable bind, read from the config as it is now.
///
/// With no `ws_bind` the daemon takes this Mac's tailnet address. Run by launchd
/// it also watches for one and restarts itself onto it a minute or so after it
/// appears, so a bind still unreachable when somebody asks means Tailscale is
/// not set up or not connected here — not that it was late at login. A valid
/// `ws_bind` the daemon cannot bind stops it rather than falling back, so a
/// `ws_bind` other than the bound address is one it has not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindCause {
    /// No `ws_bind`: the tailnet was the daemon's to find.
    Tailnet,
    /// `ws_bind` names the bound address itself.
    Named,
    /// `ws_bind` names another address, not applied yet.
    Pending(IpAddr),
}

pub(crate) fn bind_cause(bound: Option<IpAddr>, ws_bind: Option<IpAddr>) -> BindCause {
    match ws_bind {
        None => BindCause::Tailnet,
        Some(bind) if bound == Some(bind) => BindCause::Named,
        Some(bind) => BindCause::Pending(bind),
    }
}

/// `ws_bind` as the daemon would take it: an address, or none.
pub(crate) fn configured_ws_bind(config: &protocol::config::Config) -> Option<IpAddr> {
    config.ws_bind.as_deref().and_then(|bind| bind.parse().ok())
}

/// An address the daemon reported, bare or bracketed; `None` for a name.
pub(crate) fn host_address(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

/// Why a code for this bind would be dead, and the way out.
fn unreachable_bind_message(host: &str, problem: &str, cause: BindCause) -> String {
    let why = match cause {
        BindCause::Tailnet => "With no `ws_bind` in ~/.codeconnect/config.json it uses this \
                               Mac's Tailscale address, and it found none: Tailscale is not \
                               set up or not connected on this Mac.\n\n  \
                               Set up or connect Tailscale, or set `ws_bind` \
                               (https://codeconnect.sh/support explains both), then:"
            .to_string(),
        BindCause::Named => "`ws_bind` in ~/.codeconnect/config.json names this address. Set \
                             it to this Mac's address on a network your phone is on, or remove \
                             it to use Tailscale (https://codeconnect.sh/support explains both), \
                             then:"
            .to_string(),
        BindCause::Pending(bind) => format!(
            "`ws_bind` in ~/.codeconnect/config.json names {bind}, which the daemon has \
             not applied yet:"
        ),
    };
    format!(
        "this daemon is listening on {host}, which {problem}, so a paired \
         phone could never reach it.\n\n  \
         {why}\n\n      \
         codeconnect daemon restart\n      \
         codeconnect pair\n\n  \
         `codeconnect daemon status` shows the address it is currently bound to."
    )
}

/// The address as the app's manual field takes it: the port only when it is not
/// the one the app assumes, and an IPv6 literal bracketed so its port reads.
fn typed_address(host: &str, port: u16) -> String {
    if port == APP_DEFAULT_PORT {
        host.to_string()
    } else if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub fn devices(args: &[String]) -> Result<()> {
    // `codeconnect devices` takes no arguments, but the obvious guess for revoking is
    // `codeconnect devices revoke <id>` — it mirrors `codeconnect sessions prune`. Discarding the
    // extra words would print the device table, which reads as success, and the
    // device would still be connectable. A revocation that looks done and is
    // not is the one failure this command must never produce.
    if let Some(first) = args.first() {
        if first == "revoke" {
            bail!(
                "`codeconnect devices` only lists; to revoke, run: codeconnect revoke <device>{}",
                args.get(1)
                    .map(|id| format!(" (here: codeconnect revoke {id})"))
                    .unwrap_or_default()
            );
        }
    }
    crate::no_arguments(args, "codeconnect devices")?;

    let reply = daemon::request(&ClientFrame::ListDevices)?;
    let DaemonFrame::Devices { devices } = reply else {
        bail!("unexpected reply from ccd: {reply:?}");
    };
    if devices.is_empty() {
        println!("no paired devices; run `codeconnect pair` to add one");
        return Ok(());
    }

    // Wide enough for the name the app registers itself under plus the `-N`
    // the daemon appends when a second device claims it. At 18 the two read as
    // "CodeConnect iPhone" and "CodeConnect iPhon…", which is a poor thing to
    // choose between when the choice is which credential to revoke.
    println!("{:<14} {:<24} {:<10} LAST SEEN", "DEVICE", "NAME", "STATUS");
    for device in &devices {
        println!(
            "{:<14} {:<24} {:<10} {}",
            device.device_id,
            truncate(&device.name, 24),
            if device.is_active() {
                "active"
            } else {
                "revoked"
            },
            device.last_seen_at.as_deref().unwrap_or("never"),
        );
    }
    Ok(())
}

pub fn revoke(args: &[String]) -> Result<()> {
    // A flag or nothing in the device's place — `revoke --help` — is a question
    // about the command, never a device to cut off.
    let Some(device) = args
        .first()
        .filter(|device| !device.is_empty() && !device.starts_with('-'))
    else {
        bail!("usage: codeconnect revoke <device>   (`codeconnect devices` lists them)");
    };
    crate::no_arguments(&args[1..], "codeconnect revoke <device>")?;
    let reply = daemon::request(&ClientFrame::RevokeDevice {
        device: device.clone(),
        ssh_only: false,
    })?;
    let DaemonFrame::Revoked {
        device,
        token_revoked,
        ..
    } = reply
    else {
        bail!("unexpected reply from ccd: {reply:?}");
    };

    println!("{} ({})", device.device_id, device.name);
    if token_revoked {
        println!("  token     revoked; that device can no longer connect");
    } else {
        println!("  token     was already revoked");
    }
    Ok(())
}

/// What to say about the transport under the QR, or nothing when there is
/// nothing to say. Lines carry their own indentation, so what is asserted here
/// is what reaches the screen.
///
/// **A note about a transport has to be a fact about a transport.** It lives
/// out here rather than inside [`pair`] because that is the only way any of it
/// can be checked: `pair` needs a daemon answering the socket to get as far as
/// the line that prints this, so every claim it made used to ship untested.
///
/// The plaintext arm names no network, and that is the whole of the fix.
/// `tls == false` establishes exactly one thing — this daemon serves `ws://` —
/// and the shim holds no second fact to build on. It never sees the bind
/// address, and `host` is not a stand-in for one: an explicit non-tailnet
/// `ws_bind` with `ws_allow_plaintext` set leaves `ccd` advertising the LAN
/// address it listens on, and [`unreachable_host`] passes every private
/// address through on purpose. That operator used to be answered with
/// "Tailscale still encrypts the tailnet link" — reassurance about a tunnel
/// carrying none of it, printed over the one configuration where the pairing
/// code really does cross a network in the clear. The comment that justified
/// it said `unreachable_host` had already refused every address not on the
/// tailnet; it refuses addresses that do not *route*, which is a different set
/// and never held `192.168.0.0/16`.
///
/// Nor can the host be classified into the claim. `100.64.0.0/10` is where
/// Tailscale allocates and is also ordinary carrier-grade NAT, and a
/// MagicDNS-shaped name is a string anybody may put in `tls_hostname`. Only
/// `ccd` knows whether its listener sits on one of this node's own tailnet
/// addresses — it decides exactly that at bind time — and it keeps the answer
/// to itself: it reaches neither the `Daemon` state nor the `Pairing` frame.
/// Until it does, the honest note is this one.
fn transport_advisory(tls: bool, host: &str) -> &'static [&'static str] {
    if !tls {
        return &[
            "  note: this daemon is not serving TLS, so the app will connect over",
            "        ws://. CodeConnect encrypts nothing on that connection: the",
            "        pairing code, the token it returns and every message after",
            "        them are exactly as private as the path to the address above.",
        ];
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return &[
            "  warning: TLS is on but the address above is an IP address. The",
            "           certificate is issued for a MagicDNS name and will not",
            "           validate against an IP.",
        ];
    }
    &[]
}

/// Render `payload` as a QR code on stdout.
fn print_qr(payload: &str) -> Result<()> {
    let code = QrCode::new(payload.as_bytes())
        .map_err(|err| anyhow::anyhow!("could not encode the pairing QR: {err}"))?;
    // Default colours: a dark module becomes a drawn glyph. Combined with the
    // black-on-white SGR pair below, a drawn glyph *is* a dark module — which
    // is the polarity a scanner expects.
    let art = code.render::<Dense1x2>().quiet_zone(true).build();

    let colour = std::io::stdout().is_terminal();
    for line in art.lines() {
        if colour {
            println!("  {INK}{line}{RESET}");
        } else {
            // Piped or redirected: escape codes would be noise in a log, and a
            // file is not something anyone points a camera at anyway.
            println!("  {line}");
        }
    }
    Ok(())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A revoke with no device names the command as it is typed.
    #[test]
    fn revoke_without_a_device_shows_the_codeconnect_usage() {
        for args in [vec![], vec!["--help".to_string()]] {
            let usage = revoke(&args).unwrap_err().to_string();
            assert!(
                usage.starts_with("usage: codeconnect revoke <device>"),
                "{usage}"
            );
        }
    }

    /// An unreachable bind is explained by what the config says now: no
    /// `ws_bind` is Tailscale's to explain, a `ws_bind` naming this address is the
    /// setting to change, and any other `ws_bind` is one not applied yet. Each
    /// ends in the restart that applies the fix.
    #[test]
    fn an_unreachable_bind_is_explained_by_the_config_that_chose_it() {
        let loopback = "is a loopback address";
        let cause =
            |host: &str, ws_bind: &str| bind_cause(host_address(host), ws_bind.parse().ok());
        let tailnet = unreachable_bind_message("127.0.0.1", loopback, cause("127.0.0.1", ""));
        assert!(
            tailnet.contains("Tailscale is not set up or not connected"),
            "{tailnet}"
        );
        assert!(
            tailnet.contains("https://codeconnect.sh/support"),
            "{tailnet}"
        );
        assert!(!tailnet.contains("not up yet"), "{tailnet}");
        assert!(!tailnet.contains("`ccd`"), "{tailnet}");

        let named = unreachable_bind_message("::1", loopback, cause("::1", "::1"));
        assert_eq!(cause("[::1]", "::1"), BindCause::Named);
        assert!(named.contains("names this address"), "{named}");
        assert!(!named.contains("Tailscale is not set up"), "{named}");

        let pending =
            unreachable_bind_message("127.0.0.1", loopback, cause("127.0.0.1", "192.168.1.20"));
        assert!(
            pending.contains("names 192.168.1.20, which the daemon has not applied yet"),
            "{pending}"
        );
        assert!(!pending.contains("Tailscale is not set up"), "{pending}");

        for message in [&tailnet, &named, &pending] {
            assert!(
                message.contains("codeconnect daemon restart\n      codeconnect pair"),
                "{message}"
            );
        }
    }

    /// What `pair` prints for the app's Address field is what that field
    /// parses back (ios Store/Pairing.swift `DaemonEndpoint.parse`).
    #[test]
    fn the_typed_address_is_what_the_apps_field_takes() {
        assert_eq!(typed_address("192.168.1.20", 8787), "192.168.1.20");
        assert_eq!(
            typed_address("mac.tail1234.ts.net", 8787),
            "mac.tail1234.ts.net"
        );
        assert_eq!(typed_address("192.168.1.20", 9000), "192.168.1.20:9000");
        assert_eq!(typed_address("fd7a:115c::1", 8787), "fd7a:115c::1");
        assert_eq!(typed_address("fd7a:115c::1", 9000), "[fd7a:115c::1]:9000");
    }

    #[test]
    fn listing_devices_refuses_a_revoke_that_would_silently_do_nothing() {
        // `codeconnect devices revoke <id>` is the natural guess, because `codeconnect sessions
        // prune` is spelled that way. It used to print the device table and
        // exit 0, so an operator revoking a lost phone got a success-shaped
        // answer while the token stayed live. It must fail, and it must say
        // where the real command is.
        let err = devices(&["revoke".into(), "db1dd87a50a5".into()])
            .expect_err("a revoke spelled this way must not report success");
        let message = err.to_string();
        assert!(
            message.contains("codeconnect revoke db1dd87a50a5"),
            "the error has to carry the command that works: {message}"
        );

        // Bare `codeconnect devices` still lists, so only unexpected words are refused.
        let err = devices(&["--all".into()]).expect_err("unknown argument must not be ignored");
        assert!(
            err.to_string().contains("usage: codeconnect devices"),
            "{err}"
        );
    }

    #[test]
    fn the_pairing_payload_encodes_as_a_qr_that_fits_a_terminal() {
        // A long MagicDNS name is the worst realistic case. If this needs a
        // version so large the code stops fitting an 80-column terminal, the
        // operator would have to resize the window to scan — so it is a test.
        let payload = QrPayload::new(
            "a-very-long-machine-name-for-testing.tailnet-name.ts.net",
            8787,
            "ABCD2345",
        );
        let json = payload.to_json();
        let code = QrCode::new(json.as_bytes()).expect("must encode");
        let art = code.render::<Dense1x2>().quiet_zone(true).build();

        let widest = art.lines().map(|line| line.chars().count()).max().unwrap();
        assert!(
            widest <= 76,
            "{widest} columns is too wide to scan on screen"
        );
        assert!(art.lines().count() >= 10, "suspiciously small QR");
        // One character per module across, two module rows per text row, plus
        // the 4-module quiet zone on each side. A terminal cell is about twice
        // as tall as it is wide, so this is what makes the modules square —
        // and a stretched QR is a QR that scanners struggle with.
        assert_eq!(widest, code.width() + 8, "module dimensions changed");
        // Rounded up: an odd number of module rows still needs a whole text row
        // for the last one.
        assert_eq!(
            art.lines().count(),
            (code.width() + 8).div_ceil(2),
            "half-block packing changed"
        );
        assert!(
            art.lines().count() <= 30,
            "a QR taller than the terminal scrolls, and half a QR cannot be scanned"
        );
    }

    #[test]
    fn a_plaintext_endpoint_is_never_promised_a_network_that_encrypts_it() {
        // The shipped configuration this arm exists for: an explicit LAN
        // `ws_bind`, `ws_allow_plaintext` set, and no certificate for a name
        // that reaches the bind. `ccd` then advertises the address it listens
        // on — its own `an_explicit_lan_bind_advertises_the_address_it_listens_on`
        // asserts host `192.168.1.20` with `tls: false` — and the check above
        // waves it through, which is what puts this note on the screen. No
        // tailnet is anywhere on that path.
        //
        // Pinned here rather than taken on trust, because the deleted note was
        // justified by a comment claiming this very check had already refused
        // every non-tailnet address.
        assert_eq!(unreachable_host("192.168.1.20"), None);
        assert_eq!(unreachable_host("10.0.0.5"), None);

        let note = transport_advisory(false, "192.168.1.20").join("\n");
        let lower = note.to_lowercase();
        assert!(
            !lower.contains("tailscale") && !lower.contains("tailnet"),
            "the shim cannot see the bind address, so it may not name the \
             network carrying the bytes: {note}"
        );
        assert!(
            note.contains("ws://") && note.contains("encrypts nothing"),
            "what `tls == false` proves is all this may say: {note}"
        );
        // The stake, named rather than left to be inferred: this is the
        // connection the pairing code goes up and the device token comes back
        // down on (`hello_ack`, in ccd).
        assert!(
            note.contains("pairing code") && note.contains("token"),
            "an operator deciding about this transport needs to know what is \
             on it: {note}"
        );
    }

    #[test]
    fn a_tailnet_shaped_host_buys_no_softer_note_than_a_lan_one() {
        // `100.64.0.0/10` is where Tailscale allocates *and* ordinary
        // carrier-grade NAT; `fd7a:115c:a1e0::/48` is Tailscale's range but an
        // address in it still reaches this shim unverified; a MagicDNS-shaped
        // name is a string anybody may put in `tls_hostname`. None of the
        // three is proof, so none may earn the reassurance. Byte-identical
        // output is what stops a shape test from creeping back in.
        let lan = transport_advisory(false, "192.168.1.20");
        for host in [
            "100.64.12.34",
            "fd7a:115c:a1e0::1",
            "some-mac.tailnet-example.ts.net",
        ] {
            assert_eq!(transport_advisory(false, host), lan, "{host}");
        }
    }

    #[test]
    fn every_advisory_fits_the_screen_and_hangs_under_its_label() {
        // The other two transports, here so that pulling the advisory out of
        // `pair` cannot quietly have dropped one.
        let tls_on_an_ip = transport_advisory(true, "100.64.12.34");
        assert!(
            tls_on_an_ip[0].contains("warning:"),
            "a certificate cannot validate against an address, and the operator \
             is the only one who can fix that: {tls_on_an_ip:?}"
        );
        assert!(
            transport_advisory(true, "some-mac.tailnet-example.ts.net").is_empty(),
            "a name with a certificate behind it needs no advisory"
        );

        for advisory in [transport_advisory(false, "192.168.1.20"), tls_on_an_ip] {
            let (first, rest) = advisory.split_first().expect("an advisory has lines");
            for line in advisory {
                // The budget the QR above it is drawn to. An advisory that
                // wraps under a code that does not is the same defect twice.
                assert!(line.chars().count() <= 76, "{line:?} is too wide to read");
            }
            // `  note: ` and `  warning: ` — a continuation that does not hang
            // under its label reads as a separate, unrelated line.
            let label = first.find(": ").expect("an advisory leads with a label") + 2;
            for line in rest {
                assert_eq!(
                    line.len() - line.trim_start().len(),
                    label,
                    "{line:?} does not hang under its label"
                );
            }
        }
    }

    #[test]
    fn colour_wrapping_resets_on_every_line() {
        // A line that sets a background and never resets it bleeds across the
        // rest of the terminal, which looks like the shim corrupted the screen.
        let line = format!("  {INK}xx{RESET}");
        assert!(line.ends_with(RESET));
        assert_eq!(line.matches(INK).count(), line.matches(RESET).count());
    }

    #[test]
    fn names_are_truncated_without_breaking_the_table() {
        assert_eq!(truncate("iPhone", 24), "iPhone");
        assert_eq!(truncate(&"x".repeat(30), 24).chars().count(), 24);
        // The real case this width exists for: two devices whose names differ
        // only in a suffix must not both truncate to the same string.
        assert_ne!(
            truncate("CodeConnect iPhone", 24),
            truncate("CodeConnect iPhone-2", 24)
        );
        // Multi-byte names must not panic or be cut mid-character.
        assert_eq!(truncate(&"é".repeat(30), 5).chars().count(), 5);
    }
}

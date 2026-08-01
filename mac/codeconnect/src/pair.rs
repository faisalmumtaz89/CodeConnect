//! `codeconnect pair`, `codeconnect devices`, `codeconnect revoke`, `codeconnect ssh-revoke`.
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
//!
//! ## Why `--ssh` is a flag on this command
//!
//! Consent to install an SSH key has to be expressed at the Mac's keyboard,
//! bound to one pairing, and impossible for the phone to request on its own.
//! A flag on the command that mints the code is exactly that: it lives and dies
//! with the five-minute code (`ccd` stores the permission alongside it), and no
//! amount of anything the phone sends can turn it on.

use std::io::IsTerminal;

use anyhow::{bail, Result};
use protocol::ipc::{ClientFrame, DaemonFrame};
use protocol::pairing::{format_for_display, QrPayload, PAIRING_TTL_SECS};
use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;

use crate::daemon;

/// Black foreground on a white background. Applied per line, and reset at the
/// end of each, so a terminal that wraps the output cannot bleed the colour
/// across the rest of the screen.
const INK: &str = "\u{1b}[30;47m";
const RESET: &str = "\u{1b}[0m";

pub fn pair(args: &[String]) -> Result<()> {
    let mut allow_ssh = false;
    for arg in args {
        match arg.as_str() {
            "--ssh" => allow_ssh = true,
            other => bail!("unknown option {other:?}; usage: codeconnect pair [--ssh]"),
        }
    }

    let reply = daemon::request(&ClientFrame::CreatePairing {
        ttl_secs: PAIRING_TTL_SECS,
        allow_ssh,
    })?;
    let DaemonFrame::Pairing {
        code,
        expires_at,
        host,
        port,
        tls,
        allow_ssh,
    } = reply
    else {
        bail!("unexpected reply from ccd: {reply:?}");
    };

    let payload = QrPayload::new(&host, port, &code);
    let json = payload.to_json();

    println!();
    print_qr(&json)?;
    println!();
    println!("  scan with the CodeConnect app, or enter it by hand:");
    println!();
    println!("    code      {}", format_for_display(&code));
    println!("    host      {host}");
    println!("    port      {port}");
    println!(
        "    scheme    {}",
        if tls { "wss (TLS)" } else { "ws (no TLS)" }
    );
    println!("    expires   {expires_at}");
    println!();
    println!("  {json}");
    println!();

    if !tls {
        // Said plainly rather than buried: the operator is choosing a transport
        // here, and "no certificate" is a fact about their setup, not a bug.
        println!("  note: this daemon has no tailscale certificate, so the app will");
        println!("        connect over ws://. Tailscale still encrypts the tailnet link.");
        println!();
    } else if host.parse::<std::net::IpAddr>().is_ok() {
        println!("  warning: TLS is on but the host above is an IP address. The");
        println!("           certificate is issued for a MagicDNS name and will not");
        println!("           validate against an IP.");
        println!();
    }

    if allow_ssh {
        println!("  --ssh: if the app offers an ed25519 public key while redeeming");
        println!("         this code, it will be appended to ~/.ssh/authorized_keys");
        println!("         and that device will be able to open a shell on this Mac.");
        println!("         Remove it later with `codeconnect ssh-revoke <device>`.");
        println!();
    }
    println!(
        "  the code is single-use and expires in {} minutes.",
        PAIRING_TTL_SECS / 60
    );
    println!();
    Ok(())
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
        bail!("unexpected argument {first:?}; usage: codeconnect devices");
    }

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
    println!(
        "{:<14} {:<24} {:<10} {:<5} LAST SEEN",
        "DEVICE", "NAME", "STATUS", "SSH"
    );
    for device in &devices {
        println!(
            "{:<14} {:<24} {:<10} {:<5} {}",
            device.device_id,
            truncate(&device.name, 24),
            if device.is_active() {
                "active"
            } else {
                "revoked"
            },
            if device.ssh_key_installed { "yes" } else { "-" },
            device.last_seen_at.as_deref().unwrap_or("never"),
        );
    }
    // Only printed when there is one, and worth the line: it is how an operator
    // checks that the key on the Mac is the key on the phone.
    for device in devices.iter().filter(|d| d.ssh_fingerprint.is_some()) {
        if device.ssh_key_installed {
            println!(
                "\n{}  ssh key {}",
                device.device_id,
                device.ssh_fingerprint.as_deref().unwrap_or("")
            );
        }
    }
    Ok(())
}

pub fn revoke(args: &[String], ssh_only: bool) -> Result<()> {
    let Some(device) = args.first() else {
        bail!(
            "usage: cc {} <device>   (`codeconnect devices` lists them)",
            if ssh_only { "ssh-revoke" } else { "revoke" }
        );
    };
    let reply = daemon::request(&ClientFrame::RevokeDevice {
        device: device.clone(),
        ssh_only,
    })?;
    let DaemonFrame::Revoked {
        device,
        token_revoked,
        ssh_key_removed,
    } = reply
    else {
        bail!("unexpected reply from ccd: {reply:?}");
    };

    println!("{} ({})", device.device_id, device.name);
    match (token_revoked, ssh_only) {
        (true, _) => println!("  token     revoked; that device can no longer connect"),
        (false, true) => println!("  token     left alone (--ssh-revoke only removes the key)"),
        (false, false) => println!("  token     was already revoked"),
    }
    if ssh_key_removed {
        println!("  ssh key   removed from ~/.ssh/authorized_keys");
    } else {
        println!("  ssh key   none was installed");
    }
    Ok(())
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

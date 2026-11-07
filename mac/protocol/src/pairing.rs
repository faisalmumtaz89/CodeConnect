//! QR pairing: the payload, the code alphabet, and the device record.
//!
//! Pairing replaces "read a 64-hex-character token off a terminal and type it
//! into a phone" with "point the camera at the screen". The shapes live here so
//! `cc` (which prints the QR), `ccd` (which mints and consumes the code) and the
//! iPhone (which scans it) cannot disagree about a single field name.
//!
//! ## Why the code is short and the token is not
//!
//! The **code** is a 5-minute, single-use capability that exists only long
//! enough to cross the air gap between a screen and a camera. Eight characters
//! of a 32-symbol alphabet is 2^40 — unguessable inside a 5-minute window over a
//! tailnet where each attempt costs a TCP connection and a closed socket — and
//! short enough to read aloud when the camera fails.
//!
//! The **device token** it buys is 256 bits, lives in the Keychain, and is what
//! actually authenticates every later connection. Trading a weak short-lived
//! secret for a strong long-lived one is the whole shape of pairing.
//!
//! The alphabet omits `I`, `O`, `0` and `1`, so the fallback path — a human
//! reading characters off a screen — has no confusable pairs at all.

use serde::{Deserialize, Serialize};

/// The page a pairing QR opens; see [`QrPayload::to_link`].
pub const PAIR_LINK: &str = "https://codeconnect.sh/p";

/// Uppercase, unambiguous, 32 symbols: `A-Z` without `I`/`O`, `2-9`.
pub const PAIRING_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 8 symbols of a 32-symbol alphabet = 2^40.
pub const PAIRING_CODE_LEN: usize = 8;

/// Long enough to walk to the phone, short enough that a screenshot left on a
/// shared display is not a standing invitation.
pub const PAIRING_TTL_SECS: u64 = 300;

/// What the QR carries. The shape is fixed — the daemon encodes it and the app
/// decodes it, and neither may change it unilaterally: see [`QrPayload::to_link`].
///
/// `host` is the MagicDNS name whenever Tailscale reports one. With TLS on this
/// is not a preference but a requirement: `tailscale cert` issues for a DNS
/// name, and a certificate cannot be validated against an IP literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrPayload {
    /// Payload version, independent of `PROTOCOL_VERSION`: the QR must stay
    /// decodable by an app too old to speak the current protocol, so that it
    /// can say "update the app" instead of showing a scan failure.
    pub v: u32,
    pub host: String,
    pub port: u16,
    pub code: String,
}

impl QrPayload {
    pub fn new(host: impl Into<String>, port: u16, code: impl Into<String>) -> QrPayload {
        QrPayload {
            v: 1,
            host: host.into(),
            port,
            code: code.into(),
        }
    }

    /// The QR's contents: a web link whose fragment carries the four fields,
    /// `https://codeconnect.sh/p#v=1&h=<host>&p=<port>&c=<code>`.
    ///
    /// A phone camera opens it: the app when it is installed, and otherwise the
    /// page, which offers the App Store. A browser never sends the fragment to the
    /// server, so the code goes from the screen to the phone and nowhere else. The
    /// host is a DNS name or an IP literal and the code is from
    /// [`PAIRING_ALPHABET`], none of which can hold `&`, `=` or `#`, so nothing is
    /// escaped.
    pub fn to_link(&self) -> String {
        format!(
            "{PAIR_LINK}#v={}&h={}&p={}&c={}",
            self.v, self.host, self.port, self.code
        )
    }
}

/// Fold operator typing into the alphabet: uppercase, and drop the separators
/// (spaces, hyphens) a human naturally inserts when reading a code aloud.
///
/// Confusable characters are deliberately *not* remapped. `0`, `1`, `I` and `O`
/// are never generated, so accepting them would mean guessing what the operator
/// meant; failing with "not a pairing code" is the honest answer.
/// Why a phone could never reach this host, or `None` when it might.
///
/// **What this is not.** It is not a check that the Mac is on the tailnet, and it
/// cannot be — that would require asking Tailscale, from a process that may be
/// running before the tunnel exists. It only refuses addresses that are
/// *definitionally* unreachable from another device: a phone that dials
/// `127.0.0.1` reaches its own loopback, and `0.0.0.0` is not an address at all.
///
/// **Why it does not require a tailnet address.** The obvious stricter rule —
/// insist on `100.64/10` or a `.ts.net` name — would reject legitimate setups: a
/// custom DNS name, a tailnet with its own domain, an operator who has pinned
/// `ws_bind` to a reachable LAN address on purpose. Refusing what is certainly
/// broken is a fact; refusing what is merely unfamiliar is a guess, and this
/// system does not guess.
/// Where the Tailscale CLI lives when it is installed at all. Neither launchd
/// nor a systemd user unit hands the daemon a shell `PATH`, so lookup is by
/// known install location, never `which` — and the CLI shares this list so "is
/// Tailscale installed" cannot answer differently on the two sides of one
/// machine. The last entry is where Linux distribution packages put it.
pub const TAILSCALE_CANDIDATES: &[&str] = &[
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/usr/bin/tailscale",
];

/// The installed Tailscale binary, if any candidate exists.
pub fn tailscale_bin() -> Option<std::path::PathBuf> {
    TAILSCALE_CANDIDATES
        .iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
}

pub fn unreachable_host(host: &str) -> Option<&'static str> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Some("is empty");
    }
    if trimmed.eq_ignore_ascii_case("localhost") {
        return Some("is this computer's own loopback name");
    }
    // An IPv6 literal may arrive bracketed, as it does inside a URL.
    let bare = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    let Ok(ip) = bare.parse::<std::net::IpAddr>() else {
        // A name this crate cannot resolve is not a name it may reject.
        return None;
    };
    if ip.is_loopback() {
        return Some("is a loopback address only this computer can reach");
    }
    if ip.is_unspecified() {
        return Some("is the unspecified address, which is not somewhere to connect");
    }
    match ip {
        std::net::IpAddr::V4(v4) if v4.is_link_local() => {
            Some("is a link-local address that does not route")
        }
        std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => {
            Some("is a link-local address that does not route")
        }
        _ => None,
    }
}

pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// True when `code` could be one we issued. Cheap pre-filter so a malformed
/// code is rejected without a database round-trip.
pub fn is_well_formed(code: &str) -> bool {
    code.len() == PAIRING_CODE_LEN && code.bytes().all(|byte| PAIRING_ALPHABET.contains(&byte))
}

/// Render `code` for a human to read off the screen: `ABCD-2345`.
pub fn format_for_display(code: &str) -> String {
    if code.len() != PAIRING_CODE_LEN {
        return code.to_string();
    }
    format!("{}-{}", &code[..4], &code[4..])
}

/// A paired device, as `codeconnect devices` lists it and `codeconnect revoke` names it.
///
/// The token itself is never in this struct: the daemon stores only its hash,
/// so a leaked database backup cannot be replayed as a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSummary {
    /// Short stable handle, unique by construction. Names are chosen by the
    /// phone and may collide; this never does.
    pub device_id: String,
    pub name: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    /// Set once revoked. Revoked devices stay listed rather than vanishing:
    /// "this device was revoked on the 3rd" is a fact worth keeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    /// The Tailscale device it was last seen connecting from, by stable node id.
    /// Revoking any device with one revokes every device with the same one: they
    /// are one phone or one computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet_node: Option<String>,
    /// Paired before this daemon recorded Tailscale devices. Revoked with no
    /// `tailnet_node`, it pauses every sign-in that comes with neither a token nor
    /// a code, because the daemon cannot tell which device it was.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paired_before_nodes: bool,
}

impl DeviceSummary {
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn derived_qr_link() -> String {
        QrPayload::new("studio.tail0000.ts.net", 8787, "ABCD2345").to_link()
    }

    /// **The pairing link, emitted by this build**, compared byte for byte: one
    /// line, then a newline. The file is the fixture for the apps' and the web
    /// page's readers of the link; the copy under `ios/` is byte-identical, by
    /// `fixtures/check-duplicated-fixtures.sh`.
    #[test]
    fn the_qr_link_fixture_is_what_this_build_emits() {
        const FIXTURE: &str = include_str!("../../../fixtures/wire/qr-link.txt");
        assert_eq!(
            FIXTURE,
            derived_qr_link() + "\n",
            "the committed pairing link no longer matches what this build emits. Regenerate \
             it with cargo test -p protocol -- --ignored regenerate_the_qr_link_fixture, then \
             copy the file over its copy under ios/"
        );
        assert_eq!(
            derived_qr_link(),
            "https://codeconnect.sh/p#v=1&h=studio.tail0000.ts.net&p=8787&c=ABCD2345"
        );
    }

    /// Rewrite `fixtures/wire/qr-link.txt` with what this build emits.
    #[test]
    #[ignore = "generator, not a gate"]
    fn regenerate_the_qr_link_fixture() {
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/wire/qr-link.txt"
            ),
            derived_qr_link() + "\n",
        )
        .unwrap();
    }

    #[test]
    fn alphabet_has_no_confusable_characters() {
        for confusable in *b"IO01" {
            assert!(
                !PAIRING_ALPHABET.contains(&confusable),
                "{} is confusable and must not be generated",
                confusable as char
            );
        }
        assert_eq!(
            PAIRING_ALPHABET.len(),
            32,
            "entropy accounting assumes 2^5/char"
        );
        // No duplicates, or the entropy claim is wrong.
        let mut sorted = PAIRING_ALPHABET.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), PAIRING_ALPHABET.len());
    }

    #[test]
    fn normalisation_absorbs_how_humans_type() {
        assert_eq!(normalize_code("abcd-2345"), "ABCD2345");
        assert_eq!(normalize_code(" ABCD 2345 "), "ABCD2345");
        assert_eq!(normalize_code("ABCD_2345\n"), "ABCD2345");
    }

    #[test]
    fn confusable_input_is_rejected_rather_than_guessed() {
        // `0` is never issued, so accepting it would mean inventing an intent.
        assert!(!is_well_formed(&normalize_code("ABCD2340")));
        assert!(!is_well_formed(&normalize_code("ABCD234I")));
    }

    #[test]
    fn well_formed_rejects_wrong_lengths_and_symbols() {
        assert!(is_well_formed("ABCD2345"));
        assert!(!is_well_formed(""));
        assert!(!is_well_formed("ABCD234"));
        assert!(!is_well_formed("ABCD23456"));
        assert!(
            !is_well_formed("abcd2345"),
            "lowercase must be normalised first"
        );
        assert!(!is_well_formed("ABCD-234"));
    }

    #[test]
    fn display_grouping_is_reversible() {
        let shown = format_for_display("ABCD2345");
        assert_eq!(shown, "ABCD-2345");
        assert_eq!(normalize_code(&shown), "ABCD2345");
        // A malformed code is shown as-is rather than mangled.
        assert_eq!(format_for_display("SHORT"), "SHORT");
    }

    #[test]
    fn device_summary_omits_unset_optionals() {
        let device = DeviceSummary {
            device_id: "d1a2b3c4".into(),
            name: "iPhone".into(),
            created_at: "2026-07-31T10:00:00.000Z".into(),
            last_seen_at: None,
            revoked_at: None,
            tailnet_node: None,
            paired_before_nodes: false,
        };
        let encoded = serde_json::to_string(&device).unwrap();
        for field in ["revoked_at", "tailnet_node", "paired_before_nodes"] {
            assert!(!encoded.contains(field), "{encoded}");
        }
        assert!(device.is_active());
        assert_eq!(device, serde_json::from_str(&encoded).unwrap());
    }

    /// The addresses a phone can never reach, and the exact reason each is out.
    ///
    /// `127.0.0.1` is the one that shipped: `ccd` binds it whenever Tailscale is
    /// not up at startup, and `codeconnect pair` drew a scannable code around it
    /// and exited 0.
    #[test]
    fn definitionally_unreachable_hosts_are_refused() {
        for host in [
            "127.0.0.1",
            "127.1.2.3",
            "localhost",
            "LocalHost",
            "::1",
            "[::1]",
            "0.0.0.0",
            "::",
            "169.254.10.1",
            "fe80::1",
            "",
            "   ",
        ] {
            assert!(
                unreachable_host(host).is_some(),
                "{host:?} cannot be reached from another device and must be refused"
            );
        }
    }

    /// The half that would do real damage if it were wrong. Refusing a working
    /// setup is worse than the defect being fixed: the defect wastes one scan,
    /// this would make pairing impossible with no way for the operator to argue.
    #[test]
    fn anything_that_might_work_is_allowed_through() {
        for host in [
            // A tailnet address and a MagicDNS name — the ordinary case.
            "100.101.102.103",
            "some-mac.tailnet-example.ts.net",
            // A tailnet with its own domain, and an operator who pinned `ws_bind`
            // to something reachable on purpose. Unfamiliar is not broken.
            "mac.internal.example.com",
            "192.168.1.20",
            "10.0.0.5",
            "2001:db8::1",
            "[2001:db8::1]",
        ] {
            assert_eq!(
                unreachable_host(host),
                None,
                "{host:?} may be reachable, and refusing it would be a guess"
            );
        }
    }
}

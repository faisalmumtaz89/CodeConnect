//! Where a push is posted and what Apple is told about it.

use anyhow::Result;

/// How long APNs should keep trying to deliver a push to a phone that is off.
///
/// Long enough to survive a commute, short enough that a decision nobody
/// answered is not still buzzing tomorrow.
const PUSH_LIFETIME_SECS: i64 = 3600;

/// The one slot an ordinary doorbell occupies on a phone.
///
/// Constant on purpose — see the header's comment in `request`.
pub const COLLAPSE_ID: &str = "codeconnect";

/// The slot a **test** notification occupies, which is deliberately not the
/// doorbell's.
///
/// The user asked for this one and is watching for it; letting it replace a
/// waiting decision — or be replaced by one — would answer a different
/// question than the one they asked.
pub const TEST_COLLAPSE_ID: &str = "codeconnect-test";

/// Which Apple host to talk to.
///
/// A development build's token is **not valid on production** and vice versa;
/// the failure is a `400 BadDeviceToken`, which reads like a corrupt token
/// rather than like the wrong endpoint. The device says which world it is in
/// when it registers, so this is per-device rather than global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApnsEnvironment {
    Sandbox,
    Production,
}

impl ApnsEnvironment {
    pub fn host(self) -> &'static str {
        match self {
            ApnsEnvironment::Sandbox => "api.sandbox.push.apple.com",
            ApnsEnvironment::Production => "api.push.apple.com",
        }
    }

    pub fn parse(value: &str) -> ApnsEnvironment {
        match value {
            "production" | "prod" => ApnsEnvironment::Production,
            // Anything unrecognised is sandbox: a development build pushed at
            // production is silently undeliverable, whereas the reverse fails
            // loudly and is fixed by one config line.
            _ => ApnsEnvironment::Sandbox,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ApnsEnvironment::Sandbox => "sandbox",
            ApnsEnvironment::Production => "production",
        }
    }
}

/// The APNs request, headers and all.
///
/// **Extracted so the headers can be asserted.** The expiry in particular
/// is invisible to a payload test: Apple reads `apns-expiration` as an
/// absolute UNIX time, so a literal duration there means January 1970 — a
/// notification born expired, with no store-and-forward for a phone that is
/// switched off. A test that
/// recomputes the arithmetic proves nothing about what is on the wire; this
/// is what is on the wire.
pub fn request(
    host: &str,
    device_token: &str,
    bearer: &str,
    topic: &str,
    now_secs: i64,
    collapse: &str,
) -> Result<http::Request<()>> {
    http::Request::builder()
        .method("POST")
        .uri(format!("https://{host}/3/device/{device_token}"))
        .header("authorization", format!("bearer {bearer}"))
        .header("apns-topic", topic)
        .header("apns-push-type", "alert")
        // **One doorbell, replaced rather than queued.** Ordering the
        // sends is all this daemon can do; which notification Apple keeps
        // for a phone that is switched off is Apple's to decide, and it
        // does not promise the newest. A collapse id makes that explicit:
        // a later push *replaces* the earlier one, so a reader who was
        // away comes back to the current state rather than to whichever
        // arrived in the order the network chose.
        //
        // A constant, and deliberately not per run: this header reaches
        // Apple, and anything varying per session would let a device's
        // pushes be grouped and timed. It says only "this is CodeConnect's
        // notification", which is exactly the one slot the aggregate body
        // is written for.
        .header("apns-collapse-id", collapse)
        // 10 = deliver immediately. This is a human waiting on an agent.
        .header("apns-priority", "10")
        // An hour from now, as an absolute time: an approval nobody
        // answered by then is not worth waking anyone for, and the app
        // shows it on next foreground regardless.
        .header(
            "apns-expiration",
            (now_secs + PUSH_LIFETIME_SECS).to_string(),
        )
        .body(())
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The header on the wire, against a fixed clock.**
    ///
    /// Apple reads `apns-expiration` as an absolute UNIX time, so a literal
    /// duration there means January 1970: every notification born expired, and
    /// nothing stored for a phone that is switched off. A test that recomputes
    /// the arithmetic would pass against that too, which is why this reads the
    /// built request.
    #[test]
    fn the_expiry_header_is_an_absolute_time_an_hour_ahead() {
        let now = 1_800_000_000i64;
        let request = request(
            "api.push.apple.com",
            "aa",
            "bearer",
            "topic",
            now,
            COLLAPSE_ID,
        )
        .unwrap();
        let expiry = request.headers()["apns-expiration"].to_str().unwrap();

        assert_eq!(expiry, (now + 3600).to_string());
        assert!(
            expiry.parse::<i64>().unwrap() > now,
            "an expiry in the past is a push APNs will never store"
        );
        assert_ne!(expiry, "3600", "3600 is 1970, not an hour from now");
    }

    #[test]
    fn the_request_addresses_one_device_and_says_it_is_an_alert() {
        let request = request(
            "api.push.apple.com",
            "dev-token",
            "b",
            "topic",
            0,
            COLLAPSE_ID,
        )
        .unwrap();
        assert_eq!(request.uri().path(), "/3/device/dev-token");
        assert_eq!(request.headers()["apns-push-type"], "alert");
        assert_eq!(request.headers()["apns-priority"], "10");
        assert_eq!(
            request.headers()["apns-collapse-id"],
            "codeconnect",
            "a later doorbell replaces the earlier one rather than racing it"
        );
        assert!(
            !request.headers()["apns-collapse-id"]
                .to_str()
                .unwrap()
                .contains(|c: char| c.is_ascii_digit()),
            "and it carries no run, device or request identity through Apple"
        );
    }

    #[test]
    fn an_unknown_environment_falls_back_to_sandbox_rather_than_production() {
        // Wrong-way-round is the recoverable failure: a development token sent
        // to production is refused loudly, where the reverse is accepted and
        // silently never delivered.
        assert_eq!(ApnsEnvironment::parse(""), ApnsEnvironment::Sandbox);
        assert_eq!(ApnsEnvironment::parse("nonsense"), ApnsEnvironment::Sandbox);
        assert_eq!(
            ApnsEnvironment::parse("production"),
            ApnsEnvironment::Production
        );
        assert_eq!(ApnsEnvironment::Production.host(), "api.push.apple.com");
        assert_eq!(
            ApnsEnvironment::Sandbox.host(),
            "api.sandbox.push.apple.com"
        );
    }
}

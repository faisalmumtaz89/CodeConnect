//! A refusal as an error, carrying what it means for the device with it.

/// Apple said the app is gone from this device.
///
/// **A fact from Apple, not an inference.** The alternative was asking the
/// registry whether the device was still listed, which cannot tell "revoked"
/// from "the database did not answer just then" — and a database that blinks
/// would have retired every worker that happened to ask.
#[derive(Debug)]
pub struct DeviceGone;

impl std::fmt::Display for DeviceGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("APNs says the app is gone from this device")
    }
}

impl std::error::Error for DeviceGone {}

/// What Apple's refusal means for this device, as one decision.
///
/// **Classification and consequence together.** Read apart, a build could go on
/// deciding `410` is terminal while the refusal it produced was an ordinary
/// error — and the worker, which retires on the *typed* one, would go on
/// ringing a phone the app had been deleted from.
pub fn refusal(status: u16, reason: &str) -> anyhow::Error {
    let described = format!("APNs refused the push: {status} {reason}");
    if status == 410 {
        // `410 Unregistered` is Apple saying the app is gone from that device,
        // which never recovers.
        return anyhow::Error::new(DeviceGone).context(described);
    }
    anyhow::Error::msg(described)
}

/// The same refusal, once it is known whether it was about the token the device
/// is actually using.
///
/// **Only a refusal about the live token retires the device.** A late `410` for
/// a token the phone has already replaced says nothing about the one it is
/// using now, and typing it as a departure would close a queue holding that
/// token's work and answer its test push as though the phone were gone.
pub fn terminal_refusal(status: u16, reason: &str, was_current: bool) -> anyhow::Error {
    if was_current {
        return refusal(status, reason);
    }
    anyhow::Error::msg(format!(
        "APNs refused a token this device has already replaced: {status} {reason}"
    ))
}

/// Whether that refusal means this device token will never work again.
pub fn is_terminal(status: u16, reason: &str) -> bool {
    refusal(status, reason)
        .chain()
        .any(|cause| cause.is::<DeviceGone>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApnsEnvironment;

    /// **A refusal about a token the phone has already replaced is not a
    /// departure.** Typed as one, it would retire a queue holding the *new*
    /// token's work and answer its test push as though the phone were gone.
    #[test]
    fn only_a_refusal_about_the_live_token_retires_the_device() {
        let gone = |err: anyhow::Error| err.chain().any(|cause| cause.is::<DeviceGone>());
        assert!(
            gone(terminal_refusal(410, "{\"reason\":\"Unregistered\"}", true)),
            "the token it is using was disowned: the device is gone"
        );
        assert!(
            !gone(terminal_refusal(
                410,
                "{\"reason\":\"Unregistered\"}",
                false
            )),
            "a token it has already replaced says nothing about the one it uses now"
        );
    }

    /// The distinction that cost a live registration.
    /// The failure the first TestFlight install produced.
    #[test]
    fn a_bad_device_token_is_an_environment_problem_before_it_is_a_dead_one() {
        // `400 BadDeviceToken` means "not valid **for this host**", which a
        // wrong environment produces just as readily as a dead token. It must
        // not clear the registration, and it must leave room for the other
        // host to be tried.
        assert!(!is_terminal(400, "{\"reason\":\"BadDeviceToken\"}"));
        assert!(is_terminal(410, "{\"reason\":\"Unregistered\"}"));
        // The two hosts are genuinely different endpoints, so "the other one"
        // is always well defined.
        assert_ne!(
            ApnsEnvironment::Sandbox.host(),
            ApnsEnvironment::Production.host()
        );
    }

    #[test]
    fn only_410_is_terminal_for_a_device_token() {
        // Apple returns `400 BadDeviceToken` for a token that is simply on the
        // wrong host, which is recoverable and common while a build moves
        // between development and TestFlight. Clearing on it deletes a working
        // registration; only `410 Unregistered` means the app is gone.
        assert!(is_terminal(410, "{\"reason\":\"Unregistered\"}"));
        assert!(!is_terminal(400, "{\"reason\":\"BadDeviceToken\"}"));
        // **And that verdict is what the worker acts on.** The worker retires
        // on the typed refusal, so a build that classified `410` as terminal
        // while producing an ordinary error would go on ringing a phone the
        // app had been deleted from.
        let gone = |status, reason| {
            refusal(status, reason)
                .chain()
                .any(|cause| cause.is::<DeviceGone>())
        };
        assert!(gone(410, "{\"reason\":\"Unregistered\"}"));
        assert!(!gone(400, "{\"reason\":\"BadDeviceToken\"}"));
        assert!(!gone(503, "{\"reason\":\"ServiceUnavailable\"}"));
        assert!(!is_terminal(
            403,
            "{\"reason\":\"BadEnvironmentKeyInToken\"}"
        ));
        assert!(!is_terminal(429, "{\"reason\":\"TooManyRequests\"}"));
    }
}

//! Apple's answer as a value: what happened, and what a sender may do next.

/// The `reason` word out of Apple's error body.
///
/// **The status alone does not say what went wrong.** Apple spends `400` on a
/// dozen different faults — a token for the wrong host, a topic that is not the
/// key's, a payload that will not parse — and only the body names which. It is
/// a small JSON document, `{"reason":"BadDeviceToken","timestamp":…}`, and the
/// reason is the half that decides anything.
///
/// A body that is empty, truncated, or not JSON at all is a connection that
/// went wrong rather than a fact about the device, so it yields nothing instead
/// of a guess a caller could act on.
pub fn parse_reason(body: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(body).ok()?;
    body.get("reason")?.as_str().map(str::to_string)
}

/// What one attempt came to.
///
/// **Five outcomes, because a sender has five different jobs.** Read as a
/// status and a string, every caller re-derives the same distinctions and one
/// of them eventually gets `400 BadDeviceToken` wrong — the mistake that
/// deletes a working registration because the *host* was wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApnsOutcome {
    /// Apple took it. `apns_id` is the receipt for this exact notification when
    /// Apple sends one — the id a reader can take to Apple's delivery logs.
    Accepted { apns_id: Option<String> },
    /// `410 Unregistered`: the app is gone from that device, which never
    /// recovers. The only answer that justifies forgetting a device token.
    Unregistered,
    /// The token is not valid **at the host it was posted to**, which a wrong
    /// environment produces just as readily as a dead token. Deliberately not
    /// `Unregistered`: the other host is worth trying before anything is
    /// deleted.
    BadDeviceToken,
    /// Apple is busy or unwell. The same request later is expected to work, so
    /// nothing about the device is learned from it.
    Retryable { status: u16, reason: String },
    /// Apple will refuse this request however often it is sent — a bad topic, a
    /// provider token the key does not match, a payload too large. Retrying
    /// only burns quota; the fault is on this side and someone has to fix it.
    Rejected { status: u16, reason: String },
}

/// Read Apple's status and reason as one of the five things they can mean.
///
/// `429` and every `5xx` are Apple's own state and say nothing about the
/// device; `410` is the single answer that retires a token; everything else in
/// the `4xx` range is this sender's fault and will not improve on its own.
pub fn classify(status: u16, reason: &str, apns_id: Option<String>) -> ApnsOutcome {
    if (200..300).contains(&status) {
        return ApnsOutcome::Accepted { apns_id };
    }
    match status {
        410 => ApnsOutcome::Unregistered,
        400 if reason == "BadDeviceToken" => ApnsOutcome::BadDeviceToken,
        429 | 500..=599 => ApnsOutcome::Retryable {
            status,
            reason: reason.to_string(),
        },
        _ => ApnsOutcome::Rejected {
            status,
            reason: reason.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_is_read_from_the_body_or_not_claimed_at_all() {
        assert_eq!(
            parse_reason(r#"{"reason":"BadDeviceToken","timestamp":1234}"#).as_deref(),
            Some("BadDeviceToken")
        );
        assert_eq!(
            parse_reason(r#"{"reason":"Unregistered"}"#).as_deref(),
            Some("Unregistered")
        );
        // A refusal with no body at all is the common case for some statuses,
        // and a connection that died mid-body looks the same. Neither is a
        // reason, and inventing one would classify a device on nothing.
        assert_eq!(parse_reason(""), None);
        assert_eq!(parse_reason("<html>502 Bad Gateway</html>"), None);
        assert_eq!(parse_reason(r#"{"timestamp":1234}"#), None);
        // A `reason` that is not a word is not one either.
        assert_eq!(parse_reason(r#"{"reason":410}"#), None);
    }

    #[test]
    fn only_410_retires_a_token_and_only_apples_own_faults_are_retried() {
        assert_eq!(
            classify(200, "", Some("apns-id".into())),
            ApnsOutcome::Accepted {
                apns_id: Some("apns-id".into())
            }
        );
        assert_eq!(
            classify(200, "", None),
            ApnsOutcome::Accepted { apns_id: None }
        );
        assert_eq!(
            classify(410, "Unregistered", None),
            ApnsOutcome::Unregistered
        );
        // The wrong host, not a dead token: the other one is still worth
        // trying, so this never becomes `Unregistered`.
        assert_eq!(
            classify(400, "BadDeviceToken", None),
            ApnsOutcome::BadDeviceToken
        );
        // Another `400` is a request this sender built wrong.
        assert_eq!(
            classify(400, "BadTopic", None),
            ApnsOutcome::Rejected {
                status: 400,
                reason: "BadTopic".into()
            }
        );
        assert_eq!(
            classify(429, "TooManyRequests", None),
            ApnsOutcome::Retryable {
                status: 429,
                reason: "TooManyRequests".into()
            }
        );
        assert_eq!(
            classify(503, "ServiceUnavailable", None),
            ApnsOutcome::Retryable {
                status: 503,
                reason: "ServiceUnavailable".into()
            }
        );
        // A key or team the topic does not belong to. Sending it again changes
        // nothing.
        assert_eq!(
            classify(403, "InvalidProviderToken", None),
            ApnsOutcome::Rejected {
                status: 403,
                reason: "InvalidProviderToken".into()
            }
        );
    }
}

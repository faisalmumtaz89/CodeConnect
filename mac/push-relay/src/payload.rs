//! The notification document, composed here and nowhere else.
//!
//! The relay is given a kind and a count. It is not given, and has no way of
//! being given, a title, a body, a project, or an `aps` object — so the words on
//! a lock screen are chosen by this module from a closed vocabulary, and the
//! request that caused them could not have carried different ones.
//!
//! **The title is always `CodeConnect`.** The daemon's own direct sender names
//! the project when it knows one, because that notification goes from the user's
//! Mac to Apple and passes nothing through this service. A relayed notification
//! is built by a machine the user does not own, so the only name it is allowed
//! to say is the app's.
//!
//! Field order is not cosmetic here. These documents are asserted byte for byte
//! by the tests below, which is what makes an accidental extra field a failing
//! build rather than a thing on a stranger's phone — so the shapes are structs,
//! whose serialised order is their declaration order, rather than maps.

use serde::Serialize;

/// Why the doorbell rang: one of four words, and never the agent's own.
///
/// The same closed set the daemon composes from, restated here rather than
/// shared because the relay must be able to refuse a fifth value that a future
/// daemon invents — a relay that accepted whatever it was sent would be a relay
/// with an open vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub enum PushKind {
    #[serde(rename = "approval")]
    Approval,
    #[serde(rename = "input")]
    NeedsInput,
    #[serde(rename = "done")]
    Completed,
    #[serde(rename = "idle")]
    Idle,
}

impl PushKind {
    /// The stable word the phone reads to decide where a tap lands. Only an
    /// approval has a card in the decision list.
    pub fn tag(self) -> &'static str {
        match self {
            PushKind::Approval => "approval",
            PushKind::NeedsInput => "input",
            PushKind::Completed => "done",
            PushKind::Idle => "idle",
        }
    }

    /// The whole vocabulary of a body, as a closed set.
    fn sentence(self) -> &'static str {
        match self {
            PushKind::Approval => "Waiting on an approval",
            PushKind::NeedsInput => "Waiting for your input",
            PushKind::Completed => "Finished a turn",
            PushKind::Idle => "Waiting for you",
        }
    }

    /// Every kind, so the golden matrix cannot silently stop covering one.
    #[cfg(test)]
    const ALL: &'static [PushKind] = &[
        PushKind::Approval,
        PushKind::NeedsInput,
        PushKind::Completed,
        PushKind::Idle,
    ];
}

#[derive(Serialize)]
struct Alert {
    title: &'static str,
    body: String,
}

#[derive(Serialize)]
struct Aps {
    alert: Alert,
    sound: &'static str,
    badge: u32,
    #[serde(rename = "interruption-level")]
    interruption_level: &'static str,
}

#[derive(Serialize)]
struct Kind {
    kind: &'static str,
}

#[derive(Serialize)]
struct Doorbell {
    aps: Aps,
    codeconnect: Kind,
}

#[derive(Serialize)]
struct TestAps {
    alert: Alert,
    sound: &'static str,
}

#[derive(Serialize)]
struct TestDocument {
    aps: TestAps,
    codeconnect_test: u8,
}

/// The app's own name, and the only title a relayed notification carries.
const TITLE: &str = "CodeConnect";

/// The words of one ordinary notification.
///
/// **One slot, so the body describes the fleet rather than the event that rang.**
/// A phone holds one CodeConnect notification and a later one replaces it, so a
/// run finishing a turn would otherwise overwrite an outstanding decision. While
/// more than one agent is blocked the body is the count; only below that does
/// the kind get to speak for itself.
pub fn doorbell(kind: PushKind, blocked_count: u32) -> String {
    let body = match blocked_count {
        0 | 1 => kind.sentence().to_string(),
        n => format!("{n} agents need you"),
    };
    let document = Doorbell {
        aps: Aps {
            alert: Alert { title: TITLE, body },
            sound: "default",
            badge: blocked_count,
            interruption_level: "time-sensitive",
        },
        codeconnect: Kind { kind: kind.tag() },
    };
    serde_json::to_string(&document).unwrap_or_default()
}

/// The notification a user deliberately asked for.
///
/// The `codeconnect_test` marker is what the phone's foreground handler keys
/// on: an ordinary doorbell is redundant while the app is open, but a test the
/// user just requested has to show anyway.
pub fn test() -> String {
    let document = TestDocument {
        aps: TestAps {
            alert: Alert {
                title: TITLE,
                body: "Push works. This is the test you asked for.".to_string(),
            },
            sound: "default",
        },
        codeconnect_test: 1,
    };
    serde_json::to_string(&document).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::MAX_BLOCKED_COUNT;

    /// Apple refuses an alert payload above 4 KB outright.
    const APNS_PAYLOAD_LIMIT: usize = 4096;

    /// The exact bytes, per kind, with nothing blocked.
    #[test]
    fn every_kind_composes_its_own_sentence() {
        assert_eq!(
            doorbell(PushKind::Approval, 0),
            r#"{"aps":{"alert":{"title":"CodeConnect","body":"Waiting on an approval"},"sound":"default","badge":0,"interruption-level":"time-sensitive"},"codeconnect":{"kind":"approval"}}"#
        );
        assert_eq!(
            doorbell(PushKind::NeedsInput, 0),
            r#"{"aps":{"alert":{"title":"CodeConnect","body":"Waiting for your input"},"sound":"default","badge":0,"interruption-level":"time-sensitive"},"codeconnect":{"kind":"input"}}"#
        );
        assert_eq!(
            doorbell(PushKind::Completed, 0),
            r#"{"aps":{"alert":{"title":"CodeConnect","body":"Finished a turn"},"sound":"default","badge":0,"interruption-level":"time-sensitive"},"codeconnect":{"kind":"done"}}"#
        );
        assert_eq!(
            doorbell(PushKind::Idle, 0),
            r#"{"aps":{"alert":{"title":"CodeConnect","body":"Waiting for you"},"sound":"default","badge":0,"interruption-level":"time-sensitive"},"codeconnect":{"kind":"idle"}}"#
        );
    }

    /// Zero and one read the same; two is where the body stops being about the
    /// run that rang.
    #[test]
    fn the_count_takes_over_the_body_at_two() {
        assert_eq!(
            doorbell(PushKind::Approval, 1),
            r#"{"aps":{"alert":{"title":"CodeConnect","body":"Waiting on an approval"},"sound":"default","badge":1,"interruption-level":"time-sensitive"},"codeconnect":{"kind":"approval"}}"#
        );
        assert_eq!(
            doorbell(PushKind::Approval, 2),
            r#"{"aps":{"alert":{"title":"CodeConnect","body":"2 agents need you"},"sound":"default","badge":2,"interruption-level":"time-sensitive"},"codeconnect":{"kind":"approval"}}"#
        );
        assert_eq!(
            doorbell(PushKind::Completed, 7),
            r#"{"aps":{"alert":{"title":"CodeConnect","body":"7 agents need you"},"sound":"default","badge":7,"interruption-level":"time-sensitive"},"codeconnect":{"kind":"done"}}"#
        );
    }

    /// The count wins over the kind's sentence, whichever kind rang.
    #[test]
    fn a_blocked_fleet_reads_the_same_whatever_rang() {
        for kind in PushKind::ALL {
            let composed = doorbell(*kind, 4);
            assert!(
                composed.contains(r#""body":"4 agents need you""#),
                "{composed}"
            );
            assert!(
                composed.contains(&format!(r#""kind":"{}""#, kind.tag())),
                "{composed}"
            );
        }
    }

    #[test]
    fn the_test_notification_says_what_it_is_and_carries_nothing_else() {
        assert_eq!(
            test(),
            r#"{"aps":{"alert":{"title":"CodeConnect","body":"Push works. This is the test you asked for."},"sound":"default"},"codeconnect_test":1}"#
        );
    }

    /// **The title never varies**, which is the difference between this composer
    /// and the daemon's direct one.
    #[test]
    fn the_title_is_the_app_and_never_a_project() {
        for kind in PushKind::ALL {
            for count in [0u32, 1, 2, 9] {
                assert!(
                    doorbell(*kind, count).contains(r#""title":"CodeConnect""#),
                    "{kind:?} at {count}"
                );
            }
        }
        assert!(test().contains(r#""title":"CodeConnect""#));
    }

    /// The collapse ids are Apple's slot names, taken from the one crate that
    /// defines them rather than restated.
    #[test]
    fn the_doorbell_and_the_test_occupy_different_slots() {
        assert_eq!(push_core::COLLAPSE_ID, "codeconnect");
        assert_eq!(push_core::TEST_COLLAPSE_ID, "codeconnect-test");
        assert_ne!(
            push_core::COLLAPSE_ID,
            push_core::TEST_COLLAPSE_ID,
            "a test the user is watching for must not replace a waiting decision"
        );
    }

    /// Apple refuses an alert payload over 4 KB, and the largest document this
    /// composer can be asked for is the largest count the schema permits.
    #[test]
    fn every_payload_fits_inside_apples_bound() {
        for kind in PushKind::ALL {
            for count in [0u32, 1, 2, 9, MAX_BLOCKED_COUNT] {
                let composed = doorbell(*kind, count);
                assert!(
                    composed.len() < APNS_PAYLOAD_LIMIT,
                    "{kind:?} at {count} is {} bytes",
                    composed.len()
                );
            }
        }
        assert!(test().len() < APNS_PAYLOAD_LIMIT);
    }
}

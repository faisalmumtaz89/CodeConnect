//! Push, behind a trait so the daemon runs without an Apple key.
//!
//! Push is a **doorbell, not a data channel**: it names the project a run is in
//! and why it rang — coarse facts, deliberately — and nothing an agent wrote,
//! ran, or changed. The phone reconnects and asks the event log what is true. APNs stores exactly one offline notification per bundle id, so the
//! sender coalesces to an aggregate body and the durable event log stays
//! authoritative. The phone reconciles on foreground via `subscribe(after_seq)`;
//! a dropped push costs a delay, never a fact.

use std::sync::Mutex;

/// One thing worth waking a human for.
///
/// **Typed, not free text.** The alert's words are composed in one place
/// ([`alert`]) from these inputs, so no call site can invent copy: a tool name,
/// a risk class, or the reused `cc-<n>` counter cannot reach a lock screen,
/// because there is no seam through which a caller could put them there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushHint {
    /// What to call the run out loud, or empty when nothing does.
    pub project_label: String,
    /// Which sentence to use. The daemon never passes a sentence; it passes
    /// the *kind*, and the words live with the composer.
    pub kind: PushKind,
    /// How many runs are holding a *decision* — distinct sessions with a
    /// pending approval, which is what `blocked_runs` counts. A run merely
    /// waiting for input is not among them. The alert says "agents", so the
    /// word matches the number.
    pub blocked_sessions: usize,
    /// Which run rang, for the log only. **Never serialised into a payload**: a
    /// ULID's leading bits are the run's start time, and a notification that
    /// carried one would let Apple group a device's pushes into sessions.
    pub session_uid: String,
}

impl PushHint {
    /// The doorbell as it should read at the moment it rings.
    ///
    /// **One slot, so it has to describe the fleet and not just the event that
    /// rang.** A phone holds one CodeConnect notification and a later one
    /// replaces it, so a run finishing a turn would otherwise overwrite an
    /// outstanding decision — and a tap on that lands on the fleet, because
    /// only an approval opens the decision list. While any run is holding a
    /// decision the doorbell is about decisions; only when none is does the
    /// event that rang get to speak for itself.
    ///
    /// **And it must not name the wrong run.** A doorbell that speaks for the
    /// fleet is no longer about the run that triggered it: saying `Ledger` over
    /// `Waiting on an approval` when the decision belongs to `Aion` sends a
    /// reader looking in the wrong place. So the title follows the subject —
    /// the one blocked run when there is exactly one, and nobody in particular
    /// when there are several, where the body is a count rather than a claim
    /// about any single project.
    ///
    /// The count is read at ring time rather than at admission for the same
    /// reason: a number taken before the dispatch grace can describe a fleet
    /// that has since answered everything.
    pub fn describing(&self, blocked: usize, blocked_label: Option<String>) -> PushHint {
        PushHint {
            kind: if blocked > 0 {
                PushKind::Approval
            } else {
                self.kind
            },
            blocked_sessions: blocked,
            project_label: match blocked {
                0 => self.project_label.clone(),
                1 => blocked_label.unwrap_or_default(),
                // Several. The body is "N agents need you", and a title naming
                // one of them would be picking a favourite.
                _ => String::new(),
            },
            session_uid: self.session_uid.clone(),
        }
    }
}

/// Why the doorbell rang. One of these, never the agent's own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushKind {
    Approval,
    NeedsInput,
    Completed,
    Idle,
}

impl PushKind {
    /// The stable, machine-readable form, for deciding where a tap lands.
    ///
    /// **Coarse metadata, not a duplicate.** Under coalescing the body becomes
    /// `{n} agents need you`, so this is not always visible in the alert — it is
    /// one of four words describing why the doorbell rang, and it is deliberate
    /// additional information. What it is *not* is an identifier: it names no
    /// session, no decision and no device, and two notifications carrying the
    /// same word say nothing about whether they concern the same run.
    ///
    /// It is here because only an approval has a card in the phone's decision
    /// list. `Finished a turn` has none and never will, so a tap that always
    /// opened that list would reliably open an empty one — a wrong answer rather
    /// than a stale one.
    pub fn tag(self) -> &'static str {
        match self {
            PushKind::Approval => "approval",
            PushKind::NeedsInput => "input",
            PushKind::Completed => "done",
            PushKind::Idle => "idle",
        }
    }

    /// Every kind, so the payload matrix cannot silently stop covering one.
    ///
    /// A manual list, and therefore a thing to keep in step: `sentence` below is
    /// what the compiler forces you to update, and this is what a reviewer has
    /// to notice. Kept next to it so noticing is easy.
    #[cfg(test)]
    pub const ALL: &'static [PushKind] = &[
        PushKind::Approval,
        PushKind::NeedsInput,
        PushKind::Completed,
        PushKind::Idle,
    ];

    /// The whole vocabulary of a notification body, as a closed set.
    ///
    /// A closed set is the point: it is checkable, and a future caller cannot
    /// widen it by passing a string. Nothing an agent wrote appears here.
    pub fn sentence(self) -> &'static str {
        match self {
            PushKind::Approval => "Waiting on an approval",
            PushKind::NeedsInput => "Waiting for your input",
            PushKind::Completed => "Finished a turn",
            PushKind::Idle => "Waiting for you",
        }
    }
}

/// What one deliberate test delivery came to. Internal twin of the wire's
/// `TestPushResult`; the ws layer does the translation so this module never
/// depends on the protocol crate's wire shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestDelivery {
    /// Apple accepted it; `apns_id` is Apple's own receipt header when present.
    Accepted {
        apns_id: Option<String>,
    },
    /// The sender is the logging stub — no key configured.
    Unconfigured,
    /// The device has no registered token to send to.
    NoToken,
    Failed(String),
}

pub trait PushSender: Send + Sync {
    /// `excluded` is the seen-filter's verdict: devices whose live socket
    /// already delivered the fact this push announces. Computed by the caller
    /// at dispatch time; the sender's only job is to honour it in the fan-out.
    fn send(&self, hint: &PushHint, excluded: &[String]);
    /// One real notification to **one named device**, with the outcome
    /// reported. `send` is deliberately fire-and-forget spray; a test whose
    /// result nobody can see would prove nothing, so this one answers on the
    /// returned channel.
    fn send_test(&self, _device_id: &str) -> tokio::sync::oneshot::Receiver<TestDelivery> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(TestDelivery::Unconfigured);
        rx
    }
    /// **This device is gone**, revoked or refused by Apple as unregistered.
    ///
    /// Told rather than inferred: a sender that worked out who had departed by
    /// comparing the registry on its next send would retire nobody when nothing
    /// rang again, and would read a database that briefly refused to answer as
    /// every device leaving at once.
    fn retire(&self, _device_id: &str) {}
    /// Advertised in `hello_ack` so the phone can tell "no push configured"
    /// from "push failed".
    fn is_live(&self) -> bool {
        false
    }
}

/// The sender used until a `.p8` key exists: it logs what it *would* have sent,
/// including the coalesced body, so the coalescing logic is exercised and
/// inspectable before any Apple credential is configured.
pub struct LoggingPushSender {
    last: Mutex<Option<PushHint>>,
}

impl LoggingPushSender {
    pub fn new() -> Self {
        LoggingPushSender {
            last: Mutex::new(None),
        }
    }

    /// Test hook: the most recent hint.
    #[cfg(test)]
    pub fn last(&self) -> Option<PushHint> {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl PushSender for LoggingPushSender {
    fn send(&self, hint: &PushHint, excluded: &[String]) {
        let (title, body) = alert(hint);
        crate::log_info!(
            "push[stub] project={:?} title={:?} body={:?} excluded={}",
            hint.project_label,
            title,
            body,
            excluded.len()
        );
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(hint.clone());
    }
}

/// The words of one notification, and the only place they are chosen.
///
/// Title is the project and nothing else. `CodeConnect` is the fallback rather
/// than the run's tmux name, because a name like `cc-1` is a counter the next
/// run inherits — it tells a reader nothing, and telling them nothing honestly
/// beats telling them something meaningless.
///
/// Body is a canned sentence picked by [`PushKind`], or the count once more
/// than one session is blocked: with four waiting, which one rang is not the
/// useful fact and the lock screen has no room for the rest.
///
/// **What is deliberately absent**: the tool name, the risk class, the command,
/// any path, and any identifier. Those either say what an agent is doing to
/// whoever reads the payload, or say which run it is in a form that survives
/// beyond the notification.
pub fn alert(hint: &PushHint) -> (String, String) {
    let title = if hint.project_label.is_empty() {
        "CodeConnect".to_string()
    } else {
        hint.project_label.clone()
    };
    let body = match hint.blocked_sessions {
        0 | 1 => hint.kind.sentence().to_string(),
        n => format!("{n} agents need you"),
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(blocked: usize) -> PushHint {
        PushHint {
            project_label: "Aion".into(),
            kind: PushKind::Approval,
            blocked_sessions: blocked,
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
        }
    }

    #[test]
    fn one_blocked_session_is_named_by_its_project_and_says_what_it_wants() {
        assert_eq!(
            alert(&hint(1)),
            ("Aion".into(), "Waiting on an approval".into())
        );
        assert_eq!(
            alert(&hint(0)),
            ("Aion".into(), "Waiting on an approval".into())
        );
    }

    #[test]
    fn several_blocked_sessions_coalesce_to_the_count() {
        assert_eq!(alert(&hint(3)).1, "3 agents need you");
    }

    /// **A doorbell never announces the lesser fact while a decision waits.**
    ///
    /// One slot means the last push replaces the rest, so a completion landing
    /// while an approval is outstanding would take the decision's place and
    /// send a tap to the fleet. Whatever rang, the doorbell says decisions
    /// while there are decisions.
    #[test]
    fn a_doorbell_speaks_for_the_fleet_while_any_decision_waits() {
        for &rang in PushKind::ALL {
            let quiet = hint(0);
            assert_eq!(
                quiet.describing(0, None).kind,
                quiet.kind,
                "with nothing waiting, the event that rang speaks for itself"
            );

            let mut ambient = hint(0);
            ambient.kind = rang;
            assert_eq!(
                ambient.describing(1, Some("Aion".into())).kind,
                PushKind::Approval,
                "{rang:?} must not replace an outstanding decision with a dead end"
            );
            assert_eq!(
                alert(&ambient.describing(1, Some("Aion".into()))).1,
                "Waiting on an approval"
            );
            assert_eq!(alert(&ambient.describing(3, None)).1, "3 agents need you");
        }
    }

    /// **A doorbell that speaks for the fleet must not name the wrong run.**
    ///
    /// `Ledger` finishing a turn while `Aion` holds a decision produces an
    /// approval doorbell — and titling it `Ledger` sends the reader to the
    /// wrong project to look for a card that is not there.
    #[test]
    fn a_doorbell_names_the_run_it_is_actually_about() {
        let mut ledger = hint(0);
        ledger.project_label = "Ledger".into();
        ledger.kind = PushKind::Completed;

        let speaking = ledger.describing(1, Some("Aion".into()));
        assert_eq!(
            alert(&speaking),
            ("Aion".into(), "Waiting on an approval".into()),
            "the decision's project, not the one that happened to ring"
        );

        // Several blocked runs: the body is a count, so the title claims
        // nothing about any one of them.
        assert_eq!(
            alert(&ledger.describing(3, None)),
            ("CodeConnect".into(), "3 agents need you".into())
        );

        // Nothing blocked: the run that rang is the subject, and keeps its name.
        assert_eq!(
            alert(&ledger.describing(0, None)),
            ("Ledger".into(), "Finished a turn".into())
        );
    }

    /// A run whose `cwd` names no project is not given a counter to wear.
    #[test]
    fn a_run_with_no_project_falls_back_to_the_app_and_never_to_a_handle() {
        let mut unnamed = hint(1);
        unnamed.project_label = String::new();
        assert_eq!(alert(&unnamed).0, "CodeConnect");
    }

    /// **The literals, independently.** The payload matrix builds both sides
    /// from `tag()`, so renaming `"approval"` to `"approve"` would leave every
    /// Rust and Swift test green while every approval tap landed on the fleet.
    /// These are the four words the phone matches on; they are a wire contract,
    /// not an implementation detail.
    #[test]
    fn the_routing_tags_are_pinned_literals() {
        assert_eq!(PushKind::Approval.tag(), "approval");
        assert_eq!(PushKind::NeedsInput.tag(), "input");
        assert_eq!(PushKind::Completed.tag(), "done");
        assert_eq!(PushKind::Idle.tag(), "idle");
    }

    /// Every sentence a notification can contain, pinned. Widening this set is
    /// a deliberate act, not something a call site can do by passing a string.
    #[test]
    fn the_body_vocabulary_is_closed() {
        let sentences: Vec<&str> = PushKind::ALL.iter().map(|k| k.sentence()).collect();
        assert_eq!(
            sentences,
            vec![
                "Waiting on an approval",
                "Waiting for your input",
                "Finished a turn",
                "Waiting for you",
            ]
        );
    }

    #[test]
    fn stub_records_but_reports_itself_as_not_live() {
        let sender = LoggingPushSender::new();
        assert!(!sender.is_live());
        sender.send(&hint(2), &[]);
        assert_eq!(sender.last().unwrap().blocked_sessions, 2);
    }
}

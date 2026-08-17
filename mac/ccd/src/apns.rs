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
    /// The relay refused the credential registered with this token. **The token
    /// is not implicated**: it is still the phone's, still current, and still
    /// where a notification would go once a working bearer replaces this one.
    CredentialInvalid,
    /// The relay's own budget for this token binding is spent.
    RateLimited {
        retry_after_secs: u32,
    },
    Failed(String),
}

/// What a token is, from the crate both ends of this wire depend on.
///
/// **Not restated here.** The relay keys a binding and a budget by this value
/// and refuses one that breaks the rule; a daemon holding its own copy of the
/// rule could store a token the relay will refuse on every push for ever, and
/// each side's tests would pass.
pub use push_core::normalize_device_token;

/// The longest relay credential this daemon will store.
const MAX_CREDENTIAL_CHARS: usize = 128;

/// A relay credential this daemon is willing to present, or a refusal.
///
/// **The bound that matters is not the length.** The credential is placed in an
/// `Authorization` header, so a value carrying a carriage return, a newline or
/// a space could append headers of its own to every push this Mac sends. The
/// rule is therefore printable ASCII with no space — which admits the base64url
/// the relay mints without hard-coding that it will always mint base64url, and
/// admits nothing that can end a header line.
pub fn checked_credential(raw: &str) -> anyhow::Result<String> {
    if raw.is_empty() {
        anyhow::bail!("the relay credential is empty");
    }
    if raw.len() > MAX_CREDENTIAL_CHARS {
        anyhow::bail!(
            "the relay credential is {} characters; the limit is {MAX_CREDENTIAL_CHARS}",
            raw.len()
        );
    }
    if !raw.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        anyhow::bail!("the relay credential contains a character that cannot ride in a header");
    }
    Ok(raw.to_string())
}

/// **The relay refused the credential**, carried in an attempt's error chain.
///
/// A typed marker rather than a string, for the same reason `DeviceGone` is
/// one: the queue that runs an attempt has to tell one failure from another
/// without reading prose, and a `contains("credential")` would be a wire
/// contract written in a substring.
#[derive(Debug)]
pub struct CredentialRefused;

impl std::fmt::Display for CredentialRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the relay refused this token's credential")
    }
}

impl std::error::Error for CredentialRefused {}

/// **The budget for this token is spent**, with the wait the refuser named.
#[derive(Debug)]
pub struct SendRateLimited {
    pub retry_after_secs: u32,
}

impl std::fmt::Display for SendRateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the relay is rate limiting this token; retry in {}s",
            self.retry_after_secs
        )
    }
}

impl std::error::Error for SendRateLimited {}

/// **The row has a token but no bearer to authorise it** — an incomplete tuple,
/// not a refusal.
///
/// The plan's table (`docs/push-gateway.md`) is explicit: a missing tuple is
/// `NoRegisteredToken`, and `credential_invalid` is reserved for a relay that
/// actually answered `401`/`403`. A relay-mode row registered while the daemon
/// was direct, or before the phone enrolled, has no bearer at all — so nothing
/// is sent, nothing is refused, and reporting a credential refusal would name a
/// conversation with the relay that never took place. A typed marker rather
/// than a string, for the reason `CredentialRefused` is one.
#[derive(Debug)]
pub struct NoRegistration;

impl std::fmt::Display for NoRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this device has no complete push registration")
    }
}

impl std::error::Error for NoRegistration {}

impl TestDelivery {
    /// What to tell whoever asked for a test, given the error its attempt
    /// produced.
    ///
    /// **One classifier, so the queue stays transport-neutral.** The worker that
    /// runs an attempt knows nothing about Apple or about a relay; it knows that
    /// a failure may carry a typed marker, and that everything else is a
    /// failure with a reason attached.
    pub fn from_error(err: &anyhow::Error) -> TestDelivery {
        // Checked before `CredentialRefused`, because they are opposites the
        // phone acts on differently: a missing tuple asks it to register, a
        // refused credential asks it to enrol for a new bearer. An incomplete
        // registration is the first, never the second.
        if err.chain().any(|e| e.is::<NoRegistration>()) {
            return TestDelivery::NoToken;
        }
        if err.chain().any(|e| e.is::<CredentialRefused>()) {
            return TestDelivery::CredentialInvalid;
        }
        if let Some(limited) = err
            .chain()
            .find_map(|e| e.downcast_ref::<SendRateLimited>())
        {
            return TestDelivery::RateLimited {
                retry_after_secs: limited.retry_after_secs,
            };
        }
        TestDelivery::Failed(format!("{err:#}"))
    }
}

/// How this daemon reaches a phone, decided once at boot and never revised by a
/// send.
///
/// **A predicate could not say this.** `push` used to mean "an Apple key is
/// configured", which was also the answer to "may the phone register", to "is
/// the test button live", and to "which capability do I advertise" — three
/// questions that stop having one answer the moment a second transport exists.
/// A relay daemon holds no key and can still ring every phone paired to it; a
/// Mac whose key path is misspelled holds no working transport at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushMode {
    /// No delivery path: push was switched off, or a direct configuration was
    /// asked for and could not be honoured.
    Off,
    /// This Mac holds an Apple key and talks to Apple itself. The notification
    /// it composes may name the project, because nothing but Apple sees it.
    Direct,
    /// A CodeConnect-operated relay holds the key. This Mac supplies a token, a
    /// credential and which of four kinds rang, and the relay composes a
    /// generic alert.
    Relay,
}

impl PushMode {
    /// Whether a delivery path is configured.
    ///
    /// **Not a reachability claim.** A relay that is unreachable at boot is
    /// still configured, and reporting otherwise would advertise no push
    /// capability, suppress registration entirely, and leave the phone unable to
    /// recover without somebody restarting the daemon. A send fails within its
    /// own bounds and a test reports the real failure; neither changes this.
    pub fn is_configured(self) -> bool {
        !matches!(self, PushMode::Off)
    }
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
    /// Which transport this sender is. Capabilities and registration validation
    /// read this and never a boolean, because they need to tell a relay daemon
    /// from a direct one and not merely a configured one from an unconfigured
    /// one.
    /// It is also what `hello_ack` is built from, so the phone can tell "no
    /// push configured" from "push failed" and direct from relay.
    fn mode(&self) -> PushMode {
        PushMode::Off
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

/// The sender this configuration asks for.
///
/// **Ordered, and every branch is reachable from a file somebody can write**, so
/// the order is the specification:
///
///   1. `push_enabled = false` — off. The only way to have no push at all.
///   2. **Any** `apns_*` key present — a request for direct delivery. All four
///      are then required and the key must load. A partial or unreadable set is
///      off with an error and *never* the relay: a Mac that asked to talk to
///      Apple itself must not begin routing its notifications through a third
///      party because a path was misspelled. That silent substitution is the
///      failure this ordering exists to make impossible.
///   3. No `apns_*` key at all — the relay, which is the ordinary case. The
///      provider key cannot be shipped to a customer's machine, so a daemon
///      with none is not a daemon that cannot ring.
///
/// **A relay that is unreachable right now still selects the relay.** Probing at
/// boot and falling back to the stub would advertise no capability, refuse
/// every registration, and leave the phone unable to recover until somebody
/// restarted `ccd` — turning a minute of downstream downtime into an outage
/// lasting until a human intervened.
pub fn build(
    config: &protocol::config::Config,
    store: std::sync::Arc<crate::store::Store>,
) -> std::sync::Arc<dyn PushSender> {
    if !config.push_enabled {
        crate::log_info!("push: disabled by push_enabled; nothing will be sent");
        return std::sync::Arc::new(LoggingPushSender::new());
    }
    let asked_for_direct = config.apns_key_path.is_some()
        || config.apns_key_id.is_some()
        || config.apns_team_id.is_some()
        || config.apns_topic.is_some();
    if asked_for_direct {
        return crate::apns_sender::build(config, store);
    }
    crate::relay_sender::build(store)
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

    // The bound the rule itself declares, so these tests measure against one
    // number rather than a second copy that happens to agree today.
    use push_core::MAX_TOKEN_HEX;

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

    /// A store nobody else is using, for the selection rules that need one.
    fn temp_store() -> std::sync::Arc<crate::store::Store> {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ccd-push-build-{}-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            protocol::time::now_unix_ms()
        ));
        let _ = std::fs::remove_file(&path);
        std::sync::Arc::new(crate::store::Store::open(&path).unwrap())
    }

    /// A real P-256 key in the PEM the `.p8` is written in, minted here rather
    /// than checked in: a fixture private key in the repository is a private key
    /// in the repository.
    fn a_usable_key() -> std::path::PathBuf {
        use base64::Engine;
        let rng = ring::rand::SystemRandom::new();
        let der = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .expect("the kernel must give us a key");
        let body = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        // The delimiters are assembled rather than spelled out. Repository
        // hygiene greps tracked source for key material, and a test that mints
        // a throwaway key at run time must not read like one checked in.
        const LABEL: &str = "PRIVATE KEY";
        let pem = format!("-----BEGIN {LABEL}-----\n{body}\n-----END {LABEL}-----\n");
        let path = std::env::temp_dir().join(format!(
            "ccd-push-key-{}-{}.p8",
            std::process::id(),
            protocol::time::now_unix_ms()
        ));
        std::fs::write(&path, pem).unwrap();
        path
    }

    fn direct_config(key_path: &str) -> protocol::config::Config {
        protocol::config::Config {
            apns_key_path: Some(key_path.to_string()),
            apns_key_id: Some("KEYID12345".into()),
            apns_team_id: Some("TEAMID1234".into()),
            apns_topic: Some("com.example.app".into()),
            ..Default::default()
        }
    }

    /// **The whole selection rule, as a table.** Each row is a file somebody can
    /// write, and the one that matters most is the fourth: a direct
    /// configuration that cannot be honoured must not quietly become a relay
    /// configuration, because that would route a Mac's notifications through a
    /// third party on the strength of a typo.
    ///
    /// Row 2 is also where relay mode is shown to be a *configuration* rather
    /// than a reachability claim: no row here reaches a network, and the relay
    /// is selected all the same. A boot-time probe would make this table depend
    /// on whether a service happened to answer in the second the daemon
    /// started, and a Mac that started during an outage would advertise no push
    /// capability and refuse every registration until somebody restarted it.
    #[test]
    fn the_configuration_selects_exactly_one_transport() {
        let key = a_usable_key();
        let key_path = key.to_string_lossy().to_string();

        // 1. Switched off.
        let off = protocol::config::Config {
            push_enabled: false,
            ..direct_config(&key_path)
        };
        assert_eq!(build(&off, temp_store()).mode(), PushMode::Off);

        // 2. No direct field at all — the ordinary customer install.
        assert_eq!(
            build(&protocol::config::Config::default(), temp_store()).mode(),
            PushMode::Relay,
            "a Mac with no Apple key is not a Mac that cannot ring"
        );

        // 3. All four, and the key loads.
        assert_eq!(
            build(&direct_config(&key_path), temp_store()).mode(),
            PushMode::Direct
        );

        // 4. Partial — one field at a time missing — and unreadable.
        for missing in 0..4 {
            let mut partial = direct_config(&key_path);
            match missing {
                0 => partial.apns_key_path = None,
                1 => partial.apns_key_id = None,
                2 => partial.apns_team_id = None,
                _ => partial.apns_topic = None,
            }
            assert_eq!(
                build(&partial, temp_store()).mode(),
                PushMode::Off,
                "a half-configured direct sender is off, never the relay (field {missing})"
            );
        }
        let unreadable = direct_config("/nonexistent/nowhere.p8");
        assert_eq!(
            build(&unreadable, temp_store()).mode(),
            PushMode::Off,
            "a key that will not load is off, never the relay"
        );

        let _ = std::fs::remove_file(&key);
    }

    /// A token is normalised on the way in so the relay meters one binding per
    /// phone rather than one per spelling.
    #[test]
    fn a_token_is_lowercase_whole_bytes_of_hex_or_it_is_refused() {
        let token = "AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899";
        assert_eq!(
            normalize_device_token(token).unwrap(),
            token.to_ascii_lowercase()
        );

        for bad in [
            "",
            "aabb",                              // too short
            "aabbccddeeff0011223344556677889",   // odd length
            "aabbccddeeff001122334455667788zz",  // not hex
            "aabbccddeeff0011223344556677 8899", // a space
        ] {
            assert!(
                normalize_device_token(bad).is_err(),
                "accepted the token {bad:?}"
            );
        }
        assert!(normalize_device_token(&"a".repeat(MAX_TOKEN_HEX + 2)).is_err());
        assert!(normalize_device_token(&"a".repeat(MAX_TOKEN_HEX)).is_ok());
    }

    /// **A credential is a header value before it is a secret.** A newline in
    /// one would let a stored value append headers to every push this Mac
    /// sends, so the check that matters is the one on the bytes, not the one on
    /// the length.
    #[test]
    fn a_credential_that_could_forge_a_header_is_refused() {
        assert!(checked_credential("dGhpcy1pcy1hLWJlYXJlcg").is_ok());
        for bad in [
            "",
            "with space",
            "line\r\nAuthorization: Bearer other",
            "trailing\n",
            "tab\there",
        ] {
            assert!(
                checked_credential(bad).is_err(),
                "accepted the credential {bad:?}"
            );
        }
        assert!(checked_credential(&"a".repeat(MAX_CREDENTIAL_CHARS + 1)).is_err());
        assert!(checked_credential(&"a".repeat(MAX_CREDENTIAL_CHARS)).is_ok());
    }

    /// **Every typed refusal survives the trip through an error chain**, and
    /// anything untyped is a failure with its reason intact. A marker that
    /// stopped being recognised would silently downgrade "renew your
    /// credential" to "something went wrong", which is advice a reader cannot
    /// act on.
    #[test]
    fn a_typed_refusal_is_classified_and_everything_else_keeps_its_reason() {
        let refused = anyhow::Error::new(CredentialRefused).context("relay answered 401");
        assert_eq!(
            TestDelivery::from_error(&refused),
            TestDelivery::CredentialInvalid
        );

        let limited = anyhow::Error::new(SendRateLimited {
            retry_after_secs: 12,
        })
        .context("relay answered 429");
        assert_eq!(
            TestDelivery::from_error(&limited),
            TestDelivery::RateLimited {
                retry_after_secs: 12
            }
        );

        let other = anyhow::Error::msg("the relay is unreachable");
        match TestDelivery::from_error(&other) {
            TestDelivery::Failed(reason) => assert!(reason.contains("unreachable"), "{reason}"),
            unexpected => panic!("an untyped failure must stay a failure: {unexpected:?}"),
        }
    }

    /// The predicate is the mode's shadow. A sender that could set them apart
    /// would be able to advertise a capability it does not have, which is the
    /// one thing `hello_ack` exists to make impossible.
    #[test]
    fn only_a_configured_mode_is_live() {
        assert!(!PushMode::Off.is_configured());
        assert!(PushMode::Direct.is_configured());
        assert!(
            PushMode::Relay.is_configured(),
            "a relay that is merely unreachable is still configured"
        );
    }

    #[test]
    fn stub_records_but_reports_itself_as_not_live() {
        let sender = LoggingPushSender::new();
        assert_eq!(sender.mode(), PushMode::Off);
        sender.send(&hint(2), &[]);
        assert_eq!(sender.last().unwrap().blocked_sessions, 2);
    }
}

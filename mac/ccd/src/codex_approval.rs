//! **A Codex approval request, read off the wire and turned into a card.**
//!
//! Two request families reach a subscribed ccd leg and are worth a human's
//! attention: `item/commandExecution/requestApproval` and
//! `item/fileChange/requestApproval`. A third,
//! `item/permissions/requestApproval`, returns a permission *profile* rather
//! than a yes/no about one action; it is observed and never carded, so it has
//! no arm here.
//!
//! # The wire wins over the bundle label
//!
//! `availableDecisions` is declared only in the app-server's **experimental**
//! schema bundle, in both 0.147 and 0.153 — and it is on the live **stable**
//! wire, populated, with no `--experimental` flag anywhere in the launch. The
//! generator's stable/experimental split describes how the field is *labelled*,
//! not whether the server sends it, so this parser reads the frame in hand and
//! never the bundle. What the bundle is used for is the opposite direction:
//! every key read below is one the bundle declares, so an unknown key cannot be
//! silently believed.
//!
//! # Why the card looks like a Claude card
//!
//! The phone recomputes `SHA-256(display_text)` and compares it to
//! `payload_hash`; on a mismatch the card is replaced by a banner and both
//! actions die (`ios/CodeConnect/Model/CardVerification.swift`,
//! `Views/DecisionCard.swift`). It then separately checks that
//! `"{tool_name}\n{tool_input}"` reproduces `display_text`, and falls back to
//! printing `display_text` raw when it does not. So the only shape that renders
//! as a card at all is the one the Claude path already produces, and the way to
//! make the hash cover the command, the cwd, the reason **and** the option set —
//! which is what stops a stale card being answered against a different option
//! set — is to put all four inside `tool_input` and hash the whole thing. That
//! is what [`Approval::card`] does, and it is why there is no second hash
//! function: [`protocol::hash::approval_payload_hash`] already hashes exactly
//! this preimage.
//!
//! # Every field on the wire is ruled semantic or not, and the ruling is here
//!
//! A field the hash does not cover is a field that can change without changing
//! the card, so the question for each is not "is it read" but "could an answer
//! mean something different because of it". Every key either family carries on
//! the live 0.153 wire:
//!
//! * `command`, `cwd` (command) — **semantic**: what would run, and where.
//!   Hashed.
//! * `reason` (both) — **semantic**: the sentence a human reads first. Hashed
//!   when present; measured populated on 0.153 command requests and `null` on
//!   file changes.
//! * `availableDecisions` (command) — **semantic**: the set an answer may name,
//!   object bodies included. Hashed.
//! * `proposedExecpolicyAmendment` (command) — **semantic**: the argv an "and
//!   don't ask again" would whitelist, so it is part of what *accepting grants*.
//!   Hashed. It has been byte-equal to the `acceptWithExecpolicyAmendment` body
//!   on all three measured frames, which is an observation about three frames
//!   and not a declared invariant — hashing it costs one key and needs no
//!   invariant to be true.
//! * `grantRoot` (fileChange) — **semantic** for the same reason: it names the
//!   root an "accept for these files" would grant. Measured `null` on every
//!   capture, so it changes no card today and is hashed the day it is not.
//! * `changes[]` (fileChange) — **semantic**: the whole content, joined from the
//!   preceding `item/started`. Hashed, bounded, and what a bound omits it still
//!   commits to — see [`bounded`].
//! * `environmentId` (command) — **semantic, and pinned rather than hashed.** It
//!   names where the command runs, so an approval for another environment is a
//!   different question — and one this build has never measured and could not
//!   describe on a card. It is pinned to the **literal** `"local"` and refused
//!   otherwise ([`Refusal::UnmeasuredEnvironment`]), which is the 2e-7c rule: an
//!   unmeasured capability is refused, not guessed. Pinned to one value it is a
//!   constant, and a constant secures nothing inside a hash.
//!
//!   **Absence is refused too, and that is the measurement talking.** The bundle
//!   says `"default": null`, but a default in a JSON schema is a statement about
//!   the encoding and not about where a command runs — it never says `null`
//!   *means* local. What the captures say is unambiguous: every
//!   `commandExecution` approval carries the string `"local"`, four for four
//!   across 0.147 and 0.153, so a frame without it is a shape the wire has never
//!   produced. The file-change family carries the field on neither release,
//!   which is why the pin lives in the command arm alone rather than being a
//!   rule about approvals in general.
//! * `commandActions` (command) — **non-semantic**: a parsed view *of*
//!   `command` (measured `[{type, command}]`), and `command` is the authority
//!   both the card and the risk class are taken from. The two disagreeing would
//!   change no fact about what runs.
//! * `kind` (command) — **non-semantic**: the family, which is already the
//!   frame's `method` and already the card's `tool_name`.
//! * `startedAtMs` (both) — **non-semantic**: when the item began. It orders
//!   frames; it decides nothing about the question.
//! * `threadId`, `turnId`, `itemId` (both) — **identity, not content**: what the
//!   card is filed and retired under, and they ride the `request_id` a phone
//!   echoes back rather than the payload hash.

use protocol::composite_id::{CompositeId, ServerRequestId};
use serde_json::{Map, Value};

/// `item/commandExecution/requestApproval` — a shell command wants to run.
pub(crate) const COMMAND_METHOD: &str = "item/commandExecution/requestApproval";
/// `item/fileChange/requestApproval` — an edit wants to be written.
pub(crate) const FILE_CHANGE_METHOD: &str = "item/fileChange/requestApproval";

/// How much of one command may ride in a card.
///
/// A card is something a person reads on a phone, and the event it rides in is
/// capped at `max_payload_bytes` (512 KiB by default) by
/// `Daemon::truncate_payload`, which does not trim the card — it replaces the
/// whole payload with a preview object, and the phone then has no card to
/// decode at all. Cutting here instead keeps the card a card, and the cut is
/// made **before** the hash so the text on the phone is still provably the text
/// that was hashed.
const MAX_COMMAND_BYTES: usize = 8 * 1024;
/// The same ceiling for one file's diff.
const MAX_DIFF_BYTES: usize = 16 * 1024;
/// And a ceiling on how many files one card describes, so a thousand-file patch
/// is still a card rather than a truncated payload.
const MAX_CHANGES: usize = 32;
/// **What every diff on one card gets to spend between them.**
///
/// The per-file ceiling alone does not bound a card, and that was the hole:
/// `MAX_CHANGES * MAX_DIFF_BYTES` is 512 KiB before any other field, the changes
/// ride the payload *twice* (once as `tool_input`, once JSON-escaped inside
/// `display_text`), and the measured result was a 1,056,469-byte payload against
/// a 524,288-byte limit — with the cliff at **16** maximal diffs, not 32. So the
/// budget is shared: one file still gets the whole per-file ceiling, and
/// thirty-two get an equal share of this.
const MAX_TOTAL_DIFF_BYTES: usize = 128 * 1024;
/// A ceiling on the model's sentence about why it is asking. Nothing on the wire
/// bounds it, and it rides the payload twice like everything else in
/// `tool_input`; four kilobytes is far more prose than a card can show.
const MAX_REASON_BYTES: usize = 4 * 1024;
/// The only `environmentId` any capture has ever carried, and therefore the only
/// one a card may describe. See the field rulings in this module's header.
const LOCAL_ENVIRONMENT: &str = "local";

/// Which request family this is.
///
/// **Two, not one, and they do not share an option table.** The command family
/// reads its options off the wire; the file-change family has none on the wire
/// at all and is offered a different set of words by the TUI. A single table
/// would have to describe both and would be wrong about each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Command,
    FileChange,
}

impl Family {
    pub(crate) fn of_method(method: &str) -> Option<Family> {
        match method {
            COMMAND_METHOD => Some(Family::Command),
            FILE_CHANGE_METHOD => Some(Family::FileChange),
            _ => None,
        }
    }

    /// The name the store column carries, and the name the log says out loud.
    ///
    /// It is deliberately the app-server's own `item.type` spelling, which is
    /// what makes [`Family::of_item_type`] the exact inverse of this and lets an
    /// item's terminal be matched against a card's family without a second
    /// vocabulary to keep in step.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Family::Command => "commandExecution",
            Family::FileChange => "fileChange",
        }
    }

    /// Which family an `item.type` belongs to, if it is one a card is ever
    /// raised for.
    ///
    /// The cheap question the retirement path asks before it reads anything: an
    /// `item/completed` for a `reasoning` or an `agentMessage` can retire no
    /// card, and those are most of the frames a turn produces.
    pub(crate) fn of_item_type(item_type: &str) -> Option<Family> {
        match item_type {
            "commandExecution" => Some(Family::Command),
            "fileChange" => Some(Family::FileChange),
            _ => None,
        }
    }

    /// What the phone prints at the top of the card, verbatim.
    ///
    /// **Measured, not chosen for effect.** The app has no mapping table for
    /// `tool_name`: it renders the string raw as the card's title and switches
    /// on Claude's exact tool names only for the icon, the risk floor and the
    /// deny-consequence sentence (`ios/CodeConnect/Protocol/PayloadViews.swift`,
    /// `Views/DecisionCard.swift`). Borrowing `"Bash"` would buy a terminal
    /// glyph at the price of a title that names a tool Codex does not have, so
    /// these say what the request is. Phase 5 owns what the app makes of them.
    fn tool_name(self) -> &'static str {
        match self {
            Family::Command => "command",
            Family::FileChange => "file change",
        }
    }

    /// The decisions this family offers, and the words the TUI puts on them.
    ///
    /// **Pane-measured while a real decision was up**, because for the
    /// file-change family there is nowhere else to read them from: its request
    /// declares no `availableDecisions` in either vendored bundle and carries
    /// none on the live wire. The command family's *set* does come off the wire
    /// — this table only supplies its words.
    fn labels(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Family::Command => &[
                ("accept", "Yes, proceed"),
                (
                    "acceptWithExecpolicyAmendment",
                    "Yes, and don't ask again for this command",
                ),
                ("cancel", "No, and tell Codex what to do differently"),
            ],
            Family::FileChange => &[
                ("accept", "Yes, proceed"),
                (
                    "acceptForSession",
                    "Yes, and don't ask again for these files",
                ),
                ("cancel", "No, and tell Codex what to do differently"),
            ],
        }
    }

    fn label_for(self, id: &str) -> Option<&'static str> {
        self.labels()
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, label)| *label)
    }
}

/// One decision a human is being offered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Choice {
    /// The canonical serialization of the decision: the bare string for a
    /// string decision, and the single object key for an object one. This is
    /// what an answer names, and it is deliberately *not* the whole decision —
    /// see `payload`.
    pub(crate) id: String,
    pub(crate) label: &'static str,
    /// The object decision's own body, verbatim off the wire.
    ///
    /// **Carried opaquely and never re-derived.** An
    /// `acceptWithExecpolicyAmendment` names the exact argv the server is
    /// offering to whitelist. An answer path reconstructs the wire decision
    /// from *this* value, never from anything a phone supplies, so a phone can
    /// only ever pick between amendments the server itself proposed. It is
    /// inside `tool_input`, so it is hashed with everything else — a card that
    /// went stale cannot be answered against a widened amendment — and it is
    /// visible in the phone's "Full tool input", so what a "don't ask again"
    /// would actually grant is on screen rather than implied.
    pub(crate) payload: Option<Value>,
}

impl Choice {
    fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("id".into(), Value::String(self.id.clone()));
        out.insert("label".into(), Value::String(self.label.into()));
        if let Some(payload) = &self.payload {
            out.insert("payload".into(), payload.clone());
        }
        Value::Object(out)
    }
}

/// The context a card is built from, once the family is known.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Context {
    /// Everything a command request carries itself. `cwd` is nullable in the
    /// bundle and was populated on every live 0.153 frame; `reason` was
    /// populated on 0.153 and absent on 0.147, so it is optional by measurement
    /// rather than by caution.
    ///
    /// `command` is **not** optional here, though the bundle marks it nullable:
    /// it is the whole of what is being approved, and it is held verbatim off
    /// the wire rather than trimmed, because the risk class and the hash are
    /// both taken from what would actually run. The projection the phone is
    /// shown is built from it in [`Approval::tool_input`].
    Command {
        command: String,
        cwd: Option<String>,
        reason: Option<String>,
        /// `proposedExecpolicyAmendment`, verbatim — the argv an "and don't ask
        /// again" would whitelist, and therefore part of what accepting grants.
        amendment: Option<Value>,
    },
    /// **What the request does not carry.** A `fileChange` request names an
    /// item and nothing else — measured `reason: null`, `grantRoot: null`, no
    /// content of any kind — so the changes come from the `item/started` that
    /// precedes it, joined on `itemId`.
    ///
    /// Every change the snapshot listed, not the first few: the projection is
    /// [`Approval::tool_input`]'s job, and a card that drops files has to be
    /// able to say how many it dropped.
    FileChange {
        changes: Vec<Value>,
        /// Measured `null` on every capture of this family, and read anyway:
        /// the day it is populated it is the sentence a human reads first, and
        /// a field ruled semantic has to be in the hash before it arrives, not
        /// after.
        reason: Option<String>,
        /// `grantRoot`, likewise measured `null` everywhere — the root an
        /// "accept for these files" would grant.
        grant_root: Option<String>,
    },
}

/// A `*/requestApproval` this daemon is prepared to raise a card for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Approval {
    pub(crate) family: Family,
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) item_id: String,
    pub(crate) context: Context,
    pub(crate) choices: Vec<Choice>,
}

/// Why a frame that looked like an approval did not become one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A required identity field was missing or empty. The bundle marks
    /// `itemId`, `threadId` and `turnId` required in both releases, so a frame
    /// without them is malformed rather than merely old.
    Malformed(&'static str),
    /// The request named no action.
    ///
    /// For a `fileChange`, an `item/started` this connection never saw, so
    /// there is nothing to show. For a command, a `command` that is absent,
    /// `null` or empty — which the bundle permits and which would card a
    /// question reading only `cwd` and three buttons. Both are the same
    /// refusal because they are the same failure: carding it would put "approve
    /// something" in front of a human, which is worse than not carding it. The
    /// store is the witness for a card raised before a reconnect, and this is
    /// the only other way to reach here.
    NoContent,
    /// Every decision the wire offered is one this build has no words for.
    /// Offering an unlabelled button is worse than offering none.
    NoLabelledChoice,
    /// The command would run somewhere this build has never measured.
    ///
    /// `environmentId` was the literal `"local"` on every captured
    /// `commandExecution` request, on 0.147 and on 0.153 alike, and nothing in
    /// this daemon knows how to say "this runs in *that* environment" on a card
    /// — so an approval naming another one is a question the phone would render
    /// as though it were the local one. **Absence is refused on the same terms
    /// as a different value**: the bundle's `"default": null` is a statement
    /// about the JSON, not about where a command runs, and no capture has ever
    /// omitted the field for this family. Refused for the reason 2e-7c refuses
    /// an unmeasured `thread/start` key: a capability nobody has measured is not
    /// a capability this build may quietly exercise.
    UnmeasuredEnvironment,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Malformed(field) => write!(f, "the required field {field} was missing"),
            Refusal::NoContent => write!(
                f,
                "the request named no action: a command approval with no command, or a \
                 file change whose item/started this connection never observed"
            ),
            Refusal::NoLabelledChoice => {
                write!(f, "the wire offered no decision this build can name")
            }
            Refusal::UnmeasuredEnvironment => write!(
                f,
                "the request does not name environmentId {LOCAL_ENVIRONMENT:?}, which is the \
                 only place this build has ever measured a command running and the only one \
                 it can honestly describe on a card"
            ),
        }
    }
}

fn non_empty(params: &Value, key: &'static str) -> Result<String, Refusal> {
    match params.get(key).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        _ => Err(Refusal::Malformed(key)),
    }
}

/// A nullable string field: absent, `null` and `""` are all "not said".
fn optional(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Cut on a character boundary, saying so **and committing to what was cut**.
///
/// `String::truncate` panics on any other byte, and these are exactly the
/// fields that carry multi-byte text — a path with an accent, a diff with an
/// emoji — which is the same reasoning `Daemon::truncate_payload` records.
///
/// # Why the marker carries a digest
///
/// The marker rides inside `tool_input`, so it is inside `display_text` and
/// inside the `payload_hash` the phone recomputes. Without the digest the
/// marker names only a *length*, and two texts sharing a prefix and a length
/// produce a byte-identical projection — measured: a command ending
/// `; touch /tmp/ab` and one ending `; rm -rf /tmp/x`, both 8213 bytes with the
/// same 8192-byte prefix, hashed to the same `payload_hash`. The card was then
/// a statement about neither of them. With the digest the hash commits to every
/// byte the app-server proposed, whether or not the phone is shown it.
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}… ({} bytes elided; sha256 of the whole text {})",
        &text[..end],
        text.len() - end,
        protocol::hash::sha256_hex(text.as_bytes())
    )
}

/// The canonical id of one decision, and its body when it has one.
///
/// The six decisions the bundle declares are three bare strings (`accept`,
/// `acceptForSession`, `decline`, `cancel` — four) and two single-key objects
/// (`acceptWithExecpolicyAmendment`, `applyNetworkPolicyAmendment`). Anything
/// else is not a decision this build can name.
fn decision(value: &Value) -> Option<(String, Option<Value>)> {
    match value {
        Value::String(id) => Some((id.clone(), None)),
        Value::Object(map) if map.len() == 1 => {
            let (id, body) = map.iter().next()?;
            Some((id.clone(), Some(body.clone())))
        }
        _ => None,
    }
}

/// **The wire decision a phone's `option_id` names, taken from the card itself.**
///
/// The inverse of [`decision`], and the reason [`Choice`] keeps `id` and `payload`
/// apart: an id alone is not a decision the app-server accepts. `accept` is the bare
/// string `"accept"`, while `acceptWithExecpolicyAmendment` is the single-key object
/// whose body names the exact argv a "don't ask again" would whitelist.
///
/// **The body is the stored card's, never the phone's.** A phone sends an opaque id
/// and nothing else ([`protocol::ws::AnswerDecision::OptionId`]); the amendment it
/// would apply is read back out of the options this daemon filed when it raised the
/// card — which are the ones the app-server itself proposed. So a phone can pick
/// between the server's offers and can never compose one, and the hash the phone
/// echoes back covers that very option set, so it cannot even pick from a stale one.
///
/// `None` when the id is not in the card's own option table: a decision the wire
/// never offered for this request, refused rather than forwarded.
pub(crate) fn wire_decision(options: &Value, option_id: &str) -> Option<Value> {
    let offered = options
        .as_array()?
        .iter()
        .find(|offer| offer.get("id").and_then(Value::as_str) == Some(option_id))?;
    Some(match offered.get("payload") {
        Some(payload) if !payload.is_null() => {
            let mut body = Map::new();
            body.insert(option_id.to_string(), payload.clone());
            Value::Object(body)
        }
        _ => Value::String(option_id.to_string()),
    })
}

/// The decisions to offer, in the order they will be shown.
///
/// The command family's set is the wire's, filtered to what this build has
/// words for; the file-change family's is the pane-measured table, because the
/// wire offers nothing. Both are the same shape by the time they leave here, so
/// nothing downstream has to know which came from where.
fn choices(family: Family, params: &Value) -> Vec<Choice> {
    match family {
        Family::FileChange => family
            .labels()
            .iter()
            .map(|(id, label)| Choice {
                id: (*id).to_string(),
                label,
                payload: None,
            })
            .collect(),
        Family::Command => params
            .get("availableDecisions")
            .and_then(Value::as_array)
            .map(|offered| {
                offered
                    .iter()
                    .filter_map(decision)
                    .filter_map(|(id, payload)| {
                        family
                            .label_for(&id)
                            .map(|label| Choice { id, label, payload })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

impl Approval {
    /// Read one `*/requestApproval`, joining a `fileChange` against the
    /// `item/started` snapshot the caller found for its item.
    ///
    /// `started` is consulted only for the file-change family, and only because
    /// that family's request carries no content. The command family is
    /// self-sufficient — measured on 0.153, `command` and `cwd` ride the
    /// request itself — so it never touches the snapshot and a rebind that
    /// missed the `item/started` still cards a command perfectly well.
    pub(crate) fn read(
        family: Family,
        params: &Value,
        started: Option<&Value>,
    ) -> Result<Approval, Refusal> {
        let thread_id = non_empty(params, "threadId")?;
        let turn_id = non_empty(params, "turnId")?;
        let item_id = non_empty(params, "itemId")?;

        let context = match family {
            // A command approval that does not say what command is the command
            // family's `fileChange`-with-no-changes, and it gets that family's
            // answer. The bundle marks `command` nullable, so this is reachable
            // from a schema-valid frame rather than only from a malformed one.
            Family::Command => {
                // **Pinned to the literal, and absence is not the literal.**
                //
                // The bundle declares `environmentId: {"default": null}`, and a
                // syntactic default is not a semantic one: nothing in it says
                // `null` *means* the local environment. What is measured is
                // stronger and simpler — every captured `commandExecution`
                // approval on both 0.147 and 0.153 carries the string `"local"`,
                // four for four. So the literal is the only value this build has
                // ever seen and the only one it admits; an absent or `null`
                // environment is a frame no capture has produced, and carding it
                // would draw a local-looking card for an environment nobody
                // named. (The file-change family carries the field on neither
                // release, which is why this pin is in this arm alone.)
                match params.get("environmentId") {
                    Some(Value::String(id)) if id == LOCAL_ENVIRONMENT => {}
                    _ => return Err(Refusal::UnmeasuredEnvironment),
                }
                Context::Command {
                    command: optional(params, "command").ok_or(Refusal::NoContent)?,
                    cwd: optional(params, "cwd"),
                    reason: optional(params, "reason"),
                    amendment: params
                        .get("proposedExecpolicyAmendment")
                        .filter(|value| !value.is_null())
                        .cloned(),
                }
            }
            Family::FileChange => {
                let changes: Vec<Value> = started
                    .and_then(|item| item.get("changes"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if changes.is_empty() {
                    return Err(Refusal::NoContent);
                }
                Context::FileChange {
                    changes,
                    reason: optional(params, "reason"),
                    grant_root: optional(params, "grantRoot"),
                }
            }
        };

        let choices = choices(family, params);
        if choices.is_empty() {
            return Err(Refusal::NoLabelledChoice);
        }

        Ok(Approval {
            family,
            thread_id,
            turn_id,
            item_id,
            context,
            choices,
        })
    }

    /// The opaque wire id this card is answered by.
    ///
    /// **Derived from the item, never read off the frame.** A Codex
    /// `serverRequest` id is a per-connection integer from zero, shared across
    /// families: the very same approval re-delivered to a reconnecting link
    /// arrives as `id: 0` again, so it identifies nothing across the one event
    /// — a daemon bounce — that most needs it to. What is stable is `itemId`, a
    /// uuid measured byte-identical across that re-delivery, and it goes into
    /// [`CompositeId`]'s request-id slot in its `Text` form, which is
    /// type-tagged and so can never collide with a numeric id that prints the
    /// same. The visit generation is in there for the reason it always was: an
    /// A→B→A revisit reuses the thread id and must not reuse the activation.
    pub(crate) fn request_id(
        &self,
        session_uid: &str,
        generation: u64,
    ) -> Result<String, protocol::composite_id::CompositeIdError> {
        CompositeId {
            session_uid: session_uid.to_string(),
            thread_id: self.thread_id.clone(),
            server_request_id: ServerRequestId::Text(self.item_id.clone()),
            generation,
        }
        .encode()
    }

    /// Everything the card describes, as one hashable object.
    fn tool_input(&self) -> Value {
        let mut input = Map::new();
        match &self.context {
            Context::Command {
                command,
                cwd,
                reason,
                amendment,
            } => {
                // `command` first, because it is the key the phone's
                // `principalArgument` looks for and therefore the line a human
                // reads before anything else.
                input.insert(
                    "command".into(),
                    Value::String(bounded(command, MAX_COMMAND_BYTES)),
                );
                if let Some(cwd) = cwd {
                    input.insert("cwd".into(), Value::String(cwd.clone()));
                }
                if let Some(reason) = reason {
                    input.insert(
                        "reason".into(),
                        Value::String(bounded(reason, MAX_REASON_BYTES)),
                    );
                }
                // What "and don't ask again for this command" would actually
                // whitelist, on the card rather than implied by it.
                if let Some(amendment) = amendment {
                    input.insert("proposed_amendment".into(), amendment.clone());
                }
            }
            Context::FileChange {
                changes,
                reason,
                grant_root,
            } => {
                // `path` for the same reason `command` leads above: it is what
                // the phone reads as the one line that identifies the call. It
                // is the first changed path and says so by being singular; the
                // full list is right beside it.
                if let Some(path) = changes.first().and_then(|c| c.get("path")).cloned() {
                    input.insert("path".into(), path);
                }
                let shown = changes.len().min(MAX_CHANGES);
                // The shared budget, split evenly: one file keeps the whole
                // per-file ceiling, thirty-two get a thirty-second of the total.
                // `shown` is at least one here — a context with no changes is
                // refused in `read`.
                let per_diff = (MAX_TOTAL_DIFF_BYTES / shown.max(1)).min(MAX_DIFF_BYTES);
                input.insert(
                    "changes".into(),
                    Value::Array(
                        changes[..shown]
                            .iter()
                            .map(|change| bounded_change(change, per_diff))
                            .collect(),
                    ),
                );
                // **A card that drops files says so, and commits to the ones it
                // dropped.** Silently showing the first 32 of a 40-file patch
                // is a card that describes a smaller change than the one being
                // approved; the count is what a human needs, and the digest is
                // what stops two patches with the same visible 32 files being
                // the same card.
                if changes.len() > shown {
                    let omitted = Value::Array(changes[shown..].to_vec());
                    input.insert(
                        "changes_omitted".into(),
                        serde_json::json!({
                            "count": changes.len() - shown,
                            "sha256": protocol::hash::sha256_hex(omitted.to_string().as_bytes()),
                        }),
                    );
                }
                if let Some(reason) = reason {
                    input.insert(
                        "reason".into(),
                        Value::String(bounded(reason, MAX_REASON_BYTES)),
                    );
                }
                // The root an "and don't ask again for these files" would grant.
                if let Some(grant_root) = grant_root {
                    input.insert("grant_root".into(), Value::String(grant_root.clone()));
                }
            }
        }
        input.insert(
            "options".into(),
            Value::Array(self.choices.iter().map(Choice::to_json).collect()),
        );
        Value::Object(input)
    }

    /// What the risk classifier is shown: the request as the app-server sent
    /// it, before any display bound.
    ///
    /// A separate value from [`Approval::tool_input`] because the two answer
    /// different questions. `tool_input` is what a human reads and what the
    /// hash covers, so it is bounded; this is what *would run*, so it is not.
    fn classified(&self) -> Value {
        match &self.context {
            // `risk::scan_text` returns `tool_input.command` verbatim when the
            // key is present and reads nothing else, so the whole command is
            // the whole of what this needs to carry.
            Context::Command { command, .. } => {
                serde_json::json!({ "command": command })
            }
            // No `command` key, so the classifier collects the string values —
            // the paths and the diffs — and those are already whole in the
            // context.
            Context::FileChange { changes, .. } => {
                serde_json::json!({ "changes": changes })
            }
        }
    }

    /// The card, in the one shape the phone will render.
    ///
    /// `display_text` and `payload_hash` are the Claude path's own pair, over
    /// the same preimage, for the reason this module's header gives: the phone
    /// hard-gates on `SHA-256(display_text) == payload_hash`, so a card whose
    /// hash covers anything other than exactly `display_text` is a card with no
    /// buttons. Everything the hash must cover — the command or the changes,
    /// the cwd, the reason, and the `{id, label, payload}` of every option — is
    /// inside `tool_input`, and is therefore inside the preimage.
    pub(crate) fn card(&self, request_id: String, generation: u64) -> protocol::ws::ApprovalCard {
        let tool_name = self.family.tool_name();
        let tool_input = self.tool_input();
        protocol::ws::ApprovalCard {
            request_id,
            payload_hash: protocol::hash::approval_payload_hash(tool_name, &tool_input),
            display_text: protocol::hash::approval_payload_text(tool_name, &tool_input),
            // A real classification, not a placeholder: `risk::classify` reads
            // `tool_input.command` verbatim for its destructive patterns, so a
            // Codex `rm -rf` is described exactly as a Claude one is.
            //
            // **Read off the wire's command, not off the projection above.**
            // The display bound cuts at 8 KiB, and a command whose destructive
            // half sits past that was classified on its benign prefix —
            // measured: `echo <8 KiB> ; rm -rf /` came back `Medium` while the
            // bare `rm -rf /` came back `High`. What the classifier must read is
            // what would run. Its own `MAX_SCAN_BYTES` still stops the scan at
            // 16 KiB, which is the same protocol-level bound a 16 KiB Claude
            // `Bash` command meets, and is not this module's to move.
            risk: Some(protocol::risk::classify(tool_name, &self.classified())),
            tool_name: tool_name.to_string(),
            tool_input,
            permission_suggestions: None,
            prompt_id: None,
            permission_mode: None,
            generation,
            // Claude's fingerprint of a prompt on the Mac's screen. A Codex
            // decision is not answered by typing into a pane this daemon can
            // see, so there is no prompt to bind and nothing that would make
            // this true.
            identity_bound: false,
        }
    }
}

/// One `changes[]` entry with its diff cut to this card's per-file share.
fn bounded_change(change: &Value, max_diff: usize) -> Value {
    let Some(map) = change.as_object() else {
        return change.clone();
    };
    let mut out = map.clone();
    if let Some(diff) = map.get("diff").and_then(Value::as_str) {
        out.insert("diff".into(), Value::String(bounded(diff, max_diff)));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The real 0.153 command request, from `fixtures/codex/command-execution.jsonl`
    /// with the `reason` 0.153 adds.
    fn command_params() -> Value {
        json!({
            "threadId": "01a01282-ba87-7660-9f8d-05e2219cd505",
            "turnId": "01a01282-c951-76c1-84d1-6e33d6fdb219",
            "itemId": "exec-cf7b67c7-3a19-4dd8-a9a6-6f243db33bd4",
            "startedAtMs": 1787016966352i64,
            "environmentId": "local",
            "kind": "command",
            "command": "/bin/zsh -lc 'touch marker.txt'",
            "cwd": "/work/p-accept",
            "commandActions": [{"type": "unknown", "command": "touch marker.txt"}],
            "proposedExecpolicyAmendment": ["touch", "marker.txt"],
            "reason": "the sandbox is read-only",
            "availableDecisions": [
                "accept",
                {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["touch", "marker.txt"]}},
                "cancel"
            ]
        })
    }

    fn file_change_params() -> Value {
        json!({
            "threadId": "01a0128d-de08-7620-bfd3-5af294128c54",
            "turnId": "01a0128d-ecb1-7413-b509-d32e3f83193c",
            "itemId": "exec-7c581ae8-64a3-49b1-9157-a98fbb2af3e0",
            "startedAtMs": 1787017695911i64,
            "reason": Value::Null,
            "grantRoot": Value::Null
        })
    }

    fn file_change_started() -> Value {
        json!({
            "type": "fileChange",
            "id": "exec-7c581ae8-64a3-49b1-9157-a98fbb2af3e0",
            "changes": [{
                "path": "/work/hello.txt",
                "kind": {"type": "update", "move_path": Value::Null},
                "diff": "@@ -1 +1 @@\n-hello\n+goodbye\n"
            }],
            "status": "inProgress"
        })
    }

    /// **The wire's option set, not the bundle's.**
    ///
    /// `availableDecisions` is declared only in the experimental bundle and
    /// arrives populated on the live stable wire. Reading it is the whole
    /// reason the command family needs no table of its own for the *set* — and
    /// the three it offers are three of the six the schema admits, so a build
    /// that assumed all six would offer a human two buttons the server never
    /// proposed.
    ///
    /// **Mutation:** make `choices` return `family.labels()` for
    /// `Family::Command` too and the payload assertion goes red — the wire's
    /// amendment argv is gone, and with it the only thing that makes
    /// "don't ask again for this command" mean one specific command.
    #[test]
    fn a_command_reads_its_options_off_the_wire_and_keeps_the_amendment_whole() {
        let approval = Approval::read(Family::Command, &command_params(), None).unwrap();
        assert_eq!(
            approval
                .choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["accept", "acceptWithExecpolicyAmendment", "cancel"]
        );
        assert_eq!(
            approval.choices[1].label,
            "Yes, and don't ask again for this command"
        );
        assert_eq!(
            approval.choices[1].payload,
            Some(json!({"execpolicy_amendment": ["touch", "marker.txt"]})),
            "the amendment the server proposed is carried verbatim, so an answer \
             can only ever name one the server itself offered"
        );
        assert_eq!(approval.choices[0].payload, None);
    }

    /// **A file change is offered a different set of words, and the wire says
    /// nothing about them.**
    ///
    /// Its request declares no `availableDecisions` in either bundle and
    /// carries none live, so the set and the labels both come from the
    /// pane-measured table. One shared table would put "don't ask again for
    /// this command" on a file edit.
    ///
    /// **Mutation:** point `Family::FileChange`'s `labels()` at the command
    /// table and the middle id becomes `acceptWithExecpolicyAmendment`, which
    /// is a decision the app-server would refuse for this family.
    #[test]
    fn a_file_change_is_offered_the_table_its_own_family_was_measured_with() {
        let approval = Approval::read(
            Family::FileChange,
            &file_change_params(),
            Some(&file_change_started()),
        )
        .unwrap();
        assert_eq!(
            approval
                .choices
                .iter()
                .map(|c| (c.id.as_str(), c.label))
                .collect::<Vec<_>>(),
            [
                ("accept", "Yes, proceed"),
                (
                    "acceptForSession",
                    "Yes, and don't ask again for these files"
                ),
                ("cancel", "No, and tell Codex what to do differently"),
            ]
        );
        assert!(
            approval.choices.iter().all(|c| c.payload.is_none()),
            "no decision in this family carries a body"
        );
    }

    /// **The content rides the preceding `item/started`, and without it there
    /// is no card.**
    ///
    /// Measured in `fixtures/codex/file-change.jsonl`: the `fileChange`
    /// `item/started` carrying `changes[]` is frame 17, its `requestApproval`
    /// is frame 19. The request itself carries `reason: null`, `grantRoot:
    /// null` and nothing else. A card built without the snapshot could only say
    /// "some files will change", which is not a question a person can answer.
    ///
    /// **Mutation:** drop the `changes.is_empty()` refusal and the second case
    /// returns a card whose `changes` is `[]` — a decision card about nothing.
    #[test]
    fn a_file_change_without_its_started_snapshot_is_refused_rather_than_guessed() {
        let approval = Approval::read(
            Family::FileChange,
            &file_change_params(),
            Some(&file_change_started()),
        )
        .unwrap();
        let Context::FileChange { changes, .. } = &approval.context else {
            panic!("the file-change family must read a file-change context");
        };
        assert_eq!(changes[0]["path"], json!("/work/hello.txt"));

        assert_eq!(
            Approval::read(Family::FileChange, &file_change_params(), None),
            Err(Refusal::NoContent)
        );
        // And a command is untouched by the same absence: it carries its own
        // content, so a rebind that missed every `item/started` still cards it.
        assert!(Approval::read(Family::Command, &command_params(), None).is_ok());
    }

    /// **A command approval that does not name a command is refused, not
    /// carded.**
    ///
    /// The bundle marks `command` nullable, so this is a *schema-valid* frame
    /// rather than a malformed one — which is exactly why the identity refusal
    /// above does not cover it. Carded, it produced a real, hash-verifiable card
    /// whose whole `display_text` was `{"cwd":…,"options":[…]}`: three buttons
    /// and a working directory, asking a person to approve an unnamed action.
    ///
    /// **Mutation:** put the `Option<String>` back on `Context::Command` and
    /// restore the `if let Some(command)` in `tool_input`, and all three arms
    /// card again.
    #[test]
    fn a_command_approval_that_names_no_command_is_refused_rather_than_carded() {
        for absent in [Some(Value::Null), Some(json!("")), None] {
            let mut params = command_params();
            let object = params.as_object_mut().expect("the params are an object");
            match absent {
                Some(value) => {
                    object.insert("command".into(), value);
                }
                None => {
                    object.remove("command");
                }
            }
            assert_eq!(
                Approval::read(Family::Command, &params, None),
                Err(Refusal::NoContent),
                "`null`, `\"\"` and absent are the same nothing, and the same refusal \
                 a file change with no changes gets"
            );
        }
    }

    /// The three identity fields the bundle marks required in both releases.
    /// A frame missing one is malformed, not old, and mints nothing.
    #[test]
    fn a_request_missing_its_identity_is_refused_by_name() {
        for field in ["threadId", "turnId", "itemId"] {
            let mut params = command_params();
            params.as_object_mut().unwrap().remove(field);
            assert_eq!(
                Approval::read(Family::Command, &params, None),
                Err(Refusal::Malformed(field))
            );
            // Present but empty is the same absence, and it is the one a
            // `.as_str().unwrap_or_default()` would have let through into a
            // composite id that identified every item at once.
            let mut blank = command_params();
            blank.as_object_mut().unwrap()[field] = json!("");
            assert_eq!(
                Approval::read(Family::Command, &blank, None),
                Err(Refusal::Malformed(field))
            );
        }
    }

    /// **A decision this build has no words for is dropped, and a request with
    /// nothing left is not carded.**
    ///
    /// The schema admits six decisions and 0.153 offers three. Offering a
    /// button with no label — or worse, labelling an unknown decision with a
    /// neighbour's words — would put a choice in front of a person that does
    /// not say what it does.
    #[test]
    fn an_unlabelled_decision_is_dropped_and_a_request_of_only_those_is_refused() {
        let mut params = command_params();
        params["availableDecisions"] = json!([
            "accept",
            "decline",
            {"applyNetworkPolicyAmendment": {"network_policy_amendment": {}}}
        ]);
        let approval = Approval::read(Family::Command, &params, None).unwrap();
        assert_eq!(
            approval
                .choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["accept"],
            "`decline` and the network amendment are real decisions this build \
             has no measured words for"
        );

        params["availableDecisions"] = json!(["decline"]);
        assert_eq!(
            Approval::read(Family::Command, &params, None),
            Err(Refusal::NoLabelledChoice)
        );
        // A field that is absent, null, or not an array is the same nothing.
        for empty in [Value::Null, json!([]), json!("accept")] {
            params["availableDecisions"] = empty;
            assert_eq!(
                Approval::read(Family::Command, &params, None),
                Err(Refusal::NoLabelledChoice)
            );
        }
    }

    /// **The card passes the phone's own gate**, which is the only test of this
    /// card that matters: `ios/CodeConnect/Model/CardVerification.swift`
    /// recomputes `SHA-256(display_text)`, compares it to `payload_hash`, and
    /// on a mismatch replaces the card with a banner and kills both actions.
    ///
    /// The second check is the one that decides whether a human sees a command
    /// or a wall of JSON: the app re-renders `"{tool_name}\n{tool_input}"` and
    /// falls back to printing `display_text` raw when it does not reproduce it.
    ///
    /// **Mutation:** set `display_text` to the reason, or to the command, and
    /// the first assertion goes red — which is what the phone would do to such
    /// a card, silently.
    #[test]
    fn the_card_hashes_to_exactly_what_the_phone_will_hash() {
        let approval = Approval::read(Family::Command, &command_params(), None).unwrap();
        let card = approval.card("rq".into(), 3);

        assert_eq!(
            card.payload_hash,
            protocol::hash::sha256_hex(card.display_text.as_bytes()),
            "the phone recomputes exactly this and shows a banner instead of a card \
             when it disagrees"
        );
        assert_eq!(
            card.display_text,
            format!("{}\n{}", card.tool_name, card.tool_input),
            "and it re-renders exactly this to decide whether to show the command \
             or the raw hashed text"
        );

        // The five fields the app's `ApprovalCard` decodes non-optionally. A
        // missing one is not a degraded card, it is no card: the decode fails
        // and the timeline prints "could not be read".
        let wire = serde_json::to_value(&card).unwrap();
        for required in [
            "request_id",
            "payload_hash",
            "tool_name",
            "tool_input",
            "display_text",
        ] {
            assert!(
                wire.get(required).is_some_and(|v| !v.is_null()),
                "{required} must be present and non-null or the phone cannot decode the card"
            );
        }
    }

    /// **Everything a stale answer could disagree about is inside the hash.**
    ///
    /// The hash is over `tool_input`, so changing the command, the cwd, the
    /// reason, or any part of the option set — including an amendment's argv —
    /// produces a different card. That is what makes the phone's echoed
    /// `payload_hash` a statement about *this* question rather than about the
    /// item id it happens to share with another.
    ///
    /// **Mutation:** drop `options` from `tool_input` and the last two
    /// assertions go red: a card could then be answered `acceptWith…` against
    /// an amendment the human never saw.
    #[test]
    fn the_hash_moves_when_any_part_of_the_question_moves() {
        let base = Approval::read(Family::Command, &command_params(), None)
            .unwrap()
            .card("rq".into(), 1)
            .payload_hash;

        let different = |edit: &dyn Fn(&mut Value)| {
            let mut params = command_params();
            edit(&mut params);
            let hash = Approval::read(Family::Command, &params, None)
                .unwrap()
                .card("rq".into(), 1)
                .payload_hash;
            assert_ne!(hash, base);
        };
        different(&|p| p["command"] = json!("/bin/zsh -lc 'rm -rf /'"));
        different(&|p| p["cwd"] = json!("/elsewhere"));
        different(&|p| p["reason"] = json!("a different reason"));
        different(&|p| p["availableDecisions"] = json!(["accept", "cancel"]));
        different(&|p| {
            p["availableDecisions"][1]["acceptWithExecpolicyAmendment"]["execpolicy_amendment"] =
                json!(["rm", "-rf", "/"]);
        });
    }

    /// **Every field ruled semantic moves the hash; every field ruled
    /// non-semantic does not; the pinned one is refused when it moves.**
    ///
    /// This is the module header's ruling table, executable. The three fields it
    /// covers are the ones the hash used to be blind to — a card could be
    /// answered against a widened `proposedExecpolicyAmendment`, a granted
    /// `grantRoot`, or a command running somewhere else entirely, and none of
    /// them changed the question the phone had verified.
    ///
    /// The negative half matters as much: `commandActions` and `kind` are
    /// derived from fields already hashed, so putting them in the hash would
    /// make a card go stale for a reason that changes nothing about what runs.
    ///
    /// **Mutations:** drop `proposed_amendment` or `grant_root` from
    /// `tool_input` and the corresponding assertion goes red; delete the
    /// `environmentId` arm and the refusal assertion goes red.
    #[test]
    fn the_semantic_fields_are_in_the_hash_and_the_unmeasured_one_is_refused() {
        let hash_of = |edit: &dyn Fn(&mut Value)| {
            let mut params = command_params();
            edit(&mut params);
            Approval::read(Family::Command, &params, None)
                .unwrap()
                .card("rq".into(), 1)
                .payload_hash
        };
        let base = hash_of(&|_| {});

        // Semantic: what an "and don't ask again" would whitelist.
        assert_ne!(
            base,
            hash_of(&|p| p["proposedExecpolicyAmendment"] = json!(["rm", "-rf", "/"])),
            "a widened amendment is a different grant and must be a different card"
        );
        // Non-semantic: derived from `command`, which is already hashed.
        assert_eq!(
            base,
            hash_of(&|p| {
                p["commandActions"] = json!([{"type": "read", "command": "cat /etc/passwd"}]);
                p["kind"] = json!("something-else");
                p["startedAtMs"] = json!(1i64);
            }),
            "a parsed view of an already-hashed field changes no fact about what runs"
        );
        // Pinned to the LITERAL: refused rather than carded, because there is no
        // way to say "this runs somewhere else" on a card the phone knows how to
        // draw — and no way to say "somewhere unnamed" either. `null` and absent
        // are refused on the same terms as `"remote"`: the bundle's
        // `"default": null` describes the JSON, not the environment, and every
        // captured `commandExecution` approval on 0.147 and 0.153 carries the
        // string. A frame without it is a shape the wire has never produced.
        for elsewhere in [json!("remote"), json!("container-7"), json!(7), Value::Null] {
            let mut params = command_params();
            params["environmentId"] = elsewhere;
            assert_eq!(
                Approval::read(Family::Command, &params, None),
                Err(Refusal::UnmeasuredEnvironment)
            );
        }
        let mut absent = command_params();
        absent.as_object_mut().unwrap().remove("environmentId");
        assert_eq!(
            Approval::read(Family::Command, &absent, None),
            Err(Refusal::UnmeasuredEnvironment),
            "an omitted environment is not a local one"
        );
        // And the measured literal still cards.
        let mut local = command_params();
        local["environmentId"] = json!("local");
        assert!(Approval::read(Family::Command, &local, None).is_ok());
        // The file-change family carries the field on neither release, so the
        // pin must not reach it.
        assert!(Approval::read(
            Family::FileChange,
            &file_change_params(),
            Some(&file_change_started())
        )
        .is_ok());

        // And the file-change family's own two, measured `null` everywhere and
        // hashed the day they are not.
        let file_hash = |edit: &dyn Fn(&mut Value)| {
            let mut params = file_change_params();
            edit(&mut params);
            Approval::read(Family::FileChange, &params, Some(&file_change_started()))
                .unwrap()
                .card("rq".into(), 1)
                .payload_hash
        };
        let file_base = file_hash(&|_| {});
        assert_ne!(file_base, file_hash(&|p| p["grantRoot"] = json!("/")));
        assert_ne!(file_base, file_hash(&|p| p["reason"] = json!("because")));
    }

    /// A destructive Codex command is described exactly as a destructive Claude
    /// one is — the classifier reads `tool_input.command` verbatim, and putting
    /// the command under that key is what buys it.
    #[test]
    fn a_dangerous_command_is_classified_rather_than_left_to_the_phone() {
        let mut params = command_params();
        params["command"] = json!("/bin/zsh -lc 'rm -rf /'");
        let card = Approval::read(Family::Command, &params, None)
            .unwrap()
            .card("rq".into(), 1);
        assert_eq!(
            card.risk.as_ref().map(|r| r.class),
            Some(protocol::risk::RiskClass::High)
        );
    }

    /// **One item is one id, whatever the connection called the request.**
    ///
    /// The wire request id is a per-connection integer from zero; the item id
    /// is a uuid measured byte-identical across a re-delivery. Deriving from
    /// the item is what makes a reconnect rebind one card instead of minting a
    /// second, and the visit generation is what stops an A→B→A revisit reusing
    /// an activation.
    #[test]
    fn the_derived_id_follows_the_item_and_the_visit_rather_than_the_connection() {
        let approval = Approval::read(Family::Command, &command_params(), None).unwrap();
        let first = approval
            .request_id("01K1B3XQ8ZC0DE5FGH7JKMNPCX", 1)
            .unwrap();
        assert_eq!(
            first,
            approval
                .request_id("01K1B3XQ8ZC0DE5FGH7JKMNPCX", 1)
                .unwrap(),
            "the same item on a reconnected leg derives the same id"
        );
        assert_ne!(
            first,
            approval
                .request_id("01K1B3XQ8ZC0DE5FGH7JKMNPCX", 2)
                .unwrap(),
            "a later visit of the same thread is a different activation"
        );

        let decoded = CompositeId::decode(&first).unwrap();
        assert_eq!(
            decoded.server_request_id,
            ServerRequestId::Text(approval.item_id.clone())
        );
        assert_eq!(decoded.thread_id, approval.thread_id);
    }

    /// **A card is bounded before it is hashed, so it stays a card.**
    ///
    /// `Daemon::truncate_payload` does not trim an oversized payload, it
    /// replaces it — the phone then decodes no card at all. Cutting the two
    /// unbounded fields here, ahead of the hash, keeps the text on the phone
    /// provably the text that was hashed. The cut lands on a character
    /// boundary, which is the difference between a truncated diff and a panic.
    #[test]
    fn an_enormous_command_or_diff_is_cut_on_a_character_boundary_before_hashing() {
        let mut params = command_params();
        params["command"] = json!("é".repeat(MAX_COMMAND_BYTES));
        let card = Approval::read(Family::Command, &params, None)
            .unwrap()
            .card("rq".into(), 1);
        assert!(card.display_text.len() < MAX_COMMAND_BYTES + 1024);
        assert_eq!(
            card.payload_hash,
            protocol::hash::sha256_hex(card.display_text.as_bytes()),
            "a bounded card is still a card the phone can verify"
        );

        let mut started = file_change_started();
        started["changes"][0]["diff"] = json!("→".repeat(MAX_DIFF_BYTES));
        started["changes"][0]["path"] = json!("/work/ünïcodé/文件.txt");
        let card = Approval::read(Family::FileChange, &file_change_params(), Some(&started))
            .unwrap()
            .card("rq".into(), 1);
        let shown = &card.tool_input["changes"];
        assert!(shown[0]["diff"].as_str().unwrap().len() < MAX_DIFF_BYTES + 128);
        assert_eq!(shown[0]["path"], json!("/work/ünïcodé/文件.txt"));

        // And a patch of many files is a card about the first `MAX_CHANGES` of
        // them rather than a payload the daemon replaces wholesale.
        started["changes"] = Value::Array(
            (0..MAX_CHANGES * 4)
                .map(|n| json!({"path": format!("/work/f{n}"), "diff": "x"}))
                .collect(),
        );
        let card = Approval::read(Family::FileChange, &file_change_params(), Some(&started))
            .unwrap()
            .card("rq".into(), 1);
        assert_eq!(
            card.tool_input["changes"].as_array().unwrap().len(),
            MAX_CHANGES
        );
    }

    /// **A bound may hide content from the phone; it may not hide it from the
    /// hash, and it may not hide it from the risk class.**
    ///
    /// Three separate ways the old bounding produced a card that was a true
    /// statement about nothing, each measured before it was fixed:
    ///
    ///   * a command whose destructive half sat past the 8 KiB display bound was
    ///     classified on its benign prefix — `Medium` for a command ending
    ///     `; rm -rf /`, where the bare command is `High`;
    ///   * two commands of the same length sharing an 8192-byte prefix, one
    ///     ending `; touch /tmp/ab` and one ending `; rm -rf /tmp/x`, produced
    ///     the **same** `payload_hash` — so a card was answerable against a
    ///     command the human never saw;
    ///   * a 40-file patch became a card about 32 files with nothing anywhere
    ///     saying that eight were missing.
    ///
    /// **Mutations:** drop the digest from `bounded`'s marker and the hash
    /// assertion goes red; classify `&tool_input` instead of `&self.classified()`
    /// and the risk assertion goes red; drop the `changes_omitted` insert and
    /// the count assertion goes red.
    #[test]
    fn what_a_bound_hides_from_the_phone_it_still_commits_to() {
        let prefix = format!("echo {}", "a".repeat(MAX_COMMAND_BYTES));
        let carded = |suffix: &str| {
            let mut params = command_params();
            params["command"] = json!(format!("{prefix}{suffix}"));
            Approval::read(Family::Command, &params, None)
                .unwrap()
                .card("rq".into(), 1)
        };

        // The class is the whole command's, not the visible prefix's.
        let destructive = carded(" ; rm -rf /");
        assert_eq!(
            destructive.risk.as_ref().map(|r| r.class),
            Some(protocol::risk::RiskClass::High),
            "a destructive suffix past the display bound must still be what the \
             card is classified on"
        );
        assert!(
            !destructive.display_text.contains("rm -rf /"),
            "and it is genuinely past the bound — otherwise this proves nothing"
        );

        // Same length, same prefix, different suffix: different card.
        let (benign, hidden) = (" ; touch /tmp/ab", " ; rm -rf /tmp/x");
        assert_eq!(
            benign.len(),
            hidden.len(),
            "the collision needs equal lengths"
        );
        assert_ne!(
            carded(benign).payload_hash,
            carded(hidden).payload_hash,
            "a card whose hash does not move with the hidden bytes is answerable \
             against a command nobody was shown"
        );

        // Omitted files are counted on the card, and the card commits to them.
        let file_card = |count: usize, tail: &str| {
            let mut started = file_change_started();
            started["changes"] = Value::Array(
                (0..count)
                    .map(|n| json!({"path": format!("/work/f{n}{}", if n >= MAX_CHANGES { tail } else { "" }), "diff": "x"}))
                    .collect(),
            );
            Approval::read(Family::FileChange, &file_change_params(), Some(&started))
                .unwrap()
                .card("rq".into(), 1)
        };
        let forty = file_card(MAX_CHANGES + 8, "");
        assert_eq!(forty.tool_input["changes_omitted"]["count"], json!(8));
        assert_ne!(
            forty.payload_hash,
            file_card(MAX_CHANGES + 8, "-elsewhere").payload_hash,
            "two patches whose visible 32 files agree and whose hidden ones do not \
             are two different questions"
        );
        // A patch that fits omits nothing and says nothing about omissions.
        assert!(file_card(MAX_CHANGES, "").tool_input["changes_omitted"].is_null());
    }

    /// Only the two families a human can answer become cards. The permissions
    /// family returns a profile rather than a decision about one action, and
    /// `serverRequest/resolved` is a retirement, not a question.
    #[test]
    fn only_the_two_answerable_families_are_recognised() {
        assert_eq!(Family::of_method(COMMAND_METHOD), Some(Family::Command));
        assert_eq!(
            Family::of_method(FILE_CHANGE_METHOD),
            Some(Family::FileChange)
        );
        for other in [
            "item/permissions/requestApproval",
            "serverRequest/resolved",
            "execCommandApproval",
            "applyPatchApproval",
            "item/started",
        ] {
            assert_eq!(Family::of_method(other), None);
        }

        // **And `of_item_type` is the exact inverse of `as_str`**, which is what
        // lets an item's terminal be matched against a card's family without a
        // second vocabulary. The item types a turn is mostly made of retire
        // nothing, and saying so from the frame is what keeps the store off the
        // hot path.
        for family in [Family::Command, Family::FileChange] {
            assert_eq!(Family::of_item_type(family.as_str()), Some(family));
        }
        for other in ["reasoning", "agentMessage", "userMessage", "todoList", ""] {
            assert_eq!(Family::of_item_type(other), None);
        }
    }
    /// Regenerate `fixtures/codex/approval-card-0.153.json`. Run with
    /// `cargo test -p ccd --bin ccd -- --ignored --nocapture regenerate_the`.
    #[test]
    #[ignore = "generator, not a gate"]
    fn regenerate_the_approval_card_fixture() {
        const UID: &str = "01K1B3XQ8ZC0DE5FGH7JKMNPCX";
        let approval = Approval::read(Family::Command, &command_params(), None).unwrap();
        let command = approval.card(approval.request_id(UID, 1).unwrap(), 1);
        let approval = Approval::read(
            Family::FileChange,
            &file_change_params(),
            Some(&file_change_started()),
        )
        .unwrap();
        let file_change = approval.card(approval.request_id(UID, 1).unwrap(), 1);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "command": command,
                "file_change": file_change,
            }))
            .unwrap()
        );
    }

    /// **The contract with the app that this repository does not compile.**
    ///
    /// `ios/CodeConnect/Protocol/WireTypes.swift`'s `ApprovalCard` decodes five
    /// keys non-optionally — `request_id`, `payload_hash`, `tool_name`,
    /// `tool_input`, `display_text` — and a card missing any one of them is not
    /// a degraded card: the decode fails and `Timeline.swift` prints "Approval
    /// request could not be read". Then `CardVerification.swift` recomputes
    /// `SHA-256(display_text)`, and on a mismatch `DecisionCard.swift` replaces
    /// the card with a banner and kills both actions.
    ///
    /// So this test asserts the app's own two gates against a **committed**
    /// fixture, and then re-derives that fixture from the real 0.153 frames and
    /// requires the two to be byte-identical. Either half alone would be weak:
    /// a fixture nobody re-derives goes stale silently, and a re-derivation with
    /// no committed artefact leaves Phase 5 nothing to build against. Together
    /// they mean a change to the card shape must change the file, in the diff,
    /// where a reviewer sees it.
    ///
    /// **Mutation:** rename any of the five keys, or drop `options` from
    /// `tool_input`, and the byte comparison fails naming the file to
    /// regenerate.
    #[test]
    fn the_card_matches_the_committed_contract_the_app_decodes() {
        const FIXTURE: &str = include_str!("../../../fixtures/codex/approval-card-0.153.json");
        let committed: Value = serde_json::from_str(FIXTURE).expect("the fixture must be JSON");

        for family in ["command", "file_change"] {
            let card = &committed[family];
            // Gate one: the five keys the app decodes non-optionally.
            for required in [
                "request_id",
                "payload_hash",
                "tool_name",
                "tool_input",
                "display_text",
            ] {
                assert!(
                    card.get(required).is_some_and(|v| !v.is_null()),
                    "{family}: the app cannot decode a card without a non-null {required}"
                );
            }
            // Gate two: the hash the app recomputes before it will draw a button.
            let display = card["display_text"].as_str().expect("display_text is text");
            assert_eq!(
                card["payload_hash"].as_str().unwrap(),
                protocol::hash::sha256_hex(display.as_bytes()),
                "{family}: this card renders as a banner on the phone, not a card"
            );
            // And the check that decides whether a human sees the command or a
            // wall of JSON.
            assert_eq!(
                display,
                format!(
                    "{}\n{}",
                    card["tool_name"].as_str().unwrap(),
                    card["tool_input"]
                ),
                "{family}: the app re-renders exactly this"
            );
            // The card must round-trip through the daemon's own type, or the
            // fixture is a shape this build cannot produce.
            let decoded: protocol::ws::ApprovalCard =
                serde_json::from_value(card.clone()).expect("the daemon's own type");
            assert_eq!(decoded.request_id, card["request_id"].as_str().unwrap());
        }

        // The fixture is what this build produces, from the real 0.153 frames.
        const UID: &str = "01K1B3XQ8ZC0DE5FGH7JKMNPCX";
        let approval = Approval::read(Family::Command, &command_params(), None).unwrap();
        let command = approval.card(approval.request_id(UID, 1).unwrap(), 1);
        let approval = Approval::read(
            Family::FileChange,
            &file_change_params(),
            Some(&file_change_started()),
        )
        .unwrap();
        let file_change = approval.card(approval.request_id(UID, 1).unwrap(), 1);
        let derived = json!({"command": command, "file_change": file_change});
        assert_eq!(
            derived, committed,
            "the committed card contract no longer matches what this build produces. \
             Regenerate it: cargo test -p ccd --bin ccd -- --ignored --nocapture \
             regenerate_the_approval_card_fixture > fixtures/codex/approval-card-0.153.json"
        );
    }

    /// **The live 0.153 capture, replayed through this parser.**
    ///
    /// `fixtures/codex/approval-0.153.jsonl` is a recording, not a mock: two
    /// real `codeconnect codex` sessions driven to a real approval, tapped on
    /// the ccd leg. Replaying it here is what makes a schema drift in a future
    /// codex release fail this suite instead of failing silently in production —
    /// the same job `fixture_replay.rs` does for Claude's hooks.
    ///
    /// It pins the three things the 0.147 captures cannot show: that
    /// `availableDecisions` is on the **stable** wire, that `reason` is
    /// populated, and that a `fileChange` request carries no content at all so
    /// its card must be joined against the preceding `item/started`.
    #[test]
    fn the_live_0_153_capture_replays_into_cards_the_phone_can_verify() {
        const CAPTURE: &str = include_str!("../../../fixtures/codex/approval-0.153.jsonl");
        let rows: Vec<Value> = CAPTURE
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each line is one JSON frame"))
            .collect();

        // The `item/started` snapshots, keyed the way the observer keys them.
        let started: std::collections::HashMap<String, Value> = rows
            .iter()
            .filter(|row| row["frame"]["method"] == "item/started")
            .filter_map(|row| {
                let item = row["frame"].pointer("/params/item")?;
                Some((item["id"].as_str()?.to_string(), item.clone()))
            })
            .collect();

        let mut carded = Vec::new();
        for row in &rows {
            let frame = &row["frame"];
            let Some(method) = frame["method"].as_str() else {
                continue;
            };
            let Some(family) = Family::of_method(method) else {
                continue;
            };
            let params = &frame["params"];
            let item = params["itemId"].as_str().and_then(|id| started.get(id));
            let approval = Approval::read(family, params, item)
                .unwrap_or_else(|why| panic!("the live {method} must read: {why}"));
            let request_id = approval
                .request_id("01K1B3XQ8ZC0DE5FGH7JKMNPCX", 1)
                .unwrap();
            let card = approval.card(request_id, 1);

            // The phone's own two gates, on a card built from a real frame.
            assert_eq!(
                card.payload_hash,
                protocol::hash::sha256_hex(card.display_text.as_bytes()),
                "{method}: a card that does not hash to its own display text is a \
                 banner on the phone, not a card"
            );
            assert_eq!(
                card.display_text,
                format!("{}\n{}", card.tool_name, card.tool_input)
            );
            carded.push((family, params.clone(), card));
        }
        assert_eq!(
            carded.len(),
            2,
            "the capture holds one approval of each family"
        );

        let (family, params, card) = &carded[0];
        assert_eq!(*family, Family::Command);
        // On the STABLE wire, with no `--experimental` anywhere in the launch,
        // and populated with three of the schema's six.
        assert!(params["availableDecisions"].is_array());
        assert_eq!(
            card.tool_input["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["accept", "acceptWithExecpolicyAmendment", "cancel"]
        );
        // 0.147 sent this null; 0.153 populates it, which is why it is optional
        // rather than required — measured in both directions.
        assert!(
            params["reason"].is_string(),
            "0.153 populates `reason`, and it is what a human reads first"
        );
        assert_eq!(card.tool_input["reason"], params["reason"]);
        assert_eq!(card.tool_input["command"], params["command"]);

        let (family, params, card) = &carded[1];
        assert_eq!(*family, Family::FileChange);
        // The request carries nothing: no options, no reason, no content.
        assert!(params["availableDecisions"].is_null());
        assert!(params["reason"].is_null() && params["grantRoot"].is_null());
        // So the content is the join, and the options are this family's own
        // pane-measured table.
        assert!(card.tool_input["changes"][0]["diff"].is_string());
        assert_eq!(
            card.tool_input["options"][1]["id"],
            json!("acceptForSession")
        );
        // And without the join there is no card at all — which is the honest
        // answer to a request whose `item/started` this connection never saw.
        assert_eq!(
            Approval::read(Family::FileChange, params, None),
            Err(Refusal::NoContent)
        );

        // The control connection resumed nothing and was handed no approval.
        // That is the premise the whole observer rests on: delivery is bought by
        // the subscription, so a card is always about a thread this link visits.
        assert_eq!(
            rows.iter()
                .filter(|row| row["conn"]
                    .as_str()
                    .is_some_and(|c| c.ends_with("-unsubscribed"))
                    && row["frame"]["method"]
                        .as_str()
                        .is_some_and(|m| m.ends_with("/requestApproval")))
                .count(),
            0
        );
    }
}

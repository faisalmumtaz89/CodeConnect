//! The gauntlet.
//!
//! Each scenario attacks one invariant the event log claims, and each one
//! reports *numbers* rather than a verdict: "5 kills, 0 gaps, 0 duplicates,
//! recovery 0.4–1.2s" is something a human can compare against the next run,
//! and "PASS" is not.
//!
//! Two of the seven run against synthetic sessions (the tailer torture and the
//! kill-during-ingest run). That is not a shortcut — a synthetic transcript is
//! the only way to truncate and rewrite a JSONL file without corrupting a real
//! agent's history, and the daemon cannot tell the difference: a synthetic
//! session is adopted through exactly the hook path a real one uses. The other
//! five drive the live `codeconnect claude` session.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use protocol::event::SessionSummary;
use protocol::ipc::HookPost;
use protocol::ws::{terminal_close, AnswerDecision, AnswerResult};

use crate::env;
use crate::ws::{check_replay, Attachment, Phone, Terminal, TerminalEvent};
use crate::Outcome;

/// How long to give the tailer and the ingest path to catch up. Generous
/// against a 250ms poll plus FSEvents debounce, so a slow machine is not a
/// failure.
const SETTLE: Duration = Duration::from_secs(20);

pub struct Target {
    pub session: SessionSummary,
    pub host: String,
    pub port: u16,
    pub token: String,
    /// The one pairing this run performs, and every grant it left on the Mac.
    ///
    /// A `OnceCell` holding the *attempt* rather than the credential: a device
    /// token is shell-equivalent authority, and the cell exists so a run buys
    /// exactly one. Holding only the credential meant a failed attempt left the
    /// cell empty — `get_or_try_init` discards an `Err` — and the next caller
    /// paired again. Three of the five terminal scenarios plus the flap's
    /// per-round reconnect ask for a connection, so an attempt that failed
    /// after the daemon had already written the device row could mint up to
    /// nine grants in one run and revoke none of them. Measured on a real
    /// machine before this was fixed: thirteen accumulated `ccsoak-terminal`
    /// rows in `codeconnect devices`.
    pub pairing: tokio::sync::OnceCell<Pairing>,
}

/// The single pairing attempt a run makes, and everything it is answerable for.
pub struct Pairing {
    /// The credential, when the ack carried one.
    device: Option<PairedDevice>,
    /// Why there is none. Present exactly when `device` is absent.
    complaint: Option<String>,
    /// Every device row this run caused the daemon to write.
    minted: Vec<MintedDevice>,
    /// Set when the accounting itself could not be done, so the release reports
    /// a residue it cannot name rather than a clean run.
    unaccounted: Option<String>,
}

/// A credential this run holds.
pub struct PairedDevice {
    token: String,
    /// The name the daemon *assigned*, when the ack reported one. Never the
    /// name that was asked for: those differ, see [`device_name`].
    name: Option<String>,
}

/// A grant this run created, and the handle that hands it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MintedDevice {
    /// What `RevokeDevice` is given, and what the row must be matched on when
    /// the revoke is read back.
    handle: Handle,
    /// For the report.
    name: String,
}

/// The two spellings `RevokeDevice` accepts, kept apart rather than flattened
/// into one string.
///
/// It takes a device id, an id prefix or an exact name, so either one revokes.
/// They stop being interchangeable the moment the revoke is *confirmed* against
/// the device list: an id is unique by construction, whereas a name is chosen
/// by the phone and is unique only because the daemon uniquifies it — so a
/// confirmation that matched on the name would be reading whichever row answers
/// to that name today, which is not necessarily the row this run created. The
/// id is therefore carried whenever the listing could be read at all, and the
/// name is the fallback for the one case where it could not.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Handle {
    Id(String),
    Name(String),
}

impl Handle {
    /// What goes on the wire, and into the command an operator retypes.
    fn as_str(&self) -> &str {
        match self {
            Handle::Id(id) => id,
            Handle::Name(name) => name,
        }
    }

    /// Is `row` the row this handle names?
    fn matches(&self, row: &protocol::pairing::DeviceSummary) -> bool {
        match self {
            Handle::Id(id) => row.device_id == *id,
            Handle::Name(name) => row.name == *name,
        }
    }
}

impl Target {
    pub async fn phone(&self) -> Result<Phone> {
        Phone::connect(&self.host, self.port, &self.token).await
    }

    /// A connection that may open a terminal.
    ///
    /// [`Target::phone`] cannot: it carries the static bootstrap token, which
    /// the daemon refuses a terminal to by design — `terminal_pty` comes back
    /// false and an attach anyway is closed `not_authorised`. So the harness
    /// pairs itself the way a phone does, over the socket the operator's own
    /// `codeconnect pair` uses, and connects again with the device token the
    /// pairing ack handed over.
    pub async fn terminal_phone(&self) -> Result<Phone> {
        let pairing = self
            .pairing
            .get_or_init(|| Pairing::once(&self.host, self.port))
            .await;
        let device = match (&pairing.device, &pairing.complaint) {
            (Some(device), _) => device,
            // The stored failure, replayed. A second attempt would be a second
            // grant bought to answer a question the first already answered.
            (None, complaint) => bail!(
                "the one pairing this run makes did not produce a credential: {}",
                complaint.as_deref().unwrap_or("no reason was recorded")
            ),
        };
        let phone = Phone::connect(&self.host, self.port, &device.token).await?;
        if !phone.greeting().terminal_pty {
            bail!(
                "the daemon admitted device {:?} and still withheld terminal_pty; a paired \
                 credential is exactly what that capability is for",
                device.name.as_deref().unwrap_or("(unnamed)")
            );
        }
        Ok(phone)
    }
}

impl Pairing {
    /// Pair once, bracketed by a reading of the daemon's device list.
    ///
    /// The bracket is the whole point. The pairing code is redeemed and the
    /// device row written *before* the ack is composed, so every failure from
    /// the ack onwards — a read timeout, a protocol-minor mismatch, an `Error`
    /// frame, an ack carrying no token — leaves a standing shell-equivalent
    /// grant that the ack cannot name. The difference between the two listings
    /// names it whatever the ack did.
    async fn once(host: &str, port: u16) -> Pairing {
        let before = env::devices().ok();
        let redeemed = Pairing::redeem(host, port).await;
        let after = env::devices().ok();
        let assigned = redeemed
            .as_ref()
            .ok()
            .and_then(|device| device.name.as_deref());
        let (minted, unaccounted) = accounted(
            device_name(),
            before.as_deref().map(rows),
            after.as_deref().map(rows),
            assigned,
        );
        match redeemed {
            Ok(device) => Pairing {
                device: Some(device),
                complaint: None,
                minted,
                unaccounted,
            },
            Err(err) => Pairing {
                device: None,
                complaint: Some(format!("{err:#}")),
                minted,
                unaccounted,
            },
        }
    }

    async fn redeem(host: &str, port: u16) -> Result<PairedDevice> {
        let code = env::create_pairing()?;
        let paired = Phone::pair(host, port, &code, device_name()).await?;
        let greeting = paired.greeting();
        let token = greeting
            .device_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("the pairing ack carried no device token"))?;
        // Deliberately no fallback to [`device_name`]. The daemon uniquifies the
        // requested name against every row it has, so on any machine that has
        // run this before the assigned name is `ccsoak-terminal-7`, not
        // `ccsoak-terminal` — and revoking the name that was *asked* for
        // removes somebody else's row or nothing at all.
        Ok(PairedDevice {
            token,
            name: greeting.device_name.clone(),
        })
    }
}

/// The stem every row this harness asks for carries, so a human reading
/// `codeconnect devices` can see whose row it is at a glance.
const DEVICE_STEM: &str = "ccsoak-terminal";

/// What *this run's* device asks to be called in `codeconnect devices`: the
/// stem, and a uid minted once for the life of the process.
///
/// The uid is what makes attribution safe. With a bare `ccsoak-terminal` stem
/// every run this machine has ever made shares one name, so a **concurrent**
/// ccsoak's freshly written row appeared in this run's before/after difference
/// and was revoked as if it were this run's — one gauntlet pulling the
/// credential out from under another. Only rows carrying this process's own uid
/// are ever this process's to hand back.
///
/// The daemon still uniquifies whatever it is given against every row it holds,
/// revoked ones included, so what comes back may read `…-2`. The revoked rows
/// are the audit trail of a grant that existed. Nothing here ever revokes by
/// the string that was *asked* for — see [`PairedDevice::name`].
fn device_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| match protocol::uid::new() {
        Ok(uid) => format!("{DEVICE_STEM}-{uid}"),
        // Kernel entropy is unavailable, which is a reason to fall back to a
        // weaker unique name and never a reason to fall back to the shared
        // stem: a pid and a millisecond are two things no other *live* process
        // on this Mac can both be holding.
        Err(_) => format!(
            "{DEVICE_STEM}-p{}t{}",
            std::process::id(),
            protocol::time::now_unix_ms()
        ),
    })
}

/// The daemon's device rows, reduced to what a release needs.
fn rows(devices: &[protocol::pairing::DeviceSummary]) -> Vec<MintedDevice> {
    devices
        .iter()
        .map(|device| MintedDevice {
            handle: Handle::Id(device.device_id.clone()),
            name: device.name.clone(),
        })
        .collect()
}

/// Is `name` a row the daemon would have written for the run whose device is
/// called `mine` — that name, or the uniquified `-2`, `-3` … it becomes when
/// something already answers to it?
///
/// The filter is what makes the before/after difference safe to act on. Without
/// it an operator who paired their own phone during the seconds this run was
/// pairing would have that phone revoked by a harness cleaning up after itself;
/// with `mine` a per-run name rather than a shared stem, a concurrent ccsoak's
/// row is somebody else's phone as far as this predicate is concerned, which is
/// exactly what it is.
fn is_run_device_name(mine: &str, name: &str) -> bool {
    name == mine
        || name
            .strip_prefix(mine)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

/// What this run must hand back, given the device list either side of its one
/// pairing and whatever name the ack reported.
///
/// `None` for a listing means it could not be read at all, which is not an
/// empty list: an empty list is "the daemon has no devices" and would make the
/// difference say nothing was created.
///
/// `mine` is [`device_name`] in the live path, and a fixture in the tests.
fn accounted(
    mine: &str,
    before: Option<Vec<MintedDevice>>,
    after: Option<Vec<MintedDevice>>,
    assigned_name: Option<&str>,
) -> (Vec<MintedDevice>, Option<String>) {
    let difference = match (before, after) {
        (Some(before), Some(after)) => Some(
            after
                .into_iter()
                .filter(|row| is_run_device_name(mine, &row.name))
                .filter(|row| !before.iter().any(|had| had.handle == row.handle))
                .collect::<Vec<_>>(),
        ),
        _ => None,
    };
    let mut minted = difference.clone().unwrap_or_default();
    if minted.is_empty() {
        // The listing failed, or raced. The assigned name is the only handle
        // left, and it is a real one: `RevokeDevice` resolves an exact name.
        if let Some(name) = assigned_name {
            minted.push(MintedDevice {
                handle: Handle::Name(name.to_string()),
                name: name.to_string(),
            });
        }
    }
    let unaccounted = if minted.len() > 1 {
        // One pairing buys one grant, so one row is the whole of what this run
        // can have created. Two was accepted here in silence, and every one of
        // the shapes that produces it — a redemption that wrote twice, a
        // retried pairing, something else minting under this run's own uid —
        // means the accounting below cannot be believed. The rows are still
        // handed back, and the run still fails.
        Some(format!(
            "one pairing left {} device rows carrying this run's own name ({}), where a pairing \
             mints exactly one grant — `codeconnect devices` lists them and `codeconnect revoke \
             <id>` removes one",
            minted.len(),
            minted
                .iter()
                .map(|row| format!("{} ({})", row.name, row.handle.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    } else if minted.is_empty() && difference.is_none() {
        Some(
            "the daemon's device list could not be read either side of the pairing, so a \
             standing grant this run created cannot be ruled out — `codeconnect devices` lists \
             them and `codeconnect revoke <id>` removes one"
                .to_string(),
        )
    } else {
        None
    };
    (minted, unaccounted)
}

/// What became of the grants this run created.
///
/// A type that can express failure, because the previous `Option<String>` could
/// not: an empty cell and a revoke that the daemon refused both came back as
/// something for `main` to print, and `main` keyed the exit code on scenario
/// verdicts alone. A CI job therefore read "clean run, exit 0" over a standing
/// shell-equivalent grant on the operator's Mac.
#[derive(Debug, Default)]
pub struct Release {
    /// One line per grant, for the report.
    pub said: Vec<String>,
    /// Grants that may still be standing. Non-empty is a failed run — see
    /// `exit_complaint` in `main`, which is what turns this into an exit code.
    pub standing: Vec<String>,
}

/// Hand back every terminal credential this run minted.
///
/// A device token outlives the process, so a gauntlet that left one behind
/// every run would quietly accumulate shell-equivalent grants in the operator's
/// device list. Called once, after every scenario has finished with it.
pub fn release_device(target: &Target) -> Release {
    let mut release = Release::default();
    let Some(pairing) = target.pairing.get() else {
        // No terminal scenario ran, so nothing was ever bought.
        return release;
    };
    if let Some(why) = &pairing.unaccounted {
        release.said.push(format!("UNACCOUNTED GRANT: {why}"));
        release.standing.push(why.clone());
    }
    if pairing.minted.is_empty() {
        return release;
    }
    for device in &pairing.minted {
        match env::revoke_device(device.handle.as_str()) {
            Ok(true) => release
                .said
                .push(format!("revoked the paired device {}", device.name)),
            // Not "already revoked, so all is well": the daemon is reporting
            // that it found no live *token*, which is a different claim from
            // the row carrying `revoked_at`. The listing below settles it.
            Ok(false) => release.said.push(format!(
                "the daemon had no live token to revoke for the paired device {}",
                device.name
            )),
            Err(err) => release.said.push(format!(
                "COULD NOT REVOKE the paired device {}: {err:#}",
                device.name
            )),
        }
    }
    // Read the rows back rather than trusting the answers above.
    //
    // `revoke_device` returning `Ok` is the daemon reporting on a token it
    // looked up; the row in `codeconnect devices` is the grant. This run used
    // to record `Ok(true)` and `Ok(false)` as a clean release without anybody
    // ever looking at the row, so a revoke that resolved the wrong handle — or
    // none — exited 0 over a device the operator can still connect with.
    match env::devices() {
        Ok(listing) => {
            let complaints = unrevoked(&pairing.minted, &listing);
            if complaints.is_empty() {
                release.said.push(format!(
                    "the daemon's device list shows all {} row(s) this run created revoked",
                    pairing.minted.len()
                ));
            }
            for complaint in complaints {
                release.said.push(format!("STANDING GRANT: {complaint}"));
                release.standing.push(complaint);
            }
        }
        Err(err) => {
            for device in &pairing.minted {
                let complaint = format!(
                    "the daemon's device list could not be read back after revoking {} ({}), so \
                     whether that grant is gone was never confirmed: {err:#} — `codeconnect \
                     revoke {}` removes it",
                    device.name,
                    device.handle.as_str(),
                    device.handle.as_str()
                );
                release.said.push(format!("STANDING GRANT: {complaint}"));
                release.standing.push(complaint);
            }
        }
    }
    release
}

/// Which of the rows this run created are, by the daemon's own listing, still
/// live grants — one sentence each, naming the command that removes it.
///
/// A row is matched on its device id whenever this run holds one, because an id
/// is unique by construction and a name is unique only by the daemon's
/// uniquifying: confirming on the name would be reading whichever row answers
/// to it now, which is not necessarily the row that was revoked.
fn unrevoked(minted: &[MintedDevice], listing: &[protocol::pairing::DeviceSummary]) -> Vec<String> {
    minted
        .iter()
        .filter_map(|device| {
            match listing.iter().find(|row| device.handle.matches(row)) {
                Some(row) if row.is_active() => Some(format!(
                    "the paired device {} ({}) is still an active row after the revoke — \
                     `codeconnect revoke {}` removes it",
                    device.name, row.device_id, row.device_id
                )),
                Some(_) => None,
                // A revoked device stays listed — "this device was revoked on
                // the 3rd" is a fact the daemon keeps — so a missing row is not
                // the grant being gone. It is this run holding a handle that
                // names nothing, which settles nothing about what it minted.
                None => Some(format!(
                    "the daemon lists no device matching {} ({}), so whether the grant this run \
                     created is still standing could not be read — `codeconnect devices` lists \
                     them and `codeconnect revoke <id>` removes one",
                    device.name,
                    device.handle.as_str()
                )),
            }
        })
        .collect()
}

// --------------------------------------------------------------- (a) kill -9

/// `kill -9 ccd` five times, at random moments, while events are flowing.
///
/// The claim under test is the one the whole architecture rests on: the daemon
/// is not the agents' parent, so losing it costs a reconnect and nothing else.
/// "Nothing else" is measured here as: the sequence is still gap-free, no fact
/// was recorded twice, and the supervisor came back.
pub async fn kill_storm(target: &Target, rounds: u32) -> Outcome {
    let mut notes = Vec::new();
    let uid = target.session.session_uid.clone();
    let mut recoveries = Vec::new();
    let mut posted = 0u32;
    let mut dropped = 0u32;

    // Give the real agent something cheap to do, so the daemon is killed while
    // a transcript is being written and hooks are firing — not while it idles.
    // A refusal is not fatal: the composer may be busy, and the synthetic
    // traffic below is enough on its own.
    let takeover = format!("soak-kill-{}", protocol::time::now_unix_ms());
    match target.phone().await {
        Ok(mut phone) => {
            let text = "reply with exactly: ok";
            match phone.send_text(&uid, text, Some(&takeover)).await {
                Ok(protocol::ws::SendTextResult::Sent { matched }) => {
                    notes.push(format!("prompted the live agent (matched {matched:?})"));
                    // The same mutation again, exactly as a phone on a flaky
                    // link would retry it. It must replay rather than type a
                    // second prompt into a live agent.
                    match phone.send_text(&uid, text, Some(&takeover)).await {
                        Ok(protocol::ws::SendTextResult::Duplicate { .. }) => {
                            notes.push("a retried takeover replayed instead of retyping".into());
                        }
                        Ok(other) => {
                            return Outcome::failed(format!(
                                "a retried send_text with the same identity must not act again: \
                                 {other:?}"
                            ))
                            .with_notes(notes)
                        }
                        Err(err) => {
                            return Outcome::failed(format!("retrying the takeover: {err:#}"))
                                .with_notes(notes)
                        }
                    }
                }
                Ok(other) => {
                    notes.push(format!(
                        "agent not prompted ({other:?}); synthetic traffic only"
                    ));
                }
                Err(err) => notes.push(format!(
                    "agent not prompted ({err:#}); synthetic traffic only"
                )),
            }
        }
        Err(err) => notes.push(format!("no phone connection for the prompt: {err:#}")),
    }

    for round in 0..rounds {
        let before = env::open_db()
            .and_then(|c| env::max_seq(&c, &uid))
            .unwrap_or(0);
        let info = match env::daemon_info() {
            Ok(info) => info,
            Err(err) => return Outcome::failed(format!("daemon unreachable before kill: {err:#}")),
        };

        // Traffic in flight when the axe falls: some of these posts land, some
        // hit a socket that is already gone. Both are correct behaviour, and
        // the point is that neither corrupts the log.
        let load = tokio::task::spawn_blocking({
            let uid = uid.clone();
            let name = target.session.session_id.clone();
            move || {
                let mut posted = 0;
                let mut dropped = 0;
                for i in 0..40 {
                    let ok = env::post_hook(&HookPost {
                        session_id: name.clone(),
                        session_uid: Some(uid.clone()),
                        event: "PreToolUse".into(),
                        payload: serde_json::json!({
                            "hook_event_name": "PreToolUse",
                            "tool_name": "Bash",
                            "tool_input": {"command": "echo soak"},
                            "tool_use_id": format!("soak-kill-{round}-{i}"),
                        }),
                        wait: false,
                    });
                    if ok {
                        posted += 1;
                    } else {
                        dropped += 1;
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                (posted, dropped)
            }
        });

        // Somewhere inside the burst, not at a fixed offset: a kill that always
        // lands at the same point only ever tests one interleaving.
        let delay = 80 + (protocol::time::now_unix_ms() as u64 % 400);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        if let Err(err) = env::kill_9(info.pid) {
            return Outcome::failed(format!("could not kill pid {}: {err:#}", info.pid));
        }

        let (round_posted, round_dropped) = load.await.unwrap_or((0, 0));
        posted += round_posted;
        dropped += round_dropped;

        match env::wait_for_daemon(env::RECOVERY_TIMEOUT) {
            Ok((info, took)) => {
                recoveries.push(took);
                if !info.is_launchd_managed() {
                    notes.push(format!(
                        "round {round}: the daemon came back but does not report our \
                         launchd label ({:?})",
                        info.launchd_label
                    ));
                }
            }
            Err(err) => return Outcome::failed(format!("round {round}: {err:#}")),
        }

        let after = env::open_db()
            .and_then(|c| env::max_seq(&c, &uid))
            .unwrap_or(0);
        if after < before {
            return Outcome::failed(format!(
                "round {round}: max_seq went backwards ({before} -> {after})"
            ));
        }
    }

    // The supervisor has to have reconnected, or the session is alive but
    // unreachable — which would be a silent half-failure.
    let reattached = env::wait_until(SETTLE, |_| {
        Ok(env::sessions()?
            .iter()
            .any(|s| s.session_uid == uid && s.link == protocol::event::Link::Attached))
    });
    match reattached {
        Ok(took) => notes.push(format!(
            "supervisor re-attached in {:.1}s",
            took.as_secs_f64()
        )),
        Err(err) => return Outcome::failed(format!("supervisor never re-attached: {err:#}")),
    }

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let integrity = match env::check_integrity(&conn) {
        Ok(integrity) => integrity,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };

    let slowest = recoveries.iter().max().copied().unwrap_or_default();
    let fastest = recoveries.iter().min().copied().unwrap_or_default();
    notes.push(format!(
        "{rounds} kills · {posted} hooks landed · {dropped} refused while down \
         (fail-open) · recovery {:.1}s–{:.1}s · {} runs, {} events checked",
        fastest.as_secs_f64(),
        slowest.as_secs_f64(),
        integrity.runs,
        integrity.events
    ));
    if !integrity.is_clean() {
        return Outcome::failed(format!(
            "log damaged — gaps: {:?}, duplicates: {:?}",
            integrity.gaps, integrity.duplicates
        ))
        .with_notes(notes);
    }
    Outcome::passed("no gaps, no duplicates").with_notes(notes)
}

// -------------------------------------------------- (b) duplicate-hook storm

/// The same hook payload, fifty times. Exactly one event.
///
/// This is the property that lets every other recovery path be careless: a
/// re-scan after a restart, a hook retried by a shell wrapper and a transcript
/// line seen twice all reduce to "the same fact arrived again", and the answer
/// has to be one row and one `seq`.
pub async fn duplicate_hooks(target: &Target, replays: u32) -> Outcome {
    let uid = target.session.session_uid.clone();
    let tool_use_id = format!("soak-dup-{}", protocol::time::now_unix_ms());
    let source_event_id = format!("pre:{tool_use_id}");

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let seq_before = env::max_seq(&conn, &uid).unwrap_or(0);
    let count_before = env::count_events(&conn, &uid).unwrap_or(0);
    drop(conn);

    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "echo duplicate-storm"},
        "tool_use_id": tool_use_id,
    });
    let post = HookPost {
        session_id: target.session.session_id.clone(),
        session_uid: Some(uid.clone()),
        event: "PreToolUse".into(),
        payload,
        wait: false,
    };

    // Concurrently, not in a loop: serial replays would be absorbed by any
    // in-memory cache, whereas fifty at once is the case where two ingests race
    // for the same `seq` and only the transaction can arbitrate.
    let mut tasks = Vec::new();
    for _ in 0..replays {
        let post = post.clone();
        tasks.push(tokio::task::spawn_blocking(move || env::post_hook(&post)));
    }
    let mut delivered = 0;
    for task in tasks {
        if task.await.unwrap_or(false) {
            delivered += 1;
        }
    }

    if let Err(err) = env::wait_until(SETTLE, |conn| {
        Ok(env::count_by_source_event_id(conn, &uid, &source_event_id)? >= 1)
    }) {
        return Outcome::failed(format!("the replayed hook was never recorded: {err:#}"));
    }

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let stored = env::count_by_source_event_id(&conn, &uid, &source_event_id).unwrap_or(0);
    let seq_after = env::max_seq(&conn, &uid).unwrap_or(0);
    let count_after = env::count_events(&conn, &uid).unwrap_or(0);
    let notes = vec![format!(
        "{delivered}/{replays} posts delivered · {stored} event(s) stored · \
         seq {seq_before}->{seq_after} · count {count_before}->{count_after}"
    )];

    if stored != 1 {
        return Outcome::failed(format!("{stored} events for one fact")).with_notes(notes);
    }
    // A dropped duplicate must not burn a sequence number either: the log's
    // gap-free promise is exactly "seq counts facts, not attempts".
    if seq_after != count_after {
        return Outcome::failed(format!(
            "max_seq {seq_after} != count {count_after}: a duplicate burnt a number"
        ))
        .with_notes(notes);
    }
    Outcome::passed("exactly one event, no burnt sequence").with_notes(notes)
}

// --------------------------------------------------- (c) answer replay storm

/// Twenty phones tapping the same card at the same moment.
///
/// A duplicate must return the *original outcome*, never a rejection. The
/// interesting case is not a retry a minute later — the ledger handles that —
/// but a retry that arrives while the first answer is still being typed into the
/// TTY. That used to come back as a rejection.
pub async fn answer_storm(target: &Target, taps: u32) -> Outcome {
    let uid = target.session.session_uid.clone();
    let prompt_id = format!("soak-{}", protocol::time::now_unix_ms());
    let tool_name = "Bash";
    let tool_input = serde_json::json!({"command": "echo soak-approval"});
    let payload_hash = protocol::hash::approval_payload_hash(tool_name, &tool_input);
    // The id the daemon derives when a PermissionRequest arrives with no
    // preceding PreToolUse to correlate against. Recomputed here rather than
    // read back, so a change to that derivation fails this scenario loudly.
    let request_id = format!("pr-{prompt_id}-{}", &payload_hash[..16]);

    // The composer has to be accepting input, or every answer is legitimately
    // refused and the scenario measures nothing.
    let mut phone = match target.phone().await {
        Ok(phone) => phone,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    match phone.capture(&uid, 40).await {
        Ok(pane) => {
            let ready = protocol::ipc::PromptPresence::InputBox
                .find_match(&pane, None)
                .is_some();
            if !ready {
                return Outcome::skipped(
                    "the session's composer is busy; an answer would be refused for the \
                     right reason and prove nothing",
                );
            }
        }
        Err(err) => return Outcome::failed(format!("could not read the pane: {err:#}")),
    }

    let posted = env::post_hook(&HookPost {
        session_id: target.session.session_id.clone(),
        session_uid: Some(uid.clone()),
        event: "PermissionRequest".into(),
        payload: serde_json::json!({
            "hook_event_name": "PermissionRequest",
            "prompt_id": prompt_id,
            "tool_name": tool_name,
            "tool_input": tool_input,
        }),
        wait: false,
    });
    if !posted {
        return Outcome::failed("could not post the approval");
    }
    if let Err(err) = env::wait_until(SETTLE, |_| {
        Ok(env::sessions()?
            .iter()
            .any(|s| s.session_uid == uid && s.blocked_on.iter().any(|id| id == &request_id)))
    }) {
        return Outcome::failed(format!("the approval never became answerable: {err:#}"));
    }

    // One connection each, so the taps are genuinely concurrent rather than
    // pipelined down a single socket in order.
    let mut tasks = Vec::new();
    for _ in 0..taps {
        let (host, port, token) = (target.host.clone(), target.port, target.token.clone());
        let (request_id, payload_hash, uid) =
            (request_id.clone(), payload_hash.clone(), uid.clone());
        tasks.push(tokio::spawn(async move {
            let mut phone = Phone::connect(&host, port, &token).await?;
            phone
                .answer(
                    &request_id,
                    &payload_hash,
                    // Typed into the composer, which is where a real answer for
                    // this build lands too. The text is a cheap prompt.
                    AnswerDecision::Text {
                        text: "reply with exactly: ok".into(),
                    },
                    &uid,
                )
                .await
        }));
    }

    let mut applied = Vec::new();
    let mut duplicates = Vec::new();
    let mut rejected = Vec::new();
    let mut errors = Vec::new();
    for task in tasks {
        match task.await {
            Ok(Ok(AnswerResult::Applied { outcome })) => applied.push(outcome),
            Ok(Ok(AnswerResult::Duplicate { outcome, .. })) => duplicates.push(outcome),
            Ok(Ok(AnswerResult::Rejected { reason })) => rejected.push(reason),
            Ok(Err(err)) => errors.push(format!("{err:#}")),
            Err(err) => errors.push(format!("task failed: {err}")),
        }
    }

    let mut notes = vec![format!(
        "{taps} concurrent taps · {} applied · {} duplicate · {} rejected · {} errored",
        applied.len(),
        duplicates.len(),
        rejected.len(),
        errors.len()
    )];
    if !rejected.is_empty() {
        notes.push(format!("rejections: {:?}", dedup(&rejected)));
    }
    if !errors.is_empty() {
        notes.push(format!("errors: {:?}", dedup(&errors)));
    }

    if applied.len() != 1 {
        return Outcome::failed(format!(
            "{} answers applied; exactly one must reach the agent",
            applied.len()
        ))
        .with_notes(notes);
    }
    let original = &applied[0];
    if duplicates.len() as u32 != taps - 1 {
        return Outcome::failed(format!(
            "{} of {} retries came back as duplicates; the rest were not idempotent",
            duplicates.len(),
            taps - 1
        ))
        .with_notes(notes);
    }
    if let Some(wrong) = duplicates.iter().find(|o| o.decision != original.decision) {
        return Outcome::failed(format!(
            "a duplicate returned a different decision: {:?} vs {:?}",
            wrong.decision, original.decision
        ))
        .with_notes(notes);
    }
    if duplicates
        .iter()
        .any(|o| o.resolved_at != original.resolved_at)
    {
        return Outcome::failed("a duplicate returned a different resolution time")
            .with_notes(notes);
    }
    Outcome::passed("one applied, the rest replayed the original outcome").with_notes(notes)
}

fn dedup(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = values.to_vec();
    out.sort();
    out.dedup();
    out
}

// --------------------------------------------------------------- (d) WSS flap

/// Connect, subscribe from a random point, read, disconnect. Thirty times.
///
/// A phone on a train does this all day. The invariant is that a replay from
/// *any* watermark is contiguous and monotonic — not merely that it eventually
/// converges, because a client cannot see a gap it was never told about.
pub async fn ws_flap(target: &Target, rounds: u32) -> Outcome {
    let uid = target.session.session_uid.clone();
    let mut total_events = 0usize;
    let mut watermarks = Vec::new();
    let start = Instant::now();

    for round in 0..rounds {
        let max = env::open_db()
            .and_then(|conn| env::max_seq(&conn, &uid))
            .unwrap_or(0);
        // Deterministic spread rather than a random-number dependency: every
        // watermark from 0 to max is visited, including both ends.
        let after_seq = if max == 0 {
            0
        } else {
            (round as u64 * (max + 1) / rounds.max(1) as u64).min(max)
        };
        watermarks.push(after_seq);

        let mut phone = match target.phone().await {
            Ok(phone) => phone,
            Err(err) => return Outcome::failed(format!("round {round}: connect: {err:#}")),
        };
        // Every reconnect re-reads the fleet, as the app does on foreground.
        // The run has to still be findable *and* still carry both identities,
        // or a phone that reconnected would be subscribing to a guess.
        match phone.sessions().await {
            Ok(sessions) => {
                let Some(found) = sessions.iter().find(|s| s.session_uid == uid) else {
                    return Outcome::failed(format!(
                        "round {round}: {uid} vanished from the session list"
                    ));
                };
                if found.session_id != target.session.session_id {
                    return Outcome::failed(format!(
                        "round {round}: {uid} is now called {}, was {}",
                        found.session_id, target.session.session_id
                    ));
                }
            }
            Err(err) => return Outcome::failed(format!("round {round}: sessions: {err:#}")),
        }
        let events = match phone
            .subscribe_and_drain(&uid, after_seq, Duration::from_millis(700))
            .await
        {
            Ok(events) => events,
            Err(err) => return Outcome::failed(format!("round {round}: {err:#}")),
        };
        if let Err(err) = check_replay(&events, after_seq, &uid) {
            return Outcome::failed(format!("round {round}: {err:#}"));
        }
        total_events += events.len();
        // Dropped without a close frame on odd rounds: a phone losing signal
        // does not say goodbye, and the server must not care.
        if round % 2 == 0 {
            drop(phone);
        }
    }

    let notes = vec![format!(
        "{rounds} connect/disconnect cycles in {:.1}s · watermarks {}–{} · \
         {total_events} events replayed, all contiguous",
        start.elapsed().as_secs_f64(),
        watermarks.iter().min().copied().unwrap_or(0),
        watermarks.iter().max().copied().unwrap_or(0),
    )];
    Outcome::passed("every replay gap-free and monotonic").with_notes(notes)
}

// ----------------------------------------------------------- (e) tail torture

/// Truncate and rewrite a transcript underneath the tailer.
///
/// The cursor claims "I have consumed up to byte N of this file". An editor, a
/// crash or a `--resume` can make that claim false while the file's identity is
/// unchanged, which is why the cursor also hashes the last line it consumed.
/// This scenario makes the claim false in the three ways that happen in
/// practice and checks that the recovery costs a re-scan and nothing else.
pub async fn tail_torture(target: &Target) -> Outcome {
    let dir = match env::scratch_dir("tail") {
        Ok(dir) => dir,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let transcript = dir.join("soak.jsonl");
    let uid = match protocol::uid::new() {
        Ok(uid) => uid,
        Err(err) => return Outcome::failed(format!("minting a uid: {err:#}")),
    };
    let name = format!("soak-{}", &uid[uid.len() - 6..]);

    let announce = |lines_written: &str| HookPost {
        session_id: name.clone(),
        session_uid: Some(uid.clone()),
        event: "SessionStart".into(),
        payload: serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": lines_written,
            "cwd": dir.to_string_lossy(),
            "transcript_path": transcript.to_string_lossy(),
        }),
        wait: false,
    };

    // Phase 1 — a normal tail.
    let first: Vec<String> = (0..6).map(|i| line(&format!("{uid}-a{i}"))).collect();
    if let Err(err) = std::fs::write(&transcript, first.join("")) {
        return Outcome::failed(format!("writing the transcript: {err}"));
    }
    if !env::post_hook(&announce("phase-1")) {
        return Outcome::failed("could not register the synthetic session");
    }
    if let Err(err) = env::wait_until(SETTLE, |conn| Ok(env::count_events(conn, &uid)? >= 7)) {
        return Outcome::failed(format!(
            "the tailer never picked the transcript up: {err:#}"
        ));
    }

    // Phase 2 — truncate to nothing and rewrite with *different* content. The
    // file keeps its inode, so identity checks pass and only the last-line hash
    // can tell that the cursor's claim is now false.
    let rewritten: Vec<String> = (0..4).map(|i| line(&format!("{uid}-b{i}"))).collect();
    if let Err(err) = std::fs::write(&transcript, rewritten.join("")) {
        return Outcome::failed(format!("rewriting the transcript: {err}"));
    }
    if let Err(err) = env::wait_until(SETTLE, |conn| {
        Ok(env::count_by_source_event_id(conn, &uid, &format!("{uid}-b3"))? == 1)
    }) {
        return Outcome::failed(format!(
            "the tailer did not recover from a rewrite: {err:#}"
        ));
    }

    // Phase 3 — truncate to a *prefix* of the rewritten file and grow again.
    // The replayed prefix must dedup away and only the genuinely new line count.
    let mut regrown = rewritten[..2].to_vec();
    regrown.push(line(&format!("{uid}-c0")));
    if let Err(err) = std::fs::write(&transcript, regrown.join("")) {
        return Outcome::failed(format!("regrowing the transcript: {err}"));
    }
    if let Err(err) = env::wait_until(SETTLE, |conn| {
        Ok(env::count_by_source_event_id(conn, &uid, &format!("{uid}-c0"))? == 1)
    }) {
        return Outcome::failed(format!(
            "the tailer did not recover from a truncation: {err:#}"
        ));
    }

    // Phase 4 — a torn write. Half a JSON object must never be ingested.
    if let Err(err) = std::fs::write(
        &transcript,
        format!("{}{{\"type\":\"user\",\"uu", regrown.join("")),
    ) {
        return Outcome::failed(format!("tearing the transcript: {err}"));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    // Every uuid ever written, exactly once each, plus the session_start the
    // hook itself produced.
    let expected_ids: Vec<String> = (0..6)
        .map(|i| format!("{uid}-a{i}"))
        .chain((0..4).map(|i| format!("{uid}-b{i}")))
        .chain(std::iter::once(format!("{uid}-c0")))
        .collect();
    let mut wrong = Vec::new();
    for id in &expected_ids {
        match env::count_by_source_event_id(&conn, &uid, id) {
            Ok(1) => {}
            Ok(n) => wrong.push(format!("{id} x{n}")),
            Err(err) => wrong.push(format!("{id}: {err}")),
        }
    }
    let total = env::count_events(&conn, &uid).unwrap_or(0);
    let max = env::max_seq(&conn, &uid).unwrap_or(0);

    let notes = vec![format!(
        "{} transcript lines across 4 rewrites · {total} events · max_seq {max} · \
         scratch {}",
        expected_ids.len(),
        transcript.display()
    )];

    let _ = std::fs::remove_dir_all(&dir);
    let _ = target; // the live session is untouched by this scenario, by design

    if !wrong.is_empty() {
        return Outcome::failed(format!(
            "lines ingested the wrong number of times: {wrong:?}"
        ))
        .with_notes(notes);
    }
    if total != max {
        return Outcome::failed(format!(
            "max_seq {max} != count {total}: the rescan burnt sequence numbers"
        ))
        .with_notes(notes);
    }
    Outcome::passed("cursor recovered from truncate, rewrite and a torn line").with_notes(notes)
}

fn line(uuid: &str) -> String {
    format!("{{\"type\":\"user\",\"uuid\":\"{uuid}\"}}\n")
}

// -------------------------------------------------- (f) kill during ingest

/// Write transcript lines, then `kill -9` the daemon *while it is reading them*.
///
/// The exact window that matters. The cursor used to be saved before the events
/// reached the log, so a death in between left a cursor claiming bytes that had
/// never been ingested — and nothing ever re-read them, because the cursor said they
/// were done. The loss is silent and permanent: no gap in `seq`, no duplicate,
/// nothing for the other scenarios to notice. Only "is every line I wrote in the
/// log?" can see it.
///
/// The kill is timed at the poll rather than at a fixed offset, and repeated, so
/// across rounds it lands at different points inside the read-then-ingest path.
pub async fn kill_during_ingest(target: &Target, rounds: u32) -> Outcome {
    let dir = match env::scratch_dir("ingest") {
        Ok(dir) => dir,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let transcript = dir.join("ingest.jsonl");
    let uid = match protocol::uid::new() {
        Ok(uid) => uid,
        Err(err) => return Outcome::failed(format!("minting a uid: {err:#}")),
    };
    let name = format!("soak-{}", &uid[uid.len() - 6..]);

    let announce = HookPost {
        session_id: name.clone(),
        session_uid: Some(uid.clone()),
        event: "SessionStart".into(),
        payload: serde_json::json!({
            "hook_event_name": "SessionStart",
            "cwd": dir.to_string_lossy(),
            "transcript_path": transcript.to_string_lossy(),
        }),
        wait: false,
    };
    if let Err(err) = std::fs::write(&transcript, "") {
        return Outcome::failed(format!("creating the transcript: {err}"));
    }
    if !env::post_hook(&announce) {
        return Outcome::failed("could not register the synthetic session");
    }
    if let Err(err) = env::wait_until(SETTLE, |conn| Ok(env::count_events(conn, &uid)? >= 1)) {
        return Outcome::failed(format!("the session never registered: {err:#}"));
    }

    let mut written: Vec<String> = Vec::new();
    let mut notes = Vec::new();
    let mut recoveries = Vec::new();

    for round in 0..rounds {
        let info = match env::daemon_info() {
            Ok(info) => info,
            Err(err) => return Outcome::failed(format!("daemon unreachable: {err:#}")),
        };

        // Appended in one write, so the tailer sees whole lines and the only
        // thing under test is whether it survives being killed after reading
        // them and before recording them.
        let batch: Vec<String> = (0..8).map(|i| format!("{uid}-r{round}i{i}")).collect();
        let bytes: String = batch.iter().map(|id| line(id)).collect();
        if let Err(err) = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .and_then(|mut file| std::io::Write::write_all(&mut file, bytes.as_bytes()))
        {
            return Outcome::failed(format!("appending to the transcript: {err}"));
        }
        written.extend(batch);

        // Inside the tail poll (250ms by default, and FSEvents fires sooner), so
        // the axe falls while the scan is in flight rather than long after it.
        let delay = 5 + (protocol::time::now_unix_ms() as u64 % 240);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        if let Err(err) = env::kill_9(info.pid) {
            return Outcome::failed(format!("could not kill pid {}: {err:#}", info.pid));
        }
        match env::wait_for_daemon(env::RECOVERY_TIMEOUT) {
            Ok((_, took)) => recoveries.push(took),
            Err(err) => return Outcome::failed(format!("round {round}: {err:#}")),
        }
    }

    // Every line, exactly once. The daemon has had every chance to re-read them.
    if let Err(err) = env::wait_until(SETTLE, |conn| {
        let last = written.last().cloned().unwrap_or_default();
        Ok(env::count_by_source_event_id(conn, &uid, &last)? == 1)
    }) {
        return Outcome::failed(format!("the last batch never landed: {err:#}"));
    }

    let conn = match env::open_db() {
        Ok(conn) => conn,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let mut missing = Vec::new();
    let mut duplicated = Vec::new();
    for id in &written {
        match env::count_by_source_event_id(&conn, &uid, id) {
            Ok(1) => {}
            Ok(0) => missing.push(id.clone()),
            Ok(n) => duplicated.push(format!("{id} x{n}")),
            Err(err) => missing.push(format!("{id}: {err}")),
        }
    }
    let total = env::count_events(&conn, &uid).unwrap_or(0);
    let max = env::max_seq(&conn, &uid).unwrap_or(0);
    let slowest = recoveries.iter().max().copied().unwrap_or_default();
    notes.push(format!(
        "{rounds} kills timed inside the tail poll · {} lines written · {total} events · \
         max_seq {max} · slowest recovery {:.1}s",
        written.len(),
        slowest.as_secs_f64()
    ));

    let _ = std::fs::remove_dir_all(&dir);
    let _ = target;

    if !missing.is_empty() {
        return Outcome::failed(format!(
            "{} transcript line(s) were skipped permanently — the cursor claimed bytes that \
             never reached the log: {:?}",
            missing.len(),
            &missing[..missing.len().min(5)]
        ))
        .with_notes(notes);
    }
    if !duplicated.is_empty() {
        return Outcome::failed(format!("lines ingested twice: {duplicated:?}")).with_notes(notes);
    }
    if total != max {
        return Outcome::failed(format!("max_seq {max} != count {total}")).with_notes(notes);
    }
    Outcome::passed("every transcript line survived a kill mid-ingest").with_notes(notes)
}

// ------------------------------------------------- (g) concurrent commits

/// Hammer one session from many connections at once and watch a socket read it.
///
/// Committing a `seq` and publishing it are two steps; without a gate
/// around both, a task holding seq 1 can be overtaken by one holding seq 2 —
/// and a socket that has accepted 2 can never accept 1, so the event is dropped
/// on that connection with nothing said. The subscriber here asserts strict
/// succession from its own watermark, which is the only vantage point the defect
/// is visible from: the database is perfectly consistent either way.
pub async fn concurrent_commits(target: &Target, writers: u32) -> Outcome {
    let uid = target.session.session_uid.clone();
    let name = target.session.session_id.clone();
    let tag = protocol::time::now_unix_ms();

    let mut phone = match target.phone().await {
        Ok(phone) => phone,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    let from = env::open_db()
        .and_then(|conn| env::max_seq(&conn, &uid))
        .unwrap_or(0);
    if let Err(err) = phone
        .subscribe_and_drain(&uid, from, Duration::from_millis(500))
        .await
    {
        return Outcome::failed(format!("subscribe: {err:#}"));
    }

    // Concurrent, from separate blocking threads and separate sockets, so the
    // ingests genuinely race rather than being pipelined in order.
    let mut tasks = Vec::new();
    for i in 0..writers {
        let (uid, name) = (uid.clone(), name.clone());
        tasks.push(tokio::task::spawn_blocking(move || {
            env::post_hook(&HookPost {
                session_id: name,
                session_uid: Some(uid),
                event: "PreToolUse".into(),
                payload: serde_json::json!({
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Bash",
                    "tool_input": {"command": "echo order"},
                    "tool_use_id": format!("soak-order-{tag}-{i}"),
                }),
                wait: false,
            })
        }));
    }
    let mut posted = 0u32;
    for task in tasks {
        if task.await.unwrap_or(false) {
            posted += 1;
        }
    }

    // Drain what the socket actually received, in the order it received it.
    let mut received: Vec<u64> = Vec::new();
    let mut resyncs = 0usize;
    loop {
        match phone.next_message(Duration::from_millis(1_500)).await {
            Ok(protocol::ws::ServerMessage::Event { event }) => {
                if event.session_uid != uid {
                    continue;
                }
                if event.kind == protocol::event::EventKind::Resync {
                    resyncs += 1;
                    continue;
                }
                received.push(event.seq);
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    let notes = vec![format!(
        "{posted}/{writers} concurrent commits · {} events delivered live from seq {from} · \
         {resyncs} resync marker(s)",
        received.len()
    )];

    // Strictly successive from the watermark: no reordering, no repeats, and —
    // the failure this exists for — nothing skipped.
    // Zipped rather than a hand-rolled counter: the sequence *is* the index
    // offset from the watermark, and saying so lets the compiler keep the two
    // in step.
    for (expected, seq) in (from + 1..).zip(received.iter()) {
        if *seq != expected {
            return Outcome::failed(format!(
                "a socket saw seq {seq} where {expected} was due: an event was published out \
                 of order and would have been lost on this connection"
            ))
            .with_notes(notes);
        }
    }
    if resyncs > 0 {
        return Outcome::failed(format!(
            "{resyncs} resync marker(s) during live delivery: the daemon published out of order \
             and had to re-read the log to recover"
        ))
        .with_notes(notes);
    }
    if received.is_empty() {
        return Outcome::skipped("no live events arrived; nothing was measured").with_notes(notes);
    }
    Outcome::passed("every commit reached the socket in sequence order").with_notes(notes)
}

/// Pick the run to attack: the newest attached one, or a named reference.
///
/// A **name** has to be resolved the way the daemon resolves it — attached
/// first, then newest — because `cc-1` may name several runs and the dead ones
/// have no supervisor. Picking the first match by list order is how the first
/// version of this harness spent two minutes proving that a session which
/// exited last week does not answer.
pub fn choose_target(reference: Option<&str>) -> Result<SessionSummary> {
    let mut sessions = env::sessions().context("asking ccd for its sessions")?;
    // Attached before detached, then newest first. A ULID sorts by mint time,
    // so the identity is its own tiebreak.
    sessions.sort_by(|a, b| {
        let attached = |s: &SessionSummary| s.link == protocol::event::Link::Attached;
        attached(b)
            .cmp(&attached(a))
            .then_with(|| b.session_uid.cmp(&a.session_uid))
    });

    if let Some(reference) = reference {
        return sessions
            .into_iter()
            .find(|s| s.session_uid == reference || s.session_id == reference)
            .ok_or_else(|| anyhow::anyhow!("no session matches {reference:?}"));
    }
    match sessions
        .into_iter()
        .find(|s| s.link == protocol::event::Link::Attached)
    {
        Some(session) => Ok(session),
        None => bail!(
            "no attached session to soak against; start one with \
             `codeconnect claude` (or soak/run.sh, which starts its own)"
        ),
    }
}

// ------------------------------------------------ (h) slash-command recovery

/// The slash-command release criterion, verbatim from the ruling: **every
/// tested dialog is followed immediately by a successful ordinary phone
/// send, without anyone touching the Mac.**
///
/// `/status`, `/usage` and `/cost` are the three dialog commands the phone
/// offers natively. Each send must come back `composer_recovered` carrying
/// the saved pane — the daemon typed the command, watched the composer
/// disappear, saved the view, pressed Esc, and proved the composer's
/// return — and the ordinary send right after must land as a plain `sent`.
/// One refusal is a skip, not a failure: a busy composer means the
/// measurement would be about contention, not recovery.
pub async fn slash_commands(target: &Target) -> Outcome {
    let uid = target.session.session_uid.clone();
    let mut phone = match target.phone().await {
        Ok(phone) => phone,
        Err(err) => return Outcome::failed(format!("no phone connection: {err:#}")),
    };
    let mut notes = Vec::new();

    for command in ["/status", "/usage", "/cost"] {
        let name = &command[1..];
        let id = format!("soak-snap-{name}-{}", protocol::time::now_unix_ms());
        // The previous probe's turn is starting as this send arrives, and
        // the moment a turn begins the pane can redraw mid-capture — the
        // pre-send interlock then refuses, honestly. That is contention,
        // not recovery, so a refusal here is retried briefly, exactly like
        // the app's own after-Escape sends.
        let mut attempt = phone.send_text(&uid, command, Some(&id)).await;
        for _ in 0..6 {
            match &attempt {
                Ok(protocol::ws::SendTextResult::Refused { .. }) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    attempt = phone.send_text(&uid, command, Some(&id)).await;
                }
                _ => break,
            }
        }
        match attempt {
            Ok(protocol::ws::SendTextResult::ComposerRecovered { pane_snapshot, .. }) => {
                let lines = pane_snapshot
                    .as_deref()
                    .map(|pane| pane.lines().filter(|l| !l.trim().is_empty()).count())
                    .unwrap_or(0);
                if lines == 0 {
                    return Outcome::failed(format!(
                        "{command}: recovered, but without a readable pane snapshot"
                    ))
                    .with_notes(notes);
                }
                notes.push(format!(
                    "{command} → composer_recovered, pane {lines} lines"
                ));
            }
            Ok(protocol::ws::SendTextResult::Refused { reason }) => {
                return Outcome::skipped(format!(
                    "{command} refused ({reason}); the session was not idle enough to measure"
                ))
                .with_notes(notes)
            }
            Ok(other) => {
                return Outcome::failed(format!(
                    "{command}: expected composer_recovered, got {other:?}"
                ))
                .with_notes(notes)
            }
            Err(err) => return Outcome::failed(format!("{command}: {err:#}")).with_notes(notes),
        }

        // The criterion itself: an ordinary send, immediately, untouched.
        let probe = format!("soak-snapprobe-{name}-{}", protocol::time::now_unix_ms());
        match phone
            .send_text(&uid, "reply with exactly: ok", Some(&probe))
            .await
        {
            Ok(protocol::ws::SendTextResult::Sent { .. }) => {
                notes.push(format!("ordinary send right after {command} landed"));
            }
            Ok(other) => {
                return Outcome::failed(format!(
                    "ordinary send after {command} must land untouched, got {other:?}"
                ))
                .with_notes(notes)
            }
            Err(err) => {
                return Outcome::failed(format!("ordinary send after {command}: {err:#}"))
                    .with_notes(notes)
            }
        }
    }

    Outcome::passed("3 dialogs recovered with their panes; 3 immediate ordinary sends landed")
        .with_notes(notes)
}

/// Freeze the shared tmux server, ask the phone to type, and demand an
/// *answer* — bounded, honest about what was not typed — rather than a
/// supervisor silently stuck behind a client tmux will never service. Then
/// thaw and retry with the **same** request id: the refusal must have
/// released the claim, so the retry types fresh rather than being told
/// "unknown".
///
/// The freeze is a `SIGSTOP` on the real server — the stand-in for a server
/// that holds connections and never services them. Two layers put it back:
/// a `Drop` guard for every ordinary exit, and a detached watchdog armed
/// *before* the stop for the exits `Drop` never sees (Ctrl-C, a kill) — a
/// soak must never leave the operator's real server frozen.
pub async fn tmux_freeze(target: &Target) -> Outcome {
    struct Thaw(u32);
    impl Drop for Thaw {
        fn drop(&mut self) {
            // One syscall, no child: a guard that spawned a process to send
            // a signal would be an unbounded wait inside the cleanup path.
            unsafe { libc::kill(self.0 as i32, libc::SIGCONT) };
        }
    }

    let uid = target.session.session_uid.clone();
    let Some(tmux) = protocol::tmux::tmux_bin() else {
        return Outcome::skipped("no tmux on this machine");
    };
    let server_pid = |tmux: &std::path::Path| match protocol::proc::run_deadlined(
        std::process::Command::new(tmux).args([
            "-L",
            protocol::TMUX_SOCKET_NAME,
            "display",
            "-p",
            "#{pid}",
        ]),
        Duration::from_secs(2),
    ) {
        Ok(protocol::proc::RunOutcome::Completed { status, stdout, .. }) if status.success() => {
            String::from_utf8_lossy(&stdout).trim().parse::<u32>().ok()
        }
        _ => None,
    };

    let mut phone = match target.phone().await {
        Ok(phone) => phone,
        Err(err) => return Outcome::failed(format!("{err:#}")),
    };
    match phone.capture(&uid, 40).await {
        Ok(pane) => {
            if protocol::ipc::PromptPresence::InputBox
                .find_match(&pane, None)
                .is_none()
            {
                return Outcome::skipped(
                    "the session's composer is busy; a send would be refused for the right \
                     reason and prove nothing",
                );
            }
        }
        Err(err) => return Outcome::failed(format!("could not read the pane: {err:#}")),
    }

    let request_id = format!("soak-freeze-{}", protocol::time::now_unix_ms());
    let text = "soak: tmux-freeze probe";

    // Read once to arm the watchdog with, then revalidated immediately
    // before the stop — a server restart in between would make the signal
    // stop a stranger.
    let Some(pid) = server_pid(&tmux) else {
        return Outcome::failed("the tmux server did not name its pid");
    };
    // The watchdog first, the stop second: `Drop` runs on every ordinary
    // exit, but not on Ctrl-C or a kill, and those must not leave the
    // operator's server frozen. `set -m` gives the background job its own
    // process group — a terminal Ctrl-C signals the whole foreground group,
    // and a watchdog inside that group would rely on shells ignoring the
    // signal for background jobs rather than on real isolation. The job's
    // pid comes back on stdout so it can be disarmed once the thaw is
    // verified.
    let armed = protocol::proc::run_deadlined(
        std::process::Command::new("/bin/sh").args([
            "-c",
            &format!("set -m; (/bin/sleep 30; /bin/kill -CONT {pid}) >/dev/null 2>&1 & echo $!"),
        ]),
        Duration::from_secs(2),
    );
    let watchdog_pid: i32 = match armed {
        Ok(protocol::proc::RunOutcome::Completed { status, stdout, .. }) if status.success() => {
            match String::from_utf8_lossy(&stdout).trim().parse() {
                Ok(pid) => pid,
                Err(_) => return Outcome::failed("the thaw watchdog did not name its pid"),
            }
        }
        other => return Outcome::failed(format!("could not arm the thaw watchdog: {other:?}")),
    };
    let disarm = |watchdog_pid: i32| unsafe {
        // The job's own group first, so the parked sleep goes with it.
        libc::kill(-watchdog_pid, libc::SIGKILL);
        libc::kill(watchdog_pid, libc::SIGKILL);
    };
    // Revalidated at the last instant: arming took real time, and a server
    // restart inside that window would make the signal stop a stranger.
    if server_pid(&tmux) != Some(pid) {
        disarm(watchdog_pid);
        return Outcome::failed("the tmux server changed while arming; nothing was stopped");
    }
    if unsafe { libc::kill(pid as i32, libc::SIGSTOP) } != 0 {
        disarm(watchdog_pid);
        return Outcome::failed(format!("could not stop tmux server {pid}"));
    }
    let _thaw = Thaw(pid);

    // The whole point: an answer, within the daemon's own supervisor budget,
    // that admits nothing was typed — not silence, not a claim held forever.
    let asked = Instant::now();
    let frozen = phone.send_text(&uid, text, Some(&request_id)).await;
    let waited = asked.elapsed();
    let refusal = match frozen {
        Ok(protocol::ws::SendTextResult::Refused { reason })
            if reason.contains("did not answer") =>
        {
            reason
        }
        Ok(protocol::ws::SendTextResult::Refused { reason }) => {
            return Outcome::failed(format!(
                "refused for a reason other than the tmux deadline — the freeze proved \
                 nothing: {reason:?}"
            ))
        }
        Ok(other) => {
            return Outcome::failed(format!(
                "a frozen tmux produced {other:?} instead of a refusal that released the claim"
            ))
        }
        Err(err) => return Outcome::failed(format!("no answer from a frozen tmux: {err:#}")),
    };
    if waited > Duration::from_secs(8) {
        return Outcome::failed(format!(
            "the refusal took {waited:?}; the bound exists so a wedge costs seconds"
        ));
    }

    drop(_thaw);
    // The server needs a beat to drain what queued while it was stopped —
    // and the thaw is verified before the watchdog is disarmed, because the
    // watchdog is the only rescuer left if CONT did not land.
    tokio::time::sleep(Duration::from_millis(500)).await;
    match protocol::proc::run_deadlined(
        std::process::Command::new(&tmux).args(["-L", protocol::TMUX_SOCKET_NAME, "list-sessions"]),
        Duration::from_secs(2),
    ) {
        Ok(protocol::proc::RunOutcome::Completed { status, .. }) if status.success() => {
            disarm(watchdog_pid);
        }
        other => {
            return Outcome::failed(format!(
                "the server did not answer after the thaw ({other:?}); the watchdog stays \
                 armed to resume it"
            ))
        }
    }

    // Same request id, because the refusal released the claim: this must be a
    // fresh, successful type — not a Duplicate, not an unknown.
    match phone.send_text(&uid, text, Some(&request_id)).await {
        Ok(protocol::ws::SendTextResult::Sent { .. }) => Outcome::passed(format!(
            "frozen tmux refused in {}ms ({refusal:?}); the thawed retry typed fresh",
            waited.as_millis()
        )),
        Ok(other) => Outcome::failed(format!(
            "after the thaw, the retry produced {other:?} instead of typing fresh — the \
             refusal did not release the claim"
        )),
        Err(err) => Outcome::failed(format!("the thawed retry got no answer: {err:#}")),
    }
}

// ------------------------------------------------------- (j–n) live terminal
//
// Five scenarios that attack the Terminal tab's carrier, and three premises
// they share.
//
// **A paired credential.** A live terminal is shell-equivalent authority, so
// the daemon offers it to a per-device token and never to the static bootstrap
// token every other scenario here uses. The harness therefore pairs itself:
// `create_pairing` over the unix socket is the operator's own authority — the
// same call `codeconnect pair` makes — and the hello that redeems the code comes
// back with a device token. One device per run, revoked at the end.
//
// **A scratch session.** Every one of these types into a pane, floods it, drops
// a stream mid-flight or kills the session outright. Doing that to the
// operator's agent would be a harness that broke the thing it was measuring, so
// each scenario spawns its own tmux session stamped with a freshly minted uid —
// which is exactly what a `terminal_attach` resolves against, so the carrier
// cannot tell the difference. Nothing registers it with the daemon: a terminal
// is resolved through tmux, not through the event log.
//
// **Exact credit.** Over-granting output credit, or typing past the input
// window, is answered with `protocol_error`. A harness that tripped either
// incidentally would be measuring its own bug, so both ledgers are mirrored
// locally and refuse before the wire does.

/// The bytes every attach opens with: reset, clear, home. The carrier holds the
/// pane's own output until this paint lands, so the first chunk is the repaint
/// — the same assertion the daemon's own tests make about it.
const SNAPSHOT_PREFIX: &[u8] = b"\x1b[0m\x1b[2J\x1b[H";

/// How long the daemon may take to resolve, spawn, verify and paint.
const PAINT_DEADLINE: Duration = Duration::from_secs(20);

/// How long a marker typed into a pane gets to appear on that pane's screen
/// before the attach it is meant to prove is abandoned. The shell is `/bin/sh`
/// running one `printf`, so this is generous by two orders of magnitude.
const MARKER_DEADLINE: Duration = Duration::from_secs(10);

/// The daemon's output-stall deadline, mirrored.
///
/// Source of truth: `OUTPUT_STALL_DEADLINE` in `mac/ccd/src/terminal.rs`. This
/// is a copy rather than an import because `ccd` builds no library target, so
/// nothing outside that binary can name its constants — which means a drift
/// between the two is **invisible here** and would surface only as this
/// scenario failing with a close outside the window below. Promoting the
/// constant into `protocol`, the way the two `attachment_limit` reasons now
/// are, is the fix; it is not done here because this pass is scoped to the
/// harness.
const DAEMON_STALL_DEADLINE: Duration = Duration::from_secs(30);

/// How far *under* the deadline a correct close may measure from this side.
///
/// The daemon arms the deadline when its reader begins forwarding a chunk it
/// cannot finish; the earliest thing this side can see is the partial chunk
/// pushed microseconds later. So a correct close always measures a little less
/// than the full thirty seconds here and a strict `>= 30s` would fail on
/// arithmetic rather than on behaviour. Two seconds covers what the harness's
/// own scheduling can add on a loaded machine, and it is three orders of
/// magnitude above the regression the lower bound exists to catch: a close at
/// t=0, which is what a deadline armed in the wrong place produces and what the
/// daemon's own `an_output_stall_closes_as_slow_consumer_and_reaps` asserts
/// against from the inside.
const STALL_EARLY_SLACK: Duration = Duration::from_secs(2);

/// How far over the deadline a loaded machine may run before it is a finding.
/// Eight seconds is 27% of the deadline — room for a busy scheduler, and well
/// short of the point where "closed late" stops being distinguishable from
/// "closed for some other reason".
const STALL_LATE_SLACK: Duration = Duration::from_secs(8);

/// The ceiling the starvation scenario waits before calling a terminal that was
/// never closed a failure. Above [`DAEMON_STALL_DEADLINE`] plus
/// [`STALL_LATE_SLACK`] on purpose, so "closed late" and "never closed" are
/// different sentences in the report rather than the same one.
const STALL_DEADLINE: Duration = Duration::from_secs(45);

/// How long a scratch session may outlive the process that made it before the
/// watchdog removes it.
const SCRATCH_TTL_SECS: u64 = 300;

/// The output window the starvation scenario grants and never renews. Small
/// enough that the first flooded chunk outruns it, and non-zero because zero is
/// itself a protocol error.
const STARVED_CREDIT: u32 = 512;

/// Whether a scratch session is still on the shared server.
///
/// Three-valued for the reason [`env::TmuxAnswer`] is: `has-session` exiting
/// non-zero is tmux saying the session is gone, and tmux saying nothing at all
/// is not the same statement. Folding the second into the first is what
/// disarmed a live session's last rescuer, because the frozen server that makes
/// `kill-session` fail is the same one that makes `has-session` time out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gone {
    Yes,
    No,
    Unknown,
}

/// Which of the two "this is not the session this run created" situations tmux
/// described. Kept apart only so the report can tell them apart: the first is
/// the ordinary end of a scenario that killed its own pane, and the second is
/// the one worth shouting about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Foreign {
    /// No session answers to the name at all.
    Absent,
    /// A session is there and it is not this one — a different run's uid, or no
    /// stamp at all. This is the case the stamp check exists for.
    Stranger,
}

/// Whether the session now answering to this run's scratch name *is* this run's
/// session.
///
/// Three-valued, and the third value is why this exists: a tmux name is not an
/// identity. tmux hands a freed name to whoever asks next, and `Scratch` mints
/// its name and arms a rescuer on it before the session exists, so "a session
/// called `soak-term-…` is on the server" has never been the same statement as
/// "this run's session is on the server". Everything destructive in teardown
/// asks this instead of asking the name, and only [`Stamp::Ours`] answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stamp {
    /// tmux returned exactly the uid `spawn` stamped into the session
    /// environment. The only state that permits destroying anything.
    Ours,
    /// tmux answered, and what it described is not this run's session.
    NotOurs(Foreign),
    /// Nothing was learned: tmux did not answer, or answered something that
    /// settles neither way. Carries the sentence that says so.
    Unreadable(String),
}

impl Stamp {
    /// Whether this state authorises destroying anything. Exactly one does.
    fn permits_a_kill(&self) -> bool {
        matches!(self, Stamp::Ours)
    }

    /// One clause naming what tmux said, for a sentence that must never read as
    /// a measurement.
    fn situation(&self) -> String {
        match self {
            Stamp::Ours => "it carries this run's stamp".to_string(),
            Stamp::NotOurs(Foreign::Absent) => "tmux has no session by that name".to_string(),
            Stamp::NotOurs(Foreign::Stranger) => {
                "the session holding that name carries another stamp, or none".to_string()
            }
            Stamp::Unreadable(why) => why.clone(),
        }
    }
}

/// What teardown is permitted to do, decided before anything is destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Teardown {
    /// Signal the pane group, kill the session, and give up the rescuers once
    /// it is provably gone.
    Remove,
    /// Destroy nothing, and disarm the watchdog — because an armed watchdog over
    /// a name this run does not own is itself the destructive act.
    DisarmOnly,
    /// Destroy nothing and disarm nothing. Safe only because the watchdog
    /// proves the stamp itself before it kills, so leaving it armed can remove
    /// nothing but this run's own session.
    HandsOff,
}

/// A throwaway tmux session on the shared server, for a terminal to attack.
///
/// Stamped in its session environment with a minted uid, which is what the
/// daemon resolves an attach against. Three layers remove it: a `kill-session`,
/// a SIGKILL of the pane's process group, and a detached watchdog armed
/// **before the session is created** that removes it after a TTL. A soak must
/// not leave sessions behind on the operator's server.
struct Scratch {
    name: String,
    uid: String,
    /// The pane's shell as it read at creation. tmux gives each pane its own
    /// process group, so the negative pid reaches whatever it spawned as well.
    /// Zero when it was never read — which is a case teardown has to survive,
    /// not assume away.
    ///
    /// Never the value that is signalled. Pids are recycled, so a number read
    /// minutes ago names *some* process group and not necessarily this one;
    /// [`Scratch::signal_pane_group`] re-reads it from the session immediately
    /// before it fires. What this field carries is only whether a pane was ever
    /// read at all, which is what `kill_pane` reports on.
    pane_pid: i32,
    watchdog: i32,
    /// Whether a session by this name may exist on the shared server.
    ///
    /// Set the moment `new-session` is *issued*, not once it is confirmed. A
    /// `new-session` whose answer never came back may perfectly well have
    /// created the session — `run_deadlined` kills the client, not the server —
    /// and teardown keyed on "we saw it succeed" leaks exactly those.
    live: bool,
    /// Teardown runs once. `close` runs it; `Drop` runs it only for the paths
    /// `close` never reached (an early `return`, a panic).
    reclaimed: bool,
}

impl Scratch {
    fn spawn(tag: &str) -> Result<Scratch> {
        let tmux = protocol::tmux::tmux_bin().context("tmux is not installed")?;
        let uid = protocol::uid::new().context("minting a uid")?;
        let name = format!("soak-term-{tag}-{}", &uid[uid.len() - 6..]);

        // The watchdog is armed *before* the session exists.
        //
        // It is keyed on the session name, which is already known, so arming it
        // early costs nothing — a watchdog that fires on a session that was
        // never created is a `kill-session` that finds nothing. What it buys is
        // the window the previous order left open: between `new-session`
        // returning and the watchdog being armed the session existed on the
        // operator's shared server with no TTL and no rescuer, and the
        // `pane_pid` read in between is an `env::tmux` call that can time out.
        // One such timeout returned `Err` from a guard holding `pane_pid: 0,
        // watchdog: 0`, so nothing killed anything and the session and its
        // `/bin/sh` leaked permanently.
        //
        // The uid goes to the watchdog as well as to the session, so the
        // rescuer can prove the thing it is about to remove is the thing this
        // run created — see [`watchdog_script`]. Arming reads the uid, so it is
        // hoisted out of the struct literal, which would otherwise have moved
        // it into the `uid` field first.
        let watchdog = arm_watchdog(&tmux, &format!("={name}"), &uid)?;
        let mut scratch = Scratch {
            name: name.clone(),
            uid,
            pane_pid: 0,
            watchdog,
            live: false,
            reclaimed: false,
        };

        let stamp = format!("{}={}", protocol::ENV_SESSION_UID, scratch.uid);
        let cwd = std::env::temp_dir();
        // `/bin/sh` rather than the operator's login shell: a predictable pane
        // that echoes what is typed into it, with none of a personal rc file's
        // colour, prompt escapes or startup output in the stream being asserted
        // on.
        let created = env::tmux_answer(&[
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "80",
            "-y",
            "24",
            "-c",
            &cwd.to_string_lossy(),
            "-e",
            &stamp,
            "--",
            "/bin/sh",
        ]);
        match created {
            env::TmuxAnswer::Ok(_) => scratch.live = true,
            // tmux answered and refused. Nothing was created, so the guard owns
            // nothing and its `Drop` disarms the watchdog on the way out.
            env::TmuxAnswer::Failed { .. } => {
                return Err(anyhow::anyhow!(
                    "{}",
                    created.complaint(&format!("creating the scratch session {name}"))
                ))
            }
            // No answer, which is not "no session". The guard claims it, so the
            // `Drop` below reclaims it by name and the watchdog stays armed.
            env::TmuxAnswer::Unknown(_) => {
                scratch.live = true;
                return Err(anyhow::anyhow!(
                    "{}",
                    created.complaint(&format!("creating the scratch session {name}"))
                ));
            }
        }

        // From here the guard owns the session — which is now true rather than
        // merely intended, because every failure below drops a guard that can
        // reclaim it by name.
        scratch.pane_pid = env::tmux(&["display", "-p", "-t", &scratch.target(), "#{pane_pid}"])
            .and_then(|out| out.trim().parse().ok())
            .unwrap_or(0);
        if scratch.pane_pid <= 1 {
            return Err(anyhow::anyhow!(
                "the scratch session {name} named no pane pid"
            ));
        }
        Ok(scratch)
    }

    /// `=name`, so tmux matches this session exactly and never a prefix of a
    /// name somebody else is using.
    fn exact(&self) -> String {
        format!("={}", self.name)
    }

    fn target(&self) -> String {
        format!("={}:", self.name)
    }

    /// How many tmux clients are attached right now, or why that is not known.
    ///
    /// `Err` covers both "tmux refused the question" — which for `list-clients`
    /// means there is no such session, itself a finding since every caller here
    /// asks about a session it has just been using — and "tmux did not answer".
    /// Neither is a count, and neither may reach an assertion as one:
    /// `unwrap_or(0)` was the defect, and zero is the *passing* value at every
    /// call site.
    fn clients(&self) -> std::result::Result<usize, String> {
        let answer =
            env::tmux_answer(&["list-clients", "-t", &self.exact(), "-F", "#{client_pid}"]);
        match &answer {
            env::TmuxAnswer::Ok(out) => {
                Ok(out.lines().filter(|line| !line.trim().is_empty()).count())
            }
            _ => Err(answer.complaint(&format!(
                "counting the tmux clients on the scratch session {}",
                self.name
            ))),
        }
    }

    /// Wait for the disposable clients to go, reporting how many are left — or
    /// that the count could not be taken.
    ///
    /// The early return is on a *count*, never on a failure to take one. With
    /// `unwrap_or(0)` and every caller passing `want = 0`, one timed-out poll
    /// satisfied `0 <= 0` and ended the wait reporting "0 clients left", which
    /// is the passing answer at each of the three call sites. An unreadable
    /// poll now keeps the loop running for the whole timeout, because an
    /// indeterminate answer may resolve, and is returned as itself if it never
    /// does.
    fn wait_for_clients(
        &self,
        want: usize,
        timeout: Duration,
    ) -> std::result::Result<usize, String> {
        let start = Instant::now();
        loop {
            let seen = self.clients();
            if matches!(seen, Ok(count) if count <= want) || start.elapsed() > timeout {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Wait for the window to read exactly `cols`x`rows`, or say what it read.
    ///
    /// Wait until the daemon's disposable client declares `cols` as its own
    /// width — the causal signal of a resize, read from the *client* rather than
    /// the window on purpose.
    ///
    /// Measured on tmux 3.7b: the daemon attaches `-f ignore-size`, and such a
    /// client governs the window's geometry only while it is the sole client on
    /// the *whole server*. Any other client anywhere — a different session, any
    /// size, even smaller — suppresses it entirely and the window keeps its
    /// created size. That is the "a phone never shrinks the human at the Mac"
    /// property doing its job, and it is not an edge case here: this gauntlet's
    /// own target run holds a client on the server for the whole run, so the
    /// window never follows. An assertion on `#{window_width}` therefore fails
    /// against a daemon that resized correctly, which is the opposite of what a
    /// gauntlet is for.
    ///
    /// What the daemon *can* do, and does, is issue `refresh-client -C
    /// {cols}x{rows}`, and that updates the client's own declared width whether
    /// or not the window follows. So a daemon that dropped the resize leaves the
    /// width at the value it attached with, and one that forwarded it moves the
    /// width — the exact fact to observe, and it holds no matter who else is on
    /// the server. `#{client_height}` is not reported for a control-mode client,
    /// so the width alone carries the proof; the resize changes both dimensions
    /// and the width is the observable half. tmux failing to answer is not the
    /// width we want and must not pass as one, which is why this reads the
    /// tri-state.
    fn wait_for_client_width(
        &self,
        cols: u16,
        timeout: Duration,
    ) -> std::result::Result<(), String> {
        let want = cols.to_string();
        let start = Instant::now();
        loop {
            let answer =
                env::tmux_answer(&["list-clients", "-t", &self.exact(), "-F", "#{client_width}"]);
            let last = match &answer {
                env::TmuxAnswer::Ok(out) if out.trim() == want => return Ok(()),
                env::TmuxAnswer::Ok(out) => format!("{:?}", out.trim()),
                _ => answer.complaint("reading the daemon client's declared width"),
            };
            if start.elapsed() > timeout {
                return Err(format!(
                    "the daemon client's width never read {want}; the last thing tmux said \
                     was {last}"
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Print a marker into the pane and wait until the pane's own screen shows
    /// it. Returns the bytes an honest snapshot of that screen must contain.
    ///
    /// This is what makes a snapshot assertion causal. Control mode streams only
    /// what a pane prints *next*, so a marker printed before the attach — to a
    /// pane that then goes quiet — can reach the phone only inside the paint.
    /// The daemon's own `a_fresh_attach_paints_the_existing_screen` proves the
    /// same property the same way.
    fn screen_marker(&self, tag: &str) -> std::result::Result<Vec<u8>, String> {
        // Spelled so the marker's literal text is on the screen exactly once:
        // the echoed command line shows `SNAP_%s`, and only its output shows
        // the stamp. Without that, the echo of the command would satisfy the
        // assertion and the paint would prove nothing.
        let stamp = format!("{tag}-{}", protocol::time::now_unix_ms());
        let marker = format!("SNAP_{stamp}");
        let typed = format!("printf 'SNAP_%s\\n' {stamp}\n");
        if env::tmux(&["send-keys", "-t", &self.target(), "-l", &typed]).is_none() {
            return Err(format!("could not type the marker {marker} into the pane"));
        }
        // Polled rather than assumed: `send-keys` returns once tmux holds the
        // bytes, not once the shell has run them, so an attach fired straight
        // after would race the print and fail on a screen not yet drawn.
        let start = Instant::now();
        loop {
            let answer = env::tmux_answer(&["capture-pane", "-p", "-t", &self.target()]);
            let last = match &answer {
                env::TmuxAnswer::Ok(screen) if screen.contains(&marker) => {
                    return Ok(marker.into_bytes())
                }
                env::TmuxAnswer::Ok(_) => "the pane has not printed it".to_string(),
                _ => answer.complaint("capturing the scratch pane"),
            };
            if start.elapsed() > MARKER_DEADLINE {
                return Err(format!("the pane never showed {marker}: {last}"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Make the pane print far more than any credit window covers.
    fn flood(&self) -> bool {
        env::tmux(&["send-keys", "-t", &self.target(), "seq 1 20000", "Enter"]).is_some()
    }

    /// Whether the session now answering to this run's name is this run's
    /// session, asked of the stamp rather than of the name.
    fn stamp(&self) -> Stamp {
        stamp_from_answer(
            &self.uid,
            &env::tmux_answer(&[
                "show-environment",
                "-t",
                &self.exact(),
                protocol::ENV_SESSION_UID,
            ]),
        )
    }

    /// SIGKILL the pane's process group, having first proved there is a pane of
    /// this run's to signal.
    ///
    /// Both halves are load-bearing. A name is not an identity on a tmux server,
    /// so the stamp is what says the session at this name is the one this run
    /// created; and pids are recycled, so even on the right session the pid has
    /// to be read *now* rather than taken from `pane_pid`, which was read at
    /// creation. Signalling a stale pid on an unverified name is how a harness
    /// SIGKILLs a stranger's process group.
    fn signal_pane_group(&self) -> std::result::Result<i32, String> {
        let stamp = self.stamp();
        if !stamp.permits_a_kill() {
            return Err(format!(
                "the scratch session {} is not confirmed to be this run's ({}), so no process \
                 group was signalled",
                self.name,
                stamp.situation()
            ));
        }
        self.signal_confirmed_pane_group()
    }

    /// The same signal, for the one caller that has *just* confirmed the stamp
    /// and must not pay a second five-second probe to confirm it again.
    fn signal_confirmed_pane_group(&self) -> std::result::Result<i32, String> {
        let answer = env::tmux_answer(&["display", "-p", "-t", &self.target(), "#{pane_pid}"]);
        let pid = match &answer {
            env::TmuxAnswer::Ok(out) => out.trim().parse::<i32>().unwrap_or(0),
            _ => {
                return Err(answer.complaint(&format!(
                    "re-reading the pane pid of the scratch session {}",
                    self.name
                )))
            }
        };
        if pid <= 1 {
            // The `> 1` guard is what stops `kill(-0, …)` signalling the
            // harness's own process group and `kill(-1, …)` signalling every
            // process this user owns.
            return Err(format!(
                "the scratch session {} named pane pid {pid}, which is not a process group this \
                 may signal",
                self.name
            ));
        }
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(pid, libc::SIGKILL);
        }
        Ok(pid)
    }

    /// End the session the way the kill scenarios end a daemon: `kill -9`, here
    /// on the pane's process group. The shell is gone, so tmux closes the
    /// window, and the last window closing takes the session with it.
    ///
    /// `false` means nothing was signalled, and the reason is printed rather
    /// than swallowed: since the signal is bound to the stamp, "no pane was ever
    /// read" is no longer the only way to get here.
    fn kill_pane(&mut self) -> bool {
        if self.pane_pid <= 1 {
            return false;
        }
        match self.signal_pane_group() {
            Ok(_) => {
                self.pane_pid = 0;
                true
            }
            Err(why) => {
                eprintln!("         · {why}");
                false
            }
        }
    }

    /// Is the session verifiably gone, and what did tmux say?
    ///
    /// The answer comes back alongside the verdict because the [`Gone::Unknown`]
    /// report has to name what tmux actually said: "did not answer" and
    /// "answered something this cannot read" are both Unknown and they are not
    /// the same sentence.
    fn probe_gone(&self) -> (Gone, env::TmuxAnswer) {
        let answer = env::tmux_answer(&["has-session", "-t", &self.exact()]);
        (gone_from_answer(&answer), answer)
    }

    /// Ordinary teardown. Idempotent with [`Drop`], which is what runs it on
    /// every path `close` does not reach.
    fn close(mut self) {
        self.reclaim();
    }

    /// Remove the session, and give up the rescuers only once it is provably
    /// gone.
    fn reclaim(&mut self) {
        if self.reclaimed {
            return;
        }
        let stamp = self.stamp();
        self.reclaim_with(stamp);
    }

    /// Teardown given what the stamp said, split from the probe so the decision
    /// can be exercised without a tmux server.
    ///
    /// The order matters and each step is here for a measured reason. The
    /// `kill-session` is bounded (five seconds) and cannot be replaced by the
    /// process-group signal alone, because the path that leaks a session is
    /// precisely the one where `pane_pid` was never read. And nothing is
    /// disarmed on anything but [`Gone::Yes`]: an earlier version read a
    /// timed-out `has-session` as "gone", zeroed `pane_pid` before the check so
    /// `Drop` would also do nothing, and SIGKILLed the watchdog — leaving a live
    /// `/bin/sh` in a live tmux session on the operator's shared server with no
    /// TTL and no rescuer at all.
    ///
    /// Everything destructive sits behind the stamp, because the whole of this
    /// used to be bound to the *name*. `spawn` arms the watchdog on `={name}`
    /// before the session exists, so a `new-session` refused because a foreign
    /// session already held that exact name returned `Err` with `live = false`,
    /// and the `Drop` that followed saw the foreign session listed, read "not
    /// gone", and left the watchdog armed — which then killed a session this
    /// harness never created.
    fn reclaim_with(&mut self, stamp: Stamp) {
        self.reclaimed = true;
        match teardown_for(&stamp) {
            Teardown::Remove => {}
            // Confirmed to be somebody else's, or nothing at all. Destroy
            // nothing — and *disarm*, because leaving a rescuer armed over a
            // name this run does not own is exactly what removes a stranger's
            // session.
            Teardown::DisarmOnly => {
                self.live = false;
                self.pane_pid = 0;
                self.disarm_watchdog();
                if stamp == Stamp::NotOurs(Foreign::Stranger) {
                    eprintln!(
                        "         · the scratch session name {} is held by a session this run \
                         did not create; nothing was killed and its watchdog was disarmed so it \
                         cannot kill it either",
                        self.name
                    );
                }
                return;
            }
            // Nothing was learned, so nothing may be destroyed on the strength
            // of it. The watchdog stays armed and that is safe: it proves the
            // stamp itself before it kills anything.
            Teardown::HandsOff => {
                eprintln!(
                    "         · the scratch session {} could not be identified ({}), so nothing \
                     was killed; its watchdog stays armed and removes it within \
                     {SCRATCH_TTL_SECS}s, and only if the stamp reads as this run's by then",
                    self.name,
                    stamp.situation()
                );
                return;
            }
        }

        let _ = self.signal_confirmed_pane_group();
        if self.live {
            let _ = env::tmux(&["kill-session", "-t", &self.exact()]);
        }
        let (verdict, answer) = self.probe_gone();
        match verdict {
            Gone::Yes => {
                self.live = false;
                self.pane_pid = 0;
                self.disarm_watchdog();
            }
            // Not proved gone. Both rescuers stay in place, and the operator is
            // told, because until the TTL expires this is a live session on the
            // server their own agents run on.
            Gone::No => eprintln!(
                "         · the scratch session {} is not confirmed gone (tmux still lists it); \
                 its watchdog stays armed and removes it within {SCRATCH_TTL_SECS}s",
                self.name
            ),
            Gone::Unknown => eprintln!(
                "         · the scratch session {} is not confirmed gone ({}); its watchdog \
                 stays armed and removes it within {SCRATCH_TTL_SECS}s",
                self.name,
                answer.complaint("asking the shared server whether it is still there")
            ),
        }
    }

    fn disarm_watchdog(&mut self) {
        if self.watchdog > 1 {
            // Negative first: the watchdog was forked into its own process
            // group precisely so this reaches the `sleep` it is parked on. The
            // `> 1` guard is the same one `signal_pane_group` explains.
            unsafe {
                libc::kill(-self.watchdog, libc::SIGKILL);
                libc::kill(self.watchdog, libc::SIGKILL);
            }
        }
        self.watchdog = 0;
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.reclaim();
    }
}

/// What one `has-session` answer proves about a scratch session.
///
/// Free of the spawn so it can be exercised, for the reason [`env::classify`]
/// gives for the same split: as an inline `match` the only way to reach the
/// interesting arms would be to break the operator's shared server, which is to
/// say never under `cargo test`.
///
/// The defect it replaces mapped *every* completed non-zero exit to
/// [`Gone::Yes`], and only some of them mean absence. Measured on tmux 3.7b, all
/// four of these exit 1: `can't find session: X` (gone), `error connecting to …
/// (No such file or directory)` (no server, so gone), `error connecting to …
/// (Permission denied)` and `… (Socket operation on non-socket)` — and the last
/// two say nothing whatever about whether a session exists. [`Gone::Yes`] is
/// what disarms the watchdog, so reading either of them as absence retires the
/// only rescuer a live session has left.
fn gone_from_answer(answer: &env::TmuxAnswer) -> Gone {
    match answer {
        // `has-session` exits zero only when the session is there.
        env::TmuxAnswer::Ok(_) => Gone::No,
        // A client that was signalled never finished its statement, and the
        // stderr it had written so far may be a fragment of one. Whatever it
        // says, it is not a completed answer, so it settles nothing — this is
        // deliberately decided before the words are read at all.
        env::TmuxAnswer::Failed { status: None, .. } => Gone::Unknown,
        // A completed refusal. Which refusal it is, tmux says in its own words,
        // and the production classifier is what reads them — one needle list on
        // this machine, not two that can drift apart.
        env::TmuxAnswer::Failed {
            status: Some(_),
            stderr,
        } => match protocol::tmux::presence_from_probe(false, stderr) {
            protocol::tmux::SessionPresence::Gone => Gone::Yes,
            protocol::tmux::SessionPresence::Unknown(_) => Gone::Unknown,
            // Unreachable from a `false`, and named rather than swallowed by a
            // bare `_` so that a variant added to `SessionPresence` later has to
            // be considered here instead of quietly landing in whichever arm the
            // wildcard happened to cover.
            protocol::tmux::SessionPresence::Present => Gone::No,
        },
        env::TmuxAnswer::Unknown(_) => Gone::Unknown,
    }
}

/// What one `show-environment -t <exact> CODECONNECT_SESSION_UID` answer proves
/// about who holds the name.
///
/// Free of the spawn for the same reason as [`gone_from_answer`], and reusing
/// the daemon's own [`protocol::tmux::owner_from_probe`] rather than re-reading
/// tmux's wording here, because a second copy of that mapping is a second thing
/// to keep true.
///
/// Confirmation is exit 0 *and* a stamp equal to this run's uid, and nothing
/// else is confirmation. Measured on tmux 3.7b, the near misses each mean
/// something different and none of them mean "ours":
///
/// | tmux said | meaning |
/// |---|---|
/// | exit 0, `CODECONNECT_SESSION_UID=<other>` | a different run's session holds the name |
/// | exit 1, `unknown variable: …` | a session is there and carries no stamp at all |
/// | exit 1, `no such session: =X` | the name is free |
/// | exit 1, `… (Permission denied)` | nothing was learned |
///
/// Row two is the one worth spelling out: this run's session is stamped by
/// `new-session -e`, so a session that answers "unknown variable" is provably
/// *not* it, and reading that as ours would authorise killing a session made by
/// hand that happened to take the same name.
fn stamp_from_answer(uid: &str, answer: &env::TmuxAnswer) -> Stamp {
    match answer {
        env::TmuxAnswer::Ok(stdout) => match protocol::tmux::owner_from_probe(true, stdout, "") {
            (_, Some(protocol::tmux::SessionOwner::Uid(seen))) if seen == uid => Stamp::Ours,
            // Present and identified as somebody else, or present and not
            // identified at all. Either way, not this run's to destroy.
            _ => Stamp::NotOurs(Foreign::Stranger),
        },
        // Signalled rather than exited: see [`gone_from_answer`]. A truncated
        // stderr must not be read as though it were tmux's whole sentence.
        env::TmuxAnswer::Failed { status: None, .. } => {
            Stamp::Unreadable(answer.complaint("reading the scratch session's stamp"))
        }
        env::TmuxAnswer::Failed {
            status: Some(_),
            stderr,
        } => match protocol::tmux::owner_from_probe(false, "", stderr) {
            // `unknown variable`: tmux is confirming the session exists and has
            // no such variable, which is proof it is not this stamped one.
            (protocol::tmux::SessionPresence::Present, _) => Stamp::NotOurs(Foreign::Stranger),
            (protocol::tmux::SessionPresence::Gone, _) => Stamp::NotOurs(Foreign::Absent),
            (protocol::tmux::SessionPresence::Unknown(_), _) => {
                Stamp::Unreadable(answer.complaint("reading the scratch session's stamp"))
            }
        },
        env::TmuxAnswer::Unknown(_) => {
            Stamp::Unreadable(answer.complaint("reading the scratch session's stamp"))
        }
    }
}

/// What teardown may do, given what the stamp said. One place, so "only a
/// confirmed stamp authorises a kill" is a fact about a function rather than a
/// property spread across three call sites.
fn teardown_for(stamp: &Stamp) -> Teardown {
    match stamp {
        Stamp::Ours => Teardown::Remove,
        Stamp::NotOurs(_) => Teardown::DisarmOnly,
        Stamp::Unreadable(_) => Teardown::HandsOff,
    }
}

/// How long a rejected arm waits for the forked rescuer to name itself. The
/// write happens microseconds after the fork, so this is slack for a loaded
/// machine and not a real expectation.
const WATCHDOG_PID_FILE_PATIENCE: Duration = Duration::from_millis(500);

/// The shell the detached rescuer runs, less its arguments.
///
/// Everything variable is passed to `/bin/sh` as a positional parameter — see
/// [`watchdog_argv`] — so nothing here is built by string interpolation except
/// the TTL, which is a `u64`, and the variable name, which is a compile-time
/// constant. A session name or a uid can therefore never be word-split, and
/// never reparsed as a flag, no matter what it contains.
///
/// The rescuer proves the stamp itself, immediately before it kills. It has to:
/// the watchdog is armed on `={name}` *before* the session exists, and a name is
/// not an identity, so a rescuer that only knew the name would remove whatever
/// held it when the TTL expired — including a session this harness never
/// created. `show-environment` exiting non-zero prints nothing on stdout, so the
/// comparison fails closed for an unstamped session, a missing session and a
/// tmux that cannot be reached alike.
///
/// `set -m` gives the watchdog its own process group: a terminal's Ctrl-C
/// signals the whole foreground group, and a rescuer inside that group would be
/// relying on shells ignoring the signal for background jobs rather than on real
/// isolation. The pid is recorded to a file *and* echoed, in that order, because
/// the `&` has already run by the time stdout is read and an arm that fails
/// after it would otherwise leave a `sleep` nobody can name — see
/// [`recover_forked_watchdog`]. It is written by the parent shell rather than
/// inside the subshell because `$$` in a subshell is the *parent's* pid in every
/// sh this ships against; `$!` in the parent is the subshell's, which is also
/// its process group id, which is what disarming needs.
fn watchdog_script(ttl_secs: u64) -> String {
    format!(
        "set -m; (/bin/sleep {ttl_secs}; \
         said=$(\"$1\" \"$2\" \"$3\" show-environment -t \"$4\" {var} 2>/dev/null); \
         [ \"$said\" = \"{var}=$5\" ] && \"$1\" \"$2\" \"$3\" kill-session -t \"$4\") \
         >/dev/null 2>&1 & echo $! > \"$6\"; echo $!",
        var = protocol::ENV_SESSION_UID,
    )
}

/// The complete argv for `/bin/sh`, so a test can arm the same rescuer against a
/// private `-S` socket instead of the operator's shared one.
///
/// `server` is the socket flag and its value as a pair — `["-L",
/// "codeconnect"]` in production. The rescuer runs on the shared server because
/// that is where the scratch session is; what changed is that it now proves what
/// it is about to remove.
fn watchdog_argv(
    tmux: &std::path::Path,
    server: [&str; 2],
    exact: &str,
    uid: &str,
    ttl_secs: u64,
    pid_file: &std::path::Path,
) -> Vec<std::ffi::OsString> {
    vec![
        std::ffi::OsString::from("-c"),
        std::ffi::OsString::from(watchdog_script(ttl_secs)),
        // `$0`, which the script never reads. The parameters it does read start
        // after it.
        std::ffi::OsString::from("--"),
        tmux.as_os_str().to_os_string(),
        std::ffi::OsString::from(server[0]),
        std::ffi::OsString::from(server[1]),
        std::ffi::OsString::from(exact),
        std::ffi::OsString::from(uid),
        pid_file.as_os_str().to_os_string(),
    ]
}

/// Where one arm's rescuer records its pid. Unique per call, so two scratch
/// sessions in one run — or two runs at once — cannot read each other's.
fn watchdog_pid_file() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ccsoak-watchdog-{}-{}-{}.pid",
        std::process::id(),
        protocol::time::now_unix_ms(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ))
}

/// Kill the rescuer a failed arm had already forked, and say which happened.
///
/// The failure this closes: the `&` runs before the pid reaches stdout, so a
/// blown two-second deadline — or stdout that does not parse — used to leave a
/// `/bin/sleep {SCRATCH_TTL_SECS}` that no `> 1` guard would ever signal and
/// that outlived the whole gauntlet. The pid file is the fork's own record of
/// itself, so recovery does not depend on the stdout that just failed.
///
/// The poll exists because the write lands microseconds *after* the fork, and an
/// arm can fail inside that window. If nothing appears, the fork never reached
/// the background job and there is nothing to recover — which the sentence says,
/// rather than leaving the operator to wonder.
fn recover_forked_watchdog(pid_file: &std::path::Path) -> String {
    let start = Instant::now();
    let found = loop {
        let pid = std::fs::read_to_string(pid_file)
            .ok()
            .and_then(|said| said.trim().parse::<i32>().ok())
            .filter(|pid| *pid > 1);
        if pid.is_some() || start.elapsed() >= WATCHDOG_PID_FILE_PATIENCE {
            break pid;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = std::fs::remove_file(pid_file);
    match found {
        Some(pid) => {
            // Negative first, for the reason `disarm_watchdog` gives: the group
            // is what the `sleep` is parked in.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
            format!("the rescuer it had already forked (pid {pid}) was killed")
        }
        None => format!(
            "no rescuer named itself within {WATCHDOG_PID_FILE_PATIENCE:?}, so the fork never \
             reached the background job and nothing was left running"
        ),
    }
}

/// One sentence for a failed arm, which also closes what the failure left
/// running. Every failure path below goes through this.
fn watchdog_failure(pid_file: &std::path::Path, why: String) -> anyhow::Error {
    anyhow::anyhow!("{why}; {}", recover_forked_watchdog(pid_file))
}

/// Fork the detached rescuer that removes `exact` after a TTL — but only if it
/// still carries `uid` by then — and come back with the pid that disarms it.
fn arm_watchdog(tmux: &std::path::Path, exact: &str, uid: &str) -> Result<i32> {
    let pid_file = watchdog_pid_file();
    let armed = protocol::proc::run_deadlined(
        std::process::Command::new("/bin/sh").args(watchdog_argv(
            tmux,
            ["-L", protocol::TMUX_SOCKET_NAME],
            exact,
            uid,
            SCRATCH_TTL_SECS,
            &pid_file,
        )),
        Duration::from_secs(2),
    );
    let said = match armed {
        Ok(protocol::proc::RunOutcome::Completed { status, stdout, .. }) if status.success() => {
            String::from_utf8_lossy(&stdout).trim().to_string()
        }
        other => {
            return Err(watchdog_failure(
                &pid_file,
                format!("could not arm the scratch session's cleanup watchdog: {other:?}"),
            ))
        }
    };
    // Rejected, never defaulted to zero. A pid that does not parse is a rescuer
    // this run can never disarm: the `> 1` guards would skip it for ever. It is
    // also the only thing that asserts the property the fork is written for —
    // that the pid comes back on stdout.
    let pid = match said.parse::<i32>() {
        Ok(pid) if pid > 1 => pid,
        Ok(pid) => {
            return Err(watchdog_failure(
                &pid_file,
                format!(
                    "the watchdog reported pid {pid}, which is not a process group this may \
                         signal"
                ),
            ))
        }
        Err(_) => {
            return Err(watchdog_failure(
                &pid_file,
                format!("the watchdog was forked but named no pid on stdout; it said {said:?}"),
            ))
        }
    };
    // The pid is tracked now, so the file has done its whole job and a run that
    // arms one watchdog per scenario should not leave a trail of them in /tmp.
    let _ = std::fs::remove_file(&pid_file);
    Ok(pid)
}

/// The paired connection and the scratch session a terminal scenario needs, or
/// the outcome to report in place of running it.
async fn stage(target: &Target, tag: &str) -> std::result::Result<(Phone, Scratch), Outcome> {
    if protocol::tmux::tmux_bin().is_none() {
        return Err(Outcome::skipped("no tmux on this machine"));
    }
    let phone = match target.terminal_phone().await {
        Ok(phone) => phone,
        Err(err) => {
            return Err(Outcome::failed(format!(
                "no connection carrying terminal authority: {err:#}"
            )))
        }
    };
    match Scratch::spawn(tag) {
        Ok(scratch) => Ok((phone, scratch)),
        Err(err) => Err(Outcome::failed(format!("{err:#}"))),
    }
}

fn attachment_id(tag: &str) -> String {
    format!("soak-{tag}-{}", protocol::time::now_unix_ms())
}

/// A marker the pane's echo of the command cannot satisfy: the shell strips the
/// empty quotes, so what is typed and what is printed are different strings and
/// only the genuine round trip matches.
fn marker(tag: &str) -> (String, Vec<u8>) {
    let stamp = protocol::time::now_unix_ms();
    (
        format!("echo SOAK''-{tag}-{stamp}\n"),
        format!("SOAK-{tag}-{stamp}").into_bytes(),
    )
}

/// A live terminal, or the sentence explaining why there is not one.
///
/// One attach, no retry. There used to be a loop here waiting out an
/// `attachment_limit` for up to twenty seconds, on the reasoning that a
/// session's lease frees only once the previous disposable client is dead and
/// the daemon was telling the truth in the meantime. It was telling the truth,
/// and it was also the harness quietly covering for a lockout no phone could
/// cover for: the daemon now supersedes whatever holds the session's terminal
/// and waits on the lease itself, so a client that needs a retry loop is a
/// client meeting a defect.
async fn open_terminal(
    phone: Phone,
    uid: &str,
    tag: &str,
    credit: u32,
) -> std::result::Result<Box<Terminal>, String> {
    match Terminal::attach(phone, &attachment_id(tag), uid, 80, 24, credit).await {
        Ok(Attachment::Live(terminal)) => Ok(terminal),
        Ok(Attachment::Refused { code, reason, .. }) => Err(refusal(&code, &reason, uid)),
        Err(err) => Err(format!("attaching: {err:#}")),
    }
}

/// What a refused attach means — and, for the one refusal that can be saying
/// something other than what it says, the evidence against it.
///
/// `session_not_hosted` for a uid tmux is hosting *right now* is not a fact
/// about the session: it is the daemon reading the shared server differently
/// from this process, and the report has to say so or the next reader spends an
/// hour on the session instead of on the daemon. Measured once, for real: under
/// launchd ccd has no UTF-8 locale, tmux then rendered the unit separator in
/// `list-sessions -F` output as `_`, every epoch line failed to parse, and every
/// session in the fleet looked absent to the terminal carrier.
///
/// That separator is gone — the epoch and reverify formats are delimited with a
/// space, which tmux passes through whatever the client's locale — so this
/// diagnostic should no longer have anything to say. It stays because it is the
/// only instrument that can tell "the session really is not there" apart from
/// "the daemon cannot read the server it is looking at", and that distinction
/// outlives the one defect that made it necessary.
fn refusal(code: &str, reason: &str, uid: &str) -> String {
    let complaint = format!("the attach was refused as {code} ({reason})");
    if code != terminal_close::SESSION_NOT_HOSTED {
        return complaint;
    }
    // `#{s/…//:VAR}`, never `#{E:VAR}`.
    //
    // `E:` makes tmux **secondarily expand** the session-environment value, and
    // that value is attacker-controlled text. Measured on tmux 3.7b: a session
    // stamped `CODECONNECT_SESSION_UID=x\n<uid>` comes back from `list-sessions
    // -F '#{E:…}'` as *two* lines, the second of which is exactly the uid — so
    // one crafted session forges the evidence and makes this diagnostic accuse
    // a daemon that is reading the server correctly. It is the same defect
    // class as the one being fixed in `protocol::tmux` right now.
    //
    // The substitution has no `E:`, so nothing is expanded a second time, and
    // the character class strips every byte a forged line would need: newline,
    // CR, TAB, space, `#`, `{`. It is global by default and must **not** carry a
    // `g` flag — tmux silently accepts an unknown flag rather than refusing it,
    // so a `g` here would be a modifier nobody could tell was doing nothing.
    // The class keeps both cases because `uid::is_well_formed` accepts both; on
    // the same tmux a clean uid and a lowercased one both pass through
    // unchanged, and the forged stamp above becomes the single line `x<uid>`,
    // which matches nothing.
    let stamp = format!("#{{s/[^0-9A-Za-z]//:{}}}", protocol::ENV_SESSION_UID);
    let answer = env::tmux_answer(&["list-sessions", "-F", &stamp]);
    match &answer {
        env::TmuxAnswer::Ok(listed) if listed.lines().any(|line| line.trim() == uid) => format!(
            "{complaint} — but tmux lists a session carrying {uid} on the {} server as this is \
             read, so what is wrong is how the daemon reads that server, not the session",
            protocol::TMUX_SOCKET_NAME
        ),
        env::TmuxAnswer::Ok(_) => complaint,
        // The diagnostic itself could not be run. Said out loud rather than
        // dropped: silently returning the bare complaint reads as "the second
        // opinion agreed", which is the one thing this instrument exists not to
        // let happen.
        _ => format!(
            "{complaint} — and the second opinion could not be taken either: {}",
            answer.complaint("listing the shared server's sessions")
        ),
    }
}

/// What a caller demands of the painted snapshot.
///
/// Opt-in because one scenario genuinely cannot make the stronger claim, and
/// the honest way to say so is a name at the call site rather than a weaker
/// assertion for everybody.
#[derive(Clone, Copy)]
enum SnapshotProof<'a> {
    /// The repaint prefix, and nothing beyond it.
    ///
    /// For the starvation scenario alone, whose whole instrument is a 512-byte
    /// window it never replenishes: the daemon splits a chunk by the credit
    /// available, so the first frame there is a *truncated* paint, and
    /// accumulating the rest would mean returning the credit the scenario
    /// exists to withhold. It proves the paint began. It does not prove the
    /// paint carried a screen.
    PrefixOnly,
    /// The repaint prefix, and this marker inside the first frame.
    ///
    /// The prefix alone is satisfied by a daemon that emits eleven bytes and no
    /// capture at all — a blank screen, which is precisely the failure the
    /// attach paint exists to prevent, since control mode streams only what a
    /// pane prints *next*. The marker is printed into the pane and confirmed on
    /// the pane's own screen before the attach, so a frame carrying it can only
    /// have come from a real capture of that screen.
    Contains(&'a [u8]),
}

/// Does the attach's first frame satisfy what the caller demanded of it?
///
/// Free of the socket on purpose. The whole of the judgement is here and none
/// of it needs a daemon, so it is exercised under `cargo test`; as an inline
/// block inside [`read_snapshot`] it could only ever be checked by a live run,
/// which is to say never in CI.
fn snapshot_satisfies(bytes: &[u8], proof: SnapshotProof<'_>) -> std::result::Result<(), String> {
    if !bytes.starts_with(SNAPSHOT_PREFIX) {
        return Err(format!(
            "the first chunk was not a repaint: {:?}",
            String::from_utf8_lossy(&bytes[..bytes.len().min(48)])
        ));
    }
    if let SnapshotProof::Contains(want) = proof {
        // The first frame only. With the ordinary 64 KiB window a pane this
        // size paints whole in one carrier chunk, so accumulating frames would
        // only hide a daemon that split a paint it should not have.
        if !crate::ws::contains(bytes, want) {
            return Err(format!(
                "the repaint carried no screen: {} bytes that do not contain {:?}, which the \
                 pane was showing when the attach was sent — a cleared, homed, empty screen is \
                 exactly what an attach without a capture looks like",
                bytes.len(),
                String::from_utf8_lossy(want)
            ));
        }
    }
    Ok(())
}

/// The painted snapshot, unreplenished — the caller returns the credit, because
/// one scenario's whole instrument is not returning it.
async fn read_snapshot(
    terminal: &mut Terminal,
    proof: SnapshotProof<'_>,
) -> std::result::Result<Vec<u8>, String> {
    let start = Instant::now();
    loop {
        let left = match PAINT_DEADLINE.checked_sub(start.elapsed()) {
            Some(left) => left,
            None => return Err(format!("no snapshot within {PAINT_DEADLINE:?}")),
        };
        match terminal.next(left).await {
            Ok(TerminalEvent::Output(bytes)) => {
                snapshot_satisfies(&bytes, proof)?;
                return Ok(bytes);
            }
            Ok(TerminalEvent::Closed { code, reason, .. }) => {
                return Err(format!("closed as {code} ({reason}) before painting"))
            }
            Ok(_) => continue,
            Err(err) => return Err(format!("waiting for the snapshot: {err:#}")),
        }
    }
}

/// Attach, prove the paint, return its credit. The opening of four scenarios.
async fn attach_and_paint(
    phone: Phone,
    uid: &str,
    tag: &str,
    credit: u32,
    proof: SnapshotProof<'_>,
) -> std::result::Result<Box<Terminal>, String> {
    let mut terminal = open_terminal(phone, uid, tag, credit).await?;
    let snapshot = read_snapshot(&mut terminal, proof).await?;
    terminal
        .grant(snapshot.len() as u32)
        .await
        .map_err(|err| format!("returning the snapshot's credit: {err:#}"))?;
    Ok(terminal)
}

/// Print a marker, wait for the pane to show it, attach, and require the paint
/// to carry it. The causal opening of every terminal scenario but the starved
/// one — and re-done per attach, because the scenarios that flood a pane scroll
/// an earlier marker off the screen.
async fn paint_and_attach(
    scratch: &Scratch,
    phone: Phone,
    tag: &str,
    credit: u32,
) -> std::result::Result<Box<Terminal>, String> {
    let marker = scratch.screen_marker(tag)?;
    attach_and_paint(
        phone,
        &scratch.uid,
        tag,
        credit,
        SnapshotProof::Contains(&marker),
    )
    .await
}

/// Type a marker and read its echo back, proving the stream is live in both
/// directions. Returns how long the round trip took.
async fn round_trip(terminal: &mut Terminal, tag: &str) -> std::result::Result<Duration, String> {
    let (typed, expected) = marker(tag);
    let started = Instant::now();
    if let Err(err) = terminal.type_bytes(typed.as_bytes()).await {
        return Err(format!("typing into the pane: {err:#}"));
    }
    match terminal
        .read_until(&expected, Duration::from_secs(15))
        .await
    {
        Ok(_) => Ok(started.elapsed()),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// Did the slow-consumer close land on the documented deadline?
///
/// Both bounds, because only the pair says anything. The scenario used to
/// assert the close *code* alone, which a daemon closing at t=0.0s satisfies —
/// and a deadline armed in the wrong place is exactly what produces a close at
/// t=0. The ceiling on its own is no better: it was [`STALL_DEADLINE`], 45
/// seconds, half as long again as the deadline it was checking.
///
/// `took` is measured from the last byte the daemon sent before the close; see
/// [`terminal_starvation`] for why that instant and not the flood's.
fn stall_is_on_deadline(took: Duration) -> std::result::Result<(), String> {
    let floor = DAEMON_STALL_DEADLINE.saturating_sub(STALL_EARLY_SLACK);
    let ceiling = DAEMON_STALL_DEADLINE + STALL_LATE_SLACK;
    if took < floor {
        return Err(format!(
            "the close came {:.1}s after the last byte the daemon sent, inside the \
             {DAEMON_STALL_DEADLINE:?} its chunk's deadline is armed for: a stalled consumer is \
             carried to the deadline, not dropped as soon as credit runs out",
            took.as_secs_f64()
        ));
    }
    if took > ceiling {
        return Err(format!(
            "the close came {:.1}s after the last byte the daemon sent, past the \
             {DAEMON_STALL_DEADLINE:?} deadline and the {STALL_LATE_SLACK:?} of slack a loaded \
             machine is allowed",
            took.as_secs_f64()
        ));
    }
    Ok(())
}

/// A round count the flap scenario can prove something with.
///
/// Zero is not a smaller run. `for round in 0..0` never executes, so no socket
/// is ever dropped, no client is ever counted, and every number the verdict
/// reads sits at its passing value — the scenario printed "0 drops mid-stream"
/// and reported PASS beside it. `--rounds 0` reached this from the command
/// line.
fn flap_rounds(rounds: u32) -> std::result::Result<u32, String> {
    if rounds == 0 {
        return Err(
            "a flap of zero rounds drops no socket and measures nothing; `--rounds` must be at \
             least 1"
                .to_string(),
        );
    }
    Ok(rounds)
}

/// The most tmux clients seen on the scratch session across a flap's rounds —
/// and, separately, whether any round's count could not be taken at all.
///
/// Separate because they fail the verdict for different reasons and must read
/// differently in the report. A poll that answered nothing is not "0 clients",
/// and folding it into the maximum let a run whose every poll timed out report
/// "at most 0 tmux clients on the session" and pass.
#[derive(Debug, Default)]
struct PeakClients {
    seen: Option<usize>,
    /// The first poll that answered nothing, kept for the complaint.
    unread: Option<String>,
}

impl PeakClients {
    fn observe(&mut self, answer: std::result::Result<usize, String>) {
        match answer {
            Ok(count) => self.seen = Some(self.seen.unwrap_or(0).max(count)),
            Err(why) => {
                self.unread.get_or_insert(why);
            }
        }
    }

    /// The number for the report, or the sentence saying there is not one.
    fn note(&self) -> String {
        match self.seen {
            Some(count) => format!("at most {count} tmux client(s) on the session at any time"),
            None => "no tmux client count was read at all".to_string(),
        }
    }

    /// `Ok` only when every round was counted and none exceeded `want`.
    fn verdict(&self, want: usize) -> std::result::Result<(), String> {
        if let Some(why) = &self.unread {
            return Err(format!(
                "a round's tmux client count could not be read ({why}), so whether the carriers \
                 accumulate was never measured"
            ));
        }
        match self.seen {
            None => Err("no round's tmux client count was read at all".to_string()),
            Some(count) if count > want => Err(format!(
                "{count} concurrent tmux clients on one session: the carriers are accumulating"
            )),
            Some(_) => Ok(()),
        }
    }
}

/// Is the run this gauntlet was pointed at still there, undisturbed?
fn target_still_listed(target: &Target) -> std::result::Result<(), String> {
    match env::sessions() {
        Ok(sessions) => {
            if sessions
                .iter()
                .any(|s| s.session_uid == target.session.session_uid)
            {
                Ok(())
            } else {
                Err(format!(
                    "the daemon no longer lists {}: the terminal disturbed the run it was \
                     supposed to leave alone",
                    target.session.session_uid
                ))
            }
        }
        Err(err) => Err(format!("the daemon stopped listing sessions: {err:#}")),
    }
}

// ------------------------------------------------------- (j) terminal round trip

/// Attach, watch the screen arrive, type, resize, detach.
///
/// The whole contract in one pass: a snapshot within a deadline (control mode
/// streams only what a pane prints *next*, so without the paint an attach is a
/// blank screen), keystrokes that reach the pane, credit that keeps flowing,
/// a resize that is applied to the daemon's own client and nobody else's, and a
/// detach that is acknowledged. And afterwards the operator's run is still
/// listed, because a viewer must cost the session nothing.
pub async fn terminal_roundtrip(target: &Target) -> Outcome {
    let (phone, scratch) = match stage(target, "roundtrip").await {
        Ok(stage) => stage,
        Err(outcome) => return outcome,
    };
    let mut notes = Vec::new();

    // Printed and confirmed on the pane's own screen *before* the attach, so
    // the paint below can be required to carry it. Without that the only claim
    // available is the eleven-byte repaint prefix, which a daemon that cleared
    // the screen and captured nothing satisfies exactly.
    let marker = match scratch.screen_marker("roundtrip") {
        Ok(marker) => marker,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(complaint);
        }
    };
    let mut terminal = match open_terminal(
        phone,
        &scratch.uid,
        "roundtrip",
        protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
    )
    .await
    {
        Ok(terminal) => terminal,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(complaint);
        }
    };

    let painted = Instant::now();
    let snapshot = match read_snapshot(&mut terminal, SnapshotProof::Contains(&marker)).await {
        Ok(snapshot) => snapshot,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(complaint).with_notes(notes);
        }
    };
    notes.push(format!(
        "snapshot {} bytes in {:.2}s, clearing and re-homing the screen and carrying {}",
        snapshot.len(),
        painted.elapsed().as_secs_f64(),
        String::from_utf8_lossy(&marker)
    ));
    if let Err(err) = terminal.grant(snapshot.len() as u32).await {
        scratch.close();
        return Outcome::failed(format!("returning the snapshot's credit: {err:#}"))
            .with_notes(notes);
    }

    match round_trip(&mut terminal, "M").await {
        Ok(took) => notes.push(format!(
            "a typed marker echoed back in {}ms",
            took.as_millis()
        )),
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(complaint).with_notes(notes);
        }
    }

    // A resize applies to the daemon's disposable client — and it has to be
    // *observed*, not inferred.
    //
    // `terminal_resize` is fire-and-forget: the frame is written and there is no
    // acknowledgement anywhere in the protocol (`terminal_attached` carries an
    // attachment id and an input credit, and no `ServerMessage` reports
    // geometry at all). So "a later marker still round-trips" passes
    // identically against a daemon that dropped the frame on the floor, and the
    // note claiming a geometry nobody had looked at was simply untrue.
    //
    // Read out of band from tmux instead, from the client and not the window —
    // see `wait_for_client_width` for why the window is the wrong instrument
    // here: the daemon's `-f ignore-size` client governs the window only while
    // it is the sole client on the whole server, and this gauntlet's own target
    // run keeps a client on it, so the window deliberately does not follow. The
    // client's declared width does follow the daemon's `refresh-client -C`, so
    // it is the fact that separates a forwarded resize from a dropped one.
    //
    // The baseline is genuine and read, not assumed — the pane is created 80x24
    // and the attach asks for 80x24, so the client starts at 80 and 100 is a
    // real change (inside the protocol's 2..=512 by 2..=256 bounds). Reading it
    // through the same instrument means a client that already declared 100 would
    // fail the baseline here rather than pass the resize vacuously below.
    if let Err(complaint) = scratch.wait_for_client_width(80, Duration::from_secs(10)) {
        scratch.close();
        return Outcome::failed(format!(
            "the daemon's client is not at the 80 columns it attached with, so a resize to \
             100 would prove nothing: {complaint}"
        ))
        .with_notes(notes);
    }
    if let Err(err) = terminal.resize(100, 30).await {
        scratch.close();
        return Outcome::failed(format!("resizing: {err:#}")).with_notes(notes);
    }
    if let Err(complaint) = scratch.wait_for_client_width(100, Duration::from_secs(10)) {
        scratch.close();
        return Outcome::failed(format!(
            "the resize never reached the daemon's client: {complaint} — the daemon issues \
             `refresh-client -C` on the resize, which moves its client's declared width even \
             when `ignore-size` keeps the window from following"
        ))
        .with_notes(notes);
    }
    notes.push(
        "the daemon's client widened to 100 columns after the resize (the window itself follows \
         only while that client is sole on the server, so a phone never resizes a human's view)"
            .into(),
    );
    // And the stream is neither closed nor wedged by it, which the absence of a
    // complaint would not show.
    match round_trip(&mut terminal, "R").await {
        Ok(took) => notes.push(format!(
            "still streaming after the resize ({}ms)",
            took.as_millis()
        )),
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(format!("after the resize: {complaint}")).with_notes(notes);
        }
    }

    let received = terminal.received;
    let chunks = terminal.chunks;
    let closed = match terminal.detach().await {
        Ok((_, code)) => code,
        Err(err) => {
            scratch.close();
            return Outcome::failed(format!("detaching: {err:#}")).with_notes(notes);
        }
    };
    notes.push(format!(
        "{received} bytes in {chunks} chunks · detach acknowledged as {closed:?}"
    ));
    if closed != terminal_close::DETACHED {
        scratch.close();
        return Outcome::failed(format!(
            "a detach must close as {:?}, not {closed:?}",
            terminal_close::DETACHED
        ))
        .with_notes(notes);
    }

    // The disposable client is disposable: nothing may be left attached. A
    // count that could not be read fails here rather than reading as zero,
    // which is the passing value.
    match scratch.wait_for_clients(0, Duration::from_secs(10)) {
        Ok(0) => {}
        Ok(left) => {
            scratch.close();
            return Outcome::failed(format!(
                "{left} tmux client(s) still attached after the detach"
            ))
            .with_notes(notes);
        }
        Err(why) => {
            scratch.close();
            return Outcome::failed(format!(
                "whether the disposable client went could not be established: {why}"
            ))
            .with_notes(notes);
        }
    }
    let listed = target_still_listed(target);
    scratch.close();
    match listed {
        Ok(()) => Outcome::passed("painted, typed, resized and detached cleanly").with_notes(notes),
        Err(complaint) => Outcome::failed(complaint).with_notes(notes),
    }
}

// -------------------------------------------------------- (k) terminal starvation

/// Attach with a small window, never replenish, and flood the pane.
///
/// A phone that stops rendering must lose its terminal rather than pin one open
/// for as long as it stays silent: while the carrier's reader is blocked on
/// credit it cannot see the session's own notifications, and tmux holds the
/// stalled output in the server. So the daemon closes the attachment as a slow
/// consumer past its deadline. What must survive is everything around it — the
/// connection, and the right to attach again.
pub async fn terminal_starvation(target: &Target) -> Outcome {
    let (phone, scratch) = match stage(target, "starve").await {
        Ok(stage) => stage,
        Err(outcome) => return outcome,
    };
    let mut notes = Vec::new();

    let mut terminal = match open_terminal(phone, &scratch.uid, "starve", STARVED_CREDIT).await {
        Ok(terminal) => terminal,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(complaint);
        }
    };
    // Read the paint and deliberately do not return its credit: from here the
    // window only shrinks.
    //
    // Prefix-only, and this is the one scenario entitled to it. The daemon
    // splits a chunk by the credit available, so with a 512-byte window the
    // paint arrives truncated; requiring a marker inside the first frame would
    // mean accumulating frames, which means returning credit, which is the one
    // thing this scenario must never do. What that costs is stated in the note
    // below rather than quietly absorbed.
    let snapshot = match read_snapshot(&mut terminal, SnapshotProof::PrefixOnly).await {
        Ok(snapshot) => snapshot,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(complaint);
        }
    };
    notes.push(format!(
        "granted {STARVED_CREDIT} bytes of output credit, spent {} on the snapshot, renewed none",
        snapshot.len()
    ));
    notes.push(
        "the paint was checked for its repaint prefix only, not for the screen's contents: a \
         512-byte window truncates it and accumulating the rest would return the credit this \
         scenario withholds"
            .into(),
    );
    if !scratch.flood() {
        scratch.close();
        return Outcome::failed("could not make the pane print").with_notes(notes);
    }

    let flooded = Instant::now();
    let mut stray = 0usize;
    // When the daemon last managed to push bytes. This, not the flood, is what
    // the close is measured against — see the note where `took` is taken.
    let mut last_output: Option<Instant> = None;
    let starved = terminal.id().to_string();
    let (closed_id, code, reason) = loop {
        let left = match STALL_DEADLINE.checked_sub(flooded.elapsed()) {
            Some(left) => left,
            None => {
                scratch.close();
                return Outcome::failed(format!(
                    "the terminal was still open {STALL_DEADLINE:?} after the flood; a stalled \
                     consumer must be closed at the {DAEMON_STALL_DEADLINE:?} deadline, not \
                     carried"
                ))
                .with_notes(notes);
            }
        };
        match terminal.next(left).await {
            Ok(TerminalEvent::Output(bytes)) => {
                // Whatever the unspent window still covered. Counted, never
                // replenished.
                stray += bytes.len();
                last_output = Some(Instant::now());
            }
            Ok(TerminalEvent::Closed {
                attachment_id,
                code,
                reason,
            }) => break (attachment_id, code, reason),
            Ok(_) => continue,
            Err(err) => {
                scratch.close();
                return Outcome::failed(format!(
                    "waiting for the starved terminal to be closed ended the read instead: \
                     {err:#}"
                ))
                .with_notes(notes);
            }
        }
    };
    // The close has to be *this* attachment's, or the timing and the code below
    // are being read off somebody else's stream. The ledger deliberately does
    // not police a close's id — `terminal_duplicate` needs a foreign one to
    // reach it as evidence — so every consumer that asserts on a close owns
    // this check.
    if closed_id != starved {
        scratch.close();
        return Outcome::failed(format!(
            "the close named attachment {closed_id:?} as {code} ({reason}), not the starved \
             {starved:?}"
        ))
        .with_notes(notes);
    }
    // Measured from the last byte the daemon sent, not from the flood.
    //
    // The daemon's deadline is absolute for the whole chunk and armed once,
    // when its reader begins forwarding a chunk it cannot fully deliver — so
    // the clock starts inside the daemon, and not when `send-keys` returned
    // here. The closest thing this side can observe to that instant is the
    // partial chunk the daemon pushes microseconds after arming, with whatever
    // credit the paint left over. When the window was already empty no chunk
    // arrives at all and the flood instant is the only lower bound there is; it
    // is *earlier* than the arming, so falling back to it can only make `took`
    // read longer, never shorter, and the late slack is what absorbs that.
    let measured_from_output = last_output.is_some();
    let took = last_output.unwrap_or(flooded).elapsed();
    notes.push(format!(
        "closed as {code:?} ({reason}) {:.1}s after {} · {stray} further bytes arrived inside \
         the unspent window",
        took.as_secs_f64(),
        if measured_from_output {
            "the last byte the daemon sent"
        } else {
            "the flood began, no byte having been sent (the window was already spent)"
        }
    ));
    if code != terminal_close::SLOW_CONSUMER {
        scratch.close();
        return Outcome::failed(format!(
            "a starved terminal must close as {:?}, not {code:?}",
            terminal_close::SLOW_CONSUMER
        ))
        .with_notes(notes);
    }
    // The code alone is satisfied by a close at t=0.0s, and the only ceiling
    // was the 45s the loop above waits. Both bounds, against the documented
    // deadline.
    if let Err(complaint) = stall_is_on_deadline(took) {
        scratch.close();
        return Outcome::failed(complaint).with_notes(notes);
    }

    // The connection is not the attachment. It has to still answer.
    let mut phone = terminal.into_phone();
    match phone.sessions().await {
        Ok(sessions) => notes.push(format!(
            "the connection survived the close and still listed {} session(s)",
            sessions.len()
        )),
        Err(err) => {
            scratch.close();
            return Outcome::failed(format!(
                "the connection did not survive its terminal being closed: {err:#}"
            ))
            .with_notes(notes);
        }
    }

    // And a fresh terminal on that same connection must paint again — with the
    // full window this time, so this one can be held to the whole screen. A
    // fresh marker, because the flood has scrolled the pane.
    let again = match paint_and_attach(
        &scratch,
        phone,
        "starve2",
        protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
    )
    .await
    {
        Ok(terminal) => terminal,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(format!("re-attaching after the close: {complaint}"))
                .with_notes(notes);
        }
    };
    notes.push("a fresh attach on the same connection painted again".into());
    let closed = again.detach().await;
    scratch.close();
    match closed {
        Ok(_) => Outcome::passed("a stalled consumer lost its terminal and nothing else")
            .with_notes(notes),
        Err(err) => {
            Outcome::failed(format!("detaching the second terminal: {err:#}")).with_notes(notes)
        }
    }
}

// -------------------------------------------------------------- (l) terminal flap

/// The window the flap opens once the attach's own window is exhausted.
///
/// Small on purpose, and tiny against what is still queued. `forward` sends
/// what the grant covers and waits for more only when none is left, so a window
/// the daemon can spend *entirely* against a chunk it cannot finish leaves it
/// suspended at `credit.acquire()` with `sent < bytes.len()` — which is the
/// state the socket has to disappear underneath.
const MID_FORWARD_GRANT: u32 = 1024;

/// How long the exhausted stream is watched before it is called quiet.
const MID_FORWARD_QUIET: Duration = Duration::from_millis(750);

/// How long the attach's opening window has to arrive under the flood.
const MID_FORWARD_DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// How long [`MID_FORWARD_GRANT`] has to come back once it is granted.
///
/// The three budgets above sum to less than [`DAEMON_STALL_DEADLINE`], and that
/// is the whole constraint on them. The daemon's stall deadline is absolute and
/// taken once per chunk, so it is already running by the time the window
/// empties: a manoeuvre that took longer than it would have the carrier close
/// as a slow consumer, and the round would fail on a measurement it was not
/// making.
const MID_FORWARD_GRANT_DEADLINE: Duration = Duration::from_secs(5);

/// What the flap observed while manoeuvring the carrier into a forward it
/// cannot finish, and whether that is enough to drop the socket on.
///
/// Free of the socket, because the whole of the judgement is arithmetic and
/// none of it needs a daemon. As an inline condition inside the round it could
/// only ever be exercised by a live run against a live Mac, which is to say
/// never under `cargo test` — and the rule it replaces is one that looked
/// right: the round read until one `terminal_output` arrived and dropped on it.
/// One chunk arriving proves a forward **completed**, not that one is in
/// flight, and the round had just handed that chunk's credit back and awaited
/// its own send before dropping, so the daemon was very likely parked idle by
/// then. A carrier that leaks only while suspended *inside* `forward` passed
/// that scenario every time.
#[derive(Debug, Default, Clone, Copy)]
struct MidForward {
    /// The window the attach opened with, which is exactly what a drain that
    /// reached silence must have carried off.
    window: u32,
    /// Bytes read out of that window without a byte of credit going back.
    drained: u64,
    /// Whether the stream then went quiet, which is the daemon out of credit
    /// rather than out of data.
    quiet: bool,
    /// The window opened afterwards.
    granted: u32,
    /// Bytes that arrived against it.
    arrived: u64,
}

impl MidForward {
    /// May the socket be dropped on this, or what is missing from the proof?
    ///
    /// Every clause is a way the drop lands on an idle daemon and measures
    /// nothing while reporting a pass.
    fn permits_drop(&self) -> std::result::Result<(), String> {
        if self.window == 0 {
            // Otherwise every clause below is satisfied by nothing having
            // happened: a window of zero is drained the instant it is opened.
            return Err(
                "the attach opened with no output window, so there was never one to drain and \
                 none of what follows is a measurement"
                    .to_string(),
            );
        }
        if self.drained != u64::from(self.window) {
            return Err(format!(
                "{} of the {} bytes the attach's window covers arrived before the stream went \
                 quiet, so the daemon still held credit and nothing here says it is suspended \
                 inside a forward",
                self.drained, self.window
            ));
        }
        if !self.quiet {
            return Err(format!(
                "the drained stream was never watched to silence, so the {} bytes read may be \
                 what the pane had rather than what the window covered, and a daemon with data \
                 left is not a daemon waiting on credit",
                self.drained
            ));
        }
        if self.granted == 0 {
            return Err(
                "no window was opened after the drain, so nothing was ever asked of the carrier \
                 and whether it is suspended inside a forward or parked between chunks was not \
                 measured"
                    .to_string(),
            );
        }
        if self.arrived > u64::from(self.granted) {
            return Err(format!(
                "{} bytes arrived against a {} byte window, which is the over-send the ledger \
                 refuses; nothing measured either side of it can be trusted",
                self.arrived, self.granted
            ));
        }
        if self.arrived != u64::from(self.granted) {
            return Err(format!(
                "{} of the {} bytes granted after the drain came back: a window the daemon did \
                 not spend in full is a daemon that ran out of data, which leaves it idle rather \
                 than suspended mid-forward and the drop measuring nothing",
                self.arrived, self.granted
            ));
        }
        Ok(())
    }
}

/// Manoeuvre the carrier into a forward it cannot finish, and report what was
/// seen doing it.
///
/// Three steps, each of which is one of [`MidForward`]'s fields:
///
///   1. **Drain** the outstanding window without granting a byte back. The
///      flood is `seq 1 20000` — about 108 KB against a 64 KB window — so the
///      daemon runs out of *credit* long before it runs out of *data*, and a
///      window read down to zero is a task parked at `credit.acquire()`.
///   2. **Watch it go quiet**, which is what separates "the window is spent"
///      from "the pane had nothing more to say".
///   3. **Grant a window far smaller than what is still queued** and read
///      exactly that many bytes back. `forward` sends what the grant covers and
///      only then waits, so exactly [`MID_FORWARD_GRANT`] arriving means the
///      daemon had more than that queued and is suspended again mid-chunk.
///
/// Nothing is granted afterwards. The caller drops the socket immediately.
async fn park_mid_forward(
    terminal: &mut Terminal,
    window: u32,
) -> std::result::Result<MidForward, String> {
    let mut observed = MidForward {
        window,
        ..MidForward::default()
    };

    let drain_started = Instant::now();
    while observed.drained < u64::from(window) {
        let left = match MID_FORWARD_DRAIN_DEADLINE.checked_sub(drain_started.elapsed()) {
            Some(left) => left,
            // Not "the drain timed out, carry on": the precondition was not
            // established, and dropping anyway would be the round claiming a
            // measurement it did not take.
            None => break,
        };
        match terminal.next(left).await {
            Ok(TerminalEvent::Output(bytes)) => observed.drained += bytes.len() as u64,
            Ok(TerminalEvent::Closed { code, reason, .. }) => {
                return Err(format!(
                    "the terminal closed as {code} ({reason}) while its window was being drained"
                ))
            }
            // Credit for what the round trip typed, a pong, an event on the
            // shared connection. None of them is pane output.
            Ok(_) => continue,
            Err(err) => {
                if drain_started.elapsed() >= MID_FORWARD_DRAIN_DEADLINE {
                    break;
                }
                return Err(format!("draining the attach's window: {err:#}"));
            }
        }
    }
    if observed.drained != u64::from(window) {
        // Reported through `permits_drop` rather than here, so the sentence an
        // operator reads is the same one the test pins.
        return Ok(observed);
    }

    let quiet_started = Instant::now();
    while let Some(left) = MID_FORWARD_QUIET.checked_sub(quiet_started.elapsed()) {
        match terminal.next(left).await {
            // Unreachable through the ledger, which refuses an over-send before
            // it reaches here; kept because "the daemon streamed past a window
            // it was never given" is the one violation this harness exists for
            // and it must never be read as silence.
            Ok(TerminalEvent::Output(bytes)) => {
                return Err(format!(
                    "the daemon sent {} more bytes of output with its window empty",
                    bytes.len()
                ))
            }
            Ok(TerminalEvent::Closed { code, reason, .. }) => {
                return Err(format!(
                    "the terminal closed as {code} ({reason}) while it was out of credit, before \
                     the socket was dropped"
                ))
            }
            Ok(_) => continue,
            // A read that spends its whole budget is the silence being waited
            // for. One that ends early ended for another reason, and a broken
            // socket is not a parked daemon.
            Err(err) => {
                if quiet_started.elapsed() >= MID_FORWARD_QUIET {
                    break;
                }
                return Err(format!(
                    "watching the exhausted stream for silence: {err:#}"
                ));
            }
        }
    }
    observed.quiet = true;

    terminal
        .grant(MID_FORWARD_GRANT)
        .await
        .map_err(|err| format!("granting the window the carrier is measured inside: {err:#}"))?;
    observed.granted = MID_FORWARD_GRANT;

    let grant_started = Instant::now();
    while observed.arrived < u64::from(MID_FORWARD_GRANT) {
        let left = match MID_FORWARD_GRANT_DEADLINE.checked_sub(grant_started.elapsed()) {
            Some(left) => left,
            None => break,
        };
        match terminal.next(left).await {
            Ok(TerminalEvent::Output(bytes)) => observed.arrived += bytes.len() as u64,
            Ok(TerminalEvent::Closed { code, reason, .. }) => {
                return Err(format!(
                    "the terminal closed as {code} ({reason}) against the window it was just \
                     granted"
                ))
            }
            Ok(_) => continue,
            Err(err) => {
                if grant_started.elapsed() >= MID_FORWARD_GRANT_DEADLINE {
                    break;
                }
                return Err(format!("reading the granted window back: {err:#}"));
            }
        }
    }
    Ok(observed)
}

/// Drop the whole socket mid-stream, reconnect, reattach. A few times.
///
/// A phone in a lift does not detach, it disappears — and the carrier it leaves
/// behind is a tmux client, a child process and two pipes. The invariant is
/// that every round costs exactly one of each, transiently: the screen repaints,
/// the stream resumes, and tmux never accumulates the daemon's disposable
/// clients.
///
/// The socket goes while the daemon is *suspended inside* `forward`, not merely
/// after one has finished — see [`park_mid_forward`] for how that state is
/// reached and [`MidForward`] for why the difference decides what this scenario
/// measures.
pub async fn terminal_flap(target: &Target, rounds: u32) -> Outcome {
    // Before anything is staged: a zero-round flap satisfies every number this
    // scenario's verdict reads, so it must not be allowed to reach them.
    let rounds = match flap_rounds(rounds) {
        Ok(rounds) => rounds,
        Err(complaint) => return Outcome::failed(complaint),
    };
    let (mut phone, scratch) = match stage(target, "flap").await {
        Ok(stage) => stage,
        Err(outcome) => return outcome,
    };
    let mut notes = Vec::new();
    let mut peak = PeakClients::default();
    let started = Instant::now();
    // Rounds whose drop landed on a carrier that was provably suspended
    // mid-forward. Every round establishes that or fails, so this can only
    // reach the report equal to `rounds`; it is printed because a number is
    // what the next run gets compared against.
    let mut parked = 0u32;

    for round in 0..rounds {
        let mut terminal = match paint_and_attach(
            &scratch,
            phone,
            "flap",
            protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
        )
        .await
        {
            Ok(terminal) => terminal,
            Err(complaint) => {
                scratch.close();
                return Outcome::failed(format!("round {round}: {complaint}")).with_notes(notes);
            }
        };
        if let Err(complaint) = round_trip(&mut terminal, "F").await {
            scratch.close();
            return Outcome::failed(format!("round {round}: {complaint}")).with_notes(notes);
        }
        peak.observe(scratch.clients());

        // More output than any window covers, which is what makes the daemon
        // run out of credit rather than out of data — the whole manoeuvre below
        // rests on there always being more queued than it has been granted.
        if !scratch.flood() {
            scratch.close();
            return Outcome::failed(format!("round {round}: could not make the pane print"))
                .with_notes(notes);
        }
        // Park the carrier inside a forward it cannot finish. Whether it is
        // there is decided by `permits_drop`, and a round that could not
        // establish it fails saying so rather than dropping anyway and
        // reporting the measurement it did not take.
        let observed =
            match park_mid_forward(&mut terminal, protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT)
                .await
            {
                Ok(observed) => observed,
                Err(complaint) => {
                    scratch.close();
                    return Outcome::failed(format!("round {round}: {complaint}"))
                        .with_notes(notes);
                }
            };
        if let Err(complaint) = observed.permits_drop() {
            scratch.close();
            return Outcome::failed(format!(
                "round {round}: the socket was not dropped, because {complaint}"
            ))
            .with_notes(notes);
        }
        parked += 1;
        // No detach, no close frame: the socket simply ceases to exist, and it
        // does so with the daemon suspended at `credit.acquire()` part way
        // through a chunk. Nothing further is granted between here and the drop.
        drop(terminal);

        match scratch.wait_for_clients(0, Duration::from_secs(15)) {
            Ok(0) => {}
            Ok(left) => {
                scratch.close();
                return Outcome::failed(format!(
                    "round {round}: {left} disposable tmux client(s) survived the dropped socket \
                     — they accumulate, one per flap"
                ))
                .with_notes(notes);
            }
            Err(why) => {
                scratch.close();
                return Outcome::failed(format!(
                    "round {round}: whether the disposable client survived the dropped socket \
                     could not be established: {why}"
                ))
                .with_notes(notes);
            }
        }

        phone = match target.terminal_phone().await {
            Ok(phone) => phone,
            Err(err) => {
                scratch.close();
                return Outcome::failed(format!("round {round}: reconnecting: {err:#}"))
                    .with_notes(notes);
            }
        };
    }

    // One last attach on the reconnected socket, taken all the way to a clean
    // detach: the rounds above prove nothing wedged, this proves the ordinary
    // path still works afterwards.
    let terminal = match paint_and_attach(
        &scratch,
        phone,
        "flap-last",
        protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
    )
    .await
    {
        Ok(terminal) => terminal,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(format!("after {rounds} flaps: {complaint}")).with_notes(notes);
        }
    };
    let detached = terminal.detach().await;
    let residue = scratch.wait_for_clients(0, Duration::from_secs(10));
    notes.push(format!(
        "{rounds} drops in {:.1}s, {parked} of them on a carrier suspended mid-forward · {} · {}",
        started.elapsed().as_secs_f64(),
        peak.note(),
        match &residue {
            Ok(left) => format!("{left} left at the end"),
            Err(why) => format!("the count at the end could not be read ({why})"),
        }
    ));
    scratch.close();
    if let Err(err) = detached {
        return Outcome::failed(format!("the final detach: {err:#}")).with_notes(notes);
    }
    if let Err(complaint) = peak.verdict(1) {
        return Outcome::failed(complaint).with_notes(notes);
    }
    match residue {
        Ok(0) => Outcome::passed("every reattach repainted and resumed; no client accumulated")
            .with_notes(notes),
        Ok(left) => Outcome::failed(format!(
            "{left} tmux client(s) left on the session at the end: the carriers are accumulating"
        ))
        .with_notes(notes),
        Err(why) => Outcome::failed(format!(
            "whether any tmux client was left at the end could not be established: {why}"
        ))
        .with_notes(notes),
    }
}

// --------------------------------------------------------- (m) terminal duplicate

/// Two attaches for one session, from two sides, and the takeover each one is.
///
/// A **second connection** takes the terminal over: the incumbent is closed as
/// `superseded` and the newcomer streams. That is the answer the reconnect path
/// needs and the old one — refuse the newcomer, keep the incumbent — is the
/// lockout it replaced: a phone whose socket died without a FIN leaves a lease
/// held by nothing, and its own reconnect was refused its own session's
/// terminal until TCP noticed, which is minutes.
///
/// A **second attach on the same connection** is the same rule down one socket:
/// the terminal that exists is closed as `superseded`, named by its own id, and
/// the new id is answered rather than left hanging.
pub async fn terminal_duplicate(target: &Target) -> Outcome {
    let (phone, scratch) = match stage(target, "dup").await {
        Ok(stage) => stage,
        Err(outcome) => return outcome,
    };
    let mut notes = Vec::new();

    let mut first = match paint_and_attach(
        &scratch,
        phone,
        "dup-a",
        protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
    )
    .await
    {
        Ok(terminal) => terminal,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(complaint);
        }
    };
    let incumbent = first.id().to_string();

    // The second connection, standing in for the reconnecting phone.
    let second = match target.terminal_phone().await {
        Ok(phone) => phone,
        Err(err) => {
            scratch.close();
            return Outcome::failed(format!("a second connection: {err:#}"));
        }
    };
    let contender = attachment_id("dup-b");
    let taken_over = match Terminal::attach(
        second,
        &contender,
        &scratch.uid,
        80,
        24,
        protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
    )
    .await
    {
        Ok(Attachment::Live(terminal)) => terminal,
        Ok(Attachment::Refused { code, reason, .. }) => {
            scratch.close();
            notes.push(format!(
                "the second connection was refused as {code:?} ({reason})"
            ));
            // Every refusal here is a finding, and they are different findings.
            return match code.as_str() {
                // The takeover began and the incumbent did not let go inside the
                // daemon's bound. A true statement, and not the one this
                // scenario is about — but the harness cannot tell a stuck child
                // from a broken supersede, so it says which it saw.
                terminal_close::SESSION_BUSY => Outcome::failed(
                    "the takeover timed out: the session's previous terminal did not release its \
                     lease inside the daemon's supersede bound, so the reconnecting phone was \
                     still refused its own session's terminal",
                ),
                // The Mac is full of terminals this harness did not open and may
                // not close, so the takeover was never reached. Unmeasured, not
                // skipped: this run learned nothing about the claim.
                terminal_close::ATTACHMENT_LIMIT => Outcome::unmeasured(format!(
                    "the Mac's global terminal cap answered the contender ({reason}), so the \
                     takeover was NOT measured — close the other terminals and run this again"
                )),
                _ => Outcome::failed(format!(
                    "a second connection must take the terminal over, not be refused as {code:?}"
                )),
            }
            .with_notes(notes);
        }
        Err(err) => {
            scratch.close();
            return Outcome::failed(format!("the second attach: {err:#}")).with_notes(notes);
        }
    };
    notes.push("a second connection took the session's terminal over".to_string());

    // And the incumbent is told which ending that was, on its own id. A socket
    // that merely goes quiet is not this: the phone showing that terminal has to
    // be able to say why it stopped.
    let displaced = match read_close(&mut first, Duration::from_secs(15)).await {
        Ok(closed) => closed,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(format!("the displaced terminal: {complaint}"))
                .with_notes(notes);
        }
    };
    notes.push(format!(
        "the displaced terminal was closed as {:?} ({})",
        displaced.1, displaced.2
    ));
    if displaced.0 != incumbent || displaced.1 != terminal_close::SUPERSEDED {
        scratch.close();
        return Outcome::failed(format!(
            "the displaced close must name {incumbent:?} as {:?}; it named {:?} as {:?}",
            terminal_close::SUPERSEDED,
            displaced.0,
            displaced.1
        ))
        .with_notes(notes);
    }

    // The newcomer really has a terminal: the proof is that it streams.
    let mut taken_over = taken_over;
    match round_trip(&mut taken_over, "D").await {
        Ok(took) => notes.push(format!(
            "the terminal that took over streams ({}ms)",
            took.as_millis()
        )),
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(format!("the terminal that took over: {complaint}"))
                .with_notes(notes);
        }
    }

    // Now the same rule down one socket: a second attach on the connection that
    // already holds the terminal.
    let held = taken_over.id().to_string();
    let ghost = attachment_id("dup-c");
    if let Err(err) = taken_over.attach_again(&ghost, &scratch.uid).await {
        scratch.close();
        return Outcome::failed(format!("sending a second attach: {err:#}")).with_notes(notes);
    }
    let closed = match read_close(&mut taken_over, Duration::from_secs(15)).await {
        Ok(closed) => closed,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(format!("after a second attach: {complaint}"))
                .with_notes(notes);
        }
    };
    notes.push(format!(
        "a second attach on one connection closed {} as {:?}",
        if closed.0 == held {
            "the terminal that existed"
        } else {
            "the wrong attachment"
        },
        closed.1
    ));
    if closed.0 != held || closed.1 != terminal_close::SUPERSEDED {
        scratch.close();
        return Outcome::failed(format!(
            "the close must name {held:?} as {:?}; it named {:?} as {:?}",
            terminal_close::SUPERSEDED,
            closed.0,
            closed.1
        ))
        .with_notes(notes);
    }

    // And the id that displaced it is *answered*, either way, rather than left
    // hanging — the permanent "Opening a terminal…" this rule replaced.
    let phone = taken_over.into_phone();
    let answered = match Terminal::attach(
        phone,
        &ghost,
        &scratch.uid,
        80,
        24,
        protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
    )
    .await
    {
        Ok(attachment) => attachment,
        Err(err) => {
            scratch.close();
            return Outcome::failed(format!("the id the second attach named: {err:#}"))
                .with_notes(notes);
        }
    };
    let outcome = match answered {
        Attachment::Live(terminal) => match terminal.detach().await {
            Ok(_) => Outcome::passed(
                "a second connection took the terminal over and the incumbent was told it was \
                 superseded; a second attach on one connection did the same and answered the id \
                 it named",
            )
            .with_notes(notes),
            Err(err) => Outcome::failed(format!("detaching the recovered terminal: {err:#}"))
                .with_notes(notes),
        },
        Attachment::Refused { code, reason, .. } => Outcome::failed(format!(
            "the connection could not open another terminal afterwards: {code} ({reason})"
        ))
        .with_notes(notes),
    };
    scratch.close();
    outcome
}

/// Read a terminal's stream until it closes, returning the id, code and reason.
///
/// Credit is returned for anything that streams on the way, so a close this
/// waits for is the one the scenario is about and never a stall this caused.
async fn read_close(
    terminal: &mut Terminal,
    within: Duration,
) -> std::result::Result<(String, String, String), String> {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| format!("nothing closed the terminal within {within:?}"))?;
        match terminal.next(left).await {
            Ok(TerminalEvent::Closed {
                attachment_id,
                code,
                reason,
            }) => return Ok((attachment_id, code, reason)),
            Ok(TerminalEvent::Output(bytes)) => {
                let spent = bytes.len() as u32;
                terminal
                    .grant(spent)
                    .await
                    .map_err(|err| format!("returning credit: {err:#}"))?;
            }
            Ok(_) => continue,
            Err(err) => return Err(format!("{err:#}")),
        }
    }
}

// -------------------------------------------------------------- (n) terminal exit

/// Kill the session under its viewer.
///
/// The phone is owed the truth about why its screen stopped — a session that
/// ended is not a stalled stream and not a network fault — and the daemon is
/// owed nothing at all: the carrier is a disposable client, so the thing it was
/// watching dying must cost one attachment and no more.
pub async fn terminal_exit(target: &Target) -> Outcome {
    let (phone, mut scratch) = match stage(target, "exit").await {
        Ok(stage) => stage,
        Err(outcome) => return outcome,
    };
    let mut notes = Vec::new();

    let mut terminal = match paint_and_attach(
        &scratch,
        phone,
        "exit",
        protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT,
    )
    .await
    {
        Ok(terminal) => terminal,
        Err(complaint) => {
            scratch.close();
            return Outcome::failed(complaint);
        }
    };
    // Live before it is killed, or the close proves nothing about a stream that
    // was flowing.
    if let Err(complaint) = round_trip(&mut terminal, "X").await {
        scratch.close();
        return Outcome::failed(complaint);
    }

    if !scratch.kill_pane() {
        scratch.close();
        return Outcome::failed("the scratch session named no pane to kill");
    }
    let killed = Instant::now();
    let watching = terminal.id().to_string();
    let (closed_id, code, reason) = loop {
        let left = match Duration::from_secs(20).checked_sub(killed.elapsed()) {
            Some(left) => left,
            None => {
                scratch.close();
                return Outcome::failed(
                    "the session was killed and the terminal was never closed; the phone would \
                     be watching a screen that cannot change",
                )
                .with_notes(notes);
            }
        };
        match terminal.next(left).await {
            Ok(TerminalEvent::Closed {
                attachment_id,
                code,
                reason,
            }) => break (attachment_id, code, reason),
            // The pane's dying bytes. Credit for them keeps the stream honest
            // right up to the end.
            Ok(TerminalEvent::Output(bytes)) => {
                let spent = bytes.len() as u32;
                let _ = terminal.grant(spent).await;
            }
            Ok(_) => continue,
            Err(err) => {
                scratch.close();
                return Outcome::failed(format!(
                    "waiting for the killed session to be reported ended the read instead: \
                     {err:#}"
                ))
                .with_notes(notes);
            }
        }
    };
    notes.push(format!(
        "closed as {code:?} ({reason}) {}ms after the pane was killed",
        killed.elapsed().as_millis()
    ));
    // This attachment's close, not one that happened to arrive on the shared
    // connection: the code asserted below is only about the session's end if it
    // names the terminal that was watching it.
    if closed_id != watching {
        scratch.close();
        return Outcome::failed(format!(
            "the close named attachment {closed_id:?} as {code} ({reason}), not the \
             {watching:?} that was watching the session"
        ))
        .with_notes(notes);
    }
    if code != terminal_close::SESSION_EXITED {
        scratch.close();
        return Outcome::failed(format!(
            "a session that ended under its viewer must close as {:?}, not {code:?}",
            terminal_close::SESSION_EXITED
        ))
        .with_notes(notes);
    }

    // The daemon is not the session's parent, and a terminal is not the
    // daemon's: both must be exactly as healthy as before. One attach, with no
    // waiting out the reaper: the just-closed attachment's lease is released
    // once its tmux client is genuinely dead, and the attach waits on that
    // itself, so what comes back is an answer about the *resolve* and nothing
    // else.
    let mut phone = terminal.into_phone();
    let dead_uid = scratch.uid.clone();
    match Terminal::attach(phone, &attachment_id("exit-gone"), &dead_uid, 80, 24, 4096).await {
        Ok(Attachment::Refused {
            phone: back,
            code,
            reason,
        }) => {
            notes.push(format!(
                "attaching to the dead session is refused as {code:?} ({reason})"
            ));
            if code != terminal_close::SESSION_NOT_HOSTED {
                scratch.close();
                return Outcome::failed(format!(
                    "a uid nothing hosts must be refused as {:?}, not {code:?}",
                    terminal_close::SESSION_NOT_HOSTED
                ))
                .with_notes(notes);
            }
            phone = *back;
        }
        Ok(Attachment::Live(_)) => {
            scratch.close();
            return Outcome::failed("the daemon opened a terminal on a session that is gone")
                .with_notes(notes);
        }
        Err(err) => {
            scratch.close();
            return Outcome::failed(format!("attaching to the dead session: {err:#}"))
                .with_notes(notes);
        }
    }
    let healthy = match phone.sessions().await {
        Ok(sessions) => sessions.len(),
        Err(err) => {
            scratch.close();
            return Outcome::failed(format!("the daemon stopped answering: {err:#}"))
                .with_notes(notes);
        }
    };
    notes.push(format!(
        "the daemon still answers and lists {healthy} session(s)"
    ));
    let listed = target_still_listed(target);
    scratch.close();
    match listed {
        Ok(()) => Outcome::passed("the session's end was reported, and cost nothing else")
            .with_notes(notes),
        Err(complaint) => Outcome::failed(complaint).with_notes(notes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row the daemon listed, so the handle is its id.
    fn device(id: &str, name: &str) -> MintedDevice {
        MintedDevice {
            handle: Handle::Id(id.to_string()),
            name: name.to_string(),
        }
    }

    /// A row known only by the name the ack reported, which is the fallback
    /// [`accounted`] takes when the listing could not be read.
    fn named(name: &str) -> MintedDevice {
        MintedDevice {
            handle: Handle::Name(name.to_string()),
            name: name.to_string(),
        }
    }

    /// One `codeconnect devices` row.
    fn row(id: &str, name: &str, revoked: bool) -> protocol::pairing::DeviceSummary {
        protocol::pairing::DeviceSummary {
            device_id: id.to_string(),
            name: name.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_seen_at: None,
            revoked_at: revoked.then(|| "2026-01-01T00:01:00Z".to_string()),
        }
    }

    /// This run's own device name, as `accounted` is given it live.
    const MINE: &str = "ccsoak-terminal-01JRUNAAAAAAAAAAAAAAAAAAAA";

    /// A ccsoak that is pairing at the same moment, on the same Mac. Same stem,
    /// different uid — which is the whole of what keeps the two apart.
    const THEIRS: &str = "ccsoak-terminal-01JRUNBBBBBBBBBBBBBBBBBBBB";

    #[test]
    fn a_snapshot_prefix_is_not_a_screen() {
        const MARKER: &[u8] = b"SNAP_roundtrip-7";

        // The failure the causal check exists for: eleven repaint bytes and no
        // capture at all. Blank screens satisfy the prefix, which is the whole
        // of what this used to assert.
        let blank = SNAPSHOT_PREFIX.to_vec();
        assert!(snapshot_satisfies(&blank, SnapshotProof::PrefixOnly).is_ok());
        let refused = snapshot_satisfies(&blank, SnapshotProof::Contains(MARKER))
            .expect_err("a repaint with no screen in it is not a snapshot of that screen");
        assert!(refused.contains("carried no screen"), "{refused}");

        // A real paint of a pane that was showing the marker.
        let mut painted = SNAPSHOT_PREFIX.to_vec();
        painted.extend_from_slice(b"sh-3.2$ printf 'SNAP_%s\\n' roundtrip-7\r\n");
        painted.extend_from_slice(b"SNAP_roundtrip-7\r\n");
        painted.extend_from_slice(b"\x1b[3;1H");
        assert!(snapshot_satisfies(&painted, SnapshotProof::Contains(MARKER)).is_ok());

        // The echoed command line alone does not satisfy it: the marker is
        // spelled so that only the `printf` *output* carries the literal text,
        // and a screen showing the command but not its output is a screen the
        // paint raced.
        let mut echo_only = SNAPSHOT_PREFIX.to_vec();
        echo_only.extend_from_slice(b"sh-3.2$ printf 'SNAP_%s\\n' roundtrip-7\r\n");
        assert!(snapshot_satisfies(&echo_only, SnapshotProof::Contains(MARKER)).is_err());

        // And the prefix is still required of everybody: live pane bytes that
        // never cleared the screen are not a paint under either proof.
        let unpainted = b"SNAP_roundtrip-7\r\n".to_vec();
        assert!(snapshot_satisfies(&unpainted, SnapshotProof::PrefixOnly).is_err());
        assert!(snapshot_satisfies(&unpainted, SnapshotProof::Contains(MARKER)).is_err());
    }

    #[test]
    fn a_stall_at_zero_and_a_stall_that_never_comes_both_fail() {
        // A close at t=0 is the regression a deadline armed in the wrong place
        // produces, and the code assertion alone accepts it.
        assert!(stall_is_on_deadline(Duration::from_millis(0)).is_err());
        assert!(stall_is_on_deadline(Duration::from_secs(1)).is_err());
        // On the deadline, from either side of the measurement slack.
        assert!(stall_is_on_deadline(DAEMON_STALL_DEADLINE).is_ok());
        assert!(stall_is_on_deadline(DAEMON_STALL_DEADLINE - STALL_EARLY_SLACK).is_ok());
        assert!(stall_is_on_deadline(DAEMON_STALL_DEADLINE + STALL_LATE_SLACK).is_ok());
        // And outside it on both sides.
        assert!(
            stall_is_on_deadline(DAEMON_STALL_DEADLINE - STALL_EARLY_SLACK - ONE_TICK).is_err()
        );
        assert!(stall_is_on_deadline(DAEMON_STALL_DEADLINE + STALL_LATE_SLACK + ONE_TICK).is_err());
        // The window has to fit inside the ceiling the scenario's read loop
        // waits, or "closed late" and "never closed" become the same sentence.
        assert!(DAEMON_STALL_DEADLINE + STALL_LATE_SLACK < STALL_DEADLINE);
    }

    /// Enough to be outside a bound and nothing like a real measurement.
    const ONE_TICK: Duration = Duration::from_millis(1);

    #[test]
    fn the_socket_is_dropped_only_on_a_carrier_that_is_provably_mid_forward() {
        let window = protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT;
        let full = |granted: u32, arrived: u64| MidForward {
            window,
            drained: u64::from(window),
            quiet: true,
            granted,
            arrived,
        };

        // The whole proof: the attach's window drained without a byte going
        // back, silence after it, then a small window spent to the byte. The
        // daemon had more queued than it was granted, so it is suspended at
        // `credit.acquire()` with a chunk half-delivered.
        assert!(full(MID_FORWARD_GRANT, u64::from(MID_FORWARD_GRANT))
            .permits_drop()
            .is_ok());

        // The rule this replaces, and the reason the scenario measured nothing:
        // one chunk arrived, so the round called it a forward in flight. A
        // chunk arriving is a forward that *finished*, and the round had handed
        // its credit back and awaited its own send before dropping.
        let old_rule = MidForward {
            window,
            drained: 0,
            quiet: false,
            granted: 0,
            arrived: 4096,
        };
        let refused = old_rule
            .permits_drop()
            .expect_err("one chunk that arrived is a forward that completed");
        assert!(refused.contains("suspended inside a forward"), "{refused}");

        // A window the daemon did not spend in full: it ran out of data, so it
        // is idle rather than mid-chunk and the drop would land on nothing.
        let short = full(MID_FORWARD_GRANT, 300)
            .permits_drop()
            .expect_err("a partly spent window is a daemon with nothing left to send");
        assert!(short.contains("did not spend in full"), "{short}");

        // A window spent in full proves nothing on its own: without the drain
        // the daemon may simply have had that much credit all along.
        let ungrounded = MidForward {
            window,
            drained: 0,
            quiet: false,
            granted: MID_FORWARD_GRANT,
            arrived: u64::from(MID_FORWARD_GRANT),
        }
        .permits_drop()
        .expect_err("a spent window with no drain behind it is not a parked carrier");
        assert!(ungrounded.contains("still held credit"), "{ungrounded}");

        // Draining without watching for silence leaves "the window is spent"
        // and "the pane had no more to say" indistinguishable.
        let unwatched = MidForward {
            window,
            drained: u64::from(window),
            quiet: false,
            granted: MID_FORWARD_GRANT,
            arrived: u64::from(MID_FORWARD_GRANT),
        }
        .permits_drop()
        .expect_err("a drain nobody watched to silence is not evidence of an empty window");
        assert!(unwatched.contains("watched to silence"), "{unwatched}");

        // Granting nothing asks nothing of the carrier, so where it is parked
        // was never established.
        let ungranted = full(0, 0)
            .permits_drop()
            .expect_err("a window nobody opened measures nothing");
        assert!(ungranted.contains("no window was opened"), "{ungranted}");

        // A partial drain is a daemon that still holds credit.
        let partial = MidForward {
            window,
            drained: u64::from(window) - 1,
            quiet: true,
            granted: MID_FORWARD_GRANT,
            arrived: u64::from(MID_FORWARD_GRANT),
        }
        .permits_drop()
        .expect_err("one byte of credit left is a daemon that is not waiting for any");
        assert!(partial.contains("still held credit"), "{partial}");

        // And an over-send is the ledger's violation, not a measurement.
        assert!(full(MID_FORWARD_GRANT, u64::from(MID_FORWARD_GRANT) + 1)
            .permits_drop()
            .is_err());

        // A window of zero drains itself, which would satisfy every clause
        // above without anything having happened at all.
        let empty = MidForward {
            window: 0,
            drained: 0,
            quiet: true,
            granted: MID_FORWARD_GRANT,
            arrived: u64::from(MID_FORWARD_GRANT),
        }
        .permits_drop()
        .expect_err("a window of zero is not a window that was drained");
        assert!(empty.contains("no output window"), "{empty}");
    }

    #[test]
    fn the_manoeuvre_fits_inside_the_deadline_it_is_performed_under() {
        // The daemon's stall deadline is absolute and taken once per chunk, so
        // it is already running by the time the window empties. Everything from
        // the drain to the drop has to fit inside it, or the carrier closes as
        // a slow consumer and the round fails on a measurement it is not
        // making.
        let manoeuvre = MID_FORWARD_DRAIN_DEADLINE + MID_FORWARD_QUIET + MID_FORWARD_GRANT_DEADLINE;
        assert!(
            manoeuvre < DAEMON_STALL_DEADLINE,
            "{manoeuvre:?} of manoeuvring inside a {DAEMON_STALL_DEADLINE:?} deadline"
        );
        // And the window granted at the end has to be small against what is
        // still queued, or the daemon finishes what it has and parks between
        // chunks instead of inside one. The flood is `seq 1 20000`, about
        // 108 KB, against the 64 KB the attach opens with.
        const { assert!(MID_FORWARD_GRANT > 0) };
        const { assert!(MID_FORWARD_GRANT < protocol::ws::TERMINAL_INITIAL_OUTPUT_CREDIT / 8) };
    }

    #[test]
    fn a_flap_of_no_rounds_is_refused() {
        // `--rounds 0` reached the flap and passed it: the loop never ran, so
        // nothing was dropped and every number sat at its passing value.
        assert!(flap_rounds(0).is_err());
        assert_eq!(flap_rounds(1).unwrap(), 1);
        assert_eq!(flap_rounds(30).unwrap(), 30);
    }

    #[test]
    fn an_unread_client_count_can_never_satisfy_the_no_accumulation_claim() {
        let mut clean = PeakClients::default();
        clean.observe(Ok(1));
        clean.observe(Ok(0));
        clean.observe(Ok(1));
        assert!(clean.verdict(1).is_ok());
        assert!(clean.note().contains("at most 1"));

        // Two carriers on one session is the accumulation this measures.
        let mut piling = PeakClients::default();
        piling.observe(Ok(1));
        piling.observe(Ok(2));
        assert!(piling.verdict(1).is_err());

        // A poll that answered nothing is not a count of zero, and must not
        // reach the verdict as one — the defect this replaces reported "at most
        // 0 tmux clients" off a `list-clients` that never completed.
        let mut unread = PeakClients::default();
        unread.observe(Ok(1));
        unread.observe(Err("tmux did not answer".to_string()));
        assert!(unread.verdict(1).is_err());
        assert!(unread
            .verdict(1)
            .unwrap_err()
            .contains("tmux did not answer"));

        // And a run in which nothing was ever counted proves nothing either.
        let never = PeakClients::default();
        assert!(never.verdict(1).is_err());
        assert!(!never.note().contains("at most"));
    }

    /// Every refusal an attach can meet is a code of its own, so this harness
    /// never has to read the human `reason` to know which one answered.
    ///
    /// There used to be a prose matcher here — the daemon minted two different
    /// `attachment_limit` sentences and the duplicate scenario sorted them by
    /// their English, which is a field documented as text the phone displays
    /// verbatim. The codes below replaced it, and this is what says so: if any
    /// two of them ever collide, the classification the scenarios do on the code
    /// alone silently stops distinguishing what it claims to.
    #[test]
    fn the_refusals_this_harness_tells_apart_are_distinct_codes() {
        use protocol::ws::terminal_close as tc;
        let refusals = [
            tc::ATTACHMENT_LIMIT,
            tc::SESSION_BUSY,
            tc::SUPERSEDED,
            tc::SESSION_NOT_HOSTED,
            tc::NOT_AUTHORISED,
            tc::PROTOCOL_ERROR,
        ];
        for (n, code) in refusals.iter().enumerate() {
            assert!(
                !refusals[..n].contains(code),
                "{code} answers two different refusals, so no scenario can tell \
                 them apart on the code"
            );
        }
    }

    #[test]
    fn a_soak_device_name_is_this_run_and_nothing_else() {
        // The daemon uniquifies against every row it has, revoked ones
        // included, so a name something already answers to gets `-2`, `-3`, …
        assert!(is_run_device_name(MINE, MINE));
        assert!(is_run_device_name(MINE, &format!("{MINE}-2")));
        assert!(is_run_device_name(MINE, &format!("{MINE}-13")));
        // The filter is what keeps a cleanup from revoking the operator's own
        // phone if they happened to pair one while this was pairing.
        assert!(!is_run_device_name(MINE, "iPhone"));
        assert!(!is_run_device_name(MINE, &format!("{MINE}-")));
        assert!(!is_run_device_name(MINE, &format!("{MINE}-mine")));
        assert!(!is_run_device_name(MINE, &format!("{MINE}s")));
        assert!(!is_run_device_name(MINE, &format!("my-{MINE}")));
    }

    #[test]
    fn a_concurrent_ccsoaks_row_is_never_this_runs_to_revoke() {
        // The measured defect: with one shared `ccsoak-terminal` stem, a second
        // gauntlet pairing during the seconds this one paired had its row
        // appear in this run's before/after difference — and this run's release
        // revoked it, pulling the credential out from under a live run. The
        // per-run uid is what makes the two different devices.
        assert!(!is_run_device_name(MINE, THEIRS));
        assert!(!is_run_device_name(MINE, &format!("{THEIRS}-2")));
        assert!(!is_run_device_name(THEIRS, MINE));
        // Sharing the stem is not sharing an identity.
        assert!(MINE.starts_with(DEVICE_STEM) && THEIRS.starts_with(DEVICE_STEM));
        assert!(!is_run_device_name(MINE, DEVICE_STEM));

        // And through the accounting, which is where it did the damage.
        let before = vec![device("d1", "iPhone")];
        let after = vec![
            device("d1", "iPhone"),
            device("d2", MINE),
            device("d3", THEIRS),
        ];
        let (minted, unaccounted) = accounted(MINE, Some(before), Some(after), Some(MINE));
        assert_eq!(minted, vec![device("d2", MINE)]);
        assert_eq!(unaccounted, None);
    }

    #[test]
    fn a_run_is_minted_one_name_and_it_is_recognisably_this_harness() {
        // Once per process, so every scenario's pairing and the release that
        // follows are all talking about the same device.
        assert_eq!(device_name(), device_name());
        assert!(device_name().starts_with(DEVICE_STEM), "{}", device_name());
        // And it is not the bare stem, which is the name every run on this
        // machine used to share.
        assert_ne!(device_name(), DEVICE_STEM);
        assert!(is_run_device_name(device_name(), device_name()));
    }

    #[test]
    fn a_grant_created_by_a_pairing_that_then_failed_is_still_reclaimed() {
        // The failure this accounting exists for: the code is redeemed and the
        // row written before the ack is composed, so a lost ack leaves a
        // standing shell-equivalent grant that the ack cannot name. The
        // difference between the two listings names it anyway.
        let before = vec![device("d1", "iPhone")];
        let after = vec![device("d1", "iPhone"), device("d2", MINE)];
        let (minted, unaccounted) = accounted(MINE, Some(before), Some(after), None);
        assert_eq!(minted, vec![device("d2", MINE)]);
        assert_eq!(unaccounted, None);
    }

    #[test]
    fn only_this_runs_own_rows_are_handed_back() {
        // A row that was already there is not this run's to revoke, and neither
        // is a device somebody paired alongside it.
        let before = vec![device("d1", THEIRS)];
        let after = vec![
            device("d1", THEIRS),
            device("d2", &format!("{MINE}-2")),
            device("d3", "iPhone"),
        ];
        let (minted, unaccounted) =
            accounted(MINE, Some(before), Some(after), Some(&format!("{MINE}-2")));
        assert_eq!(minted, vec![device("d2", &format!("{MINE}-2"))]);
        assert_eq!(unaccounted, None);

        // A pairing that created nothing leaves nothing to hand back.
        let (none, clean) = accounted(
            MINE,
            Some(vec![device("d1", "iPhone")]),
            Some(vec![device("d1", "iPhone")]),
            None,
        );
        assert!(none.is_empty());
        assert_eq!(clean, None);
    }

    #[test]
    fn a_pairing_that_left_two_rows_is_never_a_clean_run() {
        // One pairing buys one grant. Two rows carrying this run's own name was
        // accepted here in silence, so a redemption that wrote twice passed as
        // a clean accounting — and every sentence downstream then described one
        // grant where there were two.
        let before = vec![device("d1", "iPhone")];
        let after = vec![
            device("d1", "iPhone"),
            device("d2", MINE),
            device("d3", &format!("{MINE}-2")),
        ];
        let (minted, unaccounted) = accounted(MINE, Some(before), Some(after), Some(MINE));
        // Both are still handed back — leaving one behind would be the leak
        // this whole accounting exists to stop.
        assert_eq!(
            minted,
            vec![device("d2", MINE), device("d3", &format!("{MINE}-2"))]
        );
        let complaint = unaccounted.expect("two rows from one pairing is not a clean run");
        assert!(complaint.contains(MINE), "{complaint}");
        assert!(complaint.contains(&format!("{MINE}-2")), "{complaint}");
        assert!(
            complaint.contains("d2") && complaint.contains("d3"),
            "{complaint}"
        );
        assert!(complaint.contains("codeconnect revoke"), "{complaint}");
    }

    #[test]
    fn an_unreadable_device_list_falls_back_to_the_name_and_says_so_when_it_cannot() {
        // The listing failed but the ack named the device the daemon assigned.
        // That name is exact — `RevokeDevice` resolves one — so there is still
        // a handle and nothing is left standing.
        let (minted, unaccounted) = accounted(MINE, None, None, Some("ccsoak-terminal-9"));
        assert_eq!(minted, vec![named("ccsoak-terminal-9")]);
        assert_eq!(unaccounted, None);

        // Neither the listing nor the ack. A grant may exist and nothing here
        // can name it, which has to be reported rather than read as a clean
        // run: this is the one residue this design cannot close.
        let (nothing, residue) = accounted(MINE, None, None, None);
        assert!(nothing.is_empty());
        assert!(residue.is_some(), "an unnameable grant is never silent");
        assert!(residue.unwrap().contains("codeconnect devices"));
    }

    #[test]
    fn a_revoke_is_believed_only_once_the_row_says_revoked() {
        let minted = vec![device("d2", MINE)];
        // The daemon's own listing, showing the row carrying `revoked_at`. This
        // is the only reading that closes the grant.
        assert!(unrevoked(
            &minted,
            &[row("d1", "iPhone", false), row("d2", MINE, true)]
        )
        .is_empty());

        // The defect: `revoke_device` answered and the row is still live. Both
        // `Ok(true)` and `Ok(false)` were recorded as a clean release without
        // anybody ever looking at it.
        let standing = unrevoked(&minted, &[row("d2", MINE, false)]);
        assert_eq!(standing.len(), 1);
        assert!(
            standing[0].contains("still an active row"),
            "{}",
            standing[0]
        );
        assert!(
            standing[0].contains("codeconnect revoke d2"),
            "{}",
            standing[0]
        );

        // A revoked device stays listed, so a row that is not there at all
        // settles nothing — it is a handle that names nothing the daemon has.
        let missing = unrevoked(&minted, &[row("d9", "iPhone", false)]);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("lists no device"), "{}", missing[0]);

        // The id is what the row is matched on, never the name: another device
        // wearing this one's name is not this one's revoke.
        let impostor = unrevoked(&minted, &[row("d7", MINE, true)]);
        assert_eq!(impostor.len(), 1, "{impostor:?}");

        // With no id to be had the name is the handle, and it is matched as an
        // exact name.
        let by_name = vec![named("ccsoak-terminal-9")];
        assert!(unrevoked(&by_name, &[row("d4", "ccsoak-terminal-9", true)]).is_empty());
        assert_eq!(
            unrevoked(&by_name, &[row("d4", "ccsoak-terminal-9", false)]).len(),
            1
        );
    }

    // ------------------------------------------- the scratch session's identity

    /// One completed refusal, spelled as `run_deadlined` reports it.
    fn refused(stderr: &str) -> env::TmuxAnswer {
        env::TmuxAnswer::Failed {
            status: Some(1),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn a_tmux_refusal_proves_absence_only_when_tmux_says_so() {
        // The four stderrs below were measured on tmux 3.7b against a private
        // socket, and every one of them exits 1. Only the first two are tmux
        // saying there is no session.
        assert_eq!(
            gone_from_answer(&refused("can't find session: soak-term-x\n")),
            Gone::Yes
        );
        assert_eq!(
            gone_from_answer(&refused(
                "error connecting to /tmp/s (No such file or directory)\n"
            )),
            Gone::Yes
        );
        // A socket that is there and cannot be opened says nothing at all about
        // what is on the server behind it. This is the defect: `Gone::Yes` is
        // what disarms the watchdog, so reading a permissions failure as absence
        // retires the last rescuer a live session has.
        assert_eq!(
            gone_from_answer(&refused("error connecting to /tmp/s (Permission denied)\n")),
            Gone::Unknown
        );
        assert_eq!(
            gone_from_answer(&refused(
                "error connecting to /tmp (Socket operation on non-socket)\n"
            )),
            Gone::Unknown
        );
        // A refusal that says nothing is the same: unrecognised is never
        // promoted into proof.
        assert_eq!(gone_from_answer(&refused("")), Gone::Unknown);

        // A signalled client never finished its statement, so its stderr may be
        // a fragment of one — decided before the words are read, which is why
        // wording that would otherwise be proof still settles nothing.
        assert_eq!(
            gone_from_answer(&env::TmuxAnswer::Failed {
                status: None,
                stderr: "can't find session: soak-term-x\n".to_string(),
            }),
            Gone::Unknown
        );

        // And the two ends of the tri-state.
        assert_eq!(
            gone_from_answer(&env::TmuxAnswer::Ok(String::new())),
            Gone::No
        );
        assert_eq!(
            gone_from_answer(&env::TmuxAnswer::Unknown(
                "tmux did not answer within 5s".to_string()
            )),
            Gone::Unknown
        );
    }

    #[test]
    fn only_this_runs_own_stamp_authorises_killing_a_scratch_session() {
        const UID: &str = "01k2c9wqjy8x7v6n5m4k3j2h1g";
        let var = protocol::ENV_SESSION_UID;

        // The one confirmation there is: exit 0, and the stamp this run put on
        // the session at creation.
        assert_eq!(
            stamp_from_answer(UID, &env::TmuxAnswer::Ok(format!("{var}={UID}\n"))),
            Stamp::Ours
        );
        // Another run's session holding the same name. tmux answered, and what
        // it named is not this session.
        assert_eq!(
            stamp_from_answer(UID, &env::TmuxAnswer::Ok(format!("{var}=01other9uid\n"))),
            Stamp::NotOurs(Foreign::Stranger)
        );
        // `unknown variable` is tmux confirming a session is there and carries
        // no stamp — a session made by hand, or by an older build. Read as
        // *not ours* rather than as unreadable, and that is the safe reading in
        // both directions: this run's session is stamped by `new-session -e`, so
        // an unstamped one provably is not it, and the state authorises nothing
        // destructive. It only disarms, which is what stops the watchdog from
        // killing the stranger.
        assert_eq!(
            stamp_from_answer(UID, &refused(&format!("unknown variable: {var}\n"))),
            Stamp::NotOurs(Foreign::Stranger)
        );
        // The name is free. Nothing to kill, and nothing to guard either.
        assert_eq!(
            stamp_from_answer(UID, &refused("no such session: =soak-term-x\n")),
            Stamp::NotOurs(Foreign::Absent)
        );
        // Nothing learned, from a refusal that settles neither way, from a
        // signalled client whose stderr would otherwise have settled it, and
        // from a deadline nobody answered.
        assert!(matches!(
            stamp_from_answer(
                UID,
                &refused("error connecting to /tmp/s (Permission denied)\n")
            ),
            Stamp::Unreadable(_)
        ));
        assert!(matches!(
            stamp_from_answer(
                UID,
                &env::TmuxAnswer::Failed {
                    status: None,
                    stderr: format!("{var}={UID}\n"),
                }
            ),
            Stamp::Unreadable(_)
        ));
        assert!(matches!(
            stamp_from_answer(UID, &env::TmuxAnswer::Unknown("tmux did not answer".into())),
            Stamp::Unreadable(_)
        ));

        // Whatever the shape of the tri-state, this is the claim that matters:
        // exactly one answer lets teardown destroy anything.
        let answers = [
            env::TmuxAnswer::Ok(format!("{var}={UID}\n")),
            env::TmuxAnswer::Ok(format!("{var}=01other9uid\n")),
            env::TmuxAnswer::Ok(String::new()),
            refused(&format!("unknown variable: {var}\n")),
            refused("no such session: =soak-term-x\n"),
            refused("error connecting to /tmp/s (Permission denied)\n"),
            refused(""),
            env::TmuxAnswer::Unknown("tmux did not answer".into()),
        ];
        let permitted: Vec<bool> = answers
            .iter()
            .map(|answer| stamp_from_answer(UID, answer).permits_a_kill())
            .collect();
        assert_eq!(
            permitted,
            vec![true, false, false, false, false, false, false, false]
        );
        assert_eq!(
            answers
                .iter()
                .filter(|answer| teardown_for(&stamp_from_answer(UID, answer)) == Teardown::Remove)
                .count(),
            1,
            "more than the exact-uid answer reached the branch that kills"
        );
    }

    #[test]
    fn teardown_of_a_name_this_run_does_not_own_kills_nothing_and_disarms() {
        // Two real process groups this test owns, standing in for the pane and
        // the rescuer: "nothing was killed" is only worth asserting against
        // something that could have been. Under the name-bound teardown this
        // replaces, the pane group is SIGKILLed from the saved pid without ever
        // asking whose session it is.
        let pane = RecordedSleep::fork();
        let dog = RecordedSleep::fork();
        let mut scratch = unit_scratch(pane.pid, dog.pid);

        scratch.reclaim_with(Stamp::NotOurs(Foreign::Stranger));
        assert!(
            alive(pane.pid),
            "teardown signalled a process group on a session this run did not create"
        );
        assert!(
            wait_for_exit(dog.pid, Duration::from_secs(5)),
            "the rescuer over a name this run does not own was left armed, which is the one \
             thing that kills the stranger"
        );
        assert!(!scratch.live);
        assert_eq!(scratch.pane_pid, 0);
        assert_eq!(scratch.watchdog, 0);

        // An unreadable stamp destroys nothing and gives up nothing either: the
        // rescuer stays armed, which is safe only because it proves the stamp
        // itself before it kills.
        let dog = RecordedSleep::fork();
        let mut scratch = unit_scratch(0, dog.pid);
        scratch.reclaim_with(Stamp::Unreadable("tmux did not answer".to_string()));
        assert!(
            alive(dog.pid),
            "an unreadable stamp disarmed the last rescuer"
        );
        assert_eq!(scratch.watchdog, dog.pid);
        assert!(scratch.live);
    }

    #[test]
    fn a_watchdog_that_could_not_be_tracked_leaves_no_stray_sleep() {
        // The `&` has already run by the time stdout is read, so an arm that
        // fails after it used to leave a `/bin/sleep 300` that no `> 1` guard
        // would ever signal and that outlived the whole gauntlet.
        let stray = RecordedSleep::fork();
        assert!(alive(stray.pid), "the stand-in rescuer never started");

        let refused = watchdog_failure(
            &stray.pid_file,
            "the watchdog was forked but named no pid on stdout".to_string(),
        )
        .to_string();
        assert!(refused.contains(&stray.pid.to_string()), "{refused}");
        assert!(
            wait_for_exit(stray.pid, Duration::from_secs(5)),
            "a rejected arm left its forked rescuer running"
        );
        assert!(
            !stray.pid_file.exists(),
            "the pid file outlived the recovery"
        );

        // And when the arm failed before the fork ever reached the background
        // job there is nothing to recover, which the sentence has to say rather
        // than imply a kill that did not happen.
        let never = watchdog_pid_file();
        let refused = watchdog_failure(
            &never,
            "could not arm the scratch session's cleanup watchdog".to_string(),
        )
        .to_string();
        assert!(refused.contains("nothing was left running"), "{refused}");
        assert!(!never.exists());
    }

    #[test]
    fn a_same_named_session_this_run_did_not_create_is_never_killed() {
        let Some(server) = PrivateTmux::new("strange") else {
            println!("skipped: tmux is not installed on this machine");
            return;
        };
        let name = server.session_name();
        // Created the way a session made by hand is: no stamp at all. This is
        // the leak path exactly — `spawn` arms a rescuer on `={name}` before the
        // session exists, so a `new-session` refused because this session
        // already holds the name leaves the rescuer standing over somebody
        // else's session.
        assert!(
            server
                .run(&["new-session", "-d", "-s", &name, "--", "/bin/sleep", "120"])
                .status
                .success(),
            "the private server would not take a session"
        );

        let uid = protocol::uid::new().expect("minting a uid");
        let rescuer = server.arm(&format!("={name}"), &uid, 1);
        assert!(
            wait_for_exit(rescuer, Duration::from_secs(30)),
            "the rescuer never finished, so what it did is unknown"
        );
        assert!(
            server.has_session(&name),
            "the watchdog killed a session this harness never created"
        );
    }

    #[test]
    fn a_session_carrying_this_runs_stamp_is_removed_by_that_same_watchdog() {
        // The positive control, and it is not optional: without it the test
        // above passes against a rescuer that never kills anything at all.
        let Some(server) = PrivateTmux::new("ours") else {
            println!("skipped: tmux is not installed on this machine");
            return;
        };
        let name = server.session_name();
        let uid = protocol::uid::new().expect("minting a uid");
        assert!(
            server
                .run(&[
                    "new-session",
                    "-d",
                    "-s",
                    &name,
                    "-e",
                    &format!("{}={uid}", protocol::ENV_SESSION_UID),
                    "--",
                    "/bin/sleep",
                    "120",
                ])
                .status
                .success(),
            "the private server would not take a stamped session"
        );

        let rescuer = server.arm(&format!("={name}"), &uid, 1);
        assert!(
            wait_for_exit(rescuer, Duration::from_secs(30)),
            "the rescuer never finished, so what it did is unknown"
        );
        assert!(
            !server.has_session(&name),
            "the watchdog left this run's own session behind, so it removes nothing at all"
        );
    }

    // ------------------------------------------------------------------ fixtures

    /// A `Scratch` that owns no tmux session, for exercising the teardown
    /// decision without one. The name is this process's and nonsense, so even a
    /// reverted teardown that reached the shared server would find nothing.
    fn unit_scratch(pane_pid: i32, watchdog: i32) -> Scratch {
        Scratch {
            name: format!(
                "soak-term-unit-{}-{}",
                std::process::id(),
                protocol::time::now_unix_ms()
            ),
            uid: protocol::uid::new().expect("minting a uid"),
            pane_pid,
            watchdog,
            live: true,
            reclaimed: false,
        }
    }

    /// Whether a process this test forked is still there. `kill(pid, 0)` asks
    /// the question without sending anything.
    fn alive(pid: i32) -> bool {
        pid > 1 && unsafe { libc::kill(pid, 0) } == 0
    }

    fn wait_for_exit(pid: i32, timeout: Duration) -> bool {
        let start = Instant::now();
        while alive(pid) {
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    fn kill_group(pid: i32) {
        if pid > 1 {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }

    /// A background `sleep` in its own process group, recording its pid exactly
    /// where the rescuer records it — the shape a failed arm leaves behind.
    ///
    /// A guard rather than a bare pid, because a test that panicked between
    /// forking one of these and killing it would leak precisely the stray
    /// `sleep` these tests exist to prove cannot happen. `Drop` signals only a
    /// process that is still there, so the ordinary path — where the code under
    /// test did the killing — cannot signal a pid the kernel has since handed to
    /// somebody else.
    struct RecordedSleep {
        pid: i32,
        pid_file: std::path::PathBuf,
    }

    impl RecordedSleep {
        fn fork() -> RecordedSleep {
            let pid_file = watchdog_pid_file();
            let out = std::process::Command::new("/bin/sh")
                .args([
                    std::ffi::OsString::from("-c"),
                    std::ffi::OsString::from(
                        "set -m; (/bin/sleep 120) >/dev/null 2>&1 & echo $! > \"$1\"",
                    ),
                    std::ffi::OsString::from("--"),
                    pid_file.as_os_str().to_os_string(),
                ])
                .output()
                .expect("forking a stand-in rescuer");
            assert!(out.status.success(), "the stand-in rescuer would not fork");
            let pid = std::fs::read_to_string(&pid_file)
                .expect("the stand-in rescuer recorded no pid")
                .trim()
                .parse()
                .expect("the stand-in rescuer's pid did not parse");
            RecordedSleep { pid, pid_file }
        }
    }

    impl Drop for RecordedSleep {
        fn drop(&mut self) {
            if alive(self.pid) {
                kill_group(self.pid);
            }
            let _ = std::fs::remove_file(&self.pid_file);
        }
    }

    /// A tmux server of this test's own, on its own socket.
    ///
    /// `-S <path>` and never `-L codeconnect`. These tests create sessions and
    /// destroy servers, and the shared label is where the operator's own agents
    /// live: a `kill-server` there would be a worse defect than any this file
    /// tests for. The path carries the pid and the clock so two tests running at
    /// once cannot land on one server, and stays short because a unix socket
    /// path has about a hundred bytes to fit in.
    struct PrivateTmux {
        bin: std::path::PathBuf,
        dir: std::path::PathBuf,
        socket: std::path::PathBuf,
    }

    impl PrivateTmux {
        /// `None` when there is no tmux, which is a skip and never a failure.
        fn new(tag: &str) -> Option<PrivateTmux> {
            let bin = protocol::tmux::tmux_bin()?;
            let dir = std::env::temp_dir().join(format!(
                "ccsk-{tag}-{}-{}",
                std::process::id(),
                protocol::time::now_unix_ms()
            ));
            std::fs::create_dir_all(&dir).ok()?;
            let socket = dir.join("s");
            Some(PrivateTmux { bin, dir, socket })
        }

        fn run(&self, args: &[&str]) -> std::process::Output {
            std::process::Command::new(&self.bin)
                .arg("-S")
                .arg(&self.socket)
                .args(args)
                // So a `cargo test` run from inside tmux is not treated as a
                // nested client on somebody else's server.
                .env_remove("TMUX")
                .output()
                .expect("running tmux against a private socket")
        }

        /// A name shaped like the one `Scratch::spawn` mints, so the test is
        /// about the identity check and not about an unusual name.
        fn session_name(&self) -> String {
            format!(
                "soak-term-probe-{}-{}",
                std::process::id(),
                protocol::time::now_unix_ms()
            )
        }

        fn has_session(&self, name: &str) -> bool {
            self.run(&["has-session", "-t", &format!("={name}")])
                .status
                .success()
        }

        /// Arm the production rescuer — the same script, byte for byte — against
        /// this private server, and hand back the pid it named.
        fn arm(&self, exact: &str, uid: &str, ttl_secs: u64) -> i32 {
            let socket = self.socket.to_string_lossy().into_owned();
            let pid_file = watchdog_pid_file();
            let out = std::process::Command::new("/bin/sh")
                .args(watchdog_argv(
                    &self.bin,
                    ["-S", &socket],
                    exact,
                    uid,
                    ttl_secs,
                    &pid_file,
                ))
                .output()
                .expect("forking the watchdog");
            assert!(out.status.success(), "the watchdog would not fork");
            let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
            assert_eq!(
                std::fs::read_to_string(&pid_file)
                    .expect("the watchdog recorded no pid")
                    .trim(),
                said,
                "the pid file and stdout named different rescuers"
            );
            let _ = std::fs::remove_file(&pid_file);
            said.parse().expect("the watchdog named no pid")
        }
    }

    impl Drop for PrivateTmux {
        fn drop(&mut self) {
            // `kill-server`, and only ever reachable through `self.socket`.
            let _ = self.run(&["kill-server"]);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

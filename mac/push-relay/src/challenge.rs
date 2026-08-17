//! The value that makes an attestation about this moment rather than about a
//! recording of one.
//!
//! An App Attest object is a signed document. Nothing inside it says when it was
//! made or who asked for it, so a relay that accepted any well-formed object
//! would accept the same one forever, from anyone who ever saw it. The challenge
//! is what fixes that: 256 bits the relay issued, that the device signed over,
//! that live for ten minutes and can be spent once.
//!
//! **Only the hash is written down.** A leaked table of outstanding challenges
//! would otherwise let its reader answer a challenge it was never issued, which
//! is the entire property this module exists to provide.
//!
//! **The challenge bytes are the client data the device signs.** The relay
//! defines that convention, and it defines it this way because the value is
//! already 32 bytes of server-chosen entropy — hashing it again would add
//! nothing except a second thing for the app and the relay to disagree about.
//! Apple's own published attestation object is built the same way, which is what
//! lets the enrollment path be tested against a chain Apple actually signed.

use anyhow::{Context, Result};
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::secret::{sha256_hex, Secret};

/// Ten minutes, which is the maximum the plan allows.
///
/// It bounds two different things at once: how long a stolen challenge is worth
/// stealing, and how long the table has to remember one. Both are the reason it
/// is not longer, and a phone that cannot produce an attestation within ten
/// minutes of asking for a challenge has a problem an eleventh would not fix.
pub const LIFETIME_MS: i64 = 10 * 60 * 1_000;

/// The longest challenge string the relay will read back from a caller.
///
/// It issues 43 characters. The bound is generous rather than exact so that the
/// refusal a caller sees is "no such challenge" — a fact about the database —
/// rather than a length rule they can measure the issuer with.
pub const MAX_CHALLENGE_CHARS: usize = 128;

/// 256 bits, as the plan specifies.
const CHALLENGE_BYTES: usize = 32;

/// A freshly issued challenge, on its way out of the process for the only time.
pub struct Issued {
    pub challenge: Secret,
    pub expires_in_seconds: u64,
}

/// Why a challenge was not accepted.
///
/// Four caller-facing refusals and one that is the relay's own fault, kept apart
/// because the first four are answered with a refusal and the fifth is answered
/// with a five hundred and paged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChallengeError {
    Malformed,
    Unknown,
    Expired,
    Consumed,
    Database(String),
}

impl std::fmt::Display for ChallengeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChallengeError::Malformed => write!(f, "the challenge is not one this relay issues"),
            ChallengeError::Unknown => write!(f, "no such challenge is outstanding"),
            ChallengeError::Expired => write!(f, "the challenge is past its ten minutes"),
            ChallengeError::Consumed => write!(f, "the challenge has already been spent"),
            ChallengeError::Database(why) => write!(f, "the challenge could not be read: {why}"),
        }
    }
}

/// Issue one challenge, and take the expired ones out of the table while here.
///
/// Pruning on the issuing path is what keeps the common case immediate: on a
/// relay anyone is enrolling against, an expired challenge is gone by the time
/// the next one is asked for, which costs a single indexed delete. It is not
/// what makes ten minutes a *maximum* — a relay nobody asks would keep the row
/// for as long as nobody asked — and [`crate::db::spawn_sweeps`] is.
pub fn issue(conn: &Connection, now_ms: i64) -> Result<Issued> {
    let mut bytes = [0u8; CHALLENGE_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("the system random number generator refused"))?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    prune(conn, now_ms)?;
    store(conn, &challenge, now_ms)?;

    Ok(Issued {
        challenge: Secret::new(challenge),
        expires_in_seconds: (LIFETIME_MS / 1_000) as u64,
    })
}

/// Record a challenge the relay has decided to honour.
///
/// Separate from [`issue`] so that the acceptance tests can put Apple's own
/// published challenge into the table and drive the real enrollment path with
/// the one attestation object that exists.
pub(crate) fn store(conn: &Connection, challenge: &str, now_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO challenges (challenge_hash, issued_ms, expires_ms) VALUES (?1, ?2, ?3)",
        rusqlite::params![
            sha256_hex(challenge.as_bytes()),
            now_ms,
            now_ms + LIFETIME_MS
        ],
    )
    .context("recording an enrollment challenge")?;
    Ok(())
}

/// Spend a challenge and return the bytes the device signed over.
///
/// **Takes a transaction, not a connection.** The plan requires consumption to
/// commit with the enrollment it authorised: consumed in one transaction and
/// enrolled in another, a crash between them either burns a challenge a phone
/// still needs or leaves one spendable after the credential it paid for already
/// exists.
///
/// **Single use is a property of the file, not of a lock this process holds.**
/// Today one connection behind one mutex serialises every caller inside the
/// relay, so no two of them are ever between the read and the update at once.
/// That is an arrangement, not a guarantee: a second connection to the same
/// database — another process, an operator's tool, a relay that later stops
/// funnelling every request through one connection — is outside it. What holds
/// there is SQLite's own write serialisation plus this statement: the update
/// repeats the conditions the read checked, so a caller whose transaction began
/// before another spent the challenge and committed after it either reads the
/// spend or changes no rows, and `spent != 1` is that second case refused.
pub fn consume(
    tx: &Transaction<'_>,
    challenge: &str,
    now_ms: i64,
) -> Result<Vec<u8>, ChallengeError> {
    if challenge.is_empty() || challenge.len() > MAX_CHALLENGE_CHARS {
        return Err(ChallengeError::Malformed);
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(challenge)
        .map_err(|_| ChallengeError::Malformed)?;
    let hash = sha256_hex(challenge.as_bytes());

    let outstanding: Option<(i64, Option<i64>)> = tx
        .query_row(
            "SELECT expires_ms, consumed_ms FROM challenges WHERE challenge_hash = ?1",
            [&hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| ChallengeError::Database(e.to_string()))?;

    let (expires_ms, consumed_ms) = outstanding.ok_or(ChallengeError::Unknown)?;
    if consumed_ms.is_some() {
        return Err(ChallengeError::Consumed);
    }
    if expires_ms <= now_ms {
        return Err(ChallengeError::Expired);
    }

    let spent = tx
        .execute(
            "UPDATE challenges SET consumed_ms = ?1
             WHERE challenge_hash = ?2 AND consumed_ms IS NULL AND expires_ms > ?1",
            rusqlite::params![now_ms, hash],
        )
        .map_err(|e| ChallengeError::Database(e.to_string()))?;
    if spent != 1 {
        return Err(ChallengeError::Consumed);
    }
    Ok(decoded)
}

/// Delete every challenge past its lifetime, spent or not.
///
/// A spent challenge is deleted by the same rule rather than immediately: the
/// row is what makes a replay answer "already spent" instead of "no such
/// challenge", and ten minutes later the two are the same refusal anyway.
pub fn prune(conn: &Connection, now_ms: i64) -> Result<usize> {
    conn.execute("DELETE FROM challenges WHERE expires_ms <= ?1", [now_ms])
        .context("pruning expired challenges")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Connection {
        crate::db::open_in_memory().unwrap()
    }

    const NOW: i64 = 1_700_000_000_000;

    fn outstanding(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM challenges", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn an_issued_challenge_is_256_bits_of_base64url_and_never_the_same_twice() {
        let conn = database();
        let first = issue(&conn, NOW).unwrap();
        let second = issue(&conn, NOW).unwrap();

        assert_ne!(first.challenge.expose(), second.challenge.expose());
        // 32 bytes is 43 unpadded base64 characters.
        assert_eq!(first.challenge.expose().len(), 43);
        assert!(!first.challenge.expose().contains('='));
        assert_eq!(first.expires_in_seconds, 600);
        assert_eq!(outstanding(&conn), 2);
    }

    /// **The table is a list of digests.** A reader of the file must not be able
    /// to answer a challenge they were never issued.
    #[test]
    fn the_table_holds_the_hash_and_never_the_challenge() {
        let conn = database();
        let issued = issue(&conn, NOW).unwrap();

        let stored: String = conn
            .query_row("SELECT challenge_hash FROM challenges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, sha256_hex(issued.challenge.expose().as_bytes()));
        assert_ne!(stored, issued.challenge.expose());
        assert!(!stored.contains(issued.challenge.expose()));
    }

    #[test]
    fn a_challenge_is_spent_once_and_the_replay_says_so() {
        let mut conn = database();
        let issued = issue(&conn, NOW).unwrap();
        let challenge = issued.challenge.expose().to_string();

        let tx = conn.transaction().unwrap();
        let bytes = consume(&tx, &challenge, NOW).expect("the first spend");
        assert_eq!(bytes.len(), CHALLENGE_BYTES);
        tx.commit().unwrap();

        let tx = conn.transaction().unwrap();
        assert_eq!(
            consume(&tx, &challenge, NOW),
            Err(ChallengeError::Consumed),
            "a replayed challenge must not authorise a second enrollment"
        );
    }

    /// **Single use across connections, which is the level the property has to
    /// hold at.** Two real connections to one file, because a single connection
    /// behind a mutex would prove the mutex — and the mutex is an arrangement
    /// inside this process rather than a fact about the database.
    #[test]
    fn two_connections_racing_on_one_challenge_spend_it_exactly_once() {
        let dir =
            std::env::temp_dir().join(format!("push-relay-challenge-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("relay.sqlite");

        let conn = crate::db::open(&path).unwrap();
        let issued = issue(&conn, NOW).unwrap();
        let challenge = issued.challenge.expose().to_string();
        drop(conn);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = barrier.clone();
            let challenge = challenge.clone();
            threads.push(std::thread::spawn(move || {
                let mut conn = crate::db::open(&path).unwrap();
                barrier.wait();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                let outcome = consume(&tx, &challenge, NOW);
                tx.commit().unwrap();
                outcome
            }));
        }
        let outcomes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();

        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
            1,
            "one challenge authorised two enrollments: {outcomes:?}"
        );
        // And the caller that lost is refused by the rule rather than by a
        // database error it would have to be told to retry.
        assert!(
            outcomes.iter().any(|outcome| outcome
                .as_ref()
                .err()
                .is_some_and(|e| *e == ChallengeError::Consumed)),
            "{outcomes:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The rollback case.** An enrollment that fails after consuming its
    /// challenge leaves the challenge spendable, because the whole thing is one
    /// transaction — which is also what makes the successful case atomic.
    #[test]
    fn a_rolled_back_enrollment_does_not_burn_the_challenge() {
        let mut conn = database();
        let issued = issue(&conn, NOW).unwrap();
        let challenge = issued.challenge.expose().to_string();

        let tx = conn.transaction().unwrap();
        consume(&tx, &challenge, NOW).unwrap();
        drop(tx);

        let tx = conn.transaction().unwrap();
        assert!(consume(&tx, &challenge, NOW).is_ok());
    }

    #[test]
    fn ten_minutes_is_the_boundary_and_it_is_exclusive_at_the_far_end() {
        let mut conn = database();
        let issued = issue(&conn, NOW).unwrap();
        let challenge = issued.challenge.expose().to_string();

        let tx = conn.transaction().unwrap();
        assert!(consume(&tx, &challenge, NOW + LIFETIME_MS - 1).is_ok());
        drop(tx);

        let tx = conn.transaction().unwrap();
        assert_eq!(
            consume(&tx, &challenge, NOW + LIFETIME_MS),
            Err(ChallengeError::Expired)
        );
        assert_eq!(
            consume(&tx, &challenge, NOW + LIFETIME_MS + 1),
            Err(ChallengeError::Expired)
        );
    }

    #[test]
    fn a_challenge_nobody_issued_is_unknown_and_a_shapeless_one_is_malformed() {
        let mut conn = database();
        let tx = conn.transaction().unwrap();

        let unissued = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
        assert_eq!(consume(&tx, &unissued, NOW), Err(ChallengeError::Unknown));

        for shapeless in [
            "",
            "not base64url!!",
            "AAAA=",
            &"A".repeat(MAX_CHALLENGE_CHARS + 1),
        ] {
            assert_eq!(
                consume(&tx, shapeless, NOW),
                Err(ChallengeError::Malformed),
                "{shapeless:?}"
            );
        }
    }

    /// The retention promise is about the file, so it is asserted about the file.
    #[test]
    fn nothing_older_than_ten_minutes_survives_in_the_table() {
        let conn = database();
        for _ in 0..5 {
            issue(&conn, NOW).unwrap();
        }
        assert_eq!(outstanding(&conn), 5);

        // Issuing after the window sweeps them, which is the immediate case;
        // the timer in `crate::db` is what covers the relay nobody is asking.
        issue(&conn, NOW + LIFETIME_MS).unwrap();
        assert_eq!(outstanding(&conn), 1);

        assert_eq!(prune(&conn, NOW + LIFETIME_MS * 3).unwrap(), 1);
        assert_eq!(outstanding(&conn), 0);
    }

    /// The far end of the same rule: a challenge inside its ten minutes is not
    /// swept, so a sweep on a short interval cannot take one a phone still has
    /// time to spend.
    #[test]
    fn a_challenge_still_inside_its_ten_minutes_survives_a_sweep() {
        let conn = database();
        issue(&conn, NOW).unwrap();

        assert_eq!(prune(&conn, NOW + LIFETIME_MS - 1).unwrap(), 0);
        assert_eq!(outstanding(&conn), 1);
        assert_eq!(prune(&conn, NOW + LIFETIME_MS).unwrap(), 1);
        assert_eq!(outstanding(&conn), 0);
    }
}

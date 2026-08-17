//! Abuse limits, and the reason none of their keys is an identifier.
//!
//! **Rate keys are the token binding, never the credential.** A budget attached
//! to a bearer is a budget anyone can reset by rotating, which turns the one
//! lifecycle operation the relay offers for free into the way around every
//! limit. The key is the token's hash, so a rotation lands on the same bucket
//! the previous credential was spending from.
//!
//! **Address keys are a daily rotating HMAC and never an address.** The pepper
//! is a secret file, the day is part of the message, and the result is a hex
//! digest — so yesterday's buckets are unreachable rather than merely old, and a
//! copy of the database says how much traffic came from somebody without saying
//! from where. A raw address is not written to this table and does not reach a
//! log line; [`crate::logging::RequestLog`] has no field that could carry one.
//!
//! **The pepper falls back to a random one, not to no limiting.** A relay whose
//! pepper file has not been mounted still has to refuse a flood. Restarting then
//! resets the address buckets, which matters for at most a day — the buckets
//! expire within a day anyway — and is a far smaller failure than an endpoint
//! that stops counting.
//!
//! Two windows, because the limits are two shapes. Burst and per-minute are a
//! token bucket in memory, where a restart losing them costs nothing. The daily
//! count is in SQLite, because a restart handing every caller a fresh five
//! hundred is exactly what an attacker would arrange.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::Connection;

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const MINUTE_MS: f64 = 60_000.0;

/// How often the sweeps run, at most. Both tables are small and both sweeps are
/// a single statement, but running either on every request would make a limiter
/// the most expensive part of a request it allowed.
const PRUNE_INTERVAL_MS: i64 = 60_000;

/// A memory window untouched for this long is a caller who has gone away. Five
/// minutes is long enough that a returning caller does not get a free burst it
/// had already spent, since the bucket refills completely in one.
const IDLE_WINDOW_MS: i64 = 5 * 60_000;

/// One rule: how many at once, how many a minute, and how many a day.
///
/// The first two are one token bucket — capacity is the burst, refill is the
/// sustained rate — because they are the same statement about a caller measured
/// over two intervals, and two independent counters would let a caller alternate
/// between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub burst: u32,
    pub per_minute: u32,
    /// Zero means the rule has no daily component and touches no table.
    pub per_day: u32,
}

/// Per token binding, from the plan: burst 10, sustained 10/minute, 500/day.
///
/// Every route that can be attributed to a token spends from this one bucket —
/// pushes, status checks, and lifecycle operations alike — because a budget the
/// caller can move between is not a budget.
pub const BINDING: Limit = Limit {
    burst: 10,
    per_minute: 10,
    per_day: 500,
};

/// What an unauthenticated caller naming a token may spend before it has proved
/// anything.
///
/// **A separate bucket from [`BINDING`], and this is the whole point.** An
/// enrollment or an assertion names a token in its body, and until the
/// attestation or the assertion has been verified nothing connects the caller to
/// that phone. Charging the phone's own budget there would let anyone holding a
/// stolen APNs token — which §5 says "alone cannot use the relay" — silence it
/// for a day by spending its five hundred with documents that verify against
/// nothing. Sized like the address rule, because the work behind it is the same
/// certificate verification.
pub const UNVERIFIED_TOKEN: Limit = Limit {
    burst: 5,
    per_minute: 5,
    per_day: 50,
};

/// Challenges are cheap to issue and cheap to ask for, so the address limit is
/// the one that matters and it is generous: a phone that reinstalls, retries,
/// and re-enrolls a few times in a day is not an attacker.
pub const CHALLENGE_IP: Limit = Limit {
    burst: 10,
    per_minute: 10,
    per_day: 200,
};

/// Enrollment runs certificate-chain verification, which is the most expensive
/// thing an unauthenticated caller can ask for, so its address limit is tighter
/// than the challenge that precedes it.
pub const ENROLL_IP: Limit = Limit {
    burst: 5,
    per_minute: 5,
    per_day: 50,
};

/// Invalid authentication gets the strict rotating-address rule, because a
/// caller guessing bearers has no legitimate version of that behaviour.
pub const INVALID_AUTH_IP: Limit = Limit {
    burst: 5,
    per_minute: 5,
    per_day: 25,
};

/// The service-wide ceiling on challenge issuance. Memory only: a global daily
/// cap would take the whole fleet down for the rest of the day after one bad
/// afternoon, which is an outage rather than a limit.
pub const CHALLENGE_GLOBAL: Limit = Limit {
    burst: 200,
    per_minute: 200,
    per_day: 0,
};

/// The same ceiling for enrollment, sized to the verification work behind it.
pub const ENROLL_GLOBAL: Limit = Limit {
    burst: 100,
    per_minute: 100,
    per_day: 0,
};

/// The two service-wide keys. Constants rather than derived strings so that
/// nothing a caller sends can land on them.
pub const CHALLENGE_GLOBAL_KEY: &str = "global:challenge";
pub const ENROLL_GLOBAL_KEY: &str = "global:enroll";

/// Whether a request may proceed, and if not, when to come back.
///
/// `Retry-After` is on the refusal because the plan requires it and because
/// neither the daemon nor the app retries automatically: the number is for the
/// human and the next deliberate attempt, not for a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Limited { retry_after_seconds: u64 },
}

impl Decision {
    pub fn retry_after_seconds(self) -> Option<u64> {
        match self {
            Decision::Allowed => None,
            Decision::Limited {
                retry_after_seconds,
            } => Some(retry_after_seconds),
        }
    }

    pub fn is_allowed(self) -> bool {
        matches!(self, Decision::Allowed)
    }
}

/// The bucket every request about one token spends from.
///
/// **Derived from the token's hash and nothing else.** Not the bearer, not the
/// installation, not the credential's generation — all three change on a
/// rotation, and a key that changed with them would hand out a fresh budget for
/// the price of one assertion.
pub fn binding_key(token_hash: &str) -> String {
    format!("binding:{token_hash}")
}

/// The bucket a caller spends from while it is still only *claiming* a token.
///
/// The same token hash as [`binding_key`] with a different prefix, so the two
/// are one namespace apart: a caller that cannot verify spends here and never
/// touches the budget the phone pushes from, and §3's rule that the key is the
/// token binding rather than the credential is untouched — a rotation still
/// lands on the same bucket it always did.
pub fn unverified_key(token_hash: &str) -> String {
    format!("unverified:{token_hash}")
}

/// Where the pepper came from, reported by `/readyz` so an operator can see that
/// address buckets will not survive a restart.
pub const PEPPER_FILE: &str = "file";
pub const PEPPER_EPHEMERAL: &str = "ephemeral";

struct Window {
    tokens: f64,
    updated_ms: i64,
}

pub struct Limiter {
    pepper: hmac::Key,
    pepper_source: &'static str,
    windows: Mutex<HashMap<String, Window>>,
    windows_swept_ms: AtomicI64,
    buckets_swept_ms: AtomicI64,
}

impl Limiter {
    /// Load the pepper, or mint one for this process.
    ///
    /// The generator refusing is a panic and not a fallback: a relay that cannot
    /// produce random bytes cannot mint a credential either, so it is broken
    /// rather than degraded, and starting anyway would hide that behind a
    /// working health check.
    pub fn new(pepper_file: &Path) -> Self {
        let (material, source) = match std::fs::read(pepper_file) {
            Ok(bytes) if !bytes.is_empty() => (bytes, PEPPER_FILE),
            _ => {
                let mut bytes = [0u8; 32];
                SystemRandom::new()
                    .fill(&mut bytes)
                    .expect("the system random number generator refused");
                (bytes.to_vec(), PEPPER_EPHEMERAL)
            }
        };
        Limiter {
            pepper: hmac::Key::new(hmac::HMAC_SHA256, &material),
            pepper_source: source,
            windows: Mutex::new(HashMap::new()),
            windows_swept_ms: AtomicI64::new(i64::MIN),
            buckets_swept_ms: AtomicI64::new(i64::MIN),
        }
    }

    pub fn pepper_source(&self) -> &'static str {
        self.pepper_source
    }

    /// The bucket for one address on one day, under one purpose.
    ///
    /// The day is inside the HMAC rather than beside it, so a key does not
    /// merely stop being used at midnight — it stops being derivable, and
    /// yesterday's rows cannot be linked to today's caller by anyone reading the
    /// table.
    ///
    /// An absent address is its own bucket rather than an exemption. Behind the
    /// deployment's proxy the header is always set; locally it is not, and the
    /// shared bucket is the fail-closed answer.
    pub fn ip_key(&self, purpose: &'static str, ip: Option<&str>, now_ms: i64) -> String {
        let day = now_ms.div_euclid(DAY_MS);
        let address = ip.unwrap_or("unknown");
        let tag = hmac::sign(&self.pepper, format!("{day}\u{0}{address}").as_bytes());
        let mut hex = String::with_capacity(32);
        for byte in &tag.as_ref()[..16] {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        format!("{purpose}:{hex}")
    }

    /// Spend one unit of `key`'s budget, or say when to come back.
    ///
    /// The memory bucket is spent first and is **not** refunded when the daily
    /// cap refuses afterwards. A caller who is over their day should not also be
    /// refilling the window they will meet tomorrow, and the alternative — hold
    /// the map locked across a database write — puts a mutex around the disk.
    pub fn check(
        &self,
        conn: &Connection,
        key: &str,
        limit: Limit,
        now_ms: i64,
    ) -> Result<Decision> {
        let burst = self.take(key, limit, now_ms);
        if !burst.is_allowed() {
            return Ok(burst);
        }
        if limit.per_day == 0 {
            return Ok(burst);
        }
        self.daily(conn, key, limit, now_ms)
    }

    /// Whether `key` has nothing left, asked without spending anything.
    ///
    /// **A read, and on the hot path not even that.** The caller is a route that
    /// has to decide whether a request is worth doing work for *before* it knows
    /// which rule the request belongs to, so it cannot spend: charging every
    /// caller against a rule most of them are not breaking would be a limit on
    /// the wrong traffic. The memory window is consulted first and answers
    /// without a statement, which is what makes a sustained flood cost no
    /// database write at all — the table is touched only while the burst has
    /// something left in it.
    ///
    /// The window is not advanced here, so a peek neither refills nor spends;
    /// the refill it computes is the same one [`Self::take`] would.
    pub fn spent(&self, conn: &Connection, key: &str, limit: Limit, now_ms: i64) -> bool {
        if self.available(key, limit, now_ms) < 1.0 {
            return true;
        }
        if limit.per_day == 0 {
            return false;
        }
        let day_start = now_ms - now_ms.rem_euclid(DAY_MS);
        let count: i64 = conn
            .query_row(
                "SELECT day_count FROM rate_buckets WHERE bucket_key = ?1 AND day_start_ms = ?2",
                rusqlite::params![key, day_start],
                |row| row.get(0),
            )
            .unwrap_or(0);
        count >= i64::from(limit.per_day)
    }

    /// The tokens `key` would have if it were spending now, without spending.
    fn available(&self, key: &str, limit: Limit, now_ms: i64) -> f64 {
        let windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match windows.get(key) {
            None => f64::from(limit.burst),
            Some(window) => {
                let elapsed = now_ms.saturating_sub(window.updated_ms).max(0) as f64;
                (window.tokens + elapsed * f64::from(limit.per_minute) / MINUTE_MS)
                    .min(f64::from(limit.burst))
            }
        }
    }

    fn take(&self, key: &str, limit: Limit, now_ms: i64) -> Decision {
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A caller who has gone away is a map entry that never goes away, so the
        // map is swept on the same interval as the table rather than on every
        // request — a sweep per request would be linear in the fleet.
        if due(&self.windows_swept_ms, now_ms) {
            windows.retain(|_, window| now_ms.saturating_sub(window.updated_ms) <= IDLE_WINDOW_MS);
        }

        let window = windows.entry(key.to_string()).or_insert(Window {
            tokens: f64::from(limit.burst),
            updated_ms: now_ms,
        });
        let elapsed = now_ms.saturating_sub(window.updated_ms).max(0) as f64;
        window.tokens = (window.tokens + elapsed * f64::from(limit.per_minute) / MINUTE_MS)
            .min(f64::from(limit.burst));
        window.updated_ms = now_ms;

        if window.tokens >= 1.0 {
            window.tokens -= 1.0;
            return Decision::Allowed;
        }
        let wait_ms = (1.0 - window.tokens) * MINUTE_MS / f64::from(limit.per_minute);
        Decision::Limited {
            retry_after_seconds: (wait_ms / 1_000.0).ceil().max(1.0) as u64,
        }
    }

    /// The count that survives a restart, and the sweep that keeps the table
    /// from becoming a list of every address that ever called.
    fn daily(&self, conn: &Connection, key: &str, limit: Limit, now_ms: i64) -> Result<Decision> {
        let day_start = now_ms - now_ms.rem_euclid(DAY_MS);
        self.sweep_if_due(conn, now_ms)?;

        // One statement, so two callers on one key cannot both read the same
        // count and both write it back incremented by one.
        let count: i64 = conn
            .query_row(
                "INSERT INTO rate_buckets (bucket_key, day_start_ms, day_count) VALUES (?1, ?2, 1)
                 ON CONFLICT (bucket_key) DO UPDATE SET
                     day_count = CASE WHEN rate_buckets.day_start_ms = excluded.day_start_ms
                                      THEN rate_buckets.day_count + 1
                                      ELSE 1 END,
                     day_start_ms = excluded.day_start_ms
                 RETURNING day_count",
                rusqlite::params![key, day_start],
                |row| row.get(0),
            )
            .context("counting a request against its daily budget")?;

        if count > i64::from(limit.per_day) {
            let remaining_ms = (day_start + DAY_MS - now_ms).max(1_000);
            return Ok(Decision::Limited {
                retry_after_seconds: (remaining_ms / 1_000) as u64,
            });
        }
        Ok(Decision::Allowed)
    }

    /// Delete every bucket from a day that has ended, and answer how many.
    ///
    /// A bucket for a live key is reset in place by the upsert above, so what
    /// accumulates is the keys nobody will use again — every rotated address
    /// key, once a day, forever. This is what bounds the table.
    ///
    /// **The idle memory windows go with them.** They are the same rotating
    /// address keys held in this process, and the request path only reaches
    /// them while requests are arriving — so the fleet that stops pushing is
    /// exactly the case neither half was swept in.
    ///
    /// Public and driven by `now_ms` because the timer in [`crate::db`] calls
    /// it with no request to derive a clock from, and because a test that had
    /// to wait for a day to end would be a test that waits for a day to end.
    pub fn sweep(&self, conn: &Connection, now_ms: i64) -> Result<usize> {
        let day_start = now_ms - now_ms.rem_euclid(DAY_MS);
        let removed = conn
            .execute(
                "DELETE FROM rate_buckets WHERE day_start_ms < ?1",
                [day_start],
            )
            .context("sweeping expired rate buckets")?;
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, window| now_ms.saturating_sub(window.updated_ms) <= IDLE_WINDOW_MS);
        Ok(removed)
    }

    /// The same sweep on a request that needs a daily limit, at most once per
    /// [`PRUNE_INTERVAL_MS`] so that a limiter is not the most expensive part
    /// of a request it allowed.
    fn sweep_if_due(&self, conn: &Connection, now_ms: i64) -> Result<()> {
        if !due(&self.buckets_swept_ms, now_ms) {
            return Ok(());
        }
        self.sweep(conn, now_ms)?;
        Ok(())
    }

    /// How many memory windows are being held, for the test that the sweep
    /// releases the ones nobody is spending from.
    #[cfg(test)]
    fn window_count(&self) -> usize {
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// Whether a sweep is due, claiming the interval as it answers.
fn due(last_ms: &AtomicI64, now_ms: i64) -> bool {
    let last = last_ms.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < PRUNE_INTERVAL_MS {
        return false;
    }
    last_ms.store(now_ms, Ordering::Relaxed);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn limiter() -> Limiter {
        Limiter::new(Path::new("/nonexistent/ip-pepper"))
    }

    fn database() -> Connection {
        crate::db::open_in_memory().unwrap()
    }

    fn buckets(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM rate_buckets", [], |r| r.get(0))
            .unwrap()
    }

    /// Burst 10 and sustained 10 a minute, as the plan states them.
    #[test]
    fn ten_at_once_then_a_refusal_that_says_when_to_come_back() {
        let limiter = limiter();
        let conn = database();
        let key = binding_key(&"a".repeat(64));

        for attempt in 0..BINDING.burst {
            assert!(
                limiter
                    .check(&conn, &key, BINDING, NOW)
                    .unwrap()
                    .is_allowed(),
                "attempt {attempt} of the burst"
            );
        }
        let refused = limiter.check(&conn, &key, BINDING, NOW).unwrap();
        let retry = refused
            .retry_after_seconds()
            .expect("a 429 carries Retry-After");
        assert!((1..=60).contains(&retry), "{retry}");

        // Refilling at ten a minute is one token every six seconds.
        assert!(limiter
            .check(&conn, &key, BINDING, NOW + 6_000)
            .unwrap()
            .is_allowed());
        assert!(!limiter
            .check(&conn, &key, BINDING, NOW + 6_000)
            .unwrap()
            .is_allowed());
    }

    /// **The daily count is in the database, so a restart is not a fresh 500.**
    #[test]
    fn five_hundred_a_day_survives_the_process_that_counted_them() {
        let conn = database();
        let key = binding_key(&"b".repeat(64));

        // A new limiter every few requests is the restart being simulated; the
        // clock advances enough to keep the per-minute window out of the way.
        let mut spent = 0;
        while spent < BINDING.per_day {
            let limiter = limiter();
            for _ in 0..5 {
                let now = NOW + i64::from(spent) * 6_000;
                assert!(
                    limiter
                        .check(&conn, &key, BINDING, now)
                        .unwrap()
                        .is_allowed(),
                    "request {spent} of the day"
                );
                spent += 1;
            }
        }

        let limiter = limiter();
        let over = limiter
            .check(&conn, &key, BINDING, NOW + i64::from(spent) * 6_000)
            .unwrap();
        let retry = over
            .retry_after_seconds()
            .expect("a 429 carries Retry-After");
        assert!(retry > 0);
        assert!(retry <= (DAY_MS / 1_000) as u64, "{retry}");
    }

    /// The acceptance gate, in one line: the key is the token, and a rotation
    /// changes the bearer and the generation, neither of which appears here.
    #[test]
    fn the_binding_key_is_the_token_and_nothing_that_a_rotation_changes() {
        let token_hash = "c".repeat(64);
        assert_eq!(binding_key(&token_hash), binding_key(&token_hash));
        assert!(binding_key(&token_hash).contains(&token_hash));
        assert_ne!(binding_key(&token_hash), binding_key(&"d".repeat(64)));
    }

    #[test]
    fn an_address_key_rotates_daily_and_is_never_the_address() {
        let limiter = limiter();
        let today = limiter.ip_key("enroll", Some("203.0.113.7"), NOW);
        let again = limiter.ip_key("enroll", Some("203.0.113.7"), NOW + 1_000);
        let tomorrow = limiter.ip_key("enroll", Some("203.0.113.7"), NOW + DAY_MS);
        let elsewhere = limiter.ip_key("enroll", Some("203.0.113.8"), NOW);
        let other_purpose = limiter.ip_key("challenge", Some("203.0.113.7"), NOW);

        assert_eq!(today, again);
        assert_ne!(today, tomorrow, "an address key expires within a day");
        assert_ne!(today, elsewhere);
        assert_ne!(today, other_purpose);
        assert!(!today.contains("203.0.113.7"));
        assert!(!today.contains('.'));

        // And a second pepper is a second namespace, so a stolen table cannot be
        // replayed against a running relay.
        let elsewhere_peppered = Limiter::new(Path::new("/nonexistent/ip-pepper"));
        assert_ne!(
            today,
            elsewhere_peppered.ip_key("enroll", Some("203.0.113.7"), NOW)
        );
    }

    /// An absent address shares one bucket rather than skipping the limit.
    #[test]
    fn a_request_without_an_address_is_still_counted() {
        let limiter = limiter();
        let conn = database();
        let key = limiter.ip_key("challenge", None, NOW);
        assert_eq!(key, limiter.ip_key("challenge", None, NOW));

        for _ in 0..CHALLENGE_IP.burst {
            assert!(limiter
                .check(&conn, &key, CHALLENGE_IP, NOW)
                .unwrap()
                .is_allowed());
        }
        assert!(!limiter
            .check(&conn, &key, CHALLENGE_IP, NOW)
            .unwrap()
            .is_allowed());
    }

    /// **The table cannot become a list of every address that ever called.**
    #[test]
    fn yesterdays_buckets_are_gone_rather_than_merely_stale() {
        let limiter = limiter();
        let conn = database();

        for index in 0..20 {
            let key = limiter.ip_key("enroll", Some(&format!("198.51.100.{index}")), NOW);
            limiter.check(&conn, &key, ENROLL_IP, NOW).unwrap();
        }
        assert_eq!(buckets(&conn), 20);

        let tomorrow = limiter.ip_key("enroll", Some("198.51.100.0"), NOW + DAY_MS);
        limiter
            .check(&conn, &tomorrow, ENROLL_IP, NOW + DAY_MS)
            .unwrap();
        assert_eq!(buckets(&conn), 1, "yesterday's rows must not survive");
    }

    /// **The same rows go without a second request arriving**, which is the
    /// case the timer exists for: a fleet that stopped calling yesterday must
    /// not leave a table of yesterday's address keys behind it.
    #[test]
    fn the_sweep_drops_a_finished_day_and_keeps_the_one_in_progress() {
        let limiter = limiter();
        let conn = database();

        for index in 0..20 {
            let key = limiter.ip_key("enroll", Some(&format!("198.51.100.{index}")), NOW);
            limiter.check(&conn, &key, ENROLL_IP, NOW).unwrap();
        }
        assert_eq!(buckets(&conn), 20);
        assert_eq!(limiter.window_count(), 20);

        // Later the same day: every row is still the day in progress, and the
        // windows have been spent from within the idle interval.
        assert_eq!(limiter.sweep(&conn, NOW + 1_000).unwrap(), 0);
        assert_eq!(buckets(&conn), 20);
        assert_eq!(limiter.window_count(), 20);

        assert_eq!(limiter.sweep(&conn, NOW + DAY_MS).unwrap(), 20);
        assert_eq!(buckets(&conn), 0);
        assert_eq!(
            limiter.window_count(),
            0,
            "a caller who has gone away must not be held in memory either"
        );
    }

    /// A rule with no daily component writes nothing, which is what keeps the
    /// service-wide ceiling from being a single hot row.
    #[test]
    fn a_memory_only_rule_touches_no_table() {
        let limiter = limiter();
        let conn = database();
        for _ in 0..CHALLENGE_GLOBAL.burst {
            assert!(limiter
                .check(&conn, CHALLENGE_GLOBAL_KEY, CHALLENGE_GLOBAL, NOW)
                .unwrap()
                .is_allowed());
        }
        assert!(!limiter
            .check(&conn, CHALLENGE_GLOBAL_KEY, CHALLENGE_GLOBAL, NOW)
            .unwrap()
            .is_allowed());
        assert_eq!(buckets(&conn), 0);
    }

    /// **A peek spends nothing and writes nothing.** It is asked on the path a
    /// flood takes, so a peek that charged would refuse the traffic it is meant
    /// to let through, and one that wrote would be the cost it exists to avoid.
    #[test]
    fn asking_whether_a_bucket_is_spent_neither_spends_it_nor_writes_a_row() {
        let limiter = limiter();
        let conn = database();
        let key = limiter.ip_key("auth", Some("203.0.113.7"), NOW);

        for _ in 0..20 {
            assert!(!limiter.spent(&conn, &key, INVALID_AUTH_IP, NOW));
        }
        assert_eq!(buckets(&conn), 0, "a peek is not a write");

        for attempt in 0..INVALID_AUTH_IP.burst {
            assert!(
                limiter
                    .check(&conn, &key, INVALID_AUTH_IP, NOW)
                    .unwrap()
                    .is_allowed(),
                "attempt {attempt}"
            );
        }
        assert!(limiter.spent(&conn, &key, INVALID_AUTH_IP, NOW));

        // The burst refills at five a minute, and the peek sees that without
        // taking the token it is reporting.
        assert!(!limiter.spent(&conn, &key, INVALID_AUTH_IP, NOW + 12_000));
        assert!(limiter
            .check(&conn, &key, INVALID_AUTH_IP, NOW + 12_000)
            .unwrap()
            .is_allowed());
    }

    /// The daily half of a peek, which is the half that outlives a restart.
    #[test]
    fn a_bucket_whose_day_is_spent_reads_as_spent_from_a_fresh_process() {
        let conn = database();
        let key = limiter().ip_key("auth", Some("198.51.100.4"), NOW);

        let mut spent = 0;
        while spent < INVALID_AUTH_IP.per_day {
            let limiter = limiter();
            let now = NOW + i64::from(spent) * 12_000;
            assert!(limiter
                .check(&conn, &key, INVALID_AUTH_IP, now)
                .unwrap()
                .is_allowed());
            spent += 1;
        }
        // A new limiter has a full memory window, so what answers here is the
        // count in the table and nothing else.
        let fresh = limiter();
        let later = NOW + i64::from(spent) * 12_000;
        assert!(fresh.spent(&conn, &key, INVALID_AUTH_IP, later));
        assert!(!fresh.spent(&conn, &key, INVALID_AUTH_IP, later + DAY_MS));
    }

    /// **An unverified caller and the phone it names never share a bucket**, so
    /// naming somebody's token cannot spend the budget they push from.
    #[test]
    fn the_unverified_bucket_is_a_different_bucket_from_the_bindings() {
        let token_hash = "e".repeat(64);
        assert_ne!(unverified_key(&token_hash), binding_key(&token_hash));
        assert!(unverified_key(&token_hash).contains(&token_hash));
        assert_eq!(unverified_key(&token_hash), unverified_key(&token_hash));
        assert_ne!(unverified_key(&token_hash), unverified_key(&"f".repeat(64)));

        let limiter = limiter();
        let conn = database();
        for _ in 0..UNVERIFIED_TOKEN.burst {
            assert!(limiter
                .check(&conn, &unverified_key(&token_hash), UNVERIFIED_TOKEN, NOW)
                .unwrap()
                .is_allowed());
        }
        assert!(!limiter
            .check(&conn, &unverified_key(&token_hash), UNVERIFIED_TOKEN, NOW)
            .unwrap()
            .is_allowed());
        // And the binding's own bucket is untouched by all of it.
        assert!(limiter
            .check(&conn, &binding_key(&token_hash), BINDING, NOW)
            .unwrap()
            .is_allowed());
    }

    #[test]
    fn a_pepper_file_is_used_when_it_is_there_and_reported_when_it_is_not() {
        let dir = std::env::temp_dir().join(format!("push-relay-pepper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pepper = dir.join("ip-pepper");

        assert_eq!(Limiter::new(&pepper).pepper_source(), PEPPER_EPHEMERAL);

        std::fs::write(&pepper, b"a mounted secret file").unwrap();
        let first = Limiter::new(&pepper);
        let second = Limiter::new(&pepper);
        assert_eq!(first.pepper_source(), PEPPER_FILE);
        // The same file is the same namespace across restarts, which is what
        // makes a bucket outlive the process that opened it.
        assert_eq!(
            first.ip_key("enroll", Some("203.0.113.7"), NOW),
            second.ip_key("enroll", Some("203.0.113.7"), NOW)
        );

        // An empty file is an unmounted one, not a pepper of zero bytes.
        std::fs::write(&pepper, b"").unwrap();
        assert_eq!(Limiter::new(&pepper).pepper_source(), PEPPER_EPHEMERAL);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

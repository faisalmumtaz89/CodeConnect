//! `session_uid` — the stable, unique identity of one agent run.
//!
//! ## Why this exists
//!
//! `cc-1` is a *tmux session name*. `codeconnect claude` picks the lowest free one, so
//! when a session dies the next one is called `cc-1` again. That name was
//! originally the event log's primary key too, which meant a new session
//! inherited the dead one's log and continued its `seq` numbering. Two concrete
//! failures came out of that:
//!
//!   * **History integrity.** One `cc-1` timeline on the phone was actually two
//!     unrelated agent runs spliced together, with no marker between them.
//!   * **Approval safety.** A `request_id` answered in the first run stayed in
//!     the answers ledger under the same session key, so a card from the second
//!     run that happened to reuse an id would be reported as an already-applied
//!     duplicate — an answer nobody gave.
//!
//! The fix is to separate the two jobs the name was doing. `session_id` stays
//! the human-facing tmux name (display, `codeconnect attach`, `send-keys` targeting);
//! `session_uid` is minted once at spawn, never reused, and is what the log,
//! the tail cursors and the answers ledger are keyed by.
//!
//! ## Why ULID rather than a UUID
//!
//! A ULID is 48 bits of millisecond timestamp followed by 80 bits of entropy,
//! rendered in Crockford Base32. Two properties earn it the choice:
//!
//!   * **It sorts by creation time as a plain string**, so "the newest `cc-1`"
//!     — the resolution a phone that only knows the legacy name needs — is a
//!     string comparison rather than a join against `created_at`.
//!   * **It is one case-insensitive token with no separators**, so it survives
//!     being a filename, a tmux argument and a JSON key without quoting.
//!
//! 80 bits of entropy per millisecond is the collision argument: two sessions
//! minted in the same millisecond collide with probability 2^-80. Not colliding
//! is not the same as being ORDERED, though, and the resolution above needs both
//! — so [`new`] mints monotonically within a process, by the ULID
//! specification's own monotonic factory. See its doc comment.
//!
//! Implemented here rather than pulled in as a crate: the whole of it is the
//! forty lines below, it needs no randomness source beyond the one the daemon
//! already uses for tokens, and a dependency for this would arrive with its own
//! RNG stack.

/// Crockford Base32. Excludes `I`, `L`, `O` and `U` so a uid read aloud or
/// re-typed from a log cannot become a different, equally valid uid.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 26 Base32 symbols carry 130 bits; a ULID is 128, so the leading symbol holds
/// only 3 bits and can never exceed `7`.
pub const UID_LEN: usize = 26;

/// The largest value the 80 entropy bits can hold; a tail already at it cannot be
/// stepped without carrying into the timestamp.
const MAX_TAIL: u128 = (1u128 << 80) - 1;

/// The largest millisecond the 48-bit timestamp can hold. There is no millisecond
/// after it to carry into.
const MAX_MS: i64 = (1i64 << 48) - 1;

/// The millisecond this process last minted for, and the tail it used.
///
/// See [`new`] for why this exists. `None` until the first mint.
static LAST_MINT: std::sync::Mutex<Option<(i64, u128)>> = std::sync::Mutex::new(None);

/// Mint a new uid from the current time and kernel entropy, strictly greater than
/// every uid this process has already minted.
///
/// ## Why the mint is ordered and not merely unique
///
/// "The newest session named `cc-1`" is answered in three places — demo-daemon's
/// `Fleet::resolve`, ccd's `state.rs` and the `codeconnect` CLI — by taking the
/// `max` of the candidates' uids. Uniqueness is not enough for that question. Two
/// uids minted in the SAME millisecond differ only in their 80 random bits, so a
/// successor minted in the millisecond its predecessor died had an even chance of
/// sorting BELOW the corpse it replaced, and `max` then named the dead run. That
/// is not theoretical: demo-daemon's
/// `two_runs_retiring_in_one_tick_over_corpses_each_end_their_own_log` failed 5
/// times in 100 runs on the unordered mint.
///
/// The fix is the ULID specification's own monotonic factory. Within one process a
/// uid minted for a millisecond that is not strictly later than the previous
/// mint's reuses that millisecond and the previous tail **plus one**, so it sorts
/// immediately after its predecessor. Fresh entropy is drawn only when the clock
/// has genuinely moved on.
///
/// The `<=` rather than `==` is deliberate: a clock that steps BACKWARDS (an NTP
/// correction between two spawns) is the other way a successor could sort below
/// its predecessor, and it is covered by the same rule. The cost is that while the
/// clock is behind, [`timestamp_ms`] reports the last millisecond this process saw
/// rather than the corrected one — a lie bounded by the size of the step, and a
/// smaller one than naming a dead session as the live one.
///
/// **Cross-process ties are NOT covered and remain possible.** Two `codeconnect`
/// launchers in one millisecond each hold their own `LAST_MINT`, so their uids
/// still separate on 80 random bits alone. And that is the production shape, not
/// a corner of it: a successor session is launched by a NEW process, so the very
/// tie this factory closes within a process is still decided by the random tail
/// between two of them — practically unreachable at 2^-80 per millisecond, but
/// not impossible, and only a shared minting authority would close it. That is
/// not built, and nothing here claims to solve it.
pub fn new() -> std::io::Result<String> {
    let now = clamp_ms(crate::time::now_unix_ms());
    // A poisoned lock still holds a valid `(ms, tail)`: the panic that poisoned it
    // cannot have come from inside this block, which only does arithmetic. Refusing
    // to mint a uid for the rest of the process's life would be the worse failure.
    let mut last = LAST_MINT.lock().unwrap_or_else(|e| e.into_inner());
    // On the error path `last` is left exactly as it was — the `?` returns before
    // the store, so a refusal never advances the state.
    let (ms, tail) = step(*last, now, random_tail)?;
    *last = Some((ms, tail));
    Ok(encode(((ms as u128) << 80) | tail))
}

/// The monotonic decision itself: given what this process last minted and what the
/// clock says now, the `(ms, tail)` the next uid must carry.
///
/// Split out from [`new`] — which is now the lock, this, and the store — because
/// the ceiling below is otherwise untestable. Seeding the real `LAST_MINT` would
/// make every other test's `new()` fail for as long as the seed stood, and minting
/// 2^80 uids to reach the ceiling honestly is not a test. `fresh` is the entropy
/// source, injected for the same reason.
fn step(
    previous: Option<(i64, u128)>,
    now: i64,
    fresh: impl FnOnce() -> std::io::Result<u128>,
) -> std::io::Result<(i64, u128)> {
    let Some((previous_ms, previous_tail)) = previous else {
        return Ok((now, fresh()?));
    };
    if now > previous_ms {
        return Ok((now, fresh()?));
    }
    if previous_tail < MAX_TAIL {
        return Ok((previous_ms, previous_tail + 1));
    }
    if previous_ms >= MAX_MS {
        // The representational ceiling, where there is no next millisecond to carry
        // into. Clamping back to `previous_ms` with a fresh tail — which is what
        // this did — mints a uid that sorts at or BELOW its predecessor, which is
        // the one failure this whole factory exists to prevent. Refusing is the
        // only answer left that keeps the guarantee `new` makes in its own doc.
        return Err(std::io::Error::other(
            "uid space exhausted: the 48-bit timestamp is at its ceiling and the \
             80-bit tail is fully stepped",
        ));
    }
    // 2^80 mints inside one millisecond. Unreachable, but the only other answer
    // here is a tie, so borrow the next millisecond instead.
    Ok((previous_ms + 1, fresh()?))
}

/// 80 fresh bits from the source the daemon already uses for tokens.
fn random_tail() -> std::io::Result<u128> {
    let mut tail: u128 = 0;
    for byte in crate::secret::random_bytes::<10>()? {
        tail = (tail << 8) | byte as u128;
    }
    Ok(tail)
}

/// Mint a uid that sorts as though it were created at `unix_ms`.
///
/// Used when an upgraded daemon synthesises identities for sessions that
/// predate them, so the migrated rows keep their real ordering instead of all
/// appearing to have been created at migration time.
///
/// Deliberately NOT monotonic, unlike [`new`]: this is a pure function of its
/// argument, and a caller migrating historical rows asks for the millisecond it
/// names, not for the one this process last happened to mint in.
pub fn at(unix_ms: i64) -> std::io::Result<String> {
    Ok(encode(compose(
        unix_ms,
        crate::secret::random_bytes::<10>()?,
    )))
}

/// Is this a syntactically valid uid? Accepts either case; [`new`] always emits
/// upper case.
pub fn is_well_formed(value: &str) -> bool {
    value.len() == UID_LEN
        && value
            .bytes()
            .all(|byte| symbol_value(byte).is_some())
        // The leading symbol carries 3 bits. Anything above 7 is 129 bits of
        // "ULID" and decodes to a different value than it displays.
        && symbol_value(value.as_bytes()[0]).is_some_and(|v| v < 8)
}

/// The millisecond the uid was minted, or `None` if it is not a uid.
pub fn timestamp_ms(value: &str) -> Option<i64> {
    decode(value).map(|bits| (bits >> 80) as i64)
}

/// A negative or absurd clock must not silently wrap into another session's
/// range; clamping keeps the value inside the 48 bits the format allows.
fn clamp_ms(unix_ms: i64) -> i64 {
    unix_ms.clamp(0, MAX_MS)
}

fn compose(unix_ms: i64, entropy: [u8; 10]) -> u128 {
    let ms = clamp_ms(unix_ms) as u128;
    let mut bits = ms << 80;
    for (index, byte) in entropy.iter().enumerate() {
        bits |= (*byte as u128) << (72 - index * 8);
    }
    bits
}

fn encode(bits: u128) -> String {
    let mut out = String::with_capacity(UID_LEN);
    for position in (0..UID_LEN).rev() {
        let shift = position * 5;
        let index = ((bits >> shift) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}

fn decode(value: &str) -> Option<u128> {
    if value.len() != UID_LEN {
        return None;
    }
    let mut bits: u128 = 0;
    for byte in value.bytes() {
        bits = bits
            .checked_mul(32)?
            .checked_add(symbol_value(byte)? as u128)?;
    }
    Some(bits)
}

fn symbol_value(byte: u8) -> Option<u8> {
    let upper = byte.to_ascii_uppercase();
    ALPHABET
        .iter()
        .position(|symbol| *symbol == upper)
        .map(|index| index as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_minted_uid_is_well_formed() {
        let uid = new().unwrap();
        assert_eq!(uid.len(), UID_LEN);
        assert!(is_well_formed(&uid), "{uid}");
        assert!(uid
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn uids_are_unique_across_a_burst() {
        // A tight loop mints many uids inside one millisecond. This used to claim it
        // exercised the ENTROPY half, and that claim is now false: since `new` became
        // monotonic a same-millisecond mint steps the tail rather than drawing one,
        // so what this measures is uniqueness — which the step gives just as surely,
        // and which is still worth pinning. The entropy claim moved to
        // `a_mint_in_a_fresh_millisecond_draws_real_entropy`, which can actually
        // fail if the draw stops being random.
        let mut seen = HashSet::new();
        for _ in 0..2000 {
            assert!(seen.insert(new().unwrap()), "uids must never repeat");
        }
    }

    #[test]
    fn a_mint_in_a_fresh_millisecond_draws_real_entropy() {
        // Entropy is what separates two PROCESSES minting in the same millisecond —
        // the tie `new`'s counter cannot reach, and the reason `random_tail` still
        // has to be random. From inside one process the only place a draw is
        // observable is across a millisecond boundary, so this crosses several.
        //
        // Asserted as MAGNITUDE, not distinctness. A `random_tail` stuck at zero
        // would still hand out distinct uids — the step makes them 0, 1, 2 — and
        // would sail through a "no duplicates" check. It cannot produce a tail up
        // near 2^80. A real 80-bit draw clears 2^64 with probability 1 - 2^-16, so
        // over six samples this is decisive either way.
        let mut tails = Vec::new();
        while tails.len() < 6 {
            let uid = new().unwrap();
            tails.push(decode(&uid).unwrap() & MAX_TAIL);
            // Past the millisecond, so the next mint draws instead of stepping.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            tails.iter().any(|tail| *tail > (1u128 << 64)),
            "not one of {tails:?} looks like an 80-bit draw; the entropy source is \
             not random"
        );
    }

    #[test]
    fn the_representational_ceiling_refuses_rather_than_minting_a_uid_that_sorts_below() {
        // The one input on which "step the tail" has no answer. At `MAX_MS` with the
        // tail exhausted there is no next millisecond to carry into, and the branch
        // used to clamp back to `MAX_MS` and draw a FRESH tail — which sorts below
        // its predecessor whenever that draw comes in under `MAX_TAIL`, i.e. always.
        // A refusal is the only outcome that does not break `new`'s guarantee.
        let err = step(Some((MAX_MS, MAX_TAIL)), MAX_MS, || Ok(0))
            .expect_err("the ceiling has no uid left to give");
        assert!(
            err.to_string().contains("uid space exhausted"),
            "the refusal must say why: {err}"
        );

        // One millisecond below the ceiling still carries forward rather than
        // refusing, so the refusal is the ceiling's and not the carry's.
        assert_eq!(
            step(Some((MAX_MS - 1, MAX_TAIL)), MAX_MS - 1, || Ok(7)).unwrap(),
            (MAX_MS, 7)
        );

        // The three ordinary paths, pinned in the same place so the shape of the
        // decision is readable in one test rather than inferred from a burst.
        assert_eq!(step(None, 5, || Ok(9)).unwrap(), (5, 9), "first mint draws");
        assert_eq!(
            step(Some((5, 9)), 5, || Ok(0)).unwrap(),
            (5, 10),
            "the same millisecond steps the tail"
        );
        assert_eq!(
            step(Some((5, 9)), 3, || Ok(0)).unwrap(),
            (5, 10),
            "a backwards clock steps the tail too, rather than sorting below"
        );
        assert_eq!(
            step(Some((5, 9)), 6, || Ok(4)).unwrap(),
            (6, 4),
            "a later millisecond draws fresh entropy"
        );
    }

    #[test]
    fn uids_minted_in_one_millisecond_strictly_increase() {
        // The session-id TIE. "The newest session named `cc-1`" is resolved in three
        // places by `max_by(session_uid)`, so a successor minted in the SAME
        // millisecond as the corpse it replaces wins or loses on 80 random bits — a
        // coin flip that made demo-daemon's
        // `two_runs_retiring_in_one_tick_over_corpses_each_end_their_own_log` fail
        // 5 times in 100 runs. A burst inside one process must be strictly ordered.
        let burst: Vec<String> = (0..2000).map(|_| new().unwrap()).collect();
        let mut ties = Vec::new();
        let mut within_one_ms = 0usize;
        for pair in burst.windows(2) {
            if pair[0] >= pair[1] {
                ties.push((pair[0].clone(), pair[1].clone()));
            }
            if timestamp_ms(&pair[0]) == timestamp_ms(&pair[1]) {
                within_one_ms += 1;
                // The monotonic rule, measured rather than merely implied: within a
                // millisecond a successor is the previous tail STEPPED, not freshly
                // drawn. The step is +1, but the bound asserted is deliberately
                // loose. `LAST_MINT` is process-wide, which is the point of it, so
                // a sibling test's thread minting between two of this thread's
                // mints takes steps this thread cannot see — adjacency is not
                // this burst's to claim. The whole binary mints a few thousand
                // uids, so a million is slack no interleaving can use, and it
                // still separates the two designs completely: a fresh 80-bit draw
                // lands within a million of its predecessor with probability about
                // 2^-60.
                //
                // `checked_sub` so a successor that sorts BELOW its predecessor
                // reports that, rather than panicking on the subtraction.
                let delta = decode(&pair[1])
                    .unwrap()
                    .checked_sub(decode(&pair[0]).unwrap());
                assert!(
                    delta.is_some_and(|d| d <= (1 << 20)),
                    "{} then {} share a millisecond but are {delta:?} apart — that is \
                     a fresh random tail, not a stepped one",
                    pair[0],
                    pair[1]
                );
            }
        }
        assert!(
            ties.is_empty(),
            "{} of 1999 successors did not sort after their predecessor, e.g. {:?}",
            ties.len(),
            &ties[..ties.len().min(3)]
        );
        // Without this the test could pass on a machine that gave every mint its
        // own millisecond, having measured nothing.
        assert!(
            within_one_ms > 100,
            "only {within_one_ms} of 1999 adjacent pairs shared a millisecond; this \
             burst never exercised the tie"
        );
    }

    #[test]
    fn uids_sort_by_creation_time_as_plain_strings() {
        let early = at(1_600_000_000_000).unwrap();
        let late = at(1_700_000_000_000).unwrap();
        assert!(early < late, "{early} should sort before {late}");
        // Within one millisecond `at` gives a total order but an ARBITRARY one —
        // and that is not enough for "newest session with this name", which is
        // why `new` is monotonic and `at` is not. See
        // `uids_minted_in_one_millisecond_strictly_increase`.
        let a = at(1_700_000_000_000).unwrap();
        let b = at(1_700_000_000_000).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_timestamp_survives_the_round_trip() {
        for ms in [0i64, 1, 1_700_000_000_000, (1i64 << 48) - 1] {
            let uid = at(ms).unwrap();
            assert_eq!(timestamp_ms(&uid), Some(ms), "{uid}");
        }
    }

    #[test]
    fn an_impossible_clock_is_clamped_rather_than_wrapped() {
        // A machine with a broken clock must not mint a uid that sorts into
        // another session's range or overflows into the entropy bits.
        let negative = at(-1).unwrap();
        assert_eq!(timestamp_ms(&negative), Some(0));
        let far_future = at(i64::MAX).unwrap();
        assert_eq!(timestamp_ms(&far_future), Some((1i64 << 48) - 1));
        assert!(is_well_formed(&far_future));
    }

    #[test]
    fn malformed_uids_are_rejected() {
        for bad in [
            "",
            "cc-1",
            // One symbol short, one symbol long.
            "01ARZ3NDEKTSV4RRFFQ69G5FA",
            "01ARZ3NDEKTSV4RRFFQ69G5FAVX",
            // `I`, `L`, `O` and `U` are not in the alphabet.
            "01ARZ3NDEKTSV4RRFFQ69G5FIV",
            "01ARZ3NDEKTSV4RRFFQ69G5FOV",
            // 129 bits: displays as a uid, decodes as something else.
            "81ARZ3NDEKTSV4RRFFQ69G5FAV",
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZ",
        ] {
            assert!(!is_well_formed(bad), "{bad:?} must not pass as a uid");
        }
        // Overflowing the 128 bits is rejected by the decoder too, not only by
        // the shape check — so a caller that skips validation still cannot get a
        // wrong timestamp out of a wrong uid.
        assert_eq!(timestamp_ms("ZZZZZZZZZZZZZZZZZZZZZZZZZZ"), None);
        assert_eq!(timestamp_ms("cc-1"), None);
    }

    #[test]
    fn decoding_accepts_either_case() {
        let uid = new().unwrap();
        assert!(is_well_formed(&uid.to_lowercase()));
        assert_eq!(
            timestamp_ms(&uid.to_lowercase()),
            timestamp_ms(&uid),
            "case must not change the value"
        );
    }

    #[test]
    fn the_encoding_is_the_documented_crockford_one() {
        // Pinned against the ULID specification's own example, so a rewrite of
        // the bit-twiddling cannot silently change the format.
        assert_eq!(encode(0), "00000000000000000000000000");
        assert_eq!(encode(u128::MAX), "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
        assert_eq!(decode("7ZZZZZZZZZZZZZZZZZZZZZZZZZ"), Some(u128::MAX));
        assert_eq!(
            decode(&encode(12345678901234567890)),
            Some(12345678901234567890)
        );
        assert!(!ALPHABET.contains(&b'I'));
        assert!(!ALPHABET.contains(&b'L'));
        assert!(!ALPHABET.contains(&b'O'));
        assert!(!ALPHABET.contains(&b'U'));
    }
}

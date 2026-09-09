//! The composite wire id for a Codex approval.
//!
//! A Codex `serverRequest` id is a per-thread small integer that restarts from
//! zero and is shared across request families (A2). It cannot be the phone's
//! `request_id` directly: two threads, two sessions resuming one thread, and the
//! same thread revisited (A→B→A) all reuse the same small integers, and the
//! phone correlates answers by that string alone
//! (`DaemonConnection.swift:784`). So the wire id is a composite of everything
//! that makes an activation unique — `(session_uid, thread_id,
//! server_request_id, generation)` — encoded opaquely.
//!
//! Two properties are load-bearing and are what the property tests pin:
//!   * **Type-tagged.** A JSON-RPC id may be a number *or* a string, and the
//!     number `5` and the string `"5"` are different ids upstream. The kind is a
//!     byte in the material, so their composite ids differ — a numeric id can
//!     never collide with the string that prints the same.
//!   * **Length-bounded, unambiguous.** Fields are length-prefixed (never
//!     delimiter-joined — the ambiguity `hash::send_text_hash` calls out), the
//!     inputs are bounded, and both encode and decode refuse anything longer,
//!     so a hostile id cannot balloon a frame or smuggle trailing bytes.
//!
//! The encoding is versioned; a future layout is a new version byte, and this
//! decoder refuses one it does not know rather than misreading it.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

/// Layout version. A new binary layout bumps this; [`decode`] refuses any other.
const VERSION: u8 = 1;

const KIND_NUMBER: u8 = 0;
const KIND_TEXT: u8 = 1;

/// Bounds on the variable-length inputs. Generous for real values (a ULID uid is
/// 26 bytes, a Codex thread id well under 128) but finite, so the encoded id has
/// a fixed ceiling and neither side has to defend against an unbounded one.
pub const MAX_SESSION_UID_LEN: usize = 64;
pub const MAX_THREAD_ID_LEN: usize = 128;
pub const MAX_TEXT_REQUEST_ID_LEN: usize = 128;

/// Ceiling on the decoded material, and thus (via base64url) on the wire string.
/// = version + kind + two u16 length prefixes + the two bounded strings + the
/// largest request-id encoding (text: u16 len + bytes) + u64 generation.
const MAX_MATERIAL_LEN: usize =
    1 + 1 + 2 + MAX_SESSION_UID_LEN + 2 + MAX_THREAD_ID_LEN + 2 + MAX_TEXT_REQUEST_ID_LEN + 8;

/// A JSON-RPC request id: a number or a string, kept distinct on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerRequestId {
    /// A JSON-RPC numeric id (the shape Codex 0.147 actually uses).
    Number(i64),
    /// A JSON-RPC string id. Distinct from the number that prints the same.
    Text(String),
}

/// Everything that makes one approval activation unique on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeId {
    pub session_uid: String,
    pub thread_id: String,
    pub server_request_id: ServerRequestId,
    /// The **visit** generation (D4) — not the thread. An A→B→A revisit reuses
    /// the thread id but not the generation, so a stale gen-1 answer cannot pose
    /// as gen-3 traffic.
    pub generation: u64,
}

/// Why an id could not be built or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositeIdError {
    /// An input field exceeded its bound.
    TooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    /// The base64url text was not valid.
    BadBase64,
    /// The material was structurally wrong: bad version, unknown kind, a length
    /// prefix past the end, or trailing bytes.
    Malformed,
}

impl std::fmt::Display for CompositeIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompositeIdError::TooLong { field, len, max } => {
                write!(
                    f,
                    "composite id field {field} is {len} bytes, over the {max} bound"
                )
            }
            CompositeIdError::BadBase64 => write!(f, "composite id is not valid base64url"),
            CompositeIdError::Malformed => write!(f, "composite id is malformed"),
        }
    }
}

impl std::error::Error for CompositeIdError {}

impl CompositeId {
    /// Encode to the opaque base64url wire string, or refuse an over-long input.
    pub fn encode(&self) -> Result<String, CompositeIdError> {
        check_len("session_uid", self.session_uid.len(), MAX_SESSION_UID_LEN)?;
        check_len("thread_id", self.thread_id.len(), MAX_THREAD_ID_LEN)?;

        let mut material = Vec::with_capacity(MAX_MATERIAL_LEN);
        material.push(VERSION);
        put_bytes(&mut material, self.session_uid.as_bytes());
        put_bytes(&mut material, self.thread_id.as_bytes());
        match &self.server_request_id {
            ServerRequestId::Number(n) => {
                material.push(KIND_NUMBER);
                material.extend_from_slice(&n.to_be_bytes());
            }
            ServerRequestId::Text(s) => {
                check_len("server_request_id", s.len(), MAX_TEXT_REQUEST_ID_LEN)?;
                material.push(KIND_TEXT);
                put_bytes(&mut material, s.as_bytes());
            }
        }
        material.extend_from_slice(&self.generation.to_be_bytes());

        Ok(URL_SAFE_NO_PAD.encode(&material))
    }

    /// Read an id back, refusing anything malformed or over-long rather than
    /// guessing at a partial one.
    pub fn decode(wire: &str) -> Result<CompositeId, CompositeIdError> {
        // base64url of the bounded material cannot exceed this; reject before
        // allocating for a hostile string.
        let max_wire = URL_SAFE_NO_PAD.encode(vec![0u8; MAX_MATERIAL_LEN]).len();
        if wire.len() > max_wire {
            return Err(CompositeIdError::Malformed);
        }
        let material = URL_SAFE_NO_PAD
            .decode(wire.as_bytes())
            .map_err(|_| CompositeIdError::BadBase64)?;

        let mut cur = Cursor::new(&material);
        if cur.u8()? != VERSION {
            return Err(CompositeIdError::Malformed);
        }
        let session_uid = cur.string(MAX_SESSION_UID_LEN)?;
        let thread_id = cur.string(MAX_THREAD_ID_LEN)?;
        let server_request_id = match cur.u8()? {
            KIND_NUMBER => ServerRequestId::Number(i64::from_be_bytes(cur.array8()?)),
            KIND_TEXT => ServerRequestId::Text(cur.string(MAX_TEXT_REQUEST_ID_LEN)?),
            _ => return Err(CompositeIdError::Malformed),
        };
        let generation = u64::from_be_bytes(cur.array8()?);
        if !cur.at_end() {
            return Err(CompositeIdError::Malformed);
        }
        Ok(CompositeId {
            session_uid,
            thread_id,
            server_request_id,
            generation,
        })
    }
}

fn check_len(field: &'static str, len: usize, max: usize) -> Result<(), CompositeIdError> {
    if len > max {
        Err(CompositeIdError::TooLong { field, len, max })
    } else {
        Ok(())
    }
}

/// Length-prefixed (u16 BE) — never a separator, so a value containing the
/// separator can never impersonate a different split.
fn put_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(bytes);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { bytes, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, CompositeIdError> {
        let byte = *self
            .bytes
            .get(self.pos)
            .ok_or(CompositeIdError::Malformed)?;
        self.pos += 1;
        Ok(byte)
    }

    fn array8(&mut self) -> Result<[u8; 8], CompositeIdError> {
        let end = self.pos.checked_add(8).ok_or(CompositeIdError::Malformed)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(CompositeIdError::Malformed)?;
        let mut out = [0u8; 8];
        out.copy_from_slice(slice);
        self.pos = end;
        Ok(out)
    }

    fn string(&mut self, max: usize) -> Result<String, CompositeIdError> {
        let hi = *self
            .bytes
            .get(self.pos)
            .ok_or(CompositeIdError::Malformed)?;
        let lo = *self
            .bytes
            .get(self.pos + 1)
            .ok_or(CompositeIdError::Malformed)?;
        let len = u16::from_be_bytes([hi, lo]) as usize;
        if len > max {
            return Err(CompositeIdError::Malformed);
        }
        let start = self.pos + 2;
        let end = start.checked_add(len).ok_or(CompositeIdError::Malformed)?;
        let slice = self
            .bytes
            .get(start..end)
            .ok_or(CompositeIdError::Malformed)?;
        let text = std::str::from_utf8(slice)
            .map_err(|_| CompositeIdError::Malformed)?
            .to_string();
        self.pos = end;
        Ok(text)
    }

    fn at_end(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(srid: ServerRequestId, generation: u64) -> CompositeId {
        CompositeId {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            thread_id: "th_abc123".into(),
            server_request_id: srid,
            generation,
        }
    }

    /// **The cross-language vector, pinned on this side.** These exact strings
    /// are the ones the Swift opaque-correlation test (`CompositeIdOpaqueTests`)
    /// checks in `fixtures/codex/composite_ids.json`: a gen-1 and a gen-3
    /// activation of one `(session_uid, thread_id, server_request_id)` are
    /// **different opaque strings**, and Swift correlates by exact-string
    /// equality without parsing either. If this literal changes, the fixture and
    /// the Swift test must change with it, and that is the point of pinning it.
    #[test]
    fn the_cross_generation_wire_vector_is_stable() {
        let base = |gen| CompositeId {
            session_uid: "01K1B3XQ8ZC0DE5FGH7JKMNPQR".into(),
            thread_id: "th_A".into(),
            server_request_id: ServerRequestId::Number(0),
            generation: gen,
        };
        let gen1 = base(1).encode().unwrap();
        let gen3 = base(3).encode().unwrap();
        assert_eq!(
            gen1,
            "AQAaMDFLMUIzWFE4WkMwREU1RkdIN0pLTU5QUVIABHRoX0EAAAAAAAAAAAAAAAAAAAAAAQ"
        );
        assert_eq!(
            gen3,
            "AQAaMDFLMUIzWFE4WkMwREU1RkdIN0pLTU5QUVIABHRoX0EAAAAAAAAAAAAAAAAAAAAAAw"
        );
        assert_ne!(gen1, gen3, "different visits are different opaque ids");
        // And each decodes back to exactly its generation — the id is a codec,
        // not a coincidence of prefixes.
        assert_eq!(CompositeId::decode(&gen1).unwrap(), base(1));
        assert_eq!(CompositeId::decode(&gen3).unwrap(), base(3));
    }

    #[test]
    fn round_trips() {
        for id in [
            sample(ServerRequestId::Number(0), 1),
            sample(ServerRequestId::Number(-1), 3),
            sample(ServerRequestId::Number(i64::MAX), u64::MAX),
            sample(ServerRequestId::Text("req-5".into()), 7),
            sample(ServerRequestId::Text(String::new()), 0),
        ] {
            let wire = id.encode().expect("encodes");
            assert_eq!(CompositeId::decode(&wire).expect("decodes"), id);
        }
    }

    #[test]
    fn a_numeric_id_never_collides_with_the_string_that_prints_the_same() {
        // The type-tag property: 5 and "5" are different ids upstream, so their
        // composite ids must differ. This is the whole reason the kind is in the
        // material rather than inferred from the digits.
        let as_number = sample(ServerRequestId::Number(5), 2).encode().unwrap();
        let as_text = sample(ServerRequestId::Text("5".into()), 2)
            .encode()
            .unwrap();
        assert_ne!(as_number, as_text);
        assert_eq!(
            CompositeId::decode(&as_number).unwrap().server_request_id,
            ServerRequestId::Number(5)
        );
        assert_eq!(
            CompositeId::decode(&as_text).unwrap().server_request_id,
            ServerRequestId::Text("5".into())
        );
    }

    #[test]
    fn a_generation_or_thread_change_is_a_different_id() {
        // A→B→A: same thread and upstream id, different generation ⇒ different
        // wire id, so a stale gen-1 answer cannot be replayed at gen-3.
        let g1 = sample(ServerRequestId::Number(0), 1).encode().unwrap();
        let g3 = sample(ServerRequestId::Number(0), 3).encode().unwrap();
        assert_ne!(g1, g3);
        let other_thread = CompositeId {
            thread_id: "th_other".into(),
            ..sample(ServerRequestId::Number(0), 1)
        }
        .encode()
        .unwrap();
        assert_ne!(g1, other_thread);
    }

    #[test]
    fn over_long_inputs_are_refused_on_encode() {
        let long_uid = CompositeId {
            session_uid: "x".repeat(MAX_SESSION_UID_LEN + 1),
            ..sample(ServerRequestId::Number(0), 1)
        };
        assert!(matches!(
            long_uid.encode(),
            Err(CompositeIdError::TooLong {
                field: "session_uid",
                ..
            })
        ));
        let long_text = sample(
            ServerRequestId::Text("x".repeat(MAX_TEXT_REQUEST_ID_LEN + 1)),
            1,
        );
        assert!(matches!(
            long_text.encode(),
            Err(CompositeIdError::TooLong {
                field: "server_request_id",
                ..
            })
        ));
    }

    #[test]
    fn a_malformed_wire_string_is_refused_not_guessed() {
        assert_eq!(
            CompositeId::decode("!!!not base64!!!"),
            Err(CompositeIdError::BadBase64)
        );
        // Truncated material (valid base64url of one byte) is structurally short.
        let one_byte = URL_SAFE_NO_PAD.encode([VERSION]);
        assert_eq!(
            CompositeId::decode(&one_byte),
            Err(CompositeIdError::Malformed)
        );
        // A wrong version byte is refused rather than misread.
        let wrong_version = URL_SAFE_NO_PAD.encode([9u8, 0, 0]);
        assert_eq!(
            CompositeId::decode(&wrong_version),
            Err(CompositeIdError::Malformed)
        );
        // Trailing garbage after a valid id is refused (no partial acceptance).
        let good = sample(ServerRequestId::Number(1), 1).encode().unwrap();
        let mut material = URL_SAFE_NO_PAD.decode(good.as_bytes()).unwrap();
        material.push(0xff);
        let with_trailer = URL_SAFE_NO_PAD.encode(&material);
        assert_eq!(
            CompositeId::decode(&with_trailer),
            Err(CompositeIdError::Malformed)
        );
    }

    #[test]
    fn the_encoded_id_stays_under_its_ceiling() {
        // Length bound as a property: every value the codec accepts stays under
        // the fixed wire ceiling, so it can never balloon a frame.
        let max_wire = URL_SAFE_NO_PAD.encode(vec![0u8; MAX_MATERIAL_LEN]).len();
        let biggest = CompositeId {
            session_uid: "s".repeat(MAX_SESSION_UID_LEN),
            thread_id: "t".repeat(MAX_THREAD_ID_LEN),
            server_request_id: ServerRequestId::Text("r".repeat(MAX_TEXT_REQUEST_ID_LEN)),
            generation: u64::MAX,
        };
        let wire = biggest.encode().unwrap();
        assert!(wire.len() <= max_wire, "{} > {max_wire}", wire.len());
        assert_eq!(CompositeId::decode(&wire).unwrap(), biggest);
    }

    /// A deterministic sweep — a property test without a new dev-dependency.
    /// Round-trip must hold, and a numeric id must never encode to the same
    /// string as its textual twin, across a wide spread of generated values.
    #[test]
    fn property_sweep_round_trip_and_no_type_collision() {
        // A small xorshift so the cases are varied but reproducible.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let uid_len = (next() as usize) % (MAX_SESSION_UID_LEN + 1);
            let thread_len = (next() as usize) % (MAX_THREAD_ID_LEN + 1);
            let gen = next();
            let n = next() as i64;
            let session_uid: String = (0..uid_len)
                .map(|i| char::from(b'a' + (i as u8 % 26)))
                .collect();
            let thread_id: String = (0..thread_len)
                .map(|i| char::from(b'0' + (i as u8 % 10)))
                .collect();

            let numeric = CompositeId {
                session_uid: session_uid.clone(),
                thread_id: thread_id.clone(),
                server_request_id: ServerRequestId::Number(n),
                generation: gen,
            };
            let wire = numeric.encode().unwrap();
            assert_eq!(CompositeId::decode(&wire).unwrap(), numeric);

            // Its textual twin (the decimal string of n) must not collide.
            let textual = CompositeId {
                server_request_id: ServerRequestId::Text(n.to_string()),
                ..numeric.clone()
            };
            assert_ne!(textual.encode().unwrap(), wire);
        }
    }
}

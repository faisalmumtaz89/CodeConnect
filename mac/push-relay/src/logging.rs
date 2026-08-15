//! Logs as documents with a fixed set of fields, because the promise about them
//! is a promise of absence.
//!
//! "No authorization header, no raw device token, no bearer, no attestation
//! object and no client address ever appears in a log line" cannot be checked by
//! reading prose after the fact — it has to be true of the shape request logging
//! takes. [`RequestLog`] is that shape: four typed fields, none of them free
//! text, and no way to attach a fifth. The startup and lifecycle lines the rest
//! of the service writes are free-text documents about the process, and they
//! follow the same rule by convention rather than by construction.
//!
//! A binding still has to be identifiable across two lines, so one field carries
//! an identifier — the first eight characters of a **hash**, which is enough to
//! follow one caller through an incident and not enough to be anything on its
//! own. [`binding_id`] refuses to shorten anything that is not a digest, so a
//! call site that reaches for a raw value gets a redaction rather than a
//! truncated secret.

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

/// Eight hex characters — 32 bits. Two bindings colliding is possible and does
/// not matter: this identifies a line, and the database identifies a binding.
const ID_LEN: usize = 8;

/// What a field prints when the value offered was not a digest.
const UNIDENTIFIED: &str = "unidentified";

/// What a field prints when there was no value at all, so the two cases are
/// distinguishable in a search.
const NONE: &str = "none";

/// The level everything is at when nothing was asked for.
const DEFAULT: &str = "info";

/// The libraries that carry the secrets in their own log lines.
///
/// The APNs request is `POST https://api.push.apple.com/3/device/<raw device
/// token>` with `authorization: bearer <provider JWT>`, and these crates print
/// request targets and HTTP/2 frames — pseudo-headers included — verbatim. A
/// promise of absence that an environment variable can switch off is not a
/// promise, and `RUST_LOG=trace` is the first thing anyone sets during an
/// incident, so a directive naming one of these is **dropped** rather than
/// honoured. That is why there is no passthrough here to restore.
const CLAMPED: &[&str] = &["h2", "hyper", "hyper_util", "rustls", "tower"];

/// Where those targets are held. A connection that fails still says so; a
/// connection that succeeds does not recite what it sent.
const CLAMPED_LEVEL: &str = "warn";

/// Install the JSON subscriber. Called once, at startup.
///
/// JSON rather than lines because these logs are read by a log service and
/// searched for the *absence* of a value; a grep over prose cannot prove that a
/// bearer never appeared, and a set of named fields can.
pub fn init() -> Result<()> {
    let requested = std::env::var(EnvFilter::DEFAULT_ENV).ok();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter(requested.as_deref()))
        .with_target(false)
        .try_init()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("installing the log subscriber")
}

/// What was asked for, with the clamped targets put back where they belong.
fn filter(requested: Option<&str>) -> EnvFilter {
    let asked = clamp(requested.unwrap_or(DEFAULT));
    // A directive nobody can parse falls back to the default, and the clamp is
    // part of both halves so neither road leads around it.
    EnvFilter::builder()
        .parse(asked)
        .unwrap_or_else(|_| EnvFilter::new(clamp(DEFAULT)))
}

fn clamp(requested: &str) -> String {
    let mut directives: Vec<&str> = requested
        .split(',')
        .map(str::trim)
        .filter(|directive| !directive.is_empty() && !names_a_clamped_target(directive))
        .collect();
    let floors: Vec<String> = CLAMPED
        .iter()
        .map(|target| format!("{target}={CLAMPED_LEVEL}"))
        .collect();
    directives.extend(floors.iter().map(String::as_str));
    directives.join(",")
}

/// Whether a directive is about a clamped crate — including a module inside
/// one, which is where the frame and request-line traces actually live.
fn names_a_clamped_target(directive: &str) -> bool {
    let target = directive
        .split('=')
        .next()
        .unwrap_or_default()
        .split('[')
        .next()
        .unwrap_or_default()
        .trim();
    CLAMPED.iter().any(|clamped| {
        target == *clamped
            || target
                .strip_prefix(clamped)
                .is_some_and(|r| r.starts_with("::"))
    })
}

/// The short identifier for a binding, derived from its hash.
///
/// **Refuses anything that is not a SHA-256 digest.** A bearer is base64url and
/// a raw device token is not 64 characters, so the wrong value offered here
/// prints as unidentified rather than as its own first eight characters — which
/// is the failure mode where a log looks redacted and is not.
pub fn binding_id(hash: &str) -> &str {
    let is_digest =
        hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if is_digest {
        &hash[..ID_LEN]
    } else {
        UNIDENTIFIED
    }
}

/// Everything one request is allowed to say about itself.
///
/// The fields are the whole vocabulary. There is no map, no `extra`, and no
/// `&dyn Display` a caller could hand a token to.
pub struct RequestLog<'a> {
    /// The route pattern, not the request target: a path carrying a token in it
    /// would put the token in the log.
    pub route: &'static str,
    pub status: u16,
    /// A word from a closed set the caller owns, describing what happened.
    pub outcome: &'static str,
    /// The **hash** of the bearer or token this line is about, when the line is
    /// about one. Shortened by [`binding_id`] on the way out.
    pub binding_hash: Option<&'a str>,
}

impl RequestLog<'_> {
    /// The value the `binding` field will carry — never the argument itself.
    fn binding(&self) -> &str {
        self.binding_hash.map_or(NONE, binding_id)
    }

    pub fn emit(&self) {
        tracing::info!(
            route = self.route,
            status = self.status,
            outcome = self.outcome,
            binding = self.binding(),
            "request"
        );
    }
}

/// Holds every callsite in the process enabled, for the tests that read logs
/// back.
///
/// `tracing` caches a callsite's interest the first time that callsite is
/// evaluated — once, for the whole process — and computes it from the
/// dispatchers that are live at that moment. A test captures by installing a
/// subscriber on **its own thread**, so a callsite that some other test's
/// thread reached first, with nothing installed anywhere, is cached as `never`;
/// from then on the event is dropped before any thread-local dispatcher is
/// consulted, and the capture comes back empty however correct the code under
/// it is.
///
/// One subscriber that is interested in everything and never goes out of scope
/// makes that unreachable. Interest is recomputed across every live dispatcher,
/// so with this one always among them a callsite resolves to `always` or to
/// `sometimes`, and `sometimes` asks the emitting thread's dispatcher — which is
/// the capturing one. Installing it rebuilds the cache too, so a callsite
/// already cached as `never` is repaired rather than inherited.
///
/// It writes nowhere. Each test still captures through its own thread-local
/// subscriber into its own buffer, which is why the tests need no lock between
/// them: a test can neither read another's lines nor add to them, so no
/// assertion about presence or absence can be satisfied by a neighbour.
#[cfg(test)]
pub(crate) fn enable_every_callsite() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let everything = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::set_global_default(everything)
            .expect("installing the test subscriber");
        // A callsite another thread was evaluating while this was being
        // installed can still have landed on `never`.
        tracing::callsite::rebuild_interest_cache();
    });
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture(f: impl FnOnce()) -> String {
        enable_every_callsite();
        let sink = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(sink.clone())
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let text = sink.text();
        assert!(!text.is_empty(), "nothing was logged at all");
        text
    }

    /// The same capture, through the filter the process would have installed.
    fn capture_through(requested: Option<&str>, f: impl FnOnce()) -> String {
        enable_every_callsite();
        let sink = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(sink.clone())
            .with_env_filter(filter(requested))
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let text = sink.text();
        assert!(!text.is_empty(), "nothing was logged at all");
        text
    }

    const DIGEST: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    /// **The promise is unconditional, so it cannot depend on a variable.** The
    /// device token is the APNs request target and the provider token is a
    /// header on it, which means the libraries that write those lines are the
    /// libraries that would leak both.
    #[test]
    fn a_device_token_in_a_request_target_does_not_survive_a_trace_filter() {
        let token = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let provider = "eyJhbGciOiJFUzI1NiIsImtpZCI6IktFWUlEIn0";
        let noisy = || {
            tracing::trace!(
                target: "hyper_util::client::legacy",
                uri = format!("https://api.push.apple.com/3/device/{token}"),
                "sending"
            );
            tracing::trace!(
                target: "h2::codec::framed_write",
                authorization = format!("bearer {provider}"),
                "HEADERS"
            );
            tracing::trace!(target: "rustls::client::hs", "handshake");
            tracing::info!(route = "/v1/push", "request");
        };

        // Every road an operator takes to a verbose log: the blanket level, the
        // crate named outright, and a module inside it. Each carries the blanket
        // level as well, because `tracing` caches whether a callsite is enabled
        // once for the whole process — a filter here that disabled everything
        // else would silence the tests running alongside this one.
        for requested in [
            "trace",
            "h2=trace,hyper=trace,trace",
            "h2::codec=trace,hyper_util::client::legacy=trace,trace",
        ] {
            let output = capture_through(Some(requested), noisy);
            assert!(
                !output.contains(token),
                "{requested:?} put a device token in the log: {output}"
            );
            assert!(
                !output.contains(provider),
                "{requested:?} put the provider token in the log: {output}"
            );
            assert!(!output.contains("bearer"), "{requested:?}: {output}");
        }

        // And the clamp is a clamp on those crates, not a gag on the relay.
        let output = capture_through(Some("trace"), noisy);
        assert!(output.contains(r#""route":"/v1/push""#), "{output}");
    }

    #[test]
    fn a_digest_shortens_and_anything_else_does_not() {
        assert_eq!(binding_id(DIGEST), "ba7816bf");
        // A bearer credential: base64url, and never eight characters of itself.
        assert_eq!(
            binding_id("aVeryReal-Bearer_Value_With32Bytes_OfEntropy"),
            UNIDENTIFIED
        );
        // Uppercase hex is not the form anything here produces, so it is not
        // trusted to be a hash either.
        assert_eq!(binding_id(&DIGEST.to_ascii_uppercase()), UNIDENTIFIED);
        assert_eq!(binding_id(""), UNIDENTIFIED);
        // A device token is hex, and the wrong length is what saves it here.
        assert_eq!(binding_id("aabbccddeeff00112233445566778899"), UNIDENTIFIED);
    }

    /// **The whole point.** What lands in the log for a line about a binding is
    /// eight characters of a digest — not the digest, and certainly not the
    /// bearer somebody passed by mistake.
    #[test]
    fn a_request_line_carries_a_short_hash_and_never_a_raw_value() {
        let bearer = "aVeryReal-Bearer_Value_With32Bytes_OfEntropy";
        let output = capture(|| {
            RequestLog {
                route: "/readyz",
                status: 200,
                outcome: "ready",
                binding_hash: Some(DIGEST),
            }
            .emit();
            RequestLog {
                route: "/readyz",
                status: 401,
                outcome: "refused",
                binding_hash: Some(bearer),
            }
            .emit();
            RequestLog {
                route: "/healthz",
                status: 200,
                outcome: "alive",
                binding_hash: None,
            }
            .emit();
        });

        assert!(output.contains(r#""binding":"ba7816bf""#), "{output}");
        assert!(
            !output.contains(DIGEST),
            "the whole digest is more than a line needs: {output}"
        );
        assert!(
            !output.contains(bearer),
            "a bearer reached the log: {output}"
        );
        assert!(output.contains(UNIDENTIFIED), "{output}");
        assert!(output.contains(r#""binding":"none""#), "{output}");
        assert!(output.contains(r#""route":"/readyz""#), "{output}");
        assert!(output.contains(r#""status":200"#), "{output}");
        // JSON, one document per line, so a log service can index the fields.
        for line in output.lines() {
            let parsed: serde_json::Value = serde_json::from_str(line).expect(line);
            let fields = parsed.get("fields").expect(line);
            let names: Vec<&str> = fields
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            for name in &names {
                assert!(
                    ["message", "route", "status", "outcome", "binding"].contains(name),
                    "an unexpected field reached the log: {name}"
                );
            }
        }
    }
}

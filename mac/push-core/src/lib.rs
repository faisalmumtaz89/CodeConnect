//! What APNs requires of a sender, with nothing in it about who is sending.
//!
//! Apple's rules are the same wherever a push originates: the ES256 provider
//! token and the two clocks Apple keeps on it, which host a device token is
//! valid at, the headers that decide whether a notification is stored for a
//! phone that is switched off, and what each refusal means for the device it
//! was about. Written once, those facts are asserted once; written twice they
//! drift, and the drift is invisible until a phone quietly stops ringing.
//!
//! Nothing here opens a connection, reads a database, or composes an alert
//! body. Those are the parts that differ between senders — and Apple has no
//! opinion about any of them — so a caller brings its own HTTP/2 client and its
//! own payload.

mod device;
mod outcome;
mod refusal;
mod request;
mod token;

pub use device::{normalize_device_token, MAX_TOKEN_HEX, MIN_TOKEN_HEX};
pub use outcome::{classify, parse_reason, ApnsOutcome};
pub use refusal::{is_terminal, refusal, terminal_refusal, DeviceGone};
pub use request::{request, ApnsEnvironment, COLLAPSE_ID, TEST_COLLAPSE_ID};
pub use token::{ApnsIdentity, ProviderToken};

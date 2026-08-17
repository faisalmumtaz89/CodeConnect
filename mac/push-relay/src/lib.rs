//! The CodeConnect push relay: the one machine in the system that is not the
//! user's.
//!
//! An App Store customer cannot be given the APNs provider key, so somebody has
//! to hold it. This service does — and it is built so that holding it buys an
//! attacker as little as possible.
//!
//! **What it can be told.** A schema number, a device token, an environment
//! word, one of four event kinds, and a count. That is the entire vocabulary.
//! There is no field for a project name, a session identifier, a command, a
//! path, a diff, or anything an agent wrote, and the parser refuses unknown
//! fields rather than dropping them — so a caller cannot send one and believe it
//! arrived, and a future contributor cannot add one without changing the type
//! and its golden tests. The privacy claim is not that the relay discards this
//! material; it is that the relay has no way to receive it.
//!
//! **What it keeps.** Hashes. The device token and the bearer credential are on
//! the wire for the length of one request and are never written down, so a
//! stolen copy of the database says who exists and cannot push to any of them.
//!
//! **What it decides.** Whether a bearer is bound to the token it claims,
//! whether the caller is inside its limits, and which Apple host the binding
//! belongs to. It decides nothing about the notification itself: which run is
//! blocked, whether the phone already knows, whether the moment has passed —
//! all of that stays on the user's Mac, where the session state actually is. The
//! relay composes the words from a closed set and forwards them.
//!
//! It runs as a single instance with SQLite on a persistent disk, which is not
//! an accident of scale: one writer is what makes "at most one live credential
//! per token" a database constraint rather than a hope.

pub mod api;
pub mod apns;
pub mod attest;
pub mod backup;
pub mod challenge;
pub mod config;
pub mod db;
pub mod dto;
pub mod enroll;
pub mod logging;
pub mod payload;
pub mod push;
pub mod ratelimit;
pub mod secret;

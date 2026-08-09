//! What this build actually is.
//!
//! A version number moves at releases and capability bumps, never on ordinary
//! commits — so between those moments it cannot answer "which code is this?",
//! and two different builds happily share one number. The commit and the dirty
//! flag baked in by `build.rs` can answer it, and that is what every binary
//! introduces itself with.
/// The identity `build.rs` embedded when this `protocol` crate was compiled.
/// An empty string means the build ran outside a usable git checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildIdentity {
    /// The first twelve characters of the commit, validated by `build.rs`
    /// against the full object id before it was baked in.
    pub short: Option<&'static str>,
    /// The ship-source tree had uncommitted changes when built.
    pub dirty: bool,
}

fn non_empty(value: &'static str) -> Option<&'static str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

pub fn installed() -> BuildIdentity {
    BuildIdentity {
        short: non_empty(env!("CODECONNECT_BUILD_COMMIT_SHORT")),
        dirty: env!("CODECONNECT_BUILD_DIRTY") == "1",
    }
}

/// `227f6d4e1791`, `227f6d4e1791-dirty`, or `build unknown`.
pub fn build_tag() -> String {
    tag_of(installed())
}

fn tag_of(identity: BuildIdentity) -> String {
    match identity.short {
        Some(short) if identity.dirty => format!("{short}-dirty"),
        Some(short) => short.to_string(),
        None => "build unknown".to_string(),
    }
}

/// `codeconnect 0.3.0 (227f6d4e1791)` — every shipped binary's one-line
/// introduction. The version says which release lineage; the tag says which
/// exact code.
pub fn version_line(binary: &str, version: &str) -> String {
    format!("{binary} {version} ({})", build_tag())
}

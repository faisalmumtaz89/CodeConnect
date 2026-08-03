//! Runtime configuration, loaded from `~/.codeconnect/config.json`.
//!
//! Every field has a working default and a malformed file is a warning, never a
//! startup failure: a daemon that refuses to start because of a stray comma
//! would take the whole product down for a cosmetic reason.
//!
//! The TUI-copy needles are configuration rather than constants on purpose.
//! Claude Code's prompt wording is the single most churn-prone thing CodeConnect
//! depends on, and a user must be able to fix a broken presence check without
//! waiting for a release.

use serde::{Deserialize, Serialize};

fn default_ws_port() -> u16 {
    8787
}
fn default_gate_hook() -> String {
    "PermissionRequest".to_string()
}
fn default_gate_timeout_ms() -> u64 {
    120_000
}
fn default_connect_timeout_ms() -> u64 {
    200
}
fn default_tail_poll_ms() -> u64 {
    250
}
fn default_stale_after_ms() -> u64 {
    30_000
}
fn default_send_keys_delay_ms() -> u64 {
    120
}
fn default_max_payload_bytes() -> usize {
    512 * 1024
}
fn default_supervisor_timeout_ms() -> u64 {
    5_000
}
fn default_pairing_ttl_secs() -> u64 {
    crate::pairing::PAIRING_TTL_SECS
}
fn default_local_resolve_grace_ms() -> u64 {
    3_000
}
fn default_local_resolve_poll_ms() -> u64 {
    2_000
}
fn default_liveness_sweep_secs() -> u64 {
    60
}
fn default_diff_max_bytes() -> usize {
    crate::ws::MAX_DIFF_BYTES
}
fn default_diff_timeout_ms() -> u64 {
    10_000
}
fn default_cert_refresh_days() -> u64 {
    30
}
fn default_log_max_bytes() -> u64 {
    8 * 1024 * 1024
}
fn default_log_rotate_secs() -> u64 {
    300
}
fn default_tmux_history_limit() -> u32 {
    50_000
}
fn default_ws_max_connections() -> usize {
    64
}
fn default_ws_max_per_peer() -> usize {
    32
}
fn default_ipc_max_connections() -> usize {
    128
}
fn default_ipc_write_queue() -> usize {
    1_024
}
fn default_pairing_max_attempts() -> u32 {
    20
}
fn default_pairing_window_secs() -> u64 {
    300
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default = "default_ws_port")]
    pub ws_port: u16,
    /// Explicit bind address. When absent the daemon asks `tailscale ip -4`,
    /// and falls back to loopback if Tailscale is not up.
    pub ws_bind: Option<String>,
    /// Also listen on `127.0.0.1`, for tools running on the Mac itself.
    ///
    /// The tailnet address is the only one the phone uses, but it is not always
    /// reachable *from this machine*: a third-party network filter that cannot
    /// identify a locally built binary will hold its connections to the utun
    /// interface open and silent (measured — Little Snitch does exactly this to
    /// an unsigned `cargo build` output), while leaving loopback alone. Without
    /// this, the soak harness and any local diagnostic client are at the mercy
    /// of the operator's firewall rules.
    ///
    /// It is not a second door: the bearer token is still required, and the
    /// unix socket next to it has been reachable by this account all along.
    /// Turn it off with `"ws_loopback": false` if the Mac has untrusted local
    /// users.
    #[serde(default = "default_true")]
    pub ws_loopback: bool,

    /// Which hook event carries the gate. `none` disables holding entirely.
    ///
    /// NOTE (claude 2.1.220): this previously read "the `PermissionRequest`
    /// hook fires but its return value does **not** decide the permission —
    /// measured against that build", and the send-keys answer path exists
    /// because of it. That measurement was real but the conclusion was wrong:
    /// the return value was being emitted in the wrong schema. `PermissionRequest`
    /// takes a *nested* `hookSpecificOutput.decision.behavior`, not the flat
    /// top-level `decision` that `PreToolUse` accepts. With the correct shape,
    /// re-measured against 2.1.220, the return value decides the permission in
    /// both directions and no local prompt appears at all.
    ///
    /// So answers no longer *have* to be typed into the pane. Moving them onto
    /// the hook's return value is what would make "approvals ride structured
    /// channels, never parsed terminal bytes" true of the approval path as well
    /// — but it is a behavioural change (the local prompt is delayed while the
    /// gate is held), so it is gated on `hold_ms` rather than switched on here.
    #[serde(default = "default_gate_hook")]
    pub gate_hook: String,

    /// How long the daemon holds a gate hook open waiting for a phone answer.
    /// Default 0: holding delays the *local* prompt, and in mirror mode the
    /// local prompt is what the phone's answer is typed into.
    pub hold_ms: u64,

    /// cc-hook's own wait budget. Must exceed `hold_ms`.
    #[serde(default = "default_gate_timeout_ms")]
    pub gate_timeout_ms: u64,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,

    /// Where the Apple `.p8` push key lives, and who it belongs to.
    ///
    /// All four are absent by default and push stays a logging stub until every
    /// one is present: a half-configured sender would advertise `push: live` in
    /// `hello_ack` and then fail on every send, which is worse than saying
    /// plainly that push is off. None can be guessed — the key id and team id
    /// come from the developer account and the topic is the app's bundle id.
    ///
    /// The key is a **private key**. Keep it outside the repository;
    /// `~/.codeconnect/secrets/` is created `0700` for exactly this.
    #[serde(default)]
    pub apns_key_path: Option<String>,
    #[serde(default)]
    pub apns_key_id: Option<String>,
    #[serde(default)]
    pub apns_team_id: Option<String>,
    #[serde(default)]
    pub apns_topic: Option<String>,

    /// Opt in to PreToolUse `ask` when the daemon is unreachable. Off by
    /// default: it is the only channel that renders our reason to the local
    /// operator, but it also forces a prompt on every auto-approved tool call.
    pub unreachable_ask: bool,

    #[serde(default = "default_tail_poll_ms")]
    pub tail_poll_ms: u64,
    #[serde(default = "default_stale_after_ms")]
    pub stale_after_ms: u64,
    #[serde(default = "default_send_keys_delay_ms")]
    pub send_keys_delay_ms: u64,
    #[serde(default = "default_supervisor_timeout_ms")]
    pub supervisor_timeout_ms: u64,
    #[serde(default = "default_max_payload_bytes")]
    pub max_payload_bytes: usize,

    /// Override the presence needles if Claude's TUI copy changes.
    pub input_box_needles: Vec<String>,
    pub permission_prompt_needles: Vec<String>,

    /// Try to obtain a `tailscale cert` and serve `wss://`. When this is on but
    /// no certificate can be had, the daemon falls back to `ws://` and says so
    /// in `hello_ack.capabilities.tls` rather than refusing to start.
    pub tls: bool,
    /// Refuse plaintext connections once every client speaks `wss://`.
    ///
    /// Off by default because both schemes share one port during the migration:
    /// turning TLS on must not lock out a phone that has not been updated yet.
    /// Turning this on is the deliberate second step.
    pub tls_required: bool,
    /// Override the MagicDNS name used for the certificate and the QR host.
    /// Normally discovered from `tailscale status --json`.
    pub tls_hostname: Option<String>,
    /// Re-issue the certificate when it has fewer days of validity than this.
    #[serde(default = "default_cert_refresh_days")]
    pub cert_refresh_days: u64,

    #[serde(default = "default_pairing_ttl_secs")]
    pub pairing_ttl_secs: u64,

    /// Watch for approvals that were answered at the Mac's keyboard and resolve
    /// them as [`crate::ws::ResolvedBy::Local`]. Best-effort by construction.
    pub local_resolve: bool,
    /// How long to wait after an approval appears before believing the absence
    /// of a prompt. The hook fires microseconds *before* the TUI renders, so
    /// checking immediately would resolve every approval as local.
    #[serde(default = "default_local_resolve_grace_ms")]
    pub local_resolve_grace_ms: u64,
    #[serde(default = "default_local_resolve_poll_ms")]
    pub local_resolve_poll_ms: u64,

    /// How often the daemon proves each session's `lifecycle` against tmux.
    ///
    /// This is the backstop for the case a supervisor cannot report: the daemon
    /// was down when the agent died, the supervisor was killed, or the Mac
    /// slept. Without it a row that nobody reported the end of stayed `live` for
    /// ever, and the fleet confidently showed agents that had been gone for
    /// days.
    ///
    /// A sweep costs one short-lived `tmux has-session` per *distinct tmux
    /// name*, one at a time, so a minute is unnoticeable even on a machine with
    /// a long history. `0` turns it off, which leaves only the startup pass —
    /// and turning it off entirely restores the defect, so it is spelled as a
    /// deliberate choice rather than offered as a tuning knob.
    #[serde(default = "default_liveness_sweep_secs")]
    pub liveness_sweep_secs: u64,

    #[serde(default = "default_diff_max_bytes")]
    pub diff_max_bytes: usize,
    #[serde(default = "default_diff_timeout_ms")]
    pub diff_timeout_ms: u64,
    /// Explicit path to `git` (launchd has no shell PATH).
    pub git_bin: Option<String>,

    /// Use FSEvents to notice transcript writes immediately. The poll below
    /// stays running either way: FSEvents is a latency optimisation, never the
    /// correctness floor, because a coalesced stream can drop notifications.
    pub fsevents: bool,

    /// Explicit path to the real `claude` binary (launchd has no shell PATH).
    pub claude_bin: Option<String>,

    /// Cap on each of the daemon's own launchd logs. One previous generation is
    /// kept alongside, so the ceiling is twice this per stream.
    ///
    /// The daemon rotates these itself because launchd holds the descriptors
    /// open: an external rotator that renames the file would leave launchd
    /// appending to an unlinked inode forever.
    #[serde(default = "default_log_max_bytes")]
    pub log_max_bytes: u64,
    /// How often the cap is checked.
    #[serde(default = "default_log_rotate_secs")]
    pub log_rotate_secs: u64,

    /// Show tmux's status bar inside the session. Off by default: `codeconnect claude`
    /// is meant to be visually indistinguishable from plain `claude`, and a
    /// status line is the one thing that gives the hosting away.
    pub tmux_status: bool,

    /// Lines of scrollback each hosted pane keeps.
    ///
    /// This is what the wheel scrolls: Claude Code renders inline under tmux,
    /// so the conversation *is* the pane's history, and tmux's stock 2,000
    /// lines silently amputated everything older on a long run. Fixed at pane
    /// creation by tmux, so it reaches the server before the first pane —
    /// raising it later cannot enlarge panes that already exist.
    ///
    /// 50,000 dense 120-column lines cost roughly 30 MiB per pane; the ceiling
    /// exists so a typo in a config file cannot commit gigabytes.
    #[serde(default = "default_tmux_history_limit")]
    pub tmux_history_limit: u32,

    /// How many tailnet WebSocket connections may exist at once.
    ///
    /// The accept loop used to `spawn` unconditionally, so anything that could
    /// reach the tailnet port could hold the daemon's whole file-descriptor
    /// budget open — and the *local* IPC socket, which carries the hook path and
    /// every supervisor link, lives on that same budget.
    ///
    /// The numbers come from that budget rather than from taste. macOS gives a
    /// launchd job a soft `RLIMIT_NOFILE` of 256. The daemon needs roughly 20
    /// for itself (five SQLite connections plus their WAL and SHM handles, two
    /// or three listeners, the log files, the FSEvents watcher), which leaves
    /// about 230 to divide. `64` here and `128` for IPC totals 192 and keeps
    /// headroom, where a larger WebSocket cap would let a remote peer squeeze
    /// out the hooks — and a hook that cannot reach the daemon is an agent that
    /// stops being observable.
    #[serde(default = "default_ws_max_connections")]
    pub ws_max_connections: usize,
    /// How many of those one peer address may hold.
    ///
    /// The global cap alone is not enough: without a per-peer share, one
    /// misbehaving client reaches the global limit by itself and every other
    /// device is refused. Half the global cap, so at least one other peer is
    /// always guaranteed room.
    ///
    /// Generous rather than tight, for two measured reasons. Every connection
    /// arriving on the loopback listener has the *same* peer address, so a tight
    /// per-peer share would act as a global cap on all local tooling at once.
    /// And a client is entitled to open several sockets deliberately — the soak
    /// harness's answer storm opens twenty concurrently, precisely because
    /// pipelining them down one socket would not test the race it is there to
    /// test. Eight refused twelve of those; the cap has to bound a runaway
    /// without breaking a legitimate burst.
    #[serde(default = "default_ws_max_per_peer")]
    pub ws_max_per_peer: usize,
    /// How many local IPC connections may exist at once.
    ///
    /// The larger share of the descriptor budget, because this is the side that
    /// must never starve: every `cc` invocation, every hook and every supervisor
    /// is one of these. The socket is `0600`, so reaching it already means being
    /// the owner of this account — this bounds a runaway, not an attacker.
    #[serde(default = "default_ipc_max_connections")]
    pub ipc_max_connections: usize,
    /// Frames one IPC connection may have queued for writing.
    ///
    /// The writer used to be fed by an unbounded channel, so a client that
    /// stopped reading — a `codeconnect ls` suspended with ctrl-Z is enough — let the
    /// daemon buffer without limit on its behalf.
    #[serde(default = "default_ipc_write_queue")]
    pub ipc_write_queue: usize,
    /// Failed pairing attempts allowed inside [`Config::pairing_window_secs`].
    ///
    /// Codes have real entropy, so this is not what stops a guess; it is what
    /// stops an *unbounded* number of guesses against a credential whose whole
    /// safety argument is "it expires in five minutes". Zero disables it.
    #[serde(default = "default_pairing_max_attempts")]
    pub pairing_max_attempts: u32,
    /// The window those attempts are counted over.
    #[serde(default = "default_pairing_window_secs")]
    pub pairing_window_secs: u64,

    pub apns: ApnsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ws_port: default_ws_port(),
            ws_bind: None,
            ws_loopback: true,
            gate_hook: default_gate_hook(),
            hold_ms: 0,
            apns_key_path: None,
            apns_key_id: None,
            apns_team_id: None,
            apns_topic: None,
            gate_timeout_ms: default_gate_timeout_ms(),
            connect_timeout_ms: default_connect_timeout_ms(),
            unreachable_ask: false,
            tail_poll_ms: default_tail_poll_ms(),
            stale_after_ms: default_stale_after_ms(),
            send_keys_delay_ms: default_send_keys_delay_ms(),
            supervisor_timeout_ms: default_supervisor_timeout_ms(),
            max_payload_bytes: default_max_payload_bytes(),
            input_box_needles: Vec::new(),
            permission_prompt_needles: Vec::new(),
            tls: true,
            tls_required: false,
            tls_hostname: None,
            cert_refresh_days: default_cert_refresh_days(),
            pairing_ttl_secs: default_pairing_ttl_secs(),
            local_resolve: true,
            local_resolve_grace_ms: default_local_resolve_grace_ms(),
            local_resolve_poll_ms: default_local_resolve_poll_ms(),
            liveness_sweep_secs: default_liveness_sweep_secs(),
            diff_max_bytes: default_diff_max_bytes(),
            diff_timeout_ms: default_diff_timeout_ms(),
            git_bin: None,
            fsevents: true,
            claude_bin: None,
            log_max_bytes: default_log_max_bytes(),
            log_rotate_secs: default_log_rotate_secs(),
            tmux_history_limit: default_tmux_history_limit(),
            tmux_status: false,
            ws_max_connections: default_ws_max_connections(),
            ws_max_per_peer: default_ws_max_per_peer(),
            ipc_max_connections: default_ipc_max_connections(),
            ipc_write_queue: default_ipc_write_queue(),
            pairing_max_attempts: default_pairing_max_attempts(),
            pairing_window_secs: default_pairing_window_secs(),
            apns: ApnsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApnsConfig {
    /// Path to the `.p8` key. Absent keeps the logging stub in place.
    pub key_path: Option<String>,
    pub key_id: Option<String>,
    pub team_id: Option<String>,
    pub topic: Option<String>,
    /// TestFlight uses PRODUCTION APNs — the single most common misconfiguration.
    pub production: bool,
}

impl Config {
    pub fn load() -> Config {
        let path = crate::config_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Config::default(),
            Err(err) => {
                eprintln!(
                    "codeconnect: config unreadable at {}: {err}; using defaults",
                    path.display()
                );
                return Config::default();
            }
        };
        match serde_json::from_str::<Config>(&raw) {
            Ok(config) => config.sanitised(),
            Err(err) => {
                eprintln!(
                    "codeconnect: config invalid at {}: {err}; using defaults",
                    path.display()
                );
                Config::default()
            }
        }
    }

    /// Clamp values whose extremes would break an invariant rather than merely
    /// behave oddly.
    fn sanitised(mut self) -> Config {
        // Below 1,000 the wheel hits a wall mid-conversation and the scroll fix
        // this exists for is silently undone; above 200,000 a handful of panes
        // can hold gigabytes of one agent's stdout.
        if !(1_000..=200_000).contains(&self.tmux_history_limit) {
            let clamped = self.tmux_history_limit.clamp(1_000, 200_000);
            eprintln!(
                "codeconnect: tmux_history_limit {} is outside 1000..=200000; using {clamped}",
                self.tmux_history_limit
            );
            self.tmux_history_limit = clamped;
        }
        if self.gate_timeout_ms <= self.hold_ms {
            eprintln!(
                "codeconnect: gate_timeout_ms ({}) must exceed hold_ms ({}); raising it",
                self.gate_timeout_ms, self.hold_ms
            );
            self.gate_timeout_ms = self.hold_ms.saturating_add(5_000);
        }
        self.tail_poll_ms = self.tail_poll_ms.clamp(50, 10_000);
        self.connect_timeout_ms = self.connect_timeout_ms.clamp(10, 5_000);
        self.max_payload_bytes = self.max_payload_bytes.clamp(4 * 1024, 8 * 1024 * 1024);
        // A pairing code is a weak secret whose safety comes from its lifetime;
        // an hour-long "temporary" code is a standing invitation.
        self.pairing_ttl_secs = self.pairing_ttl_secs.clamp(30, 3_600);
        self.local_resolve_poll_ms = self.local_resolve_poll_ms.clamp(250, 60_000);
        // Zero is meaningful — it disables the sweep — so only a non-zero value
        // is clamped. The floor is above the confirmation delay a single sweep
        // spends inside itself, so a sweep can never be asked to start before
        // the previous one has had time to finish deciding.
        if self.liveness_sweep_secs != 0 {
            self.liveness_sweep_secs = self.liveness_sweep_secs.clamp(10, 86_400);
        }
        // Below the grace period the detector fires before the TUI has drawn
        // the prompt it is looking for, and resolves every approval as local.
        self.local_resolve_grace_ms = self.local_resolve_grace_ms.max(1_000);
        self.diff_max_bytes = self.diff_max_bytes.clamp(1024, 8 * 1024 * 1024);
        self.diff_timeout_ms = self.diff_timeout_ms.clamp(500, 120_000);
        self.cert_refresh_days = self.cert_refresh_days.clamp(1, 80);
        // A cap below a few KB would rotate away the startup banner that says
        // why the daemon is unhappy — the single most useful thing in the file.
        self.log_max_bytes = self.log_max_bytes.clamp(64 * 1024, 1024 * 1024 * 1024);
        self.log_rotate_secs = self.log_rotate_secs.clamp(10, 86_400);
        // A cap of zero would refuse every connection and leave the operator
        // with a daemon that starts, logs nothing unusual, and answers nobody.
        // The upper bounds keep a typo from restoring the unbounded behaviour
        // these exist to remove.
        self.ws_max_connections = self.ws_max_connections.clamp(1, 4_096);
        self.ws_max_per_peer = self.ws_max_per_peer.clamp(1, self.ws_max_connections);
        self.ipc_max_connections = self.ipc_max_connections.clamp(1, 4_096);
        self.ipc_write_queue = self.ipc_write_queue.clamp(8, 65_536);
        // Zero is meaningful here — it disables the limiter — so only the top
        // is clamped.
        self.pairing_max_attempts = self.pairing_max_attempts.min(10_000);
        self.pairing_window_secs = self.pairing_window_secs.clamp(1, 86_400);
        self
    }

    pub fn gate_event(&self) -> Option<crate::hook::HookEventName> {
        match self.gate_hook.as_str() {
            "none" | "" => None,
            other => Some(crate::hook::HookEventName::parse(other)),
        }
    }

    pub fn input_box_needles(&self) -> Option<&[String]> {
        (!self.input_box_needles.is_empty()).then_some(&self.input_box_needles)
    }

    pub fn permission_prompt_needles(&self) -> Option<&[String]> {
        (!self.permission_prompt_needles.is_empty()).then_some(&self.permission_prompt_needles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_coherent() {
        let config = Config::default();
        assert_eq!(config.hold_ms, 0);
        assert!(config.gate_timeout_ms > config.hold_ms);
        assert!(!config.unreachable_ask);
        assert_eq!(
            config.gate_event(),
            Some(crate::hook::HookEventName::PermissionRequest)
        );
    }

    /// macOS's soft `RLIMIT_NOFILE` for a launchd job.
    const LAUNCHD_NOFILE: usize = 256;
    /// Descriptors the daemon needs for itself: five SQLite connections plus
    /// their WAL and SHM handles, two or three listeners, the log files, the
    /// FSEvents watcher. Rounded up generously — being wrong in this direction
    /// costs a refused connection, and being wrong in the other costs the hook
    /// path.
    const DAEMON_OVERHEAD: usize = 32;

    #[test]
    fn the_connection_caps_fit_inside_the_descriptor_budget() {
        // The caps exist to protect a *shared* file-descriptor budget: the
        // tailnet listener and the local IPC socket draw on the same one, and
        // exhausting it from the network side takes the hook path down with it.
        // Caps whose sum exceeds the budget do not protect it — they are two
        // numbers that happen to be finite. This is the arithmetic that makes
        // them a bound, written down so raising one is a decision rather than
        // an accident.
        let config = Config::default();
        let total = config.ws_max_connections + config.ipc_max_connections + DAEMON_OVERHEAD;
        assert!(
            total <= LAUNCHD_NOFILE,
            "ws {} + ipc {} + {DAEMON_OVERHEAD} overhead = {total}, past the {LAUNCHD_NOFILE} \
             descriptors launchd gives us",
            config.ws_max_connections,
            config.ipc_max_connections,
        );
        // The hook path must never be the side that starves: a hook that cannot
        // reach the daemon is an agent that stops being observable.
        assert!(
            config.ipc_max_connections > config.ws_max_connections,
            "the local side must have the larger share"
        );
        // A per-peer share that equals the global cap is not a share at all.
        assert!(
            config.ws_max_per_peer < config.ws_max_connections,
            "one peer must never be able to take every slot"
        );
        // …and one generous enough for a legitimate burst. The soak harness's
        // answer storm opens twenty sockets at once on purpose, and every
        // loopback client shares one peer address.
        assert!(
            config.ws_max_per_peer >= 20,
            "a per-peer cap of {} refuses a legitimate concurrent burst",
            config.ws_max_per_peer
        );
    }

    #[test]
    fn a_per_peer_cap_above_the_global_one_is_clamped_down() {
        let config: Config =
            serde_json::from_str(r#"{"ws_max_connections": 10, "ws_max_per_peer": 9999}"#).unwrap();
        let config = config.sanitised();
        assert_eq!(config.ws_max_connections, 10);
        assert_eq!(
            config.ws_max_per_peer, 10,
            "a per-peer cap cannot exceed the global one"
        );
    }

    #[test]
    fn a_zero_connection_cap_is_raised_rather_than_refusing_everybody() {
        // A daemon that starts, logs nothing unusual and answers nobody is the
        // worst possible reading of a typo.
        let config: Config = serde_json::from_str(
            r#"{"ws_max_connections": 0, "ipc_max_connections": 0, "ipc_write_queue": 0}"#,
        )
        .unwrap();
        let config = config.sanitised();
        assert!(config.ws_max_connections >= 1);
        assert!(config.ipc_max_connections >= 1);
        assert!(config.ipc_write_queue >= 8);
    }

    #[test]
    fn a_zero_pairing_limit_means_disabled_and_survives_sanitising() {
        // Unlike the connection caps, zero is *meaningful* here — it turns the
        // limiter off — so it must not be clamped up to one, which would be the
        // tightest possible limit rather than none.
        let config: Config = serde_json::from_str(r#"{"pairing_max_attempts": 0}"#).unwrap();
        assert_eq!(config.sanitised().pairing_max_attempts, 0);
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let config: Config = serde_json::from_str(r#"{"ws_port": 9999}"#).unwrap();
        assert_eq!(config.ws_port, 9999);
        assert_eq!(config.gate_timeout_ms, default_gate_timeout_ms());
    }

    #[test]
    fn gate_timeout_is_raised_above_hold() {
        let config: Config =
            serde_json::from_str(r#"{"hold_ms": 60000, "gate_timeout_ms": 1000}"#).unwrap();
        let config = config.sanitised();
        assert!(config.gate_timeout_ms > 60_000);
    }

    #[test]
    fn gate_can_be_disabled() {
        let config: Config = serde_json::from_str(r#"{"gate_hook":"none"}"#).unwrap();
        assert_eq!(config.gate_event(), None);
    }

    #[test]
    fn absurd_poll_intervals_are_clamped() {
        let config: Config = serde_json::from_str(r#"{"tail_poll_ms": 0}"#).unwrap();
        assert_eq!(config.sanitised().tail_poll_ms, 50);
    }

    #[test]
    fn defaults_are_on_but_never_lock_anyone_out() {
        let config = Config::default();
        assert!(config.tls, "TLS is attempted by default");
        assert!(
            !config.tls_required,
            "plaintext must keep working until the operator opts out"
        );
        assert!(config.local_resolve);
        assert!(config.fsevents);
        assert_eq!(config.pairing_ttl_secs, 300);
        assert_eq!(config.diff_max_bytes, 512 * 1024);
    }

    #[test]
    fn an_older_config_file_still_loads_and_gets_defaults() {
        // The one property that matters for an in-place upgrade: a config file
        // written before these fields existed must not lose behaviour or fail
        // to parse.
        let config: Config = serde_json::from_str(
            r#"{"ws_port":8787,"hold_ms":0,"gate_hook":"PermissionRequest","tmux_status":false}"#,
        )
        .unwrap();
        let config = config.sanitised();
        assert_eq!(config.ws_port, 8787);
        assert!(config.tls);
        assert!(config.fsevents);
        assert_eq!(config.pairing_ttl_secs, 300);
    }

    #[test]
    fn a_pairing_ttl_that_defeats_the_point_is_clamped() {
        let config: Config = serde_json::from_str(r#"{"pairing_ttl_secs": 86400}"#).unwrap();
        assert_eq!(config.sanitised().pairing_ttl_secs, 3_600);
        let config: Config = serde_json::from_str(r#"{"pairing_ttl_secs": 0}"#).unwrap();
        assert_eq!(config.sanitised().pairing_ttl_secs, 30);
    }

    #[test]
    fn a_local_resolve_grace_below_the_render_race_is_raised() {
        let config: Config = serde_json::from_str(r#"{"local_resolve_grace_ms": 0}"#).unwrap();
        assert_eq!(config.sanitised().local_resolve_grace_ms, 1_000);
    }

    #[test]
    fn tls_can_be_turned_off_entirely() {
        let config: Config = serde_json::from_str(r#"{"tls": false}"#).unwrap();
        assert!(!config.sanitised().tls);
    }

    #[test]
    fn the_loopback_listener_is_on_by_default_and_can_be_turned_off() {
        // On by default because a local tool being unable to reach the daemon
        // on the same machine is a support problem with no visible cause; off
        // is one line for anyone who wants only the tailnet.
        assert!(Config::default().ws_loopback);
        let config: Config = serde_json::from_str(r#"{"ws_loopback": false}"#).unwrap();
        assert!(!config.ws_loopback);
        // A config file written before the field existed keeps the default.
        let config: Config = serde_json::from_str(r#"{"ws_port": 8787}"#).unwrap();
        assert!(config.ws_loopback);
    }

    #[test]
    fn log_rotation_defaults_are_bounded_and_absurd_values_are_clamped() {
        let config = Config::default();
        assert_eq!(config.log_max_bytes, 8 * 1024 * 1024);
        assert_eq!(config.log_rotate_secs, 300);

        // Zero would rotate on every tick and throw away the startup banner —
        // the one part of the log that explains a daemon that will not work.
        let config: Config =
            serde_json::from_str(r#"{"log_max_bytes":0,"log_rotate_secs":0}"#).unwrap();
        let config = config.sanitised();
        assert_eq!(config.log_max_bytes, 64 * 1024);
        assert_eq!(config.log_rotate_secs, 10);
    }
}

# Changelog

User-facing changes, newest first. Mac releases are cut per
[`RELEASING.md`](RELEASING.md); the iPhone app ships on its own App Store track.

## Push gateway — protocol minor 14

Relay-backed push notifications, so an iPhone can be rung by a Mac that holds no
Apple push key of its own — the case for every App Store customer.

- **Relay-backed notifications are the default.** A daemon with no Apple push key
  now sends through CodeConnect's push relay, which builds a generic notification
  titled `CodeConnect` and forwards it to Apple. The relay receives only the APNs
  token and environment, an opaque token-bound credential, one of four fixed
  event kinds, a blocked-run count, and a test marker when applicable — never
  project names, session identifiers, commands, file paths, diffs, or
  conversation content. See the [privacy policy](site/privacy.md).
- **App Attest enrollment.** The iPhone proves it is a genuine copy of the app
  with Apple's App Attest to obtain the relay credential, which lives — with the
  App Attest key ID — in `ThisDeviceOnly` Keychain storage. App Attest runs for
  enrollment or re-enrollment after pairing — normally once per install, and
  again after a reset or credential recovery — never per push.
- **Direct-key override, unchanged.** A Mac configured with its own APNs key
  (`apns_key_path`, `apns_key_id`, `apns_team_id`, `apns_topic`) talks straight
  to Apple and keeps the project-labelled payload. This override always takes
  precedence over the relay.
- **Off.** `push_enabled: false` disables push entirely.
- **Protocol.** `PROTOCOL_MINOR` is now 14 (major stays 1): the `push_relay`
  capability, `register_push.relay_credential`, the `credential_invalid` test
  result, and `hello_ack.push_environment`. All additive — an older app or daemon
  keeps working, and a relay daemon advertises the legacy `push` capability as
  false on purpose so an older app never registers a credential-less token.

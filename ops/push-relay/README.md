# Push relay deployment

The relay runs as a single Render web service built from this repository's
`ops/push-relay/Dockerfile`. This file is the deployment's settings, written
down; the operational procedures behind them — key rotation, outage, abuse,
database restore — are in [`docs/push-gateway.md`](../../docs/push-gateway.md)
§7 and are not repeated here.

## Service settings

| Setting | Value |
| --- | --- |
| Runtime | Docker |
| Repository | this repository |
| Branch | `main` |
| Dockerfile path | `./ops/push-relay/Dockerfile` |
| Docker build context | `./mac` |
| Auto-deploy | **off** |
| Instance type | a paid always-on plan |
| Instances | 1 |
| Region | `oregon` |
| Disk | 1 GB, mounted at `/var/data` |
| Health check path | `/healthz` |

Auto-deploy is off deliberately. With it on, any later push to the branch
replaces whatever release is running, so a deploy pinned to a reviewed commit
would be silently overwritten by the next unrelated commit to land.

A free instance sleeps when idle. A push path that sleeps drops the
notification that was the entire point of it, so the instance is a paid plan.

The disk holds the SQLite database. It also fixes the service at one instance
and takes each deploy through a brief stop-then-start rather than a handover —
both already assumed by the single-instance design. The region cannot be
changed afterwards without moving the disk, so it is a one-time decision.

`/healthz` must stay cheap and dependency-free. Render fails a health check
that does not answer within five seconds; fifteen seconds of consecutive
failures pauses traffic and sixty seconds restarts the instance. A health
endpoint that touched SQLite or APNs would turn a slow disk into a restart
loop.

## Secret files

Every secret is a Render **secret file**, never an environment variable and
never in this repository. Render mounts them at `/etc/secrets/<filename>` at
run time only — they do not exist during `docker build`, so nothing in the
image build may depend on one.

| Path | Contents |
| --- | --- |
| `/etc/secrets/apns-sandbox.p8` | sandbox APNs signing key |
| `/etc/secrets/apns-production.p8` | production APNs signing key |
| `/etc/secrets/generation-floor` | decimal integer, the minimum credential generation |
| `/etc/secrets/backup-key` | 32 bytes, hex encoded, encrypts the SQLite online backup |
| `/etc/secrets/ip-pepper` | HMAC key for the daily-rotating IP rate-limit keys |
| `/etc/secrets/backup-s3-secret-key` | S3 secret access key for the encrypted-backup upload target (production) |

The container runs as a non-root user in group 1000, which is what Render
documents as the group that can read these files.

## Environment

Non-secret configuration only. The secret-file paths are overridable so the
relay can be run locally against a directory that is not `/etc/secrets`.

| Variable | Notes |
| --- | --- |
| `PORT` | supplied by Render; the process binds `0.0.0.0:$PORT` |
| `RELAY_DB_PATH` | defaults to `/var/data/relay.sqlite` |
| `RELAY_APP_ID` | Apple team ID and bundle ID, for App Attest |
| `RELAY_ATTEST_ENVIRONMENT` | `development` or `production` |
| `RELAY_MIN_BUNDLE_VERSION` | optional attestation floor |
| `RELAY_APNS_SANDBOX_KEY_ID` | sandbox key ID |
| `RELAY_APNS_SANDBOX_TEAM_ID` | sandbox team ID |
| `RELAY_APNS_SANDBOX_TOPIC` | sandbox APNs topic |
| `RELAY_APNS_PRODUCTION_KEY_ID` | production key ID |
| `RELAY_APNS_PRODUCTION_TEAM_ID` | production team ID |
| `RELAY_APNS_PRODUCTION_TOPIC` | production APNs topic |
| `RELAY_SEND_ENABLED` | kill switch for outbound pushes |
| `RELAY_ENROLLMENT_ENABLED` | kill switch for new enrollment, independent of sends |
| `RELAY_BACKUP_TARGET` | `s3://bucket[/prefix]` in production; `file:///path` (for example `file:///var/data/backups`) for local development |
| `RELAY_BACKUP_S3_ENDPOINT` | S3-compatible endpoint URL for the backup target |
| `RELAY_BACKUP_S3_REGION` | S3 region for the backup target (defaults to `us-east-1`) |
| `RELAY_BACKUP_S3_ACCESS_KEY_ID` | S3 access key ID for the backup target |
| `RELAY_BACKUP_RETENTION_DAYS` | backup retention |
| `RELAY_GIT_SHA` | the deployed commit, reported by the `/readyz` endpoint and the startup log |
| `RELAY_APNS_SANDBOX_KEY_FILE` | overrides `/etc/secrets/apns-sandbox.p8` |
| `RELAY_APNS_PRODUCTION_KEY_FILE` | overrides `/etc/secrets/apns-production.p8` |
| `RELAY_GENERATION_FLOOR_FILE` | overrides `/etc/secrets/generation-floor` |
| `RELAY_BACKUP_KEY_FILE` | overrides `/etc/secrets/backup-key` |
| `RELAY_IP_PEPPER_FILE` | overrides `/etc/secrets/ip-pepper` |
| `RELAY_BACKUP_S3_SECRET_KEY_FILE` | overrides `/etc/secrets/backup-s3-secret-key` |

## Keys-absent state

A missing `.p8` does not stop the relay. It boots into a reported keys-absent
state: health and the enrollment-challenge endpoints serve normally, and every
push returns `unavailable` for the environment whose key is missing. That is
the expected state until the APNs key is issued in the developer portal and
mounted, and it is why a phone can be enrolled before a single push can be
delivered.

## Deploy and rollback

Deploys are pinned to a git SHA and triggered explicitly, through the Render
API or dashboard — never by pushing a branch. Rolling back is the same
operation aimed at an earlier known-good SHA.

The database is not rolled back with the code. A restore is its own procedure,
including the generation-floor bump that keeps a revoked credential from coming
back to life, and it is in `docs/push-gateway.md` §7.

Both base images in the Dockerfile are pinned by digest, so patching them is an
edit to that file and a new deploy, on the cadence §7 sets.

Render is not connected to this repository through its GitHub app; it clones the
public repository anonymously instead, which its build log says out loud. That
is sufficient for a pinned deploy and costs nothing while auto-deploy is off,
but the deploy-on-push and pull-request preview features are unavailable until
someone grants Render access to the repository in the dashboard. Nothing here
depends on them.

## Proven on Render

Render documents the Dockerfile path and the build context as independent
settings, but does not document whether the Dockerfile may live outside the
context. `docker build -f ops/push-relay/Dockerfile mac/` is valid Docker,
is proven by the `Relay` workflow on every push, and is now proven on Render
too: the relay builds and runs there from this Dockerfile path and this build
context. Were Render ever to reject the combination, the fallback is to move
the build context to the repository root and add a root `.dockerignore`.

The mounted disk's ownership is likewise settled: the non-root user creates and
writes the database under `/var/data` on the running instance.

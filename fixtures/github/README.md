# GitHub captures for the Changes panel's GitHub read (ccd `github.rs`)

- `gh-version.txt`, `api-signed-out.*`: gh 2.87.3 under `env -i` with an empty scratch `HOME` and `GH_CONFIG_DIR`, no
  token: its own output, exit codes in `*.exit`.
- `api-offline.*`: the same gh with a placeholder `GH_TOKEN` and `HTTPS_PROXY` at a closed port on 127.0.0.1: its own
  connection error; no request left the computer.
- `pulls-open.json`, `pulls-closed.json`: GitHub's public REST API, `repos/cli/cli/pulls` (`state=open&per_page=2`;
  `state=closed&sort=updated&direction=desc&per_page=1`), unauthenticated, captured 2026-11-26; each entry kept to the fields the
  daemon reads (number, state, html_url, merged_at, base.ref, head.ref, head.sha, head.repo.full_name).
- Not here, because no credential is used to capture: an authenticated `gh api` (exit 0; its body is the pulls JSON above) and
  GitHub refusing a token (`HTTP 401`, read as signed out from gh's source).

# GitHub captures for the Changes panel's GitHub read (ccd `github.rs`)

- `gh-version.txt`, `auth-signed-out.*`, `api-signed-out.*`: gh 2.87.3 under `env -i` with an empty scratch `HOME` and
  `GH_CONFIG_DIR`, no token: its own output, exit codes in `*.exit`.
- `pulls-open.json`, `pulls-closed.json`: GitHub's public REST API, `repos/cli/cli/pulls` (`state=open&per_page=2`;
  `state=closed&sort=updated&direction=desc&per_page=1`), unauthenticated, captured 2026-11-26; each entry kept to the fields the
  daemon reads (number, state, html_url, merged_at, base.ref, head.ref, head.sha, head.repo.full_name).
- Not here, because no credential is used to capture: a signed-in `gh auth status` and an authenticated `gh api`. The tests write
  those as gh's source prints them (exit 0; the body is the pulls JSON above), marked in the test.

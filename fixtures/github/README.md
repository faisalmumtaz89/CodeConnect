# GitHub captures for the Changes panel's GitHub read (ccd `github.rs`)

- `gh-version.txt`, `api-signed-out.*`: gh 2.87.3 under `env -i` with an empty scratch `HOME` and `GH_CONFIG_DIR`, no
  token: its own output, exit codes in `*.exit`.
- `api-offline.*`: the same gh with a placeholder `GH_TOKEN` and `HTTPS_PROXY` at a closed port on 127.0.0.1: its own
  connection error; no request left the computer.
- `pulls-open.json`, `pulls-closed.json`: GitHub's public REST API, `repos/cli/cli/pulls` (`state=open&per_page=2`;
  `state=closed&sort=updated&direction=desc&per_page=1`), unauthenticated, captured 2026-11-26; each entry kept to the fields the
  daemon reads (number, state, html_url, merged_at, base.ref, head.ref, head.sha, head.repo.full_name).
- `api-token-refused.*`: **written from gh's source, not captured**: GitHub refusing a token (HTTP 401), with gh's hint to sign in.
- Not here, because no credential is used to capture: an authenticated `gh api` (exit 0; its body is the pulls JSON above).
- `checks-graphql.json`: **written from GitHub's GraphQL schema, not captured** (GraphQL takes no anonymous request): the answer to
  ccd's `gh api graphql` checks query (`statusCheckRollup` of an open PR's last commit) with each case the daemon reads (a re-run
  check among them). The owner's real-GitHub end-to-end captures a real answer to the same query: it goes beside this file with a
  test of its own (it parses, at its own head), and this one keeps the mapping cases.

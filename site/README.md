# site/ — pages the codeconnect.sh website must serve

This directory holds the App Store–required web pages. The website team deploys them to codeconnect.sh; the app's store listing links to them, and Apple's reviewer clicks both.

## What to deploy

| URL (exact, permanent) | Source file | Notes |
|---|---|---|
| `https://codeconnect.sh/privacy` | `privacy.md` | Deploy content **verbatim** — its statements are privacy claims that must match app behavior; do not edit wording without checking with the app side |
| `https://codeconnect.sh/support` | `support.md` | All CLI commands in it are verified against the shipped `codeconnect` binary |

## Requirements

- Plain readable pages (any styling), **no login**, work on a phone.
- Must return HTTP 200 before we submit to Apple — a 404 or placeholder page is an App Store rejection reason ("empty websites... will be rejected", guideline 2.1).
- URLs must stay stable permanently once submitted; the App Store listing hard-links them.
- The existing `curl … | sh` install endpoint at the domain root is unaffected — keep it exactly as is.

## Coming next (heads-up, no action yet)

`demo.codeconnect.sh` will need its DNS pointed at a Render.com service that hosts the reviewer-facing demo server (currently the subdomain resolves to a parked target and serves nothing). Exact CNAME target follows once the service exists.

## Contact shown on the pages

Both pages point at GitHub: the profile (`github.com/faisalmumtaz89`) on the privacy page, the repository's issues on the support page, and GitHub's private vulnerability reporting for security problems. No email address appears on either page; if that changes, tell the app side so the App Store support contact matches.

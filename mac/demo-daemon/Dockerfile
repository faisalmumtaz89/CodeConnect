# The demo daemon, built for Linux.
#
# Build context is `mac/`, not the repository root: this crate embeds its own
# script and diff, so nothing outside the Rust workspace is needed to produce a
# working image.
#
#   docker build -f demo-daemon/Dockerfile -t codeconnect-demo mac/
#
# The image serves plain `ws` on $PORT and expects the deployment to terminate
# TLS in front of it. `REVIEW_TOKEN` is required; the process refuses to start
# without one rather than serving a fleet nothing authenticates for.

# The workspace's own `rust-version` floor, so the image builds what CI tests
# rather than whatever `latest` happens to be.
FROM rust:1.85-slim-bookworm AS build
WORKDIR /src
COPY . .
# `--locked` so the image is built from the checked-in lockfile and a dependency
# cannot move underneath a deploy.
RUN cargo build --release --locked -p demo-daemon

# No toolchain, no source, no shell tooling in the thing that faces the network.
FROM debian:bookworm-slim
COPY --from=build /src/target/release/demo-daemon /usr/local/bin/demo-daemon
# It writes nothing, reads nothing but `/dev/urandom`, and runs nothing.
USER nobody
CMD ["demo-daemon"]

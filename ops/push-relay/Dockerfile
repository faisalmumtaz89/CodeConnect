# The push relay, built for Linux.
#
# Build context is `mac/`, not the repository root: the relay is a member of the
# Rust workspace, so the workspace manifest and every path dependency it
# resolves have to be inside the context, and nothing outside it is needed.
#
#   docker build -f ops/push-relay/Dockerfile -t codeconnect-push-relay mac/
#
# The process reads `PORT` from the environment and binds `0.0.0.0:$PORT`. It
# serves plain HTTP and expects the deployment to terminate TLS in front of it.
# Every secret it reads is a file mounted at run time; none is baked in here.

# The workspace's own `rust-version` floor, so the image builds what CI tests
# rather than whatever `latest` happens to be. Pinned by digest as well as tag,
# because a tag is a moving pointer: a deploy pinned to a git SHA that quietly
# builds on a different base image is not pinned to anything.
FROM rust:1.85-slim-bookworm@sha256:9f841bbe9e7d8e37ceb96ed907265a3a0df7f44e3737d0b100e7907a679acb36 AS build
WORKDIR /src
COPY . .
# No extra build packages: this image already carries `gcc` and `libc6-dev`,
# which is the whole of what the `cc` crate needs for `ring` and for the
# bundled SQLite amalgamation. Both compile C, not C++, so `build-essential`
# would add minutes and packages for nothing.
#
# `--locked` so the image is built from the checked-in lockfile and a
# dependency cannot move underneath a deploy.
RUN cargo build --release --locked -p push-relay

# No toolchain, no source, no shell tooling in the thing that faces the network.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241
# The relay opens outbound TLS to APNs and verifies Apple's chain against the
# platform trust store. A base image without this package has no trust store at
# all, so every push would fail at the handshake rather than at a bad token.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/push-relay /usr/local/bin/push-relay
# Not root, and not `nobody` either. The deployment mounts the APNs keys and
# the other secrets as files readable by group 1000; a process outside that
# group cannot open them, and the relay would sit in its keys-absent state
# forever while looking correctly configured. Running as root would read them
# and hand the network everything else as well.
RUN groupadd --gid 1000 relay \
 && useradd --uid 1000 --gid 1000 --no-create-home --shell /usr/sbin/nologin relay
USER relay
# The SQLite database lives on a persistent disk mounted at `/var/data` at run
# time. The image deliberately does not create that directory: a mount replaces
# whatever is at the path, so creating it here would only hide a missing volume
# until the first restart took the database with it.
CMD ["push-relay"]

# The toolchain is pinned twice: the tag names 1.100 and the digest fixes the
# exact index, so a moved tag cannot change the builder.
FROM docker.io/library/rust:1.100-bookworm@sha256:1b9dc70890ec494e466c87f5ad41ad544bfb30ea6acc99a3e068c47a4ae3f23e AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml deny.toml ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release --locked

# The runtime stage adds no packages to the slim base, creates no user, and
# copies no account database; it carries the binary and the license, and
# nothing else.
FROM docker.io/library/debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e

COPY --from=build /src/target/release/canary /usr/local/bin/canary

# The MIT notice must accompany the binary wherever it is distributed:
# Cargo.toml declares `license = "MIT"`, so the image carries the LICENSE file
# beside the binary.
COPY LICENSE /usr/local/share/canary/LICENSE

# A numeric UID needs no passwd entry, so the image creates no user and copies
# no /etc/passwd. 10001 sits outside Debian's system-account range, so it cannot
# collide with a packaged account.
USER 10001:10001

# No `PORT` default is set: an absent `PORT` is rejected configuration (`78`),
# so the image fails like the bare binary instead of serving on a guessed port.
# `EXPOSE` only documents the port the image is meant to be given.
EXPOSE 8080

# The probe is bounded by 1 s connect plus 1 s read, so --timeout covers one
# whole probe. --start-period covers the bind window: a probe before the
# listener exists exits 1, never 78, because the probe owns its own zero/one
# status. The command is exactly --health-check with no extra arguments: any
# other invocation shape is rejected with 78 and must not be read as unhealthy.
# No PORT default exists, so this check passes only for a container the operator
# started with PORT supplied.
# podman's default OCI image format drops this instruction, so build with
# `--format docker` as the README command does.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/canary", "--health-check"]

ENTRYPOINT ["/usr/local/bin/canary"]

# The toolchain is pinned twice: the tag names 1.100 and the digest fixes the
# exact index, so a moved tag cannot change the builder.
FROM docker.io/library/rust:1.100-bookworm@sha256:1b9dc70890ec494e466c87f5ad41ad544bfb30ea6acc99a3e068c47a4ae3f23e AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml deny.toml ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release --locked

# The runtime image carries the binary and nothing else: no apt step, no shell
# tools, and no account database.
FROM docker.io/library/debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e

COPY --from=build /src/target/release/canary /usr/local/bin/canary

# A numeric UID needs no passwd entry, so the image creates no user and copies
# no /etc/passwd. 10001 sits outside Debian's system-account range, so it cannot
# collide with a packaged account.
USER 10001:10001

ENV PORT=8080
EXPOSE 8080

# The probe is bounded by 1 s connect plus 1 s read, so --timeout covers one
# whole probe. --start-period covers the bind window: a probe before the
# listener exists exits 1, never 78, because the probe owns its own zero/one
# status. The command is exactly --health-check with no extra arguments: any
# other invocation shape is rejected with 78 and must not be read as unhealthy.
# podman's default OCI image format drops this instruction, so build with
# `--format docker` as the README command does.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/canary", "--health-check"]

ENTRYPOINT ["/usr/local/bin/canary"]

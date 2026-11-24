# Canary

A single-endpoint liveness probe. One HTTP listener answers exactly
`Canary <version> is alive!\r\n` (25 bytes at `0.50.0`) on `/` and nothing else,
so a supervisor can tell a live process from a loaded one with one request.

AloxStyle: no async runtime, one owned reactor, bounded work per turn, a fixed
connection table, and a bounded log queue. The normal dependency graph holds
four crates: `libc`, `mio`, `signal-hook`, and its `signal-hook-registry`.

## Replies

| Method | Target | Reply |
| --- | --- | --- |
| `GET` | `/` | `200` with the liveness body |
| `HEAD` | `/` | `200` with the headers and no body |
| `GET`, `HEAD` | any target other than `/` | `404`, headers only for `HEAD` |
| any other method | any target | `405` with `Allow: GET, HEAD` |
| any | a malformed request line, or a partial head whose sender closed before the terminator | `400` |
| any | a head that cannot be terminated within `1024` bytes | `431` |

The head is judged before the method and target are dispatched, so a `400` or
`431` outranks the method and target rows, and `405` outranks the `404` for a
target other than `/`. A connection that sends nothing, or whose request is
still unfinished when the connection deadline passes, is closed without a
reply. A `GET /` reply carries the body `Canary <version> is alive!`,
CRLF-terminated, and `Content-Length` names its exact byte count; a `HEAD /`
reply carries the same headers, `Content-Length` included, and no body.

## `PORT`

`PORT` is the only fatal input and has no default: the binary rejects an absent
`PORT` with `78` before it binds, and the image sets no default either.

- Absent, non-numeric, or outside `1..=65535`: the process exits `78` before it
  binds.
- `0`: the kernel chooses the port, and the startup record reports it.
- Any other value: the listener binds `0.0.0.0:<PORT>`.

The health probe dials `127.0.0.1:<PORT>`, so `PORT=0` cannot be probed: it
answers `1` with `PORT is zero, so the listener's port is chosen at bind time
and cannot be probed`. The image supplies no default, so the `HEALTHCHECK` it
declares passes only when the operator supplies `PORT`.

## Records

`RUST_LOG` selects the severity filter: exactly `off`, `error`, `warn`, or
`info`. An absent variable defaults to `info` and prints no notice. Every
record is one line:

```text
level=<token> kind=<kind>[ <key>=<value>]...\r\n
```

| Kind | Line |
| --- | --- |
| `startup` | `level=info kind=startup bind=0.0.0.0:8080` |
| `tick` | `level=info kind=tick served=0 rejected=0 dropped=0` |
| `shutdown` | `level=info kind=shutdown served=0 rejected=0 dropped=0` |
| `sink_failure` | `level=error kind=sink_failure failures=1` |
| `filter_default` | `level=warn kind=filter_default` |

- `startup` is printed once after the bind and names the address the kernel
  actually bound.
- `tick` reports the counters at most once per second, and only when they move.
- `shutdown` is the final record after a requested `SIGTERM` or `SIGINT`.
- `sink_failure` means diagnostics were lost while the service kept serving:
  the drain was refused, so external visibility degraded but the listener did
  not. It survives every filter except `off`.
- `filter_default` appears only when `RUST_LOG` supplied a value that names no
  filter, and only at the start.

## Exit status

Serving:

| Status | Meaning |
| --- | --- |
| `0` | requested shutdown (`SIGTERM` or `SIGINT`) |
| `78` | rejected configuration: `PORT`, the bind, or standard output |
| `70` | an internal signal, queue, or reactor step failed |

Probe (`--health-check`):

| Status | Meaning |
| --- | --- |
| `0` | the listener answered an `HTTP/1.1 200` status line |
| `1` | it did not, and one stderr line names the cause |

`143` (a shutdown signal that arrives before the handlers are installed) and
`101` (a panic) sit outside both sets.

## Build, test, run

```sh
cargo build --locked
cargo build --release --locked
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check

PORT=8080 cargo run --release
curl -i http://127.0.0.1:8080/
PORT=8080 cargo run --release -- --health-check
```

## Container

The image is a pinned two-stage build: `rust:1.100-bookworm` compiles with
`cargo build --release --locked`, and `debian:bookworm-slim` carries the binary
and its MIT license, and runs as UID `10001`.

```sh
podman build --format docker -t canary .
podman run --rm -e PORT=8080 -p 8080:8080 canary
docker run --rm -e PORT=8080 -p 8080:8080 canary
podman inspect --format '{{.State.Health.Status}}' <container>
```

The image sets no `PORT` default, so the run line is exactly
`-e PORT=8080 -p 8080:8080`: a container started without `-e PORT` exits `78`
before it binds, and the declared health check therefore passes only for a
container that was given `PORT`.

`--format docker` is required: podman's default OCI image format drops the
`HEALTHCHECK` instruction. The image declares the health check, so
`podman run --health-cmd` can be omitted. When it is given it must spell
exactly `/usr/local/bin/canary --health-check`: any other invocation shape is
a rejected invocation (`78`), not an unhealthy probe. The probe adds no extra
arguments, and `--start-period`/`--timeout` cover the bind window, where a
probe before the listener exists answers `1`.

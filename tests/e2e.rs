//! End-to-end behaviour of the spawned canary binary.
//!
//! Each test starts the real binary with an emptied environment plus only the
//! inputs it chooses, and observes what the operating system exposes: the lines
//! the process prints, the bytes it answers with, and the status it exits with.
//! Every process is killed when its guard is dropped, and every read carries a
//! deadline, so no test outlives a child that stops responding.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Error, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// The deadline one test allows any single observable.
const DEADLINE: Duration = Duration::from_secs(5);

/// The deadline a signalled process is allowed to exit within.
const EXIT_DEADLINE: Duration = Duration::from_secs(2);

/// The attempts a silent start allows for losing its claimed port to another
/// process before the child binds.
const SILENT_ATTEMPTS: usize = 3;

/// The wait a tick test allows for the first aggregate record after a request.
///
/// The reactor emits at most one tick per second, and its first tick is due as
/// soon as the counters move, so this bound covers the interval plus a margin.
const TICK_WAIT: Duration = Duration::from_millis(1200);

/// The liveness body the binary must answer with, built from the build version.
const LIVENESS: &[u8] = concat!("Canary ", env!("CARGO_PKG_VERSION"), " is alive!\r\n").as_bytes();

/// The `Content-Length` line the liveness reply must declare.
fn liveness_length_line() -> Vec<u8> {
    format!("Content-Length: {}\r\n", LIVENESS.len()).into_bytes()
}

/// The packages `Cargo.lock` may not name.
const FORBIDDEN_PACKAGES: [&str; 6] = [
    "tokio",
    "actix-web",
    "actix-http",
    "actix-rt",
    "env_logger",
    "log",
];

/// The dependencies `Cargo.toml` must declare, exact-pinned.
const DECLARED_DEPENDENCIES: [&str; 3] = ["libc", "mio", "signal-hook"];

/// The binary under test, with an emptied environment and piped streams.
fn command(env: &[(&str, &OsStr)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_canary"));
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &(name, value) in env {
        command.env(name, value);
    }
    command
}

/// The lines `stream` prints, each delivered whole with its line terminator.
fn pump(stream: impl Read + Send + 'static) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    let _ = thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            let Ok(read) = reader.read_line(&mut line) else {
                break;
            };
            if read == 0 {
                break;
            }
            if sender.send(line.clone()).is_err() {
                break;
            }
        }
    });
    receiver
}

/// The time left until `deadline`, saturating at zero.
fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// The next line the stream prints, or `None` once it has closed.
fn next_line(receiver: &Receiver<String>, deadline: Instant) -> Option<String> {
    match receiver.recv_timeout(remaining(deadline)) {
        Ok(line) => Some(line),
        Err(RecvTimeoutError::Disconnected) => None,
        Err(RecvTimeoutError::Timeout) => {
            panic!("the stream printed no line before the {DEADLINE:?} deadline")
        }
    }
}

/// Every line the stream prints until it closes.
fn lines_until_closed(receiver: &Receiver<String>, deadline: Instant) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(line) = next_line(receiver, deadline) {
        lines.push(line);
    }
    lines
}

/// The port named by the `bind=0.0.0.0:<port>` field of a startup line.
fn startup_port(line: &str) -> u16 {
    let field = line
        .trim_end()
        .split(' ')
        .find(|field| field.starts_with("bind="))
        .expect("the startup line names the bound address");
    let address = field
        .strip_prefix("bind=0.0.0.0:")
        .expect("the listener binds every IPv4 address");
    address.parse().expect("the bound port is decimal")
}

/// Waits up to `deadline` for `child` to exit.
fn wait_bounded(child: &mut Child, deadline: Instant) -> ExitStatus {
    loop {
        if let Some(status) = child.try_wait().expect("the child status is readable") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the child did not exit before its deadline"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// The value of the `served=` field of a record line.
fn served_count(line: &str) -> u64 {
    line.split(' ')
        .find_map(|field| field.strip_prefix("served="))
        .expect("the record names its served count")
        .parse()
        .expect("the served count is decimal")
}

/// The quoted version specification a dependency entry names.
fn pinned_version(entry: &str) -> &str {
    let value = entry
        .split_once(" = ")
        .expect("a dependency entry is an assignment")
        .1;
    let spec = match value.strip_prefix('"') {
        Some(rest) => rest,
        None => {
            value
                .split_once("version = \"")
                .expect("a table entry names its version")
                .1
        }
    };
    let end = spec.find('"').expect("the version specification is quoted");
    &spec[..end]
}

/// Whether `spec` is an exact `=X.Y.Z` pin, with three numeric components.
///
/// A major-only or major-minor pin such as `=1.2` lets the resolver choose a
/// different patch release, so it is not an exact pin.
fn is_exact_pin(spec: &str) -> bool {
    let Some(version) = spec.strip_prefix('=') else {
        return false;
    };
    let components: Vec<&str> = version.split('.').collect();
    components.len() == 3
        && components.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Whether `haystack` holds `needle` as a contiguous run of bytes.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The body of a reply, which begins after the empty header line.
fn body_of(reply: &[u8]) -> &[u8] {
    let split = reply
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the reply carries a header terminator")
        + 4;
    &reply[split..]
}

/// Sends `head` to the server and returns every reply byte it answers with.
fn request(port: u16, head: &[u8]) -> Vec<u8> {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).expect("the server accepts a connection");
    stream
        .set_read_timeout(Some(DEADLINE))
        .expect("the reply read is bounded");
    stream
        .write_all(head)
        .expect("the request reaches the server");
    let mut received = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => received.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == ErrorKind::ConnectionReset => break,
            Err(error) => panic!("the client could not read the reply: {error}"),
        }
    }
    received
}

/// Claims a loopback port and releases it for the child to bind.
fn claim_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("the test claims a loopback port");
    listener
        .local_addr()
        .expect("the claimed port is readable")
        .port()
}

/// A running canary process, killed when the value is dropped.
struct Canary {
    child: Child,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
    port: Option<u16>,
}

impl Canary {
    /// Spawns the binary with exactly `env` set and no arguments.
    fn spawn(env: &[(&str, &OsStr)]) -> Self {
        Self::spawn_with(env, &[])
    }

    /// Spawns the binary with exactly `env` set and `args` appended.
    fn spawn_with(env: &[(&str, &OsStr)], args: &[&str]) -> Self {
        let mut child = command(env)
            .args(args)
            .spawn()
            .expect("the canary binary starts");
        let stdout = pump(child.stdout.take().expect("standard output is piped"));
        let stderr = pump(child.stderr.take().expect("standard error is piped"));
        Self {
            child,
            stdout,
            stderr,
            port: None,
        }
    }

    /// Spawns the binary and waits for the startup line that names its port.
    fn start(env: &[(&str, &OsStr)]) -> Self {
        let deadline = Instant::now() + DEADLINE;
        let mut canary = Self::spawn(env);
        let port = loop {
            let line = canary
                .next_stdout(deadline)
                .expect("the process closed standard output before its startup line");
            if line.contains("kind=startup") {
                break startup_port(&line);
            }
        };
        canary.port = Some(port);
        canary
    }

    /// Spawns the binary that prints no startup line and waits for the reactor.
    ///
    /// The filter these tests set suppresses the startup line, so the listener
    /// is the readiness oracle instead: the process installs its shutdown
    /// handlers before it binds, so a connection the listener accepts proves the
    /// signal would reach a live reactor rather than a process still starting.
    /// The claimed port is released before the child spawns, so the connection
    /// may reach a concurrent process's listener instead; a caller that signals
    /// the child afterwards retries that lost claim. `Err` carries the lost
    /// claim's diagnostics; any other startup death panics.
    fn start_silently(env: &[(&str, &OsStr)]) -> Result<Self, String> {
        let port = claim_port();
        let port_text = port.to_string();
        let mut full_env = Vec::from(env);
        full_env.push(("PORT", OsStr::new(&port_text)));
        let mut canary = Self::spawn(&full_env);
        canary.port = Some(port);
        canary.await_accept().map(|()| canary)
    }

    /// The loopback address the readiness poll dials.
    fn probe_address(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port()))
    }

    /// Polls the claimed port until the reactor accepts a connection.
    ///
    /// Each attempt carries the time left before [`DEADLINE`], so a foreign
    /// listener with a full accept queue cannot block the poll past the
    /// deadline. `Ok(())` means the port accepted the connection. `Err` carries
    /// the readiness diagnostic when the child exited `78` without a listener of
    /// its own, which [`lost_claim_before_ready`] attributes to the released
    /// claim and the caller retries. Every other exit panics with the same
    /// diagnostic: no signal of this test has reached the child yet, so a signal
    /// death here is a real startup failure. A child that stays alive without
    /// accepting is the deadline failure and panics the same way.
    fn await_accept(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + DEADLINE;
        let mut last_error = None;
        loop {
            let left = remaining(deadline);
            if !left.is_zero() {
                match TcpStream::connect_timeout(&self.probe_address(), left) {
                    Ok(_) => return Ok(()),
                    Err(error) => last_error = Some(error),
                }
            }
            let status = self.child.try_wait().expect("the child status is readable");
            if let Some(status) = status {
                let failure = self.readiness_failure(Some(status), last_error.as_ref());
                if lost_claim_before_ready(status) {
                    return Err(failure);
                }
                panic!("{failure}");
            }
            assert!(
                Instant::now() < deadline,
                "{}",
                self.readiness_failure(None, last_error.as_ref())
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The readiness diagnostic for a child that did not accept a connection.
    fn readiness_failure(&self, status: Option<ExitStatus>, last_error: Option<&Error>) -> String {
        let signal = status.as_ref().and_then(ExitStatus::signal);
        let stdout: Vec<String> = self.stdout.try_iter().collect();
        let stderr: Vec<String> = self.stderr.try_iter().collect();
        format!(
            "the reactor accepted no connection before {DEADLINE:?}: \
             last_error={last_error:?} exited={status:?} signal={signal:?} \
             stdout={stdout:?} stderr={stderr:?}"
        )
    }

    /// The port the startup line named.
    fn port(&self) -> u16 {
        self.port.expect("the startup line named the bound port")
    }

    /// Sends `signal` to the process.
    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status()
            .expect("the kill command runs");
        assert!(status.success(), "the process accepts SIG{signal}");
    }

    /// Waits up to `deadline` for the process to exit.
    fn wait(&mut self, deadline: Instant) -> ExitStatus {
        wait_bounded(&mut self.child, deadline)
    }

    /// The next startup-or-later line the process prints.
    fn next_stdout(&self, deadline: Instant) -> Option<String> {
        next_line(&self.stdout, deadline)
    }

    /// The next standard-output line if one arrives before `deadline`.
    fn next_stdout_within(&self, deadline: Instant) -> Option<String> {
        self.stdout.recv_timeout(remaining(deadline)).ok()
    }

    /// Every remaining standard-output line until the process closes it.
    fn stdout_until_closed(&self, deadline: Instant) -> Vec<String> {
        lines_until_closed(&self.stdout, deadline)
    }

    /// Every standard-error line until the process closes it.
    fn stderr_until_closed(&self, deadline: Instant) -> Vec<String> {
        lines_until_closed(&self.stderr, deadline)
    }
}

impl Drop for Canary {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A process that ran to exit without being signalled.
struct Finished {
    status: ExitStatus,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

/// Runs the binary with no arguments to exit within [`DEADLINE`].
fn run_to_exit(env: &[(&str, &OsStr)]) -> Finished {
    run_args_to_exit(env, &[])
}

/// Runs the binary with `args` to exit within [`DEADLINE`] and collects both
/// streams.
fn run_args_to_exit(env: &[(&str, &OsStr)], args: &[&str]) -> Finished {
    let mut canary = Canary::spawn_with(env, args);
    let status = canary.wait(Instant::now() + DEADLINE);
    let deadline = Instant::now() + DEADLINE;
    Finished {
        status,
        stdout: canary.stdout_until_closed(deadline),
        stderr: canary.stderr_until_closed(deadline),
    }
}

/// Starts the binary silently, requests shutdown, and collects what it printed.
///
/// `Err` means the silent start lost its claimed port before the reactor was
/// ready, carrying that attempt's readiness diagnostic; the caller retries. The
/// shutdown request is only sent to a child that accepted the readiness
/// connection.
fn silence_until_shutdown(filter: &str) -> Result<Finished, String> {
    let env = [("RUST_LOG", OsStr::new(filter))];
    let mut canary = Canary::start_silently(&env)?;
    canary.signal("TERM");
    let status = canary.wait(Instant::now() + EXIT_DEADLINE);
    let deadline = Instant::now() + DEADLINE;
    Ok(Finished {
        status,
        stdout: canary.stdout_until_closed(deadline),
        stderr: canary.stderr_until_closed(deadline),
    })
}

/// Whether a silent start lost its claimed port before it reached readiness.
///
/// [`Canary::start_silently`] releases its claim before the child spawns, so a
/// concurrent process may bind that port first and the child then exits `78`,
/// naming the bind as rejected configuration. `78` is the only status a caller
/// retries: a signal death or any other status this early is a real startup
/// failure, and the caller reports it with the diagnostic it carries.
fn lost_claim_before_ready(status: ExitStatus) -> bool {
    status.code() == Some(78)
}

/// Whether a signalled silent start lost its claimed port instead of shutting
/// down.
///
/// By the time the caller signals the child, the readiness connection has been
/// accepted, so a foreign listener may have answered it while the child was
/// still starting. The shutdown signal then either kills the child before its
/// handlers exist — the operating system reports a signal rather than a status
/// — or reaches a child whose own bind failed with `78`. Both outcomes belong to
/// the lost claim; every other outcome belongs to the filter under test.
fn lost_claim_after_signal(status: ExitStatus) -> bool {
    matches!(status.code(), None | Some(78))
}

#[test]
fn the_liveness_body_is_the_frozen_twenty_five_bytes() {
    let expected = format!("Canary {} is alive!\r\n", env!("CARGO_PKG_VERSION"));
    assert_eq!(LIVENESS, expected.as_bytes());
    assert_eq!(LIVENESS.len(), 25, "{}", String::from_utf8_lossy(LIVENESS));
}

#[test]
fn the_startup_line_ends_with_a_carriage_return_line_feed() {
    let canary = Canary::spawn(&[("PORT", OsStr::new("0"))]);
    let line = canary
        .next_stdout(Instant::now() + DEADLINE)
        .expect("the process printed its startup line");
    assert!(line.contains("kind=startup"), "{line:?}");
    assert!(line.ends_with("\r\n"), "{line:?}");
    assert_eq!(
        line.trim_end(),
        &line[..line.len() - 2],
        "the line terminator is exactly one CRLF: {line:?}"
    );
}

#[test]
fn the_root_replies_with_the_exact_liveness_body() {
    let canary = Canary::start(&[("PORT", OsStr::new("0"))]);
    let reply = request(
        canary.port(),
        b"GET / HTTP/1.1\r\nHost: canary\r\nConnection: close\r\n\r\n",
    );
    assert!(
        reply.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        String::from_utf8_lossy(&reply)
    );
    assert!(
        contains(&reply, &liveness_length_line()),
        "{}",
        String::from_utf8_lossy(&reply)
    );
    assert_eq!(body_of(&reply), LIVENESS);
}

#[test]
fn the_head_reply_carries_the_headers_without_a_body() {
    let canary = Canary::start(&[("PORT", OsStr::new("0"))]);
    let reply = request(
        canary.port(),
        b"HEAD / HTTP/1.1\r\nHost: canary\r\nConnection: close\r\n\r\n",
    );
    assert!(
        reply.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        String::from_utf8_lossy(&reply)
    );
    assert!(
        contains(&reply, &liveness_length_line()),
        "{}",
        String::from_utf8_lossy(&reply)
    );
    assert_eq!(body_of(&reply), b"");
}

#[test]
fn other_targets_and_methods_are_refused() {
    let canary = Canary::start(&[("PORT", OsStr::new("0"))]);
    let not_found = request(
        canary.port(),
        b"GET /nope HTTP/1.1\r\nHost: canary\r\nConnection: close\r\n\r\n",
    );
    assert!(
        not_found.starts_with(b"HTTP/1.1 404 Not Found\r\n"),
        "{}",
        String::from_utf8_lossy(&not_found)
    );
    let not_allowed = request(
        canary.port(),
        b"POST / HTTP/1.1\r\nHost: canary\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(
        not_allowed.starts_with(b"HTTP/1.1 405 Method Not Allowed\r\n"),
        "{}",
        String::from_utf8_lossy(&not_allowed)
    );
    assert!(
        contains(&not_allowed, b"Allow: GET, HEAD\r\n"),
        "{}",
        String::from_utf8_lossy(&not_allowed)
    );
}

#[test]
fn a_served_request_is_followed_by_an_aggregate_tick() {
    let canary = Canary::start(&[("PORT", OsStr::new("0"))]);
    let reply = request(
        canary.port(),
        b"GET / HTTP/1.1\r\nHost: canary\r\nConnection: close\r\n\r\n",
    );
    assert!(
        reply.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        String::from_utf8_lossy(&reply)
    );
    let deadline = Instant::now() + TICK_WAIT;
    let mut tick = None;
    while let Some(line) = canary.next_stdout_within(deadline) {
        if line.contains("kind=tick") {
            tick = Some(line);
            break;
        }
    }
    let tick = tick.expect("the reactor reported a tick after serving a request");
    assert!(served_count(&tick) >= 1, "{tick}");
}

#[test]
fn a_bad_or_absent_port_is_rejected_with_the_config_status() {
    let cases: [(&str, Option<&str>); 5] = [
        ("an absent PORT", None),
        ("an empty PORT", Some("")),
        ("a non-numeric PORT", Some("abc")),
        ("an out-of-range PORT", Some("70000")),
        ("a negative PORT", Some("-1")),
    ];
    for (case, port) in cases {
        let mut env: Vec<(&str, &OsStr)> = Vec::new();
        if let Some(value) = port {
            env.push(("PORT", OsStr::new(value)));
        }
        let finished = run_to_exit(&env);
        let stderr = finished.stderr.join("\n");
        assert_eq!(finished.status.code(), Some(78), "{case}");
        assert!(stderr.contains("PORT"), "{case}: {stderr}");
        assert!(finished.stdout.is_empty(), "{case}: {:?}", finished.stdout);
    }
}

#[test]
fn a_health_check_against_a_live_listener_succeeds_without_booting() {
    let canary = Canary::start(&[("PORT", OsStr::new("0"))]);
    let port = canary.port().to_string();
    let finished = run_args_to_exit(&[("PORT", OsStr::new(&port))], &["--health-check"]);
    assert_eq!(finished.status.code(), Some(0), "{:?}", finished.stderr);
    assert!(finished.stdout.is_empty(), "{:?}", finished.stdout);
    assert!(finished.stderr.is_empty(), "{:?}", finished.stderr);
}

#[test]
fn a_health_check_against_a_closed_port_fails_with_one_line() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("the test holds a loopback port");
    let port = listener
        .local_addr()
        .expect("the held port is readable")
        .port()
        .to_string();
    let finished = run_args_to_exit(&[("PORT", OsStr::new(&port))], &["--health-check"]);
    drop(listener);
    assert_eq!(finished.status.code(), Some(1), "{:?}", finished.stderr);
    assert!(finished.stdout.is_empty(), "{:?}", finished.stdout);
    assert_eq!(finished.stderr.len(), 1, "{:?}", finished.stderr);
    assert!(
        finished.stderr[0].starts_with("canary: health check: "),
        "{:?}",
        finished.stderr
    );
    assert!(
        finished.stderr[0].contains("cannot read the probe reply"),
        "{:?}",
        finished.stderr
    );
}

#[test]
fn a_health_check_without_a_port_names_it_and_fails() {
    let finished = run_args_to_exit(&[], &["--health-check"]);
    assert_eq!(finished.status.code(), Some(1), "{:?}", finished.stderr);
    assert!(finished.stdout.is_empty(), "{:?}", finished.stdout);
    assert_eq!(finished.stderr.len(), 1, "{:?}", finished.stderr);
    assert!(finished.stderr[0].contains("PORT"), "{:?}", finished.stderr);
}

#[test]
fn a_health_check_with_an_ephemeral_port_fails() {
    let finished = run_args_to_exit(&[("PORT", OsStr::new("0"))], &["--health-check"]);
    assert_eq!(finished.status.code(), Some(1), "{:?}", finished.stderr);
    assert!(finished.stdout.is_empty(), "{:?}", finished.stdout);
    assert_eq!(finished.stderr.len(), 1, "{:?}", finished.stderr);
    assert!(
        finished.stderr[0].contains("cannot be probed"),
        "{:?}",
        finished.stderr
    );
}

#[test]
fn an_unknown_invocation_is_rejected_with_the_config_status() {
    let cases: [&[&str]; 3] = [&["--health-check", "--extra"], &["--serve"], &[""]];
    for args in cases {
        let finished = run_args_to_exit(&[("PORT", OsStr::new("0"))], args);
        assert_eq!(finished.status.code(), Some(78), "{args:?}");
        assert!(
            finished.stdout.is_empty(),
            "{args:?}: {:?}",
            finished.stdout
        );
        assert_eq!(finished.stderr.len(), 1, "{args:?}: {:?}", finished.stderr);
        assert!(
            finished.stderr[0].contains("usage"),
            "{args:?}: {:?}",
            finished.stderr
        );
    }
}

#[test]
fn an_exact_pin_names_three_numeric_components() {
    assert!(is_exact_pin("=0.2.191"));
    assert!(is_exact_pin("=1.2.3"));
    for rejected in [
        "0.2.191", "=1.2", "=1", "=1.2.3.4", "=^1.2.3", "=1.2.x", "= 1.2.3", "=", "=1..3", "=1.2.",
    ] {
        assert!(!is_exact_pin(rejected), "{rejected:?}");
    }
}

#[test]
fn a_shutdown_signal_ends_the_process_cleanly() {
    for signal in ["TERM", "INT"] {
        let mut canary = Canary::start(&[("PORT", OsStr::new("0"))]);
        canary.signal(signal);
        let status = canary.wait(Instant::now() + EXIT_DEADLINE);
        assert_eq!(status.code(), Some(0), "SIG{signal}");
        let deadline = Instant::now() + DEADLINE;
        let lines = canary.stdout_until_closed(deadline);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("kind=shutdown"))
                .count(),
            1,
            "SIG{signal}: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("kind=startup")),
            "SIG{signal}: {lines:?}"
        );
        let stderr = canary.stderr_until_closed(deadline);
        assert!(stderr.is_empty(), "SIG{signal}: {stderr:?}");
    }
}

#[test]
fn a_silencing_filter_prints_nothing_and_still_exits_cleanly() {
    for filter in ["off", "warn"] {
        let mut outcome = silence_until_shutdown(filter);
        for _ in 1..SILENT_ATTEMPTS {
            let retry = match &outcome {
                Err(_) => true,
                Ok(finished) => lost_claim_after_signal(finished.status),
            };
            if !retry {
                break;
            }
            outcome = silence_until_shutdown(filter);
        }
        let Finished {
            status,
            stdout,
            stderr,
        } = match outcome {
            Ok(finished) => finished,
            Err(lost) => panic!(
                "RUST_LOG={filter}: no silent start reached readiness within {SILENT_ATTEMPTS} \
                 attempts: {lost}"
            ),
        };
        assert_eq!(
            status.code(),
            Some(0),
            "RUST_LOG={filter}: status={status:?} stdout={stdout:?} stderr={stderr:?}"
        );
        assert!(stdout.is_empty(), "RUST_LOG={filter}: {stdout:?}");
        assert!(stderr.is_empty(), "RUST_LOG={filter}: {stderr:?}");
    }
}

#[test]
fn an_unrecognised_filter_earns_exactly_one_notice() {
    let env = [
        ("PORT", OsStr::new("0")),
        ("RUST_LOG", OsStr::new("garbage")),
    ];
    let mut canary = Canary::start(&env);
    let deadline = Instant::now() + DEADLINE;
    assert_eq!(
        canary.next_stdout(deadline).as_deref().map(str::trim_end),
        Some("level=warn kind=filter_default")
    );
    canary.signal("TERM");
    assert_eq!(canary.wait(Instant::now() + EXIT_DEADLINE).code(), Some(0));
    let lines = canary.stdout_until_closed(Instant::now() + DEADLINE);
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("kind=filter_default")),
        "{lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("kind=shutdown"))
            .count(),
        1,
        "{lines:?}"
    );
}

#[test]
fn a_non_unicode_filter_earns_the_notice_rather_than_silence() {
    let invalid = OsString::from_vec(vec![0xff]);
    let env = [("PORT", OsStr::new("0")), ("RUST_LOG", invalid.as_os_str())];
    let mut canary = Canary::start(&env);
    let deadline = Instant::now() + DEADLINE;
    assert_eq!(
        canary.next_stdout(deadline).as_deref().map(str::trim_end),
        Some("level=warn kind=filter_default")
    );
    canary.signal("TERM");
    assert_eq!(canary.wait(Instant::now() + EXIT_DEADLINE).code(), Some(0));
    let lines = canary.stdout_until_closed(Instant::now() + DEADLINE);
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("kind=filter_default")),
        "{lines:?}"
    );
}

#[test]
fn the_lock_graph_and_manifest_hold_only_the_declared_dependencies() {
    let lock: &str = include_str!("../Cargo.lock");
    let packages: Vec<&str> = lock
        .lines()
        .filter_map(|line| line.trim().strip_prefix("name = \""))
        .filter_map(|name| name.strip_suffix('"'))
        .collect();
    assert!(!packages.is_empty(), "the lock file names its packages");
    for forbidden in FORBIDDEN_PACKAGES {
        assert!(
            !packages.contains(&forbidden),
            "{forbidden} is in Cargo.lock: {packages:?}"
        );
    }

    let manifest: &str = include_str!("../Cargo.toml");
    let section = manifest
        .split_once("[dependencies]")
        .expect("the manifest declares a dependency table")
        .1;
    let mut entries: Vec<&str> = Vec::new();
    for line in section.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        if !line.is_empty() {
            entries.push(line);
        }
    }
    let names: BTreeSet<&str> = entries
        .iter()
        .map(|entry| {
            entry
                .split_once(" = ")
                .expect("a dependency entry is an assignment")
                .0
        })
        .collect();
    let declared: BTreeSet<&str> = DECLARED_DEPENDENCIES.into_iter().collect();
    assert_eq!(names, declared, "{entries:?}");
    for entry in &entries {
        let spec = pinned_version(entry);
        assert!(is_exact_pin(spec), "{entry} is not exact-pinned");
    }
}

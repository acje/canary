//! End-to-end behaviour of the spawned canary binary.
//!
//! Each test starts the real binary with an emptied environment plus only the
//! inputs it chooses, and observes what the operating system exposes: the lines
//! the process prints, the bytes it answers with, and the status it exits with.
//! Every process is killed when its guard is dropped, and every read carries a
//! deadline, so no test outlives a child that stops responding.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::os::unix::ffi::OsStringExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// The deadline one test allows any single observable.
const DEADLINE: Duration = Duration::from_secs(5);

/// The deadline a signalled process is allowed to exit within.
const EXIT_DEADLINE: Duration = Duration::from_secs(2);

/// The settle a filter-only test allows the process to reach its serving loop.
///
/// The filter those tests set suppresses the startup line, the only output that
/// would mark readiness, and no port is needed to observe an empty stream, so
/// the test waits a short fixed period and then signals the process.
const SETTLE: Duration = Duration::from_millis(200);

/// The liveness body the binary must answer with, built from the build version.
const LIVENESS: &[u8] = concat!("Canary ", env!("CARGO_PKG_VERSION"), " is alive!\r\n").as_bytes();

/// The `Content-Length` line the liveness reply declares.
const LIVENESS_LENGTH: &[u8] = b"Content-Length: 25\r\n";

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

/// The lines `stream` prints, delivered as each one ends.
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
            if sender.send(line.trim_end().to_owned()).is_err() {
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

/// A running canary process, killed when the value is dropped.
struct Canary {
    child: Child,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
    port: Option<u16>,
}

impl Canary {
    /// Spawns the binary with exactly `env` set.
    fn spawn(env: &[(&str, &OsStr)]) -> Self {
        let mut child = command(env).spawn().expect("the canary binary starts");
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

    /// Spawns the binary that prints no startup line and lets it settle.
    fn start_silently(env: &[(&str, &OsStr)]) -> Self {
        let canary = Self::spawn(env);
        thread::sleep(SETTLE);
        canary
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

/// Runs the binary to exit within [`DEADLINE`] and collects both streams.
fn run_to_exit(env: &[(&str, &OsStr)]) -> Finished {
    let mut canary = Canary::spawn(env);
    let status = canary.wait(Instant::now() + DEADLINE);
    let deadline = Instant::now() + DEADLINE;
    Finished {
        status,
        stdout: canary.stdout_until_closed(deadline),
        stderr: canary.stderr_until_closed(deadline),
    }
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
        contains(&reply, LIVENESS_LENGTH),
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
        contains(&reply, LIVENESS_LENGTH),
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
        let env = [("PORT", OsStr::new("0")), ("RUST_LOG", OsStr::new(filter))];
        let mut canary = Canary::start_silently(&env);
        canary.signal("TERM");
        let status = canary.wait(Instant::now() + EXIT_DEADLINE);
        assert_eq!(status.code(), Some(0), "RUST_LOG={filter}");
        let deadline = Instant::now() + DEADLINE;
        let stdout = canary.stdout_until_closed(deadline);
        assert!(stdout.is_empty(), "RUST_LOG={filter}: {stdout:?}");
        let stderr = canary.stderr_until_closed(deadline);
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
        canary.next_stdout(deadline).as_deref(),
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
        canary.next_stdout(deadline).as_deref(),
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
    assert_eq!(entries.len(), DECLARED_DEPENDENCIES.len(), "{entries:?}");
    for (entry, name) in entries.iter().zip(DECLARED_DEPENDENCIES) {
        let (key, version) = entry
            .split_once(" = ")
            .expect("a dependency entry is an assignment");
        assert_eq!(key, name, "{entries:?}");
        assert!(version.contains("\"="), "{entry} is not exact-pinned");
    }
}

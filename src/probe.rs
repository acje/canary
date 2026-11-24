//! The dependency-free health probe the container runtime runs instead of a
//! shell.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::port::{ListenPort, PortError};

/// The one argument that asks for a health check instead of a served process.
const HEALTH_CHECK: &str = "--health-check";

/// The deadline for the loopback connection to the listener.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// The deadline for each reply read once the connection is established.
const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// The most reply bytes the probe reads before it judges the status line.
const STATUS_LIMIT: usize = 512;

/// The bounded liveness request, which asks the listener to close after one
/// reply.
const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";

/// How the process was invoked, parsed from the arguments after the program
/// name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Invocation {
    /// No arguments: serve the liveness listener.
    Serve,
    /// Exactly `--health-check`: probe the listener and exit with its status.
    HealthCheck,
    /// Any other shape: print the usage diagnostic and exit as a configuration
    /// error.
    Reject,
}

impl Invocation {
    /// Parses the arguments that follow the program name.
    ///
    /// Zero arguments mean [`Invocation::Serve`] and exactly one argument equal
    /// to `--health-check` means [`Invocation::HealthCheck`]. Every other
    /// shape, including a repeated, empty, or unknown argument, means
    /// [`Invocation::Reject`], so an invocation the process does not implement
    /// is refused rather than served.
    #[must_use]
    pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Self {
        let mut arguments = arguments.into_iter();
        match (arguments.next(), arguments.next()) {
            (None, _) => Self::Serve,
            (Some(argument), None) if argument == OsStr::new(HEALTH_CHECK) => Self::HealthCheck,
            _ => Self::Reject,
        }
    }
}

/// The result of one health probe.
///
/// The outcome is the probe's whole answer, so discarding it is never the
/// caller's intent; the container runtime turns it into the process status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum ProbeOutcome {
    /// The listener answered with an HTTP/1.1 200 status line.
    Healthy,
    /// The listener did not, for the named reason.
    Unhealthy(ProbeFailure),
}

impl ProbeOutcome {
    /// The process status the probe asks for: zero when healthy, one when not.
    ///
    /// This zero/one contract belongs to the probe, not to
    /// [`ExitCode`](crate::exit::ExitCode): the probe answers the container
    /// runtime's yes/no question about one served listener, while the exit-code
    /// contract names the terminal outcomes of the service itself.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Healthy => 0,
            Self::Unhealthy(_) => 1,
        }
    }
}

impl fmt::Display for ProbeOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => formatter.write_str("healthy"),
            Self::Unhealthy(failure) => write!(formatter, "unhealthy: {failure}"),
        }
    }
}

/// Why a probe could not establish that the listener is healthy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeFailure {
    /// `PORT` is absent or does not name a fixed port to connect to.
    PortAbsent,
    /// `PORT` is zero, so the listener's port is chosen at bind time and the
    /// probe has no fixed port to reach.
    EphemeralPort,
    /// The loopback connection to the listener could not be made.
    Connect(io::ErrorKind),
    /// The bounded request could not be written.
    Write(io::ErrorKind),
    /// The reply could not be read.
    Read(io::ErrorKind),
    /// The reply did not begin with an HTTP/1.1 200 status line.
    MalformedStatus,
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PortAbsent => formatter.write_str("PORT is absent or does not name a fixed port"),
            Self::EphemeralPort => formatter.write_str(
                "PORT is zero, so the listener's port is chosen at bind time and cannot be probed",
            ),
            Self::Connect(kind) => write!(formatter, "cannot connect to the listener: {kind}"),
            Self::Write(kind) => write!(formatter, "cannot send the probe request: {kind}"),
            Self::Read(kind) => write!(formatter, "cannot read the probe reply: {kind}"),
            Self::MalformedStatus => {
                formatter.write_str("the reply does not begin with an HTTP/1.1 200 status line")
            }
        }
    }
}

impl std::error::Error for ProbeFailure {}

/// Probes the listener named by `PORT` in the process environment.
///
/// The probe reads no other input and starts nothing: it connects to
/// `127.0.0.1:<port>` within [`CONNECT_TIMEOUT`], writes one bounded request,
/// bounds every read by [`READ_TIMEOUT`], reads at most [`STATUS_LIMIT`] bytes
/// of the reply, and is healthy only when the reply begins with an
/// `HTTP/1.1 200` status line.
pub fn check() -> ProbeOutcome {
    check_listen(ListenPort::from_env())
}

/// Probes the listener that the environment's `PORT` answer names.
fn check_listen(listen: Result<ListenPort, PortError>) -> ProbeOutcome {
    let port = match listen {
        Ok(ListenPort::Fixed(port)) => port,
        Ok(ListenPort::Ephemeral) => {
            return ProbeOutcome::Unhealthy(ProbeFailure::EphemeralPort);
        }
        Err(_) => return ProbeOutcome::Unhealthy(ProbeFailure::PortAbsent),
    };
    let address = SocketAddr::from(([127, 0, 0, 1], u16::from(port)));
    let mut stream = match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(error) => return ProbeOutcome::Unhealthy(ProbeFailure::Connect(error.kind())),
    };
    if let Err(error) = stream.set_read_timeout(Some(READ_TIMEOUT)) {
        return ProbeOutcome::Unhealthy(ProbeFailure::Read(error.kind()));
    }
    if let Err(error) = stream.write_all(REQUEST) {
        return ProbeOutcome::Unhealthy(ProbeFailure::Write(error.kind()));
    }
    judge_status(&mut stream)
}

/// Reads at most [`STATUS_LIMIT`] reply bytes and judges the status line.
fn judge_status(stream: &mut TcpStream) -> ProbeOutcome {
    let mut buffer = [0u8; STATUS_LIMIT];
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                if buffer[..filled].contains(&b'\n') {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return ProbeOutcome::Unhealthy(ProbeFailure::Read(error.kind())),
        }
    }
    if buffer[..filled].starts_with(b"HTTP/1.1 200") {
        ProbeOutcome::Healthy
    } else {
        ProbeOutcome::Unhealthy(ProbeFailure::MalformedStatus)
    }
}

#[cfg(test)]
mod tests {
    use super::{Invocation, ProbeFailure, ProbeOutcome, check_listen};
    use crate::port::{ListenPort, Port, PortError};
    use std::ffi::OsString;
    use std::io::{self, Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::thread;
    use std::time::Duration;

    fn invocation(arguments: &[&str]) -> Invocation {
        Invocation::parse(arguments.iter().map(|text| OsString::from(*text)))
    }

    fn failures() -> [ProbeFailure; 6] {
        [
            ProbeFailure::PortAbsent,
            ProbeFailure::EphemeralPort,
            ProbeFailure::Connect(io::ErrorKind::ConnectionRefused),
            ProbeFailure::Write(io::ErrorKind::BrokenPipe),
            ProbeFailure::Read(io::ErrorKind::TimedOut),
            ProbeFailure::MalformedStatus,
        ]
    }

    fn fixed_port(port: u16) -> ListenPort {
        ListenPort::Fixed(Port::try_from(port).unwrap())
    }

    /// Probes a loopback listener that answers one request with `reply`.
    fn probe_reply(reply: &'static [u8]) -> ProbeOutcome {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = [0u8; 256];
            let _ = stream.read(&mut request);
            stream.write_all(reply).unwrap();
            let _ = stream.shutdown(Shutdown::Both);
        });
        let outcome = check_listen(Ok(fixed_port(port)));
        server.join().unwrap();
        outcome
    }

    #[test]
    fn no_argument_serves() {
        assert_eq!(invocation(&[]), Invocation::Serve);
        assert_eq!(
            Invocation::parse(std::iter::empty::<OsString>()),
            Invocation::Serve
        );
    }

    #[test]
    fn the_single_health_check_argument_probes() {
        assert_eq!(invocation(&["--health-check"]), Invocation::HealthCheck);
    }

    #[test]
    fn any_other_argument_shape_is_rejected() {
        for arguments in [
            &["--health-check", "--extra"][..],
            &["--health-check", "--health-check"][..],
            &["--health-check", ""][..],
            &["--healthcheck"][..],
            &[""][..],
            &["--extra"][..],
        ] {
            assert_eq!(invocation(arguments), Invocation::Reject, "{arguments:?}");
        }
    }

    #[test]
    fn healthy_is_zero_and_every_failure_is_one() {
        assert_eq!(ProbeOutcome::Healthy.code(), 0);
        for failure in failures() {
            assert_eq!(ProbeOutcome::Unhealthy(failure).code(), 1, "{failure}");
        }
    }

    #[test]
    fn display_names_the_outcome_and_the_failure() {
        assert_eq!(ProbeOutcome::Healthy.to_string(), "healthy");
        assert_eq!(
            ProbeOutcome::Unhealthy(ProbeFailure::MalformedStatus).to_string(),
            "unhealthy: the reply does not begin with an HTTP/1.1 200 status line"
        );
        assert_eq!(
            ProbeFailure::Connect(io::ErrorKind::ConnectionRefused).to_string(),
            "cannot connect to the listener: connection refused"
        );
        for failure in failures() {
            assert!(!failure.to_string().is_empty(), "{failure:?}");
        }
    }

    #[test]
    fn an_absent_or_rejected_port_is_not_a_probe_target() {
        for rejected in [
            PortError::Missing,
            PortError::NotUnicode,
            PortError::Empty,
            PortError::NonNumeric,
            PortError::OutOfRange,
        ] {
            assert_eq!(
                check_listen(Err(rejected)),
                ProbeOutcome::Unhealthy(ProbeFailure::PortAbsent),
                "{rejected}"
            );
        }
    }

    #[test]
    fn an_ephemeral_port_names_the_unreachable_port_choice() {
        assert_eq!(
            check_listen(Ok(ListenPort::Ephemeral)),
            ProbeOutcome::Unhealthy(ProbeFailure::EphemeralPort)
        );
    }

    #[test]
    fn a_port_nobody_listens_on_is_a_connect_failure() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(
            check_listen(Ok(fixed_port(port))),
            ProbeOutcome::Unhealthy(ProbeFailure::Connect(io::ErrorKind::ConnectionRefused))
        );
    }

    #[test]
    fn a_two_hundred_status_line_is_healthy() {
        let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        assert_eq!(probe_reply(reply), ProbeOutcome::Healthy);
    }

    #[test]
    fn another_status_line_is_malformed() {
        for reply in [
            &b"HTTP/1.1 500 Internal Server Error\r\n\r\n"[..],
            &b"HTTP/1.1 404 Not Found\r\n\r\n"[..],
            &b"HTTP/1.0 200 OK\r\n\r\n"[..],
        ] {
            assert_eq!(
                probe_reply(reply),
                ProbeOutcome::Unhealthy(ProbeFailure::MalformedStatus),
                "{}",
                String::from_utf8_lossy(reply)
            );
        }
    }

    #[test]
    fn a_reply_that_is_not_http_is_malformed() {
        assert_eq!(
            probe_reply(b"not an HTTP reply\r\n"),
            ProbeOutcome::Unhealthy(ProbeFailure::MalformedStatus)
        );
    }

    #[test]
    fn a_listener_that_closes_without_replying_is_malformed() {
        assert_eq!(
            probe_reply(b""),
            ProbeOutcome::Unhealthy(ProbeFailure::MalformedStatus)
        );
    }
}

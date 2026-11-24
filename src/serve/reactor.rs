//! The single-owner mio reactor turn.
//!
//! [`Reactor`] owns the listening socket, the fixed slot table, and the poller.
//! One [`Reactor::turn`] call runs a fixed, bounded sequence: the listener
//! accept (at most [`ACCEPT_PER_TURN`]), one visit to each connection with a
//! poll event (at most [`MAX_EVENTS`], each visited once), the absolute-deadline
//! sweep, and exactly one bounded log drain. No step can exceed its budget, so a
//! turn is bounded by construction and never blocks on a socket or on the sink.
//!
//! The signal token is reserved for the source that reports shutdown; it is not
//! registered here, and the signal step ahead of the listener arrives with it.
//!
//! [`Reactor::next_deadline`] is the only timer owner: the earliest connection
//! deadline, or the log retry deadline a stalled drain schedules.

use std::fmt;
use std::io::{ErrorKind, Read as _, Write};
use std::time::{Duration, Instant};

use mio::net::TcpListener;
use mio::{Events, Interest, Poll, Token};

use crate::exit::ExitCode;
use crate::http::{MAX_HEAD_BYTES, Reply, ResponsePlan};
use crate::log::sink::{self, DrainBudget, DrainOutcome, LOG_RETRY};
use crate::log::{LogFilter, LogStore};

use super::MAX_CONNECTIONS;
use super::slots::{Pending, Phase, ReadOutcome, Slots, WriteProgress};

/// The poll token of the listening socket.
pub const TOKEN_LISTENER: usize = 0;

/// The poll token reserved for the shutdown signal source.
///
/// The source is registered by the increment that adds shutdown handling, so no
/// event carries this token yet.
pub const TOKEN_SIGNAL: usize = 1;

/// The poll token of the connection in the first slot.
const TOKEN_FIRST_CONNECTION: usize = 2;

/// The poll events one turn may collect.
pub const MAX_EVENTS: usize = 64;

/// The connections one turn may accept.
pub const ACCEPT_PER_TURN: usize = 16;

/// The bytes one turn may read from one connection.
pub const READ_BYTES_PER_TURN: usize = MAX_HEAD_BYTES;

/// The bytes one turn may write to one connection.
pub const WRITE_BYTES_PER_TURN: usize = 512;

const _: () = assert!(
    TOKEN_FIRST_CONNECTION
        .checked_add(MAX_CONNECTIONS)
        .is_some()
);

/// The poll token that names the connection in slot `index`.
#[must_use]
pub const fn connection_token(index: usize) -> Token {
    Token(TOKEN_FIRST_CONNECTION + index)
}

/// The slot a connection token names.
///
/// Returns `None` for the listener token, the signal token, and every token past
/// the last connection slot.
#[must_use]
pub const fn connection_slot(token: Token) -> Option<usize> {
    let index = token.0.wrapping_sub(TOKEN_FIRST_CONNECTION);
    if index < MAX_CONNECTIONS {
        Some(index)
    } else {
        None
    }
}

/// How long one turn waits for work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnWait {
    /// Block until the next deadline the reactor owns.
    ///
    /// When the reactor owns no deadline the turn blocks until an event arrives.
    UntilDeadline,
    /// Return immediately with whatever is ready.
    Immediate,
}

/// What one turn did.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnOutcome {
    /// The turn found no work.
    Idle,
    /// The turn accepted, read, wrote, closed, or drained at least one item.
    Progress,
    /// The turn observed shutdown. No path constructs this variant until the
    /// signal source is registered.
    ShuttingDown,
}

/// A poller or socket operation the reactor could not complete.
///
/// Every variant maps to [`ExitCode::Software`] through
/// [`ServeError::exit_code`]: a reactor failure is an internal fault, never a
/// configuration problem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServeError {
    /// Waiting for events failed.
    Poll(ErrorKind),
    /// Accepting a pending connection failed.
    Accept(ErrorKind),
    /// Registering a source with the poller failed.
    Register(ErrorKind),
    /// Deregistering a source from the poller failed.
    Deregister(ErrorKind),
}

impl ServeError {
    /// The process exit code a reactor failure maps to.
    #[must_use]
    pub const fn exit_code(self) -> ExitCode {
        ExitCode::Software
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (step, kind) = match self {
            Self::Poll(kind) => ("poll", kind),
            Self::Accept(kind) => ("accept", kind),
            Self::Register(kind) => ("register", kind),
            Self::Deregister(kind) => ("deregister", kind),
        };
        write!(formatter, "reactor {step} failed: {kind:?}")
    }
}

impl std::error::Error for ServeError {}

/// The single owner of the listener, the connection slots, and the poller.
pub struct Reactor {
    poll: Poll,
    listener: TcpListener,
    listener_registered: bool,
    accept_backlog: bool,
    plan: ResponsePlan,
    slots: Slots,
    events: Events,
    log_retry_at: Option<Instant>,
    served: u64,
    rejected: u64,
}

impl Reactor {
    /// Registers `listener` with the poller and reserves the event buffers.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Poll`] when the poller cannot be created and
    /// [`ServeError::Register`] when the listener cannot be registered.
    pub fn new(mut listener: TcpListener, plan: ResponsePlan) -> Result<Self, ServeError> {
        let poll = Poll::new().map_err(|error| ServeError::Poll(error.kind()))?;
        poll.registry()
            .register(&mut listener, Token(TOKEN_LISTENER), Interest::READABLE)
            .map_err(|error| ServeError::Register(error.kind()))?;
        Ok(Self {
            poll,
            listener,
            listener_registered: true,
            accept_backlog: false,
            plan,
            slots: Slots::new(),
            events: Events::with_capacity(MAX_EVENTS),
            log_retry_at: None,
            served: 0,
            rejected: 0,
        })
    }

    /// The requests whose reply the reactor wrote completely.
    ///
    /// Saturates at `u64::MAX`.
    #[must_use]
    pub const fn served(&self) -> u64 {
        self.served
    }

    /// The connections the reactor closed without answering.
    ///
    /// A rejection is a malformed head, an oversized head, or a connection whose
    /// absolute deadline passed before its reply completed. A client that leaves
    /// before its head terminates is neither served nor rejected.
    ///
    /// Saturates at `u64::MAX`.
    #[must_use]
    pub const fn rejected(&self) -> u64 {
        self.rejected
    }

    /// The instant the next [`TurnWait::UntilDeadline`] turn must wake by.
    ///
    /// The earliest of the occupied slot deadlines, the accept backlog a
    /// budget-cut batch left behind (reported as `now`), and the log retry
    /// deadline a stalled drain scheduled; `None` when the reactor owns no
    /// timer, which lets the turn block until an event arrives. A deadline at
    /// or before `now` is reported as `now`, so the caller's timeout is zero and
    /// never negative.
    ///
    /// The accept backlog needs a deadline because the kernel poller reports a
    /// listening socket edge-wise: a batch stopped by [`ACCEPT_PER_TURN`] leaves
    /// connections queued that no further edge announces.
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.accept_backlog {
            return Some(now);
        }
        let pending = match (self.slots.earliest_deadline(), self.log_retry_at) {
            (Some(slot), Some(retry)) => Some(slot.min(retry)),
            (Some(slot), None) => Some(slot),
            (None, Some(retry)) => Some(retry),
            (None, None) => None,
        };
        pending.map(|deadline| deadline.max(now))
    }

    /// Performs one bounded turn.
    ///
    /// The turn collects at most [`MAX_EVENTS`] events, accepts at most
    /// [`ACCEPT_PER_TURN`] connections, visits each connection with an event at
    /// most once, releases every expired connection, and drains the log queue
    /// once.
    ///
    /// The filter names the threshold the reactor's lifecycle records use; this
    /// increment derives only the drain, so the value is carried unused.
    ///
    /// # Errors
    ///
    /// Returns the [`ServeError`] of the poller or socket operation that failed.
    pub fn turn(
        &mut self,
        store: &mut LogStore,
        filter: LogFilter,
        sink: &mut impl Write,
        now: Instant,
        wait: TurnWait,
    ) -> Result<TurnOutcome, ServeError> {
        let timeout = match wait {
            TurnWait::Immediate => Some(Duration::ZERO),
            TurnWait::UntilDeadline => self
                .next_deadline(now)
                .map(|deadline| deadline.saturating_duration_since(now)),
        };
        self.poll
            .poll(&mut self.events, timeout)
            .map_err(|error| ServeError::Poll(error.kind()))?;
        let mut tokens = [Token(0); MAX_EVENTS];
        let mut count = 0;
        for event in &self.events {
            if count == MAX_EVENTS {
                break;
            }
            tokens[count] = event.token();
            count += 1;
        }
        let mut progress = false;
        if self.accept_backlog || tokens[..count].contains(&Token(TOKEN_LISTENER)) {
            progress |= self.accept_ready(now)?;
        }
        let mut visited = [false; MAX_CONNECTIONS];
        for &token in &tokens[..count] {
            let Some(index) = connection_slot(token) else {
                continue;
            };
            if visited[index] {
                continue;
            }
            visited[index] = true;
            progress |= self.visit(index)?;
        }
        progress |= self.expire(now)?;
        progress |= self.drain_logs(store, filter, sink, now);
        Ok(if progress {
            TurnOutcome::Progress
        } else {
            TurnOutcome::Idle
        })
    }

    /// Accepts pending connections up to the per-turn budget.
    ///
    /// A full slot table stops the batch and drops the poller's listener interest
    /// so no further connection is accepted until a release frees a slot.
    fn accept_ready(&mut self, now: Instant) -> Result<bool, ServeError> {
        let mut progress = false;
        let mut accepted = 0;
        for _ in 0..ACCEPT_PER_TURN {
            match self.listener.accept() {
                Ok((stream, _address)) => {
                    accepted += 1;
                    let Some(index) = self.slots.acquire(stream, now) else {
                        self.accept_backlog = false;
                        self.set_listener_registered(false)?;
                        return Ok(true);
                    };
                    let Some(source) = self.slots.stream_mut(index) else {
                        break;
                    };
                    self.poll
                        .registry()
                        .register(source, connection_token(index), Interest::READABLE)
                        .map_err(|error| ServeError::Register(error.kind()))?;
                    progress = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(ServeError::Accept(error.kind())),
            }
        }
        self.accept_backlog = accepted == ACCEPT_PER_TURN;
        Ok(progress)
    }

    /// Visits the connection in `index` once, following its current phase.
    fn visit(&mut self, index: usize) -> Result<bool, ServeError> {
        match self.slots.phase(index) {
            Some(Phase::Reading) => self.read_ready(index),
            Some(Phase::Writing) => self.write_ready(index),
            Some(Phase::Closing) => {
                self.release(index)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Reads one bounded chunk into a reading slot and acts on its head verdict.
    fn read_ready(&mut self, index: usize) -> Result<bool, ServeError> {
        let Some(room) = self.slots.head_room(index) else {
            return Ok(false);
        };
        let cap = room.min(READ_BYTES_PER_TURN);
        if cap == 0 {
            return Ok(false);
        }
        let mut buffer = [0u8; READ_BYTES_PER_TURN];
        let Some(stream) = self.slots.stream_mut(index) else {
            return Ok(false);
        };
        let read = match stream.read(&mut buffer[..cap]) {
            Ok(0) => return self.begin_close_and_release(index),
            Ok(read) => read,
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                return Ok(false);
            }
            Err(_) => return self.begin_close_and_release(index),
        };
        let Some(outcome) = self.slots.read_into(index, &buffer[..read]) else {
            return Ok(false);
        };
        match outcome {
            ReadOutcome::Incomplete => Ok(read > 0),
            ReadOutcome::Complete(request) => {
                let total_len = match self.plan.reply(request) {
                    Reply::Full(bytes) => bytes.len(),
                    Reply::HeadOnly(head) => head.head().len(),
                };
                self.begin_write(index, Pending::Answer(request), total_len)
            }
            ReadOutcome::Malformed(_) => {
                let total_len = self.plan.bad_request().len();
                self.reject(index, Pending::BadRequest, total_len)
            }
            ReadOutcome::TooLarge => {
                let total_len = self.plan.too_large().len();
                self.reject(index, Pending::TooLarge, total_len)
            }
        }
    }

    /// Moves a reading slot into a write of `pending` and asks for writability.
    fn begin_write(
        &mut self,
        index: usize,
        pending: Pending,
        total_len: usize,
    ) -> Result<bool, ServeError> {
        if !self.slots.begin_write(index, pending, total_len) {
            return Ok(false);
        }
        self.set_connection_interest(index, Interest::WRITABLE)?;
        Ok(true)
    }

    /// Moves a reading slot into a rejected reply and counts the rejection.
    fn reject(
        &mut self,
        index: usize,
        pending: Pending,
        total_len: usize,
    ) -> Result<bool, ServeError> {
        if !self.begin_write(index, pending, total_len)? {
            return Ok(false);
        }
        self.rejected = self.rejected.saturating_add(1);
        Ok(true)
    }

    /// Writes one bounded chunk of a writing slot's reply.
    fn write_ready(&mut self, index: usize) -> Result<bool, ServeError> {
        let Some(pending) = self.slots.pending(index) else {
            return Ok(false);
        };
        let Some(cursor) = self.slots.cursor(index) else {
            return Ok(false);
        };
        let bytes = reply_bytes(&self.plan, pending);
        let Some(remaining) = bytes.get(cursor..) else {
            return Ok(false);
        };
        let cap = remaining.len().min(WRITE_BYTES_PER_TURN);
        if cap == 0 {
            return Ok(false);
        }
        let Some(stream) = self.slots.stream_mut(index) else {
            return Ok(false);
        };
        let written = match stream.write(&remaining[..cap]) {
            Ok(written) => written,
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                return Ok(false);
            }
            Err(_) => {
                self.release(index)?;
                return Ok(true);
            }
        };
        match self.slots.advance_written(index, written) {
            Some(WriteProgress::Finished) => {
                self.release(index)?;
                if matches!(pending, Pending::Answer(_)) {
                    self.served = self.served.saturating_add(1);
                }
                Ok(true)
            }
            Some(WriteProgress::Remaining) => Ok(written > 0),
            None => Ok(false),
        }
    }

    /// Marks `index` closing, then deregisters and frees it.
    fn begin_close_and_release(&mut self, index: usize) -> Result<bool, ServeError> {
        let marked = self.slots.begin_close(index);
        self.release(index)?;
        Ok(marked)
    }

    /// Deregisters and frees `index`, reopening the listener if a slot is free.
    ///
    /// A listener that was deregistered for capacity is registered again, and the
    /// accept backlog is marked pending: connections may have queued while no
    /// interest was registered, and the poller announces a listening socket
    /// edge-wise, so the next turn must try an accept without waiting for an edge.
    fn release(&mut self, index: usize) -> Result<(), ServeError> {
        let reopen = !self.listener_registered;
        if let Some(stream) = self.slots.stream_mut(index) {
            self.poll
                .registry()
                .deregister(stream)
                .map_err(|error| ServeError::Deregister(error.kind()))?;
        }
        self.slots.release(index);
        self.set_listener_registered(true)?;
        if reopen {
            self.accept_backlog = true;
        }
        Ok(())
    }

    /// Releases every occupied slot whose absolute deadline `now` has passed.
    fn expire(&mut self, now: Instant) -> Result<bool, ServeError> {
        let mut expired = [0usize; MAX_CONNECTIONS];
        let count = self.slots.expired(now, &mut expired);
        for &index in &expired[..count] {
            self.release(index)?;
            self.rejected = self.rejected.saturating_add(1);
        }
        Ok(count > 0)
    }

    /// Drains the log queue once and schedules the retry a stall asks for.
    ///
    /// The filter is the threshold for the lifecycle records this step will own;
    /// the drain itself only moves queued bytes, so the value is carried unused.
    fn drain_logs(
        &mut self,
        store: &mut LogStore,
        _filter: LogFilter,
        sink: &mut impl Write,
        now: Instant,
    ) -> bool {
        let outcome = sink::drain(store, sink, DrainBudget::DEFAULT);
        self.log_retry_at = match outcome {
            DrainOutcome::Stalled => Some(now + LOG_RETRY),
            DrainOutcome::Idle | DrainOutcome::Progress { .. } | DrainOutcome::Failed { .. } => {
                None
            }
        };
        matches!(outcome, DrainOutcome::Progress { .. })
    }

    /// Registers or deregisters the listener so `registered` holds afterwards.
    fn set_listener_registered(&mut self, registered: bool) -> Result<(), ServeError> {
        match (self.listener_registered, registered) {
            (true, false) => self
                .poll
                .registry()
                .deregister(&mut self.listener)
                .map_err(|error| ServeError::Deregister(error.kind()))?,
            (false, true) => self
                .poll
                .registry()
                .register(
                    &mut self.listener,
                    Token(TOKEN_LISTENER),
                    Interest::READABLE,
                )
                .map_err(|error| ServeError::Register(error.kind()))?,
            (true, true) | (false, false) => {}
        }
        self.listener_registered = registered;
        Ok(())
    }

    /// Replaces the poll interest of the occupied slot `index`.
    fn set_connection_interest(
        &mut self,
        index: usize,
        interest: Interest,
    ) -> Result<(), ServeError> {
        let Some(stream) = self.slots.stream_mut(index) else {
            return Ok(());
        };
        self.poll
            .registry()
            .reregister(stream, connection_token(index), interest)
            .map_err(|error| ServeError::Register(error.kind()))
    }
}

/// The bytes a stored pending reply writes.
fn reply_bytes(plan: &ResponsePlan, pending: Pending) -> &[u8] {
    match pending {
        Pending::Answer(request) => match plan.reply(request) {
            Reply::Full(bytes) => bytes,
            Reply::HeadOnly(head) => head.head(),
        },
        Pending::BadRequest => plan.bad_request(),
        Pending::TooLarge => plan.too_large(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ACCEPT_PER_TURN, Reactor, ServeError, TOKEN_LISTENER, TOKEN_SIGNAL, TurnOutcome, TurnWait,
        connection_slot, connection_token,
    };
    use crate::exit::ExitCode;
    use crate::http::ResponsePlan;
    use crate::log::sink::{self, LOG_RETRY};
    use crate::log::{LogFilter, LogStore, Record, WriteOutcome};
    use crate::serve::{CONNECTION_DEADLINE, HEAD_BYTES, MAX_CONNECTIONS};
    use mio::Token;
    use mio::net::TcpListener as MioListener;
    use std::io::{self, ErrorKind, Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::{Duration, Instant};

    const DRIVE_TURNS: usize = 1024;

    struct Harness {
        reactor: Reactor,
        store: LogStore,
        sink: Vec<u8>,
        address: SocketAddr,
        now: Instant,
    }

    impl Harness {
        fn new() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let listener = MioListener::from_std(listener);
            let plan = ResponsePlan::new(env!("CARGO_PKG_VERSION"));
            Self {
                reactor: Reactor::new(listener, plan).unwrap(),
                store: LogStore::new().unwrap(),
                sink: Vec::new(),
                address,
                now: Instant::now(),
            }
        }

        fn connect(&self) -> TcpStream {
            let client = TcpStream::connect(self.address).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            client
        }

        fn turn(&mut self) -> TurnOutcome {
            self.reactor
                .turn(
                    &mut self.store,
                    LogFilter::Info,
                    &mut self.sink,
                    self.now,
                    TurnWait::Immediate,
                )
                .unwrap()
        }

        fn drive_until<F>(&mut self, mut done: F)
        where
            F: FnMut(&Self) -> bool,
        {
            for _ in 0..DRIVE_TURNS {
                if done(self) {
                    return;
                }
                let _ = self.turn();
            }
            assert!(done(self), "the reactor did not reach the expected state");
        }

        fn advance(&mut self, delta: Duration) {
            self.now += delta;
        }

        fn exchange(&mut self, request: &[u8]) -> Vec<u8> {
            self.exchange_driven(request, |harness| {
                harness.reactor.served() == 1 && harness.reactor.slots.is_empty()
            })
        }

        fn exchange_driven<F>(&mut self, request: &[u8], done: F) -> Vec<u8>
        where
            F: FnMut(&Self) -> bool,
        {
            let mut client = self.connect();
            client.write_all(request).unwrap();
            self.drive_until(done);
            read_reply(&mut client)
        }
    }

    fn read_reply(client: &mut TcpStream) -> Vec<u8> {
        let mut received = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            match client.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => received.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::ConnectionReset => break,
                Err(error) => panic!("the client could not read the reply: {error}"),
            }
        }
        received
    }

    struct StalledSink;

    impl Write for StalledSink {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(ErrorKind::WouldBlock))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn plan() -> ResponsePlan {
        ResponsePlan::new(env!("CARGO_PKG_VERSION"))
    }

    fn liveness_line() -> &'static [u8] {
        concat!("Canary ", env!("CARGO_PKG_VERSION"), " is alive!\r\n").as_bytes()
    }

    fn body_of(reply: &[u8]) -> &[u8] {
        let split = reply
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        &reply[split..]
    }

    #[test]
    fn tokens_name_the_listener_the_signal_and_the_slots() {
        assert_eq!(TOKEN_LISTENER, 0);
        assert_eq!(TOKEN_SIGNAL, 1);
        assert_eq!(connection_token(0), Token(2));
        assert_eq!(
            connection_token(MAX_CONNECTIONS - 1),
            Token(MAX_CONNECTIONS + 1)
        );
        assert_eq!(connection_slot(Token(TOKEN_LISTENER)), None);
        assert_eq!(connection_slot(Token(TOKEN_SIGNAL)), None);
        assert_eq!(connection_slot(connection_token(7)), Some(7));
        assert_eq!(connection_slot(Token(MAX_CONNECTIONS + 2)), None);
    }

    #[test]
    fn a_serve_error_maps_to_software_and_names_its_step() {
        let error = ServeError::Register(ErrorKind::AlreadyExists);
        assert_eq!(error.exit_code(), ExitCode::Software);
        assert_eq!(error.to_string(), "reactor register failed: AlreadyExists");
    }

    #[test]
    fn get_root_answers_200_with_the_liveness_body() {
        let mut harness = Harness::new();
        let received = harness.exchange(b"GET / HTTP/1.1\r\n\r\n");
        assert_eq!(received, plan().ok_get());
        assert_eq!(body_of(&received), liveness_line());
        assert_eq!(body_of(&received).len(), 25);
        assert_eq!(harness.reactor.served(), 1);
        assert_eq!(harness.reactor.rejected(), 0);
    }

    #[test]
    fn head_root_answers_headers_without_a_body() {
        let mut harness = Harness::new();
        let received = harness.exchange(b"HEAD / HTTP/1.1\r\n\r\n");
        assert!(received.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(received, plan().ok_head().head());
        assert_eq!(body_of(&received), []);
        assert_eq!(harness.reactor.served(), 1);
    }

    #[test]
    fn other_target_answers_404() {
        let mut harness = Harness::new();
        let received = harness.exchange(b"GET /nope HTTP/1.1\r\n\r\n");
        assert_eq!(received, plan().not_found());
        assert_eq!(harness.reactor.served(), 1);
    }

    #[test]
    fn other_method_answers_405() {
        let mut harness = Harness::new();
        let received = harness.exchange(b"POST / HTTP/1.1\r\n\r\n");
        assert_eq!(received, plan().method_not_allowed());
        assert_eq!(harness.reactor.served(), 1);
    }

    #[test]
    fn an_unterminated_eleven_hundred_byte_head_answers_431() {
        let mut harness = Harness::new();
        let mut request = Vec::from(&b"GET / HTTP/1.1\r\nX: "[..]);
        request.resize(1100, b'a');
        assert!(!request.windows(4).any(|window| window == b"\r\n\r\n"));
        let received = harness.exchange_driven(&request, |harness| {
            harness.reactor.rejected() == 1 && harness.reactor.slots.is_empty()
        });
        assert_eq!(received, plan().too_large());
        assert_eq!(harness.reactor.rejected(), 1);
        assert_eq!(harness.reactor.served(), 0);
    }

    #[test]
    fn a_request_split_across_two_turns_answers_exactly_once() {
        let mut harness = Harness::new();
        let mut client = harness.connect();
        client.write_all(b"GET / HTT").unwrap();
        harness.drive_until(|harness| {
            harness.reactor.slots.head_room(0) == Some(HEAD_BYTES - b"GET / HTT".len())
        });
        client.write_all(b"P/1.1\r\n\r\n").unwrap();
        harness.drive_until(|harness| {
            harness.reactor.served() == 1 && harness.reactor.slots.is_empty()
        });
        let mut received = Vec::new();
        client.read_to_end(&mut received).unwrap();
        assert_eq!(received, plan().ok_get());
        assert_eq!(harness.reactor.served(), 1);
    }

    #[test]
    fn a_capacity_refusal_keeps_the_slots_and_closes_the_over_capacity_peer() {
        let mut harness = Harness::new();
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            clients.push(harness.connect());
        }
        harness.drive_until(|harness| harness.reactor.slots.len() == MAX_CONNECTIONS);
        assert!(harness.reactor.listener_registered);
        let mut overflow = harness.connect();
        harness.drive_until(|harness| !harness.reactor.listener_registered);
        assert_eq!(harness.reactor.slots.len(), MAX_CONNECTIONS);
        let mut buffer = [0u8; 1];
        assert_eq!(overflow.read(&mut buffer).unwrap(), 0);
        assert_eq!(clients.len(), MAX_CONNECTIONS);
    }

    #[test]
    fn a_released_slot_reopens_the_listener_for_the_next_accept() {
        let mut harness = Harness::new();
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            clients.push(harness.connect());
        }
        harness.drive_until(|harness| harness.reactor.slots.len() == MAX_CONNECTIONS);
        let _overflow = harness.connect();
        harness.drive_until(|harness| !harness.reactor.listener_registered);
        harness.advance(CONNECTION_DEADLINE + Duration::from_nanos(1));
        harness.drive_until(|harness| harness.reactor.slots.is_empty());
        assert_eq!(
            harness.reactor.rejected(),
            u64::try_from(MAX_CONNECTIONS).unwrap()
        );
        assert!(harness.reactor.listener_registered);
        let mut replacement = harness.connect();
        replacement.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        harness.drive_until(|harness| {
            harness.reactor.served() == 1 && harness.reactor.slots.is_empty()
        });
        let mut received = Vec::new();
        replacement.read_to_end(&mut received).unwrap();
        assert_eq!(received, plan().ok_get());
        assert_eq!(harness.reactor.served(), 1);
        assert_eq!(clients.len(), MAX_CONNECTIONS);
    }

    #[test]
    fn one_turn_accepts_at_most_the_accept_budget() {
        let mut harness = Harness::new();
        let mut clients = Vec::new();
        for _ in 0..(2 * ACCEPT_PER_TURN) {
            clients.push(harness.connect());
        }
        for turn in 1..=8 {
            let _ = harness.turn();
            assert!(
                harness.reactor.slots.len() <= turn * ACCEPT_PER_TURN,
                "turn {turn} accepted past its budget"
            );
        }
        assert_eq!(harness.reactor.slots.len(), 2 * ACCEPT_PER_TURN);
        assert_eq!(clients.len(), 2 * ACCEPT_PER_TURN);
    }

    #[test]
    fn a_silent_connection_is_released_at_its_deadline_and_the_peer_sees_eof() {
        let mut harness = Harness::new();
        let mut client = harness.connect();
        harness.drive_until(|harness| harness.reactor.slots.len() == 1);
        harness.advance(CONNECTION_DEADLINE + Duration::from_nanos(1));
        harness.drive_until(|harness| harness.reactor.slots.is_empty());
        assert_eq!(harness.reactor.rejected(), 1);
        let mut buffer = [0u8; 1];
        assert_eq!(client.read(&mut buffer).unwrap(), 0);
    }

    #[test]
    fn an_empty_immediate_turn_is_idle() {
        let mut harness = Harness::new();
        assert_eq!(harness.turn(), TurnOutcome::Idle);
    }

    #[test]
    fn one_turn_drains_at_most_one_bounded_log_batch() {
        let mut harness = Harness::new();
        let queued = 32u64;
        for served in 0..queued {
            let record = Record::Tick {
                served,
                rejected: 0,
                dropped: 0,
            };
            assert_eq!(
                harness.store.write(LogFilter::Info, record),
                WriteOutcome::Written
            );
        }
        assert_eq!(harness.turn(), TurnOutcome::Progress);
        let remaining = u64::try_from(harness.store.len()).unwrap();
        assert_eq!(
            remaining,
            queued - u64::try_from(sink::SINK_MAX_OPS).unwrap()
        );
        let lines = harness
            .sink
            .windows(2)
            .filter(|window| *window == b"\r\n")
            .count();
        assert_eq!(lines, sink::SINK_MAX_OPS);
        assert_eq!(harness.turn(), TurnOutcome::Progress);
        assert!(harness.store.is_empty());
    }

    #[test]
    fn the_timer_owner_reports_the_earliest_deadline_or_none() {
        let mut harness = Harness::new();
        assert_eq!(harness.reactor.next_deadline(harness.now), None);
        let _client = harness.connect();
        harness.drive_until(|harness| harness.reactor.slots.len() == 1);
        assert_eq!(
            harness.reactor.next_deadline(harness.now),
            Some(harness.now + CONNECTION_DEADLINE)
        );
    }

    #[test]
    fn a_stalled_drain_schedules_the_log_retry_deadline() {
        let mut harness = Harness::new();
        assert_eq!(
            harness
                .store
                .write(LogFilter::Info, Record::FilterDefaulted),
            WriteOutcome::Written
        );
        let outcome = harness
            .reactor
            .turn(
                &mut harness.store,
                LogFilter::Info,
                &mut StalledSink,
                harness.now,
                TurnWait::Immediate,
            )
            .unwrap();
        assert_eq!(outcome, TurnOutcome::Idle);
        assert_eq!(
            harness.reactor.next_deadline(harness.now),
            Some(harness.now + LOG_RETRY)
        );
    }
}

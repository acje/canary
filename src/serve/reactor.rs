//! The single-owner mio reactor turn.
//!
//! [`Reactor`] owns the listening socket, the fixed slot table, the shutdown
//! self-pipe, and the poller. One turn is two calls. [`Reactor::wait`] derives a
//! timeout from the instant a caller sampled before it waits, polls, and
//! snapshots the readiness it collected. [`Reactor::turn`] then runs a fixed,
//! bounded sequence over that snapshot at the instant sampled after the wait
//! returned: the shutdown step (at most [`SIGNAL_READS_PER_TURN`] reads), the
//! listener accept (at most [`ACCEPT_PER_TURN`]), one visit to each connection
//! that had an event (at most [`MAX_EVENTS`], each visited once), the
//! absolute-deadline sweep, the aggregate tick, and exactly one bounded log
//! drain. No step can exceed its budget, so a turn is bounded by construction and
//! never blocks on a socket, on the signal pipe, or on the sink.
//!
//! The signal token carries shutdown. [`ShutdownSignals`] owns the read end of a
//! self-pipe and the write end a registered handler writes one byte to, and
//! [`Reactor::new`] registers the read end at [`TOKEN_SIGNAL`]. The turn that
//! observes the token drains the pipe, latches the request, and reports
//! [`TurnOutcome::ShuttingDown`] before any other step; a second signal is
//! absorbed by the latch. [`Reactor::shutdown`] is then the one bounded flush.
//!
//! [`Reactor::next_deadline`] is the only timer owner: the earliest connection
//! deadline, the accept backlog a budget-cut batch left behind, the aggregate
//! tick the counters moved ahead of, or the log retry a stalled, failed, or
//! unfinished drain schedules.

use std::fmt;
use std::io::{self, ErrorKind, PipeWriter, Read, Write};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use mio::net::TcpListener;
use mio::unix::pipe::Receiver;
use mio::{Events, Interest, Poll, Token};
use signal_hook::SigId;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::low_level::pipe as signal_pipe;

use crate::http::{MAX_HEAD_BYTES, MAX_REPLY_BYTES, ParseOutcome, Reply, ResponsePlan};
use crate::log::sink::{self, DrainBudget, DrainOutcome, FLUSH_DRAIN, LOG_RETRY};
use crate::log::{LogFilter, LogStore, Record, WriteOutcome};

use super::slots::{Pending, Phase, Slots, WriteProgress};
use super::{HEAD_BYTES, MAX_CONNECTIONS};

/// The poll token of the listening socket.
pub const TOKEN_LISTENER: usize = 0;

/// The poll token of the shutdown self-pipe.
///
/// [`Reactor::new`] registers the pipe's read end here, so the first turn that
/// sees the token reports [`TurnOutcome::ShuttingDown`].
pub const TOKEN_SIGNAL: usize = 1;

/// The poll token of the connection in the first slot.
const TOKEN_FIRST_CONNECTION: usize = TOKEN_SIGNAL + 1;

/// The poll events one turn may collect.
pub const MAX_EVENTS: usize = 64;

/// The connections one turn may accept.
pub const ACCEPT_PER_TURN: usize = 16;

/// The bytes one turn may read from one connection.
pub const READ_BYTES_PER_TURN: usize = MAX_HEAD_BYTES;

/// The bytes one turn may write to one connection.
pub const WRITE_BYTES_PER_TURN: usize = 512;

/// The poll token of the last connection slot, which must fit the token space.
const TOKEN_LAST_CONNECTION: usize = TOKEN_FIRST_CONNECTION + MAX_CONNECTIONS - 1;

/// The interval between two aggregate tick records.
///
/// Owner: this reactor. Scope: one reactor, aggregate across connections. Units:
/// wall-clock time. Limit: one second, so [`Record::Tick`] costs the sink at most
/// one line per second however much traffic arrives. Exhaustion: a tick the
/// interval does not admit waits for the next [`Reactor::next_deadline`].
pub const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// The bytes one turn reads from the shutdown pipe in a single read.
pub const SIGNAL_DRAIN_BYTES: usize = 64;

/// The reads one turn spends draining the shutdown pipe.
///
/// Owner: this reactor's shutdown step. Scope: one turn, the one shutdown pipe.
/// Units: reads of [`SIGNAL_DRAIN_BYTES`], aggregate per turn. Limit: 8, so a
/// turn clears up to 512 coalesced signal bytes. Exhaustion: the shutdown
/// request is latched regardless, and a residue wakes the next turn, which
/// reports [`TurnOutcome::ShuttingDown`] again.
pub const SIGNAL_READS_PER_TURN: usize = 8;

/// The number of signals [`ShutdownSignals`] subscribes to.
const SHUTDOWN_SIGNAL_COUNT: usize = 2;

/// The signals that request a shutdown.
const SHUTDOWN_SIGNALS: [libc::c_int; SHUTDOWN_SIGNAL_COUNT] = [SIGTERM, SIGINT];

const _: () = assert!(TOKEN_LISTENER < TOKEN_SIGNAL);
const _: () = assert!(TOKEN_SIGNAL < TOKEN_FIRST_CONNECTION);
const _: () = assert!(TOKEN_LAST_CONNECTION.checked_add(1).is_some());
const _: () = assert!(WRITE_BYTES_PER_TURN >= MAX_REPLY_BYTES);

/// The read end of the shutdown self-pipe and the write end the handlers feed.
///
/// The read end is the mio source [`Reactor::new`] registers at
/// [`TOKEN_SIGNAL`]: a registered handler writes one byte into the pipe, so the
/// poller wakes with the token and the reactor reads the request where an
/// ordinary turn can act on it.
///
/// [`ShutdownSignals::detached`] builds the pipe without touching a process
/// signal handler, which is how a test drives shutdown by writing a byte into it
/// itself. [`ShutdownSignals::install`] is the only constructor that registers
/// handlers, and only the one value it returns owns them: dropping that value
/// unregisters them.
///
/// The write end is kept for the value's lifetime even though the handlers own a
/// clone of it. A pipe whose write end has no owner left reports end-of-file on
/// every read while staying readable, so a retained writer is what keeps the
/// registered source from waking the poller for nothing.
#[derive(Debug)]
pub struct ShutdownSignals {
    receiver: Receiver,
    writer: PipeWriter,
    registrations: [Option<SigId>; SHUTDOWN_SIGNAL_COUNT],
}

impl ShutdownSignals {
    /// Creates the self-pipe without registering a signal handler.
    ///
    /// # Errors
    ///
    /// Returns the operating system error when the pipe cannot be created or its
    /// read end cannot be switched to nonblocking mode.
    pub fn detached() -> io::Result<Self> {
        let (reader, writer) = io::pipe()?;
        let receiver = Receiver::from(OwnedFd::from(reader));
        receiver.set_nonblocking(true)?;
        Ok(Self {
            receiver,
            writer,
            registrations: [None; SHUTDOWN_SIGNAL_COUNT],
        })
    }

    /// Creates the self-pipe and registers `SIGTERM` and `SIGINT` to write into
    /// it.
    ///
    /// Each registration takes ownership of its own clone of the write end, so
    /// the handlers never share a descriptor with the value this returns.
    ///
    /// # Errors
    ///
    /// Returns [`ShutdownSignals::detached`]'s error, or the operating system
    /// error when a write-end clone or a signal registration fails. A failure
    /// part way through unregisters the signals already registered.
    pub fn install() -> io::Result<Self> {
        let mut signals = Self::detached()?;
        let Self {
            writer,
            registrations,
            ..
        } = &mut signals;
        for (registration, signal) in registrations.iter_mut().zip(SHUTDOWN_SIGNALS) {
            *registration = Some(signal_pipe::register(signal, writer.try_clone()?)?);
        }
        Ok(signals)
    }

    /// Writes one byte through the write end, the way a registered handler does.
    #[cfg(test)]
    fn trigger(&mut self) -> io::Result<()> {
        self.writer.write_all(&[SIGNAL_BYTE])
    }

    /// The registered read end, for the poller to register.
    fn receiver_mut(&mut self) -> &mut Receiver {
        &mut self.receiver
    }
}

impl Read for ShutdownSignals {
    /// Reads what the pipe holds.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::WouldBlock`] when no byte is pending, and the
    /// operating system error when the read fails.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.receiver.read(buffer)
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for registration in self.registrations.iter().flatten() {
            signal_hook::low_level::unregister(*registration);
        }
    }
}

/// The byte a shutdown signal writes into the pipe.
#[cfg(test)]
const SIGNAL_BYTE: u8 = b'X';

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
    /// When the reactor owns no deadline the wait blocks until an event arrives.
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
    /// The turn observed shutdown. No later turn does work, so a caller runs
    /// [`Reactor::shutdown`] and exits.
    ShuttingDown,
}

/// What the final shutdown flush did.
///
/// Returned by [`Reactor::shutdown`] so its caller can report a final line the
/// queue refused to hold and a flush that the deadline or a failing sink cut
/// short.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownReport {
    /// The outcome of queueing the final [`Record::Shutdown`]; a filtered or
    /// dropped record never reached the queue.
    pub record: WriteOutcome,
    /// The outcome of the bounded final drain.
    pub drain: DrainOutcome,
}

/// A poller or socket operation the reactor could not complete.
///
/// `main` maps every variant to
/// [`ExitCode::Software`](crate::exit::ExitCode::Software): a reactor failure
/// is an internal fault, never a configuration problem.
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
    /// Reading the shutdown self-pipe failed.
    Signal(ErrorKind),
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (step, kind) = match self {
            Self::Poll(kind) => ("poll", kind),
            Self::Accept(kind) => ("accept", kind),
            Self::Register(kind) => ("register", kind),
            Self::Deregister(kind) => ("deregister", kind),
            Self::Signal(kind) => ("signal", kind),
        };
        write!(formatter, "reactor {step} failed: {kind:?}")
    }
}

impl std::error::Error for ServeError {}

/// The sink's failure state between drains.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SinkHealth {
    /// The kind of the drain failure the reactor has not yet recovered from.
    failed: Option<ErrorKind>,
    /// Whether this failure episode has already attempted its record; a filtered
    /// attempt counts as handled.
    recorded: bool,
    /// The failed drains counted since startup.
    failures: u64,
}

/// The aggregate counter sample the tick record reports.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TickLedger {
    /// The instant of the last tick record, absent before the first one.
    last: Option<Instant>,
    /// The served count the last tick reported.
    served: u64,
    /// The rejected count the last tick reported.
    rejected: u64,
}

/// The single owner of the listener, the connection slots, the shutdown source,
/// and the poller.
pub struct Reactor {
    poll: Poll,
    listener: TcpListener,
    listener_registered: bool,
    accept_backlog: bool,
    plan: ResponsePlan,
    slots: Slots,
    events: Events,
    tokens: [Token; MAX_EVENTS],
    token_count: usize,
    signals: ShutdownSignals,
    shutting_down: bool,
    sink: SinkHealth,
    tick: TickLedger,
    log_retry_at: Option<Instant>,
    served: u64,
    rejected: u64,
}

impl Reactor {
    /// Registers `listener` and the shutdown pipe with the poller and reserves
    /// the event buffers.
    ///
    /// The events buffer is reserved with exactly [`MAX_EVENTS`] slots, so the
    /// token snapshot [`Reactor::wait`] takes can never drop an event the poller
    /// collected.
    ///
    /// `signals` is the caller's choice of shutdown source: [`ShutdownSignals::install`]
    /// makes a real signal reach the poller, and [`ShutdownSignals::detached`]
    /// keeps the process's handlers out of the loop so a test can drive the pipe
    /// itself. Either way the reactor owns the value, which keeps the pipe's
    /// write end open for the reactor's lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Poll`] when the poller cannot be created and
    /// [`ServeError::Register`] when the listener or the shutdown pipe cannot be
    /// registered.
    ///
    /// # Panics
    ///
    /// Panics when the poller rounds the reserved event capacity below
    /// [`MAX_EVENTS`]. The poller documents its capacity as the reserved value,
    /// so a rounding change breaks the snapshot invariant and must fail here
    /// rather than serve.
    pub fn new(
        mut listener: TcpListener,
        mut signals: ShutdownSignals,
        plan: ResponsePlan,
    ) -> Result<Self, ServeError> {
        let poll = Poll::new().map_err(|error| ServeError::Poll(error.kind()))?;
        poll.registry()
            .register(&mut listener, Token(TOKEN_LISTENER), Interest::READABLE)
            .map_err(|error| ServeError::Register(error.kind()))?;
        poll.registry()
            .register(
                signals.receiver_mut(),
                Token(TOKEN_SIGNAL),
                Interest::READABLE,
            )
            .map_err(|error| ServeError::Register(error.kind()))?;
        let events = Events::with_capacity(MAX_EVENTS);
        assert_eq!(
            events.capacity(),
            MAX_EVENTS,
            "the poller must reserve the events the token snapshot holds"
        );
        Ok(Self {
            poll,
            listener,
            listener_registered: true,
            accept_backlog: false,
            plan,
            slots: Slots::new(),
            events,
            tokens: [Token(0); MAX_EVENTS],
            token_count: 0,
            signals,
            shutting_down: false,
            sink: SinkHealth::default(),
            tick: TickLedger::default(),
            log_retry_at: None,
            served: 0,
            rejected: 0,
        })
    }

    /// Whether a shutdown request has been observed.
    ///
    /// A latched request is final: the reactor performs no further work, and
    /// [`Reactor::shutdown`] is the only step left for its owner.
    #[must_use]
    pub const fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// The requests whose planned reply the reactor wrote completely.
    ///
    /// Only a request line that parsed and selected a `200`, `404`, or `405` reply
    /// counts, and only once every byte of that reply left the reactor. A refusal
    /// is never counted here, whatever it wrote.
    ///
    /// Saturates at `u64::MAX`.
    #[must_use]
    pub const fn served(&self) -> u64 {
        self.served
    }

    /// The requests the reactor refused.
    ///
    /// A refusal is a malformed head (`400`), a truncated head whose peer closed
    /// before its terminator (`400`), an oversized head (`431`), or a connection
    /// whose absolute deadline passed before its reply completed. It is counted
    /// when the reactor decides it rather than when its reply finishes, so a
    /// `400` or `431` whose reply was cut short still counts, while a client
    /// that closes without sending a byte is neither served nor rejected.
    ///
    /// Saturates at `u64::MAX`.
    #[must_use]
    pub const fn rejected(&self) -> u64 {
        self.rejected
    }

    /// The instant the next [`TurnWait::UntilDeadline`] wait must wake by.
    ///
    /// The earliest of the occupied slot deadlines, the accept backlog a
    /// budget-cut batch left behind (reported as `now`), the log deadline the
    /// last drain scheduled (reported as `now` when a productive drain left the
    /// queue non-empty, as `now + LOG_RETRY` when it stalled or failed, and as
    /// `now + LOG_RETRY` after a failure until the retry is due), and the
    /// aggregate tick deadline while the counters moved ahead of the last
    /// sample; `None` when the reactor owns no timer, which lets the wait block
    /// until an event arrives. A deadline at or before `now` is reported as
    /// `now`, so the caller's timeout is zero and never negative.
    ///
    /// The accept backlog needs a deadline because the kernel poller reports a
    /// listening socket edge-wise: a batch stopped by [`ACCEPT_PER_TURN`] leaves
    /// connections queued that no further edge announces.
    ///
    /// `now` is the instant the caller sampled before waiting, so this value
    /// bounds the wait only; it never dates the work [`Reactor::turn`] performs.
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.accept_backlog {
            return Some(now);
        }
        [
            self.slots.earliest_deadline(),
            self.log_retry_at,
            self.tick_deadline(now),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|deadline| deadline.max(now))
    }

    /// The instant the aggregate tick is next due, while the counters moved.
    ///
    /// A tick is due [`TICK_INTERVAL`] after the last sample, and at once when no
    /// sample exists yet, which is why the deadline is `now` for a reactor whose
    /// counters just left zero.
    fn tick_deadline(&self, now: Instant) -> Option<Instant> {
        if !self.tick_moved() {
            return None;
        }
        Some(self.tick.last.map_or(now, |last| last + TICK_INTERVAL))
    }

    /// Whether the counters moved since the last tick record.
    fn tick_moved(&self) -> bool {
        self.served != self.tick.served || self.rejected != self.tick.rejected
    }

    /// Waits for readiness and snapshots the tokens [`Reactor::turn`] acts on.
    ///
    /// The timeout is [`TurnWait::Immediate`] for no wait, and otherwise the
    /// distance from `now` to [`Reactor::next_deadline`]; a
    /// [`TurnWait::UntilDeadline`] wait that owns no deadline blocks until an
    /// event arrives, and the shutdown pipe waking it is one such event.
    ///
    /// The snapshot is the whole of what a turn may act on, and every call
    /// replaces it: a second wait before the turn discards the tokens the first
    /// collected.
    ///
    /// A poll the platform reports as interrupted is retried, because the
    /// shutdown handler this reactor installs is itself a signal handler: the
    /// byte it writes makes the retried poll return at once with the signal token.
    /// The retry re-derives its timeout from [`Instant::now`] against the deadline
    /// the wait owns, so an interruption never grants a fresh full timeout, and
    /// [`TurnWait::Immediate`] keeps its zero timeout on every retry.
    ///
    /// `now` dates the wait only. A caller samples that instant before this
    /// call and samples a fresh one for [`Reactor::turn`] after it returns, so
    /// the work is never dated by an instant the wait invalidated.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Poll`] when the poller fails.
    pub fn wait(&mut self, now: Instant, wait: TurnWait) -> Result<(), ServeError> {
        let deadline = match wait {
            TurnWait::Immediate => None,
            TurnWait::UntilDeadline => self.next_deadline(now),
        };
        loop {
            let timeout = match (wait, deadline) {
                (TurnWait::Immediate, _) => Some(Duration::ZERO),
                (TurnWait::UntilDeadline, Some(deadline)) => {
                    Some(deadline.saturating_duration_since(Instant::now()))
                }
                (TurnWait::UntilDeadline, None) => None,
            };
            match self.poll.poll(&mut self.events, timeout) {
                Ok(()) => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(ServeError::Poll(error.kind())),
            }
        }
        let mut count = 0;
        for (snapshot, event) in self.tokens.iter_mut().zip(self.events.iter()) {
            *snapshot = event.token();
            count += 1;
        }
        self.token_count = count;
        Ok(())
    }

    /// Performs one bounded turn over the readiness [`Reactor::wait`] collected.
    ///
    /// `now` is the work-phase instant and must be sampled after
    /// [`Reactor::wait`] returns. Every connection accepted here takes its
    /// absolute deadline from `now`, and every sweep compares against it, so an
    /// instant captured before the wait would date a fresh connection by a
    /// moment that may already have passed. The caller's shape is
    /// `wait(before, UntilDeadline)?; turn(store, filter, sink, Instant::now())`.
    ///
    /// A turn acts on the snapshot the last [`Reactor::wait`] took and on nothing
    /// else, so a turn without a preceding wait sees an empty snapshot and runs
    /// only the work the deadlines it owns make due.
    ///
    /// The shutdown step runs first: a snapshot carrying [`TOKEN_SIGNAL`] drains
    /// the pipe, latches the request, and returns [`TurnOutcome::ShuttingDown`]
    /// before any other step, so no accept, read, or write follows a signal. A
    /// turn after the latch reports the same outcome without doing work, which is
    /// what absorbs a second signal.
    ///
    /// Otherwise the turn consumes the snapshot, accepts at most
    /// [`ACCEPT_PER_TURN`] connections, visits each connection with an event at
    /// most once, releases every connection whose deadline `now` has passed,
    /// queues the aggregate tick when it is due, and drains the log queue once.
    /// Accepts run before the sweep, which is load-bearing rather than
    /// incidental: no slot is acquired after the visit loop starts, so an index
    /// the sweep releases cannot name a fresh occupant within the same turn.
    ///
    /// The filter names the threshold the reactor's own records use; the drain
    /// only moves queued bytes, so the record paths are what spend it.
    ///
    /// # Errors
    ///
    /// Returns the [`ServeError`] of the socket, registration, or signal-pipe
    /// operation that failed; a poll failure is [`Reactor::wait`]'s alone. A
    /// failure leaves the reactor unusable, so a caller must not retry a turn
    /// that returned an error; a stream whose registration failed occupies its
    /// slot until the process exits.
    pub fn turn(
        &mut self,
        store: &mut LogStore,
        filter: LogFilter,
        sink: &mut impl Write,
        now: Instant,
    ) -> Result<TurnOutcome, ServeError> {
        let count = std::mem::take(&mut self.token_count);
        if self.tokens[..count].contains(&Token(TOKEN_SIGNAL)) {
            self.absorb_shutdown()?;
        }
        if self.shutting_down {
            return Ok(TurnOutcome::ShuttingDown);
        }
        let mut progress = false;
        if self.accept_backlog || self.tokens[..count].contains(&Token(TOKEN_LISTENER)) {
            progress |= self.accept_ready(now)?;
        }
        let mut visited = [false; MAX_CONNECTIONS];
        for position in 0..count {
            let Some(index) = connection_slot(self.tokens[position]) else {
                continue;
            };
            if visited[index] {
                continue;
            }
            visited[index] = true;
            progress |= self.visit(index)?;
        }
        progress |= self.expire(now)?;
        progress |= self.tick(store, filter, now);
        progress |= self.drain_logs(store, filter, sink, now);
        Ok(if progress {
            TurnOutcome::Progress
        } else {
            TurnOutcome::Idle
        })
    }

    /// Flushes the final counter sample and the queue within the shutdown budget.
    ///
    /// Queues exactly one [`Record::Shutdown`] carrying the counters and the drop
    /// count sampled at this instant, then runs exactly one
    /// [`sink::drain_until`] under [`FLUSH_DRAIN`]: the drain repeats its
    /// bounded turn while it progresses and the deadline has not passed, so the
    /// flush is bounded even against a reader that never drains. The sink's
    /// failure latch does not suppress it — the last attempt is the one worth
    /// making, and the deadline bounds it.
    ///
    /// The caller exits [`ExitCode::Ok`](crate::exit::ExitCode::Ok) once this
    /// returns, whatever the outcome:
    /// a shutdown the process was asked for is a successful stop. Records the
    /// deadline or a failing sink left queued are abandoned with the process.
    /// This final drain queues no [`Record::SinkFailure`] of its own: a failure
    /// an earlier turn latched already carries its own record when the filter and
    /// the queue allowed one, while a failure first observed here is visible only
    /// through the returned [`ShutdownReport`], whose fields name whether the last
    /// line was queued and why the flush ended short.
    pub fn shutdown(
        &mut self,
        store: &mut LogStore,
        filter: LogFilter,
        sink: &mut impl Write,
        now: Instant,
    ) -> ShutdownReport {
        let record = Record::Shutdown {
            served: self.served,
            rejected: self.rejected,
            dropped: store.dropped(),
        };
        let record = store.write(filter, record);
        let drain = sink::drain_until(store, sink, DrainBudget::DEFAULT, now + FLUSH_DRAIN);
        ShutdownReport { record, drain }
    }

    /// Drains whatever the shutdown pipe holds and latches the request.
    ///
    /// The latch makes the request idempotent: a second signal is drained like
    /// the first and changes nothing. A read of zero means the pipe has no writer
    /// left; the reactor keeps its own, so that answer only arises once the value
    /// is being torn down, and the latch is set regardless.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError::Signal`] when reading the pipe fails.
    fn absorb_shutdown(&mut self) -> Result<(), ServeError> {
        let mut buffer = [0u8; SIGNAL_DRAIN_BYTES];
        for _ in 0..SIGNAL_READS_PER_TURN {
            match self.signals.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                {
                    break;
                }
                Err(error) => return Err(ServeError::Signal(error.kind())),
            }
        }
        self.shutting_down = true;
        Ok(())
    }

    /// Queues the aggregate counter sample when it is due and the counters moved.
    ///
    /// The sample is due [`TICK_INTERVAL`] after the last one and immediately when
    /// there is none, so the first request after startup is reported at once and
    /// every later change waits for the interval. The snapshot advances with the
    /// attempt rather than with the write, so a queue that refused the record
    /// costs one retry per interval instead of a retry per turn.
    fn tick(&mut self, store: &mut LogStore, filter: LogFilter, now: Instant) -> bool {
        if self
            .tick_deadline(now)
            .is_none_or(|deadline| deadline > now)
        {
            return false;
        }
        let record = Record::Tick {
            served: self.served,
            rejected: self.rejected,
            dropped: store.dropped(),
        };
        self.tick.last = Some(now);
        match store.write(filter, record) {
            WriteOutcome::Written => {
                self.tick.served = self.served;
                self.tick.rejected = self.rejected;
                true
            }
            WriteOutcome::Filtered => {
                self.tick.served = self.served;
                self.tick.rejected = self.rejected;
                false
            }
            WriteOutcome::Dropped => false,
        }
    }

    /// Accepts pending connections up to the per-turn budget.
    ///
    /// The batch stops on the accept that observes an empty backlog, so a batch
    /// that ended any other way — the budget, a capacity refusal, or a run of
    /// interrupted calls — marks the backlog pending and the next turn retries
    /// without waiting for an edge the kernel already consumed.
    ///
    /// A full slot table stops the batch and drops the poller's listener interest
    /// so no further connection is accepted until a release frees a slot.
    fn accept_ready(&mut self, now: Instant) -> Result<bool, ServeError> {
        let mut progress = false;
        let mut at_would_block = false;
        let mut at_capacity = false;
        for _ in 0..ACCEPT_PER_TURN {
            match self.listener.accept() {
                Ok((stream, _address)) => {
                    let Some(index) = self.slots.acquire(stream, now) else {
                        at_capacity = true;
                        break;
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
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    at_would_block = true;
                    break;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(ServeError::Accept(error.kind())),
            }
        }
        if at_capacity {
            self.set_listener_registered(false)?;
        }
        self.accept_backlog = !(at_would_block || at_capacity);
        Ok(progress || at_capacity)
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

    /// Reads the head bytes a reading slot offers and acts on their verdict.
    ///
    /// One turn spends at most [`READ_BYTES_PER_TURN`] bytes from one
    /// connection, one stack scratch buffer of that size at a time, never held
    /// across turns and never grown. Within that budget the slot is read until
    /// its head classifies or the socket reports it has nothing more to offer.
    ///
    /// Reading past the last buffered byte is what exposes an end of input: an
    /// edge-triggered poller announces the readiness that carried a partial head
    /// once, so a peer that half-closed behind its bytes would otherwise wait out
    /// its deadline instead of earning its `400`.
    fn read_ready(&mut self, index: usize) -> Result<bool, ServeError> {
        let Some(room) = self.slots.head_room(index) else {
            return Ok(false);
        };
        let mut budget = room.min(READ_BYTES_PER_TURN);
        if budget == 0 {
            return Ok(false);
        }
        let mut buffer = [0u8; READ_BYTES_PER_TURN];
        let mut read_any = false;
        while budget > 0 {
            let Some(stream) = self.slots.stream_mut(index) else {
                return Ok(false);
            };
            let read = match stream.read(&mut buffer[..budget]) {
                Ok(0) => return self.read_eof(index),
                Ok(read) => read,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(read_any),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return self.begin_close_and_release(index),
            };
            budget -= read;
            read_any = true;
            let Some(outcome) = self.slots.read_into(index, &buffer[..read]) else {
                return Ok(false);
            };
            match outcome {
                ParseOutcome::Incomplete => {}
                ParseOutcome::Complete(request) => {
                    return self.begin_write(index, Pending::Answer(request));
                }
                ParseOutcome::Malformed(_) => return self.reject(index, Pending::BadRequest),
                ParseOutcome::TooLarge => return self.reject(index, Pending::TooLarge),
            }
        }
        Ok(read_any)
    }

    /// Acts on a read that ended at end of input in the reading slot `index`.
    ///
    /// A peer that sent at least one head byte and then closed has truncated
    /// its head before the terminator, which is a client error answered `400`
    /// exactly like a malformed line. A peer that closed without sending a byte
    /// has made no request, so it is closed without a reply.
    fn read_eof(&mut self, index: usize) -> Result<bool, ServeError> {
        if self.slots.head_room(index) == Some(HEAD_BYTES) {
            return self.begin_close_and_release(index);
        }
        self.reject(index, Pending::BadRequest)
    }

    /// Moves a reading slot into a write of `pending` and asks for writability.
    ///
    /// The reply length is read from the same [`reply_bytes`] call the write path
    /// uses, so the bytes stored as the write's total and the bytes written can
    /// never disagree.
    fn begin_write(&mut self, index: usize, pending: Pending) -> Result<bool, ServeError> {
        let total_len = reply_bytes(&self.plan, pending).len();
        if !self.slots.begin_write(index, pending, total_len) {
            return Ok(false);
        }
        self.set_connection_interest(index, Interest::WRITABLE)?;
        Ok(true)
    }

    /// Moves a reading slot into a rejected reply and counts the rejection.
    fn reject(&mut self, index: usize, pending: Pending) -> Result<bool, ServeError> {
        if !self.begin_write(index, pending)? {
            return Ok(false);
        }
        self.rejected = self.rejected.saturating_add(1);
        Ok(true)
    }

    /// Writes one bounded chunk of a writing slot's reply.
    ///
    /// [`WRITE_BYTES_PER_TURN`] covers a whole reply — the build asserts it is at
    /// least [`MAX_REPLY_BYTES`] — so this path never truncates a reply to its
    /// budget: every call offers the entire remainder. A resumed write therefore
    /// happens only when a kernel write accepts fewer bytes than it was offered,
    /// which no reactor-local test can inject, so the split path is bounded and
    /// retained but unexercised here.
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

    /// Drains the log queue once and schedules the deadline the outcome earns.
    ///
    /// A queue the sink refused, or one the sink failed to take, retries after
    /// [`LOG_RETRY`]; a productive drain that left records queued reports `now`,
    /// so the next turn drains again instead of waiting for an unrelated event.
    /// Only an empty queue or an idle drain leaves the reactor without a log
    /// deadline.
    ///
    /// A failed drain latches the sink: no drain is offered again before the
    /// retry deadline, so a broken sink costs one attempt per second instead of
    /// one per turn. The first failure of an episode attempts its one
    /// [`Record::SinkFailure`], and a filtered attempt counts as handled; a drain
    /// the sink takes clears the latch, which makes the next failure a new
    /// episode.
    fn drain_logs(
        &mut self,
        store: &mut LogStore,
        filter: LogFilter,
        sink: &mut impl Write,
        now: Instant,
    ) -> bool {
        if !self.drain_due(now) {
            return false;
        }
        match sink::drain(store, sink, DrainBudget::DEFAULT) {
            DrainOutcome::Failed { kind } => {
                self.sink.failed = Some(kind);
                self.sink.failures = self.sink.failures.saturating_add(1);
                self.log_retry_at = Some(now + LOG_RETRY);
                if !self.sink.recorded {
                    self.sink.recorded = store.write(
                        filter,
                        Record::SinkFailure {
                            failures: self.sink.failures,
                        },
                    ) != WriteOutcome::Dropped;
                }
                false
            }
            DrainOutcome::Stalled => {
                self.log_retry_at = Some(now + LOG_RETRY);
                false
            }
            DrainOutcome::Progress { .. } => {
                self.recover_sink();
                self.log_retry_at = (!store.is_empty()).then_some(now);
                true
            }
            DrainOutcome::Idle => {
                self.recover_sink();
                self.log_retry_at = None;
                false
            }
        }
    }

    /// Whether this turn may offer the queue to the sink.
    ///
    /// Only a latched failure suppresses a drain, and only until the retry
    /// deadline that failure scheduled. A stalled sink is retried every turn and
    /// kept from spinning by [`Reactor::next_deadline`].
    fn drain_due(&self, now: Instant) -> bool {
        self.sink.failed.is_none() || self.log_retry_at.is_none_or(|retry| now >= retry)
    }

    /// Clears the sink's failure latch after a drain the sink took.
    fn recover_sink(&mut self) {
        self.sink.failed = None;
        self.sink.recorded = false;
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
        ACCEPT_PER_TURN, Reactor, ServeError, ShutdownSignals, TICK_INTERVAL, TOKEN_LISTENER,
        TOKEN_SIGNAL, TurnOutcome, TurnWait, connection_slot, connection_token,
    };
    use crate::http::ResponsePlan;
    use crate::log::sink::{self, DrainOutcome, LOG_RETRY};
    use crate::log::{LogFilter, LogStore, QUEUE_SLOTS, Record, WriteOutcome};
    use crate::serve::{CONNECTION_DEADLINE, HEAD_BYTES, MAX_CONNECTIONS};
    use mio::Token;
    use mio::net::TcpListener as MioListener;
    use std::any::Any;
    use std::io::{self, ErrorKind, Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpStream};
    use std::panic::{self, AssertUnwindSafe};
    use std::time::{Duration, Instant};

    const DRIVE_DEADLINE: Duration = Duration::from_secs(2);
    const DRIVE_SLACK_TURNS: usize = 64;
    const IDLE_SLEEP: Duration = Duration::from_millis(1);

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
            let signals = ShutdownSignals::detached().unwrap();
            let plan = ResponsePlan::new(env!("CARGO_PKG_VERSION"));
            Self {
                reactor: Reactor::new(listener, signals, plan).unwrap(),
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
            self.reactor.wait(self.now, TurnWait::Immediate).unwrap();
            self.reactor
                .turn(&mut self.store, LogFilter::Info, &mut self.sink, self.now)
                .unwrap()
        }

        fn drive_until<F>(&mut self, mut done: F)
        where
            F: FnMut(&Self) -> bool,
        {
            let started = Instant::now();
            let allowance = self.drive_allowance();
            for turn in 0.. {
                if done(self) {
                    return;
                }
                assert!(
                    turn < allowance,
                    "the reactor did not reach the expected state in {turn} turns: {}",
                    self.state()
                );
                assert!(
                    started.elapsed() <= DRIVE_DEADLINE,
                    "the reactor did not reach the expected state within {DRIVE_DEADLINE:?}: {}",
                    self.state()
                );
                if self.turn() == TurnOutcome::Idle {
                    std::thread::sleep(IDLE_SLEEP);
                }
            }
        }

        fn drive_allowance(&self) -> usize {
            let work = self.store.len().saturating_add(self.reactor.slots.len());
            2usize
                .saturating_mul(work)
                .saturating_add(DRIVE_SLACK_TURNS)
        }

        fn state(&self) -> String {
            format!(
                "served={} rejected={} slots={} listener_registered={}",
                self.reactor.served(),
                self.reactor.rejected(),
                self.reactor.slots.len(),
                self.reactor.listener_registered
            )
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

    struct FailingSink {
        kind: ErrorKind,
        calls: usize,
    }

    impl Write for FailingSink {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            Err(io::Error::from(self.kind))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn panic_text(payload: &(dyn Any + Send)) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| {
                payload.downcast_ref::<&str>().map_or_else(
                    || String::from("<unnamed panic>"),
                    |text| (*text).to_owned(),
                )
            })
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
    fn a_serve_error_names_its_step() {
        let error = ServeError::Register(ErrorKind::AlreadyExists);
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
    fn a_truncated_head_at_eof_answers_400_and_releases_the_slot() {
        let mut harness = Harness::new();
        let mut client = harness.connect();
        client.write_all(b"GET / HTT").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        harness.drive_until(|harness| {
            harness.reactor.rejected() == 1 && harness.reactor.slots.is_empty()
        });
        let mut received = Vec::new();
        client.read_to_end(&mut received).unwrap();
        assert_eq!(received, plan().bad_request());
        assert_eq!(harness.reactor.rejected(), 1);
        assert_eq!(harness.reactor.served(), 0);
    }

    #[test]
    fn a_peer_that_closes_before_sending_a_byte_gets_no_reply() {
        let mut harness = Harness::new();
        let mut client = harness.connect();
        harness.drive_until(|harness| harness.reactor.slots.len() == 1);
        client.shutdown(Shutdown::Write).unwrap();
        harness.drive_until(|harness| harness.reactor.slots.is_empty());
        let mut received = Vec::new();
        client.read_to_end(&mut received).unwrap();
        assert_eq!(received, []);
        assert_eq!(harness.reactor.rejected(), 0);
        assert_eq!(harness.reactor.served(), 0);
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
    fn one_turn_accepts_the_pending_backlog_not_one_connection() {
        let mut harness = Harness::new();
        let mut clients = Vec::new();
        for _ in 0..ACCEPT_PER_TURN {
            clients.push(harness.connect());
        }
        harness.drive_until(|harness| !harness.reactor.slots.is_empty());
        let accepted = harness.reactor.slots.len();
        assert!(
            accepted > 1,
            "the first accepting turn took {accepted} of the {ACCEPT_PER_TURN} queued connections"
        );
        assert!(accepted <= ACCEPT_PER_TURN);
        harness.drive_until(|harness| harness.reactor.slots.len() == ACCEPT_PER_TURN);
        assert_eq!(clients.len(), ACCEPT_PER_TURN);
    }

    #[test]
    fn a_reopened_listener_accepts_queued_peers_without_a_fresh_connect() {
        let mut harness = Harness::new();
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            clients.push(harness.connect());
        }
        harness.drive_until(|harness| harness.reactor.slots.len() == MAX_CONNECTIONS);
        let _overflow = harness.connect();
        harness.drive_until(|harness| !harness.reactor.listener_registered);
        let mut queued = Vec::new();
        for _ in 0..3 {
            queued.push(harness.connect());
        }
        harness.advance(CONNECTION_DEADLINE + Duration::from_nanos(1));
        let _ = harness.turn();
        assert!(harness.reactor.slots.is_empty());
        assert!(harness.reactor.listener_registered);
        assert!(harness.reactor.accept_backlog);
        let _ = harness.turn();
        assert!(
            !harness.reactor.slots.is_empty(),
            "the reopened listener admitted no queued peer"
        );
        harness.drive_until(|harness| harness.reactor.slots.len() == queued.len());
        assert_eq!(queued.len(), 3);
        assert_eq!(clients.len(), MAX_CONNECTIONS);
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
        assert_eq!(
            harness.reactor.next_deadline(harness.now),
            Some(harness.now)
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
    fn a_connection_accepted_after_a_long_idle_wait_gets_the_work_instant_deadline() {
        let mut harness = Harness::new();
        let _client = harness.connect();
        let before = harness.now;
        harness
            .reactor
            .wait(before, TurnWait::UntilDeadline)
            .unwrap();
        let fresh = before + Duration::from_secs(60);
        let outcome = harness
            .reactor
            .turn(
                &mut harness.store,
                LogFilter::Info,
                &mut harness.sink,
                fresh,
            )
            .unwrap();
        assert_eq!(outcome, TurnOutcome::Progress);
        assert_eq!(harness.reactor.slots.len(), 1);
        assert_eq!(
            harness.reactor.next_deadline(fresh),
            Some(fresh + CONNECTION_DEADLINE)
        );
        let at_deadline = fresh + CONNECTION_DEADLINE;
        harness
            .reactor
            .wait(at_deadline, TurnWait::Immediate)
            .unwrap();
        let outcome = harness
            .reactor
            .turn(
                &mut harness.store,
                LogFilter::Info,
                &mut harness.sink,
                at_deadline,
            )
            .unwrap();
        assert_eq!(outcome, TurnOutcome::Idle);
        assert_eq!(harness.reactor.slots.len(), 1);
        assert_eq!(harness.reactor.rejected(), 0);
        let late = at_deadline + Duration::from_nanos(1);
        harness.reactor.wait(late, TurnWait::Immediate).unwrap();
        let _ = harness
            .reactor
            .turn(&mut harness.store, LogFilter::Info, &mut harness.sink, late)
            .unwrap();
        assert!(harness.reactor.slots.is_empty());
        assert_eq!(harness.reactor.rejected(), 1);
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
        let outcome = {
            harness
                .reactor
                .wait(harness.now, TurnWait::Immediate)
                .unwrap();
            harness
                .reactor
                .turn(
                    &mut harness.store,
                    LogFilter::Info,
                    &mut StalledSink,
                    harness.now,
                )
                .unwrap()
        };
        assert_eq!(outcome, TurnOutcome::Idle);
        assert_eq!(
            harness.reactor.next_deadline(harness.now),
            Some(harness.now + LOG_RETRY)
        );
    }

    #[test]
    fn a_signal_requests_shutdown_and_the_latch_absorbs_the_second() {
        let mut harness = Harness::new();
        let _client = harness.connect();
        assert!(!harness.reactor.is_shutting_down());
        harness.reactor.signals.trigger().unwrap();
        assert_eq!(harness.turn(), TurnOutcome::ShuttingDown);
        assert!(harness.reactor.is_shutting_down());
        assert_eq!(harness.reactor.slots.len(), 0);
        harness.reactor.signals.trigger().unwrap();
        assert_eq!(harness.turn(), TurnOutcome::ShuttingDown);
        assert_eq!(harness.reactor.served(), 0);
        assert_eq!(harness.reactor.rejected(), 0);
    }

    #[test]
    fn shutdown_flushes_one_shutdown_record_within_the_deadline() {
        let mut harness = Harness::new();
        harness.reactor.served = 3;
        harness.reactor.rejected = 2;
        assert_eq!(
            harness
                .store
                .write(LogFilter::Info, Record::FilterDefaulted),
            WriteOutcome::Written
        );
        harness.now = Instant::now();
        let report = harness.reactor.shutdown(
            &mut harness.store,
            LogFilter::Info,
            &mut harness.sink,
            harness.now,
        );
        assert_eq!(report.record, WriteOutcome::Written);
        assert_eq!(report.drain, DrainOutcome::Idle);
        assert!(harness.store.is_empty());
        assert_eq!(
            harness.sink.as_slice(),
            b"level=warn kind=filter_default\r\nlevel=info kind=shutdown served=3 rejected=2 dropped=0\r\n"
        );
    }

    #[test]
    fn shutdown_reports_a_final_record_the_queue_refused() {
        let mut harness = Harness::new();
        for served in 0..u64::try_from(QUEUE_SLOTS).unwrap() {
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
        harness.now = Instant::now();
        let report = harness.reactor.shutdown(
            &mut harness.store,
            LogFilter::Info,
            &mut harness.sink,
            harness.now,
        );
        assert_eq!(report.record, WriteOutcome::Dropped);
        assert_eq!(harness.store.dropped(), 1);
    }

    #[test]
    fn the_tick_reports_once_per_interval_and_stops_when_idle() {
        let mut harness = Harness::new();
        assert_eq!(harness.reactor.next_deadline(harness.now), None);
        harness.exchange(b"GET / HTTP/1.1\r\n\r\n");
        assert_eq!(
            harness.sink.as_slice(),
            b"level=info kind=tick served=1 rejected=0 dropped=0\r\n"
        );
        let tick_at = harness.reactor.tick.last.unwrap();
        assert_eq!(harness.reactor.next_deadline(harness.now), None);
        harness.advance(TICK_INTERVAL / 2);
        let received = harness.exchange_driven(b"GET / HTTP/1.1\r\n\r\n", |harness| {
            harness.reactor.served() == 2 && harness.reactor.slots.is_empty()
        });
        assert_eq!(received, plan().ok_get());
        assert_eq!(
            harness.sink.as_slice(),
            b"level=info kind=tick served=1 rejected=0 dropped=0\r\n"
        );
        assert_eq!(
            harness.reactor.next_deadline(harness.now),
            Some(tick_at + TICK_INTERVAL)
        );
        harness.advance(TICK_INTERVAL / 2);
        assert_eq!(
            harness.reactor.next_deadline(harness.now),
            Some(harness.now)
        );
        assert_eq!(harness.turn(), TurnOutcome::Progress);
        assert_eq!(
            harness.sink.as_slice(),
            b"level=info kind=tick served=1 rejected=0 dropped=0\r\nlevel=info kind=tick served=2 rejected=0 dropped=0\r\n"
        );
        assert_eq!(harness.reactor.next_deadline(harness.now), None);
        assert_eq!(harness.turn(), TurnOutcome::Idle);
        assert_eq!(
            harness.sink.as_slice(),
            b"level=info kind=tick served=1 rejected=0 dropped=0\r\nlevel=info kind=tick served=2 rejected=0 dropped=0\r\n"
        );
    }

    #[test]
    fn a_failed_drain_is_latched_and_reported_once_per_episode() {
        let mut harness = Harness::new();
        assert_eq!(
            harness
                .store
                .write(LogFilter::Info, Record::FilterDefaulted),
            WriteOutcome::Written
        );
        let mut sink = FailingSink {
            kind: ErrorKind::BrokenPipe,
            calls: 0,
        };
        let failed_at = harness.now;
        harness
            .reactor
            .wait(failed_at, TurnWait::Immediate)
            .unwrap();
        assert_eq!(
            harness
                .reactor
                .turn(&mut harness.store, LogFilter::Info, &mut sink, failed_at)
                .unwrap(),
            TurnOutcome::Idle
        );
        assert_eq!(sink.calls, 1);
        assert_eq!(harness.store.len(), 2);
        assert_eq!(harness.reactor.sink.failed, Some(ErrorKind::BrokenPipe));
        assert_eq!(
            harness.reactor.next_deadline(failed_at),
            Some(failed_at + LOG_RETRY)
        );
        harness.advance(LOG_RETRY / 2);
        harness
            .reactor
            .wait(harness.now, TurnWait::Immediate)
            .unwrap();
        assert_eq!(
            harness
                .reactor
                .turn(&mut harness.store, LogFilter::Info, &mut sink, harness.now)
                .unwrap(),
            TurnOutcome::Idle
        );
        assert_eq!(sink.calls, 1);
        harness.advance(LOG_RETRY / 2);
        harness
            .reactor
            .wait(harness.now, TurnWait::Immediate)
            .unwrap();
        assert_eq!(
            harness
                .reactor
                .turn(&mut harness.store, LogFilter::Info, &mut sink, harness.now)
                .unwrap(),
            TurnOutcome::Idle
        );
        assert_eq!(sink.calls, 2);
        assert_eq!(harness.store.len(), 2);
        assert_eq!(harness.reactor.sink.failures, 2);
        let mut recovered = Vec::new();
        harness.advance(LOG_RETRY);
        harness
            .reactor
            .wait(harness.now, TurnWait::Immediate)
            .unwrap();
        assert_eq!(
            harness
                .reactor
                .turn(
                    &mut harness.store,
                    LogFilter::Info,
                    &mut recovered,
                    harness.now
                )
                .unwrap(),
            TurnOutcome::Progress
        );
        assert!(harness.store.is_empty());
        assert!(harness.reactor.sink.failed.is_none());
        assert!(!harness.reactor.sink.recorded);
        assert_eq!(harness.reactor.next_deadline(harness.now), None);
        assert_eq!(
            recovered.as_slice(),
            b"level=warn kind=filter_default\r\nlevel=error kind=sink_failure failures=1\r\n"
        );
    }

    #[test]
    fn a_starved_harness_fails_in_bounded_time_with_its_state() {
        let mut harness = Harness::new();
        let allowance = harness.drive_allowance();
        assert!(
            allowance >= DRIVE_SLACK_TURNS,
            "the starvation predicate must expect real work, but its allowance is {allowance}"
        );
        let started = Instant::now();
        let caught = panic::catch_unwind(AssertUnwindSafe(|| {
            harness.drive_until(|harness| harness.reactor.served() == u64::MAX);
        }));
        let message = panic_text(caught.expect_err("the starved drive must give up").as_ref());
        assert!(started.elapsed() < DRIVE_DEADLINE);
        assert!(
            message.contains(&format!("in {allowance} turns")),
            "diagnostics {message:?} do not report the {allowance}-turn exhaustion"
        );
        for field in [
            "served=0",
            "rejected=0",
            "slots=0",
            "listener_registered=true",
        ] {
            assert!(
                message.contains(field),
                "diagnostics {message:?} lack {field}"
            );
        }
    }
}

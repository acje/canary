//! The shutdown self-pipe the reactor's poller watches.
//!
//! [`ShutdownSignals`] owns the read end of a pipe and the write end a
//! registered handler writes one byte to. The read end is the mio source
//! [`Reactor::new`](super::reactor::Reactor::new) registers at
//! [`TOKEN_SIGNAL`](super::reactor::TOKEN_SIGNAL), so a signal wakes the poller
//! and the reactor reads the request where an ordinary turn can act on it.

use std::io::{self, PipeWriter, Read};
use std::os::fd::OwnedFd;

use mio::unix::pipe::Receiver;
use signal_hook::SigId;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::low_level::pipe as signal_pipe;

/// The number of signals [`ShutdownSignals`] subscribes to.
const SHUTDOWN_SIGNAL_COUNT: usize = 2;

/// The signals that request a shutdown.
const SHUTDOWN_SIGNALS: [libc::c_int; SHUTDOWN_SIGNAL_COUNT] = [SIGTERM, SIGINT];

/// The byte a shutdown signal writes into the pipe.
#[cfg(test)]
const SIGNAL_BYTE: u8 = b'X';

/// The read end of the shutdown self-pipe and the write end the handlers feed.
///
/// The read end is the mio source [`Reactor::new`](super::reactor::Reactor::new)
/// registers at [`TOKEN_SIGNAL`](super::reactor::TOKEN_SIGNAL): a registered
/// handler writes one byte into the pipe, so the poller wakes with the token and
/// the reactor reads the request where an ordinary turn can act on it.
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
    pub(crate) fn trigger(&mut self) -> io::Result<()> {
        use std::io::Write as _;
        self.writer.write_all(&[SIGNAL_BYTE])
    }

    /// The registered read end, for the poller to register.
    pub(crate) fn receiver_mut(&mut self) -> &mut Receiver {
        &mut self.receiver
    }
}

impl Read for ShutdownSignals {
    /// Reads what the pipe holds.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::WouldBlock`](io::ErrorKind::WouldBlock) when no byte
    /// is pending, and the operating system error when the read fails.
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

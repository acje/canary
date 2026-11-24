//! Fixed connection-slot table and single-owner reactor for the service.
//!
//! [`slots::Slots`] reserves [`MAX_CONNECTIONS`] slots once at startup and never
//! grows. Each slot bounds one request head to [`HEAD_BYTES`] and one connection
//! to [`CONNECTION_DEADLINE`] from acceptance. The table stores connection state
//! only; rendered reply bytes stay owned by the reactor's reply plan, so a slot
//! never holds a reply buffer of its own.
//!
//! [`reactor::Reactor`] owns the listening socket, the slot table, the shutdown
//! self-pipe, and the poller. A turn is [`reactor::Reactor::wait`] — the poll
//! that snapshots the readiness — followed by [`reactor::Reactor::turn`] — a
//! fixed set of bounded steps dated by the instant the caller samples after the
//! wait returns — so no turn can block on a socket, on the signal pipe, or on
//! the log sink. [`reactor::ShutdownSignals`] owns the pipe: its installed form
//! is the reactor's only path from a `SIGTERM` or `SIGINT` to shutdown, and its
//! detached form keeps process handlers out of a test.

pub mod reactor;
pub mod slots;

pub use reactor::{Reactor, ServeError, ShutdownSignals, TurnOutcome, TurnWait};
pub use slots::{Pending, Phase, ReadOutcome, Slots, WriteProgress};

use std::time::Duration;

/// The number of connection slots reserved at startup.
pub const MAX_CONNECTIONS: usize = 64;

/// The largest request head one slot buffers, in bytes.
pub const HEAD_BYTES: usize = crate::http::MAX_HEAD_BYTES;

/// The absolute lifetime of one accepted connection.
pub const CONNECTION_DEADLINE: Duration = Duration::from_secs(5);

/// The bytes a slot may spend beyond its head buffer, covering the stream
/// handle, the absolute deadline, the connection tag, the write cursor, and the
/// pending-reply tag.
pub(crate) const SLOT_METADATA_BYTES: usize = 128;

/// The bytes the whole slot table may occupy.
pub const SLOT_BUDGET_BYTES: usize = 128 * 1024;

const _: () = assert!(MAX_CONNECTIONS * (HEAD_BYTES + SLOT_METADATA_BYTES) <= SLOT_BUDGET_BYTES);

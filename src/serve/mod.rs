//! Fixed connection-slot table and single-owner reactor for the service.
//!
//! [`slots::Slots`] reserves [`MAX_CONNECTIONS`] slots once at startup and never
//! grows. Each slot bounds one request head to [`HEAD_BYTES`] and one connection
//! to [`CONNECTION_DEADLINE`] from acceptance. The table stores connection state
//! only; rendered reply bytes stay owned by the reactor's reply plan, so a slot
//! never holds a reply buffer of its own.
//!
//! [`reactor::Reactor`] owns the listening socket, the slot table, and the
//! poller. Every [`reactor::Reactor::turn`] call does a fixed set of bounded
//! steps, so no turn can block on a socket or on the log sink.

pub mod reactor;
pub mod slots;

pub use reactor::{Reactor, ServeError, TurnOutcome, TurnWait};
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

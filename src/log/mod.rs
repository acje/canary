//! Bounded log queue for the single-owner reactor.
//!
//! Storage is a ring of [`QUEUE_SLOTS`] fixed [`SLOT_BYTES`] slots reserved once
//! by [`LogStore::new`]. A record is rendered into one slot before the queue is
//! touched, so a record either occupies a whole slot or is dropped; a dropped
//! record never leaves a partial line behind.
//!
//! Every line is `level=<token> kind=<kind>[ <key>=<value>]...\r\n` with exactly
//! this field order per kind:
//!
//! - `level=info kind=startup bind=<address>`, where the address renders through
//!   `SocketAddr`'s `Display` implementation.
//! - `level=info kind=tick served=<count> rejected=<count> dropped=<count>`.
//! - `level=info kind=shutdown served=<count> rejected=<count> dropped=<count>`.
//! - `level=warn kind=filter_default` with no fields.
//!
//! Values are never free-form: an address is `SocketAddr`'s `Display` form and a
//! counter is unsigned decimal, so a value cannot contain a space, `=`, or a
//! line break. The grammar therefore needs no escaping.
//!
//! [`LogFilter::allows`] is the only filter decision. Startup, tick, and
//! shutdown records are [`Level::Info`]; the filter-defaulted notice is
//! [`Level::Warn`]; a sink-failure record joins it at [`Level::Warn`] in the
//! next increment.

use std::fmt::{self, Write as _};
use std::net::SocketAddr;

/// The number of records the queue holds, a power of two so the ring index is a
/// mask.
pub const QUEUE_SLOTS: usize = 256;

/// The bytes reserved for one rendered record.
pub const SLOT_BYTES: usize = 512;

const SLOT_MASK: usize = QUEUE_SLOTS - 1;

/// The bytes reserved for the whole ring, in one allocation.
const QUEUE_BYTES: usize = QUEUE_SLOTS * SLOT_BYTES;

/// The digits a `u64` value may need in decimal.
const MAX_DECIMAL_U64: usize = 20;

/// The longest `Ipv6Addr` `Display` form: eight four-digit groups and seven
/// colons, without the scope identifier, which `SocketAddr` never renders.
const MAX_IPV6_TEXT: usize = 8 * 4 + 7;

/// The longest port decimal text.
const MAX_PORT_TEXT: usize = 5;

/// The longest `SocketAddr` `Display` form: a bracketed `Ipv6Addr`, a colon, and
/// the port.
const MAX_SOCKET_ADDR_TEXT: usize =
    "[".len() + MAX_IPV6_TEXT + "]".len() + ":".len() + MAX_PORT_TEXT;

const STARTUP_PREFIX_BYTES: usize = "level=info kind=startup bind=".len() + "\r\n".len();

const COUNTER_PREFIX_BYTES: usize =
    "level=info kind=shutdown served= rejected= dropped=".len() + "\r\n".len();

const FILTER_DEFAULT_BYTES: usize = "level=warn kind=filter_default\r\n".len();

/// The largest startup line: the prefix plus a maximal address.
const MAX_STARTUP_BYTES: usize = STARTUP_PREFIX_BYTES + MAX_SOCKET_ADDR_TEXT;

/// The largest counter line: three maximal `u64` fields.
const MAX_COUNTER_BYTES: usize = COUNTER_PREFIX_BYTES + 3 * MAX_DECIMAL_U64;

const _: () = assert!(QUEUE_SLOTS.is_power_of_two());
const _: () = assert!(QUEUE_BYTES == QUEUE_SLOTS * SLOT_BYTES);
const _: () = assert!(MAX_STARTUP_BYTES <= SLOT_BYTES);
const _: () = assert!(MAX_COUNTER_BYTES <= SLOT_BYTES);
const _: () = assert!(FILTER_DEFAULT_BYTES <= SLOT_BYTES);

/// The severity a record carries, the only input to the filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Level {
    /// Informational lifecycle events.
    Info,
    /// Recoverable degradations that the operator should see by default.
    Warn,
    /// Failures that always survive the filter except [`LogFilter::Off`].
    Error,
}

impl Level {
    const fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// The closed `RUST_LOG` severity filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogFilter {
    /// Suppress every record.
    Off,
    /// Keep [`Level::Error`] records.
    Error,
    /// Keep [`Level::Warn`] and [`Level::Error`] records.
    Warn,
    /// Keep every record.
    Info,
}

impl LogFilter {
    /// Interprets a raw `RUST_LOG` value, where `None` means the variable is
    /// unset.
    ///
    /// Exactly `off`, `error`, `warn`, and `info` are recognised, compared
    /// ASCII-case-insensitively after trimming surrounding ASCII whitespace.
    /// Every other value, including the empty string and comma-separated
    /// directive lists, yields [`FilterParse::Defaulted`].
    #[must_use]
    pub fn parse(raw: Option<&str>) -> FilterParse {
        let Some(raw) = raw else {
            return FilterParse::Defaulted;
        };
        let value = raw.trim_matches(|character: char| character.is_ascii_whitespace());
        if value.eq_ignore_ascii_case("off") {
            FilterParse::Accepted(Self::Off)
        } else if value.eq_ignore_ascii_case("error") {
            FilterParse::Accepted(Self::Error)
        } else if value.eq_ignore_ascii_case("warn") {
            FilterParse::Accepted(Self::Warn)
        } else if value.eq_ignore_ascii_case("info") {
            FilterParse::Accepted(Self::Info)
        } else {
            FilterParse::Defaulted
        }
    }

    /// Whether a record at `level` passes this filter.
    #[must_use]
    pub const fn allows(self, level: Level) -> bool {
        match self {
            Self::Off => false,
            Self::Error => matches!(level, Level::Error),
            Self::Warn => matches!(level, Level::Warn | Level::Error),
            Self::Info => true,
        }
    }
}

/// The outcome of interpreting a raw `RUST_LOG` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterParse {
    /// A recognised name selected this filter explicitly.
    Accepted(LogFilter),
    /// The value was absent or unrecognised, so [`LogFilter::Info`] applies and
    /// the caller should record [`Record::FilterDefaulted`].
    Defaulted,
}

impl FilterParse {
    /// The filter to apply, where a defaulted value means [`LogFilter::Info`].
    #[must_use]
    pub const fn filter(self) -> LogFilter {
        match self {
            Self::Accepted(filter) => filter,
            Self::Defaulted => LogFilter::Info,
        }
    }

    /// Whether the raw value was absent or unrecognised.
    #[must_use]
    pub const fn defaulted(self) -> bool {
        matches!(self, Self::Defaulted)
    }
}

/// A lifecycle event the reactor records, with no free-form text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Record {
    /// The listener bound this address at startup.
    Startup {
        /// The address the listener bound.
        bind: SocketAddr,
    },
    /// A periodic counter sample.
    Tick {
        /// Requests served since startup.
        served: u64,
        /// Requests rejected since startup.
        rejected: u64,
        /// Log records dropped since startup.
        dropped: u64,
    },
    /// The final counter sample before shutdown.
    Shutdown {
        /// Requests served since startup.
        served: u64,
        /// Requests rejected since startup.
        rejected: u64,
        /// Log records dropped since startup.
        dropped: u64,
    },
    /// `RUST_LOG` was absent or unrecognised, so the filter defaulted to info.
    FilterDefaulted,
}

impl Record {
    /// The severity this record carries.
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            Self::Startup { .. } | Self::Tick { .. } | Self::Shutdown { .. } => Level::Info,
            Self::FilterDefaulted => Level::Warn,
        }
    }

    fn render(self, slot: &mut Slot) -> fmt::Result {
        let level = self.level().name();
        match self {
            Self::Startup { bind } => write!(slot, "level={level} kind=startup bind={bind}\r\n"),
            Self::Tick {
                served,
                rejected,
                dropped,
            } => write!(
                slot,
                "level={level} kind=tick served={served} rejected={rejected} dropped={dropped}\r\n"
            ),
            Self::Shutdown {
                served,
                rejected,
                dropped,
            } => write!(
                slot,
                "level={level} kind=shutdown served={served} rejected={rejected} dropped={dropped}\r\n"
            ),
            Self::FilterDefaulted => write!(slot, "level={level} kind=filter_default\r\n"),
        }
    }
}

/// The outcome of one [`LogStore::write`] call.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
    /// The record was encoded and appended to the queue.
    Written,
    /// The filter suppressed the record; the queue and drop counter are
    /// untouched.
    Filtered,
    /// The queue was full or the record did not fit one slot; the queue is
    /// unchanged and the drop counter advanced.
    Dropped,
}

/// Why a [`LogStore`] could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogStoreError {
    /// The fixed queue buffer could not be reserved during startup.
    Reserve,
}

impl fmt::Display for LogStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Reserve => "the fixed log queue buffer could not be reserved",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for LogStoreError {}

/// A fixed-capacity log queue with a durable partial-consume cursor.
///
/// The ring holds at most [`QUEUE_SLOTS`] records of at most [`SLOT_BYTES`] bytes
/// each. The byte buffer is reserved once by [`LogStore::new`] and never grows;
/// [`LogStore::write`] rejects a record rather than allocate.
pub struct LogStore {
    bytes: Vec<u8>,
    lengths: [usize; QUEUE_SLOTS],
    head: usize,
    front_offset: usize,
    len: usize,
    dropped: u64,
}

impl LogStore {
    /// Reserves the fixed queue buffer.
    ///
    /// # Errors
    ///
    /// Returns [`LogStoreError::Reserve`] when the operating system cannot
    /// provide [`QUEUE_BYTES`] bytes at startup.
    pub fn new() -> Result<Self, LogStoreError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(QUEUE_BYTES)
            .map_err(|_| LogStoreError::Reserve)?;
        bytes.resize(QUEUE_BYTES, 0);
        Ok(Self {
            bytes,
            lengths: [0; QUEUE_SLOTS],
            head: 0,
            front_offset: 0,
            len: 0,
            dropped: 0,
        })
    }

    /// Encodes `record` into one slot and appends it when the filter allows it.
    ///
    /// A [`WriteOutcome::Filtered`] result leaves both the queue and the drop
    /// counter untouched. A [`WriteOutcome::Dropped`] result leaves the queue
    /// untouched and advances [`LogStore::dropped`] by one.
    pub fn write(&mut self, filter: LogFilter, record: Record) -> WriteOutcome {
        if !filter.allows(record.level()) {
            return WriteOutcome::Filtered;
        }
        if self.len == QUEUE_SLOTS {
            self.record_drop();
            return WriteOutcome::Dropped;
        }
        let mut slot = Slot::new();
        if record.render(&mut slot).is_err() {
            self.record_drop();
            return WriteOutcome::Dropped;
        }
        let index = (self.head + self.len) & SLOT_MASK;
        let start = index * SLOT_BYTES;
        self.bytes[start..start + slot.len].copy_from_slice(slot.as_bytes());
        self.lengths[index] = slot.len;
        self.len += 1;
        WriteOutcome::Written
    }

    /// The bytes of the front record that the sink has not yet accepted.
    ///
    /// Returns `None` when the queue is empty.
    #[must_use]
    pub fn unwritten_front(&self) -> Option<&[u8]> {
        if self.len == 0 {
            return None;
        }
        let start = self.head * SLOT_BYTES + self.front_offset;
        let end = self.head * SLOT_BYTES + self.lengths[self.head];
        Some(&self.bytes[start..end])
    }

    /// Records that `written` bytes of the front record left the queue.
    ///
    /// Consuming exactly the remaining bytes removes the record and makes the
    /// next one the front; consuming fewer keeps the remainder as the front.
    /// Consuming zero bytes is a no-op. Returns `false`, changing nothing, when
    /// `written` exceeds the remaining bytes of the front record.
    pub fn consume_front(&mut self, written: usize) -> bool {
        if self.len == 0 {
            return written == 0;
        }
        let remaining = self.lengths[self.head] - self.front_offset;
        if written > remaining {
            return false;
        }
        if written == remaining {
            self.lengths[self.head] = 0;
            self.head = (self.head + 1) & SLOT_MASK;
            self.front_offset = 0;
            self.len -= 1;
        } else {
            self.front_offset += written;
        }
        true
    }

    /// The number of records queued.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the queue holds no records.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The count of records rejected since construction, saturating at
    /// `u64::MAX`.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    fn record_drop(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }
}

/// A fixed-capacity rendering target that fails instead of overflowing.
struct Slot {
    bytes: [u8; SLOT_BYTES],
    len: usize,
}

impl Slot {
    const fn new() -> Self {
        Self {
            bytes: [0; SLOT_BYTES],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Write for Slot {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        if end > SLOT_BYTES {
            return Err(fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FilterParse, Level, LogFilter, LogStore, LogStoreError, QUEUE_SLOTS, Record, SLOT_BYTES,
        Slot, WriteOutcome,
    };
    use std::fmt::Write as _;
    use std::net::SocketAddr;

    fn store() -> LogStore {
        LogStore::new().unwrap()
    }

    fn address(raw: &str) -> SocketAddr {
        raw.parse().unwrap()
    }

    fn front(store: &LogStore) -> Vec<u8> {
        store.unwritten_front().unwrap().to_vec()
    }

    #[test]
    fn startup_record_renders_ipv4_exact_bytes() {
        let mut store = store();
        let bind = address("0.0.0.0:8080");
        assert_eq!(
            store.write(LogFilter::Info, Record::Startup { bind }),
            WriteOutcome::Written
        );
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=info kind=startup bind=0.0.0.0:8080\r\n"
        );
    }

    #[test]
    fn startup_record_renders_ipv6_exact_bytes() {
        let mut store = store();
        let bind = address("[::]:8080");
        assert_eq!(
            store.write(LogFilter::Info, Record::Startup { bind }),
            WriteOutcome::Written
        );
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=info kind=startup bind=[::]:8080\r\n"
        );
    }

    #[test]
    fn tick_record_renders_exact_bytes_in_field_order() {
        let mut store = store();
        let record = Record::Tick {
            served: 1,
            rejected: 2,
            dropped: 3,
        };
        assert_eq!(store.write(LogFilter::Info, record), WriteOutcome::Written);
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=info kind=tick served=1 rejected=2 dropped=3\r\n"
        );
    }

    #[test]
    fn shutdown_record_renders_exact_bytes_in_field_order() {
        let mut store = store();
        let record = Record::Shutdown {
            served: 4,
            rejected: 5,
            dropped: 6,
        };
        assert_eq!(store.write(LogFilter::Info, record), WriteOutcome::Written);
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=info kind=shutdown served=4 rejected=5 dropped=6\r\n"
        );
    }

    #[test]
    fn filter_defaulted_record_renders_exact_bytes() {
        let mut store = store();
        assert_eq!(
            store.write(LogFilter::Info, Record::FilterDefaulted),
            WriteOutcome::Written
        );
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=warn kind=filter_default\r\n"
        );
    }

    #[test]
    fn maximal_counters_fit_one_slot() {
        let mut store = store();
        let record = Record::Tick {
            served: u64::MAX,
            rejected: u64::MAX,
            dropped: u64::MAX,
        };
        assert_eq!(store.write(LogFilter::Info, record), WriteOutcome::Written);
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=info kind=tick served=18446744073709551615 rejected=18446744073709551615 dropped=18446744073709551615\r\n"
        );
    }

    #[test]
    fn slot_writer_refuses_overflow_without_partial_write() {
        let mut slot = Slot::new();
        let full = "x".repeat(SLOT_BYTES);
        assert!(slot.write_str(&full).is_ok());
        assert_eq!(slot.len, SLOT_BYTES);
        assert!(slot.write_str("y").is_err());
        assert_eq!(slot.len, SLOT_BYTES);
        assert_eq!(&slot.bytes[..4], b"xxxx");
    }

    #[test]
    fn record_levels_are_info_except_the_filter_notice() {
        assert_eq!(Record::FilterDefaulted.level(), Level::Warn);
        assert_eq!(
            Record::Startup {
                bind: address("0.0.0.0:1")
            }
            .level(),
            Level::Info
        );
        assert_eq!(
            Record::Tick {
                served: 0,
                rejected: 0,
                dropped: 0
            }
            .level(),
            Level::Info
        );
        assert_eq!(
            Record::Shutdown {
                served: 0,
                rejected: 0,
                dropped: 0
            }
            .level(),
            Level::Info
        );
    }

    #[test]
    fn filter_allows_exactly_its_levels() {
        let cases = [
            (LogFilter::Off, [false, false, false]),
            (LogFilter::Error, [false, false, true]),
            (LogFilter::Warn, [false, true, true]),
            (LogFilter::Info, [true, true, true]),
        ];
        for (filter, expected) in cases {
            let actual = [Level::Info, Level::Warn, Level::Error].map(|level| filter.allows(level));
            assert_eq!(actual, expected, "filter {filter:?}");
        }
    }

    #[test]
    fn filter_parse_recognises_exact_names_case_insensitively() {
        assert_eq!(
            LogFilter::parse(Some("off")),
            FilterParse::Accepted(LogFilter::Off)
        );
        assert_eq!(
            LogFilter::parse(Some("error")),
            FilterParse::Accepted(LogFilter::Error)
        );
        assert_eq!(
            LogFilter::parse(Some("warn")),
            FilterParse::Accepted(LogFilter::Warn)
        );
        assert_eq!(
            LogFilter::parse(Some("info")),
            FilterParse::Accepted(LogFilter::Info)
        );
        assert_eq!(
            LogFilter::parse(Some("  WaRn\t")),
            FilterParse::Accepted(LogFilter::Warn)
        );
        assert_eq!(
            LogFilter::parse(Some("INFO")),
            FilterParse::Accepted(LogFilter::Info)
        );
        assert!(!LogFilter::parse(Some("warn")).defaulted());
    }

    #[test]
    fn filter_parse_defaults_to_info_for_absent_or_unknown() {
        for raw in [
            None,
            Some(""),
            Some("debug"),
            Some("trace"),
            Some("warning"),
            Some("info,debug"),
            Some("="),
        ] {
            let parsed = LogFilter::parse(raw);
            assert_eq!(parsed, FilterParse::Defaulted, "raw {raw:?}");
            assert_eq!(parsed.filter(), LogFilter::Info);
            assert!(parsed.defaulted());
        }
    }

    #[test]
    fn filtered_record_touches_neither_queue_nor_drop_counter() {
        let mut store = store();
        let record = Record::Startup {
            bind: address("0.0.0.0:8080"),
        };
        assert_eq!(
            store.write(LogFilter::Error, record),
            WriteOutcome::Filtered
        );
        assert_eq!(store.dropped(), 0);
        assert!(store.is_empty());
        assert!(store.unwritten_front().is_none());
    }

    #[test]
    fn warn_filter_suppresses_info_and_keeps_warn() {
        let mut store = store();
        let tick = Record::Tick {
            served: 0,
            rejected: 0,
            dropped: 0,
        };
        assert_eq!(store.write(LogFilter::Warn, tick), WriteOutcome::Filtered);
        assert_eq!(
            store.write(LogFilter::Warn, Record::FilterDefaulted),
            WriteOutcome::Written
        );
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=warn kind=filter_default\r\n"
        );
    }

    #[test]
    fn off_filter_suppresses_every_level() {
        let mut store = store();
        assert_eq!(
            store.write(LogFilter::Off, Record::FilterDefaulted),
            WriteOutcome::Filtered
        );
        assert_eq!(
            store.write(
                LogFilter::Off,
                Record::Tick {
                    served: 0,
                    rejected: 0,
                    dropped: 0
                }
            ),
            WriteOutcome::Filtered
        );
        assert_eq!(store.dropped(), 0);
        assert!(store.is_empty());
    }

    #[test]
    fn full_queue_drops_and_counts_without_touching_the_front() {
        let mut store = store();
        let capacity = u64::try_from(QUEUE_SLOTS).unwrap();
        for served in 0..capacity {
            let record = Record::Tick {
                served,
                rejected: 0,
                dropped: 0,
            };
            assert_eq!(store.write(LogFilter::Info, record), WriteOutcome::Written);
        }
        assert_eq!(store.len(), QUEUE_SLOTS);
        let unchanged = front(&store);
        let rejected = Record::Tick {
            served: u64::MAX,
            rejected: 0,
            dropped: 0,
        };
        assert_eq!(
            store.write(LogFilter::Info, rejected),
            WriteOutcome::Dropped
        );
        assert_eq!(store.dropped(), 1);
        assert_eq!(store.len(), QUEUE_SLOTS);
        assert_eq!(store.unwritten_front().unwrap(), unchanged.as_slice());
        assert_eq!(
            store.write(LogFilter::Info, rejected),
            WriteOutcome::Dropped
        );
        assert_eq!(store.dropped(), 2);
    }

    #[test]
    fn fifo_order_survives_wrap_around() {
        let mut store = store();
        let per_round = u64::try_from(QUEUE_SLOTS).unwrap();
        let mut next_written = 0u64;
        let mut next_read = 0u64;
        for _ in 0..3 {
            for _ in 0..per_round {
                let record = Record::Tick {
                    served: next_written,
                    rejected: 0,
                    dropped: 0,
                };
                assert_eq!(store.write(LogFilter::Info, record), WriteOutcome::Written);
                next_written += 1;
            }
            for _ in 0..per_round {
                let expected =
                    format!("level=info kind=tick served={next_read} rejected=0 dropped=0\r\n");
                let front = front(&store);
                assert_eq!(front, expected.as_bytes());
                assert!(store.consume_front(front.len()));
                next_read += 1;
            }
            assert!(store.is_empty());
        }
        assert_eq!(store.dropped(), 0);
    }

    #[test]
    fn partial_consume_reveals_the_remainder_then_advances() {
        let mut store = store();
        let first = Record::Tick {
            served: 1,
            rejected: 2,
            dropped: 3,
        };
        assert_eq!(store.write(LogFilter::Info, first), WriteOutcome::Written);
        assert_eq!(
            store.write(LogFilter::Info, Record::FilterDefaulted),
            WriteOutcome::Written
        );
        let expected_first = b"level=info kind=tick served=1 rejected=2 dropped=3\r\n";
        let split = 9;
        assert_eq!(store.unwritten_front().unwrap(), expected_first);
        assert!(store.consume_front(split));
        assert_eq!(store.len(), 2);
        assert_eq!(store.unwritten_front().unwrap(), &expected_first[split..]);
        assert!(store.consume_front(expected_first.len() - split));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=warn kind=filter_default\r\n"
        );
    }

    #[test]
    fn consuming_zero_bytes_is_a_no_op() {
        let mut store = store();
        assert!(store.consume_front(0));
        assert!(store.is_empty());
        assert_eq!(
            store.write(LogFilter::Info, Record::FilterDefaulted),
            WriteOutcome::Written
        );
        let unchanged = front(&store);
        assert!(store.consume_front(0));
        assert_eq!(store.unwritten_front().unwrap(), unchanged.as_slice());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn consuming_past_the_front_record_changes_nothing() {
        let mut store = store();
        let record = Record::Tick {
            served: 7,
            rejected: 8,
            dropped: 9,
        };
        assert_eq!(store.write(LogFilter::Info, record), WriteOutcome::Written);
        let unchanged = front(&store);
        assert!(!store.consume_front(unchanged.len() + 1));
        assert_eq!(store.unwritten_front().unwrap(), unchanged.as_slice());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn empty_queue_rejects_consuming_positive_bytes() {
        let mut store = store();
        assert!(!store.consume_front(1));
        assert!(store.is_empty());
    }

    #[test]
    fn store_reserves_the_full_ring_once() {
        let store = store();
        assert_eq!(store.bytes.capacity(), QUEUE_SLOTS * SLOT_BYTES);
        assert_eq!(store.bytes.len(), QUEUE_SLOTS * SLOT_BYTES);
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
        assert_eq!(store.dropped(), 0);
    }

    #[test]
    fn error_names_the_reservation_failure() {
        assert_eq!(
            LogStoreError::Reserve.to_string(),
            "the fixed log queue buffer could not be reserved"
        );
    }
}

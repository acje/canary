//! The bounded nonblocking stdout sink for [`LogStore`].
//!
//! A turn of the reactor calls [`drain`] with a [`DrainBudget`]; the sink never
//! blocks, never allocates, and never grows. Whatever the operating system
//! refuses stays queued and is offered again next turn, so a slow reader costs
//! throughput and never a record that was accepted into the queue.
//!
//! [`NonblockingStdout::open`] sets `O_NONBLOCK` on the inherited standard
//! output once at startup, so [`std::io::Write::write`] reports a full pipe as
//! [`std::io::ErrorKind::WouldBlock`] instead of stopping the process.

use std::io::{self, ErrorKind, Write};
use std::time::{Duration, Instant};

use super::{ConsumeOutcome, LogStore};

/// The most write attempts one [`drain`] turn makes.
///
/// Owner: this sink. Scope: one turn of the single-owner reactor, per call.
/// Units: [`std::io::Write::write`] attempts, including retries after
/// [`ErrorKind::Interrupted`]. Limit: 16, which bounds a turn at sixteen
/// records even when the sink accepts a whole slot each time. Exhaustion:
/// [`drain`] returns [`DrainOutcome::Progress`] and the reactor retries after
/// [`LOG_RETRY`]. Evidence: `max_ops_budget_stops_early`.
pub const SINK_MAX_OPS: usize = 16;

/// The most bytes one [`drain`] turn accepts.
///
/// Owner: this sink. Scope: one turn of the single-owner reactor, aggregate
/// across records. Units: bytes removed from the store. Limit: 64 KiB, a
/// quarter of the 256 KiB ring, so one turn cannot monopolise the reactor.
/// Exhaustion: [`drain`] returns [`DrainOutcome::Progress`] and the remainder
/// stays queued. Evidence: `max_bytes_budget_stops_early`.
pub const SINK_MAX_BYTES: usize = 64 * 1024;

/// The delay the reactor leaves between turns that made no progress.
///
/// Owner: the reactor's retry timer, which reads this sink's budget. Scope: the
/// wait after a [`DrainOutcome::Stalled`] turn. Units: wall-clock time. Limit:
/// one second, short enough that a reader that resumes is served promptly and
/// long enough that a closed pipe costs no measurable CPU. Exhaustion: the
/// retry repeats; a stalled turn drops nothing. Evidence: the reactor timer
/// test lands with the reactor.
pub const LOG_RETRY: Duration = Duration::from_secs(1);

/// The wall-clock bound on the final drain before the process exits.
///
/// Owner: the shutdown path. Scope: the last [`drain_until`] call of the
/// process. Units: wall-clock time. Limit: 250 ms, which is enough for a
/// reader that is still consuming and short enough that shutdown stays
/// responsive. Exhaustion: the drain stops and whatever is still queued is
/// reported through the drop counter. Evidence:
/// `drain_until_with_an_elapsed_deadline_performs_exactly_one_drain`.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_millis(250);

/// The result of one [`drain`] turn.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainOutcome {
    /// The queue held nothing to offer.
    Idle,
    /// The turn moved records or bytes out of the queue.
    Progress {
        /// The records removed from the queue this turn.
        records: usize,
        /// The bytes removed from the queue this turn.
        bytes: usize,
    },
    /// The sink accepted nothing further without reporting an error, or the
    /// budget admitted no work.
    Stalled,
    /// The sink reported an error; the record it was offered is still queued.
    Failed {
        /// The kind of error the sink reported.
        kind: ErrorKind,
    },
}

/// The work one [`drain`] turn may do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainBudget {
    /// The most write attempts the turn makes.
    pub max_ops: usize,
    /// The most bytes the turn accepts.
    pub max_bytes: usize,
}

impl DrainBudget {
    /// The per-turn budget the reactor uses.
    pub const DEFAULT: Self = Self {
        max_ops: SINK_MAX_OPS,
        max_bytes: SINK_MAX_BYTES,
    };
}

/// Offers queued records to `sink` for at most `budget` work.
///
/// The turn re-reads [`LogStore::unwritten_front`] before every attempt and
/// offers the front record once, so a short write resumes where it stopped.
/// [`DrainOutcome::Progress`] counts what the store actually released, never
/// what the sink was offered. A full or closed sink leaves the record at the
/// front and returns [`DrainOutcome::Stalled`] or [`DrainOutcome::Failed`]; a
/// [`ConsumeOutcome::Rejected`] answer, or a write that claims more bytes than
/// it was offered, is an internal contract violation and returns
/// [`DrainOutcome::Failed`] with [`ErrorKind::InvalidInput`] without retrying.
pub fn drain(store: &mut LogStore, sink: &mut impl Write, budget: DrainBudget) -> DrainOutcome {
    let mut records: usize = 0;
    let mut bytes: usize = 0;
    let mut ops: usize = 0;
    loop {
        let Some(front) = store.unwritten_front() else {
            return if records == 0 {
                DrainOutcome::Idle
            } else {
                DrainOutcome::Progress { records, bytes }
            };
        };
        if ops >= budget.max_ops || bytes >= budget.max_bytes {
            return if records == 0 && bytes == 0 {
                DrainOutcome::Stalled
            } else {
                DrainOutcome::Progress { records, bytes }
            };
        }
        let offer = &front[..front.len().min(budget.max_bytes - bytes)];
        let offered = offer.len();
        let queued = store.len();
        ops += 1;
        match sink.write(offer) {
            Ok(0) => return DrainOutcome::Stalled,
            Ok(accepted) if accepted > offered => {
                return DrainOutcome::Failed {
                    kind: ErrorKind::InvalidInput,
                };
            }
            Ok(accepted) => {
                if store.consume_front(accepted) == ConsumeOutcome::Rejected {
                    return DrainOutcome::Failed {
                        kind: ErrorKind::InvalidInput,
                    };
                }
                bytes += accepted;
                if store.len() < queued {
                    records += 1;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => return DrainOutcome::Stalled,
            Err(error) => return DrainOutcome::Failed { kind: error.kind() },
        }
    }
}

/// Repeats [`drain`] while it makes progress and the deadline has not passed.
///
/// The first turn always runs, so an already-elapsed deadline still performs
/// exactly one bounded drain and returns its outcome.
pub fn drain_until(
    store: &mut LogStore,
    sink: &mut impl Write,
    budget: DrainBudget,
    deadline: Instant,
) -> DrainOutcome {
    loop {
        let outcome = drain(store, sink, budget);
        if !matches!(outcome, DrainOutcome::Progress { .. }) || Instant::now() >= deadline {
            return outcome;
        }
    }
}

/// Standard output with `O_NONBLOCK` set on the inherited descriptor.
pub struct NonblockingStdout(());

impl NonblockingStdout {
    /// Switches file descriptor 1 to nonblocking mode.
    ///
    /// # Errors
    ///
    /// Returns the operating system error when the descriptor's status flags
    /// cannot be read or written, which happens when standard output is closed.
    pub fn open() -> io::Result<Self> {
        #[expect(
            unsafe_code,
            reason = "setting O_NONBLOCK on an inherited descriptor is reachable only through fcntl"
        )]
        let set = unsafe {
            let flags = libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL);
            if flags < 0 {
                flags
            } else {
                libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK)
            }
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(()))
    }
}

impl Write for NonblockingStdout {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        io::stdout().write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DrainBudget, DrainOutcome, SHUTDOWN_DRAIN, SINK_MAX_BYTES, SINK_MAX_OPS, drain, drain_until,
    };
    use crate::log::{LogFilter, LogStore, Record, WriteOutcome};
    use std::collections::VecDeque;
    use std::io::{self, ErrorKind, Write};
    use std::time::Instant;

    const TICK_BYTES: usize = b"level=info kind=tick served=0 rejected=0 dropped=0\r\n".len();

    enum Step {
        Accepted(usize),
        Refused(ErrorKind),
    }

    struct FakeWriter {
        script: VecDeque<Step>,
        calls: Vec<Vec<u8>>,
    }

    impl FakeWriter {
        fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                script: steps.into_iter().collect(),
                calls: Vec::new(),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.len()
        }
    }

    impl Write for FakeWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.calls.push(buffer.to_vec());
            match self
                .script
                .pop_front()
                .expect("the script covers every write")
            {
                Step::Accepted(count) => Ok(count),
                Step::Refused(kind) => Err(io::Error::from(kind)),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn store_with(records: u64) -> LogStore {
        let mut store = LogStore::new().unwrap();
        for served in 0..records {
            let record = Record::Tick {
                served,
                rejected: 0,
                dropped: 0,
            };
            assert_eq!(store.write(LogFilter::Info, record), WriteOutcome::Written);
        }
        store
    }

    fn front(store: &LogStore) -> Vec<u8> {
        store.unwritten_front().unwrap().to_vec()
    }

    fn single_turn() -> DrainBudget {
        DrainBudget {
            max_ops: 1,
            max_bytes: SINK_MAX_BYTES,
        }
    }

    #[test]
    fn short_write_retains_the_remainder_and_resumes_across_turns() {
        let mut store = store_with(1);
        let record = front(&store);
        let mut sink = FakeWriter::new([Step::Accepted(4), Step::Accepted(record.len() - 4)]);
        assert_eq!(
            drain(&mut store, &mut sink, single_turn()),
            DrainOutcome::Progress {
                records: 0,
                bytes: 4
            }
        );
        assert_eq!(store.len(), 1);
        assert_eq!(store.unwritten_front().unwrap(), &record[4..]);
        assert_eq!(
            drain(&mut store, &mut sink, single_turn()),
            DrainOutcome::Progress {
                records: 1,
                bytes: record.len() - 4
            }
        );
        assert!(store.is_empty());
        assert_eq!(
            sink.calls.first().map(Vec::as_slice),
            Some(record.as_slice())
        );
        assert_eq!(sink.calls.get(1).map(Vec::as_slice), Some(&record[4..]));
    }

    #[test]
    fn zero_byte_accept_is_stalled_and_never_progress() {
        let mut store = store_with(1);
        let record = front(&store);
        let mut sink = FakeWriter::new([Step::Accepted(0)]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Stalled
        );
        assert_eq!(sink.call_count(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(store.unwritten_front().unwrap(), record.as_slice());
    }

    #[test]
    fn would_block_is_stalled_with_the_record_retained() {
        let mut store = store_with(1);
        let record = front(&store);
        let mut sink = FakeWriter::new([Step::Refused(ErrorKind::WouldBlock)]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Stalled
        );
        assert_eq!(sink.call_count(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(store.unwritten_front().unwrap(), record.as_slice());
    }

    #[test]
    fn hard_error_is_failed_with_the_record_retained() {
        let mut store = store_with(1);
        let record = front(&store);
        let mut sink = FakeWriter::new([Step::Refused(ErrorKind::BrokenPipe)]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Failed {
                kind: ErrorKind::BrokenPipe
            }
        );
        assert_eq!(sink.call_count(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(store.unwritten_front().unwrap(), record.as_slice());
    }

    #[test]
    fn interrupted_retries_within_the_budget() {
        let mut store = store_with(1);
        let record = front(&store);
        let mut sink = FakeWriter::new([
            Step::Refused(ErrorKind::Interrupted),
            Step::Accepted(record.len()),
        ]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Progress {
                records: 1,
                bytes: record.len()
            }
        );
        assert_eq!(sink.call_count(), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn max_ops_budget_stops_early() {
        let mut store = store_with(3);
        let mut sink = FakeWriter::new([Step::Accepted(TICK_BYTES), Step::Accepted(TICK_BYTES)]);
        let budget = DrainBudget {
            max_ops: 2,
            max_bytes: SINK_MAX_BYTES,
        };
        assert_eq!(
            drain(&mut store, &mut sink, budget),
            DrainOutcome::Progress {
                records: 2,
                bytes: 2 * TICK_BYTES
            }
        );
        assert_eq!(sink.call_count(), 2);
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.unwritten_front().unwrap(),
            b"level=info kind=tick served=2 rejected=0 dropped=0\r\n"
        );
    }

    #[test]
    fn max_bytes_budget_stops_early_and_truncates_the_offer() {
        let mut store = store_with(3);
        let record = front(&store);
        let mut sink = FakeWriter::new([Step::Accepted(record.len()), Step::Accepted(8)]);
        let budget = DrainBudget {
            max_ops: SINK_MAX_OPS,
            max_bytes: record.len() + 8,
        };
        assert_eq!(
            drain(&mut store, &mut sink, budget),
            DrainOutcome::Progress {
                records: 1,
                bytes: record.len() + 8
            }
        );
        assert_eq!(sink.call_count(), 2);
        assert_eq!(sink.calls.get(1).map(Vec::len), Some(8));
        assert_eq!(store.len(), 2);
        assert_eq!(
            store.unwritten_front().unwrap(),
            &b"level=info kind=tick served=1 rejected=0 dropped=0\r\n"[8..]
        );
    }

    #[test]
    fn empty_queue_is_idle_without_a_write() {
        let mut store = store_with(0);
        let mut sink = FakeWriter::new([]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Idle
        );
        assert_eq!(sink.call_count(), 0);
    }

    #[test]
    fn a_budget_that_admits_no_work_is_stalled() {
        let mut store = store_with(1);
        let mut sink = FakeWriter::new([]);
        let budget = DrainBudget {
            max_ops: 0,
            max_bytes: 0,
        };
        assert_eq!(drain(&mut store, &mut sink, budget), DrainOutcome::Stalled);
        assert_eq!(sink.call_count(), 0);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn drain_until_with_an_elapsed_deadline_performs_exactly_one_drain() {
        let mut store = store_with(2);
        let mut sink = FakeWriter::new([Step::Accepted(TICK_BYTES)]);
        let deadline = Instant::now();
        assert_eq!(
            drain_until(&mut store, &mut sink, single_turn(), deadline),
            DrainOutcome::Progress {
                records: 1,
                bytes: TICK_BYTES
            }
        );
        assert_eq!(sink.call_count(), 1);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn drain_until_repeats_turns_until_the_queue_is_empty() {
        let mut store = store_with(3);
        let mut sink = FakeWriter::new([
            Step::Accepted(TICK_BYTES),
            Step::Accepted(TICK_BYTES),
            Step::Accepted(TICK_BYTES),
        ]);
        let deadline = Instant::now() + SHUTDOWN_DRAIN;
        assert_eq!(
            drain_until(&mut store, &mut sink, single_turn(), deadline),
            DrainOutcome::Idle
        );
        assert_eq!(sink.call_count(), 3);
        assert!(store.is_empty());
    }
}

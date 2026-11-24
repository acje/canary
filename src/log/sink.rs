//! The bounded nonblocking stdout sink for [`LogQueue`].
//!
//! A turn of the reactor calls [`drain`] with a [`DrainBudget`]; the sink never
//! blocks, never allocates, and never grows. Whatever the operating system
//! refuses stays queued and is offered again on a later serving turn, so a slow
//! reader costs throughput and never a record that was accepted into the queue
//! while the process keeps serving: only the final [`FLUSH_DEADLINE`] flush can
//! abandon what a reader never drained.
//!
//! [`NonblockingStdout`] owns an unbuffered handle to the inherited standard
//! output descriptor, so [`std::io::Write::write`] returns `Ok(n)` only when n
//! bytes reached the kernel: no userspace buffer holds a tail that a later exit
//! would discard. [`NonblockingStdout::open`] sets `O_NONBLOCK` on that
//! descriptor once at startup, so a full pipe is reported as
//! [`std::io::ErrorKind::WouldBlock`] instead of stopping the process.

use std::fs::File;
use std::io::{self, ErrorKind, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::time::{Duration, Instant};

use super::{ConsumeOutcome, LogQueue};

/// The most write attempts one [`drain`] turn makes.
///
/// Owner: this sink. Scope: one turn of the single-owner reactor, per call.
/// Units: [`std::io::Write::write`] attempts, including retries after
/// [`ErrorKind::Interrupted`]. Limit: 16, which bounds a turn at sixteen
/// records even when the sink accepts a whole slot each time. Exhaustion:
/// [`drain`] returns [`DrainOutcome::Moved`] and the reactor retries after
/// [`LOG_RETRY`]. Evidence: `max_ops_budget_stops_early`.
pub(crate) const SINK_MAX_OPS: usize = 16;

/// The most bytes one [`drain`] turn accepts.
///
/// Owner: this sink. Scope: one turn of the single-owner reactor, aggregate
/// across records. Units: bytes removed from the store. Limit: 64 KiB, a
/// quarter of the 256 KiB ring, so one turn cannot monopolise the reactor.
/// Exhaustion: [`drain`] returns [`DrainOutcome::Moved`] and the remainder
/// stays queued. Evidence: `max_bytes_budget_stops_early`.
pub(crate) const SINK_MAX_BYTES: usize = 64 * 1024;

/// The delay the reactor leaves between turns that made no progress.
///
/// Owner: the reactor's retry timer (M06), the only reader of this value. Scope:
/// the wait after a [`DrainOutcome::Stalled`] turn. Units: wall-clock time.
/// Limit: one second, short enough that a reader that resumes is served promptly
/// and long enough that a closed pipe costs no measurable CPU. Exhaustion: the
/// retry repeats; a stalled turn drops nothing.
pub(crate) const LOG_RETRY: Duration = Duration::from_secs(1);

/// The wall-clock bound on each one-shot flush of the process.
///
/// Owner: the process's two flush points (M06/M07), which pass
/// `Instant::now() + FLUSH_DEADLINE` to their [`drain_until`] calls. Scope: the
/// startup drain that releases the boot line before the listener is polled, and
/// the final drain that releases whatever the shutdown left queued. Units:
/// wall-clock time. Limit: 250 ms, which is enough for a reader that is still
/// consuming and short enough that startup and shutdown stay responsive.
/// Exhaustion: the drain stops at the deadline and the remainder stays queued;
/// at startup the serving turns offer it again, and at shutdown it is abandoned
/// with the process. [`LogQueue::dropped`] counts queue refusals only, so a
/// leftover record never advances it. The returned [`DrainOutcome`] reports the
/// short flush. Evidence:
/// `drain_until_with_an_elapsed_deadline_performs_exactly_one_drain`.
pub const FLUSH_DEADLINE: Duration = Duration::from_millis(250);

/// The result of one [`drain`] turn.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainOutcome {
    /// The queue held nothing to offer.
    Idle,
    /// The turn moved records or bytes out of the queue.
    Moved {
        /// The records removed from the queue this turn.
        records: usize,
        /// The bytes removed from the queue this turn.
        bytes: usize,
    },
    /// No byte reached the kernel in this turn.
    ///
    /// The sink accepted nothing further without reporting an error, or the
    /// budget admitted no work. A record the sink refused and a record the budget
    /// never offered both stay at the front, and a turn that wrote part of a
    /// record before the sink refused more reports [`DrainOutcome::Moved`]
    /// with the bytes that did reach the kernel.
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
    pub(crate) max_ops: usize,
    /// The most bytes the turn accepts.
    pub(crate) max_bytes: usize,
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
/// The turn re-reads `LogQueue::unwritten_front` before every attempt and
/// offers the front record once, so a short write resumes where it stopped.
/// [`DrainOutcome::Moved`] counts what the store actually released, never
/// what the sink was offered. Bytes a turn released before a full sink refused
/// more are reported as [`DrainOutcome::Moved`] and the remainder stays at
/// the front; a turn that reached the kernel with nothing reports
/// [`DrainOutcome::Stalled`], and a hard error returns [`DrainOutcome::Failed`]
/// with the offered record still queued.
///
/// A `ConsumeOutcome::Rejected` answer, or a write that claims more bytes than
/// it was offered, is an internal contract violation and returns
/// [`DrainOutcome::Failed`] with [`ErrorKind::InvalidInput`] without retrying.
/// The `accepted > offered` check below is the only place this sink enforces the
/// [`std::io::Write`] contract, so a sink that reports bytes it never accepted
/// fails the turn instead of silently releasing queue bytes.
pub fn drain(store: &mut LogQueue, sink: &mut impl Write, budget: DrainBudget) -> DrainOutcome {
    let mut records: usize = 0;
    let mut bytes: usize = 0;
    let mut ops: usize = 0;
    loop {
        let Some(front) = store.unwritten_front() else {
            return if records == 0 {
                DrainOutcome::Idle
            } else {
                DrainOutcome::Moved { records, bytes }
            };
        };
        if ops >= budget.max_ops || bytes >= budget.max_bytes {
            return if records == 0 && bytes == 0 {
                DrainOutcome::Stalled
            } else {
                DrainOutcome::Moved { records, bytes }
            };
        }
        let offer = &front[..front.len().min(budget.max_bytes - bytes)];
        let offered = offer.len();
        let queued = store.len();
        ops += 1;
        match sink.write(offer) {
            Ok(0) => break,
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
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => return DrainOutcome::Failed { kind: error.kind() },
        }
    }
    if records == 0 && bytes == 0 {
        DrainOutcome::Stalled
    } else {
        DrainOutcome::Moved { records, bytes }
    }
}

/// Repeats [`drain`] while it makes progress and the deadline has not passed.
///
/// The first turn always runs, so an already-elapsed deadline still performs
/// exactly one bounded drain and returns its outcome.
pub fn drain_until(
    store: &mut LogQueue,
    sink: &mut impl Write,
    budget: DrainBudget,
    deadline: Instant,
) -> DrainOutcome {
    loop {
        let outcome = drain(store, sink, budget);
        if !matches!(outcome, DrainOutcome::Moved { .. }) || Instant::now() >= deadline {
            return outcome;
        }
    }
}

/// Standard output as an unbuffered handle to the inherited descriptor.
///
/// The handle writes to descriptor 1 directly instead of forwarding to the
/// process-global [`std::io::Stdout`], whose line buffer can hold a tail that
/// the kernel refused. It therefore never closes descriptor 1, which the process
/// still owns.
pub struct NonblockingStdout(ManuallyDrop<File>);

impl NonblockingStdout {
    /// Switches file descriptor 1 to nonblocking mode and takes an unbuffered
    /// handle to it.
    ///
    /// # Errors
    ///
    /// Returns the operating system error when the descriptor's status flags
    /// cannot be read or written, which happens when standard output is closed.
    pub fn open() -> io::Result<Self> {
        #[expect(
            unsafe_code,
            reason = "reading and writing the descriptor status flags and wrapping the inherited \
                      descriptor both require libc and from_raw_fd"
        )]
        let descriptor = unsafe {
            let flags = libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            let set = libc::fcntl(libc::STDOUT_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK);
            if set < 0 {
                return Err(io::Error::last_os_error());
            }
            ManuallyDrop::new(File::from_raw_fd(libc::STDOUT_FILENO))
        };
        Ok(Self(descriptor))
    }
}

#[cfg(test)]
impl NonblockingStdout {
    fn from_owned(descriptor: ManuallyDrop<File>) -> Self {
        Self(descriptor)
    }
}

impl Write for NonblockingStdout {
    /// Writes `buffer` straight to the descriptor.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::WouldBlock`] when the reader is not draining the
    /// pipe, and any other operating system error the descriptor reports.
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    /// Returns success without doing any work.
    ///
    /// The handle is unbuffered, so no byte is ever held back for a flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DrainBudget, DrainOutcome, FLUSH_DEADLINE, NonblockingStdout, SINK_MAX_BYTES, SINK_MAX_OPS,
        drain, drain_until,
    };
    use crate::log::{Counters, LogFilter, LogQueue, QueueOutcome, Record};
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io::{self, ErrorKind, Read, Write};
    use std::mem::ManuallyDrop;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    enum Step {
        Accepted(usize),
        Refused(ErrorKind),
    }

    struct FakeWriter {
        script: VecDeque<Step>,
        calls: Vec<usize>,
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

        fn offered(&self, index: usize) -> Option<usize> {
            self.calls.get(index).copied()
        }
    }

    impl Write for FakeWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.calls.push(buffer.len());
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

    struct OneByteWriter {
        accepted: usize,
        calls: usize,
    }

    impl OneByteWriter {
        const fn new() -> Self {
            Self {
                accepted: 0,
                calls: 0,
            }
        }
    }

    impl Write for OneByteWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            let take = buffer.len().min(1);
            self.accepted += take;
            Ok(take)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn store_with(records: u64) -> LogQueue {
        let mut store = LogQueue::new().unwrap();
        for served in 0..records {
            let record = Record::Tick(Counters {
                served,
                rejected: 0,
                dropped: 0,
            });
            assert_eq!(store.enqueue(LogFilter::Info, record), QueueOutcome::Queued);
        }
        store
    }

    fn front_len(store: &LogQueue) -> usize {
        store.unwritten_front().unwrap().len()
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
        let record_len = front_len(&store);
        let split = 4;
        let mut sink = FakeWriter::new([Step::Accepted(split), Step::Accepted(record_len - split)]);
        assert_eq!(
            drain(&mut store, &mut sink, single_turn()),
            DrainOutcome::Moved {
                records: 0,
                bytes: split
            }
        );
        assert_eq!(store.len(), 1);
        assert_eq!(front_len(&store), record_len - split);
        assert_eq!(
            drain(&mut store, &mut sink, single_turn()),
            DrainOutcome::Moved {
                records: 1,
                bytes: record_len - split
            }
        );
        assert!(store.is_empty());
        assert_eq!(sink.offered(0), Some(record_len));
        assert_eq!(sink.offered(1), Some(record_len - split));
    }

    #[test]
    fn zero_byte_accept_is_stalled_and_moves_nothing() {
        let mut store = store_with(1);
        let record_len = front_len(&store);
        let mut sink = FakeWriter::new([Step::Accepted(0)]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Stalled
        );
        assert_eq!(sink.call_count(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(front_len(&store), record_len);
    }

    #[test]
    fn would_block_is_stalled_with_the_record_retained() {
        let mut store = store_with(1);
        let record_len = front_len(&store);
        let mut sink = FakeWriter::new([Step::Refused(ErrorKind::WouldBlock)]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Stalled
        );
        assert_eq!(sink.call_count(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(front_len(&store), record_len);
    }

    #[test]
    fn a_full_pipe_after_a_whole_record_reports_moved_then_stalls() {
        let mut store = store_with(2);
        let record_len = front_len(&store);
        let mut sink = FakeWriter::new([
            Step::Accepted(record_len),
            Step::Refused(ErrorKind::WouldBlock),
            Step::Refused(ErrorKind::WouldBlock),
        ]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Moved {
                records: 1,
                bytes: record_len
            }
        );
        assert_eq!(sink.call_count(), 2);
        assert_eq!(store.len(), 1);
        assert_eq!(front_len(&store), record_len);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Stalled
        );
        assert_eq!(sink.call_count(), 3);
        assert_eq!(store.len(), 1);
        assert_eq!(front_len(&store), record_len);
        assert_eq!(store.dropped(), 0);
    }

    #[test]
    fn hard_error_is_failed_with_the_record_retained() {
        let mut store = store_with(1);
        let record_len = front_len(&store);
        let mut sink = FakeWriter::new([Step::Refused(ErrorKind::BrokenPipe)]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Failed {
                kind: ErrorKind::BrokenPipe
            }
        );
        assert_eq!(sink.call_count(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(front_len(&store), record_len);
    }

    #[test]
    fn a_write_that_claims_more_than_it_was_offered_fails_without_consuming() {
        let truncating = DrainBudget {
            max_ops: 1,
            max_bytes: 8,
        };
        assert!(truncating.max_bytes < front_len(&store_with(1)));
        for budget in [truncating, DrainBudget::DEFAULT] {
            let mut store = store_with(1);
            let record_len = front_len(&store);
            let offered = record_len.min(budget.max_bytes);
            let mut sink = FakeWriter::new([Step::Accepted(offered + 1)]);
            assert_eq!(
                drain(&mut store, &mut sink, budget),
                DrainOutcome::Failed {
                    kind: ErrorKind::InvalidInput
                }
            );
            assert_eq!(sink.offered(0), Some(offered));
            assert_eq!(sink.call_count(), 1);
            assert_eq!(store.len(), 1);
            assert_eq!(store.dropped(), 0);
            assert_eq!(front_len(&store), record_len);
        }
    }

    #[test]
    fn interrupted_retries_within_the_budget() {
        let mut store = store_with(1);
        let record_len = front_len(&store);
        let mut sink = FakeWriter::new([
            Step::Refused(ErrorKind::Interrupted),
            Step::Accepted(record_len),
        ]);
        assert_eq!(
            drain(&mut store, &mut sink, DrainBudget::DEFAULT),
            DrainOutcome::Moved {
                records: 1,
                bytes: record_len
            }
        );
        assert_eq!(sink.call_count(), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn max_ops_budget_stops_early() {
        let mut store = store_with(3);
        let record_len = front_len(&store);
        let mut sink = FakeWriter::new([Step::Accepted(record_len), Step::Accepted(record_len)]);
        let budget = DrainBudget {
            max_ops: 2,
            max_bytes: SINK_MAX_BYTES,
        };
        assert_eq!(
            drain(&mut store, &mut sink, budget),
            DrainOutcome::Moved {
                records: 2,
                bytes: 2 * record_len
            }
        );
        assert_eq!(sink.call_count(), 2);
        assert_eq!(store.len(), 1);
        assert_eq!(front_len(&store), record_len);
    }

    #[test]
    fn max_bytes_budget_stops_early_and_truncates_the_offer() {
        let mut store = store_with(3);
        let record_len = front_len(&store);
        let tail = 8;
        let mut sink = FakeWriter::new([Step::Accepted(record_len), Step::Accepted(tail)]);
        let budget = DrainBudget {
            max_ops: SINK_MAX_OPS,
            max_bytes: record_len + tail,
        };
        assert_eq!(
            drain(&mut store, &mut sink, budget),
            DrainOutcome::Moved {
                records: 1,
                bytes: record_len + tail
            }
        );
        assert_eq!(sink.call_count(), 2);
        assert_eq!(sink.offered(1), Some(tail));
        assert_eq!(store.len(), 2);
        assert_eq!(front_len(&store), record_len - tail);
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
        let record_len = front_len(&store);
        let mut sink = FakeWriter::new([Step::Accepted(record_len)]);
        let deadline = Instant::now();
        assert_eq!(
            drain_until(&mut store, &mut sink, single_turn(), deadline),
            DrainOutcome::Moved {
                records: 1,
                bytes: record_len
            }
        );
        assert_eq!(sink.call_count(), 1);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn drain_until_repeats_turns_until_the_queue_is_empty() {
        let mut store = store_with(3);
        let record_len = front_len(&store);
        let mut sink = FakeWriter::new([
            Step::Accepted(record_len),
            Step::Accepted(record_len),
            Step::Accepted(record_len),
        ]);
        let deadline = Instant::now() + FLUSH_DEADLINE;
        assert_eq!(
            drain_until(&mut store, &mut sink, single_turn(), deadline),
            DrainOutcome::Idle
        );
        assert_eq!(sink.call_count(), 3);
        assert!(store.is_empty());
    }

    #[test]
    fn drain_until_terminates_against_a_sink_that_keeps_accepting() {
        let mut store = store_with(3);
        let record_len = front_len(&store);
        let queued = store.len();
        let expected_bytes = queued * record_len;
        let mut sink = OneByteWriter::new();
        let deadline = Instant::now() + FLUSH_DEADLINE;
        assert_eq!(
            drain_until(&mut store, &mut sink, DrainBudget::DEFAULT, deadline),
            DrainOutcome::Idle
        );
        assert!(store.is_empty());
        assert_eq!(sink.accepted, expected_bytes);
        assert_eq!(sink.calls, expected_bytes);
    }

    #[test]
    fn a_short_accept_reports_exactly_the_bytes_the_peer_reads() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut sink =
            NonblockingStdout::from_owned(ManuallyDrop::new(File::from(OwnedFd::from(writer))));

        let head = b"header";
        assert_eq!(sink.write(head).unwrap(), head.len());

        let offer = vec![b'x'; 1 << 20];
        let accepted = sink.write(&offer).unwrap();
        assert!(accepted < offer.len());

        let mut peer = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => peer.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("the peer reported {error}"),
            }
        }

        assert_eq!(peer.len(), head.len() + accepted);
        assert!(peer.starts_with(head));
    }
}

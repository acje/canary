//! Fixed-capacity connection slots owned by the reactor.
//!
//! A [`Slots`] table reserves every [`super::MAX_CONNECTIONS`] entry once and
//! never grows. Each occupied slot owns its stream, its absolute deadline, and a
//! closed [`Connection`] state. A reading slot buffers at most [`super::HEAD_BYTES`]
//! of request head and classifies it through [`crate::http::parse_head`]; a
//! writing slot stores only the reply variant, a cursor, and the reply length,
//! never rendered bytes; a closing slot accepts no further reads.

use mio::net::TcpStream;
use std::time::Instant;

use crate::http::{ParseOutcome, Request, parse_head};

use super::{CONNECTION_DEADLINE, HEAD_BYTES, MAX_CONNECTIONS, SLOT_METADATA_BYTES};

/// The reply the reactor will write once a read finishes.
///
/// A variant names the reply-plan call to make; it never holds rendered bytes,
/// so a stored reply choice stays [`Copy`] and independent of the plan's
/// lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyChoice {
    /// Answer the parsed request through `ResponsePlan::reply`.
    Matched(Request),
    /// Answer with the fixed `400` reply.
    BadRequest,
    /// Answer with the fixed `431` reply.
    TooLarge,
}

/// The outcome of advancing a writing slot's cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteProgress {
    /// Reply bytes remain to be written.
    Remaining,
    /// The cursor reached the reply length the caller supplied.
    Finished,
}

/// The phase of one occupied slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionPhase {
    /// Buffering request-head bytes.
    Reading,
    /// Writing the selected reply.
    Writing,
    /// Closing the connection after the reply.
    Closing,
}

#[expect(
    clippy::large_enum_variant,
    reason = "the reading variant inlines the fixed head buffer so a slot allocates nothing after construction"
)]
enum Connection {
    Reading {
        head: [u8; HEAD_BYTES],
        len: usize,
    },
    Writing {
        pending: ReplyChoice,
        cursor: usize,
        reply_len: usize,
    },
    Closing,
}

struct Occupant {
    stream: TcpStream,
    expires_at: Instant,
    connection: Connection,
}

const _: () = assert!(std::mem::size_of::<Option<Occupant>>() <= HEAD_BYTES + SLOT_METADATA_BYTES);

/// The connection slots reserved for the reactor.
///
/// The table is built once from a fixed array; it owns every stream it accepts
/// and exposes no way to replace or extend that array.
pub struct Slots {
    entries: [Option<Occupant>; MAX_CONNECTIONS],
    len: usize,
}

impl Slots {
    /// Reserves the fixed slot table.
    ///
    /// The reactor is the only constructor caller, so no `Default` path exists:
    /// a second way to build an empty table would only invite a second owner.
    #[expect(
        clippy::new_without_default,
        reason = "the owning reactor is the only constructor caller, so a Default impl would invite a second construction path"
    )]
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            len: 0,
        }
    }

    /// Claims the lowest free slot for `stream`, accepting it at `now`.
    ///
    /// The slot's absolute deadline is `now` plus [`super::CONNECTION_DEADLINE`].
    /// Returns `None` when every slot is occupied.
    #[must_use]
    pub fn acquire(&mut self, stream: TcpStream, now: Instant) -> Option<usize> {
        let index = self.entries.iter().position(Option::is_none)?;
        self.entries[index] = Some(Occupant {
            stream,
            expires_at: now + CONNECTION_DEADLINE,
            connection: Connection::Reading {
                head: [0; HEAD_BYTES],
                len: 0,
            },
        });
        self.len += 1;
        Some(index)
    }

    /// Drops the stream of `index` and frees its slot.
    ///
    /// A vacant or out-of-range index changes nothing.
    pub fn release(&mut self, index: usize) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if entry.take().is_some() {
            self.len -= 1;
        }
    }

    /// The stream of the occupied slot `index`, for the reactor's read and
    /// write calls.
    ///
    /// Returns `None` for a vacant or out-of-range index.
    #[must_use]
    pub fn stream_mut(&mut self, index: usize) -> Option<&mut TcpStream> {
        Some(&mut self.entries.get_mut(index)?.as_mut()?.stream)
    }

    /// The head bytes the reading slot `index` can still buffer.
    ///
    /// Returns `None` when `index` is not a reading slot.
    #[must_use]
    pub fn head_room(&self, index: usize) -> Option<usize> {
        match &self.entries.get(index)?.as_ref()?.connection {
            Connection::Reading { len, .. } => Some(HEAD_BYTES - *len),
            Connection::Writing { .. } | Connection::Closing => None,
        }
    }

    /// The bytes the writing slot `index` has already written.
    ///
    /// Returns `None` when `index` is not a writing slot.
    #[must_use]
    pub fn cursor(&self, index: usize) -> Option<usize> {
        match &self.entries.get(index)?.as_ref()?.connection {
            Connection::Writing { cursor, .. } => Some(*cursor),
            Connection::Reading { .. } | Connection::Closing => None,
        }
    }

    /// The earliest absolute deadline of any occupied slot.
    ///
    /// Returns `None` when no slot is occupied. The scan touches each slot
    /// once, so it is bounded by [`super::MAX_CONNECTIONS`].
    #[must_use]
    pub fn earliest_deadline(&self) -> Option<Instant> {
        self.entries
            .iter()
            .filter_map(|entry| entry.as_ref().map(|occupant| occupant.expires_at))
            .min()
    }

    /// Buffers `bytes` into the reading slot `index` and classifies the head.
    ///
    /// Copies at most the remaining head capacity and never grows past
    /// [`super::HEAD_BYTES`]. Returns `None` when `index` is not a reading slot.
    #[must_use]
    pub fn read_into(&mut self, index: usize, bytes: &[u8]) -> Option<ParseOutcome> {
        let occupant = self.entries.get_mut(index)?.as_mut()?;
        let Connection::Reading { head, len } = &mut occupant.connection else {
            return None;
        };
        let room = HEAD_BYTES - *len;
        let take = room.min(bytes.len());
        head[*len..*len + take].copy_from_slice(&bytes[..take]);
        *len += take;
        Some(parse_head(&head[..*len]))
    }

    /// Moves the reading slot `index` into a `total_len`-byte write of `pending`.
    ///
    /// `total_len` is the byte length of the plan reply the reactor selected.
    /// Returns `false` when `index` is not a reading slot.
    #[must_use]
    pub fn begin_write(&mut self, index: usize, pending: ReplyChoice, total_len: usize) -> bool {
        let Some(occupant) = self.entries.get_mut(index).and_then(|entry| entry.as_mut()) else {
            return false;
        };
        if !matches!(occupant.connection, Connection::Reading { .. }) {
            return false;
        }
        occupant.connection = Connection::Writing {
            pending,
            cursor: 0,
            reply_len: total_len,
        };
        true
    }

    /// Advances the write cursor of slot `index` by the bytes actually written.
    ///
    /// The advance is bounded by the reply length the caller supplied to
    /// [`Slots::begin_write`]; an over-report stops at that length. Returns
    /// [`WriteProgress::Finished`] exactly when the cursor reaches it, and `None`
    /// when `index` is not a writing slot.
    #[must_use]
    pub fn advance_written(&mut self, index: usize, written: usize) -> Option<WriteProgress> {
        let occupant = self.entries.get_mut(index)?.as_mut()?;
        let Connection::Writing {
            cursor, reply_len, ..
        } = &mut occupant.connection
        else {
            return None;
        };
        let remaining = reply_len.saturating_sub(*cursor);
        *cursor += written.min(remaining);
        Some(if *cursor >= *reply_len {
            WriteProgress::Finished
        } else {
            WriteProgress::Remaining
        })
    }

    /// The reply pending in the writing slot `index`.
    ///
    /// Returns `None` when `index` is not a writing slot.
    #[must_use]
    pub fn pending(&self, index: usize) -> Option<ReplyChoice> {
        match &self.entries.get(index)?.as_ref()?.connection {
            Connection::Writing { pending, .. } => Some(*pending),
            Connection::Reading { .. } | Connection::Closing => None,
        }
    }

    /// The phase of the occupied slot `index`.
    ///
    /// Returns `None` for a vacant or out-of-range index.
    #[must_use]
    pub fn phase(&self, index: usize) -> Option<ConnectionPhase> {
        Some(match &self.entries.get(index)?.as_ref()?.connection {
            Connection::Reading { .. } => ConnectionPhase::Reading,
            Connection::Writing { .. } => ConnectionPhase::Writing,
            Connection::Closing => ConnectionPhase::Closing,
        })
    }

    /// Marks slot `index` as closing so it accepts no further reads.
    ///
    /// Returns `false` for a vacant or out-of-range index.
    #[must_use]
    pub fn begin_close(&mut self, index: usize) -> bool {
        let Some(occupant) = self.entries.get_mut(index).and_then(|entry| entry.as_mut()) else {
            return false;
        };
        occupant.connection = Connection::Closing;
        true
    }

    /// Removes every occupant past its deadline at `now` and yields it.
    ///
    /// Scans every slot once and yields `(index, stream)` in ascending slot
    /// order for each occupant whose absolute deadline `now` is strictly after,
    /// so an instant exactly at the deadline expires nothing. Each yielded slot
    /// is already vacant and its stream is owned by the caller, so a yielded
    /// index cannot name a fresh occupant: the next [`Slots::acquire`] claims
    /// the lowest free slot for a new connection.
    pub fn take_expired(
        &mut self,
        now: Instant,
    ) -> impl Iterator<Item = (usize, TcpStream)> + use<> {
        let mut expired: [Option<(usize, TcpStream)>; MAX_CONNECTIONS] =
            std::array::from_fn(|_| None);
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if entry
                .as_ref()
                .is_some_and(|occupant| now > occupant.expires_at)
                && let Some(occupant) = entry.take()
            {
                expired[index] = Some((index, occupant.stream));
                self.len -= 1;
            }
        }
        expired.into_iter().flatten()
    }

    /// The number of occupied slots.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no slot is occupied.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnectionPhase, ReplyChoice, Slots, WriteProgress};
    use crate::http::{MalformedKind, Method, ParseOutcome, RequestTarget};
    use crate::serve::{CONNECTION_DEADLINE, HEAD_BYTES, MAX_CONNECTIONS};
    use mio::net::TcpStream;
    use std::io::Read as _;
    use std::net::{SocketAddr, TcpListener};
    use std::time::{Duration, Instant};

    struct Fixture {
        listener: TcpListener,
        address: SocketAddr,
    }

    impl Fixture {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            Self { listener, address }
        }

        fn stream(&self) -> (TcpStream, std::net::TcpStream) {
            let peer = std::net::TcpStream::connect(self.address).unwrap();
            let (server, _) = self.listener.accept().unwrap();
            server.set_nonblocking(true).unwrap();
            (TcpStream::from_std(server), peer)
        }
    }

    fn padded_head(len: usize) -> Vec<u8> {
        let mut head = Vec::from(&b"GET / HTTP/1.1\r\nX: "[..]);
        head.resize(len - 4, b'a');
        head.extend_from_slice(b"\r\n\r\n");
        head
    }

    #[test]
    fn acquire_claims_the_lowest_free_slot() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let (stream, _peer) = fixture.stream();
        assert_eq!(slots.acquire(stream, now), Some(0));
        assert_eq!(slots.len(), 1);
        assert!(!slots.is_empty());
        assert_eq!(slots.phase(0), Some(ConnectionPhase::Reading));
    }

    #[test]
    fn acquire_fills_every_slot_then_reports_capacity() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let mut peers = Vec::with_capacity(MAX_CONNECTIONS + 1);
        for expected in 0..MAX_CONNECTIONS {
            let (stream, peer) = fixture.stream();
            peers.push(peer);
            assert_eq!(slots.acquire(stream, now), Some(expected));
        }
        assert_eq!(slots.len(), MAX_CONNECTIONS);
        let (stream, peer) = fixture.stream();
        peers.push(peer);
        assert_eq!(slots.acquire(stream, now), None);
        assert_eq!(slots.len(), MAX_CONNECTIONS);
    }

    #[test]
    fn release_restores_exactly_one_slot() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let (first, _first_peer) = fixture.stream();
        let first_index = slots.acquire(first, now).unwrap();
        let (second, _second_peer) = fixture.stream();
        let second_index = slots.acquire(second, now).unwrap();
        assert_eq!((first_index, second_index), (0, 1));
        slots.release(first_index);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots.phase(first_index), None);
        assert_eq!(slots.phase(second_index), Some(ConnectionPhase::Reading));
        let (replacement, _replacement_peer) = fixture.stream();
        assert_eq!(slots.acquire(replacement, now), Some(first_index));
        assert_eq!(slots.len(), 2);
    }

    #[test]
    fn release_closes_the_stream_for_the_peer() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let (stream, mut peer) = fixture.stream();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let index = slots.acquire(stream, now).unwrap();
        slots.release(index);
        let mut buffer = [0u8; 1];
        assert_eq!(peer.read(&mut buffer).unwrap(), 0);
    }

    #[test]
    fn stream_is_reachable_only_for_occupied_slots() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        assert!(slots.stream_mut(0).is_none());
        let (stream, _peer) = fixture.stream();
        let index = slots.acquire(stream, now).unwrap();
        assert!(slots.stream_mut(index).is_some());
        slots.release(index);
        assert!(slots.stream_mut(index).is_none());
    }

    #[test]
    fn read_into_reports_incomplete_then_too_large() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let (stream, _peer) = fixture.stream();
        let index = slots.acquire(stream, now).unwrap();
        assert_eq!(
            slots.read_into(index, b"GET / HTT"),
            Some(ParseOutcome::Incomplete)
        );
        assert_eq!(
            slots.read_into(index, &[b'a'; HEAD_BYTES]),
            Some(ParseOutcome::TooLarge)
        );
    }

    #[test]
    fn head_at_the_cap_terminates_but_one_byte_over_is_too_large() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let (at_cap, _at_cap_peer) = fixture.stream();
        let at_cap_index = slots.acquire(at_cap, now).unwrap();
        let head = padded_head(HEAD_BYTES);
        assert_eq!(head.len(), HEAD_BYTES);
        assert!(matches!(
            slots.read_into(at_cap_index, &head),
            Some(ParseOutcome::Complete(_))
        ));
        let (over, _over_peer) = fixture.stream();
        let over_index = slots.acquire(over, now).unwrap();
        let head = padded_head(HEAD_BYTES + 1);
        assert_eq!(
            slots.read_into(over_index, &head),
            Some(ParseOutcome::TooLarge)
        );
        let (unterminated, _unterminated_peer) = fixture.stream();
        let unterminated_index = slots.acquire(unterminated, now).unwrap();
        assert_eq!(
            slots.read_into(unterminated_index, &[b'a'; HEAD_BYTES + 1]),
            Some(ParseOutcome::TooLarge)
        );
    }

    #[test]
    fn read_into_propagates_complete_and_malformed() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let (complete_stream, _complete_peer) = fixture.stream();
        let complete = slots.acquire(complete_stream, now).unwrap();
        let outcome = slots
            .read_into(complete, b"GET / HTTP/1.1\r\n\r\n")
            .unwrap();
        let ParseOutcome::Complete(request) = outcome else {
            panic!("expected a complete request, got {outcome:?}");
        };
        assert_eq!(request.method(), Method::Get);
        assert_eq!(request.target(), RequestTarget::Root);
        let (malformed_stream, _malformed_peer) = fixture.stream();
        let malformed = slots.acquire(malformed_stream, now).unwrap();
        assert_eq!(
            slots.read_into(malformed, b"GET / HTTP/2.0\r\n\r\n"),
            Some(ParseOutcome::Malformed(MalformedKind::VersionToken))
        );
    }

    #[test]
    fn expiry_is_strictly_after_the_deadline() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let accepted = Instant::now();
        let (stream, _peer) = fixture.stream();
        let index = slots.acquire(stream, accepted).unwrap();
        assert_eq!(
            slots.take_expired(accepted + CONNECTION_DEADLINE).count(),
            0
        );
        let mut expired =
            slots.take_expired(accepted + CONNECTION_DEADLINE + Duration::from_nanos(1));
        let (taken, _stream) = expired.next().unwrap();
        assert_eq!(taken, index);
        assert!(expired.next().is_none());
        assert!(slots.is_empty());
    }

    #[test]
    fn released_expired_index_carries_a_fresh_deadline_when_reacquired() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let accepted = Instant::now();
        let (stream, _peer) = fixture.stream();
        let index = slots.acquire(stream, accepted).unwrap();
        let late = accepted + CONNECTION_DEADLINE + Duration::from_nanos(1);
        let (taken, _stream) = slots.take_expired(late).next().unwrap();
        assert_eq!(taken, index);
        assert!(slots.is_empty());
        let (replacement, _replacement_peer) = fixture.stream();
        assert_eq!(slots.acquire(replacement, late), Some(index));
        assert_eq!(slots.take_expired(late).count(), 0);
        let (fresh, _fresh_stream) = slots
            .take_expired(late + CONNECTION_DEADLINE + Duration::from_nanos(1))
            .next()
            .unwrap();
        assert_eq!(fresh, index);
    }

    #[test]
    fn occupied_slot_reports_head_room_cursor_and_earliest_deadline() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let accepted = Instant::now();
        assert_eq!(slots.head_room(0), None);
        assert_eq!(slots.cursor(0), None);
        assert_eq!(slots.earliest_deadline(), None);
        let (stream, _peer) = fixture.stream();
        let index = slots.acquire(stream, accepted).unwrap();
        assert_eq!(
            slots.earliest_deadline(),
            Some(accepted + CONNECTION_DEADLINE)
        );
        assert_eq!(slots.head_room(index), Some(HEAD_BYTES));
        assert_eq!(slots.cursor(index), None);
        let head = b"GET / HTTP/1.1\r\n\r\n";
        let outcome = slots.read_into(index, head).unwrap();
        let ParseOutcome::Complete(request) = outcome else {
            panic!("expected a complete request, got {outcome:?}");
        };
        assert_eq!(slots.head_room(index), Some(HEAD_BYTES - head.len()));
        assert!(slots.begin_write(index, ReplyChoice::Matched(request), 120));
        assert_eq!(slots.head_room(index), None);
        assert_eq!(slots.cursor(index), Some(0));
        assert_eq!(
            slots.advance_written(index, 7),
            Some(WriteProgress::Remaining)
        );
        assert_eq!(slots.cursor(index), Some(7));
    }

    #[test]
    fn write_cursor_is_bounded_and_finishes_at_the_supplied_length() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let (stream, _peer) = fixture.stream();
        let index = slots.acquire(stream, now).unwrap();
        let outcome = slots.read_into(index, b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let ParseOutcome::Complete(request) = outcome else {
            panic!("expected a complete request, got {outcome:?}");
        };
        let total = 120;
        assert!(slots.begin_write(index, ReplyChoice::Matched(request), total));
        assert_eq!(slots.phase(index), Some(ConnectionPhase::Writing));
        assert_eq!(slots.pending(index), Some(ReplyChoice::Matched(request)));
        assert_eq!(slots.cursor(index), Some(0));
        assert_eq!(
            slots.advance_written(index, 100),
            Some(WriteProgress::Remaining)
        );
        assert_eq!(
            slots.advance_written(index, 100),
            Some(WriteProgress::Finished)
        );
        assert_eq!(slots.cursor(index), Some(total));
        assert!(!slots.begin_write(index, ReplyChoice::BadRequest, total));
    }

    #[test]
    fn closing_slot_rejects_further_reads() {
        let fixture = Fixture::new();
        let mut slots = Slots::new();
        let now = Instant::now();
        let (stream, _peer) = fixture.stream();
        let index = slots.acquire(stream, now).unwrap();
        assert!(slots.begin_close(index));
        assert_eq!(slots.phase(index), Some(ConnectionPhase::Closing));
        assert_eq!(slots.read_into(index, b"GET / HTTP/1.1\r\n\r\n"), None);
        assert_eq!(slots.pending(index), None);
        assert_eq!(slots.advance_written(index, 0), None);
    }

    #[test]
    fn vacant_slot_rejects_every_operation() {
        let mut slots = Slots::new();
        assert_eq!(slots.read_into(0, b"GET / HTTP/1.1\r\n\r\n"), None);
        assert_eq!(slots.advance_written(0, 1), None);
        assert_eq!(slots.pending(0), None);
        assert_eq!(slots.phase(0), None);
        assert!(!slots.begin_write(0, ReplyChoice::TooLarge, 1));
        assert!(!slots.begin_close(0));
        assert_eq!(slots.len(), 0);
        assert!(slots.is_empty());
    }
}

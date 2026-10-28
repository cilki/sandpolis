//! Unreliable stream messages, carried in QUIC datagrams.
//!
//! A stream can mark a message as one that may be lost: a desktop frame is
//! worth nothing once the next one exists, so queueing it behind its
//! predecessors on a reliable stream only adds latency. Those messages leave
//! the session as datagrams instead of on the stream's QUIC stream.
//!
//! A datagram holds a little over a kilobyte, so a message is cut into
//! *fragments*, each behind an eight byte header:
//!
//! ```text
//! [message: u32][index: u16][count: u16][payload]
//! ```
//!
//! `message` numbers the messages a session sends this way, and the receiver
//! puts each back together from its fragments. A message is delivered whole
//! or not at all: one that loses a fragment is never handed on, and the sender
//! drops a message outright rather than queue part of it when QUIC can't take
//! all of it at once.

use std::collections::HashMap;
use tokio_util::bytes::{Buf, BufMut, Bytes, BytesMut};

/// The header in front of every fragment.
pub const HEADER_LEN: usize = 8;

/// The most fragments one message may be cut into, a little under 90 KiB at
/// the usual datagram size. A message that needs more goes on the stream's
/// reliable QUIC stream instead.
///
/// Losing any one fragment loses the whole message, so the chance of a
/// message arriving falls off with every fragment it needs; and a burst of
/// hundreds of datagrams overruns socket buffers even on loopback, which QUIC
/// would retransmit on a stream but never does for a datagram. A message of
/// ~265 fragments was lost outright on loopback in testing; ~130 arrived.
pub const MAX_FRAGMENTS: usize = 64;

/// How many messages the receiver puts back together at once. Fragments of a
/// message beyond these evict the oldest, whose missing fragments have almost
/// certainly been lost by the time this many newer messages are arriving.
const MAX_PARTIAL: usize = 8;

/// Cut `message` into datagrams no bigger than `max_datagram`, numbered `id`
/// (the session's count of unreliable messages sent so far). `None` when it would take more
/// than [`MAX_FRAGMENTS`], or the datagram is too small to carry anything.
pub fn fragment(id: u32, message: &[u8], max_datagram: usize) -> Option<Vec<Bytes>> {
    let chunk = max_datagram.checked_sub(HEADER_LEN).filter(|&c| c > 0)?;
    let count = message.len().div_ceil(chunk).max(1);
    if count > MAX_FRAGMENTS {
        return None;
    }

    let mut fragments = Vec::with_capacity(count);
    for index in 0..count {
        let start = index * chunk;
        let end = (start + chunk).min(message.len());
        let mut fragment = BytesMut::with_capacity(HEADER_LEN + end - start);
        fragment.put_u32(id);
        fragment.put_u16(index as u16);
        fragment.put_u16(count as u16);
        fragment.extend_from_slice(&message[start..end]);
        fragments.push(fragment.freeze());
    }
    Some(fragments)
}

/// One message part of the way back together.
struct Partial {
    fragments: Vec<Option<Bytes>>,
    missing: usize,
    /// When the first fragment arrived, in fragments received by the
    /// [`Reassembler`]; the smallest is the one to evict.
    arrived: u64,
}

/// Puts messages back together from the fragments the peer sent.
#[derive(Default)]
pub struct Reassembler {
    partial: HashMap<u32, Partial>,
    received: u64,
}

impl Reassembler {
    /// Take one datagram. Returns the message it completes, if it does.
    ///
    /// A malformed datagram, or a duplicate fragment, is dropped: either the
    /// peer is confused or the network duplicated a packet, and neither is
    /// worth ending the session over.
    pub fn push(&mut self, mut datagram: Bytes) -> Option<Bytes> {
        if datagram.len() < HEADER_LEN {
            return None;
        }
        let id = datagram.get_u32();
        let index = datagram.get_u16() as usize;
        let count = datagram.get_u16() as usize;
        if count == 0 || count > MAX_FRAGMENTS || index >= count {
            return None;
        }
        self.received += 1;

        // The common case for anything small.
        if count == 1 {
            return Some(datagram);
        }

        if !self.partial.contains_key(&id) && self.partial.len() >= MAX_PARTIAL {
            let oldest = self
                .partial
                .iter()
                .min_by_key(|(_, partial)| partial.arrived)
                .map(|(id, _)| *id)?;
            self.partial.remove(&oldest);
        }

        let arrived = self.received;
        let partial = self.partial.entry(id).or_insert_with(|| Partial {
            fragments: vec![None; count],
            missing: count,
            arrived,
        });
        // A message id is reused only after 2^32 others, so a fragment that
        // disagrees about the count belongs to nothing we're assembling.
        if partial.fragments.len() != count {
            return None;
        }
        let slot = &mut partial.fragments[index];
        if slot.is_some() {
            return None;
        }
        *slot = Some(datagram);
        partial.missing -= 1;
        if partial.missing > 0 {
            return None;
        }

        let partial = self.partial.remove(&id)?;
        let mut message =
            BytesMut::with_capacity(partial.fragments.iter().flatten().map(Bytes::len).sum());
        for fragment in partial.fragments.into_iter().flatten() {
            message.extend_from_slice(&fragment);
        }
        Some(message.freeze())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn message(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Every size round-trips, including the edges of a fragment.
    #[test]
    fn fragments_reassemble() {
        for len in [0, 1, 91, 92, 93, 1000, 92 * MAX_FRAGMENTS] {
            let data = message(len);
            let fragments = fragment(7, &data, 100).unwrap();
            assert!(fragments.iter().all(|f| f.len() <= 100));
            assert_eq!(fragments.len(), len.div_ceil(92).max(1));

            let mut reassembler = Reassembler::default();
            let (last, rest) = fragments.split_last().unwrap();
            for f in rest {
                assert_eq!(reassembler.push(f.clone()), None);
            }
            assert_eq!(reassembler.push(last.clone()).as_deref(), Some(&data[..]));
        }
    }

    /// Datagrams can arrive in any order.
    #[test]
    fn out_of_order_fragments_reassemble() {
        let data = message(1000);
        let mut fragments = fragment(1, &data, 100).unwrap();
        fragments.reverse();

        let mut reassembler = Reassembler::default();
        let mut out = None;
        for f in fragments {
            out = reassembler.push(f).or(out);
        }
        assert_eq!(out.as_deref(), Some(&data[..]));
    }

    /// A message missing a fragment is never delivered, and doesn't stop the
    /// messages after it.
    #[test]
    fn a_lost_fragment_loses_only_its_message() {
        let (a, b) = (message(500), message(300));
        let mut lossy = fragment(1, &a, 100).unwrap();
        lossy.remove(2);

        let mut reassembler = Reassembler::default();
        for f in lossy {
            assert_eq!(reassembler.push(f), None);
        }
        let mut out = None;
        for f in fragment(2, &b, 100).unwrap() {
            out = reassembler.push(f).or(out);
        }
        assert_eq!(out.as_deref(), Some(&b[..]));
    }

    /// A network-duplicated fragment doesn't complete a message early or
    /// corrupt it.
    #[test]
    fn duplicate_fragments_are_ignored() {
        let data = message(300);
        let fragments = fragment(3, &data, 100).unwrap();

        let mut reassembler = Reassembler::default();
        assert_eq!(reassembler.push(fragments[0].clone()), None);
        assert_eq!(reassembler.push(fragments[0].clone()), None);
        assert_eq!(reassembler.push(fragments[1].clone()), None);
        assert_eq!(reassembler.push(fragments[2].clone()), None);
        assert_eq!(
            reassembler.push(fragments[3].clone()).as_deref(),
            Some(&data[..])
        );
    }

    /// Messages that never complete don't accumulate: past the limit the
    /// oldest is given up on.
    #[test]
    fn incomplete_messages_are_evicted() {
        let mut reassembler = Reassembler::default();
        for id in 0..(MAX_PARTIAL as u32 * 4) {
            let fragments = fragment(id, &message(300), 100).unwrap();
            reassembler.push(fragments[0].clone());
        }
        assert_eq!(reassembler.partial.len(), MAX_PARTIAL);

        // The survivors are the newest.
        assert!(
            reassembler
                .partial
                .contains_key(&(MAX_PARTIAL as u32 * 4 - 1))
        );
        assert!(!reassembler.partial.contains_key(&0));
    }

    /// Too big a message is refused rather than cut into a fragment count it
    /// can't survive, and garbage is dropped rather than trusted.
    #[test]
    fn limits_hold() {
        assert!(fragment(0, &message(92 * MAX_FRAGMENTS), 100).is_some());
        assert!(fragment(0, &message(92 * MAX_FRAGMENTS + 1), 100).is_none());
        assert!(fragment(0, b"x", HEADER_LEN).is_none());

        let mut reassembler = Reassembler::default();
        assert_eq!(reassembler.push(Bytes::from_static(&[1, 2, 3])), None);
        // index 2 of a 2-fragment message
        assert_eq!(
            reassembler.push(Bytes::from_static(&[0, 0, 0, 1, 0, 2, 0, 2, 9])),
            None
        );
        // count 0
        assert_eq!(
            reassembler.push(Bytes::from_static(&[0, 0, 0, 1, 0, 0, 0, 0, 9])),
            None
        );
        assert!(reassembler.partial.is_empty());
    }
}

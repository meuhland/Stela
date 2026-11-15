//! Checksum offloads done natively for the guest (design-stelanet.md 3.3):
//! the transmit checksum a `virtio_net_hdr_v1` marks `NEEDS_CSUM` is
//! completed before the frame leaves, and a received TCP or UDP checksum is
//! verified so the header can say `DATA_VALID` and the guest skips its own
//! check. Both are the Internet checksum (RFC 1071) over frame bytes.
//!
//! Received TCP segments of one flow that arrive together may also be
//! joined into one large frame ([`Coalesced`], LRO), so the guest's stack
//! takes one segment where the wire carried dozens.

/// `virtio_net_hdr_v1` flags.
pub const NEEDS_CSUM: u8 = 1;
pub const DATA_VALID: u8 = 2;
/// `gso_type`: none. The device segments nothing the guest sends.
pub const GSO_NONE: u8 = 0;

const ETHER_HEADER: usize = 14;
const IPV4: u16 = 0x0800;
const IPV6: u16 = 0x86dd;
const TCP: u8 = 6;
const UDP: u8 = 17;

/// The fields of a `virtio_net_hdr_v1` the device reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub flags: u8,
    pub gso_type: u8,
    pub csum_start: u16,
    pub csum_offset: u16,
}

impl Header {
    pub fn parse(bytes: &[u8; 12]) -> Self {
        Self {
            flags: bytes[0],
            gso_type: bytes[1],
            csum_start: u16::from_le_bytes([bytes[6], bytes[7]]),
            csum_offset: u16::from_le_bytes([bytes[8], bytes[9]]),
        }
    }
}

/// The ones' complement sum of `bytes` as big-endian 16-bit words, added to
/// `sum`, unfolded. Eight bytes at a time: 2^16 is 1 modulo 0xffff, so the
/// two big-endian 32-bit halves of each chunk add as their four words do.
fn add(mut sum: u64, bytes: &[u8]) -> u64 {
    let (chunks, rest) = bytes.as_chunks::<8>();
    for chunk in chunks {
        let value = u64::from_be_bytes(*chunk);
        sum += (value >> 32) + (value & 0xffff_ffff);
    }
    let (words, rest) = rest.as_chunks::<2>();
    for word in words {
        sum += u64::from(u16::from_be_bytes(*word));
    }
    if let [last] = rest {
        sum += u64::from(*last) << 8;
    }
    sum
}

fn fold(mut sum: u64) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

/// Completes a partial checksum: the ones' complement of the sum of
/// `frame[start..]` (whose checksum field holds the pseudo-header sum the
/// stack seeded) stored big-endian at `start + offset`. A result of zero is
/// stored as 0xffff, which means the same to TCP and is required of UDP.
/// `None` when the field does not lie inside the frame.
pub fn complete(frame: &mut [u8], start: u16, offset: u16) -> Option<()> {
    let start = usize::from(start);
    let field = start.checked_add(usize::from(offset))?;
    if start >= frame.len() || field + 2 > frame.len() {
        return None;
    }
    let mut checksum = !fold(add(0, &frame[start..]));
    if checksum == 0 {
        checksum = 0xffff;
    }
    frame[field..field + 2].copy_from_slice(&checksum.to_be_bytes());
    Some(())
}

/// Whether `frame` is an unfragmented TCP or UDP packet over IPv4 (with a
/// good header checksum) or IPv6 (no extension headers) whose transport
/// checksum is good. Anything else, VLAN tags and zero UDP checksums
/// included, is left for the guest to judge.
pub fn verified(frame: &[u8]) -> bool {
    let Some(&[high, low]) = frame.get(12..14) else {
        return false;
    };
    let ip = &frame[ETHER_HEADER..];
    let (protocol, pseudo, transport) = match u16::from_be_bytes([high, low]) {
        IPV4 => {
            let Some(&first) = ip.first() else {
                return false;
            };
            let header = usize::from(first & 0xf) * 4;
            if first >> 4 != 4 || header < 20 || ip.len() < header {
                return false;
            }
            let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
            let fragment = u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff;
            if total < header
                || total > ip.len()
                || fragment != 0
                || fold(add(0, &ip[..header])) != 0xffff
            {
                return false;
            }
            let transport = &ip[header..total];
            let protocol = ip[9];
            let mut pseudo = add(0, &ip[12..20]);
            pseudo += u64::from(protocol) + transport.len() as u64;
            (protocol, pseudo, transport)
        }
        IPV6 => {
            if ip.len() < 40 || ip[0] >> 4 != 6 {
                return false;
            }
            let payload = usize::from(u16::from_be_bytes([ip[4], ip[5]]));
            if 40 + payload > ip.len() {
                return false;
            }
            let transport = &ip[40..40 + payload];
            let protocol = ip[6];
            let mut pseudo = add(0, &ip[8..40]);
            pseudo += u64::from(protocol) + transport.len() as u64;
            (protocol, pseudo, transport)
        }
        _ => return false,
    };
    let field = match protocol {
        TCP if transport.len() >= 20 => 16,
        UDP if transport.len() >= 8 => 6,
        _ => return false,
    };
    if protocol == UDP && transport[field..field + 2] == [0, 0] {
        return false;
    }
    fold(add(pseudo, transport)) == 0xffff
}

/// `gso_type`s of a coalesced receive frame.
pub const GSO_TCPV4: u8 = 1;
pub const GSO_TCPV6: u8 = 4;
/// The longest IP datagram a coalesced frame carries: IPv4's total length
/// and IPv6's payload length are 16-bit.
const DATAGRAM_MAX: usize = 0xffff;
/// The longest coalesced frame: an Ethernet header, an IPv6 header and the
/// longest payload it may name.
pub const COALESCED_FRAME_MAX: usize = ETHER_HEADER + 40 + DATAGRAM_MAX;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;

/// Where a TCP segment's parts lie in its frame, as coalescing needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    v6: bool,
    /// Offset of the TCP header.
    tcp: usize,
    /// Bytes of Ethernet, IP and TCP headers: where the payload starts.
    headers: usize,
    sequence: u32,
    flags: u8,
    payload: usize,
}

impl Segment {
    /// A TCP segment that may be coalesced: over IPv4 without options or
    /// fragmentation, or IPv6 without extension headers; flags ACK and
    /// perhaps PSH, nothing else; some payload; the frame ending where the
    /// datagram does. Checksums are the caller's ([`verified`]).
    fn parse(frame: &[u8]) -> Option<Self> {
        let ip = frame.get(ETHER_HEADER..)?;
        let (v6, end) = match u16::from_be_bytes([*frame.get(12)?, *frame.get(13)?]) {
            IPV4 => {
                if ip.len() < 20 || ip[0] != 0x45 || ip[9] != TCP {
                    return None;
                }
                if u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 {
                    return None;
                }
                (false, usize::from(u16::from_be_bytes([ip[2], ip[3]])))
            }
            IPV6 => {
                if ip.len() < 40 || ip[0] >> 4 != 6 || ip[6] != TCP {
                    return None;
                }
                (true, 40 + usize::from(u16::from_be_bytes([ip[4], ip[5]])))
            }
            _ => return None,
        };
        let tcp = ETHER_HEADER + if v6 { 40 } else { 20 };
        if ETHER_HEADER + end != frame.len() || frame.len() < tcp + 20 {
            return None;
        }
        let headers = tcp + usize::from(frame[tcp + 12] >> 4) * 4;
        let flags = frame[tcp + 13];
        if headers < tcp + 20 || headers >= frame.len() || flags & !TCP_PSH != TCP_ACK {
            return None;
        }
        Some(Self {
            v6,
            tcp,
            headers,
            sequence: u32::from_be_bytes(frame[tcp + 4..tcp + 8].try_into().unwrap()),
            flags,
            payload: frame.len() - headers,
        })
    }
}

/// Consecutive in-order segments of one TCP flow joined into one frame
/// (LRO), as Linux's GRO joins them: the same addresses, ports,
/// acknowledgment, TCP options and IP fields; every segment the first's
/// payload size but the last, which may be shorter; a PSH ending the run;
/// at most a 64 KiB datagram. The frame keeps the first segment's headers
/// with the last one's window, and is whole once [`Self::finish`] fixes
/// its lengths and checksums.
#[derive(Debug)]
pub struct Coalesced {
    frame: alloc::vec::Vec<u8>,
    first: Segment,
    next_sequence: u32,
    segments: u32,
    /// A PSH or a short segment came: nothing may follow.
    ended: bool,
}

/// What a coalesced frame's `virtio_net_hdr_v1` says of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gso {
    pub gso_type: u8,
    /// Bytes of Ethernet, IP and TCP headers.
    pub hdr_len: u16,
    /// The segments' payload size.
    pub gso_size: u16,
}

impl Coalesced {
    /// A run starting with `frame`, a TCP segment whose checksums the
    /// caller verified; the frame back when it cannot start one.
    pub fn start(frame: alloc::vec::Vec<u8>) -> Result<Self, alloc::vec::Vec<u8>> {
        let Some(first) = Segment::parse(&frame) else {
            return Err(frame);
        };
        Ok(Self {
            next_sequence: first.sequence.wrapping_add(first.payload as u32),
            ended: first.flags & TCP_PSH != 0,
            frame,
            first,
            segments: 1,
        })
    }

    /// The segments joined so far.
    pub fn segments(&self) -> u32 {
        self.segments
    }

    /// Whether `next` continues the run; its checksums are still the
    /// caller's to verify before [`Self::append`].
    pub fn takes(&self, next: &[u8]) -> bool {
        self.continuation(next).is_some()
    }

    fn continuation(&self, next: &[u8]) -> Option<Segment> {
        if self.ended {
            return None;
        }
        let segment = Segment::parse(next)?;
        let first = &self.first;
        let base = &self.frame;
        let ip_header = if first.v6 { 40 } else { 0 };
        let joined = base.len() - ETHER_HEADER - ip_header + segment.payload;
        let same = |range: core::ops::Range<usize>| base[range.clone()] == next[range];
        let fits = segment.v6 == first.v6
            && segment.headers == first.headers
            && segment.sequence == self.next_sequence
            && segment.payload <= first.payload
            && joined <= DATAGRAM_MAX
            && same(0..ETHER_HEADER)
            && if first.v6 {
                // Version, class and flow label; next header and hop
                // limit; both addresses.
                same(14..18) && same(20..54)
            } else {
                // Version and TOS; DF; TTL, protocol; both addresses.
                same(14..16)
                    && base[20] & 0xe0 == next[20] & 0xe0
                    && same(22..24)
                    && same(26..34)
            }
            // Ports, acknowledgment, data offset, options.
            && same(first.tcp..first.tcp + 4)
            && same(first.tcp + 8..first.tcp + 13)
            && same(first.tcp + 20..first.headers);
        fits.then_some(segment)
    }

    /// Joins `next`'s payload when it continues the run, taking its window
    /// and PSH; whether it did.
    pub fn append(&mut self, next: &[u8]) -> bool {
        let Some(segment) = self.continuation(next) else {
            return false;
        };
        let tcp = self.first.tcp;
        self.frame.extend_from_slice(&next[segment.headers..]);
        self.frame[tcp + 14..tcp + 16].copy_from_slice(&next[tcp + 14..tcp + 16]);
        self.frame[tcp + 13] |= segment.flags & TCP_PSH;
        self.next_sequence = self.next_sequence.wrapping_add(segment.payload as u32);
        self.segments += 1;
        self.ended = segment.flags & TCP_PSH != 0 || segment.payload < self.first.payload;
        true
    }

    /// The frame, and for more than one segment its lengths and checksums
    /// made whole for the joined payload and what its header says of it.
    pub fn finish(mut self) -> (alloc::vec::Vec<u8>, Option<Gso>) {
        if self.segments == 1 {
            return (self.frame, None);
        }
        let Segment {
            v6, tcp, headers, ..
        } = self.first;
        let frame = &mut self.frame;
        let ip = ETHER_HEADER;
        let mut pseudo;
        if v6 {
            let payload = (frame.len() - ip - 40) as u16;
            frame[ip + 4..ip + 6].copy_from_slice(&payload.to_be_bytes());
            pseudo = add(0, &frame[ip + 8..ip + 40]);
        } else {
            let total = (frame.len() - ip) as u16;
            frame[ip + 2..ip + 4].copy_from_slice(&total.to_be_bytes());
            frame[ip + 10..ip + 12].copy_from_slice(&[0, 0]);
            let checksum = !fold(add(0, &frame[ip..ip + 20]));
            frame[ip + 10..ip + 12].copy_from_slice(&checksum.to_be_bytes());
            pseudo = add(0, &frame[ip + 12..ip + 20]);
        }
        pseudo += u64::from(TCP) + (frame.len() - tcp) as u64;
        frame[tcp + 16..tcp + 18].copy_from_slice(&[0, 0]);
        let checksum = !fold(add(pseudo, &frame[tcp..]));
        frame[tcp + 16..tcp + 18].copy_from_slice(&checksum.to_be_bytes());
        let gso = Gso {
            gso_type: if v6 { GSO_TCPV6 } else { GSO_TCPV4 },
            hdr_len: headers as u16,
            gso_size: self.first.payload as u16,
        };
        (self.frame, Some(gso))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// An IPv4 TCP segment from 10.0.0.1:1234 to 10.0.0.2:80 with `data`,
    /// its checksums filled in by a sum written apart from the code under
    /// test (word by word, carries folded at the end).
    pub(in crate::stela_net) fn tcp4(data: &[u8]) -> Vec<u8> {
        tcp4_segment(0, TCP_ACK | TCP_PSH, data)
    }

    /// As [`tcp4`], at `sequence` with TCP `flags`.
    pub(in crate::stela_net) fn tcp4_segment(sequence: u32, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut frame = alloc::vec![0u8; 14 + 20 + 20];
        frame[38..42].copy_from_slice(&sequence.to_be_bytes());
        frame[42..46].copy_from_slice(&0x1000u32.to_be_bytes());
        frame[48..50].copy_from_slice(&0x2000u16.to_be_bytes());
        frame[12..14].copy_from_slice(&IPV4.to_be_bytes());
        let total = (20 + 20 + data.len()) as u16;
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&total.to_be_bytes());
        frame[22] = 64;
        frame[23] = TCP;
        frame[26..30].copy_from_slice(&[10, 0, 0, 1]);
        frame[30..34].copy_from_slice(&[10, 0, 0, 2]);
        frame[34..36].copy_from_slice(&1234u16.to_be_bytes());
        frame[36..38].copy_from_slice(&80u16.to_be_bytes());
        frame[46] = 0x50;
        frame[47] = flags;
        frame.extend_from_slice(data);
        let ip = reference(&frame[14..34]);
        frame[24..26].copy_from_slice(&ip.to_be_bytes());
        let mut pseudo = Vec::new();
        pseudo.extend_from_slice(&frame[26..34]);
        pseudo.extend_from_slice(&[0, TCP]);
        pseudo.extend_from_slice(&(total - 20).to_be_bytes());
        pseudo.extend_from_slice(&frame[34..]);
        let tcp = reference(&pseudo);
        frame[50..52].copy_from_slice(&tcp.to_be_bytes());
        frame
    }

    /// An IPv6 TCP segment from fe80::1:1234 to fe80::2:80 at `sequence`
    /// with `flags` and `data`, its checksum by [`reference`].
    pub(in crate::stela_net) fn tcp6_segment(sequence: u32, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut frame = alloc::vec![0u8; 14 + 40 + 20];
        frame[12..14].copy_from_slice(&IPV6.to_be_bytes());
        frame[14] = 0x60;
        frame[18..20].copy_from_slice(&((20 + data.len()) as u16).to_be_bytes());
        frame[20] = TCP;
        frame[21] = 64;
        frame[22..24].copy_from_slice(&[0xfe, 0x80]);
        frame[37] = 1;
        frame[38..40].copy_from_slice(&[0xfe, 0x80]);
        frame[53] = 2;
        frame[54..56].copy_from_slice(&1234u16.to_be_bytes());
        frame[56..58].copy_from_slice(&80u16.to_be_bytes());
        frame[58..62].copy_from_slice(&sequence.to_be_bytes());
        frame[62..66].copy_from_slice(&7u32.to_be_bytes());
        frame[66] = 0x50;
        frame[67] = flags;
        frame[68..70].copy_from_slice(&0x2000u16.to_be_bytes());
        frame.extend_from_slice(data);
        let mut pseudo = Vec::new();
        pseudo.extend_from_slice(&frame[22..54]);
        pseudo.extend_from_slice(&((20 + data.len()) as u32).to_be_bytes());
        pseudo.extend_from_slice(&[0, 0, 0, TCP]);
        pseudo.extend_from_slice(&frame[54..]);
        let checksum = reference(&pseudo);
        frame[70..72].copy_from_slice(&checksum.to_be_bytes());
        frame
    }

    fn payload(seed: usize, length: usize) -> Vec<u8> {
        (0..length).map(|i| (i * 31 + seed * 7) as u8).collect()
    }

    /// Joins `frames` as the device does: each verified, then appended while
    /// the run takes it.
    fn join(frames: &[Vec<u8>]) -> (Vec<u8>, Option<Gso>, u32, usize) {
        let mut run = Coalesced::start(frames[0].clone()).unwrap();
        let mut taken = 1;
        for next in &frames[1..] {
            assert!(verified(next));
            if !run.takes(next) {
                break;
            }
            assert!(run.append(next));
            taken += 1;
        }
        let segments = run.segments();
        let (frame, gso) = run.finish();
        (frame, gso, segments, taken)
    }

    #[test]
    fn the_wide_sum_matches_the_reference_at_every_length_and_offset() {
        let bytes = payload(3, 80);
        for start in 0..8 {
            for end in start..bytes.len() {
                let slice = &bytes[start..end];
                assert_eq!(!fold(add(0, slice)), reference(slice), "{start}..{end}");
            }
        }
        let ones = alloc::vec![0xffu8; 70_000];
        assert_eq!(!fold(add(0, &ones)), reference(&ones));
    }

    #[test]
    fn in_order_tcp4_segments_join_into_one_valid_frame() {
        let mss = 1448;
        let datas: Vec<Vec<u8>> = (0..4)
            .map(|i| payload(i, if i == 3 { 999 } else { mss }))
            .collect();
        let mut sequence = 0x1234_5678u32;
        let mut frames = Vec::new();
        for (i, data) in datas.iter().enumerate() {
            let flags = if i == 3 { TCP_ACK | TCP_PSH } else { TCP_ACK };
            let mut frame = tcp4_segment(sequence, flags, data);
            if i == 3 {
                // The last window is the one the frame keeps.
                frame[48..50].copy_from_slice(&0x3000u16.to_be_bytes());
                frame[50..52].copy_from_slice(&[0, 0]);
                let mut pseudo = frame[26..34].to_vec();
                pseudo.extend_from_slice(&[0, TCP]);
                pseudo.extend_from_slice(&((20 + data.len()) as u16).to_be_bytes());
                pseudo.extend_from_slice(&frame[34..]);
                let checksum = reference(&pseudo);
                frame[50..52].copy_from_slice(&checksum.to_be_bytes());
            }
            frames.push(frame);
            sequence = sequence.wrapping_add(data.len() as u32);
        }
        // A fifth segment after the PSH stays apart.
        frames.push(tcp4_segment(sequence, TCP_ACK, &payload(9, mss)));
        let (joined, gso, segments, taken) = join(&frames);
        assert_eq!((segments, taken), (4, 4));
        assert_eq!(
            gso,
            Some(Gso {
                gso_type: GSO_TCPV4,
                hdr_len: 54,
                gso_size: mss as u16
            })
        );
        assert!(verified(&joined));
        let all: Vec<u8> = datas.concat();
        assert_eq!(&joined[54..], &all[..]);
        assert_eq!(
            usize::from(u16::from_be_bytes([joined[16], joined[17]])),
            40 + all.len()
        );
        assert_eq!(joined[38..42], 0x1234_5678u32.to_be_bytes());
        assert_eq!(joined[47], TCP_ACK | TCP_PSH);
        assert_eq!(joined[48..50], 0x3000u16.to_be_bytes());
        // One segment comes back as it was.
        let lone = tcp4_segment(5, TCP_ACK, b"alone");
        let (frame, gso, segments, _) = join(core::slice::from_ref(&lone));
        assert_eq!((frame, gso, segments), (lone, None, 1));
    }

    #[test]
    fn tcp6_segments_join_and_stay_valid() {
        let frames: Vec<Vec<u8>> = (0..3)
            .map(|i| tcp6_segment(100 + 1000 * i, TCP_ACK, &payload(i as usize, 1000)))
            .collect();
        let (joined, gso, segments, _) = join(&frames);
        assert_eq!(segments, 3);
        assert_eq!(gso.map(|gso| gso.gso_type), Some(GSO_TCPV6));
        assert_eq!(
            usize::from(u16::from_be_bytes([joined[18], joined[19]])),
            20 + 3000
        );
        assert!(verified(&joined));
    }

    #[test]
    fn only_continuations_of_the_same_flow_join() {
        let first = tcp4_segment(0, TCP_ACK, &payload(0, 100));
        let run = Coalesced::start(first.clone()).unwrap();
        let next = |sequence, flags, length| tcp4_segment(sequence, flags, &payload(1, length));
        assert!(run.takes(&next(100, TCP_ACK, 100)));
        assert!(run.takes(&next(100, TCP_ACK | TCP_PSH, 50)));
        assert!(!run.takes(&next(101, TCP_ACK, 100)), "a gap");
        assert!(!run.takes(&next(0, TCP_ACK, 100)), "a retransmission");
        assert!(
            !run.takes(&next(100, TCP_ACK, 101)),
            "longer than the first"
        );
        for flags in [0x01, 0x02, 0x04, 0x20, 0x40, 0x80] {
            assert!(!run.takes(&next(100, TCP_ACK | flags, 100)), "{flags:#x}");
        }
        let changed = |at: usize| {
            let mut frame = next(100, TCP_ACK, 100);
            frame[at] ^= 1;
            frame
        };
        // Destination MAC, TOS, TTL, source address, ports, acknowledgment,
        // data offset.
        for at in [0, 15, 22, 29, 35, 37, 45, 46] {
            assert!(!run.takes(&changed(at)), "byte {at}");
        }
        // The IP ID and the window may differ.
        assert!(run.takes(&changed(19)));
        assert!(run.takes(&changed(49)));
        // Padding after the datagram, and an IPv6 segment, do not join.
        let mut padded = next(100, TCP_ACK, 100);
        padded.push(0);
        assert!(!run.takes(&padded));
        assert!(!run.takes(&tcp6_segment(100, TCP_ACK, &payload(1, 100))));
        // A run cannot start on a segment without payload, a SYN or UDP.
        assert!(Coalesced::start(tcp4_segment(0, TCP_ACK, b"")).is_err());
        assert!(Coalesced::start(tcp4_segment(0, 0x02, b"x")).is_err());
        // A PSH first segment takes nothing after it.
        let pushed =
            Coalesced::start(tcp4_segment(0, TCP_ACK | TCP_PSH, &payload(0, 100))).unwrap();
        assert!(!pushed.takes(&next(100, TCP_ACK, 100)));
    }

    #[test]
    fn a_run_stops_short_of_a_64k_datagram() {
        let mss = 1460;
        let frames: Vec<Vec<u8>> = (0..50)
            .map(|i| tcp4_segment((i * mss) as u32, TCP_ACK, &payload(i, mss)))
            .collect();
        let (joined, _, segments, _) = join(&frames);
        // 40 header bytes and 44 segments of 1460 are 64280; a 45th is past
        // 65535.
        assert_eq!(segments, 44);
        assert_eq!(joined.len(), 14 + 40 + 44 * mss);
        assert!(verified(&joined));
    }

    pub(in crate::stela_net) fn reference(bytes: &[u8]) -> u16 {
        let mut sum = 0u32;
        let mut index = 0;
        while index < bytes.len() {
            let high = u32::from(bytes[index]) << 8;
            let low = bytes.get(index + 1).copied().map_or(0, u32::from);
            sum += high | low;
            index += 2;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }

    #[test]
    fn a_good_tcp4_segment_verifies_and_a_flipped_bit_does_not() {
        for length in [0usize, 1, 7, 100, 1460] {
            let data: Vec<u8> = (0..length).map(|i| (i * 7) as u8).collect();
            let mut frame = tcp4(&data);
            assert!(verified(&frame), "length {length}");
            let last = frame.len() - 1;
            frame[last] ^= 0x10;
            assert!(!verified(&frame) || length == 0, "length {length}");
        }
        // A bad IP header checksum, a fragment, a VLAN tag: not verified.
        let mut frame = tcp4(b"x");
        frame[24] ^= 1;
        assert!(!verified(&frame));
        let mut frame = tcp4(b"x");
        frame[20] = 0x20;
        assert!(!verified(&frame));
        let mut frame = tcp4(b"x");
        frame[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
        assert!(!verified(&frame));
        assert!(!verified(&[0; 13]));
        assert!(!verified(&frame[..20]));
    }

    #[test]
    fn a_partial_checksum_completes_to_the_reference() {
        for length in [0usize, 1, 2, 3, 99, 1460] {
            let data: Vec<u8> = (0..length).map(|i| (i * 13 + 5) as u8).collect();
            let good = tcp4(&data);
            // The stack leaves the pseudo-header sum, not complemented, in
            // the checksum field.
            let mut partial = good.clone();
            let mut pseudo = Vec::new();
            pseudo.extend_from_slice(&good[26..34]);
            pseudo.extend_from_slice(&[0, TCP]);
            pseudo.extend_from_slice(&((20 + length) as u16).to_be_bytes());
            let seed = !reference(&pseudo);
            partial[50..52].copy_from_slice(&seed.to_be_bytes());
            complete(&mut partial, 34, 16).unwrap();
            let expected = if good[50..52] == [0, 0] {
                [0xff, 0xff]
            } else {
                [good[50], good[51]]
            };
            assert_eq!(partial[50..52], expected, "length {length}");
            assert!(verified(&partial));
        }
        let mut short = tcp4(b"");
        let length = short.len() as u16;
        assert!(complete(&mut short, length, 0).is_none());
        assert!(complete(&mut short, 34, length).is_none());
    }

    #[test]
    fn udp6_verifies_and_a_zero_udp_checksum_is_left_to_the_guest() {
        let data = b"stelanet";
        let mut frame = alloc::vec![0u8; 14 + 40 + 8];
        frame[12..14].copy_from_slice(&IPV6.to_be_bytes());
        frame[14] = 0x60;
        frame[18..20].copy_from_slice(&((8 + data.len()) as u16).to_be_bytes());
        frame[20] = UDP;
        frame[21] = 64;
        frame[22..38].copy_from_slice(&[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        frame[38..54].copy_from_slice(&[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        frame[54..56].copy_from_slice(&5353u16.to_be_bytes());
        frame[56..58].copy_from_slice(&5353u16.to_be_bytes());
        frame[58..60].copy_from_slice(&((8 + data.len()) as u16).to_be_bytes());
        frame.extend_from_slice(data);
        assert!(!verified(&frame), "a zero UDP checksum");
        let mut pseudo = Vec::new();
        pseudo.extend_from_slice(&frame[22..54]);
        pseudo.extend_from_slice(&((8 + data.len()) as u32).to_be_bytes());
        pseudo.extend_from_slice(&[0, 0, 0, UDP]);
        pseudo.extend_from_slice(&frame[54..]);
        let checksum = reference(&pseudo);
        frame[60..62].copy_from_slice(&checksum.to_be_bytes());
        assert!(verified(&frame));
        frame[62] ^= 0xff;
        assert!(!verified(&frame));
    }

    #[test]
    fn headers_parse_their_fields() {
        let bytes = [NEEDS_CSUM, GSO_NONE, 0, 0, 0, 0, 34, 0, 16, 0, 1, 0];
        assert_eq!(
            Header::parse(&bytes),
            Header {
                flags: NEEDS_CSUM,
                gso_type: GSO_NONE,
                csum_start: 34,
                csum_offset: 16,
            }
        );
    }
}

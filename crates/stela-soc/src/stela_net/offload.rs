//! Checksum offloads done natively for the guest (design-stelanet.md 3.3):
//! the transmit checksum a `virtio_net_hdr_v1` marks `NEEDS_CSUM` is
//! completed before the frame leaves, and a received TCP or UDP checksum is
//! verified so the header can say `DATA_VALID` and the guest skips its own
//! check. Both are the Internet checksum (RFC 1071) over frame bytes.

/// `virtio_net_hdr_v1` flags.
pub const NEEDS_CSUM: u8 = 1;
pub const DATA_VALID: u8 = 2;
/// `gso_type`: none. Segmentation is not offered.
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
/// `sum`, unfolded.
fn add(mut sum: u64, bytes: &[u8]) -> u64 {
    let (words, rest) = bytes.as_chunks::<2>();
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// An IPv4 TCP segment from 10.0.0.1:1234 to 10.0.0.2:80 with `data`,
    /// its checksums filled in by a sum written apart from the code under
    /// test (word by word, carries folded at the end).
    pub(in crate::stela_net) fn tcp4(data: &[u8]) -> Vec<u8> {
        let mut frame = alloc::vec![0u8; 14 + 20 + 20];
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
        frame[47] = 0x18;
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

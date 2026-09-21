// PROXY protocol v2 header parsing. Only the binary v2 header is accepted; the
// v1 text header and a missing header are errors.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const SIGNATURE: [u8; 12] = [
    0x0d, 0x0a, 0x0d, 0x0a, 0x00, 0x0d, 0x0a, 0x51, 0x55, 0x49, 0x54, 0x0a,
];
const V1_SIGNATURE: &[u8] = b"PROXY ";
/// Signature, version/command, family and the length of the address block.
const PREFIX_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProxyError {
    Signature,
    Version,
    Command,
    Family,
    TooLong,
}

impl ProxyError {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProxyError::Signature => "signature",
            ProxyError::Version => "version",
            ProxyError::Command => "command",
            ProxyError::Family => "family",
            ProxyError::TooLong => "too_long",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProxyHeader {
    /// A valid prefix so far, but more bytes are needed.
    Incomplete,
    /// A complete header; `source` is absent for LOCAL and unspecified families.
    Complete {
        source: Option<IpAddr>,
        length: usize,
    },
}

/// Parse a PROXY v2 header from the front of `buf`, rejecting anything longer
/// than `max_length` bytes.
pub(crate) fn parse(buf: &[u8], max_length: usize) -> Result<ProxyHeader, ProxyError> {
    if buf.is_empty() {
        return Ok(ProxyHeader::Incomplete);
    }

    let seen = buf.len().min(V1_SIGNATURE.len());
    if buf[..seen] == V1_SIGNATURE[..seen] {
        return Err(ProxyError::Version);
    }

    let seen = buf.len().min(SIGNATURE.len());
    if buf[..seen] != SIGNATURE[..seen] {
        return Err(ProxyError::Signature);
    }

    if buf.len() < PREFIX_LEN {
        return Ok(ProxyHeader::Incomplete);
    }

    let version_command = buf[12];
    if version_command >> 4 != 0x2 {
        return Err(ProxyError::Version);
    }

    let command = version_command & 0x0f;
    if command > 0x1 {
        return Err(ProxyError::Command);
    }

    let family = buf[13];
    let address_length = u16::from_be_bytes([buf[14], buf[15]]) as usize;
    let length = PREFIX_LEN + address_length;
    if length > max_length {
        return Err(ProxyError::TooLong);
    }

    if buf.len() < length {
        return Ok(ProxyHeader::Incomplete);
    }

    // LOCAL connections (health checks) carry no meaningful address.
    let source = if command == 0x0 {
        None
    } else {
        match family {
            // AF_INET, stream or datagram.
            0x11 | 0x12 => {
                if address_length < 12 {
                    return Err(ProxyError::Family);
                }
                let mut octets = [0u8; 4];
                octets.copy_from_slice(&buf[16..20]);
                Some(IpAddr::V4(Ipv4Addr::from(octets)))
            }
            // AF_INET6, stream or datagram.
            0x21 | 0x22 => {
                if address_length < 36 {
                    return Err(ProxyError::Family);
                }
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&buf[16..32]);
                Some(IpAddr::V6(Ipv6Addr::from(octets)))
            }
            // Unspecified and Unix families name no network peer; the
            // transport's own peer stands.
            0x00 | 0x31 | 0x32 => None,
            _ => return Err(ProxyError::Family),
        }
    };

    Ok(ProxyHeader::Complete { source, length })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_header(family: u8, command: u8, address: &[u8]) -> Vec<u8> {
        let mut header = SIGNATURE.to_vec();
        header.push(0x20 | command);
        header.push(family);
        header.extend_from_slice(&(address.len() as u16).to_be_bytes());
        header.extend_from_slice(address);
        header
    }

    fn ipv4_address(source: [u8; 4]) -> Vec<u8> {
        let mut address = source.to_vec();
        address.extend_from_slice(&[10, 0, 0, 1]); // destination
        address.extend_from_slice(&514u16.to_be_bytes());
        address.extend_from_slice(&6514u16.to_be_bytes());
        address
    }

    #[test]
    fn parses_ipv4_source() {
        let header = v2_header(0x11, 0x1, &ipv4_address([192, 0, 2, 7]));
        assert_eq!(
            Ok(ProxyHeader::Complete {
                source: Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))),
                length: header.len(),
            }),
            parse(&header, 1024)
        );
    }

    #[test]
    fn parses_ipv6_source() {
        let mut address = vec![0u8; 36];
        address[15] = 1;
        let header = v2_header(0x21, 0x1, &address);
        assert_eq!(
            Ok(ProxyHeader::Complete {
                source: Some(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1))),
                length: header.len(),
            }),
            parse(&header, 1024)
        );
    }

    #[test]
    fn ignores_trailing_payload_and_tlvs() {
        let mut address = ipv4_address([198, 51, 100, 3]);
        address.extend_from_slice(&[0x03, 0x00, 0x04, 1, 2, 3, 4]); // a TLV
        let header = v2_header(0x11, 0x1, &address);
        let prefix_len = header.len();

        let mut buf = header;
        buf.extend_from_slice(b"<13>a message\n");

        assert_eq!(
            Ok(ProxyHeader::Complete {
                source: Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 3))),
                length: prefix_len,
            }),
            parse(&buf, 1024)
        );
    }

    #[test]
    fn unix_and_unspecified_families_have_no_source() {
        for family in [0x00, 0x31, 0x32] {
            let header = v2_header(family, 0x1, &[0u8; 216]);
            assert_eq!(
                Ok(ProxyHeader::Complete {
                    source: None,
                    length: header.len(),
                }),
                parse(&header, 1024),
                "family {:#x}",
                family
            );
        }
    }

    #[test]
    fn local_command_has_no_source() {
        let header = v2_header(0x00, 0x0, &[]);
        assert_eq!(
            Ok(ProxyHeader::Complete {
                source: None,
                length: header.len(),
            }),
            parse(&header, 1024)
        );
    }

    #[test]
    fn rejects_v1_header() {
        assert_eq!(
            Err(ProxyError::Version),
            parse(b"PROXY TCP4 192.0.2.1 10.0.0.1 514 6514\r\n", 1024)
        );
        assert_eq!(Err(ProxyError::Version), parse(b"PRO", 1024));
    }

    #[test]
    fn rejects_missing_header() {
        assert_eq!(Err(ProxyError::Signature), parse(b"<13>a message\n", 1024));
    }

    #[test]
    fn rejects_bad_version_command_and_family() {
        let mut header = v2_header(0x11, 0x1, &ipv4_address([192, 0, 2, 7]));
        header[12] = 0x31;
        assert_eq!(Err(ProxyError::Version), parse(&header, 1024));

        let mut header = v2_header(0x11, 0x1, &ipv4_address([192, 0, 2, 7]));
        header[12] = 0x27;
        assert_eq!(Err(ProxyError::Command), parse(&header, 1024));

        let header = v2_header(0x41, 0x1, &ipv4_address([192, 0, 2, 7]));
        assert_eq!(Err(ProxyError::Family), parse(&header, 1024));

        let header = v2_header(0x11, 0x1, &[0u8; 4]);
        assert_eq!(Err(ProxyError::Family), parse(&header, 1024));
    }

    #[test]
    fn rejects_oversize_header() {
        let header = v2_header(0x11, 0x1, &vec![0u8; 600]);
        assert_eq!(Err(ProxyError::TooLong), parse(&header, 256));
    }

    #[test]
    fn parses_identically_at_every_split_point() {
        let mut buf = v2_header(0x11, 0x1, &ipv4_address([203, 0, 113, 9]));
        let prefix_len = buf.len();
        buf.extend_from_slice(b"7 hello!");

        let expected = ProxyHeader::Complete {
            source: Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))),
            length: prefix_len,
        };

        for split in 0..buf.len() {
            let result = parse(&buf[..split], 1024);
            if split < prefix_len {
                assert_eq!(Ok(ProxyHeader::Incomplete), result, "split {}", split);
            } else {
                assert_eq!(Ok(expected), result, "split {}", split);
            }
        }
    }

    #[test]
    fn detects_garbled_signature_as_soon_as_it_differs() {
        let mut buf = v2_header(0x11, 0x1, &ipv4_address([203, 0, 113, 9]));
        buf[5] = 0xff;

        for split in 6..buf.len() {
            assert_eq!(
                Err(ProxyError::Signature),
                parse(&buf[..split], 1024),
                "split {}",
                split
            );
        }
    }
}

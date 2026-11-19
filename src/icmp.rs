//! A tiny raw-ICMP transport.
//!
//! The game only needs two primitives: send an echo request with a payload,
//! and answer an echo request with an echo reply.  A raw socket is used
//! (rather than a Linux "ping socket") because both peers must be able to
//! receive the other side's requests.

use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::RawFd;

pub const ECHO_REPLY: u8 = 0;
pub const ECHO_REQUEST: u8 = 8;

const MAX_PACKET: usize = 2048;

#[derive(Debug, Clone)]
pub struct IcmpPacket {
    /// `ECHO_REQUEST` or `ECHO_REPLY`.
    pub kind: u8,
    pub id: u16,
    pub seq: u16,
    pub source: Ipv4Addr,
    pub ttl: u8,
    /// The protocol payload (everything after the 8-byte ICMP header).
    pub payload: Vec<u8>,
    /// The complete ICMP message, including its 8-byte header.
    pub raw: Vec<u8>,
}

pub struct IcmpSocket {
    fd: RawFd,
    id: u16,
}

impl IcmpSocket {
    /// Opens a raw ICMP socket. Requires `CAP_NET_RAW` on Linux.
    pub fn open() -> io::Result<IcmpSocket> {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let socket = IcmpSocket {
            fd,
            id: random_id(),
        };
        socket.set_nonblocking()?;
        Ok(socket)
    }

    pub fn id(&self) -> u16 {
        self.id
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(self.fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(self.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn send_request(&self, dest: Ipv4Addr, seq: u16, payload: &[u8]) -> io::Result<Vec<u8>> {
        self.send_echo(dest, ECHO_REQUEST, self.id, seq, payload)
    }

    pub fn send_reply(
        &self,
        dest: Ipv4Addr,
        id: u16,
        seq: u16,
        payload: &[u8],
    ) -> io::Result<Vec<u8>> {
        self.send_echo(dest, ECHO_REPLY, id, seq, payload)
    }

    fn send_echo(
        &self,
        dest: Ipv4Addr,
        kind: u8,
        id: u16,
        seq: u16,
        payload: &[u8],
    ) -> io::Result<Vec<u8>> {
        if payload.len() + 8 > MAX_PACKET {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ICMP payload is too large",
            ));
        }

        let mut packet = vec![0u8; 8 + payload.len()];
        packet[0] = kind;
        packet[1] = 0;
        packet[4..6].copy_from_slice(&id.to_be_bytes());
        packet[6..8].copy_from_slice(&seq.to_be_bytes());
        packet[8..].copy_from_slice(payload);

        let sum = checksum(&packet);
        packet[2..4].copy_from_slice(&sum.to_be_bytes());

        let addr = ipv4_to_sockaddr(dest);
        let sent = unsafe {
            libc::sendto(
                self.fd,
                packet.as_ptr().cast(),
                packet.len(),
                0,
                (&addr as *const libc::sockaddr_in).cast::<libc::sockaddr>(),
                mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if sent as usize != packet.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short ICMP write"));
        }

        Ok(packet)
    }

    /// Receives one relevant ICMP packet. Returns `None` when the socket has
    /// no more packets queued.
    pub fn recv(&self) -> io::Result<Option<IcmpPacket>> {
        loop {
            let mut buf = [0u8; MAX_PACKET];
            let mut addr: libc::sockaddr_in = unsafe { mem::zeroed() };
            let mut addr_len = mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;

            let received = unsafe {
                libc::recvfrom(
                    self.fd,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    0,
                    (&mut addr as *mut libc::sockaddr_in).cast::<libc::sockaddr>(),
                    &mut addr_len,
                )
            };

            if received < 0 {
                let err = io::Error::last_os_error();
                return match err.kind() {
                    io::ErrorKind::WouldBlock => Ok(None),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(err),
                };
            }

            let received = received as usize;
            // Raw IPv4 sockets deliver the IP header in front of the packet.
            if received < 20 {
                continue;
            }
            let ihl = usize::from(buf[0] & 0x0f) * 4;
            if ihl < 20 || received < ihl + 8 {
                continue;
            }

            let icmp = &buf[ihl..received];
            let kind = icmp[0];
            if kind != ECHO_REQUEST && kind != ECHO_REPLY {
                continue;
            }
            // The kernel does not validate ICMP checksums for raw sockets.
            if checksum(icmp) != 0 {
                continue;
            }

            let ttl = buf[8];
            let id = u16::from_be_bytes([icmp[4], icmp[5]]);
            let seq = u16::from_be_bytes([icmp[6], icmp[7]]);

            return Ok(Some(IcmpPacket {
                kind,
                id,
                seq,
                source: sockaddr_to_ipv4(&addr),
                ttl,
                payload: icmp[8..].to_vec(),
                raw: icmp.to_vec(),
            }));
        }
    }
}

impl Drop for IcmpSocket {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn ipv4_to_sockaddr(ip: Ipv4Addr) -> libc::sockaddr_in {
    let mut addr: libc::sockaddr_in = unsafe { mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = 0;
    addr.sin_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(ip.octets()),
    };
    addr
}

fn sockaddr_to_ipv4(addr: &libc::sockaddr_in) -> Ipv4Addr {
    Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes())
}

/// Internet checksum (RFC 1071), used by ICMP.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let (chunks, remainder) = data.as_chunks::<2>();
    for chunk in chunks {
        sum += u32::from(u16::from_be_bytes(*chunk));
    }
    if let Some(&byte) = remainder.first() {
        sum += u32::from(byte) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

pub fn random_u32() -> u32 {
    use std::io::Read;

    let mut bytes = [0u8; 4];
    if let Ok(mut urandom) = std::fs::File::open("/dev/urandom")
        && urandom.read_exact(&mut bytes).is_ok()
    {
        return u32::from_ne_bytes(bytes);
    }

    // Extremely unlikely fallback (no /dev/urandom).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = now ^ (u64::from(std::process::id()) << 32) ^ 0x9e37_79b9_7f4a_7c15;
    (mixed ^ (mixed >> 32)) as u32
}

fn random_id() -> u16 {
    let mut id = random_u32() as u16;
    if id == 0 {
        id = 0x5043;
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_valid_icmp_is_zero() {
        let mut packet = vec![8, 0, 0, 0, 0x12, 0x34, 0x00, 0x01];
        packet.extend_from_slice(b"pingchess");
        let sum = checksum(&packet);
        packet[2..4].copy_from_slice(&sum.to_be_bytes());
        assert_eq!(checksum(&packet), 0);
    }

    #[test]
    fn checksum_handles_odd_lengths() {
        let mut packet = vec![8, 0, 0, 0, 0x12, 0x34, 0x00, 0x01];
        packet.extend_from_slice(b"hello");
        assert_eq!(packet.len() % 2, 1);
        let sum = checksum(&packet);
        packet[2..4].copy_from_slice(&sum.to_be_bytes());
        assert_eq!(checksum(&packet), 0);
    }

    #[test]
    fn sockaddr_roundtrip() {
        let ip = Ipv4Addr::new(127, 0, 0, 1);
        let addr = ipv4_to_sockaddr(ip);
        assert_eq!(sockaddr_to_ipv4(&addr), ip);
    }
}

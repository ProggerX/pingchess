//! The pingchess wire protocol.
//!
//! Every chess message travels inside the payload of an ICMP echo
//! request/reply.  The format is deliberately small and boring so that it is
//! easy to inspect with tcpdump:
//!
//! ```text
//! 0        4      5      6      8      10            14
//! +--------+------+------+------+------+-------------+
//! | "PCHS" | ver  | kind | sess | seq  | sender (32) |
//! +--------+------+------+------+------+-------------+
//! | body ...                                         |
//! +--------------------------------------------------+
//! ```

use std::error;
use std::fmt;

use shakmaty::Color;

pub const MAGIC: [u8; 4] = *b"PCHS";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 14;
pub const MAX_DATAGRAM: usize = 1024;

const MAX_NAME: usize = 24;
const MAX_REASON: usize = 72;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    HelloAck = 2,
    Move = 3,
    Resign = 4,
    Ack = 5,
    Nack = 6,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Hello => "hello",
            Kind::HelloAck => "hello-ack",
            Kind::Move => "move",
            Kind::Resign => "resign",
            Kind::Ack => "ack",
            Kind::Nack => "nack",
        }
    }

    fn from_u8(value: u8) -> Result<Kind, ProtoError> {
        Ok(match value {
            1 => Kind::Hello,
            2 => Kind::HelloAck,
            3 => Kind::Move,
            4 => Kind::Resign,
            5 => Kind::Ack,
            6 => Kind::Nack,
            other => return Err(ProtoError::UnknownKind(other)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Hello {
        name: String,
    },
    HelloAck {
        name: String,
        /// Color of the sender of this message (the host).
        color: Color,
    },
    Move {
        /// Half-move index before this move was played.
        ply: u16,
        /// The move in UCI notation, e.g. `e2e4` or `e7e8q`.
        uci: String,
        /// Hash of the position after the move.
        hash: u64,
    },
    Resign {
        reason: String,
    },
    Ack {
        acked: u16,
    },
    Nack {
        acked: u16,
        reason: String,
    },
}

impl Body {
    pub fn kind(&self) -> Kind {
        match self {
            Body::Hello { .. } => Kind::Hello,
            Body::HelloAck { .. } => Kind::HelloAck,
            Body::Move { .. } => Kind::Move,
            Body::Resign { .. } => Kind::Resign,
            Body::Ack { .. } => Kind::Ack,
            Body::Nack { .. } => Kind::Nack,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: Kind,
    pub session: u16,
    pub seq: u16,
    pub sender: u32,
    pub body: Body,
}

impl Message {
    pub fn new(session: u16, seq: u16, sender: u32, body: Body) -> Message {
        Message {
            kind: body.kind(),
            session,
            seq,
            sender,
            body,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + 32);
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(self.kind as u8);
        out.extend_from_slice(&self.session.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.sender.to_be_bytes());

        match &self.body {
            Body::Hello { name } => put_str(&mut out, name, MAX_NAME),
            Body::HelloAck { name, color } => {
                out.push(color_byte(*color));
                put_str(&mut out, name, MAX_NAME);
            }
            Body::Move { ply, uci, hash } => {
                out.extend_from_slice(&ply.to_be_bytes());
                put_str(&mut out, uci, 5);
                out.extend_from_slice(&hash.to_be_bytes());
            }
            Body::Resign { reason } => put_str(&mut out, reason, MAX_REASON),
            Body::Ack { acked } => out.extend_from_slice(&acked.to_be_bytes()),
            Body::Nack { acked, reason } => {
                out.extend_from_slice(&acked.to_be_bytes());
                put_str(&mut out, reason, MAX_REASON);
            }
        }

        out
    }

    pub fn decode(buf: &[u8]) -> Result<Message, ProtoError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtoError::TooShort);
        }
        if buf[0..4] != MAGIC {
            return Err(ProtoError::BadMagic);
        }
        if buf[4] != VERSION {
            return Err(ProtoError::BadVersion(buf[4]));
        }

        let kind = Kind::from_u8(buf[5])?;
        let session = u16::from_be_bytes([buf[6], buf[7]]);
        let seq = u16::from_be_bytes([buf[8], buf[9]]);
        let sender = u32::from_be_bytes([buf[10], buf[11], buf[12], buf[13]]);

        let mut reader = Reader::new(&buf[HEADER_LEN..]);
        let body = match kind {
            Kind::Hello => Body::Hello {
                name: reader.string(MAX_NAME)?,
            },
            Kind::HelloAck => {
                let color = color_from_byte(reader.u8()?)?;
                Body::HelloAck {
                    name: reader.string(MAX_NAME)?,
                    color,
                }
            }
            Kind::Move => {
                let ply = reader.u16()?;
                let uci = reader.string(5)?;
                let hash = reader.u64()?;
                Body::Move { ply, uci, hash }
            }
            Kind::Resign => Body::Resign {
                reason: reader.string(MAX_REASON)?,
            },
            Kind::Ack => Body::Ack {
                acked: reader.u16()?,
            },
            Kind::Nack => {
                let acked = reader.u16()?;
                let reason = reader.string(MAX_REASON)?;
                Body::Nack { acked, reason }
            }
        };

        if reader.remaining() != 0 {
            return Err(ProtoError::TrailingBytes(reader.remaining()));
        }

        Ok(Message {
            kind,
            session,
            seq,
            sender,
            body,
        })
    }
}

fn color_byte(color: Color) -> u8 {
    match color {
        Color::Black => 0,
        Color::White => 1,
    }
}

fn color_from_byte(byte: u8) -> Result<Color, ProtoError> {
    match byte {
        0 => Ok(Color::Black),
        1 => Ok(Color::White),
        other => Err(ProtoError::BadColor(other)),
    }
}

fn truncate_utf8(s: &str, max: usize) -> &[u8] {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s.as_bytes()[..end]
}

fn put_str(out: &mut Vec<u8>, s: &str, max: usize) {
    let bytes = truncate_utf8(s, max);
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    TooShort,
    BadMagic,
    BadVersion(u8),
    UnknownKind(u8),
    BadColor(u8),
    Truncated,
    FieldTooLong,
    BadUtf8,
    TrailingBytes(usize),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::TooShort => f.write_str("packet is too short"),
            ProtoError::BadMagic => f.write_str("wrong magic bytes"),
            ProtoError::BadVersion(version) => {
                write!(f, "unsupported protocol version {version}")
            }
            ProtoError::UnknownKind(kind) => write!(f, "unknown message kind {kind}"),
            ProtoError::BadColor(color) => write!(f, "invalid color byte {color}"),
            ProtoError::Truncated => f.write_str("truncated message body"),
            ProtoError::FieldTooLong => f.write_str("string field is too long"),
            ProtoError::BadUtf8 => f.write_str("string field is not UTF-8"),
            ProtoError::TrailingBytes(n) => write!(f, "{n} trailing bytes in message"),
        }
    }
}

impl error::Error for ProtoError {}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ProtoError> {
        if self.remaining() < len {
            return Err(ProtoError::Truncated);
        }
        let bytes = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtoError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u64(&mut self) -> Result<u64, ProtoError> {
        let bytes = self.take(8)?;
        let mut out = [0u8; 8];
        out.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(out))
    }

    fn string(&mut self, max: usize) -> Result<String, ProtoError> {
        let len = self.u8()? as usize;
        if len > max {
            return Err(ProtoError::FieldTooLong);
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtoError::BadUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(body: Body) {
        let message = Message::new(0xbeef, 42, 0xdead_beef, body);
        let bytes = message.encode();
        assert!(bytes.len() <= MAX_DATAGRAM);
        let decoded = Message::decode(&bytes).expect("decode");
        assert_eq!(decoded, message);
    }

    #[test]
    fn roundtrips_every_message_kind() {
        roundtrip(Body::Hello {
            name: "alice".into(),
        });
        roundtrip(Body::HelloAck {
            name: "bob".into(),
            color: Color::Black,
        });
        roundtrip(Body::Move {
            ply: 12,
            uci: "e7e8q".into(),
            hash: 0x0123_4567_89ab_cdef,
        });
        roundtrip(Body::Resign {
            reason: "good game".into(),
        });
        roundtrip(Body::Ack { acked: 7 });
        roundtrip(Body::Nack {
            acked: 9,
            reason: "not your turn".into(),
        });
    }

    #[test]
    fn rejects_a_foreign_packet() {
        assert_eq!(Message::decode(b"hello"), Err(ProtoError::TooShort));
        let mut bytes = Message::new(0, 1, 2, Body::Ack { acked: 1 }).encode();
        bytes[0] = b'X';
        assert_eq!(Message::decode(&bytes), Err(ProtoError::BadMagic));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = Message::new(0, 1, 2, Body::Ack { acked: 1 }).encode();
        bytes.push(0);
        assert_eq!(Message::decode(&bytes), Err(ProtoError::TrailingBytes(1)));
    }

    #[test]
    fn truncates_long_names_on_encode() {
        let name = "x".repeat(100);
        let bytes = Message::new(0, 1, 2, Body::Hello { name: name.clone() }).encode();
        let decoded = Message::decode(&bytes).unwrap();
        match decoded.body {
            Body::Hello { name } => assert_eq!(name.len(), MAX_NAME),
            other => panic!("unexpected body: {other:?}"),
        }
    }

    #[test]
    fn keeps_utf8_boundaries() {
        let name = "abcdefghijklmnopqrstuvwé";
        let bytes = Message::new(0, 1, 2, Body::Hello { name: name.into() }).encode();
        assert!(Message::decode(&bytes).is_ok());
    }
}

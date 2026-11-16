//! Protocol for pingchess rooms.
//!
//! The peer-to-peer protocol in [`crate::proto`] is stop-and-wait between two
//! players.  This protocol adds a hub: many clients talk to one server, which
//! owns the rooms and the games.  A client sends a message in an ICMP echo
//! request; the hub answers with an echo reply containing an acknowledgement
//! and a batch of events.
//!
//! ```text
//! 0        4      5      6      8                    12
//! +--------+------+------+------+--------------------+
//! | "PCHR" | ver  | kind | seq  | sender (32)        |
//! +--------+------+------+------+--------------------+
//! | body ...                                         |
//! +--------------------------------------------------+
//! ```

use std::error;
use std::fmt;

use shakmaty::Color;

pub const MAGIC: [u8; 4] = *b"PCHR";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 12;

/// Soft cap for a reply's encoded events. Keeps ICMP packets comfortably small.
pub const MAX_EVENT_BYTES: usize = 900;
pub const MAX_TEXT: usize = 200;
pub const MAX_NICK: usize = 20;
pub const MAX_ROOM: usize = 24;
pub const MAX_HUB: usize = 32;
pub const MAX_ERROR: usize = 120;
pub const MAX_SYSTEM: usize = 220;
pub const MAX_STATUS: usize = 80;
pub const MAX_FEN: usize = 100;
pub const MAX_MOVE: usize = 16;
pub const MAX_MOVETEXT: usize = 320;
pub const MAX_RESULT: usize = 8;
pub const MAX_ROOMS: usize = 12;
pub const MAX_MEMBERS: usize = 16;
pub const MAX_ROOM_STATUS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    Msg = 2,
    Welcome = 3,
    Reply = 4,
}

impl Kind {
    fn from_u8(value: u8) -> Result<Kind, ProtoError> {
        Ok(match value {
            1 => Kind::Hello,
            2 => Kind::Msg,
            3 => Kind::Welcome,
            4 => Kind::Reply,
            other => return Err(ProtoError::UnknownKind(other)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: Kind,
    pub seq: u16,
    pub sender: u32,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Hello {
        nick: String,
    },
    Msg {
        last_event: u32,
        action: Action,
    },
    Welcome {
        hub: String,
        nick: String,
        error: String,
    },
    Reply {
        events: Vec<Event>,
    },
}

impl Body {
    fn kind(&self) -> Kind {
        match self {
            Body::Hello { .. } => Kind::Hello,
            Body::Msg { .. } => Kind::Msg,
            Body::Welcome { .. } => Kind::Welcome,
            Body::Reply { .. } => Kind::Reply,
        }
    }
}

impl Message {
    pub fn new(seq: u16, sender: u32, body: Body) -> Message {
        Message {
            kind: body.kind(),
            seq,
            sender,
            body,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + 64);
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(self.kind as u8);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.sender.to_be_bytes());

        match &self.body {
            Body::Hello { nick } => put_str(&mut out, nick, MAX_NICK),
            Body::Msg { last_event, action } => {
                out.extend_from_slice(&last_event.to_be_bytes());
                action.encode(&mut out);
            }
            Body::Welcome { hub, nick, error } => {
                put_str(&mut out, hub, MAX_HUB);
                put_str(&mut out, nick, MAX_NICK);
                put_str(&mut out, error, MAX_ERROR);
            }
            Body::Reply { events } => {
                out.push(events.len() as u8);
                for event in events {
                    out.extend_from_slice(&event.encode());
                }
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
        let seq = u16::from_be_bytes([buf[6], buf[7]]);
        let sender = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let mut reader = Reader::new(&buf[HEADER_LEN..]);

        let body = match kind {
            Kind::Hello => Body::Hello {
                nick: reader.string(MAX_NICK)?,
            },
            Kind::Msg => {
                let last_event = reader.u32()?;
                let action = Action::decode(&mut reader)?;
                Body::Msg { last_event, action }
            }
            Kind::Welcome => Body::Welcome {
                hub: reader.string(MAX_HUB)?,
                nick: reader.string(MAX_NICK)?,
                error: reader.string(MAX_ERROR)?,
            },
            Kind::Reply => {
                let count = reader.u8()? as usize;
                if count > 64 {
                    return Err(ProtoError::TooManyItems);
                }
                let mut events = Vec::with_capacity(count);
                for _ in 0..count {
                    events.push(Event::decode(&mut reader)?);
                }
                Body::Reply { events }
            }
        };

        if reader.remaining() != 0 {
            return Err(ProtoError::TrailingBytes(reader.remaining()));
        }

        Ok(Message {
            kind,
            seq,
            sender,
            body,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Poll,
    Join { room: String },
    Part,
    Chat { text: String },
    Emote { text: String },
    Nick { nick: String },
    Move { text: String },
    Resign,
    New { opponent: String },
    List,
    Who,
    Quit,
}

impl Action {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Action::Poll => out.push(0),
            Action::Join { room } => {
                out.push(1);
                put_str(out, room, MAX_ROOM);
            }
            Action::Part => out.push(2),
            Action::Chat { text } => {
                out.push(3);
                put_str(out, text, MAX_TEXT);
            }
            Action::Emote { text } => {
                out.push(4);
                put_str(out, text, MAX_TEXT);
            }
            Action::Nick { nick } => {
                out.push(5);
                put_str(out, nick, MAX_NICK);
            }
            Action::Move { text } => {
                out.push(6);
                put_str(out, text, MAX_MOVE);
            }
            Action::Resign => out.push(7),
            Action::New { opponent } => {
                out.push(8);
                put_str(out, opponent, MAX_NICK);
            }
            Action::List => out.push(9),
            Action::Who => out.push(10),
            Action::Quit => out.push(11),
        }
    }

    fn decode(reader: &mut Reader<'_>) -> Result<Action, ProtoError> {
        Ok(match reader.u8()? {
            0 => Action::Poll,
            1 => Action::Join {
                room: reader.string(MAX_ROOM)?,
            },
            2 => Action::Part,
            3 => Action::Chat {
                text: reader.string(MAX_TEXT)?,
            },
            4 => Action::Emote {
                text: reader.string(MAX_TEXT)?,
            },
            5 => Action::Nick {
                nick: reader.string(MAX_NICK)?,
            },
            6 => Action::Move {
                text: reader.string(MAX_MOVE)?,
            },
            7 => Action::Resign,
            8 => Action::New {
                opponent: reader.string(MAX_NICK)?,
            },
            9 => Action::List,
            10 => Action::Who,
            11 => Action::Quit,
            other => return Err(ProtoError::UnknownAction(other)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: u32,
    pub body: EventBody,
}

impl Event {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(&self.id.to_be_bytes());
        self.body.encode(&mut out);
        out
    }

    fn decode(reader: &mut Reader<'_>) -> Result<Event, ProtoError> {
        let id = reader.u32()?;
        let body = EventBody::decode(reader)?;
        Ok(Event { id, body })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Member = 0,
    White = 1,
    Black = 2,
    Spectator = 3,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Member => "member",
            Role::White => "white",
            Role::Black => "black",
            Role::Spectator => "spectator",
        }
    }

    fn from_u8(value: u8) -> Result<Role, ProtoError> {
        Ok(match value {
            0 => Role::Member,
            1 => Role::White,
            2 => Role::Black,
            3 => Role::Spectator,
            other => return Err(ProtoError::UnknownRole(other)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub nick: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSummary {
    pub name: String,
    pub members: u8,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBody {
    System {
        text: String,
    },
    Chat {
        from: String,
        text: String,
    },
    Emote {
        from: String,
        text: String,
    },
    RoomInfo {
        room: String,
        total: u8,
        members: Vec<Member>,
    },
    Board {
        white: String,
        black: String,
        /// `None`, white, or black.
        turn: Option<Color>,
        status: String,
        fen: String,
        last_uci: String,
        movetext: String,
        result: String,
    },
    RoomList {
        total: u8,
        rooms: Vec<RoomSummary>,
    },
    Error {
        text: String,
    },
    /// The hub changed the client's own nick.
    SelfNick {
        nick: String,
    },
}

impl EventBody {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            EventBody::System { text } => {
                out.push(1);
                put_str(out, text, MAX_SYSTEM);
            }
            EventBody::Chat { from, text } => {
                out.push(2);
                put_str(out, from, MAX_NICK);
                put_str(out, text, MAX_TEXT);
            }
            EventBody::Emote { from, text } => {
                out.push(3);
                put_str(out, from, MAX_NICK);
                put_str(out, text, MAX_TEXT);
            }
            EventBody::RoomInfo {
                room,
                total,
                members,
            } => {
                out.push(4);
                put_str(out, room, MAX_ROOM);
                out.push(*total);
                out.push(members.len() as u8);
                for member in members {
                    put_str(out, &member.nick, MAX_NICK);
                    out.push(member.role as u8);
                }
            }
            EventBody::Board {
                white,
                black,
                turn,
                status,
                fen,
                last_uci,
                movetext,
                result,
            } => {
                out.push(5);
                put_str(out, white, MAX_NICK);
                put_str(out, black, MAX_NICK);
                out.push(match turn {
                    None => 0,
                    Some(Color::White) => 1,
                    Some(Color::Black) => 2,
                });
                put_str(out, status, MAX_STATUS);
                put_str(out, fen, MAX_FEN);
                put_str(out, last_uci, MAX_MOVE);
                put_str(out, movetext, MAX_MOVETEXT);
                put_str(out, result, MAX_RESULT);
            }
            EventBody::RoomList { total, rooms } => {
                out.push(6);
                out.push(*total);
                out.push(rooms.len() as u8);
                for room in rooms {
                    put_str(out, &room.name, MAX_ROOM);
                    out.push(room.members);
                    put_str(out, &room.status, MAX_ROOM_STATUS);
                }
            }
            EventBody::Error { text } => {
                out.push(7);
                put_str(out, text, MAX_ERROR);
            }
            EventBody::SelfNick { nick } => {
                out.push(8);
                put_str(out, nick, MAX_NICK);
            }
        }
    }

    fn decode(reader: &mut Reader<'_>) -> Result<EventBody, ProtoError> {
        Ok(match reader.u8()? {
            1 => EventBody::System {
                text: reader.string(MAX_SYSTEM)?,
            },
            2 => EventBody::Chat {
                from: reader.string(MAX_NICK)?,
                text: reader.string(MAX_TEXT)?,
            },
            3 => EventBody::Emote {
                from: reader.string(MAX_NICK)?,
                text: reader.string(MAX_TEXT)?,
            },
            4 => {
                let room = reader.string(MAX_ROOM)?;
                let total = reader.u8()?;
                let count = reader.u8()? as usize;
                if count > MAX_MEMBERS {
                    return Err(ProtoError::TooManyItems);
                }
                let mut members = Vec::with_capacity(count);
                for _ in 0..count {
                    members.push(Member {
                        nick: reader.string(MAX_NICK)?,
                        role: Role::from_u8(reader.u8()?)?,
                    });
                }
                EventBody::RoomInfo {
                    room,
                    total,
                    members,
                }
            }
            5 => {
                let white = reader.string(MAX_NICK)?;
                let black = reader.string(MAX_NICK)?;
                let turn = match reader.u8()? {
                    0 => None,
                    1 => Some(Color::White),
                    2 => Some(Color::Black),
                    other => return Err(ProtoError::UnknownColor(other)),
                };
                let status = reader.string(MAX_STATUS)?;
                let fen = reader.string(MAX_FEN)?;
                let last_uci = reader.string(MAX_MOVE)?;
                let movetext = reader.string(MAX_MOVETEXT)?;
                let result = reader.string(MAX_RESULT)?;
                EventBody::Board {
                    white,
                    black,
                    turn,
                    status,
                    fen,
                    last_uci,
                    movetext,
                    result,
                }
            }
            6 => {
                let total = reader.u8()?;
                let count = reader.u8()? as usize;
                if count > MAX_ROOMS {
                    return Err(ProtoError::TooManyItems);
                }
                let mut rooms = Vec::with_capacity(count);
                for _ in 0..count {
                    rooms.push(RoomSummary {
                        name: reader.string(MAX_ROOM)?,
                        members: reader.u8()?,
                        status: reader.string(MAX_ROOM_STATUS)?,
                    });
                }
                EventBody::RoomList { total, rooms }
            }
            7 => EventBody::Error {
                text: reader.string(MAX_ERROR)?,
            },
            8 => EventBody::SelfNick {
                nick: reader.string(MAX_NICK)?,
            },
            other => return Err(ProtoError::UnknownEvent(other)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    TooShort,
    BadMagic,
    BadVersion(u8),
    UnknownKind(u8),
    UnknownAction(u8),
    UnknownEvent(u8),
    UnknownRole(u8),
    UnknownColor(u8),
    Truncated,
    FieldTooLong,
    BadUtf8,
    TooManyItems,
    TrailingBytes(usize),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::TooShort => f.write_str("packet is too short"),
            ProtoError::BadMagic => f.write_str("wrong magic bytes"),
            ProtoError::BadVersion(version) => {
                write!(f, "unsupported room protocol version {version}")
            }
            ProtoError::UnknownKind(kind) => write!(f, "unknown message kind {kind}"),
            ProtoError::UnknownAction(action) => write!(f, "unknown action {action}"),
            ProtoError::UnknownEvent(event) => write!(f, "unknown event {event}"),
            ProtoError::UnknownRole(role) => write!(f, "unknown member role {role}"),
            ProtoError::UnknownColor(color) => write!(f, "unknown color {color}"),
            ProtoError::Truncated => f.write_str("truncated message body"),
            ProtoError::FieldTooLong => f.write_str("string field is too long"),
            ProtoError::BadUtf8 => f.write_str("string field is not UTF-8"),
            ProtoError::TooManyItems => f.write_str("too many items in message"),
            ProtoError::TrailingBytes(n) => write!(f, "{n} trailing bytes in message"),
        }
    }
}

impl error::Error for ProtoError {}

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

    fn u32(&mut self) -> Result<u32, ProtoError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
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

    fn roundtrip(message: Message) {
        let bytes = message.encode();
        assert_eq!(Message::decode(&bytes).unwrap(), message);
    }

    #[test]
    fn roundtrips_client_messages() {
        for action in [
            Action::Poll,
            Action::Join {
                room: "#lobby".into(),
            },
            Action::Part,
            Action::Chat {
                text: "hello".into(),
            },
            Action::Emote {
                text: "waves".into(),
            },
            Action::Nick {
                nick: "alice".into(),
            },
            Action::Move { text: "Nf3".into() },
            Action::Resign,
            Action::New {
                opponent: "bob".into(),
            },
            Action::List,
            Action::Who,
            Action::Quit,
        ] {
            roundtrip(Message::new(
                7,
                0xdead_beef,
                Body::Msg {
                    last_event: 42,
                    action,
                },
            ));
        }

        roundtrip(Message::new(
            1,
            7,
            Body::Hello {
                nick: "alice".into(),
            },
        ));
    }

    #[test]
    fn roundtrips_hub_messages() {
        roundtrip(Message::new(
            3,
            9,
            Body::Welcome {
                hub: "pingchess hub".into(),
                nick: "alice".into(),
                error: String::new(),
            },
        ));

        let events = vec![
            Event {
                id: 1,
                body: EventBody::System {
                    text: "welcome".into(),
                },
            },
            Event {
                id: 2,
                body: EventBody::Chat {
                    from: "alice".into(),
                    text: "hi".into(),
                },
            },
            Event {
                id: 3,
                body: EventBody::Emote {
                    from: "bob".into(),
                    text: "waves".into(),
                },
            },
            Event {
                id: 4,
                body: EventBody::RoomInfo {
                    room: "#lobby".into(),
                    total: 2,
                    members: vec![
                        Member {
                            nick: "alice".into(),
                            role: Role::White,
                        },
                        Member {
                            nick: "bob".into(),
                            role: Role::Black,
                        },
                    ],
                },
            },
            Event {
                id: 5,
                body: EventBody::Board {
                    white: "alice".into(),
                    black: "bob".into(),
                    turn: Some(Color::White),
                    status: "in progress".into(),
                    fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".into(),
                    last_uci: "e2e4".into(),
                    movetext: "1. e4".into(),
                    result: "*".into(),
                },
            },
            Event {
                id: 6,
                body: EventBody::RoomList {
                    total: 2,
                    rooms: vec![
                        RoomSummary {
                            name: "#lobby".into(),
                            members: 2,
                            status: "in progress".into(),
                        },
                        RoomSummary {
                            name: "#chess".into(),
                            members: 0,
                            status: "no game".into(),
                        },
                    ],
                },
            },
            Event {
                id: 7,
                body: EventBody::Error {
                    text: "nope".into(),
                },
            },
            Event {
                id: 8,
                body: EventBody::SelfNick {
                    nick: "alice2".into(),
                },
            },
        ];

        roundtrip(Message::new(9, 0x1234_5678, Body::Reply { events }));
    }

    #[test]
    fn rejects_foreign_messages() {
        assert_eq!(Message::decode(b"hello"), Err(ProtoError::TooShort));
        let mut bytes = Message::new(1, 2, Body::Hello { nick: "x".into() }).encode();
        bytes[0] = b'X';
        assert_eq!(Message::decode(&bytes), Err(ProtoError::BadMagic));
    }

    #[test]
    fn truncates_long_strings() {
        let message = Message::new(
            1,
            2,
            Body::Hello {
                nick: "x".repeat(100),
            },
        );
        let decoded = Message::decode(&message.encode()).unwrap();
        match decoded.body {
            Body::Hello { nick } => assert_eq!(nick.len(), MAX_NICK),
            other => panic!("unexpected body: {other:?}"),
        }
    }

    #[test]
    fn keeps_utf8_boundaries() {
        let text = "abcdefghijklmnopqrstuvwé";
        let message = Message::new(1, 2, Body::Hello { nick: text.into() });
        assert!(Message::decode(&message.encode()).is_ok());
    }
}

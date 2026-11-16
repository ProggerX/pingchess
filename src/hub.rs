//! An IRC-like ICMP hub: rooms, nicknames, chat, and games started on demand.
//!
//! Clients send every request as an ICMP echo request.  The hub answers in the
//! echo reply, both acknowledging the request and piggy-backing a batch of
//! room events.  There is no connection: clients poll periodically and the hub
//! keeps a per-client event queue until the client says it has seen it.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use shakmaty::Color;

use crate::game::{Game, GameResult};
use crate::hub_proto::{
    Action, Body, Event, EventBody, MAX_EVENT_BYTES, MAX_MEMBERS, MAX_MOVETEXT, MAX_NICK,
    MAX_ROOMS, MAX_TEXT, Member, Message, Role, RoomSummary,
};
use crate::icmp::{self, IcmpPacket, IcmpSocket};
use crate::ui;

const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const MAX_QUEUED_EVENTS: usize = 256;

pub struct Config {
    pub hub_name: String,
    pub motd: String,
    pub trace: bool,
}

#[derive(Debug)]
pub struct Error(String);

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error(err.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;
type ClientKey = (Ipv4Addr, u32);

pub fn run(config: Config) -> Result<()> {
    let socket = match IcmpSocket::open() {
        Ok(socket) => socket,
        Err(err) if err.raw_os_error() == Some(libc::EPERM) => {
            return Err(Error::new(
                "opening a raw ICMP socket failed: operation not permitted\n\
                 hint: raw ICMP needs CAP_NET_RAW. Run as root, or grant the binary:\n\
                 hint:   sudo setcap cap_net_raw+ep ./target/release/pingchess\n\
                 hint: for a local demo: unshare -Urn sh -c 'ip link set lo up; ...'",
            ));
        }
        Err(err) => return Err(err.into()),
    };

    let mut hub = Hub {
        socket,
        cfg: config,
        clients: HashMap::new(),
        rooms: HashMap::new(),
        stdin_open: true,
        input: Vec::new(),
        exit: false,
        last_cleanup: Instant::now(),
    };
    hub.rooms
        .insert("#lobby".into(), Room::new("#lobby".into()));
    hub.main_loop()
}

struct Client {
    nick: String,
    room: Option<String>,
    last_seen: Instant,
    last_seq: u16,
    events: VecDeque<Event>,
    next_event: u32,
}

impl Client {
    fn new(nick: String) -> Client {
        Client {
            nick,
            room: None,
            last_seen: Instant::now(),
            last_seq: 0,
            events: VecDeque::new(),
            next_event: 1,
        }
    }

    fn enqueue(&mut self, body: EventBody) {
        let id = self.next_event;
        self.next_event = self.next_event.saturating_add(1);
        self.events.push_back(Event { id, body });
        if self.events.len() > MAX_QUEUED_EVENTS {
            self.events.pop_front();
        }
    }
}

struct Room {
    name: String,
    members: Vec<ClientKey>,
    game: Option<RoomGame>,
}

impl Room {
    fn new(name: String) -> Room {
        Room {
            name,
            members: Vec::new(),
            game: None,
        }
    }

    fn is_playing(&self) -> bool {
        self.game.as_ref().is_some_and(|game| !game.game.is_over())
    }
}

struct RoomGame {
    game: Game,
    white: ClientKey,
    black: ClientKey,
    last_uci: Option<String>,
}

struct Hub {
    socket: IcmpSocket,
    cfg: Config,
    clients: HashMap<ClientKey, Client>,
    rooms: HashMap<String, Room>,
    stdin_open: bool,
    input: Vec<u8>,
    exit: bool,
    last_cleanup: Instant,
}

impl Hub {
    fn main_loop(&mut self) -> Result<()> {
        self.print_intro();

        while !self.exit {
            let (socket_ready, stdin_ready) = self.wait(Duration::from_millis(250))?;
            if stdin_ready {
                self.read_stdin()?;
            }
            if socket_ready {
                self.drain_socket()?;
            }
            self.cleanup_clients();
        }

        println!("[hub] shutting down");
        Ok(())
    }

    fn print_intro(&self) {
        println!(
            "pingchess rooms hub {} - IRC-like chess over ICMP",
            env!("CARGO_PKG_VERSION")
        );
        println!("hub name: {}", self.cfg.hub_name);
        println!("listening for clients on raw ICMP");
        println!("console commands: rooms, clients, motd, help, quit");
    }

    fn drain_socket(&mut self) -> Result<()> {
        while let Some(pkt) = self.socket.recv()? {
            if pkt.kind != icmp::ECHO_REQUEST {
                continue;
            }
            let message = match Message::decode(&pkt.payload) {
                Ok(message) => message,
                Err(_) => continue,
            };
            self.trace_rx(&pkt, &message);

            match message.body {
                Body::Hello { nick } => {
                    self.on_hello(pkt, message.seq, message.sender, &nick)?;
                }
                Body::Msg { last_event, action } => {
                    self.on_msg(pkt, message.seq, message.sender, last_event, action)?;
                }
                Body::Welcome { .. } | Body::Reply { .. } => {}
            }
        }
        Ok(())
    }

    fn on_hello(
        &mut self,
        pkt: IcmpPacket,
        seq: u16,
        sender: u32,
        requested_nick: &str,
    ) -> Result<()> {
        let key = (pkt.source, sender);

        if let Some(client) = self.clients.get_mut(&key) {
            client.last_seen = Instant::now();
            let nick = client.nick.clone();
            return self.send_welcome(&pkt, seq, sender, &nick);
        }

        let requested = ui::sanitize(requested_nick).trim().to_string();
        let base = clean_nick(&requested);
        let nick = self.unique_nick(&base, None);

        let mut client = Client::new(nick.clone());
        if !requested.eq_ignore_ascii_case(&nick) {
            client.enqueue(EventBody::System {
                text: format!("You are known as {nick}."),
            });
        }
        client.enqueue(EventBody::System {
            text: self.cfg.motd.clone(),
        });
        client.enqueue(EventBody::System {
            text: "Rooms: /join #lobby | /list | /who | /new <nick> | /help".into(),
        });
        client.enqueue(EventBody::SelfNick { nick: nick.clone() });

        self.clients.insert(key, client);
        self.send_welcome(&pkt, seq, sender, &nick)?;
        println!("[hub] {nick} connected from {}", pkt.source);
        Ok(())
    }

    fn send_welcome(
        &mut self,
        pkt: &IcmpPacket,
        hello_seq: u16,
        sender: u32,
        nick: &str,
    ) -> Result<()> {
        let message = Message::new(
            hello_seq,
            sender,
            Body::Welcome {
                hub: self.cfg.hub_name.clone(),
                nick: nick.to_string(),
                error: String::new(),
            },
        );
        self.send_message(pkt, &message)
    }

    fn on_msg(
        &mut self,
        pkt: IcmpPacket,
        seq: u16,
        sender: u32,
        last_event: u32,
        action: Action,
    ) -> Result<()> {
        let key = (pkt.source, sender);

        if !self.clients.contains_key(&key) {
            let message = Message::new(
                seq,
                sender,
                Body::Reply {
                    events: vec![Event {
                        id: last_event.saturating_add(1),
                        body: EventBody::Error {
                            text: "unknown session; restart pingchess connect".into(),
                        },
                    }],
                },
            );
            return self.send_message(&pkt, &message);
        }

        let is_new = {
            let client = self.clients.get_mut(&key).unwrap();
            client.last_seen = Instant::now();
            while client
                .events
                .front()
                .is_some_and(|event| event.id <= last_event)
            {
                client.events.pop_front();
            }
            seq_newer(seq, client.last_seq)
        };

        let mut remove_after_reply = false;
        if is_new {
            self.clients.get_mut(&key).unwrap().last_seq = seq;
            remove_after_reply = self.apply_action(key, action);
        }

        self.send_events_reply(&pkt, key, seq, last_event)?;

        if remove_after_reply {
            let nick = self.nick_of(key);
            self.clients.remove(&key);
            println!("[hub] {nick} disconnected");
        }
        Ok(())
    }

    /// Applies an action once. Returns true when the client should be removed
    /// after its acknowledgement has been sent.
    fn apply_action(&mut self, key: ClientKey, action: Action) -> bool {
        match action {
            Action::Poll => false,
            Action::Join { room } => {
                self.join_room(key, &room);
                false
            }
            Action::Part => {
                if self.clients.get(&key).is_some_and(|c| c.room.is_some()) {
                    self.leave_room(key, true);
                } else {
                    self.send_error(key, "you are not in a room");
                }
                false
            }
            Action::Chat { text } => {
                self.chat(key, &text, false);
                false
            }
            Action::Emote { text } => {
                self.chat(key, &text, true);
                false
            }
            Action::Nick { nick } => {
                self.change_nick(key, &nick);
                false
            }
            Action::Move { text } => {
                self.play_move(key, &text);
                false
            }
            Action::Resign => {
                self.resign(key);
                false
            }
            Action::New { opponent } => {
                self.new_game(key, &opponent);
                false
            }
            Action::List => {
                let events = self.room_list_event();
                self.enqueue(key, events);
                false
            }
            Action::Who => {
                let room_name = self.clients.get(&key).and_then(|c| c.room.clone());
                match room_name {
                    Some(room_name) => self.send_room_info(key, &room_name),
                    None => self.send_error(key, "you are not in a room"),
                }
                false
            }
            Action::Quit => {
                self.leave_room(key, true);
                true
            }
        }
    }

    fn join_room(&mut self, key: ClientKey, requested: &str) {
        let room_name = match normalize_room(requested) {
            Ok(room_name) => room_name,
            Err(reason) => {
                self.send_error(key, reason);
                return;
            }
        };

        let current = self.clients.get(&key).and_then(|c| c.room.clone());
        if current.as_deref() == Some(room_name.as_str()) {
            self.send_room_snapshot(key, &room_name);
            return;
        }

        if current.is_some() {
            self.leave_room(key, true);
        }

        self.rooms
            .entry(room_name.clone())
            .or_insert_with(|| Room::new(room_name.clone()));
        if let Some(room) = self.rooms.get_mut(&room_name)
            && !room.members.contains(&key)
        {
            room.members.push(key);
        }
        if let Some(client) = self.clients.get_mut(&key) {
            client.room = Some(room_name.clone());
        }

        self.send_room_snapshot(key, &room_name);
        let nick = self.nick_of(key);
        self.broadcast(
            &room_name,
            EventBody::System {
                text: format!("{nick} joined {room_name}."),
            },
        );
        self.broadcast_info_except(&room_name, Some(key));
        println!("[hub] {nick} joined {room_name}");
    }

    fn leave_room(&mut self, key: ClientKey, notify_self: bool) {
        let Some(room_name) = self.clients.get(&key).and_then(|c| c.room.clone()) else {
            return;
        };
        let nick = self.nick_of(key);

        let mut winner: Option<ClientKey> = None;
        let remove_room;
        {
            let Some(room) = self.rooms.get_mut(&room_name) else {
                if let Some(client) = self.clients.get_mut(&key) {
                    client.room = None;
                }
                return;
            };
            room.members.retain(|member| *member != key);
            if let Some(game) = &mut room.game
                && !game.game.is_over()
                && (game.white == key || game.black == key)
            {
                let color = if game.white == key {
                    Color::White
                } else {
                    Color::Black
                };
                game.game.resign(color);
                winner = Some(if color == Color::White {
                    game.black
                } else {
                    game.white
                });
            }
            remove_room = room.members.is_empty() && room_name != "#lobby";
        }

        if let Some(client) = self.clients.get_mut(&key) {
            client.room = None;
        }

        if notify_self {
            self.enqueue(
                key,
                EventBody::System {
                    text: format!("You left {room_name}."),
                },
            );
            self.enqueue(
                key,
                EventBody::RoomInfo {
                    room: String::new(),
                    total: 0,
                    members: Vec::new(),
                },
            );
        }

        if let Some(winner) = winner {
            let winner_nick = self.nick_of(winner);
            self.broadcast(
                &room_name,
                EventBody::System {
                    text: format!("{nick} left; {winner_nick} wins by forfeit."),
                },
            );
            self.broadcast_board(&room_name);
        } else {
            self.broadcast(
                &room_name,
                EventBody::System {
                    text: format!("{nick} left {room_name}."),
                },
            );
        }
        self.broadcast_info(&room_name);

        if remove_room {
            self.rooms.remove(&room_name);
            println!("[hub] room {room_name} closed");
        }
    }

    fn chat(&mut self, key: ClientKey, text: &str, emote: bool) {
        let Some(room_name) = self.clients.get(&key).and_then(|c| c.room.clone()) else {
            self.send_error(key, "join a room first: /join #lobby");
            return;
        };
        let text = clean_text(text);
        if text.is_empty() {
            self.send_error(key, "message is empty");
            return;
        }
        let from = self.nick_of(key);
        let body = if emote {
            EventBody::Emote { from, text }
        } else {
            EventBody::Chat { from, text }
        };
        self.broadcast(&room_name, body);
    }

    fn change_nick(&mut self, key: ClientKey, requested: &str) {
        let nick = match validate_nick(requested) {
            Ok(nick) => nick,
            Err(reason) => {
                self.send_error(key, reason);
                return;
            }
        };
        if self.nick_taken(&nick, Some(key)) {
            self.send_error(key, format!("{nick} is already in use"));
            return;
        }

        let old = self.nick_of(key);
        if let Some(client) = self.clients.get_mut(&key) {
            client.nick = nick.clone();
        }
        self.enqueue(key, EventBody::SelfNick { nick: nick.clone() });

        let room_name = self.clients.get(&key).and_then(|c| c.room.clone());
        if let Some(room_name) = room_name {
            self.broadcast(
                &room_name,
                EventBody::System {
                    text: format!("{old} is now known as {nick}."),
                },
            );
            self.broadcast_info(&room_name);
            if self
                .rooms
                .get(&room_name)
                .is_some_and(|room| room.game.is_some())
            {
                self.broadcast_board(&room_name);
            }
        } else {
            self.enqueue(
                key,
                EventBody::System {
                    text: format!("You are now known as {nick}."),
                },
            );
        }
    }

    fn play_move(&mut self, key: ClientKey, text: &str) {
        let Some(room_name) = self.clients.get(&key).and_then(|c| c.room.clone()) else {
            self.send_error(key, "join a room first: /join #lobby");
            return;
        };

        let mut outcome: std::result::Result<(String, Option<GameResult>), String> =
            Err("no game in progress; /new <nick> starts a game".into());
        if let Some(room) = self.rooms.get_mut(&room_name)
            && let Some(game) = &mut room.game
        {
            let color = if game.white == key {
                Some(Color::White)
            } else if game.black == key {
                Some(Color::Black)
            } else {
                None
            };
            outcome = match color {
                None => Err("you are spectating; /new <nick> starts a game".into()),
                Some(_) if game.game.is_over() => {
                    Err("the game is over; /new <nick> for a rematch".into())
                }
                Some(color) if game.game.turn() != color => Err("not your turn".into()),
                Some(_) => match game.game.apply_input(text) {
                    Ok(applied) => {
                        game.last_uci = Some(applied.uci.to_string());
                        Ok((applied.san.to_string(), applied.result))
                    }
                    Err(reason) => Err(format!("invalid move: {reason}")),
                },
            };
        }

        let (san, result) = match outcome {
            Ok(outcome) => outcome,
            Err(reason) => {
                self.send_error(key, reason);
                return;
            }
        };

        let nick = self.nick_of(key);
        self.broadcast(
            &room_name,
            EventBody::System {
                text: format!("{nick} plays {san}."),
            },
        );
        self.broadcast_board(&room_name);

        if let Some(result) = result {
            let text = self.result_text(&room_name, result);
            self.broadcast(&room_name, EventBody::System { text });
        }
    }

    fn resign(&mut self, key: ClientKey) {
        let Some(room_name) = self.clients.get(&key).and_then(|c| c.room.clone()) else {
            self.send_error(key, "join a room first: /join #lobby");
            return;
        };

        let mut outcome: std::result::Result<ClientKey, String> = Err("no game in progress".into());
        if let Some(room) = self.rooms.get_mut(&room_name)
            && let Some(game) = &mut room.game
        {
            let color = if game.white == key {
                Some(Color::White)
            } else if game.black == key {
                Some(Color::Black)
            } else {
                None
            };
            outcome = match color {
                None => Err("you are spectating; /new <nick> starts a game".into()),
                Some(_) if game.game.is_over() => Err("the game is already over".into()),
                Some(color) => {
                    game.game.resign(color);
                    Ok(if color == Color::White {
                        game.black
                    } else {
                        game.white
                    })
                }
            };
        }

        let winner = match outcome {
            Ok(winner) => winner,
            Err(reason) => {
                self.send_error(key, reason);
                return;
            }
        };

        let loser = self.nick_of(key);
        let winner_nick = self.nick_of(winner);
        self.broadcast(
            &room_name,
            EventBody::System {
                text: format!("{loser} resigned. {winner_nick} wins."),
            },
        );
        self.broadcast_board(&room_name);
    }

    fn new_game(&mut self, key: ClientKey, opponent: &str) {
        let Some(room_name) = self.clients.get(&key).and_then(|c| c.room.clone()) else {
            self.send_error(key, "join a room first: /join #lobby");
            return;
        };

        let members = self
            .rooms
            .get(&room_name)
            .map(|room| room.members.clone())
            .unwrap_or_default();
        let playing = self
            .rooms
            .get(&room_name)
            .is_some_and(|room| room.is_playing());
        if playing {
            self.send_error(key, "a game is already in progress in this room");
            return;
        }

        let opponent = opponent.trim();
        let opponent_key = if opponent.is_empty() {
            if members.len() == 2 {
                members.iter().copied().find(|member| *member != key)
            } else {
                None
            }
        } else {
            match validate_nick(opponent) {
                Ok(nick) => members.iter().copied().find(|member| {
                    *member != key && self.nick_of(*member).eq_ignore_ascii_case(&nick)
                }),
                Err(_) => None,
            }
        };

        let Some(opponent_key) = opponent_key else {
            let reason = if opponent.is_empty() {
                "this room does not have exactly two members; use /new <nick>"
            } else {
                "that player is not in this room"
            };
            self.send_error(key, reason);
            return;
        };

        let caller_nick = self.nick_of(key);
        let opponent_nick = self.nick_of(opponent_key);
        if let Some(room) = self.rooms.get_mut(&room_name) {
            room.game = Some(RoomGame {
                game: Game::new(),
                white: key,
                black: opponent_key,
                last_uci: None,
            });
        }

        println!("[hub] {caller_nick} vs {opponent_nick} in {room_name}");
        self.broadcast(
            &room_name,
            EventBody::System {
                text: format!("New game: {caller_nick} (White) vs {opponent_nick} (Black)."),
            },
        );
        self.broadcast_board(&room_name);
        self.broadcast_info(&room_name);
    }

    fn send_room_snapshot(&mut self, key: ClientKey, room_name: &str) {
        let Some(room) = self.rooms.get(room_name) else {
            return;
        };
        let info = self.room_info_event(room);
        let board = self.board_event(room);
        self.enqueue(key, info);
        self.enqueue(key, board);
    }

    fn send_room_info(&mut self, key: ClientKey, room_name: &str) {
        let Some(room) = self.rooms.get(room_name) else {
            return;
        };
        let info = self.room_info_event(room);
        self.enqueue(key, info);
    }

    fn room_info_event(&self, room: &Room) -> EventBody {
        let (white, black) = match &room.game {
            Some(game) => (Some(game.white), Some(game.black)),
            None => (None, None),
        };
        let members: Vec<Member> = room
            .members
            .iter()
            .take(MAX_MEMBERS)
            .map(|key| {
                let role = if Some(*key) == white {
                    Role::White
                } else if Some(*key) == black {
                    Role::Black
                } else if room.game.is_some() {
                    Role::Spectator
                } else {
                    Role::Member
                };
                Member {
                    nick: self.nick_of(*key),
                    role,
                }
            })
            .collect();
        EventBody::RoomInfo {
            room: room.name.clone(),
            total: room.members.len().min(255) as u8,
            members,
        }
    }

    fn board_event(&self, room: &Room) -> EventBody {
        if let Some(game) = &room.game {
            let result = game.game.result();
            let status = self.status_text(game);
            EventBody::Board {
                white: self.nick_of(game.white),
                black: self.nick_of(game.black),
                turn: if result.is_some() {
                    None
                } else {
                    Some(game.game.turn())
                },
                status,
                fen: game.game.fen(),
                last_uci: game.last_uci.clone().unwrap_or_default(),
                movetext: movetext_tail(&game.game.movetext()),
                result: result.map_or("*", |result| result.pgn()).to_string(),
            }
        } else {
            EventBody::Board {
                white: String::new(),
                black: String::new(),
                turn: None,
                status: "no game - /new <nick> to start".into(),
                fen: Game::new().fen(),
                last_uci: String::new(),
                movetext: String::new(),
                result: "*".into(),
            }
        }
    }

    fn status_text(&self, game: &RoomGame) -> String {
        match game.game.result() {
            None => "in progress".into(),
            Some(GameResult::Checkmate { winner }) => {
                format!("checkmate - {} wins", self.color_nick(game, winner))
            }
            Some(GameResult::Resignation { winner }) => {
                format!("resigned - {} wins", self.color_nick(game, winner))
            }
            Some(GameResult::Stalemate) => "draw - stalemate".into(),
            Some(GameResult::InsufficientMaterial) => "draw - insufficient material".into(),
            Some(GameResult::FiftyMove) => "draw - fifty-move rule".into(),
            Some(GameResult::ThreefoldRepetition) => "draw - threefold repetition".into(),
        }
    }

    fn color_nick(&self, game: &RoomGame, color: Color) -> String {
        self.nick_of(if color == Color::White {
            game.white
        } else {
            game.black
        })
    }

    fn result_text(&self, room_name: &str, result: GameResult) -> String {
        let Some(room) = self.rooms.get(room_name) else {
            return "Game over.".into();
        };
        let Some(game) = &room.game else {
            return "Game over.".into();
        };
        match result {
            GameResult::Checkmate { winner } => {
                format!("Checkmate! {} wins.", self.color_nick(game, winner))
            }
            GameResult::Resignation { winner } => {
                format!("{} wins by resignation.", self.color_nick(game, winner))
            }
            GameResult::Stalemate => "Draw by stalemate.".into(),
            GameResult::InsufficientMaterial => "Draw by insufficient material.".into(),
            GameResult::FiftyMove => "Draw by the fifty-move rule.".into(),
            GameResult::ThreefoldRepetition => "Draw by threefold repetition.".into(),
        }
    }

    fn room_list_event(&self) -> EventBody {
        let mut names: Vec<&String> = self.rooms.keys().collect();
        names.sort();
        let total = names.len().min(255) as u8;
        let rooms: Vec<RoomSummary> = names
            .into_iter()
            .take(MAX_ROOMS)
            .map(|name| {
                let room = &self.rooms[name];
                RoomSummary {
                    name: name.clone(),
                    members: room.members.len().min(255) as u8,
                    status: if room.is_playing() {
                        "playing".into()
                    } else if room.game.is_some() {
                        "game over".into()
                    } else {
                        "no game".into()
                    },
                }
            })
            .collect();
        EventBody::RoomList { total, rooms }
    }

    fn send_events_reply(
        &mut self,
        pkt: &IcmpPacket,
        key: ClientKey,
        acked: u16,
        last_event: u32,
    ) -> Result<()> {
        let mut selected = Vec::new();
        let mut size = 1usize; // event count byte

        if let Some(client) = self.clients.get(&key) {
            for event in &client.events {
                if event.id <= last_event {
                    continue;
                }
                let encoded = event.encode();
                if size + encoded.len() > MAX_EVENT_BYTES {
                    break;
                }
                size += encoded.len();
                selected.push(event.clone());
            }
        }

        let message = Message::new(acked, key.1, Body::Reply { events: selected });
        self.send_message(pkt, &message)
    }

    fn send_message(&mut self, pkt: &IcmpPacket, message: &Message) -> Result<()> {
        let payload = message.encode();
        let wire = self
            .socket
            .send_reply(pkt.source, pkt.id, pkt.seq, &payload)?;
        self.trace_tx(&wire, message);
        Ok(())
    }

    fn send_error(&mut self, key: ClientKey, text: impl Into<String>) {
        self.enqueue(key, EventBody::Error { text: text.into() });
    }

    fn enqueue(&mut self, key: ClientKey, body: EventBody) {
        if let Some(client) = self.clients.get_mut(&key) {
            client.enqueue(body);
        }
    }

    fn broadcast(&mut self, room_name: &str, body: EventBody) {
        let keys = self
            .rooms
            .get(room_name)
            .map(|room| room.members.clone())
            .unwrap_or_default();
        for key in keys {
            self.enqueue(key, body.clone());
        }
    }

    fn broadcast_info(&mut self, room_name: &str) {
        self.broadcast_info_except(room_name, None);
    }

    fn broadcast_info_except(&mut self, room_name: &str, except: Option<ClientKey>) {
        let Some(info) = self
            .rooms
            .get(room_name)
            .map(|room| self.room_info_event(room))
        else {
            return;
        };
        let keys = self
            .rooms
            .get(room_name)
            .map(|room| room.members.clone())
            .unwrap_or_default();
        for key in keys {
            if Some(key) != except {
                self.enqueue(key, info.clone());
            }
        }
    }

    fn broadcast_board(&mut self, room_name: &str) {
        let board = self.rooms.get(room_name).map(|room| self.board_event(room));
        if let Some(board) = board {
            self.broadcast(room_name, board);
        }
    }

    fn nick_of(&self, key: ClientKey) -> String {
        self.clients
            .get(&key)
            .map(|client| client.nick.clone())
            .unwrap_or_else(|| "departed".into())
    }

    fn nick_taken(&self, nick: &str, except: Option<ClientKey>) -> bool {
        self.clients
            .iter()
            .any(|(key, client)| Some(*key) != except && client.nick.eq_ignore_ascii_case(nick))
    }

    fn unique_nick(&self, base: &str, except: Option<ClientKey>) -> String {
        if !self.nick_taken(base, except) {
            return base.to_string();
        }
        for index in 2..=99 {
            let suffix = format!("_{index}");
            let keep = MAX_NICK.saturating_sub(suffix.len());
            let stem: String = base.chars().take(keep).collect();
            let candidate = format!("{stem}{suffix}");
            if !self.nick_taken(&candidate, except) {
                return candidate;
            }
        }
        "guest".into()
    }

    fn cleanup_clients(&mut self) {
        let now = Instant::now();
        if now.saturating_duration_since(self.last_cleanup) < CLEANUP_INTERVAL {
            return;
        }
        self.last_cleanup = now;

        let expired: Vec<ClientKey> = self
            .clients
            .iter()
            .filter(|(_, client)| now.saturating_duration_since(client.last_seen) > CLIENT_TIMEOUT)
            .map(|(key, _)| *key)
            .collect();

        for key in expired {
            let nick = self.nick_of(key);
            self.leave_room(key, false);
            self.clients.remove(&key);
            println!("[hub] {nick} timed out");
        }
    }

    fn read_stdin(&mut self) -> Result<()> {
        let mut buf = [0u8; 1024];
        let read = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if read == 0 {
            self.stdin_open = false;
            return Ok(());
        }
        if read < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(()),
                _ => Err(err.into()),
            };
        }

        self.input.extend_from_slice(&buf[..read as usize]);
        while let Some(pos) = self.input.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.input.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            self.handle_console(&text);
        }

        if !self.stdin_open && !self.input.is_empty() {
            let text = String::from_utf8_lossy(&self.input).to_string();
            self.input.clear();
            self.handle_console(&text);
        }
        Ok(())
    }

    fn handle_console(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        match command.to_ascii_lowercase().as_str() {
            "rooms" => self.print_rooms(),
            "clients" => self.print_clients(),
            "motd" => {
                if rest.is_empty() {
                    println!("{}", self.cfg.motd);
                } else {
                    self.cfg.motd = rest.to_string();
                    println!("motd updated");
                }
            }
            "help" | "?" => {
                println!("console commands: rooms, clients, motd [text], help, quit");
            }
            "quit" | "exit" => self.exit = true,
            other => println!("unknown console command '{other}'"),
        }
    }

    fn print_rooms(&self) {
        let mut names: Vec<&String> = self.rooms.keys().collect();
        names.sort();
        if names.is_empty() {
            println!("no rooms");
            return;
        }
        println!("{:<20} {:>8}  status", "room", "members");
        for name in names {
            let room = &self.rooms[name];
            let status = if room.is_playing() {
                "playing"
            } else if room.game.is_some() {
                "game over"
            } else {
                "no game"
            };
            println!("{:<20} {:>8}  {status}", room.name, room.members.len());
        }
    }

    fn print_clients(&self) {
        let mut clients: Vec<(&ClientKey, &Client)> = self.clients.iter().collect();
        clients.sort_by(|a, b| a.1.nick.cmp(&b.1.nick));
        if clients.is_empty() {
            println!("no clients");
            return;
        }
        for ((ip, _sender), client) in clients {
            let room = client.room.as_deref().unwrap_or("-");
            println!("{:<20} {ip:<16} {room}", client.nick);
        }
    }

    fn wait(&self, timeout: Duration) -> Result<(bool, bool)> {
        let mut fds = [
            libc::pollfd {
                fd: self.socket.raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let nfds = if self.stdin_open { 2 } else { 1 };
        let millis = timeout.as_millis().min(1000) as i32;

        let result = unsafe { libc::poll(fds.as_mut_ptr(), nfds, millis) };
        if result < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok((false, false));
            }
            return Err(err.into());
        }
        if result == 0 {
            return Ok((false, false));
        }

        let socket_ready = fds[0].revents & libc::POLLIN != 0;
        let stdin_ready =
            self.stdin_open && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        Ok((socket_ready, stdin_ready))
    }

    fn trace_rx(&self, pkt: &IcmpPacket, message: &Message) {
        if !self.cfg.trace {
            return;
        }
        let what = match &message.body {
            Body::Hello { nick } => format!("hello nick={nick}"),
            Body::Msg { last_event, action } => {
                format!("msg last_event={last_event} action={}", action_name(action))
            }
            Body::Welcome { .. } => "welcome".into(),
            Body::Reply { events } => format!("reply events={}", events.len()),
        };
        println!(
            "  icmp rx from {} seq={} ttl={} [{what}] {} B: {}",
            pkt.source,
            pkt.seq,
            pkt.ttl,
            pkt.raw.len(),
            ui::hex_bytes(&pkt.raw)
        );
    }

    fn trace_tx(&self, wire: &[u8], message: &Message) {
        if !self.cfg.trace {
            return;
        }
        let what = match &message.body {
            Body::Welcome { nick, .. } => format!("welcome nick={nick}"),
            Body::Reply { events } => format!("reply ack={} events={}", message.seq, events.len()),
            _ => "message".into(),
        };
        println!(
            "  icmp tx client={:08x} seq={} [{what}] {} B: {}",
            message.sender,
            message.seq,
            wire.len(),
            ui::hex_bytes(wire)
        );
    }
}

fn seq_newer(seq: u16, last: u16) -> bool {
    seq != last && seq.wrapping_sub(last) < 0x8000
}

fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Poll => "poll",
        Action::Join { .. } => "join",
        Action::Part => "part",
        Action::Chat { .. } => "chat",
        Action::Emote { .. } => "emote",
        Action::Nick { .. } => "nick",
        Action::Move { .. } => "move",
        Action::Resign => "resign",
        Action::New { .. } => "new",
        Action::List => "list",
        Action::Who => "who",
        Action::Quit => "quit",
    }
}

fn clean_nick(input: &str) -> String {
    let filtered: String = ui::sanitize(input)
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .take(MAX_NICK)
        .collect();
    if filtered.is_empty() {
        "guest".into()
    } else {
        filtered
    }
}

fn validate_nick(input: &str) -> std::result::Result<String, String> {
    let nick = ui::sanitize(input).trim().to_string();
    if nick.is_empty() {
        return Err("nick is empty".into());
    }
    if nick.chars().count() > MAX_NICK {
        return Err(format!("nicks are at most {MAX_NICK} characters"));
    }
    if !nick
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err("nicks may use letters, digits, '_' and '-'".into());
    }
    Ok(nick)
}

fn normalize_room(input: &str) -> std::result::Result<String, String> {
    let trimmed = ui::sanitize(input).trim().to_string();
    let body = trimmed
        .strip_prefix('#')
        .unwrap_or(&trimmed)
        .to_ascii_lowercase();
    if body.is_empty() {
        return Err("room name is empty".into());
    }
    if body.chars().count() > 20 {
        return Err("room names are at most 20 characters".into());
    }
    if !body
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err("room names may use letters, digits, '_' and '-'".into());
    }
    Ok(format!("#{body}"))
}

fn clean_text(input: &str) -> String {
    ui::sanitize(input.trim()).chars().take(MAX_TEXT).collect()
}

fn movetext_tail(text: &str) -> String {
    if text.len() <= MAX_MOVETEXT {
        return text.to_string();
    }
    let budget = MAX_MOVETEXT - 3;
    let mut start = text.len() - budget;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_rooms() {
        assert_eq!(normalize_room("#Lobby").unwrap(), "#lobby");
        assert_eq!(normalize_room("chess").unwrap(), "#chess");
        assert!(normalize_room("").is_err());
        assert!(normalize_room("bad room").is_err());
        assert!(normalize_room("#this-name-is-way-too-long-to-fit").is_err());
    }

    #[test]
    fn validates_nicks() {
        assert_eq!(validate_nick("alice").unwrap(), "alice");
        assert_eq!(validate_nick("a_b-2").unwrap(), "a_b-2");
        assert!(validate_nick("").is_err());
        assert!(validate_nick("bad nick").is_err());
        assert!(validate_nick("n".repeat(MAX_NICK + 1).as_str()).is_err());
    }

    #[test]
    fn keeps_the_tail_of_long_movetext() {
        let text = format!("1. {} 2. Qh4#", "e4 ".repeat(200));
        let tail = movetext_tail(&text);
        assert!(tail.starts_with("..."));
        assert!(tail.ends_with("Qh4#"));
        assert!(tail.len() <= MAX_MOVETEXT);
    }
}

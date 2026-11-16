//! IRC-like client for the pingchess rooms hub.
//!
//! Every request is an ICMP echo request. The hub's echo reply acknowledges
//! the request and carries room events. While idle the client polls twice a
//! second. Moves typed ahead wait until it is actually the player's turn.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use shakmaty::fen::Fen;
use shakmaty::san::SanPlus;
use shakmaty::uci::UciMove;
use shakmaty::{CastlingMode, Chess, Color, Move, Position};

use crate::hub_proto::{Action, Body, Event, EventBody, Message};
use crate::icmp::{self, IcmpPacket, IcmpSocket};
use crate::ui;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct Config {
    pub hub: Ipv4Addr,
    pub nick: String,
    pub room: String,
    pub ascii: bool,
    pub color: bool,
    pub timeout: Duration,
    pub retries: u32,
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

const HELP: &str = "\
Commands:
  /join <room>    join (or create) a room, e.g. /join #lobby
  /part           leave the current room
  /nick <nick>    change your nick
  /list           list rooms
  /who            list members of the current room
  /new [nick]     start a game in the room (you are White)
  /move <move>    force a move, e.g. /move e2e4
  /resign         resign the current game
  /say <text>     send chat even if the text looks like a move
  /me <text>      send an emote
  /board, /b      show the board
  /moves          list legal moves
  /history        show the move list
  /pgn            show the game as PGN
  /flip           flip the board
  /help, /?       show this help
  /quit           leave the hub and quit

Plain lines are chat unless they look like a chess move (e4, Nf3, e2e4, O-O).
Move-like lines are played on your turn, queued if you are waiting, and treated
as chat while spectating or after the game. Use /say to force chat.";

struct Pending {
    seq: u16,
    payload: Vec<u8>,
    label: &'static str,
    last_sent: Instant,
    attempts: u32,
}

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

    let mut client = Client {
        cfg: config,
        socket,
        sender: icmp::random_u32() | 1,
        next_seq: 1,
        nick: None,
        room: None,
        pending: None,
        last_event: 0,
        outgoing: VecDeque::new(),
        queued_moves: VecDeque::new(),
        input: Vec::new(),
        stdin_open: true,
        prompted: false,
        next_poll: Instant::now(),
        view: None,
        last_move: None,
        white: None,
        black: None,
        turn: None,
        status: String::new(),
        movetext: String::new(),
        result: "*".into(),
        orientation: Color::White,
        user_flipped: false,
        welcome: false,
        quitting: false,
        exit: false,
    };
    client.main_loop()
}

struct Client {
    cfg: Config,
    socket: IcmpSocket,
    sender: u32,
    next_seq: u16,
    nick: Option<String>,
    room: Option<String>,
    pending: Option<Pending>,
    last_event: u32,
    outgoing: VecDeque<Action>,
    queued_moves: VecDeque<String>,
    input: Vec<u8>,
    stdin_open: bool,
    prompted: bool,
    next_poll: Instant,
    view: Option<Chess>,
    last_move: Option<Move>,
    white: Option<String>,
    black: Option<String>,
    turn: Option<Color>,
    status: String,
    movetext: String,
    result: String,
    orientation: Color,
    user_flipped: bool,
    welcome: bool,
    quitting: bool,
    exit: bool,
}

impl Client {
    fn main_loop(&mut self) -> Result<()> {
        self.print_intro();
        self.send_hello()?;

        while !self.exit {
            let timeout = self.poll_timeout();
            let (socket_ready, stdin_ready) = self.wait(timeout)?;
            if stdin_ready {
                self.read_stdin()?;
            }
            if socket_ready {
                self.drain_socket()?;
            }
            self.on_timers()?;
            self.pump()?;
            self.update_prompt()?;
        }

        println!("bye");
        Ok(())
    }

    fn print_intro(&self) {
        println!("pingchess {} - rooms client", env!("CARGO_PKG_VERSION"));
        println!("connecting to {} as {}...", self.cfg.hub, self.cfg.nick);
    }

    fn send_hello(&mut self) -> Result<()> {
        let seq = self.take_seq();
        let message = Message::new(
            seq,
            self.sender,
            Body::Hello {
                nick: self.cfg.nick.clone(),
            },
        );
        let payload = message.encode();
        let wire = self.socket.send_request(self.cfg.hub, seq, &payload)?;
        self.trace_tx(&wire, seq, &format!("hello nick={}", self.cfg.nick));

        let now = Instant::now();
        self.pending = Some(Pending {
            seq,
            payload,
            label: "hello",
            last_sent: now,
            attempts: 1,
        });
        Ok(())
    }

    fn take_seq(&mut self) -> u16 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.next_seq == 0 {
            self.next_seq = 1;
        }
        seq
    }

    fn send_action(&mut self, action: Action) -> Result<()> {
        let label = action_label(&action);
        let seq = self.take_seq();
        let message = Message::new(
            seq,
            self.sender,
            Body::Msg {
                last_event: self.last_event,
                action,
            },
        );
        let payload = message.encode();
        let wire = self.socket.send_request(self.cfg.hub, seq, &payload)?;
        self.trace_tx(&wire, seq, label);

        let now = Instant::now();
        self.pending = Some(Pending {
            seq,
            payload,
            label,
            last_sent: now,
            attempts: 1,
        });
        Ok(())
    }

    fn queue_action(&mut self, action: Action) -> Result<()> {
        self.outgoing.push_back(action);
        self.pump()
    }

    fn pump(&mut self) -> Result<()> {
        if self.pending.is_some() {
            return Ok(());
        }
        if let Some(action) = self.outgoing.pop_front() {
            self.send_action(action)?;
        } else if !self.quitting && Instant::now() >= self.next_poll {
            self.send_action(Action::Poll)?;
        }
        Ok(())
    }

    fn drain_socket(&mut self) -> Result<()> {
        while let Some(pkt) = self.socket.recv()? {
            if pkt.kind != icmp::ECHO_REPLY || pkt.source != self.cfg.hub {
                continue;
            }
            let message = match Message::decode(&pkt.payload) {
                Ok(message) => message,
                Err(_) => continue,
            };
            if message.sender != self.sender {
                continue;
            }
            self.trace_rx(&pkt, &message);

            match message.body {
                Body::Welcome { hub, nick, error } => {
                    self.on_welcome(message.seq, hub, nick, error)?;
                }
                Body::Reply { events } => self.on_reply(message.seq, events)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn on_welcome(&mut self, seq: u16, hub: String, nick: String, error: String) -> Result<()> {
        if self.welcome {
            return Ok(());
        }
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.label == "hello" && pending.seq == seq)
        {
            return Ok(());
        }

        self.pending = None;
        if !error.is_empty() {
            self.emit(&format!("hub rejected the connection: {error}"));
            self.exit = true;
            return Ok(());
        }

        self.welcome = true;
        self.nick = Some(nick.clone());
        self.next_poll = Instant::now() + POLL_INTERVAL;
        self.outgoing.push_front(Action::Join {
            room: self.cfg.room.clone(),
        });
        self.emit(&format!("Connected to {hub} as {nick}."));
        self.pump()
    }

    fn on_reply(&mut self, seq: u16, events: Vec<Event>) -> Result<()> {
        let mut quit_acked = false;
        if let Some(pending) = &self.pending
            && pending.seq == seq
        {
            quit_acked = pending.label == "quit";
            self.pending = None;
        }

        self.next_poll = Instant::now() + POLL_INTERVAL;
        self.process_events(events)?;

        if quit_acked && self.quitting {
            self.exit = true;
        }
        self.pump()
    }

    fn process_events(&mut self, events: Vec<Event>) -> Result<()> {
        for event in events {
            if event.id <= self.last_event {
                continue;
            }
            self.last_event = event.id;
            self.handle_event(event.body)?;
        }
        self.flush_moves()
    }

    fn handle_event(&mut self, event: EventBody) -> Result<()> {
        match event {
            EventBody::System { text } => {
                let line = self.paint("36", &format!("* {}", ui::sanitize(&text)));
                self.emit(&line);
            }
            EventBody::Chat { from, text } => {
                let line = format!(
                    "{} {}",
                    self.paint("1;36", &format!("<{}>", ui::sanitize(&from))),
                    ui::sanitize(&text)
                );
                self.emit(&line);
            }
            EventBody::Emote { from, text } => {
                let line = self.paint(
                    "1;35",
                    &format!("* {} {}", ui::sanitize(&from), ui::sanitize(&text)),
                );
                self.emit(&line);
            }
            EventBody::RoomInfo {
                room,
                total,
                members,
            } => {
                if room.is_empty() {
                    self.room = None;
                    self.view = None;
                    self.last_move = None;
                    self.white = None;
                    self.black = None;
                    self.turn = None;
                    self.status.clear();
                    self.movetext.clear();
                    self.result = "*".into();
                    self.queued_moves.clear();
                    self.emit("You are not in a room.");
                } else {
                    self.room = Some(room.clone());
                    let mut list = String::new();
                    for member in &members {
                        if !list.is_empty() {
                            list.push_str(", ");
                        }
                        list.push_str(&format!(
                            "{} ({})",
                            ui::sanitize(&member.nick),
                            member.role.name()
                        ));
                    }
                    if total as usize > members.len() {
                        list.push_str(&format!(", and {} more", total as usize - members.len()));
                    }
                    self.emit(&format!("{room}: {list}"));
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
                self.handle_board(white, black, turn, status, fen, last_uci, movetext, result)?;
            }
            EventBody::RoomList { total, rooms } => {
                let mut text = format!("Rooms ({total}):");
                for room in &rooms {
                    text.push_str(&format!(
                        "\n  {:<18} {:>2} members  {}",
                        room.name, room.members, room.status
                    ));
                }
                if total as usize > rooms.len() {
                    text.push_str(&format!(
                        "\n  ... and {} more",
                        total as usize - rooms.len()
                    ));
                }
                self.emit(&text);
            }
            EventBody::Error { text } => {
                let text = ui::sanitize(&text);
                self.emit(&self.paint("1;31", &format!("error: {text}")));
                if text.contains("unknown session") {
                    self.exit = true;
                }
            }
            EventBody::SelfNick { nick } => {
                self.nick = Some(nick);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_board(
        &mut self,
        white: String,
        black: String,
        turn: Option<Color>,
        status: String,
        fen: String,
        last_uci: String,
        movetext: String,
        result: String,
    ) -> Result<()> {
        let old = self.view.take();
        let new_view = parse_fen(&fen);
        let highlight = old
            .as_ref()
            .and_then(|position| parse_uci_move(&last_uci, position));

        if new_view.is_none() {
            self.emit("could not parse the board sent by the hub");
        }
        self.view = new_view;
        self.last_move = highlight;
        self.white = if white.is_empty() { None } else { Some(white) };
        self.black = if black.is_empty() { None } else { Some(black) };
        self.turn = turn;
        self.status = status;
        self.movetext = movetext;
        self.result = result;

        if let Some(color) = self.my_color()
            && !self.user_flipped
        {
            self.orientation = color;
        }

        self.print_current_board();
        Ok(())
    }

    fn flush_moves(&mut self) -> Result<()> {
        if self.is_my_turn()
            && let Some(text) = self.queued_moves.pop_front()
        {
            self.queue_action(Action::Move { text })?;
            return Ok(());
        }

        if !self.queued_moves.is_empty()
            && (self.game_over() || (self.game_active() && self.my_color().is_none()))
        {
            let count = self.queued_moves.len();
            self.queued_moves.clear();
            let reason = if self.game_over() {
                "the game is over"
            } else {
                "you are spectating"
            };
            self.emit(&format!("discarded {count} queued move(s): {reason}"));
        }
        Ok(())
    }

    fn handle_line(&mut self, line: &str) -> Result<()> {
        let line = line.trim();
        if line.is_empty() {
            self.prompted = false;
            return self.update_prompt();
        }
        if self.quitting {
            return Ok(());
        }

        if line.starts_with('/') {
            self.handle_command(line)?;
        } else if looks_like_move(line) {
            if self.is_my_turn() {
                self.queue_action(Action::Move {
                    text: line.to_string(),
                })?;
            } else if self.game_over() || (self.game_active() && self.my_color().is_none()) {
                self.queue_action(Action::Chat {
                    text: line.to_string(),
                })?;
            } else {
                self.queued_moves.push_back(line.to_string());
                self.emit(&format!(
                    "queued move '{line}' ({} queued; it will play on your turn)",
                    self.queued_moves.len()
                ));
            }
        } else {
            self.queue_action(Action::Chat {
                text: line.to_string(),
            })?;
        }

        self.prompted = false;
        self.update_prompt()
    }

    fn handle_command(&mut self, line: &str) -> Result<()> {
        let mut parts = line.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("").to_ascii_lowercase();
        let rest = parts.next().unwrap_or("").trim();

        match command.as_str() {
            "/join" => {
                if rest.is_empty() {
                    self.emit("usage: /join #room");
                } else {
                    self.queue_action(Action::Join {
                        room: rest.to_string(),
                    })?;
                }
            }
            "/part" => self.queue_action(Action::Part)?,
            "/nick" => {
                if rest.is_empty() {
                    self.emit("usage: /nick <nick>");
                } else {
                    self.queue_action(Action::Nick {
                        nick: rest.to_string(),
                    })?;
                }
            }
            "/list" => self.queue_action(Action::List)?,
            "/who" | "/names" => self.queue_action(Action::Who)?,
            "/new" => self.queue_action(Action::New {
                opponent: rest.to_string(),
            })?,
            "/move" => {
                if rest.is_empty() {
                    self.emit("usage: /move <uci-or-san>");
                } else {
                    self.queue_action(Action::Move {
                        text: rest.to_string(),
                    })?;
                }
            }
            "/resign" => self.queue_action(Action::Resign)?,
            "/say" => {
                if rest.is_empty() {
                    self.emit("usage: /say <text>");
                } else {
                    self.queue_action(Action::Chat {
                        text: rest.to_string(),
                    })?;
                }
            }
            "/me" => {
                if rest.is_empty() {
                    self.emit("usage: /me <text>");
                } else {
                    self.queue_action(Action::Emote {
                        text: rest.to_string(),
                    })?;
                }
            }
            "/board" | "/b" => self.print_current_board(),
            "/moves" | "/legal" => self.print_legal(),
            "/history" | "/log" => {
                if self.movetext.is_empty() {
                    self.emit("no moves yet");
                } else {
                    self.emit(&self.movetext.clone());
                }
            }
            "/pgn" => self.print_pgn(),
            "/flip" => {
                self.orientation = self.orientation.other();
                self.user_flipped = true;
                self.print_current_board();
            }
            "/help" | "/h" | "/?" => self.emit(HELP),
            "/quit" | "/exit" => {
                self.quitting = true;
                if self.nick.is_none() {
                    self.exit = true;
                } else {
                    self.emit("leaving the hub...");
                    self.outgoing.push_back(Action::Quit);
                    self.pump()?;
                }
            }
            other => self.emit(&format!("unknown command '{other}'; /help")),
        }
        Ok(())
    }

    fn print_current_board(&mut self) {
        let Some(view) = &self.view else {
            self.emit("no board yet");
            return;
        };
        let style = ui::BoardStyle {
            unicode: !self.cfg.ascii,
            color: self.cfg.color,
            orientation: self.orientation,
        };
        let board = ui::render_board(view, self.last_move, &style);
        let header = self.board_header();
        self.emit(&format!("{header}\n{}", board.trim_end()));
    }

    fn board_header(&self) -> String {
        let mut parts = vec![self.room.clone().unwrap_or_else(|| "no room".to_string())];

        match (&self.white, &self.black) {
            (Some(white), Some(black)) => {
                parts.push(format!(
                    "{} (W) vs {} (B)",
                    ui::sanitize(white),
                    ui::sanitize(black)
                ));
                if self.result != "*" {
                    parts.push(format!("result {}", self.result));
                    if !self.status.is_empty() {
                        parts.push(self.status.clone());
                    }
                } else if let Some(turn) = self.turn {
                    parts.push(format!("{} to move", ui::color_name(turn)));
                }
            }
            _ => parts.push(if self.status.is_empty() {
                "no game".into()
            } else {
                self.status.clone()
            }),
        }

        if let Some(color) = self.my_color() {
            parts.push(format!("you are {}", ui::color_name(color)));
        }
        parts.join(" | ")
    }

    fn print_legal(&mut self) {
        let Some(view) = &self.view else {
            self.emit("no board yet");
            return;
        };
        let moves = view.legal_moves();
        if moves.is_empty() {
            self.emit("no legal moves");
            return;
        }

        let mut lines = Vec::new();
        for mv in moves.iter() {
            let san = SanPlus::from_move(view.clone(), *mv);
            let uci = UciMove::from_move(*mv, CastlingMode::Standard);
            lines.push(format!("{san:<7} ({uci})"));
        }

        let mut text = String::from("Legal moves:");
        for chunk in lines.chunks(4) {
            text.push_str("\n  ");
            text.push_str(&chunk.join("  "));
        }
        self.emit(&text);
    }

    fn print_pgn(&mut self) {
        let (Some(white), Some(black)) = (&self.white, &self.black) else {
            self.emit("no game");
            return;
        };
        let room = self.room.as_deref().unwrap_or("-");
        let mut text = format!(
            "[Event \"pingchess rooms\"]\n[Site \"{room}\"]\n[White \"{}\"]\n[Black \"{}\"]\n[Result \"{}\"]\n\n",
            ui::sanitize(white),
            ui::sanitize(black),
            self.result
        );
        text.push_str(&self.movetext);
        text.push(' ');
        text.push_str(&self.result);
        self.emit(&text);
    }

    fn my_color(&self) -> Option<Color> {
        let nick = self.nick.as_deref()?;
        if self
            .white
            .as_deref()
            .is_some_and(|white| white.eq_ignore_ascii_case(nick))
        {
            Some(Color::White)
        } else if self
            .black
            .as_deref()
            .is_some_and(|black| black.eq_ignore_ascii_case(nick))
        {
            Some(Color::Black)
        } else {
            None
        }
    }

    fn game_active(&self) -> bool {
        self.white.is_some() && self.black.is_some() && self.result == "*"
    }

    fn game_over(&self) -> bool {
        self.result != "*"
    }

    fn is_my_turn(&self) -> bool {
        self.game_active()
            && self.turn.is_some()
            && self.turn == self.my_color()
            && self.result == "*"
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.cfg.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn emit(&mut self, text: &str) {
        if self.prompted {
            println!();
            self.prompted = false;
        }
        println!("{text}");
    }

    fn update_prompt(&mut self) -> Result<()> {
        if self.exit || self.prompted || self.quitting {
            return Ok(());
        }
        let Some(nick) = &self.nick else {
            return Ok(());
        };
        let prefix = self
            .room
            .as_ref()
            .map(|room| format!("[{room}] "))
            .unwrap_or_default();
        print!("{prefix}{nick}> ");
        io::stdout().flush()?;
        self.prompted = true;
        Ok(())
    }

    fn poll_timeout(&self) -> Duration {
        let now = Instant::now();
        let mut timeout = Duration::from_millis(250);
        if let Some(pending) = &self.pending {
            timeout =
                timeout.min((pending.last_sent + self.cfg.timeout).saturating_duration_since(now));
        } else if !self.quitting {
            timeout = timeout.min(self.next_poll.saturating_duration_since(now));
        }
        timeout.max(Duration::from_millis(1))
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
            self.handle_line(&text)?;
        }

        if !self.stdin_open && !self.input.is_empty() {
            let text = String::from_utf8_lossy(&self.input).to_string();
            self.input.clear();
            self.handle_line(&text)?;
        }
        Ok(())
    }

    fn on_timers(&mut self) -> Result<()> {
        let now = Instant::now();
        if let Some(pending) = &self.pending
            && now.saturating_duration_since(pending.last_sent) >= self.cfg.timeout
        {
            if pending.attempts > self.cfg.retries {
                let label = pending.label;
                let attempts = pending.attempts;
                self.pending = None;
                if self.quitting {
                    self.emit("hub did not answer the quit; exiting anyway");
                    self.exit = true;
                    return Ok(());
                }
                return Err(Error::new(format!(
                    "{label} was not acknowledged by the hub after {attempts} attempts; is ICMP blocked?"
                )));
            }

            let seq = pending.seq;
            let payload = pending.payload.clone();
            let label = pending.label;
            let retry = pending.attempts;
            let wire = self.socket.send_request(self.cfg.hub, seq, &payload)?;
            self.trace_tx(&wire, seq, &format!("{label} retry"));

            if let Some(pending) = &mut self.pending {
                pending.attempts += 1;
                pending.last_sent = now;
            }
            self.emit(&format!(
                "no reply yet; retransmitted {label} (retry {}/{})",
                retry, self.cfg.retries
            ));
        }
        Ok(())
    }

    fn trace_tx(&mut self, wire: &[u8], seq: u16, what: &str) {
        if !self.cfg.trace {
            return;
        }
        let line = format!(
            "  icmp tx seq={seq} [{what}] {} B: {}",
            wire.len(),
            ui::hex_bytes(wire)
        );
        self.emit(&line);
    }

    fn trace_rx(&mut self, pkt: &IcmpPacket, message: &Message) {
        if !self.cfg.trace {
            return;
        }
        let what = match &message.body {
            Body::Welcome { .. } => "welcome".to_string(),
            Body::Reply { events } => format!("reply events={}", events.len()),
            _ => "message".to_string(),
        };
        let line = format!(
            "  icmp rx from {} seq={} ttl={} [{what}] {} B: {}",
            pkt.source,
            pkt.seq,
            pkt.ttl,
            pkt.raw.len(),
            ui::hex_bytes(&pkt.raw)
        );
        self.emit(&line);
    }
}

fn action_label(action: &Action) -> &'static str {
    match action {
        Action::Poll => "poll",
        Action::Join { .. } => "join",
        Action::Part => "part",
        Action::Chat { .. } => "chat",
        Action::Emote { .. } => "emote",
        Action::Nick { .. } => "nick",
        Action::Move { .. } => "move",
        Action::Resign => "resign",
        Action::New { .. } => "new game",
        Action::List => "list",
        Action::Who => "who",
        Action::Quit => "quit",
    }
}

fn parse_fen(fen: &str) -> Option<Chess> {
    let parsed: Fen = fen.parse().ok()?;
    parsed.into_position(CastlingMode::Standard).ok()
}

fn parse_uci_move(uci: &str, position: &Chess) -> Option<Move> {
    UciMove::from_ascii(uci.as_bytes())
        .ok()?
        .to_move(position)
        .ok()
}

fn looks_like_move(text: &str) -> bool {
    if UciMove::from_ascii(text.as_bytes()).is_ok() {
        return true;
    }
    let normalized = text.replace('0', "O");
    SanPlus::from_ascii(normalized.as_bytes()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_moves_without_a_position() {
        for text in ["e4", "Nf3", "e2e4", "e7e8q", "O-O", "0-0", "Qh4#"] {
            assert!(looks_like_move(text), "{text} should look like a move");
        }
        for text in ["hello", "good game", "nice", "/help"] {
            assert!(!looks_like_move(text), "{text} should be chat");
        }
    }

    #[test]
    fn parses_the_initial_fen() {
        let position = parse_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("initial FEN");
        assert_eq!(position.turn(), Color::White);
    }

    #[test]
    fn highlights_a_uci_move() {
        let position =
            parse_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1").unwrap();
        assert!(parse_uci_move("e2e4", &position).is_some());
        assert!(parse_uci_move("e2e5", &position).is_none());
    }
}

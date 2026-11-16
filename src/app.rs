//! Session state machine: handshake, reliable move delivery, and the
//! terminal event loop.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use shakmaty::{Color, Position};

use crate::game::Game;
use crate::icmp::{self, IcmpPacket, IcmpSocket};
use crate::proto::{Body, Kind, Message};
use crate::ui;

const MIN_LINGER: Duration = Duration::from_millis(1200);
const MAX_LINGER: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Host,
    Join(Ipv4Addr),
}

pub struct Config {
    pub mode: Mode,
    pub name: String,
    /// Color the host wants. The joiner always gets the other color.
    pub host_color: Color,
    /// Optional host-side filter: only accept a hello from this address.
    pub peer: Option<Ipv4Addr>,
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

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Listening,
    Joining,
    Playing,
}

#[derive(Debug, Clone)]
struct Peer {
    addr: Ipv4Addr,
    sender: u32,
    name: String,
    color: Color,
}

struct Pending {
    dest: Ipv4Addr,
    seq: u16,
    payload: Vec<u8>,
    kind: Kind,
    ply: Option<u16>,
    first_sent: Instant,
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
                 hint: to try it locally without privileges, run both peers in a private\n\
                 hint: network namespace: unshare -Urn sh -c 'ip link set lo up; ...'",
            ));
        }
        Err(err) => return Err(err.into()),
    };

    let sender = icmp::random_u32() | 1;
    let mut app = App {
        cfg: config,
        socket,
        sender,
        phase: Phase::Listening,
        session: 0,
        peer: None,
        our_color: None,
        orientation: Color::White,
        game: Game::new(),
        next_seq: 1,
        pending: None,
        input: Vec::new(),
        stdin_open: true,
        queued: VecDeque::new(),
        prompted: false,
        result_printed: false,
        game_over_at: None,
        quit_requested: false,
        exit: false,
    };
    app.main_loop()
}

struct App {
    cfg: Config,
    socket: IcmpSocket,
    sender: u32,
    phase: Phase,
    session: u16,
    peer: Option<Peer>,
    our_color: Option<Color>,
    orientation: Color,
    game: Game,
    next_seq: u16,
    pending: Option<Pending>,
    input: Vec<u8>,
    stdin_open: bool,
    queued: VecDeque<String>,
    prompted: bool,
    result_printed: bool,
    game_over_at: Option<Instant>,
    quit_requested: bool,
    exit: bool,
}

impl App {
    fn main_loop(&mut self) -> Result<()> {
        self.print_intro();

        match self.cfg.mode {
            Mode::Host => {
                self.phase = Phase::Listening;
                println!("Listening for an ICMP hello...");
                if let Some(peer) = self.cfg.peer {
                    println!("Only accepting a hello from {peer}.");
                }
                println!("Share this machine's IPv4 address with your opponent.");
                println!("Press Ctrl-C to stop.");
            }
            Mode::Join(addr) => {
                self.phase = Phase::Joining;
                self.send_hello(addr)?;
            }
        }

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
        }

        Ok(())
    }

    fn print_intro(&self) {
        println!(
            "pingchess {} - chess over ICMP echo packets",
            env!("CARGO_PKG_VERSION")
        );
        println!(
            "local ICMP id 0x{:04x}, sender id 0x{:08x}",
            self.socket.id(),
            self.sender
        );
    }

    fn send_hello(&mut self, addr: Ipv4Addr) -> Result<()> {
        println!("Pinging {addr} with an ICMP hello...");
        self.start_reliable(
            addr,
            0,
            Body::Hello {
                name: self.cfg.name.clone(),
            },
            None,
        )
    }

    fn start_reliable(
        &mut self,
        dest: Ipv4Addr,
        session: u16,
        body: Body,
        ply: Option<u16>,
    ) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::new(
                "already waiting for an acknowledgement from the peer",
            ));
        }

        let kind = body.kind();
        let seq = self.take_seq();
        let message = Message::new(session, seq, self.sender, body);
        let payload = message.encode();
        if payload.len() > crate::proto::MAX_DATAGRAM {
            return Err(Error::new(
                "protocol message exceeds the maximum datagram size",
            ));
        }
        let wire = self.socket.send_request(dest, seq, &payload)?;
        self.trace_tx(&wire, seq, kind);

        let now = Instant::now();
        self.pending = Some(Pending {
            dest,
            seq,
            payload,
            kind,
            ply,
            first_sent: now,
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

    fn new_session(&self) -> u16 {
        let session = (icmp::random_u32() >> 16) as u16;
        if session == 0 { 0x5043 } else { session }
    }

    fn handle_packet(&mut self, pkt: IcmpPacket) -> Result<()> {
        let message = match Message::decode(&pkt.payload) {
            Ok(message) => message,
            Err(_) => return Ok(()),
        };
        if message.sender == self.sender {
            // Our own echo request, reflected back by loopback.
            return Ok(());
        }
        self.trace_rx(&pkt, &message);

        match self.phase {
            Phase::Listening => self.handle_listening(pkt, message),
            Phase::Joining => self.handle_joining(pkt, message),
            Phase::Playing => self.handle_playing(pkt, message),
        }
    }

    fn handle_listening(&mut self, pkt: IcmpPacket, message: Message) -> Result<()> {
        if pkt.kind != icmp::ECHO_REQUEST || message.kind != Kind::Hello {
            return Ok(());
        }
        if let Some(only) = self.cfg.peer
            && pkt.source != only
        {
            return Ok(());
        }
        let Body::Hello { name } = message.body else {
            return Ok(());
        };
        let name = clean_name(&name);

        let our_color = self.cfg.host_color;
        self.session = self.new_session();
        self.our_color = Some(our_color);
        self.orientation = our_color;
        self.peer = Some(Peer {
            addr: pkt.source,
            sender: message.sender,
            name: name.clone(),
            color: our_color.other(),
        });
        self.phase = Phase::Playing;

        println!();
        println!(
            "ICMP hello from {name} at {} (ttl {}, {} bytes)",
            pkt.source,
            pkt.ttl,
            pkt.raw.len()
        );
        println!(
            "You are {}. {name} is {}.",
            ui::color_name(our_color),
            ui::color_name(our_color.other())
        );

        self.send_hello_ack(&pkt)?;
        self.print_board();
        self.update_prompt()
    }

    fn handle_joining(&mut self, pkt: IcmpPacket, message: Message) -> Result<()> {
        if message.kind != Kind::HelloAck || message.session == 0 {
            return Ok(());
        }
        let Mode::Join(host) = self.cfg.mode else {
            return Ok(());
        };
        if pkt.source != host {
            return Ok(());
        }
        let Body::HelloAck {
            name,
            color: host_color,
        } = message.body
        else {
            return Ok(());
        };
        let name = clean_name(&name);

        let our_color = host_color.other();
        self.session = message.session;
        self.our_color = Some(our_color);
        self.orientation = our_color;
        self.peer = Some(Peer {
            addr: pkt.source,
            sender: message.sender,
            name: name.clone(),
            color: host_color,
        });
        self.phase = Phase::Playing;

        let rtt = self.pending.as_ref().map(|p| p.first_sent.elapsed());
        self.pending = None;

        println!();
        println!("Connected to {name} at {} (ttl {})!", pkt.source, pkt.ttl);
        if let Some(rtt) = rtt {
            println!("Handshake round trip: {}", fmt_duration(rtt));
        }
        println!(
            "You are {}. {name} is {}.",
            ui::color_name(our_color),
            ui::color_name(host_color)
        );

        self.print_board();
        self.update_prompt()
    }

    fn send_hello_ack(&mut self, pkt: &IcmpPacket) -> Result<()> {
        let Some(our_color) = self.our_color else {
            return Ok(());
        };
        let seq = self.take_seq();
        let message = Message::new(
            self.session,
            seq,
            self.sender,
            Body::HelloAck {
                name: self.cfg.name.clone(),
                color: our_color,
            },
        );
        let payload = message.encode();
        let wire = self
            .socket
            .send_reply(pkt.source, pkt.id, pkt.seq, &payload)?;
        self.trace_tx(&wire, seq, Kind::HelloAck);
        Ok(())
    }

    fn handle_playing(&mut self, pkt: IcmpPacket, message: Message) -> Result<()> {
        let Some(peer) = self.peer.as_ref() else {
            return Ok(());
        };
        let (peer_addr, peer_sender) = (peer.addr, peer.sender);

        if pkt.source != peer_addr || message.sender != peer_sender {
            return Ok(());
        }

        if message.kind == Kind::Hello {
            // A retransmitted hello means our hello-ack got lost.
            if matches!(self.cfg.mode, Mode::Host) && pkt.kind == icmp::ECHO_REQUEST {
                self.send_hello_ack(&pkt)?;
            }
            return Ok(());
        }

        if message.session != self.session {
            return Ok(());
        }

        match pkt.kind {
            icmp::ECHO_REQUEST => self.handle_request(pkt, message),
            icmp::ECHO_REPLY => self.handle_reply(message),
            _ => Ok(()),
        }
    }

    fn handle_request(&mut self, pkt: IcmpPacket, message: Message) -> Result<()> {
        match message.body {
            Body::Move { ply, uci, hash } => self.on_move(&pkt, message.seq, ply, &uci, hash),
            Body::Resign { reason } => self.on_resign(&pkt, message.seq, &reason),
            Body::Hello { .. } => {
                self.send_hello_ack(&pkt)?;
                Ok(())
            }
            _ => {
                self.reply_ack(&pkt, message.seq)?;
                Ok(())
            }
        }
    }

    fn on_move(
        &mut self,
        pkt: &IcmpPacket,
        message_seq: u16,
        ply: u16,
        uci: &str,
        hash: u64,
    ) -> Result<()> {
        if ply < self.game.ply() {
            // A retransmission of a move we already applied.
            self.reply_ack(pkt, message_seq)?;
            self.extend_linger();
            return Ok(());
        }

        if ply > self.game.ply() {
            let reason = format!("expected ply {}, got {ply}", self.game.ply());
            self.reply_nack(pkt, message_seq, &reason)?;
            return Err(Error::new(format!("peer is out of sync: {reason}")));
        }

        if self.game.is_over() {
            // We already resigned (or finished); acknowledge so the peer can
            // stop retransmitting, but do not change the result.
            self.reply_ack(pkt, message_seq)?;
            self.extend_linger();
            return Ok(());
        }

        let mut trial = self.game.clone();
        let applied = match trial.apply_uci(uci) {
            Ok(applied) => applied,
            Err(reason) => {
                self.reply_nack(pkt, message_seq, &reason)?;
                return Err(Error::new(format!("peer sent an unusable move: {reason}")));
            }
        };

        if applied.hash != hash {
            let reason = format!(
                "position hash mismatch (computed {:016x}, peer sent {hash:016x})",
                applied.hash
            );
            self.reply_nack(pkt, message_seq, &reason)?;
            return Err(Error::new(format!("desynchronised: {reason}")));
        }

        self.game = trial;

        // Receiving the next move proves that the peer applied our previous
        // move, even if its acknowledgement was lost.
        if self.pending.as_ref().is_some_and(|p| {
            p.kind == Kind::Move && p.ply.is_some_and(|our_ply| our_ply + 1 == ply)
        }) {
            self.pending = None;
            println!("  (peer's reply confirms our last move; ack was lost)");
        }

        self.reply_ack(pkt, message_seq)?;

        let peer_name = self
            .peer
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "peer".into());
        let peer_color = self
            .peer
            .as_ref()
            .map(|p| p.color)
            .unwrap_or_else(|| Color::White);

        println!();
        println!(
            "{} ({}) plays {} [ply {}, {} bytes].",
            peer_name,
            ui::color_name(peer_color),
            applied.san,
            applied.ply + 1,
            pkt.raw.len()
        );
        self.print_board();

        if applied.result.is_some() {
            self.game_over_at = Some(Instant::now());
            self.announce_result();
        } else {
            self.update_prompt()?;
        }
        Ok(())
    }

    fn on_resign(&mut self, pkt: &IcmpPacket, message_seq: u16, _reason: &str) -> Result<()> {
        let Some(peer) = self.peer.as_ref() else {
            return Ok(());
        };
        let peer_color = peer.color;

        if !self.game.is_over() {
            self.game.resign(peer_color);
            self.announce_result();
        }
        self.reply_ack(pkt, message_seq)?;
        // The peer is ending the game; there is no point in retrying our own
        // outstanding message.
        self.pending = None;
        self.game_over_at = Some(Instant::now());
        Ok(())
    }

    fn extend_linger(&mut self) {
        if self.game.is_over() {
            self.game_over_at = Some(Instant::now());
        }
    }

    fn handle_reply(&mut self, message: Message) -> Result<()> {
        match message.body {
            Body::Ack { acked } => self.on_ack(acked),
            Body::Nack { acked, reason } => {
                if self.pending.as_ref().is_some_and(|p| p.seq == acked) {
                    let kind = self.pending.as_ref().map(|p| p.kind).unwrap_or(Kind::Move);
                    self.pending = None;
                    return Err(Error::new(format!(
                        "peer rejected our {}: {}",
                        kind.name(),
                        ui::sanitize(&reason)
                    )));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn on_ack(&mut self, acked: u16) -> Result<()> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        if pending.seq != acked {
            self.pending = Some(pending);
            return Ok(());
        }

        let rtt = pending.first_sent.elapsed();
        match pending.kind {
            Kind::Move => println!("  ack for move in {} (ICMP round trip)", fmt_duration(rtt)),
            Kind::Resign => println!("  resignation acknowledged in {}", fmt_duration(rtt)),
            _ => {}
        }

        if pending.kind == Kind::Resign || self.quit_requested {
            self.exit = true;
        }

        self.update_prompt()
    }

    fn reply_ack(&mut self, pkt: &IcmpPacket, acked: u16) -> Result<()> {
        let seq = self.take_seq();
        let message = Message::new(self.session, seq, self.sender, Body::Ack { acked });
        let payload = message.encode();
        let wire = self
            .socket
            .send_reply(pkt.source, pkt.id, pkt.seq, &payload)?;
        self.trace_tx(&wire, seq, Kind::Ack);
        Ok(())
    }

    fn reply_nack(&mut self, pkt: &IcmpPacket, acked: u16, reason: &str) -> Result<()> {
        let seq = self.take_seq();
        let message = Message::new(
            self.session,
            seq,
            self.sender,
            Body::Nack {
                acked,
                reason: reason.to_string(),
            },
        );
        let payload = message.encode();
        let wire = self
            .socket
            .send_reply(pkt.source, pkt.id, pkt.seq, &payload)?;
        self.trace_tx(&wire, seq, Kind::Nack);
        Ok(())
    }

    fn play_local(&mut self, text: &str) -> Result<bool> {
        self.prompted = false;
        if !self.can_play_now() {
            println!("It is not your turn.");
            return Ok(false);
        }

        let applied = match self.game.apply_input(text) {
            Ok(applied) => applied,
            Err(reason) => {
                println!("{reason}");
                return Ok(false);
            }
        };

        let our_color = self
            .our_color
            .ok_or_else(|| Error::new("internal error: no color assigned"))?;
        let dest = self
            .peer
            .as_ref()
            .map(|p| p.addr)
            .ok_or_else(|| Error::new("internal error: no peer"))?;

        self.start_reliable(
            dest,
            self.session,
            Body::Move {
                ply: applied.ply,
                uci: applied.uci.to_string(),
                hash: applied.hash,
            },
            Some(applied.ply),
        )?;

        println!();
        println!(
            "You ({}) play {} [ply {}]. Sending as an ICMP echo...",
            ui::color_name(our_color),
            applied.san,
            applied.ply + 1
        );
        self.print_board();

        if applied.result.is_some() {
            self.game_over_at = Some(Instant::now());
            self.announce_result();
        }
        Ok(true)
    }

    fn resign(&mut self) -> Result<()> {
        if self.phase != Phase::Playing || self.our_color.is_none() {
            println!("Not in a game.");
            return Ok(());
        }
        if self.game.is_over() {
            println!("The game is already over.");
            return Ok(());
        }
        if self.pending.is_some() {
            println!("Wait for the pending acknowledgement first.");
            return Ok(());
        }

        let our_color = self.our_color.unwrap();
        let dest = self.peer.as_ref().unwrap().addr;
        self.game.resign(our_color);
        self.start_reliable(
            dest,
            self.session,
            Body::Resign {
                reason: "resigned".into(),
            },
            None,
        )?;

        self.game_over_at = Some(Instant::now());
        self.announce_result();
        Ok(())
    }

    fn quit(&mut self) -> Result<()> {
        if self.phase != Phase::Playing || self.game.is_over() || self.our_color.is_none() {
            self.exit = true;
            return Ok(());
        }
        if self.pending.is_some() {
            println!("Still waiting for an acknowledgement; press Ctrl-C to force quit.");
            return Ok(());
        }
        self.quit_requested = true;
        self.resign()
    }

    fn can_play_now(&self) -> bool {
        self.phase == Phase::Playing
            && !self.game.is_over()
            && self.pending.is_none()
            && self.our_color == Some(self.game.turn())
    }

    fn update_prompt(&mut self) -> Result<()> {
        if self.exit {
            return Ok(());
        }
        let Some(our_color) = self.our_color else {
            return Ok(());
        };
        if self.phase != Phase::Playing
            || self.game.is_over()
            || self.pending.is_some()
            || self.game.turn() != our_color
        {
            return Ok(());
        }

        while let Some(line) = self.queued.pop_front() {
            if self.play_local(&line)? {
                return Ok(());
            }
        }

        if !self.prompted {
            print!("{}", ui::prompt(our_color));
            io::stdout().flush()?;
            self.prompted = true;
        }
        Ok(())
    }

    fn handle_line(&mut self, line: &str) -> Result<()> {
        let line = line.trim();
        if line.is_empty() {
            self.prompted = false;
            return self.update_prompt();
        }

        match line.to_ascii_lowercase().as_str() {
            "board" | "b" => self.print_board(),
            "moves" | "legal" => self.print_legal(),
            "history" | "log" => {
                let movetext = self.game.movetext();
                if movetext.is_empty() {
                    println!("No moves yet.");
                } else {
                    println!("{movetext}");
                }
            }
            "fen" => println!("{}", self.game.fen()),
            "pgn" => self.print_pgn(),
            "flip" => {
                self.orientation = self.orientation.other();
                self.print_board();
            }
            "resign" => self.resign()?,
            "quit" | "exit" => {
                self.quit()?;
                return Ok(());
            }
            "help" | "?" => println!("{}", ui::HELP),
            "trace" => {
                self.cfg.trace = !self.cfg.trace;
                println!("packet trace {}", if self.cfg.trace { "on" } else { "off" });
            }
            _ => {
                if self.game.is_over() {
                    println!("The game is over.");
                } else if self.can_play_now() {
                    self.play_local(line)?;
                } else if self.queued.len() < 8 {
                    self.queued.push_back(line.to_string());
                    println!("queued move '{line}'");
                } else {
                    println!("move queue is full");
                }
            }
        }

        self.prompted = false;
        self.update_prompt()
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

    fn print_board(&self) {
        let style = ui::BoardStyle {
            unicode: !self.cfg.ascii,
            color: self.cfg.color,
            orientation: self.orientation,
        };
        print!(
            "\n{}",
            ui::render_board(self.game.position(), self.game.last_move(), &style,)
        );
        if self.game.is_over() {
            println!("Game over.");
        } else if let Some(our_color) = self.our_color {
            let turn = self.game.turn();
            let side = if turn == our_color { "you" } else { "opponent" };
            let check = if self.game.position().is_check() {
                " (check)"
            } else {
                ""
            };
            println!("{} to move: {}{}", ui::color_name(turn), side, check);
        }
    }

    fn print_pgn(&self) {
        let Some(our_color) = self.our_color else {
            println!("Not in a game.");
            return;
        };
        let peer_name = self
            .peer
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or("opponent");
        let (white, black) = if our_color == Color::White {
            (self.cfg.name.as_str(), peer_name)
        } else {
            (peer_name, self.cfg.name.as_str())
        };
        let result = self.game.result().map_or("*", |result| result.pgn());

        println!("[Event \"pingchess\"]");
        println!("[White \"{white}\"]");
        println!("[Black \"{black}\"]");
        println!("[Result \"{result}\"]");
        println!();
        println!("{} {result}", self.game.movetext());
    }

    fn print_legal(&self) {
        let moves = self.game.legal_moves_san();
        if moves.is_empty() {
            println!("No legal moves.");
            return;
        }
        println!("Legal moves:");
        for chunk in moves.chunks(4) {
            let line = chunk
                .iter()
                .map(|(san, uci)| format!("{san:<7} ({uci})"))
                .collect::<Vec<_>>()
                .join("  ");
            println!("  {line}");
        }
    }

    fn announce_result(&mut self) {
        if self.result_printed {
            return;
        }
        let Some(result) = self.game.result() else {
            return;
        };
        self.result_printed = true;

        let our_color = self.our_color.unwrap_or(Color::White);
        let peer_name = self
            .peer
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "peer".into());

        println!();
        println!("{}", ui::result_line(result, our_color, &peer_name));
        let movetext = self.game.movetext();
        if !movetext.is_empty() {
            println!("{movetext}");
        }
        println!("Result: {}", result.pgn());
    }

    fn poll_timeout(&self) -> Duration {
        let now = Instant::now();
        let mut timeout = Duration::from_millis(250);

        if let Some(pending) = &self.pending {
            timeout =
                timeout.min((pending.last_sent + self.cfg.timeout).saturating_duration_since(now));
        }
        if let Some(since) = self.game_over_at
            && self.pending.is_none()
        {
            timeout = timeout.min((since + self.linger()).saturating_duration_since(now));
        }

        timeout.max(Duration::from_millis(1))
    }

    /// Stay alive briefly after the result, so that a peer whose
    /// acknowledgement was lost can retransmit and still get an answer.
    fn linger(&self) -> Duration {
        (self.cfg.timeout + Duration::from_millis(250))
            .max(MIN_LINGER)
            .min(MAX_LINGER)
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

    fn drain_socket(&mut self) -> Result<()> {
        while let Some(pkt) = self.socket.recv()? {
            self.handle_packet(pkt)?;
        }
        Ok(())
    }

    fn on_timers(&mut self) -> Result<()> {
        let now = Instant::now();

        if let Some(pending) = &self.pending
            && now.saturating_duration_since(pending.last_sent) >= self.cfg.timeout
        {
            if pending.attempts > self.cfg.retries {
                let kind = pending.kind;
                let attempts = pending.attempts;
                self.pending = None;
                return Err(Error::new(format!(
                    "{} was not acknowledged after {} attempts; is ICMP blocked?",
                    kind.name(),
                    attempts
                )));
            }

            let dest = pending.dest;
            let seq = pending.seq;
            let payload = pending.payload.clone();
            let kind = pending.kind;

            let wire = self.socket.send_request(dest, seq, &payload)?;
            self.trace_tx(&wire, seq, kind);

            if let Some(pending) = &mut self.pending {
                pending.attempts += 1;
                pending.last_sent = now;
                println!(
                    "  no acknowledgement yet; retransmitted {} (retry {}/{})",
                    kind.name(),
                    pending.attempts - 1,
                    self.cfg.retries
                );
            }
        }

        if let Some(since) = self.game_over_at
            && self.pending.is_none()
            && now.saturating_duration_since(since) >= self.linger()
        {
            self.exit = true;
        }

        Ok(())
    }

    fn trace_tx(&self, wire: &[u8], seq: u16, kind: Kind) {
        if !self.cfg.trace {
            return;
        }
        let ty = if wire.first() == Some(&icmp::ECHO_REQUEST) {
            "echo request"
        } else {
            "echo reply"
        };
        println!(
            "  icmp tx {ty} seq={seq} [{}] {} B: {}",
            kind.name(),
            wire.len(),
            ui::hex_bytes(wire)
        );
    }

    fn trace_rx(&self, pkt: &IcmpPacket, message: &Message) {
        if !self.cfg.trace {
            return;
        }
        let ty = if pkt.kind == icmp::ECHO_REQUEST {
            "echo request"
        } else {
            "echo reply"
        };
        println!(
            "  icmp rx {ty} from {} seq={} ttl={} [{}] {} B: {}",
            pkt.source,
            pkt.seq,
            pkt.ttl,
            message.kind.name(),
            pkt.raw.len(),
            ui::hex_bytes(&pkt.raw)
        );
    }
}

fn clean_name(name: &str) -> String {
    let name = ui::sanitize(name).trim().to_string();
    if name.is_empty() {
        "player".into()
    } else {
        name
    }
}

fn fmt_duration(duration: Duration) -> String {
    let millis = duration.as_secs_f64() * 1000.0;
    if millis < 10.0 {
        format!("{millis:.3} ms")
    } else {
        format!("{millis:.2} ms")
    }
}

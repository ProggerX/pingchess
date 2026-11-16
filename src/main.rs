mod app;
mod game;
mod hub;
mod hub_proto;
mod icmp;
mod proto;
mod room_client;
mod ui;

use std::io::IsTerminal;
use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::time::Duration;

use shakmaty::Color;

use app::Mode;

const DEFAULT_MOTD: &str =
    "Welcome to pingchess rooms! /join #lobby, /new <nick> to play, /help for commands.";

const USAGE: &str = "\
pingchess - chess and IRC-like rooms over ICMP

USAGE:
  pingchess host [OPTIONS]           wait for one direct peer
  pingchess join <IPV4> [OPTIONS]    connect to a direct peer
  pingchess serve [OPTIONS]          run an IRC-like rooms hub
  pingchess connect <IPV4> [OPTIONS] join a rooms hub

OPTIONS:
  --name <NAME>        player name, or hub name for 'serve'
  --nick <NICK>        alias for --name (rooms client)
  --as <white|black>   host color (host only; default white)
  --peer <IPV4>        host only: accept only this direct peer
  --room <ROOM>        rooms client: room to join (default #lobby)
  --motd <TEXT>        serve: message of the day
  --timeout <MS>       acknowledgement timeout (default 2000)
  --retries <N>        retransmissions before giving up (default 8)
  --ascii              use ASCII piece letters instead of Unicode
  --no-color           disable ANSI colors
  --trace              print hex dumps of ICMP packets
  -h, --help           show this help
  -V, --version        show the version

DIRECT GAMES:
  Two peers, one game, moves carried inside ICMP echo requests. The host is
  White by default and the joiner gets the other color.

    pingchess host --name alice
    pingchess join 192.168.1.42 --name bob

ROOMS:
  A hub hosts many named rooms. Each room has chat and can start one game at a
  time; extra members spectate. The hub owns the games, so everyone sees the
  same board.

    pingchess serve --name \"game night\"
    pingchess connect 192.168.1.42 --nick alice
    pingchess connect 192.168.1.42 --nick bob --room #lobby

  In-room commands: /join, /part, /nick, /list, /who, /new, /move, /resign,
  /say, /me, /board, /moves, /history, /pgn, /flip, /help, /quit.

PERMISSIONS:
  Raw ICMP sockets require CAP_NET_RAW. Run as root, or grant the binary:
    sudo setcap cap_net_raw+ep ./target/release/pingchess

  To try everything on one machine without privileges, use a private user and
  network namespace:
    unshare -Urn sh -c 'ip link set lo up; target/release/pingchess ...'";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("pingchess: {message}");
            ExitCode::FAILURE
        }
    }
}

enum Command {
    Host,
    Join(Ipv4Addr),
    Serve,
    Connect(Ipv4Addr),
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }

    let command = args.remove(0);
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(());
    }
    if matches!(command.as_str(), "-V" | "--version") {
        println!("pingchess {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let command = match command.as_str() {
        "host" => Command::Host,
        "join" => Command::Join(parse_address(&mut args, "join")?),
        "serve" | "hub" => Command::Serve,
        "connect" | "irc" => Command::Connect(parse_address(&mut args, "connect")?),
        other => {
            return Err(format!(
                "unknown command '{other}' (try 'pingchess --help')"
            ));
        }
    };

    let mut name: Option<String> = None;
    let mut host_color = Color::White;
    let mut peer_filter: Option<Ipv4Addr> = None;
    let mut ascii = false;
    let mut no_color = false;
    let mut timeout_ms: u64 = 2000;
    let mut retries: u32 = 8;
    let mut trace = false;
    let mut room = "#lobby".to_string();
    let mut motd = DEFAULT_MOTD.to_string();

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].clone();
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (arg.as_str(), None),
        };

        match key {
            "--name" | "--nick" => {
                name = Some(option_value(&args, &mut index, inline, key)?);
            }
            "--as" => {
                if !matches!(command, Command::Host) {
                    return Err("--as is only used by 'host'".into());
                }
                host_color = parse_color(&option_value(&args, &mut index, inline, key)?)?;
            }
            "--peer" => {
                if !matches!(command, Command::Host) {
                    return Err("--peer is only used by 'host'".into());
                }
                let value = option_value(&args, &mut index, inline, key)?;
                peer_filter = Some(
                    value
                        .parse()
                        .map_err(|_| format!("'{value}' is not an IPv4 address"))?,
                );
            }
            "--room" => {
                if !matches!(command, Command::Connect(_)) {
                    return Err("--room is only used by 'connect'".into());
                }
                room = option_value(&args, &mut index, inline, key)?;
            }
            "--motd" => {
                if !matches!(command, Command::Serve) {
                    return Err("--motd is only used by 'serve'".into());
                }
                motd = option_value(&args, &mut index, inline, key)?;
            }
            "--timeout" => {
                let value = option_value(&args, &mut index, inline, key)?;
                timeout_ms = value
                    .parse()
                    .map_err(|_| format!("'{value}' is not a number of milliseconds"))?;
                if !(100..=60_000).contains(&timeout_ms) {
                    return Err("--timeout must be between 100 and 60000 ms".into());
                }
            }
            "--retries" => {
                let value = option_value(&args, &mut index, inline, key)?;
                retries = value
                    .parse()
                    .map_err(|_| format!("'{value}' is not a number of retries"))?;
                if !(1..=100).contains(&retries) {
                    return Err("--retries must be between 1 and 100".into());
                }
            }
            "--ascii" => ascii = true,
            "--no-color" => no_color = true,
            "--trace" => trace = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("pingchess {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => return Err(format!("unknown option '{other}'")),
        }

        index += 1;
    }

    let color =
        !no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();

    match command {
        Command::Host | Command::Join(_) => {
            let name = resolve_player_name(name);
            let mode = match command {
                Command::Host => Mode::Host,
                Command::Join(peer) => Mode::Join(peer),
                _ => unreachable!(),
            };
            app::run(app::Config {
                mode,
                name,
                host_color,
                peer: peer_filter,
                ascii,
                color,
                timeout: Duration::from_millis(timeout_ms),
                retries,
                trace,
            })
            .map_err(|err| err.to_string())
        }
        Command::Serve => {
            let hub_name = name
                .map(|name| ui::sanitize(name.trim()))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "pingchess hub".into());
            hub::run(hub::Config {
                hub_name,
                motd,
                trace,
            })
            .map_err(|err| err.to_string())
        }
        Command::Connect(hub) => {
            let nick = resolve_player_name(name);
            room_client::run(room_client::Config {
                hub,
                nick,
                room,
                ascii,
                color,
                timeout: Duration::from_millis(timeout_ms),
                retries,
                trace,
            })
            .map_err(|err| err.to_string())
        }
    }
}

fn parse_address(args: &mut Vec<String>, command: &str) -> Result<Ipv4Addr, String> {
    let Some(peer) = args.first() else {
        return Err(format!("'{command}' needs an IPv4 address"));
    };
    let address: Ipv4Addr = peer
        .parse()
        .map_err(|_| format!("'{peer}' is not an IPv4 address"))?;
    args.remove(0);
    Ok(address)
}

fn resolve_player_name(name: Option<String>) -> String {
    let name = name.unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "player".into()));
    let name = ui::sanitize(name.trim());
    if name.is_empty() {
        "player".into()
    } else {
        name
    }
}

fn option_value(
    args: &[String],
    index: &mut usize,
    inline: Option<&str>,
    key: &str,
) -> Result<String, String> {
    if let Some(value) = inline {
        return Ok(value.to_string());
    }
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{key} needs a value"))
}

fn parse_color(value: &str) -> Result<Color, String> {
    match value.to_ascii_lowercase().as_str() {
        "white" | "w" => Ok(Color::White),
        "black" | "b" => Ok(Color::Black),
        _ => Err(format!("'{value}' is not a color (use white or black)")),
    }
}

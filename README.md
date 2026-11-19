# pingchess

> [!WARNING]
> **Vibecoded.** This project was built largely by an AI coding agent and has
> not been security-audited. Raw ICMP, unauthenticated games and rooms, use at
> your own risk. See [VIBECODED.md](VIBECODED.md).

**Chess over ICMP echo packets.** No TCP, no UDP, no ports — every move travels
inside a ping.

There are two ways to play:

- **Direct**: two peers, one game, with the host/join commands.
- **Rooms**: an IRC-like hub with named rooms, nicknames, chat, spectators, and
  games started on demand in a room.

The rules engine is fully aware of castling, en passant, promotion, checkmate,
stalemate, the fifty-move rule, threefold repetition, and insufficient material.

```text
$ pingchess host --name alice
pingchess 0.1.0 - chess over ICMP echo packets
local ICMP id 0xadd7, sender id 0x4d9fd263
Listening for an ICMP hello...

$ pingchess join 192.168.1.42 --name bob
Connected to alice at 192.168.1.42 (ttl 64)!
Handshake round trip: 0.187 ms
You are Black. alice is White.

    h  g  f  e  d  c  b  a
 1  R  N  B  K  Q  B  N  R  1
 ...
```

## Quick start

### Two machines

Both peers need raw ICMP access (`CAP_NET_RAW`). Either run as root or grant the
capability once:

```sh
cargo build --release
sudo setcap cap_net_raw+ep target/release/pingchess

# machine A
target/release/pingchess host --name alice

# machine B
target/release/pingchess join <machine-A-ip> --name bob
```

The host is White by default; use `--as black` to swap. Make sure ICMP is not
blocked between the two machines (`ping <host>` should work).

### Rooms

The rooms hub is a single process; clients connect to it, join named rooms,
chat, and start games with each other.

```sh
# machine A
target/release/pingchess serve --name "game night"

# machines B and C
target/release/pingchess connect <machine-A-ip> --nick alice
target/release/pingchess connect <machine-A-ip> --nick bob
```

Inside a client:

```text
alice> /join #lobby
alice> /new bob
alice> e4
bob> e5
bob> nice move
```

A room can hold any number of members. The two selected players play; everyone
else is a spectator and sees the same live board. Rooms are created on demand
and disappear when they become empty (except `#lobby`).

### One machine, no root

Unprivileged user+network namespaces give us a private loopback with
`CAP_NET_RAW`, which is enough for genuine multi-process ICMP games:

```sh
./scripts/demo.sh          # direct game: scripted Fool's Mate
./scripts/demo-rooms.sh    # hub + two clients, chat and Fool's Mate
./scripts/test-icmp.sh     # end-to-end tests in a private netns
```

Or run it yourself:

```sh
cargo build
unshare -Urn sh -c '
  ip link set lo up
  ./target/debug/pingchess host --name alice &
  sleep 0.2
  ./target/debug/pingchess join 127.0.0.1 --name bob
'
```

## Nix

A flake is included, using [crane](https://crane.dev/), with a committed
`flake.lock`:

```sh
nix build                 # ./result/bin/pingchess
nix run . -- serve        # run a rooms hub
nix develop               # Rust + iproute2, util-linux and libcap

nix shell github:ProggerX/pingchess
pingchess --help
```

The packaged binary still needs `CAP_NET_RAW` for real ICMP. Running it through
sudo needs the absolute store path, because sudo resets `PATH`:

```sh
sudo "$(command -v pingchess)" connect 85.198.71.4 --nick nickname
```

Alternatively copy it out of the read-only Nix store and grant the capability:

```sh
install -m755 "$(command -v pingchess)" ./pingchess
sudo setcap cap_net_raw+ep ./pingchess
./pingchess connect 85.198.71.4 --nick nickname
```

The namespace demos (`scripts/demo*.sh`) need neither root nor setcap.
`hydraJobs` exposes `pingchess`, `clippy`, and `fmt` jobs for `x86_64-linux`.

> If you are on a revision from before the lock was committed, Nix may try to
> write `flake.lock` into the read-only store. Update the checkout, or pass
> `--no-write-lock-file` to the Nix command.

## Playing

Moves can be typed as **UCI** (`e2e4`, `e7e8q`, castling as `e1g1`) or **SAN**
(`e4`, `Nf3`, `O-O`, `exd5`). Type-ahead works: moves entered while waiting are
queued for your next turn.

### Direct game commands

| Command | Meaning |
| --- | --- |
| `board`, `b` | show the board |
| `moves`, `legal` | list legal moves in SAN and UCI |
| `history`, `log` | show the move list |
| `fen` | show the current position as FEN |
| `pgn` | show the game as PGN |
| `flip` | flip the board |
| `resign` | resign |
| `trace` | toggle hex dumps of the ICMP packets |
| `quit`, `exit` | resign if needed, then quit |
| `help`, `?` | show help |

### Room commands

In a room, plain lines are chat unless they look like a chess move. Move-like
lines are played on your turn and queued while you wait; while spectating or
after the game they are treated as chat. `/say` always forces chat.

| Command | Meaning |
| --- | --- |
| `/join <room>` | join or create a room (e.g. `/join #lobby`) |
| `/part` | leave the current room |
| `/nick <nick>` | change your nick |
| `/list` | list rooms |
| `/who`, `/names` | list members and roles |
| `/new [nick]` | start a game in the room; you are White |
| `/move <move>` | force a move |
| `/resign` | resign |
| `/say <text>` | send chat even if the text looks like a move |
| `/me <text>` | send an emote |
| `/board`, `/b` | show the board |
| `/moves` | list legal moves |
| `/history` | show the move list |
| `/pgn` | show the game as PGN |
| `/flip` | flip the board |
| `/help`, `/?` | show help |
| `/quit` | leave the hub and quit |

## How it works

### Handshake and moves

```text
joiner                                      host
  │ ── hello (ICMP echo request) ──────────▶ │
  │ ◀─ hello-ack (ICMP echo reply) ────────── │   session id, host color
  │                                           │
  │ ── move (echo request, ply + UCI + hash) ▶│
  │ ◀─ ack (echo reply) ───────────────────── │
  │                                           │
  │ ◀─ move ───────────────────────────────── │
  │ ── ack ─────────────────────────────────▶ │
```

The protocol is stop-and-wait: each reliable message has a sequence number and
is retransmitted until an `ack` comes back. Duplicate moves are recognized by
their ply and acknowledged again instead of being replayed. Every move also
carries an FNV-1a hash of the resulting FEN, so if the two positions ever
diverge, the receiver NACKs and both sides stop rather than playing different
games.

The complete message format is deliberately tiny:

```text
 0        4      5      6      8      10            14
 +--------+------+------+------+------+-------------+---------+
 | "PCHS" | ver  | kind | sess | seq  | sender (32) | body... |
 +--------+------+------+------+------+-------------+---------+
```

`kind` is `hello`, `hello-ack`, `move`, `resign`, `ack`, or `nack`. Bodies are
length-prefixed strings plus the move fields; see [`src/proto.rs`](src/proto.rs).
`--trace` prints both the protocol fields and the complete ICMP bytes:

```text
icmp tx echo request seq=2 [move] 37 B:
  08 00 a5 b1 de a3 00 02 50 43 48 53 01 03 b8 9b 00 02 ... 66 32 66 33
  └ ICMP ┘ └ id ┘ └seq┘ └ magic ┘ v  kind sess  seq   sender   "f2f3"
```

### The rooms hub

Room mode uses a separate protocol (`PCHR`) and a star topology:

```text
alice ── echo request (action, last event) ──▶ hub
      ◀─ echo reply (ack + event batch) ──────
bob   ── echo request ───────────────────────▶ hub
      ◀─ echo reply ─────────────────────────
```

The hub is authoritative for room membership, chat, and every game. Clients
poll twice a second when idle, and each request is stop-and-wait. Actions carry
a sequence number so a retransmitted `join`, `new`, or `move` is applied only
once. Every client has its own monotonically numbered event queue; a request
says which event it last saw, so a lost reply is repaired by resending the
unseen events. Replies are packed up to a safe ICMP size and the rest are
delivered on the next poll.

That means a dropped ping does not drop a chat line or a move: the client asks
again, the hub recognises the duplicate action, and the event queue fills in
the gap. The rooms protocol has also been exercised with 40% packet loss on
loopback.

### ICMP details

- Uses `SOCK_RAW`/`IPPROTO_ICMP`, so it really is ICMP — not a UDP tunnel
  pretending to be one. This also lets both peers *receive* the other's echo
  requests, which an unprivileged Linux "ping socket" cannot do.
- ICMP checksums are generated and verified locally.
- Linux's kernel also answers every echo request automatically. pingchess
  ignores those copies: payloads carry a random sender id, and only messages
  whose body is `ack`/`nack` count as acknowledgements.
- Messages are filtered by source address, session id, sender id, and protocol
  magic.

## CLI

```text
pingchess host [OPTIONS]
pingchess join <IPV4> [OPTIONS]
pingchess serve [OPTIONS]
pingchess connect <IPV4> [OPTIONS]

--name <NAME>        player name, or hub name for 'serve' (default: $USER)
--nick <NICK>        alias for --name
--as <white|black>   host color (default: white); joiner gets the other
--peer <IPV4>        host only: only accept a hello from this address
--room <ROOM>        rooms client: room to join (default: #lobby)
--motd <TEXT>        serve: message of the day
--timeout <MS>       acknowledgement timeout (default: 2000)
--retries <N>        retransmissions before giving up (default: 8)
--ascii              ASCII pieces instead of Unicode
--no-color           disable ANSI colors
--trace              print hex dumps of ICMP packets
```

Colors are enabled automatically on a terminal and can be disabled with
`--no-color` or `NO_COLOR=1`.

## Tests

```sh
cargo test                 # chess rules, both protocols, checksum, rendering
scripts/test-icmp.sh       # real end-to-end games over loopback ICMP
```

The end-to-end tests run:

- two direct peers through Fool's Mate in both color assignments;
- a hub with two clients: join, start a game, play Fool's Mate, exchange chat,
  and quit.

The direct stop-and-wait logic was exercised with 60% packet loss on loopback,
and the rooms protocol with 40% loss (`tc qdisc ... netem loss ...`).

## Limitations

- Linux is the tested platform (raw sockets, `poll`, `/dev/urandom`).
- IPv4 only.
- No draw-by-agreement or time control (automatic draws are implemented).
- No engine; this is a two-player game.
- A rooms client is in one room at a time.
- One rooms hub per IPv4 address: ICMP has no port number to distinguish two
  hubs on the same host.
- Rooms have no private messages, topics, or operator commands beyond the hub
  console (`rooms`, `clients`, `motd`, `quit`). If the hub restarts, clients
  must reconnect.
- Not authenticated or encrypted. Anyone who can send you ICMP can offer a
  game or join a hub; use `--peer <IPV4>` on a direct host to restrict who may
  connect. A hub trusts its network.
- Raw ICMP is restricted to `CAP_NET_RAW`. Some networks block ICMP entirely.

## Layout

```text
src/main.rs        CLI for direct games, hubs, and room clients
src/app.rs         direct peer session state machine and event loop
src/hub.rs         rooms hub: clients, rooms, games, event queues
src/room_client.rs rooms client: commands, polling, local board
src/icmp.rs        raw ICMP socket, checksum, echo request/reply
src/proto.rs       direct peer protocol encode/decode
src/hub_proto.rs   rooms protocol encode/decode
src/game.rs        rules and game state (shakmaty)
src/ui.rs          board rendering and result text
tests/e2e.rs       real processes over loopback ICMP
scripts/           demo.sh, demo-rooms.sh, test-icmp.sh
```

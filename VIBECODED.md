# Vibecoded

This repository is **vibecoded**.

It was built largely by an AI coding agent from conversational prompts. Treat
it accordingly:

- It has **not** been security-audited.
- It has **not** been reviewed by a human line-by-line.
- Games and rooms are **unauthenticated**; anyone who can send ICMP to your
  host can offer a match, join a hub, chat, or spoof packets.
- `pingchess` opens raw ICMP sockets (`CAP_NET_RAW`) and deliberately uses ICMP
  echo requests/replies as its transport. Run it only on networks where you are
  allowed to send raw ICMP.
- The test suite covers the rules engine, both wire protocols, checksums, and
  real end-to-end games on loopback, but that is not a proof of correctness.
- No warranty. Use at your own risk.

If you need this on an untrusted network, review the code, add authentication
and rate limiting, and sandbox the process first.

# DeetsAirplay

Windows system audio to a HomePod, from the tray. Left-click the tray icon
for a small frameless panel — speakers, connection, transport — right-click
to reconnect to the last speaker or quit. The AirPlay 2 stack is written
from scratch in Rust: mDNS discovery, HomeKit transient pairing (SRP-6a,
PIN 3939), the ChaCha20-Poly1305 control channel, uncompressed ALAC in
encrypted RTP, NTP timing. Three crates supply the arithmetic
(`num-bigint`, `sha2`, `chacha20poly1305`); everything the speaker actually
talks to is in this repo.

## What it does

- **Speakers.** Every AirPlay receiver on the LAN, rescanned each time the
  panel opens. Click to pick, Connect (or double-click) to stream.
- **Connection.** The live line: buffer, round trip, uptime, resend
  requests. Buffer is *Auto* (settles from the measured round trip) or
  *Fixed*; *Offset* adds delay for lip-sync with a screen.
- **Transport.** Previous / play-pause / next drive the Windows media
  session (media keys as a fallback), with the current title and artist
  above them; the volume slider sets the HomePod's own volume.
- **Launch at startup**, via the per-user `Run` registry key.

## Routing

DeetsAirplay taps what Windows is already playing (WASAPI loopback), so your
headphones or speakers keep playing too and Windows keeps naming them as
the output. A dedicated virtual output device that makes the HomePod the
real destination is the next structural step; see
[`docs/roadmap.md`](docs/roadmap.md).

## Requirements

- The HomePod's **Allow Speaker & TV Access** set to *Everyone* or *Anyone
  on the Same Network* (Home app). No Apple developer account is involved.
- Windows Firewall allowing inbound UDP to the app (the speaker sends
  timing requests to us). The installed app asks once, with a UAC prompt,
  to add that rule on its first run; a dev build needs it added by hand.

## Latency

AirPlay 2 realtime streams carry a receiver-side buffer the sender chooses;
the protocol floor is 250 ms and the HomePod adds its own output path on
top. So "low latency" here means music-tight and video-usable with the
Offset slider, not gaming-grade. See `docs/architecture.md` → *Latency
policy*.

## Protocol

[`docs/protocol.md`](docs/protocol.md) is the byte-level recipe — pairing,
key derivation, the SETUP plists, packet layouts, timing — with the gotchas
called out. If you are writing your own sender, that document is the useful
part of this repo.

## Run

```bash
npm install
npm run tauri dev
```

First run compiles Rust and is slow. The app starts hidden in the tray.
Before trusting the panel, prove the path from a console:

```bash
cd src-tauri
cargo run --bin probe -- discover
cargo run --bin probe -- tone <homepod-ip>
cargo run --bin probe -- capture <homepod-ip>
```

`npm run release` produces an NSIS installer under
`src-tauri/target/release/bundle/nsis/`.

## Family

Part of the Deets family and sharing its design language:
[DeetsMusic](https://github.com/deets-137/DeetsMusic) (the token system
originates there), [DeetsRGB](https://github.com/deets-137/DeetsRGB) (the
tray shell), [DeetsBeats](https://github.com/deets-137/DeetsBeats),
[DeetsSolutions](https://github.com/deets-137/DeetsSolutions),
[DeetsFilm](https://github.com/deets-137/DeetsFilm),
[DeetsSQL](https://github.com/deets-137/DeetsSQL). Press skin only, theme
under the family's `deets.theme` key.

# DeetsAirplay — project guide for Codex

A tray-first AirPlay 2 sender for Windows 11 (Tauri v2 + WebView2, vanilla TS
front-end, Rust back-end). It captures what Windows is playing (WASAPI
loopback) and streams it to a HomePod with a hand-rolled AirPlay 2 stack:
mDNS, RTSP, HAP transient pairing (SRP, PIN 3939), ChaCha20-framed control
channel, uncompressed ALAC over encrypted RTP, NTP timing.

`README.md` is the cold start. `docs/protocol.md` is the wire recipe; read it
before touching `src-tauri/src/airplay/` or `crypto/`. `docs/architecture.md`
describes what exists. `PLAN.md` holds only what is *not* built.

## Never

- **Never HKDF the audio key.** `shk` is the first 32 bytes of the SRP
  session key K, raw. The control and event channels DO use HKDF. Mixing
  the two gives a session that pairs, connects, and plays silence.
- **Never add headers to the event-channel 200.** `Server` and `CSeq` only.
  `Content-Length: 0` or `Audio-Latency` corrupt the receiver's realtime
  timeline. Same for the control channel's replies to receiver requests.
- **Never send PTP or SETPEERS.** We advertise `timingProtocol: NTP`; PTP
  needs privileged UDP 319/320 and is for multi-room.
- **Never stall the pacer.** The RTP timeline must advance at 44.1 kHz
  whether or not capture has data; send silence rather than nothing.
  A stalled timeline is how a receiver decides the stream is dead.
- **Never block a Tauri command on the network from the main thread.**
  Scan, connect, disconnect and status are `async` and run in
  `spawn_blocking`; a synchronous command freezes the panel.
- **Never write a hex code, radius, font, or duration into a component
  rule.** Colors route through the theme tier, geometry/type/motion through
  the skin tier; same discipline as DeetsMusic, DeetsRGB, DeetsSQL,
  DeetsFilm. Every component must survive all 6 themes.
- **Never add a dependency without asking.** Rust has `tauri`, `serde`,
  `serde_json`, and exactly three primitives — `num-bigint` (SRP modpow),
  `sha2`, `chacha20poly1305` — plus `windows` for OS bindings. Everything
  protocol-shaped is hand-rolled on purpose. The front-end has
  `@tauri-apps/api`.
- **Never rename a theme id without a `RETIRED` entry.** `deets.theme` is
  shared across the family; a rename lands in `src/theme.ts`, the pre-paint
  script in `index.html`, *and* the sibling repos.

## Ported code

The token CSS (`palette.css`, `themes.css`, `skin.css`, `fonts.css`), the
fonts, `theme.ts`, and the titlebar/menu/tray plumbing are **copied** from
`../DeetsRGB` (which copied them from `../DeetsMusic`). Press skin only.
DeetsAirplay's own tokens (`--control-h`, `--speaker-row-h`,
`--transport-*`, `--range-*`, `--status-dot`, `--row-label-w`) sit at the end
of the base block in `skin.css`.

## How to verify your work

- **The user runs the app and tests your changes** (`npm run tauri dev`) with
  the actual HomePod on the actual network. Do NOT build harnesses, mock
  receivers, or a test suite.
- Cheap checks that ARE worth running: `npx tsc --noEmit`, `npx vite build`,
  `cd src-tauri && cargo check --bin probe --bin deetsairplay`.
- When changing anything in the handshake or packet path, run the probe
  first: `cargo run --bin probe -- tone <ip>` prints every RTSP exchange
  and streams a 440 Hz sine. `capture <ip>` does the same with WASAPI.
  `discover` lists speakers with their TXT records.
- Windows Firewall: the HomePod sends unsolicited UDP to our timing and
  control ports. A dev build prompts the first time; the installer will need
  a rule (PLAN.md).

## Working style

- **The user directs the architecture.** For anything non-trivial, talk it
  through first, surface the real forks (he responds well to multiple
  choice), confirm, then build.
- **Do not delegate to subagents.** Read and edit the codebase directly.
- Commit only when asked. Trailer:
  `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`

## Run

```
npm install
npm run tauri dev     # compiles Rust (first run slow); the app starts in the TRAY
npx tsc --noEmit
cd src-tauri && cargo check --bin probe --bin deetsairplay
cd src-tauri && cargo run --bin probe -- discover
```

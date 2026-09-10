# DeetsAirplay — project guide for Claude

A tray-first AirPlay 2 sender for Windows 11 (Tauri v2 + WebView2, vanilla TS
front-end, Rust back-end). It captures what Windows is playing (WASAPI
loopback) and streams it to a HomePod with a hand-rolled AirPlay 2 stack:
mDNS, RTSP, HAP transient pairing (SRP, PIN 3939), ChaCha20-framed control
channel, uncompressed ALAC over encrypted RTP, NTP timing.

`README.md` is the cold start. `docs/protocol.md` is the wire recipe; read it
before touching `crates/airplay/src/airplay/` or `crypto/`. **The sender is a
library crate, `crates/airplay` (`deets-airplay`), shared with DeetsMusic**
(which depends on it by git, rev-pinned): `airplay/`, `crypto/`, `capture.rs`.
`src-tauri/` is only the tray app on top of it. A change to the crate is a
change to both apps; bump DeetsMusic's `rev` after pushing. `docs/architecture.md`
describes what exists. `PLAN.md` holds only what is *not* built, and
`docs/roadmap.md` holds the one big item: audio routing (the WASAPI tap
today vs. a from-scratch virtual output driver, which he also wants for a
Discord-bot project).

## Never

- **Never HKDF the audio key.** `shk` is the first 32 bytes of the SRP
  session key K, raw. The control and event channels DO use HKDF. Mixing
  the two gives a session that pairs, connects, and plays silence.
- **Never add headers to the event-channel 200.** `Server` and `CSeq` only.
  `Content-Length: 0` or `Audio-Latency` corrupt the receiver's realtime
  timeline. Same for the control channel's replies to receiver requests.
- **Never let the session SETUP go out before the timing responder is
  listening.** The HomePod sends NTP timing requests to our UDP port and
  waits for the replies before it answers SETUP. That, and Windows Firewall
  dropping the same UDP, are the two ways "pairs fine, SETUP times out"
  happens. `session.rs` spawns the responder before opening TCP; keep it so.
- **Never send PTP or SETPEERS.** We advertise `timingProtocol: NTP`; PTP
  needs privileged UDP 319/320 and is for multi-room.
- **Never stall the pacer.** The RTP timeline must advance at 44.1 kHz
  whether or not capture has data; send silence rather than nothing.
  A stalled timeline is how a receiver decides the stream is dead.
- **Never let a windows-rs `PROPVARIANT` drop when it borrows a blob.** The
  per-process capture passes `AUDIOCLIENT_ACTIVATION_PARAMS` as a `VT_BLOB`
  pointing at the stack; the struct's Drop runs `PropVariantClear`, which
  frees that pointer and corrupts the heap (`STATUS_HEAP_CORRUPTION` on
  connect, found 2026-09-10). `capture.rs` keeps it in `ManuallyDrop`.
- **Never poll the process-loopback device, and never test the SILENT flag
  as bit 1.** Per-process capture (`Capture::start_process`) delivers packets
  only event-driven (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`), and
  `AUDCLNT_BUFFERFLAGS_SILENT` is 0x2 — on that device a silent buffer is
  uninitialised memory, not zeros. Also: the "include tree" flag does not
  reach from a Tauri host exe into its WebView2 children; target the
  `msedgewebview2.exe` child (`mixer::children_named`). Measured 2026-09-10;
  DeetsMusic docs/AIRPLAY.md §10 has the numbers.
- **Never block a Tauri command on the network or on WinRT from the main
  thread.** Scan, connect, disconnect, status and transport are `async` and
  run in `spawn_blocking`; a synchronous command freezes the panel.
- **Never use media keys as the first choice for transport.** A key press
  routes through the foreground window, and when the panel is focused that
  is our own WebView, which swallows it. `media.rs` drives the Windows media
  session (`Windows.Media.Control`) and falls back to keys only when no app
  has registered one.
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
  `process <ip> --pid N [--mute-after S]` streams one process tree only
  (DeetsMusic's per-process path) and can mute that app in the Windows
  mixer mid-stream — the test for whether the tap survives the mute.
  `discover` lists speakers with their TXT records.
- Windows Firewall: the HomePod sends unsolicited UDP to our timing and
  control ports. Windows does NOT prompt for it. The installed app asks once
  (UAC, `netsh`) on its first run; `probe.exe` and the dev
  `target\debug\deetsairplay.exe` each need a rule added by hand:
  `netsh advfirewall firewall add rule name=... dir=in action=allow protocol=udp program=<exe>`
  (his desk already has both).
- The tray app has no console: session logs mirror to
  `%APPDATA%\com.deetsairplay.app\deetsairplay.log`, connect failures included.
  Read that before guessing.
- Launch-at-startup and the firewall seeding are release-only
  (`cfg(not(debug_assertions))`); a dev build never touches the registry or
  the firewall.

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
npm run release          # NSIS installer under src-tauri/target/release/bundle/nsis/
```

`Cargo.toml` sets `default-run = "deetsairplay"` because there are two
binaries; without it `tauri dev` cannot pick one.

His desk: HomePod "Living Room" at 192.168.86.32:7000 (AudioAccessory6,1),
an Arcam AVR20 at .52. Both accept transient pairing.

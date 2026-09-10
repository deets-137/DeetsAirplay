# DeetsAirplay — architecture

## Shape

A single frameless 360×560 tray panel (DeetsRGB's shell): left-click the tray
icon toggles it, right-click gives *Connect to <last speaker>* / *Disconnect*,
*Open*, *Quit*. Closing the panel never quits and never stops the stream.

Three cards: **Speakers** (scrollable mDNS list), **Connection** (name, live
line, buffer choice, Connect/Disconnect), **Transport** (prev / play-pause /
next, volume).

## The crate (`crates/airplay/src/`, `deets-airplay`) + the app (`src-tauri/src/`)

The sender is a library crate shared with DeetsMusic; the tray app is a thin layer on it.

| Path | Owns |
|---|---|
| `src-tauri/src/lib.rs` | Tauri setup, tray, panel show/hide, every `#[tauri::command]`, the one live session, the latency policy (`latency_frames`). |
| `src-tauri/src/store.rs` | `%APPDATA%/com.deetsairplay.app/deetsairplay.json`: last speaker, volume, latency mode, sync offset. |
| `capture.rs` | WASAPI loopback of the default render device → 44.1 kHz / 16-bit / stereo ring. Asks the engine to convert (`AUTOCONVERTPCM`), converts in software if refused, and runs a silent render stream so loopback never stalls. |
| `src-tauri/src/media.rs` | Transport via the Windows media session (`Windows.Media.Control`), media keys as fallback; now-playing title/artist/state for the panel. |
| `crypto/` | `random` (BCrypt RNG), `hkdf` (HMAC/HKDF-SHA512, hand-rolled), `srp` (SRP-6a client, HAP flavour), `mod.rs` (the ChaCha20-Poly1305 wrapper with HAP nonces). |
| `airplay/mdns.rs` | `_airplay._tcp` browse, hand-rolled DNS parsing. |
| `airplay/rtsp.rs` | The control connection with the encrypted framing; the event-channel responder. |
| `airplay/pairing.rs` | Transient pair-setup M1–M4 → `SessionKeys`. |
| `airplay/tlv8.rs`, `airplay/bplist.rs` | The two encodings. |
| `airplay/alac.rs`, `airplay/rtp.rs` | Uncompressed ALAC frames; RTP/sync/timing/retransmit packets and NTP math. |
| `airplay/session.rs` | `connect()` runs the handshake, then spawns pacer / timing / control / events / keep-alive threads. `Session::stats()` feeds the live line. |
| `src-tauri/src/bin/probe.rs` | Console probe: `discover`, `tone <ip>`, `capture <ip>`, `bplist`. |

Threads while streaming:

- **pacer** — wall-clock token bucket; pulls 352 frames from the source,
  packs, encrypts, sends; sync packet every second. Pro Audio MMCSS.
- **timing** — answers the receiver's NTP requests.
- **control** — answers retransmit requests from the backlog.
- **events** — decrypts the receiver's pushed requests, replies 200.
- **keepalive** — `POST /feedback` every 2 s, timed (RTT).
- **wasapi-loopback** — owned by `Capture`, pushes into the ring.

## Latency policy

The only real knob is the receiver buffer (`latency` frames in the RTP
timeline). Floor 250 ms (protocol), ceiling 2 s.

- **Auto**: start at 300 ms; after 10 s of keep-alive round trips, settle
  once to `250 ms + 4 × p95 RTT` if that differs by ≥ 100 ms (a reconnect,
  ~1 s). Heuristic; tune on the desk.
- **Fixed**: the slider value.
- **Offset**: extra ms on top, for lip-sync with a screen.

Changing any of them reconnects.

## Front-end (`src/`)

| File | Owns |
|---|---|
| `main.ts` | Boot, the three cards, the 1 s status poll, scan-on-focus, settings menu. |
| `api.ts` | Typed `invoke` wrappers; the only file that names a command. |
| `theme.ts` | Copied from DeetsMusic; shared `deets.theme` key. |
| `styles.css` | Chrome (DeetsMusic → DeetsRGB lineage) + the cards. Tokens only. |
| `styles/` | `palette.css` / `themes.css` verbatim; `skin.css` = base + Press with the DeetsAirplay tokens at the end of the base block. |

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
| `src-tauri/src/music.rs` | DeetsMusic's loopback bridge, hand-rolled HTTP: the now-playing card (cover, position), precise transport, the hand-over. Fails into "not running", which is the usual case. |
| `claim.rs` | Which app on this PC holds which speaker, and what its stream carries (`%LOCALAPPDATA%\Deets\airplay-claims.tsv`). Written by `session::connect`, dropped by `Session`'s `Drop`. |
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

## Sharing the PC with DeetsMusic

Both apps send, both use this crate, and a receiver takes **one** sender. Two
channels keep them honest, and neither is required for the other to work.

**The claim file** (`claim.rs`, in the crate). `session::connect` writes
`{id, app, pid, exe, speaker, ip, port, since, send}`; the session's `Drop`
removes it, so a crash or a kill needs no cleanup — a claim is believed only
while a process with that pid *and* that exe name is alive. `send` is what the
stream carries: `all` (the default output's loopback — this app) or
`apps [names]`, which is where the per-app picker in `docs/roadmap.md` lands.
Because it lives in the crate, DeetsMusic adopts it by taking the revision.

**DeetsMusic's bridge** (`music.rs`, in the app). Ports 47825–47828, the token
from its own `settings.json`, hand-rolled HTTP/1.1 with `Connection: close`.
`GET /health` (version, whether Agent control is on) · `GET /now-playing` (the
card) · `GET /airplay` (which speaker it holds — the fallback for a DeetsMusic
too old to write a claim, and its 404 is how we know not to offer a hand-over)
· `POST /command` · `POST /airplay {action:"disconnect"}` (the hand-over).
The card needs only the token; anything that *drives* DeetsMusic also needs
Agent control on over there, and the panel says so instead of failing quietly.

The bridge is only asked while the panel is open (the status poll runs either
way, and nobody reads a card behind a hidden window), `/health` is cached for
30 s, and DeetsMusic no longer logs these polled GETs — two apps at one
request a second would otherwise be all its log ring contained.

Two routing rules, both in `lib.rs`:

- **The card** is DeetsMusic's when DeetsMusic holds the stream, or when we
  hold a stream that carries the whole PC *and* DeetsMusic is playing. Anything
  else — including DeetsMusic sitting paused while a browser plays — is the
  Windows media session. Once the picker exists, a send set without DeetsMusic
  in it must fall back the same way: the card follows what the speaker hears.
- **The volume slider** moves the thing making the sound, exactly once. Ours:
  the receiver's gain over RTSP. DeetsMusic's: its own slider, which it has
  already rerouted to the same receiver. Never DeetsMusic's volume while we
  are capturing it — that attenuates one app inside the mix we are sending
  instead of moving the speaker.

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

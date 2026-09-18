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
| `capture.rs` | WASAPI loopback of the default render device → 44.1 kHz / 16-bit / stereo ring. Asks the engine to convert (`AUTOCONVERTPCM`), converts in software (`resample::Sinc` + TPDF dither) if refused, and runs a silent render stream so loopback never stalls. |
| `resample.rs` | Hand-rolled conversion: `Linear` (the old fallback, kept for the probe), `Sinc` (polyphase Kaiser-windowed sinc for one exact rational ratio, 100 dB rejection), `Quantizer` (f32 → i16, rounded, optional TPDF dither). Streams in chunks of any size. |
| `fidelity.rs` | Dev tool behind `probe fidelity`; the apps never call it. § Measuring audio quality. |
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
| `src-tauri/src/bin/probe.rs` | Console probe: `discover`, `tone <ip>`, `capture <ip>`, `process <ip>`, `selfcapture`, `fidelity`, `bplist`. |

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

The bridge is only asked while the panel is open (nobody reads a card behind a
hidden window), `/health` is cached for 30 s, and DeetsMusic no longer logs
these polled GETs — two apps at one request a second would otherwise be all
its log ring contained.

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

## Measuring audio quality

`probe fidelity` puts numbers on what the capture's conversion does to the
sound. It needs no speaker: the network leg is lossless (ALAC), so all the
loss this crate can add happens between the Windows mix and the 44.1 kHz /
16-bit ring. Kept as a permanent tool, like the rest of the probe.

```bash
cd src-tauri
cargo run --release --bin probe -- fidelity offline [--rate 48000]   # no device, no sound
cargo run --release --bin probe -- fidelity [--no-mute]              # plays the tones itself
cargo run --release --bin probe -- fidelity --listen 35 [--no-mute]  # something else plays them
cargo run --bin probe -- fidelity js                                 # the snippet for --listen
```

Build it `--release`: the analysis is a 65 536-point FFT per row, and the
CPU line compares candidates, which a debug build distorts.

**What a run does.**
1. Mutes the master volume (the loopback sits before it, so the recording
   stays full scale) and restores it on exit, a panic, or Ctrl+C.
   `--no-mute` if a device turns out to tap after the mute (the run says so).
2. Starts the shipping capture (`Capture::start`) and a raw loopback in the
   mix format, side by side.
3. Records 1 s of silence first. Any peak above -80 dBFS means something else
   is playing, and the run stops: other sound lands in every number.
4. Plays the schedule (`TESTS`: 0.5 s lead, then 3 s tone + 1 s gap per
   test) at the mix rate, so playing it adds no conversion.
5. Converts the raw recording offline with each candidate, in 480-frame
   chunks like the capture thread, and makes a perfect copy of the schedule at
   44.1 kHz with TPDF dither as the ceiling.
6. For each test and each row: a 7-term Blackman-Harris window and FFT over
   65 536 samples, starting 1 s after that tone's onset (the first sample
   above -20 dBFS marks the schedule's start).

**The rows.**

| Row | What it is |
|---|---|
| `R` | The ceiling: the schedule made at 44.1 kHz, dithered to 16-bit. No device. |
| `device` | The raw mix-format recording. In `--listen` mode: what local listening gets after Chromium's resample. |
| `E` | The shipping capture as delivered (the engine's conversion, or `L` if the engine refused; the `[capture]` line says which). |
| `L` | `resample::Linear`, the fallback until 2026-09-16. |
| `S` | `resample::Sinc`, rounded to 16-bit, no dither. |
| `S+D` | `resample::Sinc` with TPDF dither: the capture fallback since 2026-09-16. |

**The columns.** `level dB`: each tone against what was sent (roll-off near
20 kHz shows here). `THD+N dB`: everything in 20 Hz–20 kHz that is not a tone,
against the tones. `resid dBFS`: the same residual against a full-scale sine.
`worst spur dBc @ Hz`: the largest single bin that is not a tone, up to the
Nyquist (aliases and truncation harmonics show here). `IMD dBc`: the 1 kHz
difference product of the 19 + 20 kHz pair. A `cpu` line per candidate.

**`--listen`.** Starts recording, then waits while you run the `fidelity js`
snippet in a WebView console (DeetsMusic's dev app: `node
scripts/webview-eval.mjs "<snippet>"`). The snippet builds the schedule as a
44.1 kHz float WAV and plays it through an `<audio>` element, MusicKit's own
path, so the `device` row includes Chromium's resample to the mix rate. The
WebView's volume and its Windows mixer slider must be at 100 % for the level
column to mean anything.

## Latency policy

The only real knob is the receiver buffer (`latency` frames in the RTP
timeline). Floor 250 ms (protocol), ceiling 2 s.

- **Auto**: start at 300 ms; after 10 s of keep-alive round trips, settle
  once to `250 ms + 4 × p95 RTT` if that differs by ≥ 100 ms (a reconnect,
  ~1 s). Heuristic; tune on the desk.
- **Fixed**: the slider value.
- **Offset**: extra ms on top, for lip-sync with a screen.

Changing any of them reconnects.

## Two clocks (2026-09-17)

An app that lives in the tray is hidden almost all of the time, so nothing may
cost a steady tick just because the process is running.

**The session's clock** is `spawn_housekeeper` in `lib.rs`: a thread that
reaps a session whose receiver went away, does the one-time auto-latency
retune, and writes back a volume moved by Siri. It runs at 1 Hz *while a
session is live* and otherwise sleeps 5 s between one mutex read and the next.
All three used to ride the panel's poll, which tied session correctness to a
window being open.

**The panel's clock** is the `status` poll in `main.ts`: 1 s while the panel is
visible, 10 s while it is hidden, paced from `appWindow.isVisible()` on every
focus change (blur is not hidden — a release build hides on blur, a dev build
does not). A hidden tick reads the claim file and little else.

What made this worth doing: `card()` called `media::now_playing()` on every
tick, and that is not a cheap read. It builds a whole
`GlobalSystemMediaTransportControlsSessionManager` — an object meant to be
kept and subscribed to, not made and dropped — and then
`TryGetMediaPropertiesAsync` calls *across into the app that is playing* for
its title and artist. Once a second, from launch, forever, whether or not a
window was open: percent-level CPU here and a share of it charged to Chrome,
Spotify or DeetsMusic. It is now behind the same visibility gate the bridge
already had, and a hidden panel is served `AppState::last_card` instead.

## Front-end (`src/`)

| File | Owns |
|---|---|
| `main.ts` | Boot, the three cards, the status poll, scan-on-focus, settings menu. |
| `api.ts` | Typed `invoke` wrappers; the only file that names a command. |
| `theme.ts` | Copied from DeetsMusic; shared `deets.theme` key. |
| `styles.css` | Chrome (DeetsMusic → DeetsRGB lineage) + the cards. Tokens only. |
| `styles/` | `palette.css` / `themes.css` verbatim; `skin.css` = base + Press with the DeetsAirplay tokens at the end of the base block. |

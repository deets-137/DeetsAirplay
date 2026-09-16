# DeetsAirplay — roadmap (only what is *not* built)

Audio routing (tap vs. a dedicated virtual output device) is the big one and
lives in [`docs/roadmap.md`](docs/roadmap.md).

- **Per-app picker (a volume-mixer for what gets sent).** Today the stream is
  the default output's loopback: everything. The goal is the Windows mixer's
  shape — a row per app that is making sound, with which ones go to the
  speaker. Three pieces, and the crate already has two of them:
  - `Capture::start_process(pid)` taps one process tree (measured, working —
    it is DeetsMusic's path; see its `docs/AIRPLAY.md` §10 for the traps: the
    event-driven-only delivery, the SILENT flag at 0x2, and the WebView2 child).
  - `mixer.rs` already enumerates the render device's audio sessions to mute a
    tree; it needs a `sessions()` that *returns* them (pid, exe, display name,
    icon, whether it is currently making sound) instead of muting.
  - **New:** an N-way source that sums several per-process captures into one
    `Source`, since a pick of two apps is two taps mixed. Clip on sum, and keep
    the pacer fed from whichever rings have data (a stalled timeline is death).
  The claim file already carries the answer (`claim::Send::Apps`), so the
  moment the picker exists DeetsMusic can say "DeetsAirplay is sending Chrome
  only" rather than assuming the speaker has everything. The card in
  `lib.rs` must follow the same switch: it shows DeetsMusic only while
  DeetsMusic is inside the send set — otherwise the panel would name a song
  the speaker is not playing.
- **Auto formula.** `250 ms + 4 × p95 RTT` is a guess; the desk shows RTTs of
  10–25 ms on the keep-alive, so Auto lands near 300–350 ms. Tune once a
  dropout has actually been heard.
- **Firewall rule.** The NSIS installer should add an inbound UDP rule for
  the exe (`netsh advfirewall firewall add rule … program=… protocol=udp
  dir=in action=allow`) so the HomePod's timing requests arrive without a
  prompt. Until then, accept the Windows prompt on first dev run.
- **Latency measurement.** RTT is the keep-alive round trip on the control
  channel; the HomePod reports its own render path (`arrivalToRenderLatencyMs`,
  85 ms on the desk) in the stream SETUP reply and the panel could add it to
  the buffer figure. A true end-to-end number would need a microphone loop.
- **Reconnect on drop.** When the receiver goes away mid-stream the session
  is dropped and the panel shows Idle; it does not retry.
- **Stereo pairs / groups.** A stereo pair advertises as one speaker and
  should just work; multi-room needs PTP and is out of scope.
- **Metadata.** `SET_PARAMETER` with DMAP now-playing text and cover art
  (the HomePod shows nothing, but the Home app does).
- **Transport codes not yet seen.** `togl` and `stop` are mapped on the
  pattern of the four observed on the desk (`play`, `paus`, `nitm`,
  `pitm`) but have never actually arrived; an unrecognised code logs its
  own name, so the next one costs one log line to find. See
  `docs/protocol.md` §7.


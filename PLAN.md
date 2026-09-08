# DeetsAirplay — roadmap (only what is *not* built)

Audio routing (tap vs. a dedicated virtual output device) is the big one and
lives in [`docs/roadmap.md`](docs/roadmap.md).

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
- **Volume from the speaker.** The HomePod's own volume changes (touch
  surface, Siri) arrive as events on the event channel; the slider does
  not follow them yet.


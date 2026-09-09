# Roadmap: audio routing

The one structural thing DeetsAirplay does not do yet. `PLAN.md` holds the
small items; this is the big one, kept on its own page because it grows into
a second project.

## Today: a tap

DeetsAirplay captures with WASAPI loopback, a tap on the stream headed to
the default output device. Windows still routes to the headphones or
speakers and reports that in its own controls; the HomePod receives a copy.
Consequences, measured on the desk (2026-09-08, "High Definition Audio"
headphones):

- Both play at once. Direct on the headphones, ~335 ms later on the
  HomePod (250 ms buffer + the HomePod's 85 ms render path).
- **The local volume does not affect the stream.** The tap sits before the
  endpoint volume on this driver, so turning the headphones to zero is a
  free "silent locally, stream normally" mode. Revisit this note when the
  routing below lands: a dedicated destination changes what the tap sees.

## The setting

A **Route** row in the title menu with two choices:

| Choice | What it means |
|---|---|
| **Tap current output** (today) | Loopback on whatever Windows is playing to. Nothing else changes. |
| **Dedicated destination** | DeetsAirplay's own output device becomes the default; Windows *says* HomePod, the headphones go silent, the stream is the only path. |

Switching to *Dedicated* sets the default endpoint to the virtual device and
remembers the previous default; switching back restores it. A crash or a
Quit must also restore it, or the desk goes silent with no obvious reason.

## The virtual output device

> **Tabled 2026-09-09.** The driver compiles, but the desk runs Secure Boot
> for anti-cheat and cannot load a test-signed kernel driver, and
> attestation signing through Partner Center needs an EV code-signing
> certificate issued to a registered business. Branch `awdiwhoa` holds this
> page's decisions; pick it up when an EV certificate exists. Azure
> Trusted/Artifact Signing does **not** cover kernel drivers.

Decided 2026-09-09 and started as a sibling repo,
[`../DeetsAudioDriver`](../DeetsAudioDriver) (`docs/design.md` there is the
full design). The choices, so nobody re-litigates them:

| Fork | Choice |
|---|---|
| Driver shape | Full `sysvad`-family PortCls WaveRT miniport, trimmed to one render + one capture endpoint. |
| Where it lives | Sibling repo `DeetsAudioDriver`; DeetsAirplay and the Discord bot both consume it. |
| Handoff to user mode | The **capture endpoint mirror**: "DeetsAudio Capture" is a microphone that plays back whatever was rendered to "DeetsAudio". DeetsAirplay reads it with the existing WASAPI code in normal capture mode; Discord/OBS need no client. A private IOCTL path comes later only if the extra ~10-20 ms ever matters. |
| Kernel language | C++ against the WDK. PortCls is COM-shaped; no Rust bindings exist and writing them is bigger than the driver. |
| Default-endpoint switch | `IPolicyConfig` (undocumented, stable since Vista, what EarTrumpet and SoundSwitch use), **console and multimedia roles only**; communications is left on the headset so calls are untouched. |
| Formats | 48 kHz only on the wire, 16-bit PCM and float32; the audio engine resamples in, `capture.rs` resamples out to 44.1. |
| Volume | Nodeless topology, so Windows applies endpoint volume in software: with *Dedicated* set, the local slider scales the stream. The opposite of the tap note above. |

**Sequence.**

1. `DeetsAudioDriver`: build, test-sign, install, see both endpoints, hear
   Voice Recorder play back what Spotify rendered. (Scaffold written; WDK
   not yet installed on the desk.)
2. DeetsAirplay: the **Route** row. *Dedicated* captures "DeetsAudio
   Capture", sets the default via `IPolicyConfig`, remembers the previous
   default in the store, restores on switch back, on Quit, and on next
   launch after an unclean exit.
3. Driver device interface + `INJECT`, and a user-mode mixer, for the
   Discord "Yeti + game into one mic" setups. Routing graphs never enter the
   kernel; the driver is a cable.
4. Signing story for anyone but us.

**Fallback if the driver stalls.** Per-process loopback
(`ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`,
Windows 10 2004+) captures one chosen app without a driver. It does not
silence the local output, so it is a different feature ("stream only
Spotify"), not a replacement for the dedicated destination.

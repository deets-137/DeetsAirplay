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

The dedicated destination needs an audio endpoint that Windows will list,
select, and render into, and that DeetsAirplay can read from. That is a
kernel-mode audio driver, and it is worth writing once because it serves two
projects: this one, and tapping or routing PC audio into a Discord bot.

**Shape.** A WDM audio miniport on the PortCls framework, the same family
Microsoft's `sysvad` sample uses: one render endpoint ("DeetsAudio" or
similar) whose render buffer is exposed to user mode over a device
interface, plus optionally a matching capture endpoint that plays the same
buffer back so ordinary apps (Discord, OBS) can pick it as a microphone
without any special client. Formats: 48 kHz float and 44.1 kHz 16-bit at
minimum, since Windows will resample into whatever we advertise.

**Cost.** Kernel drivers are signed or they do not load. For the desk that
means test-signing mode (`bcdedit /set testsigning on`) and a self-signed
certificate; for anything shared it means an EV certificate and attestation
signing through the Hardware Dev Center. The Windows Driver Kit and Visual
Studio are the only tools; Rust is possible through `windows-drivers-rs` but
PortCls is C++-shaped, so plan on C++ for the driver and Rust for the user
side.

**Sequence.**

1. Driver with a render endpoint only, ring buffer readable from user mode.
   Test-signed, local only.
2. DeetsAirplay reads that ring as a second capture source; the *Dedicated*
   route sets and restores the default endpoint.
3. Capture endpoint mirror for the Discord bot use, so the bot needs no
   custom client.
4. Signing story for anyone but us.

**Fallback if the driver stalls.** Per-process loopback
(`ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`,
Windows 10 2004+) captures one chosen app without a driver. It does not
silence the local output, so it is a different feature ("stream only
Spotify"), not a replacement for the dedicated destination.

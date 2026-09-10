//! `deets-airplay`: the AirPlay 2 sender shared by DeetsAirplay (the tray app)
//! and DeetsMusic (the player). Read `docs/protocol.md` in this repo before
//! touching `airplay/` or `crypto/`, and the "Never" list in `CLAUDE.md`.
//!
//! - `airplay`  find a speaker, pair, stream (`airplay::session::connect`)
//! - `crypto`   the primitives the handshake needs
//! - `capture`  WASAPI loopback (the default output, or one process tree) as a pacer `Source`
//! - `mixer`    mute/unmute an app's own sessions in the Windows volume mixer
//!
//! The crate never talks to a UI: logs go through `airplay::log`, transport
//! commands from the speaker through `session::Config::on_command`.

pub mod airplay;
pub mod capture;
pub mod crypto;
pub mod mixer;

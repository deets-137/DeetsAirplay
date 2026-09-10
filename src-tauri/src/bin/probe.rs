//! The desk probe. Everything the tray app does, from a console, with every
//! RTSP exchange printed. Run this before trusting the UI:
//!
//!   cargo run --bin probe -- discover
//!   cargo run --bin probe -- tone <ip> [--port 7000] [--latency 250] [--seconds 20]
//!   cargo run --bin probe -- capture <ip> [--latency 250] [--seconds 60]
//!   cargo run --bin probe -- process <ip> --pid <pid> [--seconds 30] [--mute-after 10] [--dry]
//!
//! `tone` plays a 440 Hz sine so the audio path is proven without WASAPI in
//! the loop; `capture` streams whatever Windows is playing; `process` streams
//! only one process tree (a running DeetsMusic), and `--mute-after N` mutes
//! that app in the Windows volume mixer after N seconds — the desk test for
//! whether the per-process tap survives the app's own mute (DeetsMusic
//! docs/AIRPLAY.md §6). Unmutes on exit. `--dry` skips the speaker and only
//! reports what the capture hears (frames in / frames not silent).

use std::net::Ipv4Addr;
use std::time::Duration;

use deets_airplay::airplay::alac::SAMPLE_RATE;
use deets_airplay::airplay::session::{self, Config, Source};
use deets_airplay::airplay::{bplist, mdns};
use deets_airplay::capture::Capture;
use deets_airplay::mixer;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn tone_source() -> Source {
    let mut phase = 0.0f64;
    Box::new(move |dst: &mut [i16]| {
        for frame in dst.chunks_mut(2) {
            let v = (phase.sin() * 0.2 * 32767.0) as i16;
            frame[0] = v;
            frame[1] = v;
            phase += 2.0 * std::f64::consts::PI * 440.0 / SAMPLE_RATE as f64;
            if phase > 2.0 * std::f64::consts::PI {
                phase -= 2.0 * std::f64::consts::PI;
            }
        }
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("discover") => {
            eprintln!("browsing _airplay._tcp for 3 s…");
            let speakers = mdns::browse(Duration::from_secs(3)).expect("mDNS browse");
            if speakers.is_empty() {
                eprintln!("nothing answered. Same network as the HomePod? Firewall allowing UDP 5353 in?");
            }
            for s in speakers {
                println!(
                    "{:<28} {:>15}:{:<5} {:<14} transient={} features=0x{:X}",
                    s.name,
                    s.ip.map(|i| i.to_string()).unwrap_or_else(|| "?".into()),
                    s.port,
                    s.model,
                    s.supports_transient_pairing(),
                    s.features()
                );
                let mut keys: Vec<_> = s.txt.iter().collect();
                keys.sort();
                for (k, v) in keys {
                    println!("    {k}={v}");
                }
            }
        }
        Some(cmd @ ("tone" | "capture" | "process")) => {
            let ip: Ipv4Addr = args.get(1).and_then(|s| s.parse().ok()).expect("usage: probe tone|capture|process <ip> [--port N] [--latency MS] [--seconds N] [--volume PCT] [--pid N] [--mute-after S]");
            let pid: Option<u32> = arg(&args, "--pid").and_then(|s| s.parse().ok());
            let mute_after: Option<u64> = arg(&args, "--mute-after").and_then(|s| s.parse().ok());
            let dry = args.iter().any(|a| a == "--dry");
            let port: u16 = arg(&args, "--port").and_then(|s| s.parse().ok()).unwrap_or(7000);
            let latency_ms: u32 = arg(&args, "--latency").and_then(|s| s.parse().ok()).unwrap_or(250);
            let seconds: u64 = arg(&args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
            let volume: f64 = arg(&args, "--volume").and_then(|s| s.parse().ok()).unwrap_or(50.0);
            let config = Config {
                latency_frames: latency_ms * SAMPLE_RATE / 1000,
                volume_pct: volume,
                client_name: "DeetsAirplay probe".into(),
                log: true,
                // The probe reports what the receiver sends; it must not reach
                // in and drive whatever the desk happens to be playing.
                on_command: None,
            };
            let mut capture_ref: Option<&Capture> = None;
            let capture;
            let source: Source = match cmd {
                "tone" => tone_source(),
                "process" => {
                    let pid = pid.expect("process needs --pid <pid> (a running DeetsMusic)");
                    capture = Capture::start_process(pid).expect("per-process loopback");
                    eprintln!("[capture] {}", capture.format_note);
                    capture_ref = Some(&capture);
                    capture.source()
                }
                _ => {
                    capture = Capture::start().expect("WASAPI loopback");
                    eprintln!("[capture] {}", capture.format_note);
                    capture_ref = Some(&capture);
                    capture.source()
                }
            };
            if dry {
                let mut source = source;
                let mut buf = vec![0i16; 352 * 2];
                eprintln!("dry run: capture only, {seconds}s…");
                let start = std::time::Instant::now();
                let mut next_report = 2u64;
                let mut muted = false;
                let mut last = (0u64, 0u64);
                while start.elapsed() < Duration::from_secs(seconds) {
                    std::thread::sleep(Duration::from_millis(8));
                    source(&mut buf); // drain like the pacer would
                    if let (Some(pid), Some(after)) = (pid, mute_after) {
                        if !muted && start.elapsed() >= Duration::from_secs(after) {
                            muted = true;
                            match mixer::set_tree_mute(pid, true) {
                                Ok(n) => eprintln!("[mixer] muted {n} session(s) of pid {pid} + children"),
                                Err(e) => eprintln!("[mixer] mute failed: {e}"),
                            }
                        }
                    }
                    if start.elapsed().as_secs() >= next_report {
                        next_report += 2;
                        if let Some(c) = capture_ref {
                            let (all, loud) = c.stats();
                            eprintln!(
                                "  t={:>3}s frames in={all} not silent={loud}   (last 2 s: in={} loud={} peak={})",
                                start.elapsed().as_secs(), all - last.0, loud - last.1, c.take_peak()
                            );
                            last = (all, loud);
                        }
                    }
                }
                if muted {
                    if let Some(pid) = pid {
                        match mixer::set_tree_mute(pid, false) {
                            Ok(n) => eprintln!("[mixer] unmuted {n} session(s)"),
                            Err(e) => eprintln!("[mixer] unmute failed: {e}"),
                        }
                    }
                }
                eprintln!("done.");
                return;
            }
            let s = match session::connect(ip, port, "probe", config, source) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("FAILED: {e}");
                    std::process::exit(1);
                }
            };
            eprintln!("streaming for {seconds}s (Ctrl-C to stop early)…");
            let start = std::time::Instant::now();
            let mut muted = false;
            while start.elapsed() < Duration::from_secs(seconds) && s.alive() {
                std::thread::sleep(Duration::from_secs(2));
                if let (Some(pid), Some(after)) = (pid, mute_after) {
                    if !muted && start.elapsed() >= Duration::from_secs(after) {
                        muted = true;
                        match mixer::set_tree_mute(pid, true) {
                            Ok(n) => eprintln!("[mixer] muted {n} session(s) of pid {pid} + children — is the HomePod still playing?"),
                            Err(e) => eprintln!("[mixer] mute failed: {e}"),
                        }
                    }
                }
                let st = s.stats();
                let (cin, cloud) = capture_ref.map(|c| c.stats()).unwrap_or((0, 0));
                eprintln!(
                    "  t={:>3}s packets={} timing_req={} retransmit_req={} starved={} rtt={:.1}ms p95={:.1}ms capture in={cin} loud={cloud}",
                    st.seconds, st.packets_sent, st.timing_requests, st.retransmit_requests, st.starved_packets, st.rtt_last_ms, st.rtt_p95_ms
                );
            }
            s.disconnect();
            if muted {
                if let Some(pid) = pid {
                    match mixer::set_tree_mute(pid, false) {
                        Ok(n) => eprintln!("[mixer] unmuted {n} session(s)"),
                        Err(e) => eprintln!("[mixer] unmute failed: {e}"),
                    }
                }
            }
            eprintln!("done.");
        }
        Some("selfcapture") => {
            // Does per-process loopback hear a process's own tree when the
            // capture runs INSIDE that process? (DeetsMusic captures itself.)
            // A child PowerShell plays a system WAV; we capture our own pid.
            let capture = Capture::start_process(std::process::id()).expect("per-process loopback");
            eprintln!("[capture] {} (our pid {})", capture.format_note, std::process::id());
            let mut child = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", "(New-Object Media.SoundPlayer 'C:\\Windows\\Media\\Alarm01.wav').PlaySync()"])
                .spawn()
                .expect("spawn powershell");
            let mut source = capture.source();
            let mut buf = vec![0i16; 352 * 2];
            let start = std::time::Instant::now();
            let mut next = 1u64;
            while start.elapsed() < Duration::from_secs(8) {
                std::thread::sleep(Duration::from_millis(8));
                source(&mut buf);
                if start.elapsed().as_secs() >= next {
                    next += 1;
                    let (all, loud) = capture.stats();
                    eprintln!("  t={:>2}s frames in={all} not silent={loud}", start.elapsed().as_secs());
                }
            }
            child.kill().ok();
            let (_, loud) = capture.stats();
            eprintln!("{}", if loud > 0 { "SELF-TREE CAPTURE HEARS THE CHILD" } else { "self-tree capture heard NOTHING" });
        }
        Some("bplist") => {
            // Round-trip self-check of the encoder/decoder.
            let v = bplist::dict(vec![
                ("a", bplist::Value::Int(70000)),
                ("b", bplist::Value::Bool(true)),
                ("c", bplist::Value::Str("hello".into())),
                ("d", bplist::Value::Data(vec![1, 2, 3])),
                ("e", bplist::Value::Array(vec![bplist::Value::Int(1), bplist::Value::Real(2.5)])),
            ]);
            let enc = bplist::encode(&v);
            let dec = bplist::decode(&enc).expect("decode");
            println!("{}", bplist::pretty(&dec));
            assert_eq!(v, dec);
            println!("bplist round-trip ok ({} bytes)", enc.len());
        }
        _ => {
            eprintln!("usage: probe discover | tone <ip> | capture <ip> | process <ip> --pid N [--mute-after S] [--dry] | selfcapture | bplist");
        }
    }
}

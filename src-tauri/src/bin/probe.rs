//! The desk probe. Everything the tray app does, from a console, with every
//! RTSP exchange printed. Run this before trusting the UI:
//!
//!   cargo run --bin probe -- discover
//!   cargo run --bin probe -- tone <ip> [--port 7000] [--latency 250] [--seconds 20]
//!   cargo run --bin probe -- capture <ip> [--latency 250] [--seconds 60]
//!
//! `tone` plays a 440 Hz sine so the audio path is proven without WASAPI in
//! the loop; `capture` streams whatever Windows is playing.

use std::net::Ipv4Addr;
use std::time::Duration;

use deetsairplay_lib::airplay::alac::SAMPLE_RATE;
use deetsairplay_lib::airplay::session::{self, Config, Source};
use deetsairplay_lib::airplay::{bplist, mdns};
use deetsairplay_lib::capture::Capture;

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
        Some(cmd @ ("tone" | "capture")) => {
            let ip: Ipv4Addr = args.get(1).and_then(|s| s.parse().ok()).expect("usage: probe tone|capture <ip> [--port N] [--latency MS] [--seconds N] [--volume PCT]");
            let port: u16 = arg(&args, "--port").and_then(|s| s.parse().ok()).unwrap_or(7000);
            let latency_ms: u32 = arg(&args, "--latency").and_then(|s| s.parse().ok()).unwrap_or(250);
            let seconds: u64 = arg(&args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
            let volume: f64 = arg(&args, "--volume").and_then(|s| s.parse().ok()).unwrap_or(50.0);
            let config = Config {
                latency_frames: latency_ms * SAMPLE_RATE / 1000,
                volume_pct: volume,
                client_name: "DeetsAirplay probe".into(),
                log: true,
            };
            let capture;
            let source: Source = if cmd == "tone" {
                tone_source()
            } else {
                capture = Capture::start().expect("WASAPI loopback");
                eprintln!("[capture] {}", capture.format_note);
                capture.source()
            };
            let s = match session::connect(ip, port, "probe", config, source) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("FAILED: {e}");
                    std::process::exit(1);
                }
            };
            eprintln!("streaming for {seconds}s (Ctrl-C to stop early)…");
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(seconds) && s.alive() {
                std::thread::sleep(Duration::from_secs(2));
                let st = s.stats();
                eprintln!(
                    "  t={:>3}s packets={} timing_req={} retransmit_req={} starved={} rtt={:.1}ms p95={:.1}ms",
                    st.seconds, st.packets_sent, st.timing_requests, st.retransmit_requests, st.starved_packets, st.rtt_last_ms, st.rtt_p95_ms
                );
            }
            s.disconnect();
            eprintln!("done.");
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
            eprintln!("usage: probe discover | tone <ip> | capture <ip> | bplist");
        }
    }
}

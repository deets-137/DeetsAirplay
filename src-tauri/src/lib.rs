//! Tauri glue: the tray, the panel, and every `#[tauri::command]`. The
//! AirPlay stack and the capture live in the shared `deets-airplay` crate
//! (`../crates/airplay`); this file only owns the one live session and the
//! settings store.

pub mod media;
pub mod store;

use deets_airplay::{airplay, capture};

use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use airplay::alac::SAMPLE_RATE;
use airplay::mdns::{self, Speaker};
use airplay::rtsp::RemoteCommand;
use airplay::session::{self, Config, Session, Stats, MAX_LATENCY_FRAMES, MIN_LATENCY_FRAMES};
use capture::Capture;
use serde::Serialize;
use store::{LastSpeaker, Latency, Settings, Store};
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, PhysicalPosition, State, WindowEvent,
};

/// The one live stream: the session plus the capture feeding it.
struct Live {
    session: Session,
    _capture: Capture,
    capture_note: String,
    speaker: LastSpeaker,
    /// Auto mode retunes once, from the first seconds of round-trip data.
    retuned: bool,
}

struct AppState {
    live: Mutex<Option<Live>>,
    speakers: Mutex<Vec<Speaker>>,
}

/// The panel hides when it loses focus; a tray click that caused that blur
/// must not immediately re-open it.
struct HiddenAt(Mutex<Option<Instant>>);

#[derive(Serialize)]
struct Connected {
    speaker: LastSpeaker,
    stats: Stats,
    capture: String,
}

#[derive(Serialize)]
struct Status {
    connected: Option<Connected>,
    settings: Settings,
    media: media::NowPlaying,
}

// ── latency policy ────────────────────────────────────────────────────

/// Frames of receiver buffer for a settings choice. Auto starts at 300 ms
/// and, once a session has round-trip samples, becomes
/// `250 ms + 4 × p95 RTT` (a heuristic to tune on the desk; PLAN.md).
fn latency_frames(settings: &Settings, rtt_p95_ms: Option<f64>) -> u32 {
    let base_ms = match settings.latency {
        Latency::Fixed { ms } => ms,
        Latency::Auto => match rtt_p95_ms {
            Some(rtt) => (250.0 + 4.0 * rtt).round() as u32,
            None => 300,
        },
    };
    let ms = base_ms + settings.sync_offset_ms;
    (ms * SAMPLE_RATE / 1000).clamp(MIN_LATENCY_FRAMES, MAX_LATENCY_FRAMES)
}

fn start_live(speaker: LastSpeaker, settings: &Settings, rtt_p95_ms: Option<f64>) -> Result<Live, String> {
    let ip: Ipv4Addr = speaker.ip.parse().map_err(|_| format!("bad speaker address {}", speaker.ip))?;
    let capture = Capture::start()?;
    let config = Config {
        latency_frames: latency_frames(settings, rtt_p95_ms),
        volume_pct: settings.volume,
        client_name: "DeetsAirplay".into(),
        log: true,
        // Siri, the Home app and the HomePod's touch surface relay transport
        // commands over the event channel. They drive the PC's media session,
        // the same path as the panel's buttons: the HomePod only ever hears
        // our mixed output, so pausing the source is the only real pause.
        on_command: Some(std::sync::Arc::new(|cmd| {
            media::send(match cmd {
                RemoteCommand::Play => media::Transport::Play,
                RemoteCommand::Pause | RemoteCommand::Stop => media::Transport::Pause,
                RemoteCommand::TogglePlayPause => media::Transport::PlayPause,
                RemoteCommand::Next => media::Transport::Next,
                RemoteCommand::Previous => media::Transport::Previous,
            })
        })),
    };
    airplay::log(&format!("[connect] {} at {}:{} (capture: {})", speaker.name, speaker.ip, speaker.port, capture.format_note));
    let session = session::connect(ip, speaker.port, &speaker.name, config, capture.source()).map_err(|e| {
        airplay::log(&format!("[connect] FAILED: {e}"));
        e
    })?;
    Ok(Live { session, capture_note: capture.format_note.clone(), _capture: capture, speaker, retuned: rtt_p95_ms.is_some() })
}

fn stop_live(state: &AppState) {
    if let Some(live) = state.live.lock().unwrap().take() {
        live.session.disconnect();
    }
}

fn connect_speaker(app: &AppHandle, speaker: LastSpeaker) -> Result<(), String> {
    let state = app.state::<AppState>();
    let store = app.state::<Store>();
    stop_live(&state);
    let settings = store.settings.lock().unwrap().clone();
    let live = start_live(speaker.clone(), &settings, None)?;
    *state.live.lock().unwrap() = Some(live);
    {
        let mut s = store.settings.lock().unwrap();
        s.last_speaker = Some(speaker);
    }
    store.save()?;
    rebuild_tray_menu(app);
    Ok(())
}

/// Reconnect in place (latency change, auto retune). Keeps the speaker.
fn reconnect(app: &AppHandle, rtt_p95_ms: Option<f64>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let store = app.state::<Store>();
    let Some(live) = state.live.lock().unwrap().take() else { return Ok(()) };
    let speaker = live.speaker.clone();
    live.session.disconnect();
    let settings = store.settings.lock().unwrap().clone();
    let mut live = start_live(speaker, &settings, rtt_p95_ms)?;
    live.retuned = true;
    *state.live.lock().unwrap() = Some(live);
    Ok(())
}

// ── commands ──────────────────────────────────────────────────────────

#[tauri::command]
async fn speakers_scan(app: AppHandle) -> Result<Vec<Speaker>, String> {
    let found = tauri::async_runtime::spawn_blocking(|| mdns::browse(Duration::from_millis(2500)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("mDNS: {e}"))?;
    *app.state::<AppState>().speakers.lock().unwrap() = found.clone();
    Ok(found)
}

#[tauri::command]
fn speakers_cached(state: State<AppState>) -> Vec<Speaker> {
    state.speakers.lock().unwrap().clone()
}

#[tauri::command]
async fn speaker_connect(app: AppHandle, speaker: LastSpeaker) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || connect_speaker(&app, speaker)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn speaker_disconnect(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        stop_live(&app.state::<AppState>());
        rebuild_tray_menu(&app);
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn status(app: AppHandle) -> Result<Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let store = app.state::<Store>();
        let mut settings = store.settings.lock().unwrap().clone();

        // Drop a session whose threads died (receiver went away).
        let dead = state.live.lock().unwrap().as_ref().map(|l| !l.session.alive()).unwrap_or(false);
        if dead {
            stop_live(&state);
            rebuild_tray_menu(&app);
        }

        // Auto mode: after 10 s of round trips, settle the buffer once.
        let retune = {
            let live = state.live.lock().unwrap();
            match live.as_ref() {
                Some(l) if !l.retuned && settings.latency == Latency::Auto => {
                    let st = l.session.stats();
                    if st.seconds >= 10 && st.rtt_p95_ms > 0.0 {
                        let target = latency_frames(&settings, Some(st.rtt_p95_ms));
                        let current = l.session.config.latency_frames;
                        let diff_ms = (target as i64 - current as i64).unsigned_abs() as u32 * 1000 / SAMPLE_RATE;
                        if diff_ms >= 100 { Some(st.rtt_p95_ms) } else { None }
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        if let Some(rtt) = retune {
            reconnect(&app, Some(rtt))?;
        } else if let Some(l) = state.live.lock().unwrap().as_mut() {
            // Inside the 100 ms band: call it tuned so we never flap.
            if !l.retuned && l.session.stats().seconds >= 10 && l.session.stats().rtt_p95_ms > 0.0 {
                l.retuned = true;
            }
        }

        // AirPlay volume is the receiver's own gain, not a second one stacked
        // on ours, so what session.rs polls back IS what the user is hearing.
        // A Siri volume change produces no traffic at all, which makes the
        // poll the only way the slider stays honest.
        let heard = state.live.lock().unwrap().as_ref().and_then(|l| l.session.receiver_volume_pct());
        if let Some(pct) = heard {
            if (pct - settings.volume).abs() >= 1.0 {
                settings.volume = pct;
                store.settings.lock().unwrap().volume = pct;
                store.save()?;
            }
        }

        let connected = state.live.lock().unwrap().as_ref().map(|l| Connected {
            speaker: l.speaker.clone(),
            stats: l.session.stats(),
            capture: l.capture_note.clone(),
        });
        Ok(Status { connected, settings, media: media::now_playing() })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn volume_set(pct: f64, state: State<AppState>, store: State<Store>) -> Result<(), String> {
    store.settings.lock().unwrap().volume = pct;
    store.save()?;
    if let Some(l) = state.live.lock().unwrap().as_mut() {
        l.session.set_volume(pct)?;
    }
    Ok(())
}

#[tauri::command]
async fn latency_set(app: AppHandle, latency: Latency, sync_offset_ms: u32) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let store = app.state::<Store>();
        {
            let mut s = store.settings.lock().unwrap();
            s.latency = latency;
            s.sync_offset_ms = sync_offset_ms.min(2000);
        }
        store.save()?;
        reconnect(&app, None)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn transport(kind: media::Transport) {
    // WinRT async calls block on .get(); keep them off the main thread.
    tauri::async_runtime::spawn_blocking(move || media::send(kind)).await.ok();
}

// ── launch at startup (HKCU Run key, via reg.exe; no crate needed) ──────

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "DeetsAirplay";

fn reg(args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("reg.exe: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn autostart_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", RUN_VALUE]).map(|s| s.contains(RUN_VALUE)).unwrap_or(false)
}

fn autostart_write(on: bool) -> Result<(), String> {
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let cmd = format!("\"{}\"", exe.display());
        reg(&["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &cmd, "/f"])?;
    } else {
        reg(&["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])?;
    }
    Ok(())
}

/// The HomePod sends unsolicited UDP (timing requests) to us; without an
/// inbound rule Windows drops it and the handshake stalls. Adding a rule
/// needs elevation, so this asks once through UAC via netsh.
#[cfg_attr(debug_assertions, allow(dead_code))] // release-only first-run seeding
fn firewall_add_rule() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let args = format!(
        "advfirewall firewall add rule name=\"DeetsAirplay\" dir=in action=allow protocol=udp enable=yes program=\"{}\"",
        exe.display()
    );
    // Single-quoted PowerShell string: an apostrophe in the path doubles.
    let cmd = format!("Start-Process -FilePath netsh -Verb RunAs -WindowStyle Hidden -ArgumentList '{}'", args.replace('\'', "''"));
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &cmd])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("powershell: {e}"))?;
    Ok(())
}

#[tauri::command]
fn autostart_get() -> bool {
    autostart_enabled()
}

#[tauri::command]
fn autostart_set(on: bool) -> Result<bool, String> {
    autostart_write(on)?;
    Ok(autostart_enabled())
}

#[tauri::command]
fn panel_hide(app: AppHandle) {
    hide_panel(&app);
}

#[tauri::command]
fn app_quit(app: AppHandle) {
    stop_live(&app.state::<AppState>());
    app.exit(0);
}

// ── tray + panel (DeetsRGB lineage) ───────────────────────────────────

fn hide_panel(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().ok();
        *app.state::<HiddenAt>().0.lock().unwrap() = Some(Instant::now());
    }
}

/// Show the panel with its bottom-right corner at the tray click, kept on-screen.
fn show_panel(app: &AppHandle, at: Option<PhysicalPosition<f64>>) {
    let Some(w) = app.get_webview_window("main") else { return };
    if let Some(p) = at {
        let size = w.outer_size().unwrap_or_default();
        let (mut x, mut y) = (p.x - size.width as f64, p.y - size.height as f64 - 8.0);
        if let Ok(Some(mon)) = app.monitor_from_point(p.x, p.y) {
            let (mp, ms) = (mon.position(), mon.size());
            x = x.max(mp.x as f64).min((mp.x + ms.width as i32) as f64 - size.width as f64);
            y = y.max(mp.y as f64);
        }
        w.set_position(PhysicalPosition::new(x, y)).ok();
    }
    w.show().ok();
    w.set_focus().ok();
}

fn toggle_panel(app: &AppHandle, at: PhysicalPosition<f64>) {
    let visible = app.get_webview_window("main").and_then(|w| w.is_visible().ok()).unwrap_or(false);
    if visible {
        hide_panel(app);
        return;
    }
    let just_hid = app
        .state::<HiddenAt>()
        .0
        .lock()
        .unwrap()
        .map(|t| t.elapsed() < Duration::from_millis(300))
        .unwrap_or(false);
    if !just_hid {
        show_panel(app, Some(at));
    }
}

fn rebuild_tray_menu(app: &AppHandle) {
    let Some(tray) = app.tray_by_id("main") else { return };
    let connected = app.state::<AppState>().live.lock().unwrap().as_ref().map(|l| l.speaker.name.clone());
    let last = app.state::<Store>().settings.lock().unwrap().last_speaker.clone();
    let mut menu = MenuBuilder::new(app);
    match (&connected, &last) {
        (Some(name), _) => {
            if let Ok(item) = MenuItemBuilder::with_id("disconnect", format!("Disconnect {name}")).build(app) {
                menu = menu.item(&item);
            }
        }
        (None, Some(sp)) => {
            if let Ok(item) = MenuItemBuilder::with_id("connect-last", format!("Connect to {}", sp.name)).build(app) {
                menu = menu.item(&item);
            }
        }
        _ => {}
    }
    let open = MenuItemBuilder::with_id("open", "Open DeetsAirplay").build(app);
    let quit = MenuItemBuilder::with_id("quit", "Quit DeetsAirplay").build(app);
    if let (Ok(open), Ok(quit)) = (open, quit) {
        if let Ok(m) = menu.separator().item(&open).item(&quit).build() {
            tray.set_menu(Some(m)).ok();
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(HiddenAt(Mutex::new(None)))
        .manage(AppState { live: Mutex::new(None), speakers: Mutex::new(Vec::new()) })
        .setup(|app| {
            let dir = app.path().app_data_dir().expect("app data dir");
            airplay::log_to_file(&dir.join("deetsairplay.log"));
            app.manage(Store::load(dir));

            // First run of an INSTALLED build enrols in Launch-at-startup once;
            // the title-menu toggle owns it from then on. Dev builds never touch the key.
            #[cfg(not(debug_assertions))]
            {
                let store = app.state::<Store>();
                let (autostart_seeded, firewall_seeded) = {
                    let s = store.settings.lock().unwrap();
                    (s.autostart_seeded, s.firewall_seeded)
                };
                if !autostart_seeded {
                    if let Err(e) = autostart_write(true) {
                        airplay::log(&format!("[autostart] {e}"));
                    }
                    store.settings.lock().unwrap().autostart_seeded = true;
                    store.save().ok();
                }
                if !firewall_seeded {
                    if let Err(e) = firewall_add_rule() {
                        airplay::log(&format!("[firewall] {e}"));
                    }
                    store.settings.lock().unwrap().firewall_seeded = true;
                    store.save().ok();
                }
            }

            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().expect("window icon").clone())
                .tooltip("DeetsAirplay")
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "quit" => {
                        stop_live(&app.state::<AppState>());
                        app.exit(0)
                    }
                    "open" => show_panel(app, None),
                    "disconnect" => {
                        stop_live(&app.state::<AppState>());
                        rebuild_tray_menu(app);
                    }
                    "connect-last" => {
                        let last = app.state::<Store>().settings.lock().unwrap().last_speaker.clone();
                        if let Some(sp) = last {
                            let app = app.clone();
                            std::thread::spawn(move || {
                                if let Err(e) = connect_speaker(&app, sp) {
                                    eprintln!("[connect] {e}");
                                }
                            });
                        }
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, position, .. } = ev {
                        toggle_panel(tray.app_handle(), position);
                    }
                })
                .build(app)?;
            rebuild_tray_menu(app.handle());

            #[cfg(debug_assertions)]
            if let Some(win) = app.get_webview_window("main") {
                win.open_devtools();
            }
            Ok(())
        })
        .on_window_event(|win, ev| match ev {
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                hide_panel(win.app_handle());
            }
            #[cfg(not(debug_assertions))]
            WindowEvent::Focused(false) => hide_panel(win.app_handle()),
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            speakers_scan,
            speakers_cached,
            speaker_connect,
            speaker_disconnect,
            status,
            volume_set,
            latency_set,
            transport,
            autostart_get,
            autostart_set,
            panel_hide,
            app_quit,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

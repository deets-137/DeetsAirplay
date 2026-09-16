//! `probe fidelity`: how much the capture's conversion costs, in numbers.
//! A dev tool kept in the crate so the real conversion code is what gets
//! measured. The apps never call it.
//!
//! One run plays a fixed schedule of test tones ([`TESTS`]) and records it
//! twice at once: through [`Capture::start`] (what ships: the engine's own
//! 44.1 kHz / 16-bit conversion, or the sinc + dither fallback) and as the raw mix
//! format. The raw recording is then converted offline by every candidate in
//! `resample`, a perfect 44.1 kHz copy is made as the ceiling, and each result
//! goes through a hand-rolled FFT: level, THD+N, residual, worst spur.
//!
//! `listen` mode records the same way but lets something else play the
//! schedule — a WebView running [`js_snippet`] — so the "device" row measures
//! Chromium's own resample to the mix rate (what local listening gets).
//!
//! The loopback sits before the endpoint volume, so the master volume is
//! muted for the run ([`MuteGuard`]) and restored on exit, panic or Ctrl+C.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Console::SetConsoleCtrlHandler;

use crate::airplay::alac::SAMPLE_RATE;
use crate::capture::Capture;
use crate::resample::{Frame, Linear, Quantizer, Sinc};

/// One test: tone frequencies (Hz, summed) at `dbfs` each.
pub struct Test {
    pub name: &'static str,
    pub freqs: &'static [f64],
    pub dbfs: f64,
}

pub const TESTS: &[Test] = &[
    Test { name: "1 kHz at -1 dBFS (distortion + noise)", freqs: &[1000.0], dbfs: -1.0 },
    Test { name: "1 kHz at -60 dBFS (16-bit cut, dither)", freqs: &[1000.0], dbfs: -60.0 },
    Test { name: "15 kHz at -6 dBFS (aliased harmonics)", freqs: &[15000.0], dbfs: -6.0 },
    Test { name: "19 kHz at -6 dBFS (roll-off)", freqs: &[19000.0], dbfs: -6.0 },
    Test { name: "20 kHz at -6 dBFS (roll-off)", freqs: &[20000.0], dbfs: -6.0 },
    Test { name: "19 + 20 kHz at -7 dBFS each (IMD)", freqs: &[19000.0, 20000.0], dbfs: -7.0 },
];

/// Silence before the first tone, tone length, one test's slot, fade in/out.
const LEAD_S: f64 = 0.5;
const TONE_S: f64 = 3.0;
const PERIOD_S: f64 = 4.0;
const FADE_S: f64 = 0.01;
/// Analysis window: starts this long after a tone's onset.
const WINDOW_OFFSET_S: f64 = 1.0;
const FFT_N: usize = 1 << 16;
/// Bins either side of a tone that belong to it (the window's main lobe is ±7).
const LOBE_BINS: usize = 10;

/// The whole schedule at `rate`, stereo, identical channels.
pub fn schedule(rate: u32) -> Vec<Frame> {
    let r = rate as f64;
    let total = ((LEAD_S + TESTS.len() as f64 * PERIOD_S) * r).round() as usize;
    let mut out = vec![(0f32, 0f32); total];
    for (k, t) in TESTS.iter().enumerate() {
        let a = 10f64.powf(t.dbfs / 20.0);
        let start = ((LEAD_S + k as f64 * PERIOD_S) * r).round() as usize;
        let len = (TONE_S * r).round() as usize;
        let fade = (FADE_S * r).round() as usize;
        for i in 0..len {
            let g = if i < fade {
                0.5 - 0.5 * (std::f64::consts::PI * i as f64 / fade as f64).cos()
            } else if i > len - fade {
                0.5 - 0.5 * (std::f64::consts::PI * (len - i) as f64 / fade as f64).cos()
            } else {
                1.0
            };
            let v: f64 = t.freqs.iter().map(|f| (2.0 * std::f64::consts::PI * f * i as f64 / r).sin()).sum();
            let s = (a * g * v) as f32;
            out[start + i] = (s, s);
        }
    }
    out
}

/// A console snippet that plays the same schedule at 44.1 kHz through an
/// `<audio>` element (MusicKit's path): run it in the WebView while `listen` records.
pub fn js_snippet() -> String {
    let tests: Vec<String> = TESTS
        .iter()
        .map(|t| format!("[[{}],{}]", t.freqs.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(","), t.dbfs))
        .collect();
    format!(
        "(async()=>{{const R=44100,T=[{tests}],LEAD={LEAD_S},TONE={TONE_S},PERIOD={PERIOD_S},FADE={FADE_S};\
const n=Math.round((LEAD+T.length*PERIOD)*R),d=new Float32Array(n);\
T.forEach(([f,db],k)=>{{const a=Math.pow(10,db/20),s0=Math.round((LEAD+k*PERIOD)*R),len=Math.round(TONE*R),fl=Math.round(FADE*R);\
for(let i=0;i<len;i++){{let g=1;if(i<fl)g=0.5-0.5*Math.cos(Math.PI*i/fl);else if(i>len-fl)g=0.5-0.5*Math.cos(Math.PI*(len-i)/fl);\
let v=0;for(const fr of f)v+=Math.sin(2*Math.PI*fr*i/R);d[s0+i]=a*g*v;}}}});\
const dl=n*8,b=new ArrayBuffer(44+dl),v=new DataView(b),w=(o,s)=>{{for(let i=0;i<s.length;i++)v.setUint8(o+i,s.charCodeAt(i))}};\
w(0,'RIFF');v.setUint32(4,36+dl,true);w(8,'WAVE');w(12,'fmt ');v.setUint32(16,16,true);v.setUint16(20,3,true);v.setUint16(22,2,true);\
v.setUint32(24,R,true);v.setUint32(28,R*8,true);v.setUint16(32,8,true);v.setUint16(34,32,true);w(36,'data');v.setUint32(40,dl,true);\
for(let i=0;i<n;i++){{v.setFloat32(44+i*8,d[i],true);v.setFloat32(48+i*8,d[i],true);}}\
const el=new Audio(URL.createObjectURL(new Blob([b],{{type:'audio/wav'}})));el.volume=1;await el.play();return 'playing '+(n/R)+' s';}})()",
        tests = tests.join(",")
    )
}

// ── master mute, restored whatever happens ───────────────────────────────────

/// 0 = not engaged, 1 = restore to unmuted, 2 = restore to muted.
static RESTORE: AtomicU8 = AtomicU8::new(0);

unsafe fn endpoint_volume() -> Result<IAudioEndpointVolume, String> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let device = default_device()?;
    device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioEndpointVolume: {e}"))
}

fn restore_mute() {
    let state = RESTORE.swap(0, Ordering::SeqCst);
    if state == 0 {
        return;
    }
    unsafe {
        if let Ok(v) = endpoint_volume() {
            v.SetMute(state == 2, std::ptr::null()).ok();
        }
    }
}

unsafe extern "system" fn on_console_ctrl(_ctrl: u32) -> windows_core::BOOL {
    restore_mute();
    windows_core::BOOL(0) // not handled: the default handler still ends the process
}

pub struct MuteGuard;

impl MuteGuard {
    pub fn engage() -> Result<Self, String> {
        unsafe {
            let v = endpoint_volume()?;
            let was = v.GetMute().map_err(|e| format!("GetMute: {e}"))?.as_bool();
            RESTORE.store(if was { 2 } else { 1 }, Ordering::SeqCst);
            SetConsoleCtrlHandler(Some(on_console_ctrl), true).ok();
            v.SetMute(true, std::ptr::null()).map_err(|e| format!("SetMute: {e}"))?;
        }
        Ok(Self)
    }
}

impl Drop for MuteGuard {
    fn drop(&mut self) {
        restore_mute();
    }
}

// ── WASAPI: render the schedule, record the raw mix ─────────────────────────

unsafe fn default_device() -> Result<IMMDevice, String> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
    enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| format!("default render device: {e}"))
}

#[derive(Clone, Copy)]
pub struct MixFormat {
    pub rate: u32,
    pub channels: usize,
    pub bits: u16,
    pub float: bool,
    block: usize,
}

unsafe fn read_mix(mix: *const WAVEFORMATEX) -> MixFormat {
    let f = &*mix;
    let tag = f.wFormatTag as u32;
    let float = if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = mix as *const WAVEFORMATEXTENSIBLE;
        std::ptr::addr_of!((*ext).SubFormat).read_unaligned() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        tag == WAVE_FORMAT_IEEE_FLOAT
    };
    MixFormat { rate: f.nSamplesPerSec, channels: f.nChannels as usize, bits: f.wBitsPerSample, float, block: f.nBlockAlign as usize }
}

/// Play `frames` (at the mix rate) on the default device, blocking until done.
fn play(frames: &[Frame]) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let device = default_device()?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioClient (render): {e}"))?;
        let mix = client.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?;
        let m = read_mix(mix);
        CoTaskMemFree(Some(mix as *const _));
        let ch = m.channels;
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: ch as u16,
            nSamplesPerSec: m.rate,
            nAvgBytesPerSec: m.rate * ch as u32 * 4,
            nBlockAlign: (ch * 4) as u16,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // Same rate as the mix, so the engine only reformats if the mix is not float.
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &fmt, None).map_err(|e| format!("Initialize (render): {e}"))?;
        let render: IAudioRenderClient = client.GetService().map_err(|e| format!("IAudioRenderClient: {e}"))?;
        let size = client.GetBufferSize().map_err(|e| e.to_string())? as usize;
        client.Start().map_err(|e| format!("Start (render): {e}"))?;
        let mut idx = 0;
        while idx < frames.len() {
            let padding = client.GetCurrentPadding().map_err(|e| e.to_string())? as usize;
            let n = size.saturating_sub(padding).min(frames.len() - idx);
            if n > 0 {
                let p = render.GetBuffer(n as u32).map_err(|e| format!("GetBuffer (render): {e}"))? as *mut f32;
                for i in 0..n {
                    let (l, r) = frames[idx + i];
                    for c in 0..ch {
                        *p.add(i * ch + c) = match c {
                            0 => l,
                            1 => r,
                            _ => 0.0,
                        };
                    }
                }
                render.ReleaseBuffer(n as u32, 0).map_err(|e| format!("ReleaseBuffer (render): {e}"))?;
                idx += n;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let drained = Instant::now();
        while client.GetCurrentPadding().unwrap_or(0) > 0 && drained.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        client.Stop().ok();
    }
    Ok(())
}

/// A loopback of the default device in its own mix format, kept whole
/// (first two channels as f32 pairs). Nothing is dropped or padded.
struct RawLoopback {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    frames: Arc<Mutex<Vec<Frame>>>,
    glitches: Arc<AtomicU8>,
    format: MixFormat,
}

impl RawLoopback {
    fn start() -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let glitches = Arc::new(AtomicU8::new(0));
        let (tx, rx) = std::sync::mpsc::channel::<Result<MixFormat, String>>();
        let (st, fr, gl) = (stop.clone(), frames.clone(), glitches.clone());
        let thread = std::thread::spawn(move || {
            if let Err(e) = Self::run(st, fr, gl, &tx) {
                tx.send(Err(e)).ok();
            }
        });
        let format = rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "raw loopback did not start".to_string())??;
        Ok(Self { stop, thread: Some(thread), frames, glitches, format })
    }

    fn run(stop: Arc<AtomicBool>, frames: Arc<Mutex<Vec<Frame>>>, glitches: Arc<AtomicU8>, init: &std::sync::mpsc::Sender<Result<MixFormat, String>>) -> Result<(), String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| format!("CoInitializeEx: {e}"))?;
            let device = default_device()?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioClient (raw loopback): {e}"))?;
            let mix = client.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?;
            let m = read_mix(mix);
            let init_result = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, mix, None);
            CoTaskMemFree(Some(mix as *const _));
            init_result.map_err(|e| format!("Initialize (raw loopback): {e}"))?;
            let bytes = (m.bits / 8) as usize;
            if !matches!((m.float, m.bits), (true, 32) | (false, 16) | (false, 24) | (false, 32)) {
                return Err(format!("mix format {}-bit {} is not handled", m.bits, if m.float { "float" } else { "int" }));
            }
            let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("IAudioCaptureClient: {e}"))?;
            client.Start().map_err(|e| format!("Start (raw loopback): {e}"))?;
            init.send(Ok(m)).ok();
            while !stop.load(Ordering::Relaxed) {
                loop {
                    if capture.GetNextPacketSize().unwrap_or(0) == 0 {
                        break;
                    }
                    let (mut data, mut n, mut flags) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
                    if capture.GetBuffer(&mut data, &mut n, &mut flags, None, None).is_err() {
                        break;
                    }
                    if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                        glitches.fetch_add(1, Ordering::Relaxed);
                    }
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    let mut out = frames.lock().unwrap();
                    for i in 0..n as usize {
                        if silent {
                            out.push((0.0, 0.0));
                            continue;
                        }
                        let read = |c: usize| -> f32 {
                            let p = data.add(i * m.block + c.min(m.channels - 1) * bytes);
                            match (m.float, m.bits) {
                                (true, _) => (p as *const f32).read_unaligned(),
                                (false, 16) => (p as *const i16).read_unaligned() as f32 / 32768.0,
                                (false, 24) => {
                                    let v = (*p as i32) | ((*p.add(1) as i32) << 8) | ((*p.add(2) as i8 as i32) << 16);
                                    v as f32 / 8_388_608.0
                                }
                                _ => (p as *const i32).read_unaligned() as f32 / 2_147_483_648.0,
                            }
                        };
                        out.push((read(0), read(1)));
                    }
                    drop(out);
                    capture.ReleaseBuffer(n).ok();
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            client.Stop().ok();
        }
        Ok(())
    }

    fn take(&self) -> Vec<Frame> {
        std::mem::take(&mut *self.frames.lock().unwrap())
    }

    fn stop(mut self) -> Vec<Frame> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
        self.take()
    }
}

// ── analysis ─────────────────────────────────────────────────────────────────

/// In-place radix-2 complex FFT.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = ((ang * k as f64).cos(), (ang * k as f64).sin());
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * wr - im[b] * wi;
                let ti = re[b] * wi + im[b] * wr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// 7-term Blackman-Harris: sidelobes near -180 dB, so a -100 dB spur next to
/// a full-scale tone is still visible.
fn window(n: usize) -> Vec<f64> {
    const A: [f64; 7] = [0.271_051_400_693_42, 0.433_297_939_234_48, 0.218_122_999_543_11, 0.065_925_446_388_03, 0.010_811_742_098_37, 0.000_776_584_825_22, 0.000_013_887_217_35];
    (0..n)
        .map(|i| {
            let x = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
            A.iter().enumerate().map(|(k, &a)| if k % 2 == 0 { a } else { -a } * (k as f64 * x).cos()).sum()
        })
        .collect()
}

pub struct Measure {
    /// Each tone's level against what was sent, dB.
    pub levels_db: Vec<f64>,
    /// Everything in 20 Hz–20 kHz that is not a tone, against the tones.
    pub thdn_db: f64,
    /// The same residual against a full-scale sine.
    pub residual_dbfs: f64,
    /// The largest single bin that is not a tone, against the strongest tone's bin.
    pub spur_dbc: f64,
    pub spur_hz: f64,
    /// For two tones: the difference product (f2 − f1) against the tones.
    pub imd_dbc: Option<f64>,
}

fn measure(x: &[f32], rate: u32, test: &Test) -> Measure {
    let w = window(FFT_N);
    let mut re: Vec<f64> = x.iter().zip(&w).map(|(s, w)| *s as f64 * w).collect();
    let mut im = vec![0.0; FFT_N];
    fft(&mut re, &mut im);
    let power: Vec<f64> = (0..FFT_N / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect();
    let bin_hz = rate as f64 / FFT_N as f64;
    let sum_w2: f64 = w.iter().map(|v| v * v).sum();
    let full_scale = 0.25 * FFT_N as f64 * sum_w2; // lobe power of a sine at amplitude 1
    let a = 10f64.powf(test.dbfs / 20.0);
    let centre = |hz: f64| (hz / bin_hz).round() as usize;
    let in_lobe = |k: usize| test.freqs.iter().any(|f| k.abs_diff(centre(*f)) <= LOBE_BINS);
    let lobe = |hz: f64| -> f64 {
        let c = centre(hz);
        power[c.saturating_sub(LOBE_BINS)..=(c + LOBE_BINS).min(power.len() - 1)].iter().sum()
    };
    let levels_db: Vec<f64> = test.freqs.iter().map(|f| 10.0 * (lobe(*f) / (a * a * full_scale)).log10()).collect();
    let tones: f64 = test.freqs.iter().map(|f| lobe(*f)).sum();
    let (lo, hi) = (centre(20.0), centre(20_000.0).min(power.len() - 1));
    let rest: f64 = (lo..=hi).filter(|k| !in_lobe(*k)).map(|k| power[k]).sum::<f64>().max(1e-30);
    let peak = test.freqs.iter().map(|f| power[centre(*f)]).fold(0.0, f64::max).max(1e-30);
    let spur_hi = centre((rate as f64 / 2.0 * 0.995).min(23_900.0)).min(power.len() - 1);
    let (spur_k, spur_p) = (lo..=spur_hi).filter(|k| !in_lobe(*k)).map(|k| (k, power[k])).fold((0, 0.0), |m, v| if v.1 > m.1 { v } else { m });
    let imd_dbc = (test.freqs.len() == 2).then(|| {
        let d = centre((test.freqs[1] - test.freqs[0]).abs());
        let p: f64 = power[d - 3..=d + 3].iter().sum();
        10.0 * (p.max(1e-30) / tones).log10()
    });
    Measure {
        levels_db,
        thdn_db: 10.0 * (rest / tones.max(1e-30)).log10(),
        residual_dbfs: 10.0 * (rest / full_scale).log10(),
        spur_dbc: 10.0 * (spur_p.max(1e-30) / peak).log10(),
        spur_hz: spur_k as f64 * bin_hz,
        imd_dbc,
    }
}

/// Left channel of a recording, with the first tone's onset found.
struct Take {
    name: String,
    rate: u32,
    left: Vec<f32>,
    onset: Option<usize>,
}

impl Take {
    fn new(name: &str, rate: u32, left: Vec<f32>) -> Self {
        let onset = left.iter().position(|s| s.abs() > 0.1); // the first tone is -1 dBFS
        Self { name: name.to_string(), rate, left, onset }
    }
    fn window(&self, k: usize) -> Option<&[f32]> {
        let start = self.onset? + ((k as f64 * PERIOD_S + WINDOW_OFFSET_S) * self.rate as f64).round() as usize;
        self.left.get(start..start + FFT_N)
    }
}

fn left_of_i16(v: &[i16]) -> Vec<f32> {
    v.chunks_exact(2).map(|c| c[0] as f32 / 32768.0).collect()
}

/// Run the offline candidates on a raw recording at `rate`.
fn candidates(raw: &[Frame], rate: u32, cpu: &mut Vec<(String, f64)>) -> Vec<Take> {
    const CHUNK: usize = 480; // streaming chunks, like the capture thread's packets
    let seconds = raw.len() as f64 / rate as f64;
    let mut takes = Vec::new();

    let t = Instant::now();
    let mut linear = Linear::new(rate, SAMPLE_RATE);
    let mut out = Vec::new();
    raw.chunks(CHUNK).for_each(|c| linear.process(c, &mut out));
    cpu.push(("L".into(), t.elapsed().as_secs_f64() / seconds));
    takes.push(Take::new("L  linear (old fallback)", SAMPLE_RATE, left_of_i16(&out)));

    for (label, dither) in [("S  sinc, rounded", false), ("S+D sinc + TPDF (fallback)", true)] {
        let t = Instant::now();
        let mut sinc = Sinc::new(rate, SAMPLE_RATE);
        let mut q = Quantizer::new(dither);
        let (mut mid, mut out) = (Vec::new(), Vec::new());
        for c in raw.chunks(CHUNK) {
            mid.clear();
            sinc.process(c, &mut mid);
            q.process(&mid, &mut out);
        }
        cpu.push((label.split_whitespace().next().unwrap().to_string(), t.elapsed().as_secs_f64() / seconds));
        takes.push(Take::new(label, SAMPLE_RATE, left_of_i16(&out)));
    }
    takes
}

fn print_report(takes: &[Take]) {
    for (k, test) in TESTS.iter().enumerate() {
        println!("\n{}", test.name);
        println!("  {:<30} {:>9} {:>9} {:>11} {:>20} {:>9}", "", "level dB", "THD+N dB", "resid dBFS", "worst spur dBc @ Hz", "IMD dBc");
        for take in takes {
            let Some(x) = take.window(k) else {
                println!("  {:<30} (recording too short or no onset)", take.name);
                continue;
            };
            let m = measure(x, take.rate, test);
            let level = m.levels_db.iter().map(|l| format!("{l:+.2}")).collect::<Vec<_>>().join("/");
            let imd = m.imd_dbc.map(|v| format!("{v:.1}")).unwrap_or_default();
            println!(
                "  {:<30} {:>9} {:>9.1} {:>11.1} {:>11.1} @ {:>6.0} {:>9}",
                take.name, level, m.thdn_db, m.residual_dbfs, m.spur_dbc, m.spur_hz, imd
            );
        }
    }
}

/// No device at all: the schedule made at `rate` in memory, run through the
/// same candidates. Checks the resamplers and the analysis themselves, and
/// shows the best each candidate can do.
pub fn run_offline(rate: u32) {
    let sinc = Sinc::new(rate, SAMPLE_RATE);
    eprintln!("[offline] schedule made at {rate} Hz, no device. [sinc] {} taps, adds {:.2} ms", sinc.taps(), sinc.delay_frames() as f64 * 1000.0 / rate as f64);
    let raw = schedule(rate);
    let mut ideal = Vec::new();
    Quantizer::new(true).process(&schedule(SAMPLE_RATE), &mut ideal);
    let mut takes = vec![
        Take::new("R  ideal 44.1k/16 (ceiling)", SAMPLE_RATE, left_of_i16(&ideal)),
        Take::new("source (float, before any cut)", rate, raw.iter().map(|f| f.0).collect()),
    ];
    let mut cpu = Vec::new();
    takes.extend(candidates(&raw, rate, &mut cpu));
    print_report(&takes);
    println!();
    for (name, per_s) in cpu {
        println!("cpu {name:<4} {:.3} ms per second of audio ({:.0}x realtime)", per_s * 1000.0, 1.0 / per_s.max(1e-9));
    }
}

pub struct Options {
    pub mute: bool,
    /// `None`: the probe plays the schedule. `Some(s)`: record `s` seconds
    /// while something else plays it.
    pub listen_seconds: Option<u64>,
}

pub fn run(opts: Options) -> Result<(), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _mute = if opts.mute {
        let g = MuteGuard::engage()?;
        eprintln!("[mute] master volume muted for the run (restored on exit, Ctrl+C included)");
        Some(g)
    } else {
        None
    };

    // E: the shipping capture. Its silent render stream also keeps the engine
    // ticking, so the raw loopback sees packets before anything plays.
    let engine = Capture::start()?;
    let raw = RawLoopback::start()?;
    let m = raw.format;
    eprintln!("[device] mix format {} Hz / {} ch / {}-bit {}", m.rate, m.channels, m.bits, if m.float { "float" } else { "int" });
    eprintln!("[capture] shipping path: {}", engine.format_note);
    let sinc = Sinc::new(m.rate, SAMPLE_RATE);
    eprintln!("[sinc] {} Hz -> {} Hz: {} taps, adds {:.2} ms", m.rate, SAMPLE_RATE, sinc.taps(), sinc.delay_frames() as f64 * 1000.0 / m.rate as f64);

    let mut engine_rec: Vec<i16> = Vec::new();
    let drain_for = |rec: &mut Vec<i16>, d: Duration| {
        let t = Instant::now();
        while t.elapsed() < d {
            engine.ring.drain_into(rec);
            std::thread::sleep(Duration::from_millis(5));
        }
        engine.ring.drain_into(rec);
    };

    // Quiet check: anything else playing would land in every number.
    drain_for(&mut engine_rec, Duration::from_secs(1));
    let quiet = raw.take();
    let peak = quiet.iter().map(|f| f.0.abs().max(f.1.abs())).fold(0f32, f32::max);
    if peak > 1e-4 {
        return Err(format!("something else is playing (peak {:.1} dBFS in 1 s of silence). Stop it and run again.", 20.0 * peak.log10()));
    }
    engine_rec.clear();

    let schedule_s = LEAD_S + TESTS.len() as f64 * PERIOD_S;
    match opts.listen_seconds {
        None => {
            eprintln!("[play] {} tests, {schedule_s:.1} s…", TESTS.len());
            let frames = schedule(m.rate);
            let player = std::thread::spawn(move || play(&frames));
            while !player.is_finished() {
                drain_for(&mut engine_rec, Duration::from_millis(50));
            }
            player.join().map_err(|_| "render thread panicked".to_string())??;
            drain_for(&mut engine_rec, Duration::from_millis(500));
        }
        Some(seconds) => {
            eprintln!("[listen] recording {seconds} s. Start the schedule now (probe fidelity js prints it; {schedule_s:.1} s long).");
            drain_for(&mut engine_rec, Duration::from_secs(seconds));
        }
    }
    let glitches = raw.glitches.load(Ordering::Relaxed);
    let raw_rec = raw.stop();
    drop(engine);
    if glitches > 0 {
        eprintln!("[warn] the raw loopback reported {glitches} discontinuities: numbers near a glitch are not trustworthy");
    }

    let raw_take = Take::new("device (mix rate, before AirPlay)", m.rate, raw_rec.iter().map(|f| f.0).collect());
    if raw_take.onset.is_none() {
        return Err(if opts.mute {
            "the recording is silent: on this device the loopback is after the mute. Run again with --no-mute.".into()
        } else {
            "the recording is silent: nothing played the schedule".into()
        });
    }

    let mut takes = Vec::new();
    // R: the ceiling — the schedule made at 44.1 kHz and dithered to 16-bit, no device in between.
    let mut ideal = Vec::new();
    Quantizer::new(true).process(&schedule(SAMPLE_RATE), &mut ideal);
    takes.push(Take::new("R  ideal 44.1k/16 (ceiling)", SAMPLE_RATE, left_of_i16(&ideal)));
    takes.push(raw_take);
    takes.push(Take::new("E  engine (what ships)", SAMPLE_RATE, left_of_i16(&engine_rec)));
    let mut cpu = Vec::new();
    takes.extend(candidates(&raw_rec, m.rate, &mut cpu));

    print_report(&takes);
    println!();
    for (name, per_s) in cpu {
        println!("cpu {name:<4} {:.3} ms per second of audio ({:.0}x realtime)", per_s * 1000.0, 1.0 / per_s.max(1e-9));
    }
    Ok(())
}

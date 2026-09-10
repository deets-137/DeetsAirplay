//! WASAPI loopback capture of the default render device, delivered as
//! 44.1 kHz / 16-bit / stereo through a small ring. Two tricks keep it
//! simple and low-latency:
//!
//! 1. `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` asks the audio engine to hand us
//!    44.1k/16/2 directly, whatever the device mix format is (usually 48 kHz
//!    float). If the engine refuses, we take the mix format and convert
//!    (float→i16, linear resample) ourselves.
//! 2. A silent render stream on the same device keeps the engine running,
//!    so loopback keeps delivering frames while nothing is playing and the
//!    AirPlay timeline never starves on the capture side.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

use crate::airplay::alac::{CHANNELS, SAMPLE_RATE};

/// How much captured audio we hold before dropping the oldest. This is the
/// only latency the capture side adds; the pacer normally drains it to zero.
const RING_MAX_FRAMES: usize = 4410; // 100 ms

pub struct Ring {
    samples: Mutex<VecDeque<i16>>,
    /// Frames the source delivered, and how many of those were not silence —
    /// the probe's evidence that a capture actually hears something.
    frames_in: AtomicU64,
    frames_loud: AtomicU64,
    /// Largest |sample| seen since the last `take_peak` — garbage and music both
    /// count as "not silent"; only a peak that follows the song is real audio.
    peak: AtomicU64,
}

impl Ring {
    fn new() -> Self {
        Self {
            samples: Mutex::new(VecDeque::with_capacity(RING_MAX_FRAMES * CHANNELS * 2)),
            frames_in: AtomicU64::new(0),
            frames_loud: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }
    fn push(&self, s: &[i16]) {
        self.frames_in.fetch_add((s.len() / CHANNELS) as u64, Ordering::Relaxed);
        if s.iter().any(|&v| v != 0) {
            self.frames_loud.fetch_add((s.len() / CHANNELS) as u64, Ordering::Relaxed);
            let peak = s.iter().map(|&v| (v as i32).unsigned_abs() as u64).max().unwrap_or(0);
            self.peak.fetch_max(peak, Ordering::Relaxed);
        }
        let mut q = self.samples.lock().unwrap();
        q.extend(s);
        let max = RING_MAX_FRAMES * CHANNELS;
        if q.len() > max {
            let drop = q.len() - max;
            q.drain(..drop);
        }
    }
    /// Fill `dst` from the ring, zeros for whatever is missing.
    pub fn fill(&self, dst: &mut [i16]) {
        let mut q = self.samples.lock().unwrap();
        let n = dst.len().min(q.len());
        for (i, s) in q.drain(..n).enumerate() {
            dst[i] = s;
        }
        dst[n..].iter_mut().for_each(|s| *s = 0);
    }
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub ring: Arc<Ring>,
    pub format_note: String,
}

impl Capture {
    pub fn start() -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let ring = Arc::new(Ring::new());
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<String, String>>();
        let (st, rg) = (stop.clone(), ring.clone());
        let thread = std::thread::Builder::new()
            .name("wasapi-loopback".into())
            .spawn(move || {
                crate::airplay::session::realtime_thread("capture");
                if let Err(e) = run(st, rg, &init_tx) {
                    init_tx.send(Err(e)).ok();
                }
            })
            .map_err(|e| e.to_string())?;
        let format_note = init_rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "capture thread did not start".to_string())??;
        Ok(Self { stop, thread: Some(thread), ring, format_note })
    }

    /// Per-process loopback: only what `pid` and its child processes play
    /// (Windows 10 2004+, `ActivateAudioInterfaceAsync` on the process-loopback
    /// virtual device). DeetsMusic passes its own pid: the sound comes out of
    /// the WebView2 children, which the "include tree" mode covers. Unlike
    /// [`Capture::start`], nothing keeps the engine ticking while the process
    /// is silent — packets simply stop, and the pacer pads with silence.
    pub fn start_process(pid: u32) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let ring = Arc::new(Ring::new());
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<String, String>>();
        let (st, rg) = (stop.clone(), ring.clone());
        let thread = std::thread::Builder::new()
            .name("wasapi-process-loopback".into())
            .spawn(move || {
                crate::airplay::session::realtime_thread("capture");
                if let Err(e) = process::run(pid, st, rg, &init_tx) {
                    init_tx.send(Err(e)).ok();
                }
            })
            .map_err(|e| e.to_string())?;
        let format_note = init_rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "capture thread did not start".to_string())??;
        Ok(Self { stop, thread: Some(thread), ring, format_note })
    }

    /// (frames delivered, frames that were not silence) since start.
    pub fn stats(&self) -> (u64, u64) {
        (self.ring.frames_in.load(Ordering::Relaxed), self.ring.frames_loud.load(Ordering::Relaxed))
    }

    /// Largest |sample| (0..32767) since the last call, then reset.
    pub fn take_peak(&self) -> u64 {
        self.ring.peak.swap(0, Ordering::Relaxed)
    }

    /// A pacer source that drains this capture.
    pub fn source(&self) -> crate::airplay::session::Source {
        let ring = self.ring.clone();
        Box::new(move |dst: &mut [i16]| ring.fill(dst))
    }
}

/// The per-process path. Kept apart from `run` because the device-loopback
/// tricks (silent render stream, mix-format fallback) do not apply: the
/// process-loopback virtual device has no mix format and converts to whatever
/// we ask for.
mod process {
    use super::*;
    use std::mem::ManuallyDrop;
    use windows::core::{implement, Interface, Ref};
    use windows::Win32::Media::Audio::{
        ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
        IActivateAudioInterfaceCompletionHandler_Impl, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
        AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    };
    use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
    use windows::Win32::System::Com::BLOB;
    use windows::Win32::System::Variant::VT_BLOB;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Media::Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    /// The activation completes on a worker thread; we just wait for the signal.
    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct Done(std::sync::mpsc::Sender<()>);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
        fn ActivateCompleted(&self, _op: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
            self.0.send(()).ok();
            Ok(())
        }
    }

    fn activate(pid: u32) -> Result<IAudioClient, String> {
        unsafe {
            let params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            // A VT_BLOB that borrows `params`. NEVER let this PROPVARIANT drop:
            // windows-rs runs PropVariantClear on it, which CoTaskMemFrees the
            // blob pointer — our stack — and corrupts the heap (found on the desk,
            // STATUS_HEAP_CORRUPTION on connect). ManuallyDrop, forever.
            let pv = ManuallyDrop::new(PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            blob: BLOB {
                                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                                pBlobData: &params as *const _ as *mut u8,
                            },
                        },
                    }),
                },
            });
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let handler: IActivateAudioInterfaceCompletionHandler = Done(tx).into();
            let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)
                .map_err(|e| format!("ActivateAudioInterfaceAsync: {e}"))?;
            rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "process loopback activation timed out".to_string())?;
            let mut hr = windows::core::HRESULT(0);
            let mut unk: Option<windows::core::IUnknown> = None;
            op.GetActivateResult(&mut hr, &mut unk).map_err(|e| format!("GetActivateResult: {e}"))?;
            hr.ok().map_err(|e| format!("process loopback activation failed: {e}"))?;
            let unk = unk.ok_or("process loopback: no interface")?;
            unk.cast::<IAudioClient>().map_err(|e| format!("IAudioClient cast: {e}"))
        }
    }

    pub(super) fn run(pid: u32, stop: Arc<AtomicBool>, ring: Arc<Ring>, init: &std::sync::mpsc::Sender<Result<String, String>>) -> Result<(), String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| format!("CoInitializeEx: {e}"))?;
            let client = activate(pid)?;
            let want = pcm_44100();
            // Exactly the shape of Microsoft's ApplicationLoopback sample: the
            // virtual device converts to the requested format itself, and the
            // stream is EVENT-DRIVEN — polled, it delivers nothing.
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &want, None)
                .map_err(|e| format!("Initialize (process loopback): {e}"))?;
            let event = CreateEventW(None, false, false, None).map_err(|e| format!("CreateEvent: {e}"))?;
            client.SetEventHandle(event).map_err(|e| format!("SetEventHandle: {e}"))?;
            let capture_client: IAudioCaptureClient = client.GetService().map_err(|e| format!("IAudioCaptureClient: {e}"))?;
            client.Start().map_err(|e| format!("Start (process loopback): {e}"))?;
            init.send(Ok(format!("process {pid} + children, 44.1 kHz / 16-bit, event-driven"))).ok();

            let mut out: Vec<i16> = Vec::with_capacity(8192);
            while !stop.load(Ordering::Relaxed) {
                // Woken per packet; the timeout keeps `stop` honoured while the
                // target is silent (no packets, no events).
                WaitForSingleObject(event, 100);
                loop {
                    let next = capture_client.GetNextPacketSize().unwrap_or(0);
                    if next == 0 {
                        break;
                    }
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    if capture_client.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                        break;
                    }
                    out.clear();
                    // SILENT means "the buffer holds nothing meaningful": on the
                    // process-loopback device it is uninitialised memory, not zeros.
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        out.resize(frames as usize * CHANNELS, 0);
                    } else {
                        out.extend_from_slice(std::slice::from_raw_parts(data as *const i16, frames as usize * CHANNELS));
                    }
                    capture_client.ReleaseBuffer(frames).ok();
                    ring.push(&out);
                }
            }
            client.Stop().ok();
            CloseHandle(event).ok();
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }
}

enum Convert {
    /// The engine gave us 44.1k/16/2 already.
    None,
    /// Mix format: sample rate, channels, float or i16.
    Manual { rate: u32, channels: usize, float: bool, phase: f64, carry: Vec<(f32, f32)> },
}

fn pcm_44100() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * (CHANNELS as u32) * 2,
        nBlockAlign: (CHANNELS * 2) as u16,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

fn run(stop: Arc<AtomicBool>, ring: Arc<Ring>, init: &std::sync::mpsc::Sender<Result<String, String>>) -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| format!("CoInitializeEx: {e}"))?;
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| format!("default render device: {e}"))?;

        // ── silent render stream keeps the engine (and loopback) ticking ──
        let render: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioClient (render): {e}"))?;
        let mix = render.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?;
        render
            .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix, None)
            .map_err(|e| format!("Initialize (silent render): {e}"))?;
        let render_client: IAudioRenderClient = render.GetService().map_err(|e| format!("IAudioRenderClient: {e}"))?;
        let render_frames = render.GetBufferSize().map_err(|e| e.to_string())?;
        render.Start().map_err(|e| format!("Start (render): {e}"))?;

        // ── loopback capture ──
        let first_try: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioClient (capture): {e}"))?;
        let want = pcm_44100();
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let (capture, mut convert, note): (IAudioClient, Convert, String) =
            match first_try.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &want, None) {
                Ok(()) => (first_try, Convert::None, "44.1 kHz / 16-bit via engine conversion".to_string()),
                Err(first) => {
                    // Fall back to the mix format and convert ourselves.
                    drop(first_try);
                    let retry: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("IAudioClient (capture, retry): {e}"))?;
                    let f = &*mix;
                    let tag = f.wFormatTag as u32;
                    let bits = f.wBitsPerSample;
                    let float = if tag == WAVE_FORMAT_EXTENSIBLE {
                        // packed struct: copy the GUID out before comparing
                        let ext = mix as *const WAVEFORMATEXTENSIBLE;
                        std::ptr::addr_of!((*ext).SubFormat).read_unaligned() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                    } else {
                        tag == WAVE_FORMAT_IEEE_FLOAT
                    };
                    if !float && bits != 16 {
                        return Err(format!("loopback: engine refused 44.1k/16 ({first}) and the mix format is {bits}-bit int, which is not handled"));
                    }
                    retry
                        .Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 200_000, 0, mix, None)
                        .map_err(|e| format!("Initialize (loopback, mix format): {e}"))?;
                    let rate = f.nSamplesPerSec;
                    let channels = f.nChannels as usize;
                    let note = format!("mix format {rate} Hz / {channels} ch / {} → converted in software", if float { "float" } else { "16-bit" });
                    (retry, Convert::Manual { rate, channels, float, phase: 0.0, carry: Vec::new() }, note)
                }
            };
        CoTaskMemFree(Some(mix as *const _));
        let capture_client: IAudioCaptureClient = capture.GetService().map_err(|e| format!("IAudioCaptureClient: {e}"))?;
        capture.Start().map_err(|e| format!("Start (capture): {e}"))?;
        init.send(Ok(note)).ok();

        let mut out: Vec<i16> = Vec::with_capacity(8192);
        while !stop.load(Ordering::Relaxed) {
            // Keep the silent render buffer topped up.
            if let Ok(padding) = render.GetCurrentPadding() {
                let free = render_frames.saturating_sub(padding);
                if free > 0 {
                    if let Ok(_) = render_client.GetBuffer(free) {
                        render_client.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).ok();
                    }
                }
            }
            // Drain every pending capture packet.
            loop {
                let next = capture_client.GetNextPacketSize().unwrap_or(0);
                if next == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                if capture_client.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break;
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                out.clear();
                match &mut convert {
                    Convert::None => {
                        if silent {
                            out.resize(frames as usize * CHANNELS, 0);
                        } else {
                            let src = std::slice::from_raw_parts(data as *const i16, frames as usize * CHANNELS);
                            out.extend_from_slice(src);
                        }
                    }
                    Convert::Manual { rate, channels, float, phase, carry } => {
                        // Read stereo pairs as f32.
                        let n = frames as usize;
                        let mut pairs: Vec<(f32, f32)> = std::mem::take(carry);
                        pairs.reserve(n);
                        for i in 0..n {
                            if silent {
                                pairs.push((0.0, 0.0));
                                continue;
                            }
                            let (l, r) = if *float {
                                let s = std::slice::from_raw_parts(data as *const f32, n * *channels);
                                (s[i * *channels], if *channels > 1 { s[i * *channels + 1] } else { s[i * *channels] })
                            } else {
                                let s = std::slice::from_raw_parts(data as *const i16, n * *channels);
                                (s[i * *channels] as f32 / 32768.0, if *channels > 1 { s[i * *channels + 1] as f32 / 32768.0 } else { s[i * *channels] as f32 / 32768.0 })
                            };
                            pairs.push((l, r));
                        }
                        // Linear resample rate → 44100.
                        let step = *rate as f64 / SAMPLE_RATE as f64;
                        while (*phase as usize) + 1 < pairs.len() {
                            let i0 = *phase as usize;
                            let frac = (*phase - i0 as f64) as f32;
                            let (a, b) = (pairs[i0], pairs[i0 + 1]);
                            let l = a.0 + (b.0 - a.0) * frac;
                            let r = a.1 + (b.1 - a.1) * frac;
                            out.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
                            out.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
                            *phase += step;
                        }
                        let keep = (*phase as usize).min(pairs.len());
                        *carry = pairs.split_off(keep);
                        *phase -= keep as f64;
                    }
                }
                capture_client.ReleaseBuffer(frames).ok();
                ring.push(&out);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        capture.Stop().ok();
        render.Stop().ok();
    }
    Ok(())
}

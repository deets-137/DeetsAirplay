//! Sample-rate conversion and 16-bit quantization for the capture, hand-rolled.
//!
//! - [`Linear`]: the capture's fallback until 2026-09-16 (two-point
//!   interpolation, truncating to i16). No longer used by the capture; kept so
//!   `probe fidelity` can show what it cost (aliases at -5 dBc near 20 kHz).
//! - [`Sinc`]: a polyphase Kaiser-windowed sinc for one fixed rational ratio
//!   (48 000 → 44 100 is exactly 160 → 147), so it never drifts. Passband to
//!   0.907 × the lower Nyquist (20 kHz at 44.1 kHz), full stopband at that
//!   Nyquist, [`STOPBAND_DB`] of rejection.
//! - [`Quantizer`]: f32 → i16, rounded, with optional TPDF dither.
//!
//! All three work on stereo pairs in streaming chunks of any size: state
//! carries across calls, so a chunked run equals a one-shot run.

/// Alias rejection of [`Sinc`], in dB. Sets the Kaiser β and the tap count.
pub const STOPBAND_DB: f64 = 100.0;

pub type Frame = (f32, f32);

/// The capture fallback's old linear resampler, moved here from `capture.rs`
/// unchanged: interpolate between neighbours, clamp, truncate to i16.
pub struct Linear {
    step: f64,
    phase: f64,
    carry: Vec<Frame>,
}

impl Linear {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, phase: 0.0, carry: Vec::new() }
    }

    /// Append interleaved i16 output for `input` to `out`.
    pub fn process(&mut self, input: &[Frame], out: &mut Vec<i16>) {
        let mut pairs: Vec<Frame> = std::mem::take(&mut self.carry);
        pairs.extend_from_slice(input);
        while (self.phase as usize) + 1 < pairs.len() {
            let i0 = self.phase as usize;
            let frac = (self.phase - i0 as f64) as f32;
            let (a, b) = (pairs[i0], pairs[i0 + 1]);
            let l = a.0 + (b.0 - a.0) * frac;
            let r = a.1 + (b.1 - a.1) * frac;
            out.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
            out.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
            self.phase += self.step;
        }
        let keep = (self.phase as usize).min(pairs.len());
        self.carry = pairs.split_off(keep);
        self.phase -= keep as f64;
    }
}

/// Polyphase windowed-sinc resampler for a fixed `in_rate → out_rate`.
pub struct Sinc {
    /// Up factor: phases per input sample.
    up: usize,
    /// Down factor: phase advance per output sample.
    down: usize,
    taps: usize,
    half: usize,
    /// `up` rows of `taps` coefficients, each row normalised to unity DC gain.
    table: Vec<f32>,
    hist: Vec<Frame>,
    /// Position of the next output, in 1/`up` input samples, from `hist[0]`.
    pos: usize,
    bypass: bool,
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Modified Bessel function of the first kind, order 0 (series).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, q) = (1.0, 1.0, x * x / 4.0);
    for k in 1..64 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl Sinc {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let g = gcd(in_rate, out_rate);
        let (up, down) = ((out_rate / g) as usize, (in_rate / g) as usize);
        let nyquist = in_rate.min(out_rate) as f64 / 2.0;
        let pass = 0.907 * nyquist;
        let cutoff = (pass + nyquist) / 2.0 / in_rate as f64; // cycles per input sample
        let transition = (nyquist - pass) / in_rate as f64;
        // Kaiser's estimates: β for the rejection, length for the transition width.
        let beta = 0.1102 * (STOPBAND_DB - 8.7);
        let len = ((STOPBAND_DB - 7.95) / (14.36 * transition)).ceil() as usize;
        let taps = (len + 1) & !1; // even: `half` either side of the output instant
        let half = taps / 2;
        let i0_beta = bessel_i0(beta);
        let mut table = vec![0f32; up * taps];
        for p in 0..up {
            let row = &mut table[p * taps..(p + 1) * taps];
            let mut coeffs = vec![0f64; taps];
            for (j, c) in coeffs.iter_mut().enumerate() {
                // Distance from this tap's input sample to the output instant.
                let d = (j as f64 + 1.0 - half as f64) - p as f64 / up as f64;
                let x = 2.0 * cutoff * d;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                let r = d / half as f64;
                let window = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - r * r).sqrt()) / i0_beta };
                *c = sinc * window;
            }
            let sum: f64 = coeffs.iter().sum();
            for (dst, c) in row.iter_mut().zip(coeffs) {
                *dst = (c / sum) as f32;
            }
        }
        Self {
            up,
            down,
            taps,
            half,
            table,
            // Zeros before the first input, so the first output is centred on it.
            hist: vec![(0.0, 0.0); half - 1],
            pos: (half - 1) * up,
            bypass: in_rate == out_rate,
        }
    }

    /// Group delay in input samples (what the filter adds to latency).
    pub fn delay_frames(&self) -> usize {
        if self.bypass { 0 } else { self.half }
    }

    pub fn taps(&self) -> usize {
        self.taps
    }

    /// Append the resampled frames for `input` to `out`.
    pub fn process(&mut self, input: &[Frame], out: &mut Vec<Frame>) {
        if self.bypass {
            out.extend_from_slice(input);
            return;
        }
        self.hist.extend_from_slice(input);
        while self.pos / self.up + self.half < self.hist.len() {
            let (i, p) = (self.pos / self.up, self.pos % self.up);
            let row = &self.table[p * self.taps..(p + 1) * self.taps];
            let src = &self.hist[i + 1 - self.half..i + 1 + self.half];
            let (mut l, mut r) = (0f32, 0f32);
            for (c, s) in row.iter().zip(src) {
                l += c * s.0;
                r += c * s.1;
            }
            out.push((l, r));
            self.pos += self.down;
        }
        let drop = (self.pos / self.up + 1).saturating_sub(self.half).min(self.hist.len());
        self.hist.drain(..drop);
        self.pos -= drop * self.up;
    }
}

/// f32 in [-1, 1] → i16, rounded to nearest; with `dither`, triangular (TPDF)
/// noise of ±1 LSB is added first, which turns quantization distortion into a
/// flat noise floor.
pub struct Quantizer {
    dither: bool,
    rng: u64,
}

impl Quantizer {
    pub fn new(dither: bool) -> Self {
        Self { dither, rng: 0x9E37_79B9_7F4A_7C15 }
    }

    /// xorshift64*: uniform in [0, 1). Not for anything secret.
    fn uniform(&mut self) -> f32 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    pub fn sample(&mut self, s: f32) -> i16 {
        let mut v = s * 32767.0;
        if self.dither {
            v += self.uniform() - self.uniform();
        }
        v.round().clamp(-32768.0, 32767.0) as i16
    }

    /// Append interleaved i16 for `frames` to `out`.
    pub fn process(&mut self, frames: &[Frame], out: &mut Vec<i16>) {
        for &(l, r) in frames {
            out.push(self.sample(l));
            out.push(self.sample(r));
        }
    }
}

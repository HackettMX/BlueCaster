//! A polyphase windowed-sinc resampler for the uplink, used only when the
//! capture does not deliver the voice rate itself (Windows normally
//! resamples). The low-pass sits below the lower of the two Nyquist
//! frequencies, so downsampling does not alias.

/// Zero crossings of the sinc on each side of its centre, at the cut-off
const ZERO_CROSSINGS: f64 = 16.0;
/// Cut-off as a fraction of the lower Nyquist frequency
const ROLLOFF: f64 = 0.9;
/// Kaiser window beta (about 80 dB stop band)
const KAISER_BETA: f64 = 8.6;
/// Most filter phases kept; a ratio needing more uses the nearest phase
const MAX_PHASES: usize = 512;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Modified Bessel function of the first kind, order 0
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..64 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

/// Mono resampler from `from` Hz to `to` Hz, streaming
#[derive(Debug)]
pub(crate) struct Resampler {
    /// Output step in input samples: `step_int + step_rem / den`
    step_int: usize,
    step_rem: u64,
    den: u64,
    phases: usize,
    taps: usize,
    /// `phases` rows of `taps` coefficients
    table: Vec<f32>,
    /// Input not consumed yet; `buf[pos]` is the input sample at the
    /// current output position's integer part
    buf: Vec<f32>,
    pos: usize,
    frac: u64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Resampler {
        assert!(from > 0 && to > 0);
        let g = gcd(u64::from(from), u64::from(to));
        // One output sample advances the input by from / to samples
        let (num, den) = (u64::from(from) / g, u64::from(to) / g);
        let phases = (den as usize).min(MAX_PHASES);
        // Cut-off in cycles per input sample
        let fc = 0.5 * (f64::from(to) / f64::from(from)).min(1.0) * ROLLOFF;
        let half = (ZERO_CROSSINGS / (2.0 * fc)).ceil() as usize;
        let taps = 2 * half;
        let mut table = vec![0f32; phases * taps];
        let norm = bessel_i0(KAISER_BETA);
        for p in 0..phases {
            let offset = p as f64 / phases as f64;
            let row = &mut table[p * taps..(p + 1) * taps];
            let mut sum = 0.0;
            let mut coeffs = vec![0f64; taps];
            for (k, c) in coeffs.iter_mut().enumerate() {
                // Distance from the output position to input sample k of the
                // window (which starts half - 1 samples before it)
                let u = offset + (half - 1) as f64 - k as f64;
                let x = 2.0 * fc * u;
                let sinc =
                    if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                let r = u / half as f64;
                let w = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / norm };
                *c = sinc * w;
                sum += *c;
            }
            // Unity gain at DC for every phase
            for (dst, c) in row.iter_mut().zip(coeffs) {
                *dst = (c / sum) as f32;
            }
        }
        Resampler {
            step_int: (num / den) as usize,
            step_rem: num % den,
            den,
            phases,
            taps,
            table,
            // The first output lines up with the first input sample: the
            // window reaches half - 1 samples back, which start as silence
            buf: vec![0.0; half - 1],
            pos: half - 1,
            frac: 0,
        }
    }

    /// Resamples `input`, appending the output to `out`
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        let half = self.taps / 2;
        // Output at `pos` needs input up to pos + half
        while self.pos + half < self.buf.len() {
            let start = self.pos + 1 - half;
            let phase = (self.frac * self.phases as u64 / self.den) as usize;
            let row = &self.table[phase * self.taps..(phase + 1) * self.taps];
            let window = &self.buf[start..start + self.taps];
            out.push(row.iter().zip(window).map(|(c, x)| c * x).sum());
            self.pos += self.step_int;
            self.frac += self.step_rem;
            if self.frac >= self.den {
                self.frac -= self.den;
                self.pos += 1;
            }
        }
        // Keep the history the next output needs
        let keep_from = self.pos.saturating_sub(half - 1).min(self.buf.len());
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.pos -= keep_from;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (0.5 * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as f32).collect()
    }

    /// Level of the `hz` component of `x` (a single-bin DFT), in dB re 0.5
    fn level_db(x: &[f32], rate: u32, hz: f64) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let a = 2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate);
            re += f64::from(v) * a.cos();
            im += f64::from(v) * a.sin();
        }
        let amp = 2.0 * (re * re + im * im).sqrt() / x.len() as f64;
        20.0 * (amp / 0.5).log10()
    }

    fn run(from: u32, to: u32, input: &[f32], block: usize) -> Vec<f32> {
        let mut r = Resampler::new(from, to);
        let mut out = Vec::new();
        for b in input.chunks(block) {
            r.process(b, &mut out);
        }
        out
    }

    /// The steady part of the output: 0.5 s after the first 0.1 s (a whole
    /// number of periods of every test tone)
    fn steady(out: &[f32], to: u32) -> &[f32] {
        let tenth = to as usize / 10;
        &out[tenth..tenth + 5 * tenth]
    }

    #[test]
    fn passes_1k_within_1_db() {
        let out = run(48_000, 16_000, &tone(48_000, 1000.0, 48_000), 480);
        let l = level_db(steady(&out, 16_000), 16_000, 1000.0);
        assert!(l.abs() < 1.0, "1 kHz at {l:.2} dB");
    }

    #[test]
    fn does_not_alias_when_downsampling() {
        // 9 kHz is above the 8 kHz Nyquist of 16 kHz: without the low-pass
        // it would fold to 7 kHz
        let out = run(48_000, 16_000, &tone(48_000, 9000.0, 48_000), 441);
        let l = level_db(steady(&out, 16_000), 16_000, 7000.0);
        assert!(l < -40.0, "9 kHz folded to 7 kHz at {l:.1} dB");
        // The upper voice band stays
        let out = run(48_000, 16_000, &tone(48_000, 5000.0, 48_000), 441);
        let l = level_db(steady(&out, 16_000), 16_000, 5000.0);
        assert!(l.abs() < 1.0, "5 kHz at {l:.2} dB");
    }

    #[test]
    fn output_length_follows_the_ratio() {
        for (from, to) in [(48_000, 16_000), (44_100, 16_000), (44_100, 32_000), (16_000, 32_000), (8000, 16_000)] {
            let out = run(from, to, &vec![0.0; from as usize], 333);
            let expected = to as usize;
            let half = Resampler::new(from, to).taps / 2;
            // Short by the look-ahead of the filter only
            let lag = (half as f64 * f64::from(to) / f64::from(from)).ceil() as usize + 1;
            assert!(out.len() <= expected && out.len() + lag >= expected, "{from}->{to}: {}", out.len());
        }
    }

    #[test]
    fn upsampling_keeps_a_tone() {
        let out = run(16_000, 32_000, &tone(16_000, 1000.0, 16_000), 160);
        let l = level_db(steady(&out, 32_000), 32_000, 1000.0);
        assert!(l.abs() < 1.0, "{l:.2} dB");
    }

    #[test]
    fn odd_ratio_uses_nearest_phase() {
        // 44056 / 16000: more phases than kept
        let r = Resampler::new(44_056, 16_000);
        assert_eq!(r.phases, MAX_PHASES);
        let out = run(44_056, 16_000, &tone(44_056, 1000.0, 44_056), 441);
        let l = level_db(steady(&out, 16_000), 16_000, 1000.0);
        assert!(l.abs() < 1.0, "{l:.2} dB");
    }
}

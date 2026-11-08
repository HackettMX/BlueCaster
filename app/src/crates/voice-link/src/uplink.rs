//! The uplink: a capture reduced to mono 16-bit PCM at the voice rate, from
//! the capture thread ([`UplinkSink`]) to whoever encodes it for the
//! headphones ([`UplinkConsumer`], on the BTstack thread).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use audio_capture::{Format, SampleFormat, Sink};

use crate::drift::Primed;
use crate::resample::Resampler;

/// Queued before the first samples go out, and again after an underrun:
/// a 10 ms capture packet, a 7.5 ms SCO pull and scheduling jitter
pub(crate) const PREFILL_MS: u32 = 30;
/// How far the queue may stray from the prefill before the drift control acts
pub(crate) const TOLERANCE_MS: u32 = 10;

/// Counters of an uplink
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UplinkStats {
    /// Samples delivered by the capture (after conversion)
    pub captured: u64,
    /// Samples the queue had no room for
    pub overflow: u64,
    /// Samples popped
    pub popped: u64,
    /// Zero samples popped because the queue was short (start and underruns)
    pub zeros: u64,
    /// Pops the queue could not satisfy, after it had been filled
    pub underruns: u64,
    /// Samples dropped (queue too full) and repeated (too low) by the drift control
    pub dropped: u64,
    pub duplicated: u64,
}

#[derive(Debug, Default)]
struct Shared {
    captured: AtomicU64,
    overflow: AtomicU64,
    popped: AtomicU64,
    zeros: AtomicU64,
    underruns: AtomicU64,
    dropped: AtomicU64,
    duplicated: AtomicU64,
}

impl Shared {
    fn stats(&self) -> UplinkStats {
        UplinkStats {
            captured: self.captured.load(Ordering::Relaxed),
            overflow: self.overflow.load(Ordering::Relaxed),
            popped: self.popped.load(Ordering::Relaxed),
            zeros: self.zeros.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            duplicated: self.duplicated.load(Ordering::Relaxed),
        }
    }
}

/// Converts captured audio to mono i16 at the voice rate. Implements
/// [`audio_capture::Sink`]: open the capture with `preferred_rate` = the
/// voice rate so Windows resamples; the sink then only converts the sample
/// type and downmixes (the average of all channels). A capture at another
/// rate goes through a windowed-sinc resampler.
pub struct UplinkSink {
    rate: u32,
    producer: rtrb::Producer<i16>,
    shared: Arc<Shared>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    /// For the capture rate it was made for
    resampler: Option<(u32, Resampler)>,
}

/// The encoder's side of an uplink
pub struct UplinkConsumer {
    rate: u32,
    consumer: rtrb::Consumer<i16>,
    shared: Arc<Shared>,
    primed: Primed,
    /// The microphone test: its recording plays in place of the capture
    tap: Option<crate::MicTap>,
}

impl UplinkSink {
    /// An uplink at `rate` Hz whose queue holds `capacity_ms` (at least
    /// 60 ms, so the prefill and a capture packet fit)
    pub fn new(rate: u32, capacity_ms: u32) -> (UplinkSink, UplinkConsumer) {
        assert!(rate > 0);
        let capacity = (rate as usize * capacity_ms.max(60) as usize / 1000).max(1);
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let shared = Arc::new(Shared::default());
        let prefill = (rate * PREFILL_MS / 1000) as usize;
        let tolerance = (rate * TOLERANCE_MS / 1000) as usize;
        let sink = UplinkSink {
            rate,
            producer,
            shared: shared.clone(),
            mono: Vec::with_capacity(rate as usize / 10),
            resampled: Vec::with_capacity(rate as usize / 10),
            resampler: None,
        };
        let consumer =
            UplinkConsumer { rate, consumer, shared, primed: Primed::new(rate, prefill, tolerance), tap: None };
        (sink, consumer)
    }

    fn push(&mut self, input_rate: u32) {
        let samples = if input_rate == self.rate {
            self.resampler = None;
            &self.mono
        } else {
            if self.resampler.as_ref().is_none_or(|(r, _)| *r != input_rate) {
                tracing::debug!("voice link: uplink resamples {input_rate} Hz to {} Hz", self.rate);
                self.resampler = Some((input_rate, Resampler::new(input_rate, self.rate)));
            }
            self.resampled.clear();
            let (_, r) = self.resampler.as_mut().expect("set above");
            r.process(&self.mono, &mut self.resampled);
            &self.resampled
        };
        let n = samples.len();
        self.shared.captured.fetch_add(n as u64, Ordering::Relaxed);
        let free = self.producer.slots().min(n);
        if free < n {
            self.shared.overflow.fetch_add((n - free) as u64, Ordering::Relaxed);
        }
        if free > 0
            && let Ok(chunk) = self.producer.write_chunk_uninit(free)
        {
            chunk.fill_from_iter(samples.iter().map(|&s| to_i16(s)));
        }
    }
}

/// f32 (nominally -1..1) to i16, rounded and clamped
pub(crate) fn to_i16(s: f32) -> i16 {
    (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// Averages the channels of `frames` interleaved frames of `format` into `out`
fn downmix(data: &[u8], frames: usize, format: Format, out: &mut Vec<f32>) {
    out.clear();
    let ch = usize::from(format.channels.max(1));
    let bps = format.sample_format.bytes_per_sample();
    let frames = frames.min(data.len() / (ch * bps));
    let scale = 1.0 / ch as f32;
    for frame in data[..frames * ch * bps].chunks_exact(ch * bps) {
        let sum: f32 = frame
            .chunks_exact(bps)
            .map(|b| match format.sample_format {
                SampleFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                SampleFormat::I16 => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0,
                SampleFormat::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            })
            .sum();
        out.push(sum * scale);
    }
}

impl Sink for UplinkSink {
    fn audio(&mut self, data: &[u8], frames: usize, format: Format) {
        downmix(data, frames, format, &mut self.mono);
        self.push(format.sample_rate);
    }

    fn silence(&mut self, frames: usize, format: Format) {
        self.mono.clear();
        self.mono.resize(frames, 0.0);
        self.push(format.sample_rate);
    }
}

impl UplinkConsumer {
    /// The voice rate
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Pops up to `out.len()` mono samples; fills the rest with zeros when
    /// short and counts an underrun. Never blocks. Returns the samples that
    /// came from the capture.
    ///
    /// Zeros also come until the queue holds the prefill (30 ms), at the
    /// start and after an underrun, and a slow drift control drops or
    /// repeats a sample now and then to keep the queue near the prefill
    /// (the capture runs on another clock than the Bluetooth link).
    pub fn pop(&mut self, out: &mut [i16]) -> usize {
        let r = self.primed.pull(&mut self.consumer, out);
        let s = &self.shared;
        s.popped.fetch_add(r.got as u64, Ordering::Relaxed);
        s.zeros.fetch_add(r.zeros as u64, Ordering::Relaxed);
        s.underruns.fetch_add(u64::from(r.underrun || r.reprimed), Ordering::Relaxed);
        s.dropped.fetch_add(r.dropped as u64, Ordering::Relaxed);
        s.duplicated.fetch_add(r.duplicated as u64, Ordering::Relaxed);
        // The capture keeps flowing (its queue stays primed) while the
        // microphone test's recording replaces it
        if let Some(tap) = &self.tap {
            tap.play_into(self.rate, out);
        }
        r.got
    }

    /// While `tap` plays its recording, it goes to the headphones instead of
    /// the capture
    pub fn set_tap(&mut self, tap: crate::MicTap) {
        self.tap = Some(tap);
    }

    /// Samples waiting in the queue
    pub fn queued(&self) -> usize {
        self.consumer.slots()
    }

    pub fn stats(&self) -> UplinkStats {
        self.shared.stats()
    }
}

impl std::fmt::Debug for UplinkSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UplinkSink").field("rate", &self.rate).finish()
    }
}

impl std::fmt::Debug for UplinkConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UplinkConsumer").field("rate", &self.rate).field("stats", &self.stats()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_stereo(frames: &[(f32, f32)]) -> Vec<u8> {
        frames.iter().flat_map(|&(l, r)| l.to_le_bytes().into_iter().chain(r.to_le_bytes())).collect()
    }

    #[test]
    fn converts_and_downmixes() {
        let (mut sink, mut cons) = UplinkSink::new(16_000, 100);
        let fmt = Format { sample_rate: 16_000, channels: 2, sample_format: SampleFormat::F32 };
        // 30 ms (the prefill) of L=0.5, R=0.25 -> 0.375
        let data = f32_stereo(&vec![(0.5, 0.25); 480]);
        sink.audio(&data, 480, fmt);
        let mut out = [0i16; 480];
        assert_eq!(cons.pop(&mut out), 480);
        assert!(out.iter().all(|&s| s == 12_288), "{:?}", &out[..4]);

        let fmt16 = Format { sample_rate: 16_000, channels: 1, sample_format: SampleFormat::I16 };
        let data: Vec<u8> = (0..400i16).flat_map(|i| (i * 10).to_le_bytes()).collect();
        sink.audio(&data, 400, fmt16);
        let mut out = [0i16; 400];
        assert_eq!(cons.pop(&mut out), 400);
        assert_eq!(out[7], 70);

        let fmt32 = Format { sample_rate: 16_000, channels: 3, sample_format: SampleFormat::I32 };
        let data: Vec<u8> = (0..320 * 3).flat_map(|_| (1i32 << 30).to_le_bytes()).collect();
        sink.audio(&data, 320, fmt32);
        let mut out = [0i16; 320];
        assert_eq!(cons.pop(&mut out), 320);
        assert_eq!(out[0], 16_384);
    }

    #[test]
    fn prefill_then_underrun_counts_and_zero_fill() {
        let (mut sink, mut cons) = UplinkSink::new(16_000, 100);
        let fmt = Format { sample_rate: 16_000, channels: 1, sample_format: SampleFormat::I16 };
        let data: Vec<u8> = (0..160).flat_map(|_| 1000i16.to_le_bytes()).collect();
        // 10 ms: less than the prefill, zeros come
        sink.audio(&data, 160, fmt);
        let mut out = [7i16; 120];
        assert_eq!(cons.pop(&mut out), 0);
        assert!(out.iter().all(|&s| s == 0));
        assert_eq!(cons.stats().underruns, 0);
        sink.audio(&data, 160, fmt);
        sink.audio(&data, 160, fmt);
        // 480 queued: 120, 120, 120, 120, then short
        for _ in 0..4 {
            assert_eq!(cons.pop(&mut out), 120);
        }
        assert_eq!(cons.pop(&mut out), 0);
        let s = cons.stats();
        assert_eq!(s.underruns, 1);
        assert_eq!(s.popped, 480);
        assert_eq!(s.zeros, 240);
    }

    #[test]
    fn silence_is_queued_as_zeros() {
        let (mut sink, mut cons) = UplinkSink::new(16_000, 100);
        let fmt = Format { sample_rate: 16_000, channels: 2, sample_format: SampleFormat::F32 };
        sink.silence(480, fmt);
        let mut out = [1i16; 480];
        assert_eq!(cons.pop(&mut out), 480);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn full_queue_counts_overflow() {
        let (mut sink, cons) = UplinkSink::new(16_000, 60);
        let fmt = Format { sample_rate: 16_000, channels: 1, sample_format: SampleFormat::I16 };
        let data = vec![0u8; 2000 * 2];
        sink.audio(&data, 2000, fmt);
        assert_eq!(cons.queued(), 960);
        assert_eq!(cons.stats().overflow, 2000 - 960);
    }

    #[test]
    fn other_capture_rate_is_resampled() {
        let (mut sink, mut cons) = UplinkSink::new(16_000, 500);
        let fmt = Format { sample_rate: 48_000, channels: 2, sample_format: SampleFormat::F32 };
        let tone: Vec<(f32, f32)> = (0..4800)
            .map(|i| {
                let s = (0.5 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin()) as f32;
                (s, s)
            })
            .collect();
        for block in tone.chunks(480) {
            sink.audio(&f32_stereo(block), block.len(), fmt);
        }
        // 100 ms in at 48 kHz, about 100 ms out at 16 kHz (less the filter delay)
        let q = cons.queued();
        assert!((1500..=1600).contains(&q), "{q}");
        let mut out = vec![0i16; q];
        cons.pop(&mut out);
        let peak = out[800..].iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((15_500..=17_000).contains(&peak), "peak {peak}");
    }

    #[test]
    fn drift_control_keeps_the_queue_near_the_prefill() {
        for (ratio, grows) in [(1.002, true), (0.998, false)] {
            let (mut sink, mut cons) = UplinkSink::new(16_000, 1000);
            let fmt = Format { sample_rate: 16_000, channels: 1, sample_format: SampleFormat::I16 };
            let data = vec![0u8; 160 * 2];
            let mut out = [0i16; 120];
            // The capture clock runs `ratio` times the link's: 10 ms pushes,
            // 7.5 ms pulls, for 60 s
            let mut credit = 400.0;
            let mut max_level = 0;
            for i in 0..8000 {
                while credit >= 160.0 {
                    sink.audio(&data, 160, fmt);
                    credit -= 160.0;
                }
                if i > 400 {
                    max_level = max_level.max(cons.queued());
                }
                cons.pop(&mut out);
                credit += 120.0 * ratio;
            }
            let s = cons.stats();
            if grows {
                // 0.2 % of 960 000 samples
                assert!((1500..=2100).contains(&s.dropped), "{s:?}");
            } else {
                assert!((1500..=2100).contains(&s.duplicated), "{s:?}");
            }
            assert_eq!(s.underruns, 0, "{s:?}");
            // Prefill + tolerance, then two pushes and a pull of jitter on top
            assert!(max_level < 480 + 160 + 2 * 160 + 120, "max {max_level}, {s:?}");
        }
    }
}

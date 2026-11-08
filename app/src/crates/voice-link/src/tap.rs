//! The microphone test's tap: the level of the headset microphone, a short
//! recording of it, and its playback to the headphones in place of the
//! uplink (Diagnostics > Microphone).
//!
//! [`PcmProducer::set_tap`](crate::PcmProducer::set_tap) feeds it every
//! decoded microphone sample; [`UplinkConsumer::set_tap`](crate::UplinkConsumer::set_tap)
//! lets a playback replace what goes to the headphones. Both run on the
//! BTstack (or a decode) thread: the level is an atomic, the recording only
//! tried (a feed that finds it locked skips one packet of the recording,
//! never waits).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, Default)]
struct Rec {
    /// The rate of `samples` (the voice link's), 0 before the first recording
    rate: u32,
    samples: Vec<i16>,
    /// Recording until `samples` holds this many
    recording: Option<usize>,
    /// Playing from this position
    playing: Option<usize>,
}

#[derive(Debug, Default)]
struct Shared {
    /// Peak magnitude (0..=32768) of the microphone since the last read
    peak: AtomicU32,
    /// ... of the playback
    play_peak: AtomicU32,
    rec: Mutex<Rec>,
}

/// The tap; clones share it
#[derive(Clone, Debug, Default)]
pub struct MicTap(Arc<Shared>);

fn peak_of(pcm: &[i16]) -> u32 {
    pcm.iter().map(|&s| (s as i32).unsigned_abs()).max().unwrap_or(0)
}

fn level(peak: u32) -> f32 {
    (peak as f32 / 32768.0).min(1.0)
}

impl MicTap {
    pub fn new() -> MicTap {
        MicTap::default()
    }

    fn rec(&self) -> MutexGuard<'_, Rec> {
        self.0.rec.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Microphone samples at `rate` Hz (the producer's side)
    pub fn feed(&self, rate: u32, pcm: &[i16]) {
        self.0.peak.fetch_max(peak_of(pcm), Ordering::Relaxed);
        let Ok(mut rec) = self.0.rec.try_lock() else { return };
        let Some(max) = rec.recording else { return };
        if rec.rate != rate {
            // The link came up again with another codec: what was recorded
            // would play at the wrong speed
            rec.samples.clear();
            rec.rate = rate;
        }
        let n = pcm.len().min(max.saturating_sub(rec.samples.len()));
        rec.samples.extend_from_slice(&pcm[..n]);
        if rec.samples.len() >= max {
            rec.recording = None;
        }
    }

    /// The microphone's peak level (0..=1) since the last call
    pub fn take_level(&self) -> f32 {
        level(self.0.peak.swap(0, Ordering::Relaxed))
    }

    /// The playback's peak level (0..=1) since the last call
    pub fn take_play_level(&self) -> f32 {
        level(self.0.play_peak.swap(0, Ordering::Relaxed))
    }

    /// Starts a new recording at `rate` Hz of at most `max_ms` (the last
    /// one is dropped)
    pub fn record(&self, rate: u32, max_ms: u32) {
        let mut rec = self.rec();
        let max = (u64::from(rate) * u64::from(max_ms) / 1000) as usize;
        rec.rate = rate;
        rec.samples = Vec::with_capacity(max);
        rec.recording = Some(max);
        rec.playing = None;
    }

    pub fn stop_recording(&self) {
        self.rec().recording = None;
    }

    pub fn recording(&self) -> bool {
        self.rec().recording.is_some()
    }

    /// The length of the recording
    pub fn recorded_ms(&self) -> u32 {
        let rec = self.rec();
        if rec.rate == 0 {
            return 0;
        }
        (rec.samples.len() as u64 * 1000 / u64::from(rec.rate)) as u32
    }

    /// Plays the recording from its start; false when there is none
    pub fn play(&self) -> bool {
        let mut rec = self.rec();
        if rec.samples.is_empty() {
            return false;
        }
        rec.recording = None;
        rec.playing = Some(0);
        true
    }

    pub fn stop_playing(&self) {
        self.rec().playing = None;
    }

    pub fn playing(&self) -> bool {
        self.rec().playing.is_some()
    }

    /// Where the playback is (0 when none plays)
    pub fn played_ms(&self) -> u32 {
        let rec = self.rec();
        match rec.playing {
            Some(pos) if rec.rate > 0 => (pos as u64 * 1000 / u64::from(rec.rate)) as u32,
            _ => 0,
        }
    }

    /// Drops the recording
    pub fn clear(&self) {
        *self.rec() = Rec::default();
        self.0.peak.store(0, Ordering::Relaxed);
        self.0.play_peak.store(0, Ordering::Relaxed);
    }

    /// The uplink's side: while the recording plays (at this `rate`),
    /// replaces `out` with its next samples (silence after its end) and
    /// returns true
    pub fn play_into(&self, rate: u32, out: &mut [i16]) -> bool {
        let Ok(mut rec) = self.0.rec.try_lock() else { return false };
        let Some(pos) = rec.playing else { return false };
        if rec.rate != rate {
            rec.playing = None;
            return false;
        }
        let n = out.len().min(rec.samples.len() - pos);
        out[..n].copy_from_slice(&rec.samples[pos..pos + n]);
        out[n..].fill(0);
        self.0.play_peak.fetch_max(peak_of(&out[..n]), Ordering::Relaxed);
        let next = pos + n;
        rec.playing = (next < rec.samples.len()).then_some(next);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_up_to_its_length_and_plays_it_back_once() {
        let tap = MicTap::new();
        tap.feed(16000, &[1000; 160]);
        // Not recording: only the level
        assert_eq!(tap.recorded_ms(), 0);
        assert!((tap.take_level() - 1000.0 / 32768.0).abs() < 1e-6);
        assert_eq!(tap.take_level(), 0.0);
        tap.record(16000, 20);
        tap.feed(16000, &[-2000; 200]);
        assert!(tap.recording());
        tap.feed(16000, &[3000; 200]);
        // 20 ms = 320 samples: full, recording ends by itself
        assert!(!tap.recording());
        assert_eq!(tap.recorded_ms(), 20);
        assert!(tap.play());
        let mut out = [7i16; 240];
        assert!(tap.play_into(16000, &mut out));
        assert_eq!(out[0], -2000);
        assert_eq!(out[239], 3000);
        assert!(tap.playing());
        assert!(tap.play_into(16000, &mut out));
        assert_eq!(&out[..80], &[3000; 80]);
        assert_eq!(&out[80..], &[0; 160]);
        assert!(!tap.playing());
        assert!(!tap.play_into(16000, &mut out));
        assert!((tap.take_play_level() - 3000.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn another_rate_drops_what_does_not_fit_it() {
        let tap = MicTap::new();
        tap.record(16000, 1000);
        tap.feed(16000, &[1; 100]);
        tap.feed(8000, &[2; 50]);
        assert!(tap.play());
        let mut out = [0i16; 10];
        assert!(!tap.play_into(16000, &mut out));
        assert!(!tap.playing());
        assert!(tap.play());
        assert!(tap.play_into(8000, &mut out));
        assert_eq!(out, [2; 10]);
        tap.clear();
        assert!(!tap.play());
    }
}

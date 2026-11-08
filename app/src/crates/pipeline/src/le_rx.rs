//! The LE Audio microphone (source ASEs of a conversational stream): the
//! "le-decode" thread.
//!
//! BTstack thread --[bt-host's `LeRxHandle`: a lock-free ring of `RxSdu`]-->
//! le-decode thread --[`PcmOut`]--> the microphone's output (core puts the
//! voice link's queue there: its renderer, or its WAV file)
//!
//! Per source stream (a CIS with a source ASE) a sequence-number jitter
//! buffer ([`JitterBuffer`]): it holds the SDUs of the last
//! [`JITTER_SLOTS`] sequence numbers and releases the next one once
//! [`PREFILL_SDUS`] (two SDU intervals) newer ones have been seen, so that
//! SDUs arriving out of order are put back in order and a missing one is
//! concealed (liblc3's PLC) rather than waited for. Lost SDUs often produce
//! no packet at all: the gap in the sequence numbers tells. A SDU with a
//! packet status other than "valid" is concealed too. Every channel of a
//! stream has its own LC3 decoder; all channels of all streams are mixed to
//! mono (the average), e.g. the microphones of two earbuds.
//!
//! No allocation per frame once the thread runs: the slots, decoders and
//! PCM buffers are made at start.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use bt_host::{ISO_STATUS_VALID, LeRxHandle, MAX_RX_SDU, RxSdu};
use codec::lc3::{Lc3ChannelDecoder, Lc3Params};

/// Where the decoded microphone PCM goes: mono 16-bit at the source rate,
/// one LC3 frame (10 or 7.5 ms) per call. Called on the le-decode thread:
/// must not block for long (the voice link's renderer pushes into a ring).
pub trait PcmOut: Send {
    fn push(&mut self, mono_i16: &[i16]);
}

/// SDUs of a source stream the jitter buffer holds (sequence numbers)
pub const JITTER_SLOTS: usize = 16;
/// SDU intervals the jitter buffer waits before it releases an SDU (and
/// before it conceals a missing one)
pub const PREFILL_SDUS: u16 = 2;
/// Decoded frames a stream may be ahead of the others before the mix goes
/// on without them
const MIX_AHEAD_FRAMES: usize = 6;
/// Frames of decoded PCM kept per stream for the mix
const FIFO_FRAMES: usize = 8;
/// How often the thread looks into the ring
const POLL: Duration = Duration::from_millis(2);

/// The counters of the microphone path (shared with the session)
#[derive(Debug, Default)]
pub struct MicStats {
    /// SDUs taken from the ring
    pub sdus: AtomicU64,
    /// Frames decoded from SDUs (per stream)
    pub decoded: AtomicU64,
    /// Frames concealed (lost or invalid SDUs; per stream)
    pub concealed: AtomicU64,
    /// SDUs that came after their sequence number was released
    pub late: AtomicU64,
    /// Sequence jumps past the jitter buffer (the buffer started over)
    pub resyncs: AtomicU64,
    /// Mono frames given to the output
    pub frames_out: AtomicU64,
    /// Decoder errors
    pub errors: AtomicU64,
}

/// A copy of [`MicStats`]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MicCounters {
    pub sdus: u64,
    pub decoded: u64,
    pub concealed: u64,
    pub late: u64,
    pub resyncs: u64,
    pub frames_out: u64,
    pub errors: u64,
}

impl MicStats {
    pub fn snapshot(&self) -> MicCounters {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MicCounters {
            sdus: g(&self.sdus),
            decoded: g(&self.decoded),
            concealed: g(&self.concealed),
            late: g(&self.late),
            resyncs: g(&self.resyncs),
            frames_out: g(&self.frames_out),
            errors: g(&self.errors),
        }
    }
}

impl MicCounters {
    /// "LE Audio: microphone: N frames out (D decoded, C concealed), S SDUs,
    /// L late, R resyncs, E errors" (the emulator's checks read it)
    pub fn message(&self) -> String {
        format!(
            "LE Audio: microphone: {} frames out ({} decoded, {} concealed), {} SDUs, {} late, {} resyncs, {} errors",
            self.frames_out, self.decoded, self.concealed, self.sdus, self.late, self.resyncs, self.errors
        )
    }
}

// ---------------------------------------------------------------------------
// Jitter buffer
// ---------------------------------------------------------------------------

/// What became of an SDU given to the jitter buffer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Stored,
    /// Stored, after a jump past what the buffer holds: it started over
    Resync,
    /// Its sequence number was released already: dropped
    Late,
}

/// What the jitter buffer releases for the next sequence number
#[derive(Debug, PartialEq, Eq)]
pub enum Released<'a> {
    /// The SDU (valid)
    Sdu(&'a [u8]),
    /// Lost or invalid: conceal
    Lost,
}

#[derive(Clone, Copy)]
struct Slot {
    seq: u16,
    filled: bool,
    valid: bool,
    len: u16,
    data: [u8; MAX_RX_SDU],
}

/// The sequence-number jitter buffer of one source stream
pub struct JitterBuffer {
    slots: Box<[Slot; JITTER_SLOTS]>,
    /// The next sequence number to release (None before the first SDU)
    next: Option<u16>,
    /// The newest sequence number seen
    newest: u16,
    prefill: u16,
}

impl JitterBuffer {
    pub fn new(prefill: u16) -> JitterBuffer {
        let empty = Slot { seq: 0, filled: false, valid: false, len: 0, data: [0; MAX_RX_SDU] };
        JitterBuffer {
            slots: Box::new([empty; JITTER_SLOTS]),
            next: None,
            newest: 0,
            prefill: prefill.clamp(1, JITTER_SLOTS as u16 - 1),
        }
    }

    /// An SDU arrived
    pub fn push(&mut self, seq: u16, valid: bool, sdu: &[u8]) -> Pushed {
        let mut resync = false;
        match self.next {
            None => {
                self.next = Some(seq);
                self.newest = seq;
            }
            Some(next) => {
                let ahead = seq.wrapping_sub(next) as i16;
                if ahead < 0 {
                    return Pushed::Late;
                }
                if ahead as usize >= JITTER_SLOTS {
                    // A long gap (or the sender started over): begin anew,
                    // nothing to conceal for so long
                    for s in self.slots.iter_mut() {
                        s.filled = false;
                    }
                    self.next = Some(seq);
                    self.newest = seq;
                    resync = true;
                } else if (seq.wrapping_sub(self.newest) as i16) > 0 {
                    self.newest = seq;
                }
            }
        }
        let s = &mut self.slots[usize::from(seq) % JITTER_SLOTS];
        let n = sdu.len().min(MAX_RX_SDU);
        s.seq = seq;
        s.filled = true;
        s.valid = valid && n > 0;
        s.len = n as u16;
        s.data[..n].copy_from_slice(&sdu[..n]);
        if resync { Pushed::Resync } else { Pushed::Stored }
    }

    /// The next sequence number's SDU, once `prefill` newer ones were seen;
    /// None: wait
    pub fn pop(&mut self) -> Option<Released<'_>> {
        let next = self.next?;
        if self.newest.wrapping_sub(next) < self.prefill || (self.newest.wrapping_sub(next) as i16) < 0 {
            return None;
        }
        self.next = Some(next.wrapping_add(1));
        let s = &mut self.slots[usize::from(next) % JITTER_SLOTS];
        if s.filled && s.seq == next {
            s.filled = false;
            if s.valid {
                return Some(Released::Sdu(&s.data[..usize::from(s.len)]));
            }
        }
        Some(Released::Lost)
    }
}

// ---------------------------------------------------------------------------
// Decoder and mix
// ---------------------------------------------------------------------------

/// The LC3 configuration of the microphone and its streams
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicConfig {
    pub sample_rate: u32,
    pub frame_us: u32,
    /// Per source stream (plan order): (channels, octets per channel)
    pub streams: Vec<(u8, u16)>,
}

impl MicConfig {
    pub fn of(rx: &LeRxHandle) -> MicConfig {
        MicConfig {
            sample_rate: rx.sample_rate,
            frame_us: rx.frame_us,
            streams: rx.streams.iter().map(|s| (s.channels, s.octets)).collect(),
        }
    }
}

struct StreamDec {
    jitter: JitterBuffer,
    decoders: Vec<Lc3ChannelDecoder>,
    octets: usize,
    /// Decoded mono frames waiting for the mix (ring of FIFO_FRAMES)
    fifo: Vec<i16>,
    head: usize,
    queued: usize,
    /// Has produced a frame (a stream that never did does not hold up the
    /// mix)
    active: bool,
}

/// The decode work of the le-decode thread (also driven directly by tests)
pub struct MicDecoder {
    streams: Vec<StreamDec>,
    frame_samples: usize,
    /// One channel's decoded frame
    pcm: Vec<i16>,
    /// One stream's mono frame (sum of its channels)
    acc: Vec<i32>,
    /// The mixed output frame
    out: Vec<i16>,
    stats: Arc<MicStats>,
}

impl MicDecoder {
    pub fn new(config: &MicConfig, prefill: u16, stats: Arc<MicStats>) -> Result<MicDecoder, String> {
        if config.streams.is_empty() {
            return Err("no microphone stream".into());
        }
        let mut streams = Vec::with_capacity(config.streams.len());
        let mut frame_samples = 0;
        for &(channels, octets) in &config.streams {
            let params = Lc3Params { sample_rate: config.sample_rate, frame_us: config.frame_us, octets };
            let mut decoders = Vec::with_capacity(usize::from(channels.max(1)));
            for _ in 0..channels.max(1) {
                let d = Lc3ChannelDecoder::new(params).map_err(|e| {
                    format!("LC3 decoder {} Hz, {} us, {octets} octets: {e}", config.sample_rate, config.frame_us)
                })?;
                frame_samples = d.frame_samples();
                decoders.push(d);
            }
            streams.push(StreamDec {
                jitter: JitterBuffer::new(prefill),
                decoders,
                octets: usize::from(octets),
                fifo: Vec::new(),
                head: 0,
                queued: 0,
                active: false,
            });
        }
        for s in &mut streams {
            s.fifo = vec![0; frame_samples * FIFO_FRAMES];
        }
        Ok(MicDecoder {
            streams,
            frame_samples,
            pcm: vec![0; frame_samples],
            acc: vec![0; frame_samples],
            out: vec![0; frame_samples],
            stats,
        })
    }

    /// PCM samples per frame
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// An SDU from the ring
    pub fn push(&mut self, sdu: &RxSdu) {
        self.stats.sdus.fetch_add(1, Ordering::Relaxed);
        let Some(s) = self.streams.get_mut(usize::from(sdu.stream)) else { return };
        match s.jitter.push(sdu.sequence, sdu.status == ISO_STATUS_VALID, sdu.sdu()) {
            Pushed::Stored => {}
            Pushed::Resync => {
                self.stats.resyncs.fetch_add(1, Ordering::Relaxed);
            }
            Pushed::Late => {
                self.stats.late.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Decodes what the jitter buffers release and gives the mixed frames to
    /// `out` (frame by frame, so that a burst never overflows a FIFO)
    pub fn process(&mut self, out: &mut dyn PcmOut) {
        loop {
            let mut any = false;
            for i in 0..self.streams.len() {
                any |= self.decode_one(i);
            }
            self.mix(out);
            if !any {
                return;
            }
        }
    }

    /// One frame the jitter buffer of stream `i` releases into its FIFO;
    /// false when it releases none
    fn decode_one(&mut self, i: usize) -> bool {
        let n = self.frame_samples;
        let MicDecoder { streams, pcm, acc, stats, .. } = self;
        let s = &mut streams[i];
        let Some(released) = s.jitter.pop() else { return false };
        let (frame, lost) = match released {
            Released::Sdu(sdu) => (Some(sdu), false),
            Released::Lost => (None, true),
        };
        acc.fill(0);
        let mut concealed = lost;
        for (ch, dec) in s.decoders.iter_mut().enumerate() {
            // The channel's frame in the SDU (location order); a short SDU
            // is concealed
            let part = frame.and_then(|f| f.get(ch * s.octets..(ch + 1) * s.octets));
            match dec.decode(part, pcm) {
                Ok(plc) => concealed |= plc,
                Err(_) => {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                    pcm.fill(0);
                }
            }
            for (a, &x) in acc.iter_mut().zip(pcm.iter()) {
                *a += i32::from(x);
            }
        }
        if concealed {
            stats.concealed.fetch_add(1, Ordering::Relaxed);
        } else {
            stats.decoded.fetch_add(1, Ordering::Relaxed);
        }
        // Into the stream's FIFO (the oldest goes when it is full)
        if s.queued == FIFO_FRAMES {
            s.head = (s.head + 1) % FIFO_FRAMES;
            s.queued -= 1;
        }
        let slot = (s.head + s.queued) % FIFO_FRAMES;
        let channels = s.decoders.len() as i32;
        for (dst, &a) in s.fifo[slot * n..(slot + 1) * n].iter_mut().zip(acc.iter()) {
            *dst = (a / channels) as i16;
        }
        s.queued += 1;
        s.active = true;
        true
    }
    /// Mono frames from the streams' FIFOs: when every active stream has one,
    /// or one is too far ahead (the others count as silence then)
    fn mix(&mut self, out: &mut dyn PcmOut) {
        let n = self.frame_samples;
        loop {
            let active = self.streams.iter().filter(|s| s.active).count();
            if active == 0 {
                return;
            }
            let ready = self.streams.iter().filter(|s| s.active && s.queued > 0).count();
            let ahead = self.streams.iter().any(|s| s.queued >= MIX_AHEAD_FRAMES);
            if ready == 0 || (ready < active && !ahead) {
                return;
            }
            self.acc.fill(0);
            for s in self.streams.iter_mut().filter(|s| s.active && s.queued > 0) {
                for (a, &x) in self.acc.iter_mut().zip(&s.fifo[s.head * n..(s.head + 1) * n]) {
                    *a += i32::from(x);
                }
                s.head = (s.head + 1) % FIFO_FRAMES;
                s.queued -= 1;
            }
            let count = active as i32;
            for (o, &a) in self.out.iter_mut().zip(&self.acc) {
                *o = (a / count).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            }
            out.push(&self.out);
            self.stats.frames_out.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

/// The running le-decode thread
pub struct LeRxPipeline {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<MicStats>,
    sample_rate: u32,
}

impl LeRxPipeline {
    /// Starts the le-decode thread on the stream's received SDUs
    pub fn start(rx: LeRxHandle, out: Box<dyn PcmOut>) -> Result<LeRxPipeline, String> {
        let stats = Arc::new(MicStats::default());
        let config = MicConfig::of(&rx);
        let decoder = MicDecoder::new(&config, PREFILL_SDUS, stats.clone())?;
        tracing::info!(
            "LE Audio: microphone: decoding {} stream(s), LC3 {} Hz {} us ({}), mono out",
            config.streams.len(),
            config.sample_rate,
            config.frame_us,
            rx.preset
        );
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let st = stats.clone();
        let thread = std::thread::Builder::new()
            .name("le-decode".into())
            .spawn(move || run(rx.consumer, decoder, out, &s, &st))
            .map_err(|e| format!("le-decode thread: {e}"))?;
        Ok(LeRxPipeline { stop, thread: Some(thread), stats, sample_rate: config.sample_rate })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn stats(&self) -> MicCounters {
        self.stats.snapshot()
    }

    /// Ends the thread (the output is dropped there: a WAV file is
    /// finished) and returns the counters
    pub fn stop(mut self) -> MicCounters {
        self.halt();
        self.stats.snapshot()
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for LeRxPipeline {
    fn drop(&mut self) {
        self.halt();
    }
}

fn run(
    mut consumer: rtrb::Consumer<RxSdu>,
    mut decoder: MicDecoder,
    mut out: Box<dyn PcmOut>,
    stop: &AtomicBool,
    stats: &MicStats,
) {
    while !stop.load(Ordering::Acquire) {
        let mut any = false;
        while let Ok(sdu) = consumer.pop() {
            decoder.push(&sdu);
            any = true;
        }
        if any {
            decoder.process(out.as_mut());
        }
        std::thread::sleep(POLL);
    }
    drop(out);
    tracing::info!("{}", stats.snapshot().message());
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use codec::lc3::Lc3ChannelEncoder;

    use super::*;

    /// Collects the output
    #[derive(Clone, Default)]
    struct Collect(Arc<Mutex<Vec<i16>>>);

    impl PcmOut for Collect {
        fn push(&mut self, mono_i16: &[i16]) {
            self.0.lock().unwrap().extend_from_slice(mono_i16);
        }
    }

    fn sdu(stream: u8, sequence: u16, status: u8, data: &[u8]) -> RxSdu {
        let mut s = RxSdu { stream, sequence, status, len: data.len() as u16, ..RxSdu::default() };
        s.data[..data.len()].copy_from_slice(data);
        s
    }

    #[test]
    fn jitter_buffer_waits_for_the_prefill_and_reorders() {
        let mut j = JitterBuffer::new(2);
        assert_eq!(j.pop(), None);
        assert_eq!(j.push(10, true, &[10]), Pushed::Stored);
        assert_eq!(j.pop(), None);
        // 12 before 11
        j.push(12, true, &[12]);
        assert_eq!(j.pop(), Some(Released::Sdu(&[10])));
        assert_eq!(j.pop(), None, "11 may still come");
        j.push(11, true, &[11]);
        assert_eq!(j.pop(), None);
        j.push(13, true, &[13]);
        assert_eq!(j.pop(), Some(Released::Sdu(&[11])));
        assert_eq!(j.pop(), None);
        // A late duplicate of 11
        assert_eq!(j.push(11, true, &[11]), Pushed::Late);
    }

    #[test]
    fn jitter_buffer_conceals_lost_and_invalid_sdus() {
        let mut j = JitterBuffer::new(2);
        for seq in [0xFFFE, 0xFFFF, 1, 2, 3] {
            j.push(seq, seq != 2, &[seq as u8]);
        }
        // 0 never came (seq wraps), 2 had a bad status
        assert_eq!(j.pop(), Some(Released::Sdu(&[0xFE])));
        assert_eq!(j.pop(), Some(Released::Sdu(&[0xFF])));
        assert_eq!(j.pop(), Some(Released::Lost));
        assert_eq!(j.pop(), Some(Released::Sdu(&[1])));
        assert_eq!(j.pop(), None);
        j.push(4, true, &[]);
        assert_eq!(j.pop(), Some(Released::Lost), "2 had a bad status");
        j.push(5, true, &[5]);
        j.push(6, true, &[6]);
        assert_eq!(j.pop(), Some(Released::Sdu(&[3])));
        assert_eq!(j.pop(), Some(Released::Lost), "4: valid, but no data");
        // A jump past the buffer: it starts over
        assert_eq!(j.push(100, true, &[100]), Pushed::Resync);
        assert_eq!(j.pop(), None);
        j.push(101, true, &[101]);
        j.push(102, true, &[102]);
        assert_eq!(j.pop(), Some(Released::Sdu(&[100])));
    }

    /// LC3 frames of a 1 kHz tone at 16 kHz, 10 ms, 40 octets
    fn tone_frames(count: usize) -> Vec<Vec<u8>> {
        let p = Lc3Params { sample_rate: 16000, frame_us: 10000, octets: 40 };
        let mut enc = Lc3ChannelEncoder::new(p).unwrap();
        (0..count)
            .map(|f| {
                let pcm: Vec<i16> = (0..160)
                    .map(|n| {
                        let t = (f * 160 + n) as f64 / 16000.0;
                        ((2.0 * std::f64::consts::PI * 1000.0 * t).sin() * 10000.0) as i16
                    })
                    .collect();
                let mut out = vec![0u8; 40];
                enc.encode(&pcm, &mut out).unwrap();
                out
            })
            .collect()
    }

    fn config(streams: usize) -> MicConfig {
        MicConfig { sample_rate: 16000, frame_us: 10000, streams: vec![(1, 40); streams] }
    }

    #[test]
    fn decodes_in_order_and_conceals_a_lost_sdu() {
        let stats = Arc::new(MicStats::default());
        let mut dec = MicDecoder::new(&config(1), 2, stats.clone()).unwrap();
        assert_eq!(dec.frame_samples(), 160);
        let out = Collect::default();
        let mut o = out.clone();
        let frames = tone_frames(20);
        for (i, f) in frames.iter().enumerate() {
            if i == 7 {
                continue; // lost: no packet at all
            }
            dec.push(&sdu(0, 500 + i as u16, 0, f));
            dec.process(&mut o);
        }
        // 20 SDUs, 2 still held back: 18 frames, one of them concealed
        let c = stats.snapshot();
        assert_eq!((c.frames_out, c.decoded, c.concealed, c.sdus, c.late), (18, 17, 1, 19, 0));
        assert_eq!(out.0.lock().unwrap().len(), 18 * 160);
        // The tone is there after the decoder settled
        let pcm = out.0.lock().unwrap();
        let peak = pcm[160 * 10..160 * 16].iter().map(|x| x.saturating_abs()).max().unwrap();
        assert!(peak > 5000, "peak {peak}");
        assert!(c.message().starts_with("LE Audio: microphone: 18 frames out (17 decoded, 1 concealed)"));
    }

    #[test]
    fn two_streams_are_mixed_to_mono() {
        let stats = Arc::new(MicStats::default());
        let mut dec = MicDecoder::new(&config(2), 2, stats.clone()).unwrap();
        let out = Collect::default();
        let mut o = out.clone();
        let frames = tone_frames(10);
        let silent = {
            let p = Lc3Params { sample_rate: 16000, frame_us: 10000, octets: 40 };
            let mut f = vec![0u8; 40];
            codec::lc3::silence_frame(p, &mut f).unwrap();
            f
        };
        for (i, f) in frames.iter().enumerate() {
            // Different sequence numbers per CIS: aligned by arrival
            dec.push(&sdu(0, i as u16, 0, f));
            dec.push(&sdu(1, 9000 + i as u16, 0, &silent));
            dec.process(&mut o);
        }
        assert_eq!(stats.snapshot().frames_out, 8);
        let pcm = out.0.lock().unwrap();
        let peak = pcm[160 * 4..].iter().map(|x| x.saturating_abs()).max().unwrap();
        // The tone at half the level (averaged with the silent microphone)
        assert!((2500..8000).contains(&peak), "peak {peak}");
        drop(pcm);
        // One stream stops: the other goes on once it is far enough ahead
        for i in 10..20u16 {
            dec.push(&sdu(0, i, 0, &frames[0]));
            dec.process(&mut o);
        }
        assert!(stats.snapshot().frames_out >= 8 + 10 - MIX_AHEAD_FRAMES as u64, "{:?}", stats.snapshot());
    }

    #[test]
    fn the_thread_decodes_from_the_ring_and_stops() {
        let (mut producer, consumer) = rtrb::RingBuffer::new(64);
        let rx = LeRxHandle {
            consumer,
            streams: vec![bt_host::LeRxStream { device_id: 0, cis_id: 0, ase_id: 3, channels: 1, octets: 40 }],
            sample_rate: 16000,
            frame_us: 10000,
            preset: "16_2_1".into(),
            stats: Arc::default(),
        };
        let out = Collect::default();
        let p = LeRxPipeline::start(rx, Box::new(out.clone())).unwrap();
        assert_eq!(p.sample_rate(), 16000);
        for (i, f) in tone_frames(12).iter().enumerate() {
            producer.push(sdu(0, i as u16, 0, f)).unwrap();
        }
        for _ in 0..500 {
            if p.stats().frames_out == 10 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let c = p.stop();
        assert_eq!((c.sdus, c.frames_out), (12, 10));
        assert_eq!(out.0.lock().unwrap().len(), 1600);
    }
}

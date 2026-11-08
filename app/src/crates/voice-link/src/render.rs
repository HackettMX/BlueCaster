//! The microphone render: mono 16-bit PCM from the voice link to a Windows
//! render endpoint (a virtual cable, whose capture side applications use as
//! a microphone).
//!
//! The PCM arrives through a lock-free queue ([`PcmProducer`], pushed on the
//! BTstack or decode thread). The render thread ("voice-render", MMCSS
//! "Audio") owns every COM object of the stream, as audio-capture's loopback
//! does: WASAPI shared mode, event driven, the stream format mono i16 at the
//! voice rate with Windows converting (AUTOCONVERTPCM). It writes silence
//! until the queue holds the prefill, silence on an underrun (and primes
//! again), and a slow drift control keeps the queue near the prefill.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wasapi::{AudioClient, AudioRenderClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode, WaveFormat};
use win_sys::audio_policy::ComApartment;

use crate::drift::{Primed, pop_into, push_from};

/// The stream buffer asked for (100 ns units); the engine period decides
/// how much is written per event, normally 10 ms
const BUFFER_HNS: i64 = 200_000;
/// Used when the client cannot tell its period
const DEFAULT_PERIOD: Duration = Duration::from_millis(10);
/// How long `open` waits for the thread to initialise the client
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for the device's event, so `stop` is seen
const WAIT_MS: u32 = 100;
const AUDCLNT_E_DEVICE_INVALIDATED: u32 = 0x8889_0004;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("no audio output device")]
    NoDevice,
    #[error("audio output device not found: {0}")]
    DeviceNotFound(String),
    #[error("{context} failed: HRESULT 0x{hresult:08x} ({message})")]
    Windows { context: &'static str, hresult: u32, message: String },
    #[error("{context} failed: {message}")]
    Wasapi { context: &'static str, message: String },
    #[error("opening the audio output timed out")]
    Timeout,
    #[error("cannot start the render thread: {0}")]
    Thread(String),
    #[error("the render thread stopped unexpectedly")]
    ThreadGone,
}

impl RenderError {
    /// The Windows error code behind this error, if there is one
    pub fn hresult(&self) -> Option<u32> {
        match self {
            RenderError::Windows { hresult, .. } => Some(*hresult),
            _ => None,
        }
    }

    fn from_wasapi(context: &'static str, e: wasapi::WasapiError) -> RenderError {
        match e {
            wasapi::WasapiError::Windows(w) => {
                RenderError::Windows { context, hresult: w.code().0 as u32, message: w.message() }
            }
            other => RenderError::Wasapi { context, message: other.to_string() },
        }
    }
}

/// Counters of a render (frames are mono samples at the voice rate)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    /// Frames from the queue written to the device
    pub frames_written: u64,
    /// Silence frames written (priming and underruns)
    pub silence_frames: u64,
    /// Times the queue ran dry after it had been primed
    pub underruns: u64,
    /// Frames pushed while the queue was full (lost)
    pub overflow: u64,
    /// Frames dropped (queue too full) and repeated (too low) by the drift control
    pub dropped: u64,
    pub duplicated: u64,
    /// Frames waiting in the queue now
    pub queued: u64,
}

#[derive(Debug, Default)]
struct Shared {
    stop: AtomicBool,
    ended: AtomicBool,
    end_error: OnceLock<String>,
    frames_written: AtomicU64,
    silence_frames: AtomicU64,
    underruns: AtomicU64,
    overflow: AtomicU64,
    dropped: AtomicU64,
    duplicated: AtomicU64,
    queued: AtomicU64,
}

impl Shared {
    fn stats(&self) -> RenderStats {
        RenderStats {
            frames_written: self.frames_written.load(Ordering::Relaxed),
            silence_frames: self.silence_frames.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            overflow: self.overflow.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            duplicated: self.duplicated.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
        }
    }
}

/// Pushes mono i16 PCM into a render queue. Lock-free: callable from the
/// BTstack or a decode thread.
pub struct PcmProducer {
    /// None: a second producer of a [`Renderer`], which only counts
    queue: Option<rtrb::Producer<i16>>,
    shared: Arc<Shared>,
    /// The microphone test's tap and the rate of what is pushed
    tap: Option<(crate::MicTap, u32)>,
}

impl PcmProducer {
    /// Everything pushed from now on also goes to `tap` (as `rate` Hz)
    pub fn set_tap(&mut self, tap: crate::MicTap, rate: u32) {
        self.tap = Some((tap, rate));
    }

    /// Queues as much of `mono_i16` as fits; returns the frames accepted
    /// (the rest is counted as overflow)
    pub fn push(&mut self, mono_i16: &[i16]) -> usize {
        if let Some((tap, rate)) = &self.tap {
            tap.feed(*rate, mono_i16);
        }
        let n = self.queue.as_mut().map_or(0, |q| push_from(q, mono_i16));
        if n < mono_i16.len() {
            self.shared.overflow.fetch_add((mono_i16.len() - n) as u64, Ordering::Relaxed);
        }
        n
    }
}

impl std::fmt::Debug for PcmProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmProducer").field("attached", &self.queue.is_some()).finish()
    }
}

/// The other end of a [`pcm_channel`] (what the render thread reads)
pub struct PcmConsumer {
    queue: rtrb::Consumer<i16>,
    shared: Arc<Shared>,
}

impl PcmConsumer {
    /// Moves up to `out.len()` queued frames to `out`; returns how many
    pub fn pop(&mut self, out: &mut [i16]) -> usize {
        pop_into(&mut self.queue, out)
    }

    pub fn queued(&self) -> usize {
        self.queue.slots()
    }

    /// The counters both ends share
    pub fn stats(&self) -> RenderStats {
        let mut s = self.shared.stats();
        s.queued = self.queue.slots() as u64;
        s
    }
}

impl std::fmt::Debug for PcmConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmConsumer").field("queued", &self.queued()).finish()
    }
}

/// A PCM queue of `capacity` frames without a device: what [`Renderer`]
/// uses inside, and for tests and tools that want the decoded audio
pub fn pcm_channel(capacity: usize) -> (PcmProducer, PcmConsumer) {
    let (p, c) = rtrb::RingBuffer::new(capacity.max(1));
    let shared = Arc::new(Shared::default());
    (PcmProducer { queue: Some(p), shared: shared.clone(), tap: None }, PcmConsumer { queue: c, shared })
}

/// The device side of the render loop, so the loop can be tested without
/// WASAPI
pub(crate) trait Endpoint {
    /// Waits for the device (at most about [`WAIT_MS`]) and returns how many
    /// frames it takes now (0 after a timeout)
    fn wait_space(&mut self) -> Result<usize, RenderError>;
    /// Writes `frames` to the device (at most what `wait_space` said)
    fn write(&mut self, frames: &[i16]) -> Result<(), RenderError>;
}

/// Feeds an [`Endpoint`] from the queue: silence until the prefill is
/// queued, silence (and priming again) on an underrun, drift control
pub(crate) struct Feeder {
    rate: u32,
    primed: Primed,
    buf: Vec<i16>,
    /// Frames written since the last summary
    since_summary: u64,
    window: RenderStats,
}

impl Feeder {
    /// `prefill` and `period` in frames at `rate`
    pub fn new(rate: u32, prefill: usize, period: usize) -> Feeder {
        Feeder {
            rate,
            primed: Primed::new(rate, prefill, period.max(1)),
            buf: vec![0; (rate as usize / 10).max(period * 2)],
            since_summary: 0,
            window: RenderStats::default(),
        }
    }

    /// One round: wait for the device, fill what it takes
    pub fn step(&mut self, endpoint: &mut impl Endpoint, queue: &mut PcmConsumer) -> Result<(), RenderError> {
        let space = endpoint.wait_space()?;
        if space == 0 {
            return Ok(());
        }
        if self.buf.len() < space {
            self.buf.resize(space, 0);
        }
        let out = &mut self.buf[..space];
        let r = self.primed.pull(&mut queue.queue, out);
        endpoint.write(out)?;
        let s = &queue.shared;
        s.frames_written.fetch_add(r.got as u64, Ordering::Relaxed);
        s.silence_frames.fetch_add(r.zeros as u64, Ordering::Relaxed);
        s.underruns.fetch_add(u64::from(r.underrun || r.reprimed), Ordering::Relaxed);
        s.dropped.fetch_add(r.dropped as u64, Ordering::Relaxed);
        s.duplicated.fetch_add(r.duplicated as u64, Ordering::Relaxed);
        s.queued.store(queue.queue.slots() as u64, Ordering::Relaxed);
        self.since_summary += space as u64;
        if self.since_summary >= u64::from(self.rate) * crate::SUMMARY_INTERVAL.as_secs() {
            self.since_summary = 0;
            let now = s.stats();
            let w = &self.window;
            tracing::debug!(
                "voice link: render last {} s: {} frames, {} silence, {} underruns, {} overflow, \
                 {} dropped / {} repeated (drift), queue {} frames",
                crate::SUMMARY_INTERVAL.as_secs(),
                now.frames_written - w.frames_written,
                now.silence_frames - w.silence_frames,
                now.underruns - w.underruns,
                now.overflow - w.overflow,
                now.dropped - w.dropped,
                now.duplicated - w.duplicated,
                now.queued
            );
            self.window = now;
        }
        Ok(())
    }
}

/// PCM going to the Windows render endpoint (the headset microphone)
pub struct Renderer {
    rate: u32,
    shared: Arc<Shared>,
    producer: Mutex<Option<PcmProducer>>,
    thread: Option<JoinHandle<()>>,
}

impl Renderer {
    /// Opens the render endpoint `device_id` (empty: the default output
    /// device) in WASAPI shared mode, event driven, with Windows converting
    /// from mono i16 at `rate` Hz; the thread "voice-render" runs at MMCSS
    /// "Audio". Silence until `prefill_ms` is queued; silence on underrun; a
    /// slow drift control drops or repeats a sample when the queue drifts
    /// past the prefill by more than one device period.
    pub fn open(device_id: &str, rate: u32, prefill_ms: u32) -> Result<Renderer, RenderError> {
        assert!(rate > 0);
        let prefill = (rate as usize * prefill_ms as usize / 1000).max(1);
        // Room for the prefill and generous jitter on top
        let capacity = (prefill * 4).max(rate as usize / 4);
        let (producer, consumer) = pcm_channel(capacity);
        let shared = consumer.shared.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let id = device_id.to_owned();
        let thread_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("voice-render".into())
            .spawn(move || thread_main(&id, rate, prefill, consumer, ready_tx, thread_shared))
            .map_err(|e| RenderError::Thread(e.to_string()))?;
        match ready_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Stuck inside Windows: the thread sees `stop` if the call
                // ever returns
                shared.stop.store(true, Ordering::Release);
                tracing::warn!("voice link: opening the microphone output timed out");
                return Err(RenderError::Timeout);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                return Err(RenderError::ThreadGone);
            }
        }
        Ok(Renderer { rate, shared, producer: Mutex::new(Some(producer)), thread: Some(thread) })
    }

    /// The producer side: lock-free, callable from the BTstack or decode
    /// thread. There is one queue: the first call returns its producer; a
    /// later call returns one that only counts what it gets as overflow.
    pub fn producer(&self) -> PcmProducer {
        let taken = self.producer.lock().unwrap_or_else(|e| e.into_inner()).take();
        taken.unwrap_or_else(|| {
            tracing::warn!("voice link: render producer requested twice; the second one is not connected");
            PcmProducer { queue: None, shared: self.shared.clone(), tap: None }
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn stats(&self) -> RenderStats {
        self.shared.stats()
    }

    /// The device went away or failed (removed, disabled, format changed);
    /// nothing more is rendered
    pub fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::Acquire)
    }

    /// Why it ended, once [`Renderer::ended`]
    pub fn end_error(&self) -> Option<&str> {
        self.shared.end_error.get().map(String::as_str)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").field("rate", &self.rate).field("stats", &self.stats()).finish()
    }
}

/// The WASAPI stream, created and used on the render thread only
struct Wasapi {
    client: AudioClient,
    render: AudioRenderClient,
    event: Handle,
    bytes: Vec<u8>,
}

impl Endpoint for Wasapi {
    fn wait_space(&mut self) -> Result<usize, RenderError> {
        // A timeout is not an error: `stop` is checked by the caller
        if self.event.wait_for_event(WAIT_MS).is_err() {
            return Ok(0);
        }
        self.client
            .get_available_space_in_frames()
            .map(|n| n as usize)
            .map_err(|e| RenderError::from_wasapi("GetCurrentPadding", e))
    }

    fn write(&mut self, frames: &[i16]) -> Result<(), RenderError> {
        self.bytes.clear();
        self.bytes.extend(frames.iter().flat_map(|s| s.to_le_bytes()));
        self.render
            .write_to_device(frames.len(), &self.bytes, None)
            .map_err(|e| RenderError::from_wasapi("writing to the audio output", e))
    }
}

fn open_stream(device_id: &str, rate: u32) -> Result<(Wasapi, Duration, String), RenderError> {
    let enumerator = DeviceEnumerator::new().map_err(|e| RenderError::from_wasapi("device enumerator", e))?;
    let device = if device_id.is_empty() {
        enumerator.get_default_device(&Direction::Render).map_err(|_| RenderError::NoDevice)?
    } else {
        enumerator.get_device(device_id).map_err(|_| RenderError::DeviceNotFound(device_id.to_owned()))?
    };
    if device.get_direction() != Direction::Render {
        return Err(RenderError::DeviceNotFound(device_id.to_owned()));
    }
    let name = device.get_friendlyname().unwrap_or_default();
    let mut client =
        device.get_iaudioclient().map_err(|e| RenderError::from_wasapi("activating the audio client", e))?;
    let wave = WaveFormat::new(16, 16, &SampleType::Int, rate as usize, 1, None);
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: BUFFER_HNS };
    client
        .initialize_client(&wave, &Direction::Render, &mode)
        .map_err(|e| RenderError::from_wasapi("initializing the audio output", e))?;
    let event = client.set_get_eventhandle().map_err(|e| RenderError::from_wasapi("SetEventHandle", e))?;
    let render =
        client.get_audiorenderclient().map_err(|e| RenderError::from_wasapi("GetService(IAudioRenderClient)", e))?;
    let period = client
        .get_device_period()
        .ok()
        .and_then(|(default, _min)| u64::try_from(default).ok())
        .filter(|&hns| hns > 0)
        .map(|hns| Duration::from_nanos(hns * 100))
        .unwrap_or(DEFAULT_PERIOD);
    tracing::debug!(
        "voice link: render client for \u{201C}{name}\u{201D}: {rate} Hz mono i16 (converted by Windows), buffer {:?} \
         frames, period {:.2} ms",
        client.get_buffer_size().ok(),
        period.as_secs_f64() * 1000.0
    );
    Ok((Wasapi { client, render, event, bytes: Vec::new() }, period, name))
}

fn thread_main(
    device_id: &str,
    rate: u32,
    prefill: usize,
    mut queue: PcmConsumer,
    ready: mpsc::SyncSender<Result<(), RenderError>>,
    shared: Arc<Shared>,
) {
    let _com = match ComApartment::init_mta() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(RenderError::Windows {
                context: "CoInitializeEx",
                hresult: e.code().0 as u32,
                message: e.message(),
            }));
            return;
        }
    };
    let (mut stream, period, name) = match open_stream(device_id, rate) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("voice link: microphone output: {e}");
            let _ = ready.send(Err(e));
            return;
        }
    };
    // The buffer starts full of silence (Microsoft's render example)
    let start = stream.client.get_available_space_in_frames().map(|n| n as usize).unwrap_or(0);
    if let Err(e) = stream.write(&vec![0; start]) {
        let _ = ready.send(Err(e));
        return;
    }
    if let Err(e) = stream.client.start_stream() {
        let _ = ready.send(Err(RenderError::from_wasapi("starting the audio output", e)));
        return;
    }
    if ready.send(Ok(())).is_err() {
        return; // `open` gave up waiting
    }
    let _mmcss = match win_sys::mmcss::MmcssGuard::register(win_sys::mmcss::MmcssTask::Audio) {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!("voice link: MMCSS registration failed: {e}");
            None
        }
    };
    let period_frames = (period.as_secs_f64() * f64::from(rate)).round() as usize;
    tracing::info!(
        "voice link: rendering the microphone to \u{201C}{name}\u{201D}, {rate} Hz, prefill {} ms",
        prefill * 1000 / rate as usize
    );
    let mut feeder = Feeder::new(rate, prefill, period_frames);
    let result = loop {
        if shared.stop.load(Ordering::Acquire) {
            break Ok(());
        }
        if let Err(e) = feeder.step(&mut stream, &mut queue) {
            break Err(e);
        }
    };
    let _ = stream.client.stop_stream();
    let s = shared.stats();
    tracing::debug!(
        "voice link: render totals: {} frames, {} silence, {} underruns, {} overflow, {} dropped / {} repeated",
        s.frames_written,
        s.silence_frames,
        s.underruns,
        s.overflow,
        s.dropped,
        s.duplicated
    );
    match result {
        Ok(()) => tracing::info!("voice link: microphone render stopped"),
        Err(e) => {
            if e.hresult() == Some(AUDCLNT_E_DEVICE_INVALIDATED) {
                tracing::info!("voice link: microphone output went away");
            } else {
                tracing::warn!("voice link: microphone render ended: {e}");
            }
            let _ = shared.end_error.set(e.to_string());
            shared.ended.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device that takes `period` frames per event at `speed` times the
    /// producer's clock, with a producer pushing `packet` frames on its
    /// own schedule (both driven from `wait_space`)
    struct FakeDevice {
        period: usize,
        speed: f64,
        producer: PcmProducer,
        packet: usize,
        /// Frames the producer owes
        credit: f64,
        next_value: i16,
        written: Vec<i16>,
        events: usize,
        /// Producer silent for these events (a gap in the link)
        gap: std::ops::Range<usize>,
    }

    impl Endpoint for FakeDevice {
        fn wait_space(&mut self) -> Result<usize, RenderError> {
            self.events += 1;
            self.credit += self.period as f64 / self.speed;
            while self.credit >= self.packet as f64 {
                self.credit -= self.packet as f64;
                if !self.gap.contains(&self.events) {
                    let pkt: Vec<i16> = (0..self.packet)
                        .map(|_| {
                            self.next_value = self.next_value.wrapping_add(1).max(1);
                            self.next_value
                        })
                        .collect();
                    self.producer.push(&pkt);
                }
            }
            Ok(self.period)
        }

        fn write(&mut self, frames: &[i16]) -> Result<(), RenderError> {
            assert!(frames.len() <= self.period);
            self.written.extend_from_slice(frames);
            Ok(())
        }
    }

    fn run(speed: f64, events: usize, gap: std::ops::Range<usize>) -> (FakeDevice, PcmConsumer, Feeder) {
        // 16 kHz, 40 ms prefill, 10 ms device period, 7.5 ms link packets
        let (producer, mut consumer) = pcm_channel(4000);
        let mut dev = FakeDevice {
            period: 160,
            speed,
            producer,
            packet: 120,
            credit: 0.0,
            next_value: 0,
            written: Vec::new(),
            events: 0,
            gap,
        };
        let mut feeder = Feeder::new(16_000, 640, 160);
        for _ in 0..events {
            feeder.step(&mut dev, &mut consumer).unwrap();
        }
        (dev, consumer, feeder)
    }

    #[test]
    fn prefill_comes_first_then_the_audio_in_order() {
        let (dev, consumer, _) = run(1.0, 500, 0..0);
        let s = consumer.stats();
        // Silence until 640 frames were queued (about 4 events)
        let lead = dev.written.iter().take_while(|&&x| x == 0).count();
        assert!((480..=800).contains(&lead), "lead {lead}");
        assert_eq!(s.silence_frames as usize, lead);
        // Then the samples in order, none lost or repeated
        let audio = &dev.written[lead..];
        let next = |x: i16| if x == i16::MAX { 1 } else { x + 1 };
        assert!(audio.windows(2).all(|w| w[1] == next(w[0])), "audio out of order");
        assert_eq!(s.underruns, 0);
        assert_eq!((s.dropped, s.duplicated, s.overflow), (0, 0, 0));
        assert_eq!(s.frames_written as usize, audio.len());
        // The queue holds about the prefill
        assert!((500..=900).contains(&s.queued), "queued {}", s.queued);
    }

    #[test]
    fn underrun_writes_silence_and_primes_again() {
        // The link delivers nothing for 20 events (200 ms)
        let (dev, consumer, _) = run(1.0, 300, 100..120);
        let s = consumer.stats();
        assert_eq!(s.underruns, 1, "{s:?}");
        // After the gap: silence until the prefill is back, then audio again
        let tail = &dev.written[dev.written.len() - 1600..];
        assert!(tail.iter().all(|&x| x != 0));
    }

    #[test]
    fn drift_control_follows_a_faster_and_a_slower_device() {
        for speed in [1.002, 0.998] {
            // 60 s
            let (_, consumer, _) = run(speed, 6000, 0..0);
            let s = consumer.stats();
            assert_eq!(s.underruns, 0, "speed {speed}: {s:?}");
            assert_eq!(s.overflow, 0, "speed {speed}: {s:?}");
            // 0.2 % of 960 000 frames, corrected one way
            let corrected = if speed > 1.0 { s.duplicated } else { s.dropped };
            assert!((1400..=2200).contains(&corrected), "speed {speed}: {s:?}");
            // And the queue stays near the prefill (640) within a period or two
            assert!((300..=1000).contains(&s.queued), "speed {speed}: {s:?}");
        }
    }

    #[test]
    fn second_producer_is_detached() {
        let (p, c) = pcm_channel(10);
        let shared = c.shared.clone();
        drop(p);
        let mut detached = PcmProducer { queue: None, shared, tap: None };
        assert_eq!(detached.push(&[1, 2, 3]), 0);
        assert_eq!(c.stats().overflow, 3);
    }

    #[test]
    fn handles_move_between_threads() {
        fn send<T: Send>() {}
        send::<Renderer>();
        send::<PcmProducer>();
        send::<PcmConsumer>();
        send::<RenderError>();
    }

    /// Renders a 1 kHz tone to the default output device for 1 s (audible)
    #[test]
    #[ignore = "plays a tone on the default audio output"]
    fn renders_to_the_default_device() {
        let r = Renderer::open("", 16_000, 40).unwrap();
        let mut p = r.producer();
        let tone: Vec<i16> = (0..16_000)
            .map(|i| (1500.0 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 16_000.0).sin()) as i16)
            .collect();
        for chunk in tone.chunks(120) {
            let mut pushed = 0;
            while pushed < chunk.len() {
                pushed += p.push(&chunk[pushed..]);
                if pushed < chunk.len() {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            thread::sleep(Duration::from_micros(7400));
        }
        thread::sleep(Duration::from_millis(300));
        let s = r.stats();
        assert!(!r.ended(), "{:?}", r.end_error());
        assert!(s.frames_written > 12_000, "{s:?}");
        assert_eq!(s.overflow, 0, "{s:?}");
        drop(r);
    }
}

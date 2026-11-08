//! Headset mode of a classic stream (docs/headset-mic-dev.md): the A2DP
//! stream is suspended (the link kept), the HFP audio connection carries the
//! PC's audio to the headphones and their microphone back, which is
//! rendered to the output the user chose (a virtual cable) or written to a
//! WAV file (developer CLI). Off again: the audio connection is closed and
//! A2DP resumes with a fresh pipeline.
//!
//! bt-host needs the [`VoiceTx`] / [`VoiceRx`] when the audio connection is
//! asked for, before the codec is negotiated: they are codec-agnostic shells
//! here, armed with the encoder, decoder, microphone output and uplink once
//! [`VoiceEvent::Established`] names the codec. The session thread locks
//! their slot only to arm and disarm them; the BTstack thread only tries the
//! lock (silence or a dropped packet when it is taken), so it never waits.
//!
//! The session ([`crate::stream`]) runs [`HeadsetMode::tick`] on every tick
//! of a link that is fine, [`HeadsetMode::link_lost`] when the link drops and
//! [`HeadsetMode::end`] when the session ends.
//!
//! [`HeadsetMic::Auto`] opens the voice link only while an application
//! records from the microphone output's capture side (the cable's "CABLE
//! Output"; [`audio_capture::mic_capture_active`], polled once a second) and
//! goes back to A2DP once nobody has recorded for [`AUTO_IDLE`].
//!
//! Where the microphone goes is decided here for both kinds of device:
//! [`resolve_mic_output`] (a render endpoint by id or name, a WAV file, the
//! default output), [`mic_conflict`] (the feedback-loop refusal) and
//! [`MicOutput`] (the [`Renderer`] or the [`WavSink`]); the LE Audio session
//! ([`crate::le_stream`]) uses them for its microphone too.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audio_capture::{Capture, CaptureOptions, Running, Source};
use bt_host::{BtHost, HostError, VoiceEvent, VoiceRx, VoiceTx};
use voice_link::{PcmProducer, Renderer, UplinkConsumer, UplinkSink, VoiceCodec, VoiceDecoder, VoiceEncoder, WavSink};

use crate::messages::Msg;
use crate::service::{Inner, VoiceChange};
use crate::stream::{SourceRequest, StreamState};

pub use bt_host::HeadsetMic;

/// The microphone render's prefill (HFP and LE Audio alike: the renderer's
/// drift control absorbs the headset's clock around it)
const RENDER_PREFILL_MS: u32 = 40;
/// The uplink queue (capture to encoder)
const UPLINK_CAPACITY_MS: u32 = 200;
/// How long headset mode waits for the HFP service level connection, which
/// comes up some time after A2DP
const SLC_WAIT: Duration = Duration::from_secs(20);
/// How long an audio connection asked for may take to come up
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// The session's own summary while the voice link is up
const SUMMARY_EVERY: Duration = Duration::from_secs(10);
/// How often an application source the uplink does not capture is looked
/// for while the voice link is up
const APP_RECHECK: Duration = Duration::from_secs(1);

/// How often Auto asks whether an application records from the microphone
/// output's capture side
const AUTO_POLL: Duration = Duration::from_secs(1);
/// How long nobody must record before Auto goes back to A2DP: an app that
/// opens the device again does not bounce the link
const AUTO_IDLE: Duration = Duration::from_secs(2);

/// Auto's view of the recording applications: polled at most every
/// [`AUTO_POLL`]; recording seen once turns it on, not recording for
/// [`AUTO_IDLE`] off again
#[derive(Debug, Default)]
struct AutoWatch {
    /// The microphone output, resolved at the first poll (a device
    /// enumeration, not on every poll); None inside: no output device
    target: Option<Option<MicTarget>>,
    polled: Option<Instant>,
    /// Someone records (after the hysteresis)
    recording: bool,
    /// Since when the polls see nobody recording while `recording`
    idle_since: Option<Instant>,
    /// A missing capture side or a failed look-up was logged
    warned: bool,
}

impl AutoWatch {
    fn due(&self, now: Instant) -> bool {
        self.polled.is_none_or(|t| now.duration_since(t) >= AUTO_POLL)
    }

    /// One poll's answer; returns whether someone records now
    fn seen(&mut self, recording: bool, now: Instant) -> bool {
        self.polled = Some(now);
        if recording {
            self.recording = true;
            self.idle_since = None;
        } else if self.recording {
            let since = *self.idle_since.get_or_insert(now);
            if now.duration_since(since) >= AUTO_IDLE {
                self.recording = false;
                self.idle_since = None;
            }
        }
        self.recording
    }

    /// Whether an application records from `target`'s capture side: a WAV
    /// file always (the developer CLI's test path); no capture side or a
    /// failed look-up count as nobody, logged once
    fn look(&mut self, target: Option<&MicTarget>) -> bool {
        let id = match target {
            Some(MicTarget::Wav(_)) => return true,
            Some(MicTarget::Device { id, .. }) => id,
            None => {
                if !std::mem::replace(&mut self.warned, true) {
                    tracing::warn!("voice: auto: no output device for the microphone");
                }
                return false;
            }
        };
        match audio_capture::mic_capture_active(id) {
            Ok(Some(on)) => on,
            Ok(None) => {
                if !std::mem::replace(&mut self.warned, true) {
                    tracing::warn!(
                        "voice: auto: the microphone output {} has no recording side, headset mode stays off",
                        target.map_or(String::new(), |t| t.to_string())
                    );
                }
                false
            }
            Err(e) => {
                if !std::mem::replace(&mut self.warned, true) {
                    tracing::warn!("voice: auto: cannot tell whether an app records: {e}");
                }
                false
            }
        }
    }

    /// Polls when due; returns whether someone records now
    fn poll(&mut self, mic_output: Option<&str>, now: Instant) -> bool {
        if !self.due(now) {
            return self.recording;
        }
        let target = self.target.get_or_insert_with(|| resolve_mic_output(mic_output)).clone();
        let looked = self.look(target.as_ref());
        let before = self.recording;
        let after = self.seen(looked, now);
        if after != before {
            let what = target.map_or(String::new(), |t| t.to_string());
            if after {
                tracing::info!("voice: auto: an application records from {what}");
            } else {
                tracing::info!("voice: auto: nobody recorded from {what} for {} s", AUTO_IDLE.as_secs());
            }
        }
        after
    }
}

/// Whether the uplink looks for its application again now: the link is
/// armed, the source is an application, and the last look is
/// [`APP_RECHECK`] ago
fn app_recheck_due(source: &SourceRequest, armed: bool, last: Instant, now: Instant) -> bool {
    armed && matches!(source, SourceRequest::Application { .. }) && now.duration_since(last) >= APP_RECHECK
}

/// What the session does to its A2DP stream around the voice link
pub(crate) trait A2dp {
    /// Stops the capture and the encode thread and suspends the stream (the
    /// link stays up)
    fn suspend(&mut self);
    /// Starts the stream again, with a fresh pipeline and the capture. The
    /// error ends the session.
    fn resume(&mut self) -> Result<(), Msg>;
}

/// Where the microphone goes
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicTarget {
    /// A Windows render endpoint (a virtual cable's playback side)
    Device { id: String, name: String },
    /// A mono 16-bit WAV file (developer CLI, emulator tests)
    Wav(PathBuf),
}

impl MicTarget {
    /// The render endpoint's id for [`ipc::VoiceStatus::mic_device_id`];
    /// empty for a file
    pub fn device_id(&self) -> &str {
        match self {
            MicTarget::Device { id, .. } => id,
            MicTarget::Wav(_) => "",
        }
    }

    /// The target of the `n`th microphone of a session (1: as given). A
    /// file gets "-2", "-3", ... before its extension, so an LE stream set
    /// up again does not overwrite what the previous one wrote.
    pub fn nth(&self, n: u32) -> MicTarget {
        match self {
            MicTarget::Wav(path) if n > 1 => {
                let stem = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                let ext = path.extension().map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
                MicTarget::Wav(path.with_file_name(format!("{stem}-{n}{ext}")))
            }
            other => other.clone(),
        }
    }
}

impl std::fmt::Display for MicTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MicTarget::Device { id, name } => write!(f, "{} ({id})", crate::diag::quoted(name)),
            MicTarget::Wav(path) => write!(f, "file {}", path.display()),
        }
    }
}

/// The microphone output of a request ([`crate::StreamRequest::mic_output`],
/// `bt_host::LeRequest::mic_output`): a path ending in ".wav"; a render
/// endpoint by id, or (command line) by a part of its friendly name; none
/// or empty: the default output. None when there is no output device at all.
pub fn resolve_mic_output(spec: Option<&str>) -> Option<MicTarget> {
    let spec = spec.unwrap_or("").trim();
    if spec.to_ascii_lowercase().ends_with(".wav") {
        return Some(MicTarget::Wav(PathBuf::from(spec)));
    }
    let devices = audio_capture::render_devices().unwrap_or_else(|e| {
        tracing::warn!("voice: cannot list the output devices: {e}");
        Vec::new()
    });
    let wanted = spec.to_lowercase();
    let found = if spec.is_empty() {
        devices.iter().find(|d| d.is_default)
    } else {
        devices
            .iter()
            .find(|d| d.id == spec)
            .or_else(|| devices.iter().find(|d| d.name.to_lowercase().contains(&wanted)))
    };
    match found {
        Some(d) => Some(MicTarget::Device { id: d.id.clone(), name: d.name.clone() }),
        // Gone (or not listed): opening it says so
        None if !spec.is_empty() => Some(MicTarget::Device { id: spec.to_string(), name: spec.to_string() }),
        None => None,
    }
}

/// Why headset mode must not render to `target` while `source` is captured
/// (the message key): the microphone would come back into the stream
pub fn mic_conflict(target: &MicTarget, source: &SourceRequest) -> Option<&'static str> {
    match target {
        MicTarget::Device { id, .. } => {
            let default = audio_capture::default_render_device().unwrap_or_default();
            crate::profiles::mic_output_conflicts(source, id, &default)
        }
        MicTarget::Wav(_) => None,
    }
}

/// The capture of the uplink: the stream's source at the voice rate (Windows
/// converts); None for an application that does not run (silence goes up)
fn uplink_source(source: &SourceRequest) -> Option<(Source, bool)> {
    Some(match source {
        SourceRequest::TestTone => (Source::TestTone, false),
        SourceRequest::SystemLoopback => (Source::SystemLoopback, true),
        SourceRequest::VirtualDevice { id, .. } | SourceRequest::OutputDevice { id, .. } if !id.is_empty() => {
            (Source::RenderDevice { id: id.clone() }, true)
        }
        SourceRequest::VirtualDevice { .. } | SourceRequest::OutputDevice { .. } => (Source::SystemLoopback, true),
        SourceRequest::Application { exe } => {
            (Source::Application { process_id: audio_capture::find_process(exe)? }, true)
        }
    })
}

struct TxParts {
    encoder: VoiceEncoder,
    uplink: UplinkConsumer,
}

struct RxParts {
    decoder: VoiceDecoder,
    out: PcmProducer,
}

type Slot<T> = Arc<Mutex<Option<T>>>;

/// The [`VoiceTx`] bt-host holds: silence until armed
struct ShellTx(Slot<TxParts>);

impl VoiceTx for ShellTx {
    fn fill(&mut self, out: &mut [u8]) {
        match self.0.try_lock() {
            Ok(mut parts) => match parts.as_mut() {
                Some(p) => p.encoder.fill(&mut p.uplink, out),
                None => out.fill(0),
            },
            // Being armed or disarmed right now
            Err(_) => out.fill(0),
        }
    }
}

/// The [`VoiceRx`] bt-host holds: drops packets until armed
struct ShellRx(Slot<RxParts>);

impl VoiceRx for ShellRx {
    fn packet(&mut self, payload: &[u8], status: u8) {
        if let Ok(mut parts) = self.0.try_lock()
            && let Some(p) = parts.as_mut()
        {
            p.decoder.decode(payload, status, &mut p.out);
        }
    }
}

/// The open microphone output: mono 16-bit PCM at the voice rate pushed
/// through its [`PcmProducer`]
pub enum MicOutput {
    /// Rendered (40 ms prefill, drift control)
    Device(Renderer),
    Wav(WavSink),
}

impl MicOutput {
    /// Opens `target` for PCM at `rate` Hz
    pub fn open(target: &MicTarget, rate: u32) -> Result<MicOutput, String> {
        match target {
            MicTarget::Device { id, .. } => {
                Renderer::open(id, rate, RENDER_PREFILL_MS).map(MicOutput::Device).map_err(|e| e.to_string())
            }
            MicTarget::Wav(path) => {
                WavSink::create(path, rate).map(MicOutput::Wav).map_err(|e| format!("{}: {e}", path.display()))
            }
        }
    }

    /// Its producer (once)
    pub fn producer(&mut self) -> Option<PcmProducer> {
        match self {
            MicOutput::Device(r) => Some(r.producer()),
            MicOutput::Wav(w) => w.producer(),
        }
    }

    /// The render device went away or failed: why
    pub fn ended(&self) -> Option<String> {
        match self {
            MicOutput::Device(r) if r.ended() => Some(r.end_error().unwrap_or("ended").to_string()),
            _ => None,
        }
    }

    /// The render's counters, for a summary line
    pub fn describe(&self) -> String {
        match self {
            MicOutput::Device(r) => {
                let s = r.stats();
                format!(
                    "render {} frames, {} underruns, {} overflow, {} dropped / {} repeated",
                    s.frames_written, s.underruns, s.overflow, s.dropped, s.duplicated
                )
            }
            MicOutput::Wav(_) => "to a file".into(),
        }
    }
}

/// One voice link, from asking for the audio connection to closing it
struct VoiceLink {
    tx: Slot<TxParts>,
    rx: Slot<RxParts>,
    target: MicTarget,
    asked: Instant,
    /// Since Established
    codec: Option<(VoiceCodec, Instant)>,
    output: Option<MicOutput>,
    capture: Option<Running>,
    /// The process the uplink captures (an application source)
    uplink_pid: Option<u32>,
    /// The application looked for last ([`app_recheck_due`]), and a
    /// process whose capture failed (not tried again)
    app_checked: Instant,
    app_failed_pid: Option<u32>,
    summary_at: Instant,
}

impl VoiceLink {
    fn new(target: MicTarget) -> (VoiceLink, Box<dyn VoiceTx>, Box<dyn VoiceRx>) {
        let (tx, rx): (Slot<TxParts>, Slot<RxParts>) = Default::default();
        let now = Instant::now();
        let link = VoiceLink {
            tx: tx.clone(),
            rx: rx.clone(),
            target,
            asked: now,
            codec: None,
            output: None,
            capture: None,
            uplink_pid: None,
            app_checked: now,
            app_failed_pid: None,
            summary_at: now,
        };
        (link, Box::new(ShellTx(tx)), Box::new(ShellRx(rx)))
    }

    /// The audio connection is up with `codec`: the microphone output at its
    /// rate, the uplink capture of `source`, and the codec halves armed. Err:
    /// the output cannot be opened (the message for the user).
    fn arm(&mut self, codec: VoiceCodec, source: &SourceRequest) -> Result<(), Msg> {
        let rate = codec.rate();
        let mut output = MicOutput::open(&self.target, rate).map_err(|e| {
            tracing::warn!("voice: cannot open the microphone output {}: {e}", self.target);
            Msg::new("status.headset_mic_output_failed")
        })?;
        let out = output.producer().ok_or_else(|| Msg::new("status.headset_mic_output_failed"))?;
        let uplink = self.start_uplink(rate, source);
        *self.rx.lock().unwrap_or_else(|e| e.into_inner()) = Some(RxParts { decoder: VoiceDecoder::new(codec), out });
        *self.tx.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(TxParts { encoder: VoiceEncoder::new(codec), uplink });
        self.output = Some(output);
        let now = Instant::now();
        self.codec = Some((codec, now));
        self.summary_at = now + SUMMARY_EVERY;
        tracing::info!(
            "voice: {} at {rate} Hz, {} ms after asking; microphone to {}, uplink from {source}",
            codec.name(),
            now.duration_since(self.asked).as_millis(),
            self.target
        );
        Ok(())
    }

    /// A capture of `source` into a new uplink (its consumer returned)
    fn start_uplink(&mut self, rate: u32, source: &SourceRequest) -> UplinkConsumer {
        if let Some(r) = self.capture.take() {
            r.stop();
        }
        self.uplink_pid = None;
        self.app_checked = Instant::now();
        let (sink, consumer) = UplinkSink::new(rate, UPLINK_CAPACITY_MS);
        match uplink_source(source) {
            Some((src, fill_silence)) => {
                let pid = match &src {
                    Source::Application { process_id } => Some(*process_id),
                    _ => None,
                };
                let options = CaptureOptions { preferred_rate: Some(rate), fill_silence };
                match Capture::open(&src, &options).and_then(|c| {
                    let f = c.format();
                    c.start(Box::new(sink)).map(|r| (r, f))
                }) {
                    Ok((r, f)) => {
                        tracing::debug!(
                            "voice: uplink capture {} Hz, {} channel(s), {:?}",
                            f.sample_rate,
                            f.channels,
                            f.sample_format
                        );
                        self.capture = Some(r);
                        self.uplink_pid = pid;
                        self.app_failed_pid = None;
                    }
                    Err(e) => {
                        tracing::warn!("voice: cannot capture {source} for the headphones, sending silence: {e}");
                        self.app_failed_pid = pid;
                    }
                }
            }
            None => tracing::info!("voice: {source} is not running, sending silence to the headphones"),
        }
        consumer
    }

    /// Another source chosen: only the uplink's capture changes
    fn change_source(&mut self, source: &SourceRequest) {
        let Some((codec, _)) = self.codec else { return };
        let uplink = self.start_uplink(codec.rate(), source);
        if let Some(parts) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            parts.uplink = uplink;
        }
    }

    /// An application source the uplink does not capture (not running when
    /// the link came up, or exited since): looked for at most once a
    /// second, and captured again once it runs
    fn recheck_app(&mut self, source: &SourceRequest) {
        let now = Instant::now();
        if !app_recheck_due(source, self.codec.is_some(), self.app_checked, now) {
            return;
        }
        self.app_checked = now;
        let exited = self.uplink_pid.is_some_and(audio_capture::process_exited);
        if self.capture.as_ref().is_some_and(|r| !r.ended()) && !exited {
            return;
        }
        if let Some(r) = self.capture.take() {
            tracing::info!("voice: {source} exited, sending silence to the headphones until it runs again");
            r.stop();
            self.uplink_pid = None;
        }
        let SourceRequest::Application { exe } = source else { return };
        match audio_capture::find_process(exe) {
            Some(pid) if self.app_failed_pid != Some(pid) => {
                tracing::info!("voice: {source} runs (PID {pid}), capturing it for the headphones");
                self.change_source(source);
            }
            _ => {}
        }
    }

    /// Another microphone output chosen: only the output changes
    fn change_output(&mut self, target: MicTarget) -> Result<(), Msg> {
        self.target = target;
        let Some((codec, _)) = self.codec else { return Ok(()) };
        let mut output = MicOutput::open(&self.target, codec.rate()).map_err(|e| {
            tracing::warn!("voice: cannot open the microphone output {}: {e}", self.target);
            Msg::new("status.headset_mic_output_failed")
        })?;
        let out = output.producer().ok_or_else(|| Msg::new("status.headset_mic_output_failed"))?;
        if let Some(parts) = self.rx.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            parts.out = out;
        }
        // The old one stops after its producer is gone
        self.output = Some(output);
        tracing::info!("voice: microphone now to {}", self.target);
        Ok(())
    }

    fn status(&self) -> Option<ipc::VoiceStatus> {
        let (codec, _) = self.codec?;
        Some(ipc::VoiceStatus {
            codec: codec.name().into(),
            link: "hfp".into(),
            rate: codec.rate(),
            mic_device_id: self.target.device_id().to_string(),
        })
    }

    fn summary(&mut self) {
        let Some((codec, since)) = self.codec else { return };
        let now = Instant::now();
        if now < self.summary_at {
            return;
        }
        self.summary_at = now + SUMMARY_EVERY;
        let up =
            self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|p| (p.encoder.stats(), p.uplink.stats()));
        let down = self.rx.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|p| p.decoder.stats());
        let render = self.output.as_ref().map_or_else(|| "no output".into(), MicOutput::describe);
        tracing::debug!(
            "voice: {} up {} s; uplink {:?}; downlink {:?}; {render}; capture {}",
            codec.name(),
            now.duration_since(since).as_secs(),
            up.map(|(e, u)| (e.bytes, u.underruns)),
            down.map(|d| (d.packets, d.bad_packets, d.concealed)),
            self.capture.as_ref().map_or(0, Running::frames)
        );
    }

    /// Everything of the link dropped: the shells bt-host may still hold
    /// send silence and drop packets
    fn disarm(&mut self) {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(r) = self.capture.take() {
            r.stop();
        }
        if let Some((codec, since)) = self.codec.take() {
            let (e, d) = (tx.as_ref().map(|p| p.encoder.stats()), rx.as_ref().map(|p| p.decoder.stats()));
            let u = tx.as_ref().map(|p| p.uplink.stats()).unwrap_or_default();
            tracing::info!(
                "voice: {} link down after {} s: sent {} bytes (uplink {} underruns, {} dropped / {} repeated), \
                 received {} packets ({} bad, {} concealed)",
                codec.name(),
                since.elapsed().as_secs(),
                e.map_or(0, |e| e.bytes),
                u.underruns,
                u.dropped,
                u.duplicated,
                d.map_or(0, |d| d.packets),
                d.map_or(0, |d| d.bad_packets),
                d.map_or(0, |d| d.concealed)
            );
        }
        drop((tx, rx));
        self.output = None;
    }
}

impl Drop for VoiceLink {
    fn drop(&mut self) {
        self.disarm();
    }
}

/// Headset mode of one session
pub(crate) struct HeadsetMode {
    /// Asked for (the request, then [`crate::Service::set_headset_mic`])
    mode: HeadsetMic,
    mic_output: Option<String>,
    /// Given up for this request (refused, failed): tried again on the next
    /// change only (not on Auto's own on and off)
    held_off: bool,
    /// Auto: whether an application records
    auto: AutoWatch,
    /// Auto: the headphones closed the audio connection; opened again only
    /// after nobody recorded for a while (or on the next change)
    hung_up: bool,
    /// Until when a missing service level connection is waited for
    slc_deadline: Option<Instant>,
    link: Option<VoiceLink>,
    /// An audio connection of the headphones' own was left alone (logged
    /// once per session)
    foreign_logged: bool,
}

impl HeadsetMode {
    pub fn new(mode: HeadsetMic, mic_output: Option<String>) -> HeadsetMode {
        HeadsetMode {
            mode,
            mic_output,
            held_off: false,
            auto: AutoWatch::default(),
            hung_up: false,
            slc_deadline: None,
            link: None,
            foreign_logged: false,
        }
    }

    /// Whether the voice link should be up now: On, or Auto while an
    /// application records (not after the headphones hung up on it)
    fn wanted(&self) -> bool {
        match self.mode {
            HeadsetMic::Off => false,
            HeadsetMic::On => true,
            HeadsetMic::Auto => self.auto.recording && !self.hung_up,
        }
    }

    /// Auto: polls the recording applications (at most every
    /// [`AUTO_POLL`])
    fn poll_auto(&mut self) {
        if self.mode != HeadsetMic::Auto {
            return;
        }
        if !self.auto.poll(self.mic_output.as_deref(), Instant::now()) {
            self.hung_up = false;
            // The next recording waits for the service level connection
            // afresh
            if self.link.is_none() {
                self.slc_deadline = None;
            }
        }
    }

    /// The voice link carries the stream (A2DP is suspended)
    pub fn active(&self) -> bool {
        self.link.is_some()
    }

    /// Another source chosen while the voice link is up
    pub fn change_source(&mut self, source: &SourceRequest) {
        if let Some(link) = self.link.as_mut() {
            link.change_source(source);
        }
    }

    /// One tick of a stream whose link is fine. The error ends the session
    /// (A2DP could not resume).
    pub fn tick(
        &mut self,
        inner: &Inner,
        host: &BtHost,
        source: &SourceRequest,
        a2dp: &mut dyn A2dp,
    ) -> Result<(), Msg> {
        if let Some(change) = inner.take_voice_change() {
            self.change(change, inner, host, a2dp)?;
        }
        for event in inner.take_voice_events() {
            self.on_event(event, inner, host, source, a2dp)?;
        }
        if let Some(link) = self.link.as_mut() {
            link.summary();
            link.recheck_app(source);
            if let Some(why) = link.output.as_ref().and_then(MicOutput::ended) {
                tracing::warn!("voice: the microphone output ended ({why}), back to A2DP");
                self.held_off = true;
                return self.leave(true, inner, host, a2dp, Some(Msg::new("status.headset_mic_output_failed")));
            }
            if link.codec.is_none() && link.asked.elapsed() > OPEN_TIMEOUT {
                tracing::warn!("voice: no audio connection {} s after asking, back to A2DP", OPEN_TIMEOUT.as_secs());
                self.held_off = true;
                return self.leave(true, inner, host, a2dp, Some(Msg::new("status.headset_mic_failed")));
            }
        }
        self.poll_auto();
        match (self.wanted(), self.link.is_some()) {
            (true, false) if !self.held_off => self.enter(inner, host, source, a2dp),
            (false, true) => {
                if self.mode == HeadsetMic::Auto {
                    tracing::info!("voice: auto: nobody records, back to A2DP");
                } else {
                    tracing::info!("voice: headset mode off, back to A2DP");
                }
                // The plain Streaming status: nothing failed
                self.leave(true, inner, host, a2dp, None)
            }
            _ => Ok(()),
        }
    }

    fn change(&mut self, c: VoiceChange, inner: &Inner, host: &BtHost, a2dp: &mut dyn A2dp) -> Result<(), Msg> {
        tracing::info!(
            "voice: headset mode {} (microphone output {:?})",
            c.mode.as_str(),
            c.mic_output.as_deref().unwrap_or("")
        );
        let output_changed = c.mic_output != self.mic_output;
        if c.mode != self.mode || output_changed {
            self.held_off = false;
            self.hung_up = false;
            self.slc_deadline = None;
            // Auto looks again, at the new output
            self.auto = AutoWatch::default();
        }
        self.mode = c.mode;
        self.mic_output = c.mic_output;
        if !(c.mode.is_on() && output_changed) {
            return Ok(());
        }
        let Some(link) = self.link.as_mut() else { return Ok(()) };
        let changed = match resolve_mic_output(self.mic_output.as_deref()) {
            Some(target) => link.change_output(target),
            None => Err(Msg::new("status.headset_mic_output_failed")),
        };
        match changed {
            Ok(()) => {
                inner.voice_status(link.status());
                Ok(())
            }
            Err(m) => {
                self.held_off = true;
                self.leave(true, inner, host, a2dp, Some(m))
            }
        }
    }

    fn on_event(
        &mut self,
        event: VoiceEvent,
        inner: &Inner,
        host: &BtHost,
        source: &SourceRequest,
        a2dp: &mut dyn A2dp,
    ) -> Result<(), Msg> {
        match event {
            VoiceEvent::Established { codec: id, .. } => {
                let Some(codec) = VoiceCodec::from_hfp_id(id) else {
                    if self.link.is_some() {
                        tracing::warn!("voice: audio connection with an unknown codec {id}, closing it");
                        self.held_off = true;
                        return self.leave(true, inner, host, a2dp, Some(Msg::new("status.headset_mic_failed")));
                    }
                    // The headphones' own: as in adopt
                    if host.voice_enabled() {
                        tracing::warn!("voice: the headphones opened an audio connection with codec {id}, closing it");
                        self.held_off = true;
                        let _ = host.voice_stop();
                    }
                    return Ok(());
                };
                match self.link.as_mut() {
                    Some(link) if link.codec.is_some() => {
                        tracing::debug!("voice: audio connection reported up again ({})", codec.name())
                    }
                    Some(link) => match link.arm(codec, source) {
                        Ok(()) => inner.voice_status(link.status()),
                        Err(m) => {
                            self.held_off = true;
                            return self.leave(true, inner, host, a2dp, Some(m));
                        }
                    },
                    // Opened by the headphones (a button on them)
                    None => return self.adopt(codec, inner, host, source, a2dp),
                }
                Ok(())
            }
            VoiceEvent::Released => {
                if self.link.is_none() {
                    return Ok(());
                }
                // The headphones closed it (or the link went): headset mode
                // ends for this request, as if switched off; a new request
                // (the setting saved again, the stream started again) opens
                // it again. Auto: also once nobody has recorded for a while.
                tracing::info!("voice: the headphones closed the audio connection, back to A2DP");
                if self.mode == HeadsetMic::Auto {
                    self.hung_up = true;
                } else {
                    self.mode = HeadsetMic::Off;
                }
                self.leave(false, inner, host, a2dp, None)
            }
            VoiceEvent::Failed { status } => {
                let Some(link) = self.link.as_ref() else { return Ok(()) };
                // Up already (SCO stopped flowing): bt-host leaves closing
                // it to us; not up: it is closed (or never opened)
                let up = link.codec.is_some();
                tracing::warn!(
                    "voice: the audio connection {} (0x{status:02x}), back to A2DP",
                    if up { "stopped working" } else { "failed" }
                );
                self.held_off = true;
                self.leave(up, inner, host, a2dp, Some(Msg::new("status.headset_mic_failed")))
            }
        }
    }

    /// The headphones opened the audio connection themselves: taken over in
    /// headset mode, closed otherwise by a stack that offers voice. A stack
    /// without voice (every user outside developer mode) leaves it alone,
    /// as before headset mode existed: a voice assistant button on the
    /// headphones opens one.
    fn adopt(
        &mut self,
        codec: VoiceCodec,
        inner: &Inner,
        host: &BtHost,
        source: &SourceRequest,
        a2dp: &mut dyn A2dp,
    ) -> Result<(), Msg> {
        if !host.voice_enabled() {
            if !self.foreign_logged {
                self.foreign_logged = true;
                tracing::info!(
                    "voice: the headphones opened an audio connection ({}); no headset mode, left alone",
                    codec.name()
                );
            }
            return Ok(());
        }
        let target = (self.wanted() && !self.held_off)
            .then(|| resolve_mic_output(self.mic_output.as_deref()))
            .flatten()
            .filter(|t| mic_conflict(t, source).is_none());
        let Some(target) = target else {
            tracing::info!("voice: the headphones opened an audio connection ({}), closing it", codec.name());
            let _ = host.voice_stop();
            return Ok(());
        };
        tracing::info!("voice: the headphones opened the audio connection ({}), taking it over", codec.name());
        a2dp.suspend();
        let (mut link, tx, rx) = VoiceLink::new(target);
        if let Err(e) = host.voice_attach(tx, rx) {
            tracing::warn!("voice: cannot take the audio connection over: {e:?}");
            let _ = host.voice_stop();
            return a2dp.resume();
        }
        match link.arm(codec, source) {
            Ok(()) => {
                inner.voice_status(link.status());
                self.link = Some(link);
                Ok(())
            }
            Err(m) => {
                self.link = Some(link);
                self.held_off = true;
                self.leave(true, inner, host, a2dp, Some(m))
            }
        }
    }

    /// Headset mode asked for and no voice link: A2DP is suspended and the
    /// audio connection asked for, once the service level connection is up.
    /// The output is resolved (a device enumeration) and checked against
    /// the capture only then, not on every tick of the wait.
    fn enter(&mut self, inner: &Inner, host: &BtHost, source: &SourceRequest, a2dp: &mut dyn A2dp) -> Result<(), Msg> {
        match host.voice_check() {
            Ok(()) => {}
            Err(HostError::NotConnected) => {
                let deadline = *self.slc_deadline.get_or_insert_with(|| Instant::now() + SLC_WAIT);
                if Instant::now() >= deadline {
                    tracing::warn!("voice: no HFP service level connection after {} s", SLC_WAIT.as_secs());
                    return self.give_up(inner, Msg::new("status.headset_mic_failed"));
                }
                return Ok(());
            }
            // The headphones are opening one: its Established decides
            Err(HostError::Busy) => return Ok(()),
            // Right after the service level connection: next tick
            Err(HostError::Settling) => return Ok(()),
            Err(e) => {
                tracing::warn!("voice: the audio connection cannot be asked for: {e:?}");
                return self.give_up(inner, Msg::new("status.headset_mic_failed"));
            }
        }
        // Before A2DP is suspended: no output, or one that feeds the capture
        let Some(target) = resolve_mic_output(self.mic_output.as_deref()) else {
            tracing::warn!("voice: no output device for the microphone");
            return self.give_up(inner, Msg::new("status.headset_mic_output_failed"));
        };
        if let Some(key) = mic_conflict(&target, source) {
            tracing::warn!("voice: the microphone output {target} feeds the stream ({source}), headset mode refused");
            return self.give_up(inner, Msg::new(key));
        }
        tracing::info!("voice: headset mode on, suspending A2DP and asking for the audio connection");
        a2dp.suspend();
        let (link, tx, rx) = VoiceLink::new(target);
        match host.voice_start(tx, rx) {
            Ok(()) => {
                self.link = Some(link);
                Ok(())
            }
            Err(e) => {
                tracing::warn!("voice: the audio connection was refused: {e:?}, back to A2DP");
                drop(link);
                a2dp.resume()?;
                self.give_up(inner, Msg::new("status.headset_mic_failed"))
            }
        }
    }

    fn give_up(&mut self, inner: &Inner, msg: Msg) -> Result<(), Msg> {
        self.held_off = true;
        inner.notify(StreamState::Streaming, msg);
        Ok(())
    }

    /// Back to A2DP: the audio connection closed (`stop`: asked to), the
    /// link's parts dropped, then the stream resumed
    fn leave(
        &mut self,
        stop: bool,
        inner: &Inner,
        host: &BtHost,
        a2dp: &mut dyn A2dp,
        msg: Option<Msg>,
    ) -> Result<(), Msg> {
        let Some(mut link) = self.link.take() else { return Ok(()) };
        if stop && let Err(e) = host.voice_stop() {
            tracing::warn!("voice: closing the audio connection: {e:?}");
        }
        let was_up = link.codec.is_some();
        link.disarm();
        if was_up {
            inner.voice_status(None);
        }
        a2dp.resume()?;
        if let Some(m) = msg {
            inner.notify(StreamState::Streaming, m);
        }
        Ok(())
    }

    /// The link to the headphones went: the voice link with it (A2DP comes
    /// back with the reconnect). True when one was up: the caller starts
    /// its capture again. Headset mode stays asked for: the reconnected
    /// stream opens it again.
    pub fn link_lost(&mut self, inner: &Inner) -> bool {
        let Some(mut link) = self.link.take() else { return false };
        tracing::info!("voice: the link went, the voice link with it");
        let was_up = link.codec.is_some();
        link.disarm();
        if was_up {
            inner.voice_status(None);
        }
        self.slc_deadline = None;
        true
    }

    /// After a reconnect: what the old connection reported is stale
    pub fn reconnected(&mut self, inner: &Inner) {
        inner.take_voice_events();
        self.slc_deadline = None;
    }

    /// The session ends: the audio connection closed, A2DP is not resumed
    pub fn end(&mut self, inner: &Inner, host: &BtHost) {
        let Some(mut link) = self.link.take() else { return };
        tracing::info!("voice: the session ends, closing the audio connection");
        let _ = host.voice_stop();
        let was_up = link.codec.is_some();
        link.disarm();
        if was_up {
            inner.voice_status(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wav_path_is_written_not_rendered() {
        assert_eq!(resolve_mic_output(Some("out\\mic.wav")), Some(MicTarget::Wav("out\\mic.wav".into())));
        assert_eq!(resolve_mic_output(Some("C:/x/MIC.WAV")), Some(MicTarget::Wav("C:/x/MIC.WAV".into())));
        assert_eq!(mic_conflict(&MicTarget::Wav("m.wav".into()), &SourceRequest::SystemLoopback), None);
    }

    #[test]
    fn later_microphones_of_a_session_get_files_of_their_own() {
        use std::path::Path;
        let wav = MicTarget::Wav("a/mic.wav".into());
        assert_eq!(wav.nth(1), wav);
        assert_eq!(wav.nth(3), MicTarget::Wav(Path::new("a").join("mic-3.wav")));
        assert_eq!(wav.device_id(), "");
        let cable = MicTarget::Device { id: "{cable}".into(), name: "CABLE Input".into() };
        assert_eq!(cable.nth(2), cable);
        assert_eq!(cable.device_id(), "{cable}");
    }

    #[test]
    fn an_unknown_device_is_kept_for_opening_to_fail() {
        let id = "{0.0.0.00000000}.{not-a-device}";
        assert_eq!(resolve_mic_output(Some(id)), Some(MicTarget::Device { id: id.into(), name: id.into() }));
    }

    #[test]
    fn the_captured_device_is_a_conflict() {
        let cable = MicTarget::Device { id: "{cable}".into(), name: "CABLE Input".into() };
        let virt = SourceRequest::VirtualDevice { id: "{cable}".into(), name: "CABLE".into(), auto_switch: true };
        assert_eq!(mic_conflict(&cable, &virt), Some("error.headset_mic_loop"));
        assert_eq!(mic_conflict(&cable, &SourceRequest::TestTone), None);
    }

    #[test]
    fn an_application_uplink_is_looked_for_once_a_second_while_armed() {
        let app = SourceRequest::Application { exe: "game.exe".into() };
        let t0 = Instant::now();
        let later = |ms| t0 + Duration::from_millis(ms);
        assert!(!app_recheck_due(&app, true, t0, later(200)));
        assert!(!app_recheck_due(&app, true, t0, later(999)));
        assert!(app_recheck_due(&app, true, t0, later(1000)));
        // Not armed yet (no codec): the capture starts with the link
        assert!(!app_recheck_due(&app, false, t0, later(5000)));
        // Other sources reopen nothing here
        assert!(!app_recheck_due(&SourceRequest::SystemLoopback, true, t0, later(5000)));
        assert!(!app_recheck_due(&SourceRequest::TestTone, true, t0, later(5000)));
    }

    #[test]
    fn auto_turns_on_at_once_and_off_after_two_idle_seconds() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut w = AutoWatch::default();
        assert!(w.due(t0));
        assert!(!w.seen(false, t0));
        // Polled at most once a second
        assert!(!w.due(at(999)));
        assert!(w.due(at(1000)));
        // Recording: on at the first poll that sees it
        assert!(w.seen(true, at(1000)));
        // Nobody for less than two seconds (an app opening the device
        // again): still on
        assert!(w.seen(false, at(2000)));
        assert!(w.seen(false, at(3000)));
        assert!(w.seen(true, at(3500)));
        assert!(w.seen(false, at(4500)));
        assert!(w.seen(false, at(5500)));
        // Two seconds of nobody: off
        assert!(!w.seen(false, at(6500)));
        assert!(!w.seen(false, at(9000)));
        assert!(w.seen(true, at(10000)));
    }

    #[test]
    fn auto_counts_a_wav_file_as_recorded_and_no_output_as_not() {
        let mut w = AutoWatch::default();
        assert!(w.look(Some(&MicTarget::Wav("mic.wav".into()))));
        assert!(!w.look(None));
        assert!(w.warned);
        let t0 = Instant::now();
        let mut w = AutoWatch::default();
        assert!(w.poll(Some("out\\mic.wav"), t0));
        assert_eq!(w.target, Some(Some(MicTarget::Wav("out\\mic.wav".into()))));
    }

    #[test]
    fn auto_wants_the_link_only_while_someone_records() {
        let mut h = HeadsetMode::new(HeadsetMic::Auto, Some("mic.wav".into()));
        assert!(!h.wanted());
        h.poll_auto();
        assert!(h.wanted());
        // Hung up by the headphones: not again until nobody records
        h.hung_up = true;
        assert!(!h.wanted());
        assert!(!HeadsetMode::new(HeadsetMic::Off, Some("mic.wav".into())).wanted());
        assert!(HeadsetMode::new(HeadsetMic::On, None).wanted());
        // On and Off do not poll
        let mut on = HeadsetMode::new(HeadsetMic::On, Some("mic.wav".into()));
        on.poll_auto();
        assert!(on.auto.polled.is_none());
    }

    #[test]
    fn unarmed_shells_send_silence_and_drop_packets() {
        let (link, mut tx, mut rx) = VoiceLink::new(MicTarget::Wav("unused.wav".into()));
        let mut out = [0x55u8; 60];
        tx.fill(&mut out);
        assert_eq!(out, [0u8; 60]);
        rx.packet(&[1, 2, 3], 0);
        assert!(link.status().is_none());
    }
}

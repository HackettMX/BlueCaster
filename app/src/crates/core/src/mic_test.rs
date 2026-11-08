//! The microphone test (Diagnostics > Microphone): records a few seconds of
//! the headset microphone over the voice link of the classic stream and
//! plays them back to the headphones, with the level while it records and
//! plays (docs/headset-mic-dev.md, "Microphone test").
//!
//! The test only asks for the voice link: the session's headset mode
//! ([`crate::voice::HeadsetMode`]) opens it while [`MicTest::wanted`],
//! reports it with [`MicTest::link_up`] / [`MicTest::link_down`], and
//! attaches the [`MicTap`] to the decoded microphone and the uplink. With
//! headset mode off the microphone goes nowhere else
//! ([`crate::MicTarget::Test`]); with it on, also to its output as always.
//!
//! The window asks for the state several times a second; a test nobody has
//! asked about for [`KEEPALIVE`] ends (the window closed), and the voice link
//! closes with it unless headset mode wants it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use voice_link::MicTap;

/// The longest recording
pub const MAX_MS: u32 = 10_000;
/// A test not asked about for this long ends
const KEEPALIVE: Duration = Duration::from_secs(5);
/// How long the voice link may take to come up (the stream may start again
/// first, for a stack with voice)
const START_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Waiting for the voice link
    Starting,
    Recording,
    Recorded,
    Playing,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Starting => "starting",
            Phase::Recording => "recording",
            Phase::Recorded => "recorded",
            Phase::Playing => "playing",
        }
    }
}

#[derive(Debug)]
struct State {
    phase: Phase,
    /// Since the phase began
    since: Instant,
    /// The window asked last
    seen: Instant,
    error: Option<&'static str>,
}

/// The test of one service
#[derive(Debug)]
pub struct MicTest {
    tap: MicTap,
    state: Mutex<State>,
    /// The test asked for a stack with voice (the stream started again for
    /// it): stacks offer HFP audio while the test runs
    voice: AtomicBool,
}

impl Default for MicTest {
    fn default() -> Self {
        let now = Instant::now();
        MicTest {
            tap: MicTap::new(),
            state: Mutex::new(State { phase: Phase::Idle, since: now, seen: now, error: None }),
            voice: AtomicBool::new(false),
        }
    }
}

impl MicTest {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The tap every voice link feeds
    pub fn tap(&self) -> MicTap {
        self.tap.clone()
    }

    fn set(&self, s: &mut State, phase: Phase) {
        if s.phase != phase {
            tracing::debug!("mic test: {} -> {}", s.phase.as_str(), phase.as_str());
            s.phase = phase;
            s.since = Instant::now();
        }
    }

    fn end_locked(&self, s: &mut State) {
        self.tap.clear();
        self.voice.store(false, Ordering::Release);
        self.set(s, Phase::Idle);
    }

    /// Ends a test nobody asks about, or one whose link never came
    fn expire(&self, s: &mut State) {
        if s.phase == Phase::Idle {
            return;
        }
        if s.seen.elapsed() > KEEPALIVE {
            tracing::info!("mic test: not asked about for {} s, ended", KEEPALIVE.as_secs());
            self.end_locked(s);
        } else if s.phase == Phase::Starting && s.since.elapsed() > START_TIMEOUT {
            tracing::warn!("mic test: no voice link after {} s", START_TIMEOUT.as_secs());
            s.error = Some("status.headset_mic_failed");
            self.end_locked(s);
        }
    }

    /// The session should hold the voice link for the test
    pub fn wanted(&self) -> bool {
        let mut s = self.lock();
        self.expire(&mut s);
        s.phase != Phase::Idle
    }

    /// The running stack must offer voice for the test
    pub fn needs_voice(&self) -> bool {
        self.voice.load(Ordering::Acquire) && self.wanted()
    }

    /// Starts a new recording: at once when the voice link is up, otherwise
    /// once it is. `restart`: the stream starts again for a stack with voice.
    pub fn record(&self, restart: bool) {
        let mut s = self.lock();
        tracing::info!("mic test: recording{}", if restart { " (the stream starts again with voice)" } else { "" });
        self.tap.clear();
        if restart {
            self.voice.store(true, Ordering::Release);
        }
        s.error = None;
        s.seen = Instant::now();
        // Starting again from Starting keeps the time it has waited
        if s.phase != Phase::Starting {
            self.set(&mut s, Phase::Starting);
        }
    }

    /// The voice link carries audio at `rate` Hz (the session, every tick)
    pub fn link_up(&self, rate: u32) {
        let mut s = self.lock();
        if s.phase == Phase::Starting {
            self.tap.record(rate, MAX_MS);
            self.set(&mut s, Phase::Recording);
        }
    }

    /// No voice link (the session, every tick without one): a recording or
    /// playback under way ends with `why`
    pub fn link_down(&self, why: Option<&'static str>) {
        let mut s = self.lock();
        if !matches!(s.phase, Phase::Recording | Phase::Recorded | Phase::Playing) {
            return;
        }
        tracing::info!("mic test: the voice link closed");
        self.tap.stop_recording();
        self.tap.stop_playing();
        s.error = why.or(Some("status.headset_mic_failed"));
        self.end_locked(&mut s);
    }

    /// The voice link could not be opened for the test (the message key)
    pub fn fail(&self, why: &'static str) {
        let mut s = self.lock();
        if s.phase == Phase::Idle {
            return;
        }
        tracing::info!("mic test: failed ({why})");
        s.error = Some(why);
        self.end_locked(&mut s);
    }

    /// Stops the recording (kept) or the playback
    pub fn stop(&self) {
        let mut s = self.lock();
        s.seen = Instant::now();
        match s.phase {
            Phase::Recording | Phase::Playing => {
                self.tap.stop_recording();
                self.tap.stop_playing();
                self.set(&mut s, Phase::Recorded);
            }
            // The link is not up yet: nothing to keep
            Phase::Starting => self.end_locked(&mut s),
            Phase::Idle | Phase::Recorded => {}
        }
    }

    /// Plays the recording to the headphones
    pub fn play(&self) {
        let mut s = self.lock();
        s.seen = Instant::now();
        if matches!(s.phase, Phase::Recording | Phase::Recorded | Phase::Playing) && self.tap.play() {
            self.set(&mut s, Phase::Playing);
        }
    }

    pub fn end(&self) {
        let mut s = self.lock();
        if s.phase != Phase::Idle {
            tracing::info!("mic test: ended");
        }
        s.error = None;
        self.end_locked(&mut s);
    }

    /// The state for the window (it keeps the test alive)
    pub fn status(&self) -> ipc::MicTestState {
        let mut s = self.lock();
        if s.phase != Phase::Idle {
            s.seen = Instant::now();
        }
        self.expire(&mut s);
        // The level of the phase it was in: the end of a playback still shows
        let level = match s.phase {
            Phase::Playing => {
                self.tap.take_level();
                self.tap.take_play_level()
            }
            Phase::Idle | Phase::Starting => {
                self.tap.take_level();
                0.0
            }
            _ => self.tap.take_level(),
        };
        match s.phase {
            Phase::Recording if !self.tap.recording() => self.set(&mut s, Phase::Recorded),
            Phase::Playing if !self.tap.playing() => self.set(&mut s, Phase::Recorded),
            _ => {}
        }
        ipc::MicTestState {
            phase: s.phase.as_str().into(),
            level,
            recorded_ms: self.tap.recorded_ms(),
            max_ms: MAX_MS,
            played_ms: self.tap.played_ms(),
            error: s.error.map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_once_the_link_is_up_then_plays() {
        let t = MicTest::default();
        assert!(!t.wanted());
        assert_eq!(t.status().phase, "idle");
        t.record(false);
        assert!(t.wanted());
        assert!(!t.needs_voice());
        assert_eq!(t.status().phase, "starting");
        t.link_up(16000);
        assert_eq!(t.status().phase, "recording");
        let tap = t.tap();
        tap.feed(16000, &[8192; 1600]);
        let s = t.status();
        assert_eq!(s.recorded_ms, 100);
        assert!((s.level - 0.25).abs() < 1e-6);
        t.stop();
        assert_eq!(t.status().phase, "recorded");
        t.play();
        assert_eq!(t.status().phase, "playing");
        let mut out = [0i16; 1600];
        tap.play_into(16000, &mut out);
        let s = t.status();
        assert!((s.level - 0.25).abs() < 1e-6);
        assert_eq!(s.phase, "recorded");
        // Still wanted until the window leaves
        assert!(t.wanted());
        t.end();
        assert!(!t.wanted());
        assert_eq!(t.status().recorded_ms, 0);
    }

    #[test]
    fn a_stack_without_voice_is_asked_for_and_the_link_lost_ends_it() {
        let t = MicTest::default();
        t.record(true);
        assert!(t.needs_voice());
        // Before the link: nothing happens
        t.link_down(None);
        assert_eq!(t.status().phase, "starting");
        t.link_up(16000);
        t.link_down(Some("status.headset_mic_failed"));
        let s = t.status();
        assert_eq!(s.phase, "idle");
        assert_eq!(s.error.as_deref(), Some("status.headset_mic_failed"));
        assert!(!t.needs_voice());
        // A new recording clears the error
        t.record(false);
        assert_eq!(t.status().error, None);
        t.fail("error.headset_mic_loop");
        assert_eq!(t.status().error.as_deref(), Some("error.headset_mic_loop"));
    }

    #[test]
    fn stopping_before_the_link_ends_the_test() {
        let t = MicTest::default();
        t.record(false);
        t.stop();
        assert!(!t.wanted());
    }
}

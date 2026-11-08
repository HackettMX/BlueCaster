//! Headset microphone audio (docs/headset-mic-dev.md, "Voice audio plumbing").
//!
//! - [`Renderer`] / [`PcmProducer`]: the microphone audio from the headphones
//!   goes to a Windows render endpoint (a virtual cable), mono 16-bit at the
//!   voice rate, WASAPI shared mode with Windows converting.
//! - [`UplinkSink`] / [`UplinkConsumer`]: a capture (an [`audio_capture::Sink`])
//!   reduced to mono 16-bit at the voice rate, for the headphones.
//! - [`VoiceEncoder`] / [`VoiceDecoder`]: the HFP voice codecs (CVSD, mSBC,
//!   LC3-SWB) between PCM and SCO payload bytes, with H2 framing ([`h2`]) and
//!   packet loss concealment.
//! - [`WavSink`]: the microphone into a WAV file instead (developer CLI,
//!   emulator tests).
//! - [`MicTap`]: the microphone test's level, recording and playback.
//!
//! The codec primitives are in `btstack-sys` (mSBC, CVSD PLC) and `lc3-sys`;
//! this crate itself has no `unsafe`.

#![forbid(unsafe_code)]

mod codec;
mod drift;
pub mod h2;
mod render;
mod resample;
mod tap;
mod uplink;
mod wav;

pub use codec::{DecoderStats, EncoderStats, VoiceCodec, VoiceDecoder, VoiceEncoder};
pub use render::{PcmConsumer, PcmProducer, RenderError, RenderStats, Renderer, pcm_channel};
pub use tap::MicTap;
pub use uplink::{UplinkConsumer, UplinkSink, UplinkStats};
pub use wav::WavSink;

/// How often the parts log their debug summary
pub(crate) const SUMMARY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

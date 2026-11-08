//! LC3 for LE Audio (experimental), encoded by liblc3 (crate `lc3-sys`,
//! Apache-2.0).
//!
//! Not an A2DP codec: no [`crate::CodecKind`], no [`crate::Encoder`]. LE
//! Audio sends one LC3 frame per channel and frame interval (10 or 7.5 ms)
//! in ISO SDUs; the LE encode thread (pipeline's `le` module) drives one
//! [`Lc3ChannelEncoder`] per output channel (v1 Stream: one
//! btstack_lc3_encoder_google_t per channel) and lays the frames out per
//! CIS. The parameters come from the BAP preset of the stream plan.

pub use lc3_sys::Error as Lc3Error;
pub use lc3_sys::{MAX_FRAME_BYTES, MIN_FRAME_BYTES};

use lc3_sys::Lc3Encoder;

/// The LC3 configuration of a stream (from its BAP preset)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lc3Params {
    /// 8000, 16000, 24000, 32000 or 48000 Hz
    pub sample_rate: u32,
    /// 7500 or 10000
    pub frame_us: u32,
    /// Octets per frame and channel (20..=400)
    pub octets: u16,
}

impl Lc3Params {
    /// PCM samples per frame and channel, or the reason the parameters do not
    /// work
    pub fn frame_samples(&self) -> Result<usize, Lc3Error> {
        if !matches!(self.frame_us, 7500 | 10000) {
            return Err(Lc3Error::BadParameters);
        }
        if !(MIN_FRAME_BYTES..=MAX_FRAME_BYTES).contains(&usize::from(self.octets)) {
            return Err(Lc3Error::BadLength);
        }
        lc3_sys::frame_samples(self.frame_us, self.sample_rate).ok_or(Lc3Error::BadParameters)
    }

    /// Bit rate per channel in kbps (e.g. 80 for 100 octets every 10 ms)
    pub fn kbps(&self) -> u32 {
        (u32::from(self.octets) * 8000).checked_div(self.frame_us).unwrap_or(0)
    }
}

/// The encoder of one LC3 channel
#[derive(Debug)]
pub struct Lc3ChannelEncoder {
    enc: Lc3Encoder,
    octets: usize,
}

impl Lc3ChannelEncoder {
    pub fn new(params: Lc3Params) -> Result<Lc3ChannelEncoder, Lc3Error> {
        params.frame_samples()?;
        tracing::debug!(
            "LC3: encoder {} Hz, {} us, {} octets ({} kbps)",
            params.sample_rate,
            params.frame_us,
            params.octets,
            params.kbps()
        );
        let enc = Lc3Encoder::new(params.frame_us, params.sample_rate)?;
        Ok(Lc3ChannelEncoder { enc, octets: usize::from(params.octets) })
    }

    /// PCM samples of one frame
    pub fn frame_samples(&self) -> usize {
        self.enc.frame_samples()
    }

    /// Octets of one encoded frame
    pub fn octets(&self) -> usize {
        self.octets
    }

    /// Encodes one frame of 16-bit mono PCM (`frame_samples()` samples)
    /// into `out` (`octets()` long). No allocation.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<(), Lc3Error> {
        if pcm.len() != self.frame_samples() || out.len() != self.octets {
            return Err(Lc3Error::BadLength);
        }
        self.enc.encode(pcm, 1, out)
    }
}

/// The decoder of one LC3 channel (the headset microphone of an LE Audio
/// conversational stream), with liblc3's packet loss concealment
#[derive(Debug)]
pub struct Lc3ChannelDecoder {
    dec: lc3_sys::Lc3Decoder,
    octets: usize,
}

impl Lc3ChannelDecoder {
    pub fn new(params: Lc3Params) -> Result<Lc3ChannelDecoder, Lc3Error> {
        params.frame_samples()?;
        tracing::debug!(
            "LC3: decoder {} Hz, {} us, {} octets ({} kbps)",
            params.sample_rate,
            params.frame_us,
            params.octets,
            params.kbps()
        );
        let dec = lc3_sys::Lc3Decoder::new(params.frame_us, params.sample_rate)?;
        Ok(Lc3ChannelDecoder { dec, octets: usize::from(params.octets) })
    }

    /// PCM samples of one frame
    pub fn frame_samples(&self) -> usize {
        self.dec.frame_samples()
    }

    /// Octets of one encoded frame
    pub fn octets(&self) -> usize {
        self.octets
    }

    /// Decodes one frame into `pcm` (`frame_samples()` samples, mono 16-bit);
    /// `None` conceals a lost frame. Ok(true) when concealment ran. No
    /// allocation.
    pub fn decode(&mut self, frame: Option<&[u8]>, pcm: &mut [i16]) -> Result<bool, Lc3Error> {
        if pcm.len() != self.frame_samples() {
            return Err(Lc3Error::BadLength);
        }
        self.dec.decode(frame, pcm, 1)
    }
}

/// One frame of digital silence, encoded by a fresh encoder (v1 encoded the
/// silence SDUs once per stream with separate encoder instances); `out` is
/// `params.octets` long
pub fn silence_frame(params: Lc3Params, out: &mut [u8]) -> Result<(), Lc3Error> {
    let mut enc = Lc3ChannelEncoder::new(params)?;
    let zeros = vec![0i16; enc.frame_samples()];
    enc.encode(&zeros, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P48: Lc3Params = Lc3Params { sample_rate: 48000, frame_us: 10000, octets: 100 };

    #[test]
    fn params() {
        assert_eq!(P48.frame_samples(), Ok(480));
        assert_eq!(P48.kbps(), 80);
        let p = Lc3Params { sample_rate: 48000, frame_us: 7500, octets: 75 };
        assert_eq!((p.frame_samples(), p.kbps()), (Ok(360), 80));
        assert_eq!(Lc3Params { sample_rate: 44100, ..P48 }.frame_samples(), Err(Lc3Error::BadParameters));
        assert_eq!(Lc3Params { frame_us: 5000, ..P48 }.frame_samples(), Err(Lc3Error::BadParameters));
        assert_eq!(Lc3Params { octets: 19, ..P48 }.frame_samples(), Err(Lc3Error::BadLength));
        assert_eq!(Lc3Params { octets: 401, ..P48 }.frame_samples(), Err(Lc3Error::BadLength));
    }

    #[test]
    fn encodes_one_frame_per_call() {
        let mut enc = Lc3ChannelEncoder::new(P48).unwrap();
        assert_eq!((enc.frame_samples(), enc.octets()), (480, 100));
        let pcm: Vec<i16> = (0..480).map(|n| ((n as f64 * 0.13).sin() * 8000.0) as i16).collect();
        let mut out = [0u8; 100];
        enc.encode(&pcm, &mut out).unwrap();
        assert!(out.iter().any(|&b| b != 0));
        assert_eq!(enc.encode(&pcm[..479], &mut out), Err(Lc3Error::BadLength));
        assert_eq!(enc.encode(&pcm, &mut out[..99]), Err(Lc3Error::BadLength));
    }

    #[test]
    fn decodes_and_conceals() {
        let p = Lc3Params { sample_rate: 32000, frame_us: 10000, octets: 80 };
        let mut enc = Lc3ChannelEncoder::new(p).unwrap();
        let mut dec = Lc3ChannelDecoder::new(p).unwrap();
        assert_eq!((dec.frame_samples(), dec.octets()), (320, 80));
        let pcm: Vec<i16> = (0..320).map(|n| ((n as f64 * 0.2).sin() * 8000.0) as i16).collect();
        let mut frame = [0u8; 80];
        enc.encode(&pcm, &mut frame).unwrap();
        let mut out = [0i16; 320];
        assert_eq!(dec.decode(Some(&frame), &mut out), Ok(false));
        assert_eq!(dec.decode(None, &mut out), Ok(true));
        assert_eq!(dec.decode(None, &mut out[..319]), Err(Lc3Error::BadLength));
    }

    /// Writes tools/emu/le_audio/mic_frames.py, the emulated headset
    /// microphone's LC3 frames: a 1 kHz tone at half scale, 10 ms frames, at
    /// 16 kHz (16_2_1, 40 octets) and 32 kHz (32_2_1, 80 octets). A 10 ms
    /// frame holds whole periods, so the encoder's frames repeat once it
    /// settled: 10 frames from there loop seamlessly. Run with
    /// `cargo test -p codec --features lc3 -- --ignored mic_frames` (the
    /// output path is relative to this crate).
    #[test]
    #[ignore]
    fn mic_frames() {
        let mut py = String::from(
            "\"\"\"LC3 frames of the emulated headset microphone (emu_le_audio_sink.py --mic).\n\n\
             A 1 kHz tone at half scale (amplitude 0.5), 10 ms frames: FRAMES[rate] is a\n\
             list of 10 frames to send in a loop (100 ms). Generated by the ignored test\n\
             mic_frames in app/src/crates/codec/src/lc3.rs (liblc3 via lc3-sys):\n\
             cargo test -p codec --features lc3 -- --ignored mic_frames\n\"\"\"\n\n\
             TONE_HZ = 1000\nAMPLITUDE = 0.5\nFRAME_US = 10000\nOCTETS = {16000: 40, 32000: 80}\n\nFRAMES = {\n",
        );
        for (rate, octets) in [(16000u32, 40u16), (32000, 80)] {
            let p = Lc3Params { sample_rate: rate, frame_us: 10000, octets };
            let mut enc = Lc3ChannelEncoder::new(p).unwrap();
            let n = enc.frame_samples();
            let mut frames = Vec::new();
            for f in 0..30 {
                let pcm: Vec<i16> = (0..n)
                    .map(|i| {
                        let t = (f * n + i) as f64 / f64::from(rate);
                        ((2.0 * std::f64::consts::PI * 1000.0 * t).sin() * 16384.0).round() as i16
                    })
                    .collect();
                let mut out = vec![0u8; usize::from(octets)];
                enc.encode(&pcm, &mut out).unwrap();
                frames.push(out);
            }
            py.push_str(&format!("    {rate}: [\n"));
            for f in &frames[20..30] {
                let hex: String = f.iter().map(|b| format!("{b:02x}")).collect();
                py.push_str(&format!("        bytes.fromhex('{hex}'),\n"));
            }
            py.push_str("    ],\n");
        }
        py.push_str("}\n");
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../tools/emu/le_audio/mic_frames.py");
        std::fs::write(&path, py).unwrap();
        println!("wrote {}", path.display());
    }

    #[test]
    fn silence_is_deterministic() {
        let (mut a, mut b) = ([0u8; 100], [0u8; 100]);
        silence_frame(P48, &mut a).unwrap();
        silence_frame(P48, &mut b).unwrap();
        assert_eq!(a, b);
        assert!(silence_frame(P48, &mut a[..50]).is_err());
    }
}

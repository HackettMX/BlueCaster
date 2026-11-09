//! Huawei L2HC: the clean-room C99 encoder of L2HC-native-linux (MIT),
//! wrapped by `l2hc-sys`. 48 / 96 kHz, 16 or 24-bit stereo, 10 ms frames
//! (480 / 960 samples), 320 / 480 / 640 / 960 kbps.
//!
//! One `encode` call produces exactly one L2HC frame of `kbps * 10 / 8`
//! bytes (400 / 600 / 800 / 1200), starting with the 0x5C / 0x7E sync byte.
//! Frames larger than the media MTU are split by the packetizer, whose
//! 1-byte payload header marks the first fragment (0x01) and continuations
//! (0x00), as Huawei Fluoride and the reference SPA plugin do.
//!
//! The encoder needs packed little-endian PCM; mono input is duplicated to a
//! stereo wire, and 24-bit input comes from the top 24 bits of the MSB-aligned
//! i32 container (the project's `PcmFormat::I32`).

use l2hc_sys::{L2hcEnc, L2hcEncParam};

use crate::{CodecKind, Encoded, Encoder, EncoderConfig, EncoderDetails, EncoderError, Pcm, PcmFormat, Quality};

const CODEC: CodecKind = CodecKind::L2hc;

/// Largest L2HC frame (960 kbps for 10 ms), for sizing the encode buffer.
/// ABR can raise the bit rate back up to the quality mode's rate, so the
/// buffer is always sized for the largest frame.
const MAX_FRAME_BYTES: usize = 1200;

/// Bit rates by quality, highest first, with their capability bits
const BITRATES: [(u32, u8); 4] = [(960, 0x08), (640, 0x04), (480, 0x02), (320, 0x01)];

/// Huawei's TXQ: more than two packets waiting means RF congestion
const ABR_QUEUE_THRESHOLD: u32 = 2;
/// ~100 ms of clean transmission before stepping back up (one tick per
/// media packet, as in the reference plugin)
const ABR_CLEAN_TICKS: u32 = 10;

fn quality_kbps(quality: Quality) -> u32 {
    match quality {
        Quality::High | Quality::LowLatency => 960,
        Quality::MediumHigh => 640,
        Quality::Standard => 480,
        Quality::Mobile => 320,
    }
}

pub struct L2hcEncoder {
    enc: L2hcEnc,
    format: PcmFormat,
    /// Channels of the input (1 or 2); the wire is always stereo
    channels: usize,
    frame_samples: usize,
    /// One frame of packed s16le / s24le stereo PCM
    packed: Vec<u8>,
    /// The quality mode's (and ABR's ceiling) bit rate, kbps
    base_kbps: u32,
    kbps: u32,
    abr: bool,
    clean_ticks: u32,
    /// The sink's bit rate bits lowered the quality mode's rate
    capped: bool,
}

impl L2hcEncoder {
    pub fn new(config: &EncoderConfig) -> Result<L2hcEncoder, EncoderError> {
        let unsupported = |what: String| EncoderError::Unsupported { codec: CODEC, what };
        // The C encoder has MDCT windows and band tables for N=480 / 960
        // only; other frame sizes would read the wrong table.
        if !matches!(config.sample_rate, 48000 | 96000) {
            return Err(unsupported(format!("sample rate {} Hz (use 48000 or 96000)", config.sample_rate)));
        }
        if !matches!(config.channels, 1 | 2) {
            return Err(unsupported(format!("{} channels", config.channels)));
        }
        let (format, bps) = match config.bits_per_sample {
            16 => (PcmFormat::I16, 16i16),
            24 => (PcmFormat::I32, 24i16),
            n => return Err(unsupported(format!("{n}-bit samples (use 16 or 24)"))),
        };
        let frame_samples = (config.sample_rate / 100) as usize;

        // Keep to the bit rates the sink actually listed (0 = not reported:
        // assume all). The SET_CONFIGURATION asks for the highest of them.
        let allowed = config.peer.l2hc_bitrates;
        let wanted = quality_kbps(config.quality.for_codec(CODEC));
        let base_kbps = if allowed == 0 {
            wanted
        } else {
            BITRATES
                .iter()
                .find(|&&(kbps, bit)| kbps <= wanted && allowed & bit != 0)
                .map(|&(kbps, _)| kbps)
                .unwrap_or(320)
        };
        let capped = base_kbps != wanted;

        let enc = L2hcEnc::new(L2hcEncParam {
            sample_rate: config.sample_rate,
            bps,
            channels: 2,
            frame_samples: frame_samples as i16,
            bitrate_kbps: base_kbps as i16,
        })
        .map_err(|e| EncoderError::Init { codec: CODEC, what: e.to_string() })?;

        let packed = vec![0u8; frame_samples * 2 * (bps as usize / 8)];
        tracing::info!(
            "L2HC: initialized (rate={}, {}-bit, {} ch input, {} samples/frame, {} kbps{}, frame {} bytes{})",
            config.sample_rate,
            bps,
            config.channels,
            frame_samples,
            base_kbps,
            if config.abr { " ABR" } else { "" },
            enc.frame_bytes(),
            if capped { ", capped by the sink's bit rates" } else { "" }
        );
        Ok(L2hcEncoder {
            enc,
            format,
            channels: config.channels as usize,
            frame_samples,
            packed,
            base_kbps,
            kbps: base_kbps,
            abr: config.abr,
            clean_ticks: 0,
            capped,
        })
    }

    /// Repacks one frame into interleaved stereo, s16le or packed s24le
    fn pack(&mut self, pcm: Pcm<'_>) {
        let channels = self.channels;
        match pcm {
            Pcm::I16(s) => {
                for (frame, dst) in s.chunks_exact(channels).zip(self.packed.as_chunks_mut::<4>().0) {
                    let (l, r) = (frame[0], frame[channels - 1]);
                    dst[..2].copy_from_slice(&l.to_le_bytes());
                    dst[2..].copy_from_slice(&r.to_le_bytes());
                }
            }
            Pcm::I32(s) => {
                for (frame, dst) in s.chunks_exact(channels).zip(self.packed.as_chunks_mut::<6>().0) {
                    let (l, r) = (frame[0].to_le_bytes(), frame[channels - 1].to_le_bytes());
                    dst[..3].copy_from_slice(&l[1..]);
                    dst[3..].copy_from_slice(&r[1..]);
                }
            }
        }
    }

    fn set_kbps(&mut self, kbps: u32) {
        if kbps == self.kbps {
            return;
        }
        match self.enc.set_bitrate_kbps(kbps as i16) {
            Ok(()) => {
                tracing::debug!("L2HC: bit rate {} -> {kbps} kbps (frame {} bytes)", self.kbps, self.enc.frame_bytes());
                self.kbps = kbps;
            }
            Err(e) => tracing::warn!("L2HC: changing the bit rate to {kbps} kbps failed: {e}"),
        }
    }
}

impl Encoder for L2hcEncoder {
    fn kind(&self) -> CodecKind {
        CODEC
    }

    fn pcm_format(&self) -> PcmFormat {
        self.format
    }

    /// One 10 ms L2HC frame (480 / 960 PCM frames) per call
    fn pcm_frames_per_encode(&self) -> usize {
        self.frame_samples
    }

    fn max_output_bytes(&self) -> usize {
        MAX_FRAME_BYTES
    }

    fn encode(&mut self, pcm: Pcm<'_>, out: &mut [u8]) -> Result<Encoded, EncoderError> {
        let expected = self.frame_samples * self.channels;
        if pcm.samples() != expected {
            return Err(EncoderError::InputLength { expected, got: pcm.samples() });
        }
        let format_ok = matches!((pcm, self.format), (Pcm::I16(_), PcmFormat::I16) | (Pcm::I32(_), PcmFormat::I32));
        if !format_ok {
            return Err(EncoderError::Encode { codec: CODEC, what: format!("input is not {:?} PCM", self.format) });
        }
        if out.len() < self.enc.frame_bytes() {
            return Err(EncoderError::OutputTooSmall { have: out.len(), need: self.enc.frame_bytes() });
        }
        self.pack(pcm);
        let bytes = self.enc.encode(&self.packed, out).map_err(|e| EncoderError::Encode {
            codec: CODEC,
            what: format!("{e} (output buffer {} bytes)", out.len()),
        })?;
        Ok(Encoded { bytes, frames: 1 })
    }

    fn bitrate_kbps(&self) -> u32 {
        self.kbps
    }

    fn bitrate_capped(&self) -> bool {
        self.capped
    }

    /// Huawei Fluoride's TXQ: step down on a backed-up send queue, step up
    /// one level after ~100 ms of clean transmission, never above the
    /// quality mode's rate.
    fn abr_adjust(&mut self, queue_depth: u32) {
        if !self.abr {
            return;
        }
        if queue_depth > ABR_QUEUE_THRESHOLD {
            self.clean_ticks = 0;
            if let Some(&(next, _)) = BITRATES.iter().find(|&&(kbps, _)| kbps < self.kbps) {
                self.set_kbps(next);
            }
        } else if queue_depth == 0 && self.kbps < self.base_kbps {
            self.clean_ticks += 1;
            if self.clean_ticks >= ABR_CLEAN_TICKS {
                self.clean_ticks = 0;
                let next = BITRATES
                    .iter()
                    .rev()
                    .copied()
                    .find(|&(kbps, _)| kbps > self.kbps && kbps <= self.base_kbps)
                    .map(|(kbps, _)| kbps)
                    .unwrap_or(self.base_kbps);
                self.set_kbps(next);
            }
        } else {
            self.clean_ticks = 0;
        }
    }

    fn abr_active(&self) -> bool {
        self.abr
    }

    fn abr_reset(&mut self) {
        self.clean_ticks = 0;
        self.set_kbps(self.base_kbps);
    }

    fn details(&self) -> EncoderDetails {
        EncoderDetails { frame_bytes: Some(self.enc.frame_bytes() as u32), ..Default::default() }
    }
}

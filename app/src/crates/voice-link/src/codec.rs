//! The HFP voice codecs between PCM and SCO payload bytes.
//!
//! - CVSD: the controller does the air coding; the host side is 16-bit
//!   little-endian linear PCM at 8 kHz, concealed with BTstack's CVSD PLC.
//! - mSBC (16 kHz): 60-byte H2 frames (header, 57-byte frame, padding byte)
//!   per 120 samples; BTstack's Bluedroid encoder and decoder (with H2 sync
//!   and SBC PLC).
//! - LC3-SWB (32 kHz): 60-byte H2 frames (header, 58-byte LC3 frame of
//!   7.5 ms) per 240 samples; liblc3, H2 sync in [`crate::h2`], LC3's PLC.

use btstack_sys::voice::{CvsdPlc, MSBC_FRAME_LEN, MSBC_SAMPLES, MsbcDecoder, MsbcEncoder};
use lc3_sys::{Lc3Decoder, Lc3Encoder};

use crate::h2::{self, H2Frame, H2Framer, H2Sync};
use crate::render::PcmProducer;
use crate::uplink::UplinkConsumer;

/// LC3-SWB: 7.5 ms frames of 58 octets at 32 kHz
const LC3_SWB_FRAME_US: u32 = 7500;
const LC3_SWB_RATE: u32 = 32_000;
const LC3_SWB_SAMPLES: usize = 240;
const LC3_SWB_OCTETS: usize = 58;

/// HFP voice codecs over SCO payloads
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VoiceCodec {
    Cvsd,
    Msbc,
    Lc3Swb,
}

impl VoiceCodec {
    /// Sampling rate of the voice PCM
    pub fn rate(self) -> u32 {
        match self {
            VoiceCodec::Cvsd => 8000,
            VoiceCodec::Msbc => 16_000,
            VoiceCodec::Lc3Swb => LC3_SWB_RATE,
        }
    }

    /// From the HFP codec ID (1 CVSD, 2 mSBC, 3 LC3-SWB)
    pub fn from_hfp_id(id: u8) -> Option<VoiceCodec> {
        match id {
            1 => Some(VoiceCodec::Cvsd),
            2 => Some(VoiceCodec::Msbc),
            3 => Some(VoiceCodec::Lc3Swb),
            _ => None,
        }
    }

    pub fn hfp_id(self) -> u8 {
        match self {
            VoiceCodec::Cvsd => 1,
            VoiceCodec::Msbc => 2,
            VoiceCodec::Lc3Swb => 3,
        }
    }

    /// The name in `VoiceStatus::codec` ("cvsd", "msbc", "lc3_swb")
    pub fn name(self) -> &'static str {
        match self {
            VoiceCodec::Cvsd => "cvsd",
            VoiceCodec::Msbc => "msbc",
            VoiceCodec::Lc3Swb => "lc3_swb",
        }
    }

    /// PCM samples per codec frame (CVSD: none, it is a sample stream)
    fn frame_samples(self) -> usize {
        match self {
            VoiceCodec::Cvsd => 0,
            VoiceCodec::Msbc => MSBC_SAMPLES,
            VoiceCodec::Lc3Swb => LC3_SWB_SAMPLES,
        }
    }
}

/// Counters of a [`VoiceEncoder`]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderStats {
    /// Codec frames encoded (CVSD: none)
    pub frames: u64,
    /// PCM samples taken from the uplink (zeros of an underrun included)
    pub samples: u64,
    /// Payload bytes handed out
    pub bytes: u64,
}

enum Encoder {
    Cvsd,
    Msbc(Box<MsbcEncoder>),
    Lc3(Box<Lc3Encoder>),
}

/// Encoder side: PCM in, SCO payload bytes out, as a byte stream the SCO
/// sender drains in whatever packet length the controller wants
pub struct VoiceEncoder {
    codec: VoiceCodec,
    encoder: Encoder,
    framer: H2Framer,
    /// Encoded bytes not handed out yet: `pending[read..]`
    pending: Vec<u8>,
    read: usize,
    pcm: Vec<i16>,
    stats: EncoderStats,
    summary_at: u64,
}

impl VoiceEncoder {
    pub fn new(codec: VoiceCodec) -> VoiceEncoder {
        let encoder = match codec {
            VoiceCodec::Cvsd => Encoder::Cvsd,
            VoiceCodec::Msbc => Encoder::Msbc(Box::new(MsbcEncoder::new())),
            VoiceCodec::Lc3Swb => Encoder::Lc3(Box::new(
                Lc3Encoder::new(LC3_SWB_FRAME_US, LC3_SWB_RATE).expect("LC3-SWB parameters are valid"),
            )),
        };
        VoiceEncoder {
            codec,
            encoder,
            framer: H2Framer::new(),
            pending: Vec::with_capacity(4 * h2::FRAME_LEN),
            read: 0,
            pcm: vec![0; codec.frame_samples().max(256)],
            stats: EncoderStats::default(),
            summary_at: summary_samples(codec),
        }
    }

    pub fn codec(&self) -> VoiceCodec {
        self.codec
    }

    pub fn stats(&self) -> EncoderStats {
        self.stats
    }

    /// Pulls `out.len()` payload bytes, encoding frames from `pcm` as
    /// needed. CVSD: 16-bit LE PCM pass-through. mSBC: H2 header + 57-byte
    /// frame + 1 padding byte per 120 samples (60 bytes). LC3-SWB: H2
    /// header + 58-byte frame per 240 samples (60 bytes). A frame may be
    /// split across calls; the stream itself never has gaps.
    pub fn fill(&mut self, pcm: &mut UplinkConsumer, out: &mut [u8]) {
        let mut written = 0;
        while written < out.len() {
            if self.read == self.pending.len() {
                self.pending.clear();
                self.read = 0;
                self.encode_next(pcm, out.len() - written);
            }
            let n = (self.pending.len() - self.read).min(out.len() - written);
            out[written..written + n].copy_from_slice(&self.pending[self.read..self.read + n]);
            self.read += n;
            written += n;
        }
        self.stats.bytes += out.len() as u64;
        if self.stats.samples >= self.summary_at {
            self.summary_at = self.stats.samples + summary_samples(self.codec);
            let u = pcm.stats();
            tracing::debug!(
                "voice link: uplink {}: {} frames, {} samples, {} bytes; capture {} samples, {} zeros, {} underruns, \
                 {} overflow, {} dropped / {} repeated (drift), queue {}",
                self.codec.name(),
                self.stats.frames,
                self.stats.samples,
                self.stats.bytes,
                u.captured,
                u.zeros,
                u.underruns,
                u.overflow,
                u.dropped,
                u.duplicated,
                pcm.queued()
            );
        }
    }

    /// Appends the next frame (CVSD: enough samples for `wanted` bytes)
    fn encode_next(&mut self, pcm: &mut UplinkConsumer, wanted: usize) {
        match &mut self.encoder {
            Encoder::Cvsd => {
                let n = wanted.div_ceil(2).clamp(1, self.pcm.len());
                pcm.pop(&mut self.pcm[..n]);
                self.pending.extend(self.pcm[..n].iter().flat_map(|s| s.to_le_bytes()));
                self.stats.samples += n as u64;
            }
            Encoder::Msbc(enc) => {
                let frame: &mut [i16; MSBC_SAMPLES] =
                    (&mut self.pcm[..MSBC_SAMPLES]).try_into().expect("buffer holds a frame");
                pcm.pop(frame);
                let encoded = enc.encode(frame);
                self.pending.extend_from_slice(&self.framer.next_header());
                self.pending.extend_from_slice(&encoded);
                self.pending.push(0);
                debug_assert_eq!(self.pending.len(), h2::HEADER_LEN + MSBC_FRAME_LEN + 1);
                self.stats.frames += 1;
                self.stats.samples += MSBC_SAMPLES as u64;
            }
            Encoder::Lc3(enc) => {
                let frame = &mut self.pcm[..LC3_SWB_SAMPLES];
                pcm.pop(frame);
                self.pending.extend_from_slice(&self.framer.next_header());
                let start = self.pending.len();
                self.pending.resize(start + LC3_SWB_OCTETS, 0);
                if let Err(e) = enc.encode(frame, 1, &mut self.pending[start..]) {
                    // Cannot happen with these parameters; send silence
                    // rather than nothing, the receiver conceals it
                    tracing::warn!("voice link: LC3-SWB encoding failed: {e}");
                }
                self.stats.frames += 1;
                self.stats.samples += LC3_SWB_SAMPLES as u64;
            }
        }
    }
}

impl std::fmt::Debug for VoiceEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceEncoder").field("codec", &self.codec).field("stats", &self.stats).finish()
    }
}

/// Counters of a [`VoiceDecoder`]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecoderStats {
    /// SCO payloads given to the decoder
    pub packets: u64,
    /// Of them, with a packet status flag other than 0
    pub bad_packets: u64,
    /// Codec frames decoded (CVSD: packets passed as good)
    pub frames: u64,
    /// Frames concealed (CVSD: packets)
    pub concealed: u64,
    /// PCM samples pushed out (and how many of them did not fit)
    pub samples: u64,
    pub overflow: u64,
}

enum Decoder {
    Cvsd { plc: Box<CvsdPlc>, odd: Option<u8>, input: Vec<i16>, output: Vec<i16> },
    Msbc(Box<MsbcDecoder>),
    Lc3 { decoder: Box<Lc3Decoder>, sync: Box<H2Sync>, pcm: Box<[i16; LC3_SWB_SAMPLES]>, frames: u64, concealed: u64 },
}

/// Decoder side: SCO payloads (with the HCI packet status flag) in, PCM
/// out, with packet loss concealment for bad or missing packets
pub struct VoiceDecoder {
    codec: VoiceCodec,
    decoder: Decoder,
    stats: DecoderStats,
    summary_at: u64,
}

impl VoiceDecoder {
    pub fn new(codec: VoiceCodec) -> VoiceDecoder {
        let decoder = match codec {
            VoiceCodec::Cvsd => {
                Decoder::Cvsd { plc: Box::new(CvsdPlc::new()), odd: None, input: Vec::new(), output: Vec::new() }
            }
            VoiceCodec::Msbc => Decoder::Msbc(Box::new(MsbcDecoder::new())),
            VoiceCodec::Lc3Swb => Decoder::Lc3 {
                decoder: Box::new(
                    Lc3Decoder::new(LC3_SWB_FRAME_US, LC3_SWB_RATE).expect("LC3-SWB parameters are valid"),
                ),
                sync: Box::new(H2Sync::new()),
                pcm: Box::new([0; LC3_SWB_SAMPLES]),
                frames: 0,
                concealed: 0,
            },
        };
        VoiceDecoder { codec, decoder, stats: DecoderStats::default(), summary_at: summary_samples(codec) }
    }

    pub fn codec(&self) -> VoiceCodec {
        self.codec
    }

    pub fn stats(&self) -> DecoderStats {
        let mut s = self.stats;
        if let Decoder::Msbc(d) = &self.decoder {
            let m = d.stats();
            s.frames = u64::from(m.good);
            s.concealed = u64::from(m.bad) + u64::from(m.zero);
        }
        s
    }

    /// `status`: HCI SCO packet status flag (0 ok, 1 possibly invalid,
    /// 2 no data, 3 partially lost; anything but 0 is concealed). Decoded
    /// PCM goes to `out` (push).
    pub fn decode(&mut self, payload: &[u8], status: u8, out: &mut PcmProducer) {
        let bad = status != 0;
        self.stats.packets += 1;
        self.stats.bad_packets += u64::from(bad);
        let stats = &mut self.stats;
        let mut emit = |pcm: &[i16]| {
            let n = out.push(pcm);
            stats.samples += pcm.len() as u64;
            stats.overflow += (pcm.len() - n) as u64;
        };
        match &mut self.decoder {
            Decoder::Cvsd { plc, odd, input, output } => {
                input.clear();
                let mut bytes = payload;
                if let Some(lo) = *odd
                    && let Some((&hi, rest)) = bytes.split_first()
                {
                    input.push(i16::from_le_bytes([lo, hi]));
                    bytes = rest;
                    *odd = None;
                }
                let (pairs, rest) = bytes.as_chunks::<2>();
                if let [last] = rest {
                    *odd = Some(*last);
                }
                input.extend(pairs.iter().map(|&b| i16::from_le_bytes(b)));
                if input.is_empty() {
                    return;
                }
                output.resize(input.len(), 0);
                plc.process(bad, input, output);
                emit(output);
            }
            Decoder::Msbc(d) => d.decode(status, payload, emit),
            Decoder::Lc3 { decoder, sync, pcm, frames, concealed } => sync.push(bad, payload, |frame| {
                let data = match frame {
                    H2Frame::Good(b) => Some(&b[h2::HEADER_LEN..h2::HEADER_LEN + LC3_SWB_OCTETS]),
                    H2Frame::Bad => None,
                };
                match decoder.decode(data, &mut pcm[..], 1) {
                    // PLC also runs for a frame that does not decode
                    Ok(plc) if plc || data.is_none() => *concealed += 1,
                    Ok(_) => *frames += 1,
                    Err(e) => {
                        tracing::debug!("voice link: LC3-SWB decoding failed: {e}");
                        pcm.fill(0);
                        *concealed += 1;
                    }
                }
                emit(&pcm[..]);
            }),
        }
        match &self.decoder {
            Decoder::Cvsd { plc, .. } => {
                let (good, bad) = plc.stats();
                self.stats.frames = u64::from(good);
                self.stats.concealed = u64::from(bad);
            }
            Decoder::Lc3 { frames, concealed, .. } => {
                self.stats.frames = *frames;
                self.stats.concealed = *concealed;
            }
            // Its own counters, read in stats()
            Decoder::Msbc(_) => {}
        }
        if self.stats.samples >= self.summary_at {
            self.summary_at = self.stats.samples + summary_samples(self.codec);
            let s = self.stats();
            tracing::debug!(
                "voice link: downlink {}: {} packets ({} bad), {} frames, {} concealed, {} samples, {} overflow",
                self.codec.name(),
                s.packets,
                s.bad_packets,
                s.frames,
                s.concealed,
                s.samples,
                s.overflow
            );
        }
    }
}

impl std::fmt::Debug for VoiceDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceDecoder").field("codec", &self.codec).field("stats", &self.stats()).finish()
    }
}

/// Samples in one summary interval
fn summary_samples(codec: VoiceCodec) -> u64 {
    u64::from(codec.rate()) * crate::SUMMARY_INTERVAL.as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{PcmConsumer, pcm_channel};
    use crate::uplink::UplinkSink;
    use audio_capture::{Format, SampleFormat, Sink};

    const ALL: [VoiceCodec; 3] = [VoiceCodec::Cvsd, VoiceCodec::Msbc, VoiceCodec::Lc3Swb];

    fn sine(rate: u32, hz: f64, n: usize) -> Vec<i16> {
        (0..n).map(|i| (8000.0 * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as i16).collect()
    }

    /// Runs `pcm` through an uplink and the encoder in SCO packets of
    /// `packet` bytes, the capture topping up the queue in 10 ms blocks as
    /// a real one does; returns the payload stream
    fn encode(codec: VoiceCodec, pcm: &[i16], packet: usize) -> Vec<u8> {
        let rate = codec.rate();
        let (mut sink, mut uplink) = UplinkSink::new(rate, 200);
        let fmt = Format { sample_rate: rate, channels: 1, sample_format: SampleFormat::I16 };
        let block = rate as usize / 100;
        let mut pos = 0;
        let mut enc = VoiceEncoder::new(codec);
        let mut stream = Vec::new();
        let mut buf = vec![0u8; packet];
        // The bytes of all but the last 40 ms: the queue never runs low (no
        // drift correction, no underrun zeros, which mSBC's decoder would
        // count as lost frames)
        let bytes_per_sample = match codec {
            VoiceCodec::Cvsd => 2.0,
            VoiceCodec::Msbc => 0.5,
            VoiceCodec::Lc3Swb => 0.25,
        };
        let total = ((pcm.len() - rate as usize / 25) as f64 * bytes_per_sample) as usize;
        while stream.len() < total {
            // To where the drift control keeps the level after priming
            // (prefill + half the tolerance): then it has nothing to do. A
            // packet longer than a frame takes two frames now and then, the
            // second one a frame lower.
            let extra = if codec != VoiceCodec::Cvsd && packet > h2::FRAME_LEN { codec.frame_samples() } else { 0 };
            let level = (rate * (crate::uplink::PREFILL_MS + crate::uplink::TOLERANCE_MS / 2) / 1000) as usize + extra;
            while uplink.queued() < level && pos < pcm.len() {
                let end = (pos + block).min(pcm.len());
                let bytes: Vec<u8> = pcm[pos..end].iter().flat_map(|s| s.to_le_bytes()).collect();
                sink.audio(&bytes, end - pos, fmt);
                pos = end;
            }
            enc.fill(&mut uplink, &mut buf);
            stream.extend_from_slice(&buf);
        }
        let s = uplink.stats();
        assert_eq!((s.underruns, s.dropped, s.duplicated), (0, 0, 0), "{s:?}");
        stream
    }

    fn drain(c: &mut PcmConsumer) -> Vec<i16> {
        let mut v = vec![0; c.queued()];
        let n = c.pop(&mut v);
        v.truncate(n);
        v
    }

    fn decode(
        codec: VoiceCodec,
        stream: &[u8],
        packet: usize,
        status: impl Fn(usize) -> u8,
    ) -> (Vec<i16>, DecoderStats) {
        let mut dec = VoiceDecoder::new(codec);
        let (mut p, mut c) = pcm_channel(codec.rate() as usize * 4);
        for (i, chunk) in stream.chunks(packet).enumerate() {
            dec.decode(chunk, status(i), &mut p);
        }
        (drain(&mut c), dec.stats())
    }

    /// Best SNR over the delays up to 1500 samples (uplink prefill and
    /// codec delay), over 2000 samples after the first 4000
    fn snr(reference: &[i16], decoded: &[i16]) -> (usize, f64) {
        let mut best = (0, f64::MIN);
        for d in 0..1500 {
            let (mut sig, mut noise) = (0.0, 0.0);
            for (i, &r) in reference.iter().enumerate().take(6000).skip(4000) {
                let r = f64::from(r);
                let e = f64::from(*decoded.get(i + d).unwrap_or(&0)) - r;
                sig += r * r;
                noise += e * e;
            }
            let snr = 10.0 * (sig / noise.max(1.0)).log10();
            if snr > best.1 {
                best = (d, snr);
            }
        }
        best
    }

    #[test]
    fn codec_ids_rates_names() {
        for c in ALL {
            assert_eq!(VoiceCodec::from_hfp_id(c.hfp_id()), Some(c));
        }
        assert_eq!(VoiceCodec::from_hfp_id(0), None);
        assert_eq!(VoiceCodec::from_hfp_id(4), None);
        assert_eq!(ALL.map(VoiceCodec::rate), [8000, 16_000, 32_000]);
        assert_eq!(ALL.map(VoiceCodec::name), ["cvsd", "msbc", "lc3_swb"]);
    }

    #[test]
    fn round_trips_in_odd_packet_lengths() {
        for codec in ALL {
            let rate = codec.rate();
            let pcm = sine(rate, 1000.0 * f64::from(rate) / 16_000.0, rate as usize);
            for packet in [24, 48, 60, 72] {
                let stream = encode(codec, &pcm, packet);
                let (out, stats) = decode(codec, &stream, packet, |_| 0);
                let (delay, snr) = snr(&pcm, &out);
                let min = match codec {
                    VoiceCodec::Cvsd => 90.0,
                    VoiceCodec::Msbc => 20.0,
                    // A perceptual codec: its waveform SNR on a pure tone
                    // is lower than its quality suggests (about 18 dB here)
                    VoiceCodec::Lc3Swb => 15.0,
                };
                assert!(snr > min, "{codec:?} packet {packet}: SNR {snr:.1} dB at delay {delay}");
                assert_eq!(stats.concealed, 0, "{codec:?} packet {packet}: {stats:?}");
                // The decoded stream is as long as the encoded one (whole frames)
                let frame_bytes = match codec {
                    VoiceCodec::Cvsd => 2,
                    _ => h2::FRAME_LEN,
                };
                let frames = stream.len() / frame_bytes;
                let samples = if codec == VoiceCodec::Cvsd { frames } else { frames * codec.frame_samples() };
                assert_eq!(out.len(), samples, "{codec:?} packet {packet}");
            }
        }
    }

    #[test]
    fn h2_headers_carry_the_sequence() {
        for codec in [VoiceCodec::Msbc, VoiceCodec::Lc3Swb] {
            let stream = encode(codec, &sine(codec.rate(), 500.0, 4000), 48);
            for (i, f) in stream.as_chunks::<{ h2::FRAME_LEN }>().0.iter().enumerate() {
                assert_eq!(f[..2], [0x01, h2::SEQUENCE[i % 4]], "{codec:?} frame {i}");
                if codec == VoiceCodec::Msbc {
                    assert_eq!(f[2], 0xAD);
                    assert_eq!(f[59], 0);
                }
            }
        }
    }

    #[test]
    fn cvsd_is_little_endian_pcm() {
        let pcm: Vec<i16> = (0..1000).map(|i| i * 7 - 3000).collect();
        let stream = encode(VoiceCodec::Cvsd, &pcm, 60);
        // After the uplink's prefill (zeros) the samples follow as they were
        let lead = stream.as_chunks::<2>().0.iter().take_while(|b| **b == [0, 0]).count();
        let first = 2 * lead;
        assert_eq!(&stream[first..first + 4], &[0x48, 0xF4, 0x4F, 0xF4]);
    }

    #[test]
    fn sync_recovers_after_a_garbage_prefix() {
        for codec in [VoiceCodec::Msbc, VoiceCodec::Lc3Swb] {
            let pcm = sine(codec.rate(), 700.0, codec.rate() as usize / 2);
            let mut stream = vec![0x01, 0x18, 0x5A, 0x01, 0x00, 0xAD, 0x77, 0x13, 0x01];
            let clean = encode(codec, &pcm, 60);
            stream.extend_from_slice(&clean);
            let (out, stats) = decode(codec, &stream, 48, |_| 0);
            assert_eq!(stats.frames as usize, clean.len() / h2::FRAME_LEN, "{codec:?}: {stats:?}");
            // The garbage is less than a frame: nothing concealed
            assert_eq!(stats.concealed, 0, "{codec:?}: {stats:?}");
            let (_, snr) = self::snr(&pcm, &out);
            assert!(snr > 20.0, "{codec:?}: {snr:.1} dB");
        }
    }

    #[test]
    fn bad_packets_are_concealed_in_time() {
        for codec in ALL {
            let pcm = sine(codec.rate(), 400.0, codec.rate() as usize / 2);
            let stream = encode(codec, &pcm, 60);
            let good = decode(codec, &stream, 60, |_| 0).0;
            // Packets 50 and 51 lost ("no data": zeros with status 2)
            let mut lossy = stream.clone();
            lossy[50 * 60..52 * 60].fill(0);
            let (out, stats) = decode(codec, &lossy, 60, |i| if i == 50 || i == 51 { 2 } else { 0 });
            assert_eq!(stats.bad_packets, 2, "{codec:?}");
            assert!(stats.concealed >= 1, "{codec:?}: {stats:?}");
            // The timing stays: as many samples as without the loss (mSBC
            // conceals once a frame's worth of bad bytes has come in)
            assert!(out.len() + codec.frame_samples() >= good.len() && out.len() <= good.len(), "{codec:?}");
            // The concealment is not silence
            let spot = match codec {
                VoiceCodec::Cvsd => 50 * 30..52 * 30,
                VoiceCodec::Msbc => 50 * 120..52 * 120,
                VoiceCodec::Lc3Swb => 50 * 240..52 * 240,
            };
            assert!(out[spot].iter().any(|s| s.unsigned_abs() > 1000), "{codec:?}");
        }
    }
}

//! The emulator's hands-free unit sends tools/emu/voice_frames.py as its
//! microphone (mSBC, LC3-SWB): they must decode to its 1.5 kHz tone. And
//! what BlueCaster's encoder sends for the test tone (1 kHz left, 1.5 kHz
//! right, mixed to mono) must decode to both tones: the emulator has no
//! mSBC / LC3 decoder of its own, it checks only the H2 headers.

use audio_capture::{Format, SampleFormat, Sink};
use voice_link::{UplinkSink, VoiceCodec, VoiceDecoder, VoiceEncoder, h2, pcm_channel};

fn frames_file() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../tools/emu/voice_frames.py");
    std::fs::read_to_string(path).expect("tools/emu/voice_frames.py")
}

/// The hex frames of the list `name = [ ... ]`
fn frames(text: &str, name: &str) -> Vec<Vec<u8>> {
    let start = text.find(&format!("{name} = [")).unwrap_or_else(|| panic!("{name} missing"));
    let body = &text[start..text[start..].find(']').map(|e| start + e).unwrap()];
    body.split("bytes.fromhex('")
        .skip(1)
        .map(|s| {
            let hex = &s[..s.find('\'').unwrap()];
            (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
        })
        .collect()
}

/// Share of the energy at `hz` (1.0: a pure tone), Goertzel
fn tone_share(x: &[i16], rate: u32, hz: f64) -> f64 {
    let k = 2.0 * (std::f64::consts::TAU * hz / f64::from(rate)).cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    let mut total = 0.0;
    for &v in x {
        let v = f64::from(v);
        (s1, s2) = (v + k * s1 - s2, s1);
        total += v * v;
    }
    let power = s1 * s1 + s2 * s2 - k * s1 * s2;
    if total == 0.0 { 0.0 } else { 2.0 * power / x.len() as f64 / total }
}

fn decode(codec: VoiceCodec, stream: &[u8]) -> Vec<i16> {
    let mut dec = VoiceDecoder::new(codec);
    let (mut p, mut c) = pcm_channel(codec.rate() as usize * 4);
    for packet in stream.chunks(h2::FRAME_LEN) {
        dec.decode(packet, 0, &mut p);
    }
    let s = dec.stats();
    assert_eq!(s.concealed, 0, "{codec:?}: {s:?}");
    let mut out = vec![0; c.queued()];
    let n = c.pop(&mut out);
    out.truncate(n);
    out
}

#[test]
fn emulator_frames_decode_to_the_tone() {
    let text = frames_file();
    for (codec, name, len) in [(VoiceCodec::Msbc, "MSBC_FRAMES", 57), (VoiceCodec::Lc3Swb, "LC3_SWB_FRAMES", 58)] {
        let list = frames(&text, name);
        assert_eq!(list.len(), 4, "{name}");
        assert!(list.iter().all(|f| f.len() == len), "{name}");
        // As the HF sends them: H2 header, frame, padding (mSBC), 2 s
        let mut framer = h2::H2Framer::new();
        let mut stream = Vec::new();
        for i in 0..(2 * 1000 * 4 / 30) {
            stream.extend_from_slice(&framer.next_header());
            stream.extend_from_slice(&list[i % 4]);
            stream.resize(stream.len().next_multiple_of(h2::FRAME_LEN), 0);
        }
        let pcm = decode(codec, &stream);
        let rate = codec.rate();
        let share = tone_share(&pcm[rate as usize / 5..], rate, 1500.0);
        assert!(share > 0.9, "{codec:?}: 1.5 kHz share {share:.3}");
    }
}

#[test]
fn the_encoder_sends_the_test_tone_mix() {
    for codec in [VoiceCodec::Cvsd, VoiceCodec::Msbc, VoiceCodec::Lc3Swb] {
        let rate = codec.rate();
        // The test tone as the capture delivers it: float stereo
        let (mut sink, mut uplink) = UplinkSink::new(rate, 200);
        let fmt = Format { sample_rate: rate, channels: 2, sample_format: SampleFormat::F32 };
        let block = rate as usize / 100;
        let mut n = 0usize;
        let mut enc = VoiceEncoder::new(codec);
        let mut stream = Vec::new();
        let mut packet = [0u8; 60];
        let bytes_per_s = if codec == VoiceCodec::Cvsd { 2 * rate as usize } else { 8000 };
        while stream.len() < 2 * bytes_per_s {
            while uplink.queued() < rate as usize * 30 / 1000 {
                let mut bytes = Vec::with_capacity(block * 8);
                for i in n..n + block {
                    let t = i as f64 / f64::from(rate);
                    let l = (0.5 * (std::f64::consts::TAU * 1000.0 * t).sin()) as f32;
                    let r = (0.5 * (std::f64::consts::TAU * 1500.0 * t).sin()) as f32;
                    bytes.extend_from_slice(&l.to_le_bytes());
                    bytes.extend_from_slice(&r.to_le_bytes());
                }
                sink.audio(&bytes, block, fmt);
                n += block;
            }
            enc.fill(&mut uplink, &mut packet);
            stream.extend_from_slice(&packet);
        }
        if codec != VoiceCodec::Cvsd {
            // Every 60-byte packet starts with the next H2 header
            for (i, f) in stream.chunks(h2::FRAME_LEN).enumerate() {
                assert_eq!(f[..2], [0x01, h2::SEQUENCE[i % 4]], "{codec:?} packet {i}");
            }
        }
        let pcm = if codec == VoiceCodec::Cvsd {
            stream.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
        } else {
            decode(codec, &stream)
        };
        let tail = &pcm[rate as usize / 2..];
        let (a, b) = (tone_share(tail, rate, 1000.0), tone_share(tail, rate, 1500.0));
        assert!(a > 0.4 && b > 0.4 && a + b > 0.9, "{codec:?}: 1 kHz {a:.3}, 1.5 kHz {b:.3}");
    }
}

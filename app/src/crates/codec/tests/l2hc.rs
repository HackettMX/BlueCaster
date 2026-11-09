//! L2HC encoder end to end: `codec::create` -> `Encoder::encode`, frame
//! sizes / sync bytes, quality modes, the sink's bit rate caps and ABR.

#![cfg(feature = "l2hc")]

use codec::{CodecKind, Encoded, EncoderConfig, Pcm, PcmFormat, PeerParams, Quality, create};

fn config(sample_rate: u32, quality: Quality, bits_per_sample: u8, abr: bool) -> EncoderConfig {
    EncoderConfig { sample_rate, channels: 2, quality, mtu: 679, bits_per_sample, abr, peer: PeerParams::default() }
}

/// A 440 Hz + 1.2 kHz tone, interleaved stereo
fn tone16(frames: usize, seed: usize) -> Vec<i16> {
    (0..frames)
        .flat_map(|i| {
            let t = (seed * frames + i) as f64 / 48000.0;
            let l = (t * 2.0 * std::f64::consts::PI * 440.0).sin() * 0.5
                + (t * 2.0 * std::f64::consts::PI * 1200.0).sin() * 0.3;
            let r = (t * 2.0 * std::f64::consts::PI * 440.0).cos() * 0.5;
            [(l * 30000.0) as i16, (r * 30000.0) as i16]
        })
        .collect()
}

fn tone32(frames: usize, seed: usize) -> Vec<i32> {
    tone16(frames, seed).into_iter().map(|s| i32::from(s) << 16).collect()
}

#[test]
fn encodes_10ms_frames() {
    let mut enc = create(CodecKind::L2hc, &config(48000, Quality::High, 16, false)).unwrap();
    assert_eq!(enc.kind(), CodecKind::L2hc);
    assert_eq!(enc.pcm_format(), PcmFormat::I16);
    assert_eq!(enc.pcm_frames_per_encode(), 480);
    assert_eq!(enc.pcm_frames_per_codec_frame(), 480);
    assert_eq!(enc.bitrate_kbps(), 960);
    assert_eq!(enc.max_output_bytes(), 1200);
    let mut out = vec![0u8; enc.max_output_bytes()];
    for f in 0..20 {
        let pcm = tone16(480, f);
        let Encoded { bytes, frames } = enc.encode(Pcm::I16(&pcm), &mut out).unwrap();
        assert_eq!(bytes, 1200, "frame {f}");
        assert_eq!(frames, 1);
        assert_eq!(out[0], 0x5C, "48 kHz sync byte of frame {f}");
    }
    assert_eq!(enc.details().frame_bytes, Some(1200));
    // A short input is rejected rather than over-read
    assert!(enc.encode(Pcm::I16(&tone16(100, 0)), &mut out).is_err());
}

#[test]
fn quality_modes_set_the_frame_length() {
    for (quality, bytes) in
        [(Quality::High, 1200), (Quality::MediumHigh, 800), (Quality::Standard, 600), (Quality::Mobile, 400)]
    {
        let mut enc = create(CodecKind::L2hc, &config(48000, quality, 16, false)).unwrap();
        let mut out = vec![0u8; enc.max_output_bytes()];
        let pcm = tone16(480, 0);
        assert_eq!(enc.encode(Pcm::I16(&pcm), &mut out).unwrap().bytes, bytes, "{quality:?}");
        assert_eq!(out[0], 0x5C);
    }
}

#[test]
fn ninety_six_khz_and_24_bit() {
    let mut enc = create(CodecKind::L2hc, &config(96000, Quality::High, 24, false)).unwrap();
    assert_eq!(enc.pcm_format(), PcmFormat::I32);
    assert_eq!(enc.pcm_frames_per_encode(), 960);
    let mut out = vec![0u8; enc.max_output_bytes()];
    let pcm = tone32(960, 0);
    assert_eq!(enc.encode(Pcm::I32(&pcm), &mut out).unwrap().bytes, 1200);
    // (1<<5) | (sr_idx=3 << 3) | (stereo << 2) | 10 ms, low-bitrate bit 0
    assert_eq!(out[0], 0x7E);
    // Unsupported rates / bit depths are refused before the C encoder sees them
    assert!(create(CodecKind::L2hc, &config(44100, Quality::High, 16, false)).is_err());
    assert!(create(CodecKind::L2hc, &config(48000, Quality::High, 32, false)).is_err());
}

#[test]
fn mono_input_goes_out_as_stereo() {
    let mut cfg = config(48000, Quality::Mobile, 16, false);
    cfg.channels = 1;
    let mut enc = create(CodecKind::L2hc, &cfg).unwrap();
    let mut out = vec![0u8; enc.max_output_bytes()];
    let mono: Vec<i16> = tone16(480, 0).into_iter().step_by(2).collect();
    assert_eq!(enc.encode(Pcm::I16(&mono), &mut out).unwrap().bytes, 400);
    assert_eq!(out[0], 0x5C);
}

#[test]
fn the_sinks_bit_rates_cap_the_quality() {
    let mut cfg = config(48000, Quality::High, 16, false);
    cfg.peer.l2hc_bitrates = 0x01; // 320 kbps only
    let enc = create(CodecKind::L2hc, &cfg).unwrap();
    assert_eq!(enc.bitrate_kbps(), 320);
    assert!(enc.bitrate_capped());
}

#[test]
fn abr_steps_down_on_a_backed_up_queue_and_recovers() {
    let mut enc = create(CodecKind::L2hc, &config(48000, Quality::High, 16, true)).unwrap();
    assert!(enc.abr_active());
    let mut out = vec![0u8; enc.max_output_bytes()];
    let pcm = tone16(480, 0);

    enc.abr_adjust(3);
    assert_eq!(enc.bitrate_kbps(), 640);
    assert_eq!(enc.encode(Pcm::I16(&pcm), &mut out).unwrap().bytes, 800);
    enc.abr_adjust(3);
    assert_eq!(enc.bitrate_kbps(), 480);
    enc.abr_adjust(3);
    assert_eq!(enc.bitrate_kbps(), 320);
    // The floor holds under continued congestion
    enc.abr_adjust(10);
    assert_eq!(enc.bitrate_kbps(), 320);
    // Ten clean ticks raise one step, never above the quality's rate
    for _ in 0..10 {
        enc.abr_adjust(0);
    }
    assert_eq!(enc.bitrate_kbps(), 480);
    enc.abr_reset();
    assert_eq!(enc.bitrate_kbps(), 960);
    assert_eq!(enc.encode(Pcm::I16(&pcm), &mut out).unwrap().bytes, 1200);
}

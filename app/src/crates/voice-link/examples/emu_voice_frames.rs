//! Writes tools/emu/voice_frames.py: the frames the emulator's hands-free
//! unit (tools/emu/hfp_audio.py) sends as its microphone for mSBC and
//! LC3-SWB, a 1.5 kHz tone encoded by the codecs BlueCaster uses.
//!
//! cargo run -p voice-link --example emu_voice_frames > tools/emu/voice_frames.py
//!
//! Four frames make whole periods of the tone at both rates (4 x 120 samples
//! at 16 kHz and 4 x 240 at 32 kHz are 45 periods), so they repeat without a
//! seam. They are taken after eight frames, once the encoders settled.

use btstack_sys::voice::{MSBC_SAMPLES, MsbcEncoder};
use lc3_sys::Lc3Encoder;

const TONE_HZ: f64 = 1500.0;
const AMPLITUDE: f64 = 8000.0;
const LOOP: usize = 4;
const SETTLE: usize = 8;

fn tone(rate: u32, start: usize, n: usize) -> Vec<i16> {
    (start..start + n)
        .map(|i| (AMPLITUDE * (std::f64::consts::TAU * TONE_HZ * i as f64 / f64::from(rate)).sin()).round() as i16)
        .collect()
}

fn msbc() -> Vec<Vec<u8>> {
    let mut enc = MsbcEncoder::new();
    (0..SETTLE + LOOP)
        .map(|f| {
            let pcm: [i16; MSBC_SAMPLES] = tone(16_000, f * MSBC_SAMPLES, MSBC_SAMPLES).try_into().unwrap();
            enc.encode(&pcm).to_vec()
        })
        .skip(SETTLE)
        .collect()
}

fn lc3_swb() -> Vec<Vec<u8>> {
    const SAMPLES: usize = 240;
    let mut enc = Lc3Encoder::new(7500, 32_000).expect("LC3-SWB parameters");
    (0..SETTLE + LOOP)
        .map(|f| {
            let mut out = vec![0u8; 58];
            enc.encode(&tone(32_000, f * SAMPLES, SAMPLES), 1, &mut out).expect("LC3 encoding");
            out
        })
        .skip(SETTLE)
        .collect()
}

fn print_list(name: &str, frames: &[Vec<u8>]) {
    println!("{name} = [");
    for f in frames {
        let hex: String = f.iter().map(|b| format!("{b:02x}")).collect();
        println!("    bytes.fromhex('{hex}'),");
    }
    println!("]");
}

fn main() {
    println!(
        "\"\"\"The microphone frames of the emulator's hands-free unit (hfp_audio.py):\n\
         a 1.5 kHz tone (amplitude 8000 of 32767) encoded by BlueCaster's codecs, four\n\
         frames that repeat seamlessly (45 periods). Without the H2 header, which the\n\
         sender adds with its sequence number; mSBC also without the padding byte.\n\
         \n\
         Generated, do not edit:\n\
         cargo run -p voice-link --example emu_voice_frames > tools/emu/voice_frames.py\n\
         (app/src/crates/voice-link/examples/emu_voice_frames.rs; the test\n\
         voice-link/tests/emu_frames.rs checks that they decode to the tone)\n\
         \"\"\"\n"
    );
    println!("TONE_HZ = {TONE_HZ:.0}\n");
    println!("# mSBC: 57-byte frames of 120 samples at 16 kHz");
    print_list("MSBC_FRAMES", &msbc());
    println!("\n# LC3-SWB: 58-byte frames of 240 samples (7.5 ms) at 32 kHz");
    print_list("LC3_SWB_FRAMES", &lc3_swb());
}

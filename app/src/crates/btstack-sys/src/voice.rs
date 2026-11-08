//! HFP voice codec primitives for the headset microphone (csrc/voice_codecs.c):
//! BTstack's Bluedroid mSBC encoder, the Bluedroid SBC decoder in mSBC mode
//! (H2 synchronisation and BTstack's SBC packet loss concealment included),
//! and BTstack's CVSD packet loss concealment.
//!
//! These are plain codecs: they do not touch the BTstack run loop or HCI, so
//! they work on any thread without a [`crate::Stack`]. Each value is `Send`
//! (its state is its own heap block) but not `Sync`.

use std::ffi::{c_int, c_void};
use std::ptr::NonNull;

/// PCM samples (16 kHz, mono) of one mSBC frame
pub const MSBC_SAMPLES: usize = 120;
/// Bytes of one mSBC frame, without the H2 header and the padding byte
pub const MSBC_FRAME_LEN: usize = 57;
/// Largest number of samples one CVSD concealment call takes (BTstack's CVSD_FS)
pub const CVSD_PLC_MAX_SAMPLES: usize = 60;

/// What the C side holds at most between two takes (csrc VOICE_MSBC_PCM_CAP)
const MSBC_PCM_CAP: usize = 16 * MSBC_SAMPLES;
/// Payload bytes fed per decode call: at most 4 frames plus the concealed
/// ones from earlier bad bytes, far below [`MSBC_PCM_CAP`]
const MSBC_FEED_CHUNK: usize = 240;

unsafe extern "C" {
    fn bluecaster_msbc_encoder_new() -> *mut c_void;
    fn bluecaster_msbc_encode(encoder: *mut c_void, pcm: *const i16, out: *mut u8) -> c_int;
    fn bluecaster_msbc_encoder_free(encoder: *mut c_void);

    fn bluecaster_msbc_decoder_new() -> *mut c_void;
    fn bluecaster_msbc_decode(decoder: *mut c_void, packet_status_flag: u8, data: *const u8, len: u16) -> c_int;
    fn bluecaster_msbc_decoder_take(decoder: *mut c_void, out: *mut i16, cap: u32) -> c_int;
    fn bluecaster_msbc_decoder_stats(
        decoder: *mut c_void,
        good: *mut u32,
        bad: *mut u32,
        zero: *mut u32,
        overflow: *mut u32,
    );
    fn bluecaster_msbc_decoder_free(decoder: *mut c_void);

    fn bluecaster_cvsd_plc_new() -> *mut c_void;
    fn bluecaster_cvsd_plc_process(
        plc: *mut c_void,
        bad_frame: c_int,
        input: *mut i16,
        num_samples: u16,
        out: *mut i16,
    );
    fn bluecaster_cvsd_plc_stats(plc: *mut c_void, good: *mut u32, bad: *mut u32);
    fn bluecaster_cvsd_plc_free(plc: *mut c_void);
}

/// The Bluedroid mSBC encoder: 120 samples (16 kHz mono) in, one 57-byte
/// frame out (syncword 0xAD, no H2 header)
pub struct MsbcEncoder(NonNull<c_void>);

// SAFETY: the encoder state is a heap block owned by this value; the C code
// keeps no global state for mSBC (the Bluedroid globals are joint stereo
// only), so it may move to another thread
unsafe impl Send for MsbcEncoder {}

impl MsbcEncoder {
    pub fn new() -> MsbcEncoder {
        // SAFETY: no arguments; returns an owned heap state or NULL
        let p = unsafe { bluecaster_msbc_encoder_new() };
        MsbcEncoder(NonNull::new(p).expect("mSBC encoder allocation failed"))
    }

    /// Encodes one frame
    pub fn encode(&mut self, pcm: &[i16; MSBC_SAMPLES]) -> [u8; MSBC_FRAME_LEN] {
        let mut out = [0u8; MSBC_FRAME_LEN];
        // SAFETY: self.0 is a live encoder; the encoder reads 120 samples
        // (mSBC: 15 blocks x 8 subbands, mono) from `pcm`, which has them,
        // and the wrapper copies 57 bytes to `out`, which has room for them
        let n = unsafe { bluecaster_msbc_encode(self.0.as_ptr(), pcm.as_ptr(), out.as_mut_ptr()) };
        assert_eq!(n, MSBC_FRAME_LEN as c_int, "mSBC frame length");
        out
    }
}

impl Default for MsbcEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MsbcEncoder {
    fn drop(&mut self) {
        // SAFETY: allocated by bluecaster_msbc_encoder_new, freed once
        unsafe { bluecaster_msbc_encoder_free(self.0.as_ptr()) };
    }
}

impl std::fmt::Debug for MsbcEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MsbcEncoder")
    }
}

/// Frame counters of an [`MsbcDecoder`]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsbcStats {
    /// Frames decoded
    pub good: u32,
    /// Frames concealed: bad packets, missing bytes, frames that failed to decode
    pub bad: u32,
    /// Frames concealed because they were all zero (a controller's filler)
    pub zero: u32,
    /// Samples dropped because they were not taken in time (should stay 0)
    pub overflow: u32,
}

/// The Bluedroid SBC decoder in mSBC mode, fed with SCO payload bytes of
/// any length. It finds the H2 headers itself, decodes each 60-byte H2
/// frame (2-byte header, 57-byte frame, padding byte) and conceals bad or
/// missing ones (after the first good frame) with BTstack's SBC PLC.
pub struct MsbcDecoder(NonNull<c_void>);

// SAFETY: as MsbcEncoder: an owned heap block, no global state
unsafe impl Send for MsbcDecoder {}

impl MsbcDecoder {
    pub fn new() -> MsbcDecoder {
        // SAFETY: no arguments; returns an owned heap state or NULL
        let p = unsafe { bluecaster_msbc_decoder_new() };
        MsbcDecoder(NonNull::new(p).expect("mSBC decoder allocation failed"))
    }

    /// Feeds `payload` with the HCI SCO packet status flag (0 ok, 1 possibly
    /// invalid, 2 no data, 3 partially lost; anything but 0 marks the frame
    /// bad). Every frame of 120 samples decoded or concealed from it goes to
    /// `out`, in order (possibly none, possibly several).
    pub fn decode(&mut self, status: u8, payload: &[u8], mut out: impl FnMut(&[i16])) {
        let mut pcm = [0i16; MSBC_PCM_CAP];
        for chunk in payload.chunks(MSBC_FEED_CHUNK) {
            // SAFETY: live decoder; `chunk` is chunk.len() (<= 240) readable
            // bytes, which the decoder copies into its own frame buffer
            unsafe { bluecaster_msbc_decode(self.0.as_ptr(), status, chunk.as_ptr(), chunk.len() as u16) };
            loop {
                // SAFETY: live decoder; `pcm` is writable for its length
                let n = unsafe { bluecaster_msbc_decoder_take(self.0.as_ptr(), pcm.as_mut_ptr(), pcm.len() as u32) };
                if n <= 0 {
                    break;
                }
                out(&pcm[..n as usize]);
            }
        }
    }

    pub fn stats(&self) -> MsbcStats {
        let mut s = MsbcStats::default();
        // SAFETY: live decoder; four valid u32 out pointers
        unsafe {
            bluecaster_msbc_decoder_stats(self.0.as_ptr(), &mut s.good, &mut s.bad, &mut s.zero, &mut s.overflow)
        };
        s
    }
}

impl Default for MsbcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MsbcDecoder {
    fn drop(&mut self) {
        // SAFETY: allocated by bluecaster_msbc_decoder_new, freed once
        unsafe { bluecaster_msbc_decoder_free(self.0.as_ptr()) };
    }
}

impl std::fmt::Debug for MsbcDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MsbcDecoder").field(&self.stats()).finish()
    }
}

/// BTstack's CVSD packet loss concealment (8 kHz 16-bit PCM). Besides the
/// frames marked bad it conceals frames its own check finds broken.
pub struct CvsdPlc(NonNull<c_void>);

// SAFETY: as MsbcEncoder: an owned heap block, no global state
unsafe impl Send for CvsdPlc {}

impl CvsdPlc {
    pub fn new() -> CvsdPlc {
        // SAFETY: no arguments; returns an owned heap state or NULL
        let p = unsafe { bluecaster_cvsd_plc_new() };
        CvsdPlc(NonNull::new(p).expect("CVSD PLC allocation failed"))
    }

    /// Passes `input` to `out` (same length), concealing it when `bad`.
    /// Works in pieces of [`CVSD_PLC_MAX_SAMPLES`]; for the concealment to
    /// match the packets, give it one SCO packet at a time.
    pub fn process(&mut self, bad: bool, input: &[i16], out: &mut [i16]) {
        assert_eq!(input.len(), out.len(), "CVSD PLC: input and output lengths differ");
        let mut scratch = [0i16; CVSD_PLC_MAX_SAMPLES];
        for (i, o) in input.chunks(CVSD_PLC_MAX_SAMPLES).zip(out.chunks_mut(CVSD_PLC_MAX_SAMPLES)) {
            // The C side takes a mutable input pointer: a copy, so `input`
            // stays untouched whatever it does
            scratch[..i.len()].copy_from_slice(i);
            // SAFETY: live state; `scratch` holds i.len() (<= 60, BTstack's
            // CVSD_FS) samples, `o` has room for as many and does not overlap
            unsafe {
                bluecaster_cvsd_plc_process(
                    self.0.as_ptr(),
                    c_int::from(bad),
                    scratch.as_mut_ptr(),
                    i.len() as u16,
                    o.as_mut_ptr(),
                )
            };
        }
    }

    /// Frames passed as good and frames concealed
    pub fn stats(&self) -> (u32, u32) {
        let (mut good, mut bad) = (0, 0);
        // SAFETY: live state; two valid u32 out pointers
        unsafe { bluecaster_cvsd_plc_stats(self.0.as_ptr(), &mut good, &mut bad) };
        (good, bad)
    }
}

impl Default for CvsdPlc {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CvsdPlc {
    fn drop(&mut self) {
        // SAFETY: allocated by bluecaster_cvsd_plc_new, freed once
        unsafe { bluecaster_cvsd_plc_free(self.0.as_ptr()) };
    }
}

impl std::fmt::Debug for CvsdPlc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CvsdPlc").field(&self.stats()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H2: [[u8; 2]; 4] = [[0x01, 0x08], [0x01, 0x38], [0x01, 0xC8], [0x01, 0xF8]];

    fn sine(n: usize, rate: f64, hz: f64, amp: f64) -> Vec<i16> {
        (0..n).map(|i| (amp * (2.0 * std::f64::consts::PI * hz * i as f64 / rate).sin()) as i16).collect()
    }

    /// H2-framed mSBC of `pcm` (whole frames), as the SCO link carries it
    fn msbc_stream(pcm: &[i16]) -> Vec<Vec<u8>> {
        let mut enc = MsbcEncoder::new();
        pcm.as_chunks::<MSBC_SAMPLES>()
            .0
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let frame = enc.encode(f);
                let mut p = H2[i % 4].to_vec();
                p.extend_from_slice(&frame);
                p.push(0);
                p
            })
            .collect()
    }

    fn best_delay_snr(reference: &[i16], decoded: &[i16]) -> (usize, f64) {
        // The codec delays the signal (analysis + synthesis filter banks);
        // find the delay, then compare
        let skip = 480;
        let len = 1600;
        let mut best = (0, f64::MIN);
        for d in 0..200 {
            let (mut sig, mut noise) = (0.0, 0.0);
            for i in skip..skip + len {
                let r = f64::from(reference[i]);
                let e = f64::from(decoded[i + d]) - r;
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
    fn msbc_round_trip_of_a_sine() {
        let pcm = sine(MSBC_SAMPLES * 40, 16_000.0, 1000.0, 8000.0);
        let mut dec = MsbcDecoder::new();
        let mut out = Vec::new();
        for p in msbc_stream(&pcm) {
            assert_eq!(p.len(), 60);
            assert_eq!(p[2], 0xAD, "mSBC syncword");
            dec.decode(0, &p, |s| out.extend_from_slice(s));
        }
        assert_eq!(out.len(), pcm.len());
        let (delay, snr) = best_delay_snr(&pcm, &out);
        assert!(snr > 20.0, "SNR {snr:.1} dB at delay {delay}");
        let s = dec.stats();
        assert_eq!((s.good, s.bad, s.zero, s.overflow), (40, 0, 0, 0));
    }

    #[test]
    fn msbc_decoder_takes_any_packet_length() {
        let pcm = sine(MSBC_SAMPLES * 30, 16_000.0, 440.0, 6000.0);
        let bytes: Vec<u8> = msbc_stream(&pcm).concat();
        let mut dec = MsbcDecoder::new();
        let mut n = 0;
        for p in bytes.chunks(24) {
            dec.decode(0, p, |s| n += s.len());
        }
        assert_eq!(n, pcm.len());
        assert_eq!(dec.stats().good, 30);
    }

    #[test]
    fn msbc_plc_conceals_a_dropped_frame() {
        let pcm = sine(MSBC_SAMPLES * 20, 16_000.0, 500.0, 8000.0);
        let mut packets = msbc_stream(&pcm);
        // Frame 10 lost on the air: the controller reports "no data"
        packets[10] = vec![0; 60];
        let mut dec = MsbcDecoder::new();
        let mut out = Vec::new();
        for (i, p) in packets.iter().enumerate() {
            dec.decode(if i == 10 { 2 } else { 0 }, p, |s| out.extend_from_slice(s));
        }
        // One frame per packet: the concealed one included
        assert_eq!(out.len(), pcm.len());
        let s = dec.stats();
        assert_eq!(s.good, 19, "{s:?}");
        assert_eq!(s.bad + s.zero, 1, "{s:?}");
        // The concealed frame is not silence
        let concealed = &out[10 * MSBC_SAMPLES..11 * MSBC_SAMPLES];
        assert!(concealed.iter().any(|&x| x.unsigned_abs() > 1000));
    }

    #[test]
    fn msbc_decoder_finds_sync_after_garbage() {
        let pcm = sine(MSBC_SAMPLES * 10, 16_000.0, 700.0, 8000.0);
        let mut bytes = vec![0x55u8, 0x01, 0x02, 0xAD, 0x33, 0x99, 0x10];
        bytes.extend(msbc_stream(&pcm).concat());
        let mut dec = MsbcDecoder::new();
        let mut n = 0;
        dec.decode(0, &bytes, |s| n += s.len());
        // Nothing is concealed before the first good frame
        assert_eq!(n, pcm.len());
        assert_eq!(dec.stats().good, 10);
    }

    #[test]
    fn cvsd_plc_smoke() {
        let pcm = sine(60 * 20, 8000.0, 300.0, 8000.0);
        let mut plc = CvsdPlc::new();
        let mut out = vec![0i16; pcm.len()];
        for (i, (inp, o)) in pcm.chunks(60).zip(out.chunks_mut(60)).enumerate() {
            plc.process(i == 15, inp, o);
        }
        // Good frames pass through unchanged (no concealment before them)
        assert_eq!(&out[..60 * 15], &pcm[..60 * 15]);
        let (good, bad) = plc.stats();
        assert_eq!((good, bad), (19, 1));
        // The concealed frame continues the waveform rather than going silent
        assert!(out[60 * 15..60 * 16].iter().any(|&x| x.unsigned_abs() > 1000));
    }

    #[test]
    fn codecs_move_between_threads() {
        fn send<T: Send>() {}
        send::<MsbcEncoder>();
        send::<MsbcDecoder>();
        send::<CvsdPlc>();
    }
}

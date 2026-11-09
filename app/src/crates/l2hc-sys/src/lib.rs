//! Huawei L2HC encoder: the clean-room C99 implementation of
//! L2HC-native-linux (MIT), vendored in `csrc/` and built by `build.rs`.
//!
//! [`ffi`] declares the C ABI (`l2hc_native_*` plus the `AudioL2hcEnc*`
//! exports the original libl2hc.so has). [`L2hcEnc`] owns the context,
//! validates the parameters and the buffer lengths, and frees it on drop, so
//! callers need no `unsafe`.

use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;

/// Declarations from csrc/l2hc_native.h
#[allow(non_camel_case_types)]
pub mod ffi {
    use std::ffi::c_void;

    /// `L2hcEncParam` (packed in C; on x86-64 / ARM64 the natural layout is
    /// the same: one u32 followed by four i16)
    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct L2hcEncParam {
        /// 44100 / 48000 / 88200 / 96000
        pub sample_rate: u32,
        /// 16 / 24 / 32 bits per sample
        pub bps: i16,
        /// 1 (mono) or 2 (stereo)
        pub channels: i16,
        /// 480 (10 ms @ 48 kHz) or 960 (10 ms @ 96 kHz)
        pub frame_samples: i16,
        /// 320 / 480 / 640 / 960 kbps
        pub bitrate_kbps: i16,
    }

    /// `L2hcEncIo` (8-byte aligned in C; the natural repr(C) layout matches)
    #[repr(C)]
    pub struct L2hcEncIo {
        pub in_buffer: *const u8,
        pub in_bytes: u32,
        pub reserved: u32,
        pub out_buffer: *mut u8,
        pub out_bytes: u32,
    }

    unsafe extern "C" {
        pub fn AudioL2hcEncGetSize(size: *mut u32) -> i32;
        pub fn AudioL2hcEncInit(encoder: *mut *mut c_void, buffer: *mut c_void, size: u32) -> i32;
        pub fn AudioL2hcEncSetParam(encoder: *mut c_void, param: *const L2hcEncParam) -> i32;
        pub fn AudioL2hcEncApply(encoder: *mut c_void, io: *mut L2hcEncIo) -> i32;

        pub fn l2hc_native_create() -> *mut c_void;
        pub fn l2hc_native_set_param(enc: *mut c_void, param: *const L2hcEncParam) -> i32;
        pub fn l2hc_native_encode(
            enc: *mut c_void,
            in_pcm: *const u8,
            in_bytes: u32,
            out_frame: *mut u8,
            out_bytes: *mut u32,
        ) -> i32;
        pub fn l2hc_native_destroy(enc: *mut c_void);
    }
}

pub use ffi::L2hcEncParam;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The encoder context could not be allocated
    Alloc,
    /// A parameter the C encoder cannot handle
    Unsupported(&'static str),
    /// A nonzero return from set_param / encode
    L2hc(i32),
    InputLength {
        expected: usize,
        got: usize,
    },
    OutputTooSmall {
        have: usize,
        need: usize,
    },
    UnexpectedFrameBytes {
        expected: usize,
        got: usize,
    },
}

// (Display is written out below rather than pulled in from thiserror: this
// crate has no dependencies beyond the C library.)
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Alloc => f.write_str("L2HC encoder: out of memory"),
            Error::Unsupported(what) => write!(f, "L2HC encoder: unsupported {what}"),
            Error::L2hc(code) => write!(f, "L2HC encoder: failure ({code})"),
            Error::InputLength { expected, got } => {
                write!(f, "L2HC encoder: expected {expected} PCM bytes, got {got}")
            }
            Error::OutputTooSmall { have, need } => {
                write!(f, "L2HC encoder: output buffer too small ({have} < {need} bytes)")
            }
            Error::UnexpectedFrameBytes { expected, got } => {
                write!(f, "L2HC encoder: wrote {got} bytes, expected {expected}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Bytes the encoder writes for one frame at `param`: the bit rate over the
/// frame duration (`kbps * frame_ms / 8`), exactly as the C encoder computes
/// its target frame size.
pub fn expected_frame_bytes(param: &L2hcEncParam) -> Result<usize, Error> {
    if param.sample_rate == 0 || param.frame_samples <= 0 || param.bitrate_kbps <= 0 {
        return Err(Error::Unsupported("sample rate / frame size / bit rate"));
    }
    // All three values are positive and range-checked by set_param.
    let frame_ms = (param.frame_samples as u32 * 1000 + param.sample_rate / 2) / param.sample_rate;
    Ok(param.bitrate_kbps as usize * frame_ms.max(5) as usize / 8)
}

/// An L2HC encoder context, freed on drop
pub struct L2hcEnc {
    handle: NonNull<c_void>,
    param: L2hcEncParam,
    frame_bytes: usize,
    pcm_bytes: usize,
}

// SAFETY: the context is plain heap memory owned by this value; the MDCT
// tables it initializes are global but idempotent, and every mutating call
// takes &mut self. No thread affinity.
unsafe impl Send for L2hcEnc {}

impl L2hcEnc {
    /// Creates the encoder and applies `param`
    pub fn new(param: L2hcEncParam) -> Result<L2hcEnc, Error> {
        // SAFETY: no arguments, returns an owned context or NULL
        let raw = unsafe { ffi::l2hc_native_create() };
        let handle = NonNull::new(raw).ok_or(Error::Alloc)?;
        let mut enc = L2hcEnc { handle, param, frame_bytes: 0, pcm_bytes: 0 };
        // On an invalid parameter the encoder is dropped, which frees the
        // context through Drop.
        enc.set_param(param)?;
        Ok(enc)
    }

    /// Changes the stream parameters (also used by ABR to change the bit
    /// rate without losing the overlap-add history).
    pub fn set_param(&mut self, param: L2hcEncParam) -> Result<(), Error> {
        if !matches!(param.bps, 16 | 24 | 32) {
            return Err(Error::Unsupported("bit depth (use 16, 24 or 32)"));
        }
        if !matches!(param.channels, 1 | 2) {
            return Err(Error::Unsupported("channel count (use 1 or 2)"));
        }
        if param.frame_samples <= 0 || param.frame_samples > 960 {
            return Err(Error::Unsupported("frame size (at most 960 samples)"));
        }
        let frame_bytes = expected_frame_bytes(&param)?;
        // SAFETY: the context is valid; the C encoder only reads the plain
        // parameter struct and checks the magic number.
        let ret = unsafe { ffi::l2hc_native_set_param(self.handle.as_ptr(), &param) };
        if ret != 0 {
            return Err(Error::L2hc(ret));
        }
        self.param = param;
        self.frame_bytes = frame_bytes;
        self.pcm_bytes = param.frame_samples as usize * param.channels as usize * (param.bps as usize / 8);
        Ok(())
    }

    /// Encodes one frame of interleaved, packed little-endian PCM into
    /// `out`, and returns the exact frame length in bytes.
    pub fn encode(&mut self, pcm: &[u8], out: &mut [u8]) -> Result<usize, Error> {
        if pcm.len() < self.pcm_bytes {
            return Err(Error::InputLength { expected: self.pcm_bytes, got: pcm.len() });
        }
        if out.len() < self.frame_bytes {
            return Err(Error::OutputTooSmall { have: out.len(), need: self.frame_bytes });
        }
        let mut written: u32 = 0;
        // SAFETY: the context is valid; both buffers have been checked to be
        // large enough above, and `written` is a local out parameter.
        let ret = unsafe {
            ffi::l2hc_native_encode(
                self.handle.as_ptr(),
                pcm.as_ptr(),
                self.pcm_bytes as u32,
                out.as_mut_ptr(),
                &mut written,
            )
        };
        if ret != 0 {
            return Err(Error::L2hc(ret));
        }
        let written = written as usize;
        if written != self.frame_bytes {
            return Err(Error::UnexpectedFrameBytes { expected: self.frame_bytes, got: written });
        }
        Ok(written)
    }

    pub fn param(&self) -> L2hcEncParam {
        self.param
    }

    /// Bytes of one encoded frame (400 / 600 / 800 / 1200)
    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// PCM bytes one encode call consumes
    pub fn pcm_bytes(&self) -> usize {
        self.pcm_bytes
    }

    /// ABR: change the bit rate in place
    pub fn set_bitrate_kbps(&mut self, kbps: i16) -> Result<(), Error> {
        let mut param = self.param;
        param.bitrate_kbps = kbps;
        self.set_param(param)
    }
}

impl Drop for L2hcEnc {
    fn drop(&mut self) {
        // SAFETY: the context came from l2hc_native_create and is owned by
        // self; nothing uses it after this
        unsafe { ffi::l2hc_native_destroy(self.handle.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(sample_rate: u32, bps: i16, frame_samples: i16, bitrate_kbps: i16) -> L2hcEncParam {
        L2hcEncParam { sample_rate, bps, channels: 2, frame_samples, bitrate_kbps }
    }

    /// A 440 Hz + 1.2 kHz tone, interleaved stereo, packed 24-bit LE
    fn tone(frame_samples: usize, seed: usize) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(frame_samples * 6);
        for i in 0..frame_samples {
            let t = (seed * frame_samples + i) as f64 / 48000.0;
            let l = (t * 2.0 * std::f64::consts::PI * 440.0).sin() * 0.5
                + (t * 2.0 * std::f64::consts::PI * 1200.0).sin() * 0.3;
            let r = (t * 2.0 * std::f64::consts::PI * 440.0).cos() * 0.5;
            for v in [l, r] {
                let s = (v * 8_388_600.0) as i32;
                pcm.extend_from_slice(&s.to_le_bytes()[..3]);
            }
        }
        pcm
    }

    #[test]
    fn encodes_fixed_size_frames() {
        let mut enc = L2hcEnc::new(param(48000, 24, 480, 320)).unwrap();
        assert_eq!(enc.frame_bytes(), 400);
        assert_eq!(enc.pcm_bytes(), 480 * 2 * 3);
        let mut out = vec![0u8; 400];
        for f in 0..50 {
            let pcm = tone(480, f);
            assert_eq!(enc.encode(&pcm, &mut out).unwrap(), 400);
            assert_eq!(out[0], 0x5C, "sync byte of frame {f}");
        }
    }

    #[test]
    fn bitrate_sets_the_frame_length() {
        for (kbps, bytes) in [(320, 400), (480, 600), (640, 800), (960, 1200)] {
            let mut enc = L2hcEnc::new(param(48000, 16, 480, kbps)).unwrap();
            assert_eq!(enc.frame_bytes(), bytes, "{kbps} kbps");
            let pcm = vec![0u8; enc.pcm_bytes()];
            let mut out = vec![0u8; bytes];
            assert_eq!(enc.encode(&pcm, &mut out).unwrap(), bytes);
            assert_eq!(out[0], 0x5C);
        }
    }

    #[test]
    fn ninety_six_khz_uses_960_sample_frames() {
        let mut enc = L2hcEnc::new(param(96000, 24, 960, 960)).unwrap();
        assert_eq!(enc.frame_bytes(), 1200);
        assert_eq!(enc.pcm_bytes(), 960 * 2 * 3);
        let pcm = vec![0u8; enc.pcm_bytes()];
        let mut out = vec![0u8; 1200];
        assert_eq!(enc.encode(&pcm, &mut out).unwrap(), 1200);
        // (1<<5) | (sr_idx=3 << 3) | (stereo << 2) | 10 ms, then the
        // low-bitrate bit: 0x3F << 1 = 0x7E (0x5C is the 48 kHz header).
        assert_eq!(out[0], 0x7E);
    }

    #[test]
    fn changing_bitrate_keeps_the_context() {
        let mut enc = L2hcEnc::new(param(48000, 16, 480, 960)).unwrap();
        let pcm = vec![0u8; enc.pcm_bytes()];
        let mut out = vec![0u8; 1200];
        assert_eq!(enc.encode(&pcm, &mut out[..1200]).unwrap(), 1200);
        enc.set_bitrate_kbps(640).unwrap();
        assert_eq!(enc.frame_bytes(), 800);
        assert_eq!(enc.encode(&pcm, &mut out[..800]).unwrap(), 800);
        assert_eq!(out[0], 0x5C);
    }

    #[test]
    fn short_buffers_and_bad_params_are_rejected() {
        let mut enc = L2hcEnc::new(param(48000, 16, 480, 320)).unwrap();
        let mut out = vec![0u8; 400];
        assert!(matches!(enc.encode(&[0u8; 10], &mut out), Err(Error::InputLength { .. })));
        assert!(matches!(enc.encode(&vec![0u8; enc.pcm_bytes()], &mut out[..10]), Err(Error::OutputTooSmall { .. })));
        assert!(L2hcEnc::new(param(48000, 20, 480, 320)).is_err());
        assert!(L2hcEnc::new(param(48000, 16, 480, 0)).is_err());
    }
}

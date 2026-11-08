//! The microphone into a WAV file instead of a render endpoint (the
//! developer CLI's `--mic-output <path>.wav`, the emulator tests): mono
//! 16-bit PCM at the voice rate, as it comes off the voice link, with no
//! prefill or drift control.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::render::{PcmConsumer, PcmProducer, pcm_channel};

/// How often the writer thread drains the queue
const DRAIN_EVERY: Duration = Duration::from_millis(20);
/// The header's sizes are brought up to date about this often, so a file
/// of a process that was killed still plays
const HEADER_EVERY: u32 = 50;

/// Writes what its [`PcmProducer`] gets to a mono 16-bit WAV file
pub struct WavSink {
    path: PathBuf,
    rate: u32,
    producer: Option<PcmProducer>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<io::Result<u64>>>,
}

impl WavSink {
    /// Creates (truncates) `path` and starts the writer thread "voice-wav"
    pub fn create(path: &Path, rate: u32) -> io::Result<WavSink> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(&header(rate, 0))?;
        // A second of room: the thread drains every 20 ms
        let (producer, consumer) = pcm_channel(rate as usize);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("voice-wav".into())
            .spawn(move || write_loop(file, rate, consumer, &thread_stop))?;
        tracing::info!("voice link: microphone to {} ({rate} Hz)", path.display());
        Ok(WavSink { path: path.to_owned(), rate, producer: Some(producer), stop, thread: Some(thread) })
    }

    /// The producer side (once; later calls get None)
    pub fn producer(&mut self) -> Option<PcmProducer> {
        self.producer.take()
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(Ok(frames))) => tracing::info!(
                "voice link: {} written, {frames} samples ({:.1} s)",
                self.path.display(),
                frames as f64 / f64::from(self.rate)
            ),
            Some(Ok(Err(e))) => tracing::warn!("voice link: writing {} failed: {e}", self.path.display()),
            _ => tracing::warn!("voice link: the WAV writer of {} stopped unexpectedly", self.path.display()),
        }
    }
}

impl std::fmt::Debug for WavSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WavSink").field("path", &self.path).field("rate", &self.rate).finish()
    }
}

fn write_loop(mut file: BufWriter<File>, rate: u32, mut queue: PcmConsumer, stop: &AtomicBool) -> io::Result<u64> {
    let mut buf = vec![0i16; rate as usize];
    let mut bytes = Vec::with_capacity(buf.len() * 2);
    let mut frames = 0u64;
    let mut rounds = 0u32;
    loop {
        let last = stop.load(Ordering::Acquire);
        let n = queue.pop(&mut buf);
        if n > 0 {
            bytes.clear();
            bytes.extend(buf[..n].iter().flat_map(|s| s.to_le_bytes()));
            file.write_all(&bytes)?;
            frames += n as u64;
        }
        rounds += 1;
        if last || rounds.is_multiple_of(HEADER_EVERY) {
            update_header(&mut file, rate, frames)?;
        }
        if last {
            file.flush()?;
            return Ok(frames);
        }
        thread::sleep(DRAIN_EVERY);
    }
}

/// Rewrites the header for `frames` samples, then goes back to the end
fn update_header(file: &mut BufWriter<File>, rate: u32, frames: u64) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header(rate, frames))?;
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

/// The 44-byte RIFF header of mono 16-bit PCM
fn header(rate: u32, frames: u64) -> [u8; 44] {
    let data = (frames * 2).min(u64::from(u32::MAX - 36)) as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_playable_mono_wav() {
        let path = std::env::temp_dir().join(format!("voice-link-wav-{}.wav", std::process::id()));
        {
            let mut sink = WavSink::create(&path, 16_000).unwrap();
            let mut p = sink.producer().unwrap();
            assert!(sink.producer().is_none());
            let pcm: Vec<i16> = (0..1600).map(|i| i as i16).collect();
            assert_eq!(p.push(&pcm), 1600);
        }
        let data = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(data.len(), 44 + 3200);
        assert_eq!(&data[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()), 36 + 3200);
        assert_eq!(u32::from_le_bytes(data[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(data[40..44].try_into().unwrap()), 3200);
        assert_eq!(&data[44..48], &[0, 0, 1, 0]);
    }
}

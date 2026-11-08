//! HFP H2 framing for the wide band codecs (mSBC, LC3-SWB): every 60-byte
//! frame on the SCO link starts with a 2-byte synchronisation header,
//! `0x01` followed by one of four sequence bytes. Port of BTstack's
//! `hfp_h2_framing` (hfp_codec.c) and `hfp_h2_sync` (hfp.c).

/// Bytes of one H2 frame: header and codec frame (and mSBC's padding byte)
pub const FRAME_LEN: usize = 60;
/// Bytes of the H2 header
pub const HEADER_LEN: usize = 2;
/// The second header byte for sequence numbers 0..=3
pub const SEQUENCE: [u8; 4] = [0x08, 0x38, 0xC8, 0xF8];

/// Writes the H2 headers of outgoing frames
#[derive(Debug, Default, Clone)]
pub struct H2Framer {
    sequence: u8,
}

impl H2Framer {
    pub fn new() -> H2Framer {
        H2Framer::default()
    }

    /// The next header (sequence 0, 1, 2, 3, 0, ...)
    pub fn next_header(&mut self) -> [u8; HEADER_LEN] {
        let h = [0x01, SEQUENCE[usize::from(self.sequence)]];
        self.sequence = (self.sequence + 1) & 3;
        h
    }
}

/// Whether `b0 b1` is an H2 header (any sequence number)
pub fn is_header(b0: u8, b1: u8) -> bool {
    if b0 != 0x01 || b1 & 0x0F != 0x08 {
        return false;
    }
    // The sequence number is sent twice: bits 0+2 equal bits 1+3 of the
    // upper nibble
    let hn = b1 >> 4;
    (hn >> 1) & 0x05 == hn & 0x05
}

/// The sequence number (0..=3) of an H2 header's second byte
pub fn sequence_of(b1: u8) -> u8 {
    let hn = b1 >> 4;
    ((hn & 0x04) >> 1) | (hn & 0x01)
}

fn find_header(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|w| is_header(w[0], w[1]))
}

/// What [`H2Sync::push`] hands on
#[derive(Debug, PartialEq, Eq)]
pub enum H2Frame<'a> {
    /// A whole frame starting with an H2 header (all [`FRAME_LEN`] bytes)
    Good(&'a [u8]),
    /// One frame's worth of bytes was lost or unusable: conceal a frame
    Bad,
}

/// Reassembles H2 frames from SCO payloads of any length. Bytes that are
/// marked bad or do not belong to a frame are counted, and every
/// [`FRAME_LEN`] of them comes out as one [`H2Frame::Bad`], so the output
/// keeps the timing of the input.
#[derive(Debug, Clone)]
pub struct H2Sync {
    frame: [u8; FRAME_LEN],
    len: usize,
    dropped: usize,
}

impl Default for H2Sync {
    fn default() -> Self {
        H2Sync { frame: [0; FRAME_LEN], len: 0, dropped: 0 }
    }
}

impl H2Sync {
    pub fn new() -> H2Sync {
        H2Sync::default()
    }

    /// Feeds one payload; `bad` (the packet status flag was not 0) drops it
    /// and whatever was collected. Frames go to `out` in order.
    pub fn push(&mut self, bad: bool, mut data: &[u8], mut out: impl FnMut(H2Frame<'_>)) {
        if bad {
            self.dropped += self.len + data.len();
            self.len = 0;
            self.report_bad(&mut out);
            return;
        }
        while !data.is_empty() {
            let n = data.len().min(FRAME_LEN - self.len);
            self.frame[self.len..self.len + n].copy_from_slice(&data[..n]);
            self.len += n;
            data = &data[n..];
            if self.len < FRAME_LEN {
                break;
            }
            match find_header(&self.frame) {
                Some(0) => {
                    out(H2Frame::Good(&self.frame));
                    self.len = 0;
                }
                Some(pos) => self.drop_front(pos, &mut out),
                // No header: keep a last 0x01, it may start the next one
                None if self.frame[FRAME_LEN - 1] == 0x01 => self.drop_front(FRAME_LEN - 1, &mut out),
                None => self.drop_front(FRAME_LEN, &mut out),
            }
        }
    }

    fn drop_front(&mut self, n: usize, out: &mut impl FnMut(H2Frame<'_>)) {
        self.frame.copy_within(n..self.len, 0);
        self.len -= n;
        self.dropped += n;
        self.report_bad(out);
    }

    fn report_bad(&mut self, out: &mut impl FnMut(H2Frame<'_>)) {
        while self.dropped >= FRAME_LEN {
            self.dropped -= FRAME_LEN;
            out(H2Frame::Bad);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize) -> Vec<u8> {
        let mut f = H2Framer::new();
        let mut v = Vec::new();
        for i in 0..n {
            v.extend_from_slice(&f.next_header());
            // Payload that never looks like a header
            v.extend((0..FRAME_LEN - 2).map(|j| (0x40 + ((i + j) % 0x30)) as u8));
        }
        v
    }

    fn collect(sync: &mut H2Sync, bad: bool, data: &[u8]) -> Vec<Option<Vec<u8>>> {
        let mut v = Vec::new();
        sync.push(bad, data, |f| {
            v.push(match f {
                H2Frame::Good(b) => Some(b.to_vec()),
                H2Frame::Bad => None,
            })
        });
        v
    }

    #[test]
    fn headers_and_sequence() {
        let mut f = H2Framer::new();
        let seq: Vec<u8> = (0..6).map(|_| f.next_header()[1]).collect();
        assert_eq!(seq, [0x08, 0x38, 0xC8, 0xF8, 0x08, 0x38]);
        for (i, &b) in SEQUENCE.iter().enumerate() {
            assert!(is_header(0x01, b));
            assert_eq!(sequence_of(b), i as u8);
        }
        assert!(!is_header(0x01, 0x18));
        assert!(!is_header(0x02, 0x08));
        assert!(!is_header(0x01, 0x09));
    }

    #[test]
    fn frames_in_odd_packet_lengths() {
        let data = frames(12);
        for len in [1, 24, 48, 59, 60, 61, 72, 120] {
            let mut sync = H2Sync::new();
            let mut got = Vec::new();
            for p in data.chunks(len) {
                got.extend(collect(&mut sync, false, p));
            }
            assert_eq!(got.len(), 12, "packet length {len}");
            for (i, f) in got.iter().enumerate() {
                assert_eq!(f.as_deref(), Some(&data[i * 60..(i + 1) * 60]), "len {len} frame {i}");
            }
        }
    }

    #[test]
    fn sync_recovers_after_a_garbage_prefix() {
        let mut data = vec![0x33, 0x01, 0x18, 0x99, 0x01];
        data.extend(vec![0x77; 100]);
        let garbage = data.len();
        data.extend(frames(5));
        let mut sync = H2Sync::new();
        let mut got = Vec::new();
        for p in data.chunks(48) {
            got.extend(collect(&mut sync, false, p));
        }
        let good: Vec<_> = got.iter().flatten().collect();
        assert_eq!(good.len(), 5);
        assert_eq!(good[0][..2], [0x01, 0x08]);
        // 105 garbage bytes: one whole frame's worth reported bad
        assert_eq!(got.iter().filter(|f| f.is_none()).count(), garbage / FRAME_LEN);
        assert!(got[..garbage / FRAME_LEN].iter().all(|f| f.is_none()));
    }

    #[test]
    fn bad_packets_become_bad_frames() {
        let data = frames(6);
        let mut sync = H2Sync::new();
        let mut got = Vec::new();
        for (i, p) in data.chunks(30).enumerate() {
            // Packets 4 and 5 (frame 2) lost
            got.extend(collect(&mut sync, i == 4 || i == 5, p));
        }
        assert_eq!(got.len(), 6);
        assert!(got[2].is_none());
        assert_eq!(got.iter().filter(|f| f.is_some()).count(), 5);
    }
}

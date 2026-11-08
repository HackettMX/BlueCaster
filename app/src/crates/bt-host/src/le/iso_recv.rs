//! Receiving ISO SDUs on the BTstack thread: the source ASEs of a
//! conversational stream (the headset microphone).
//!
//! BTstack hands complete (reassembled) HCI ISO data packets to the ISO
//! handler with packet type HCI_ISO_DATA_PACKET: handle (12 bits), PB flag
//! (bits 12-13), TS flag (bit 14), ISO_Data_Load length (14 bits); then the
//! time stamp (4 octets, when TS is set), the packet sequence number, the
//! ISO_SDU_Length (12 bits) with the packet status flag (bits 14-15: 0
//! valid, 1 possibly invalid, 2 lost) and the SDU. [`IsoRx`] parses them and
//! moves `(stream, sequence, status, SDU)` into a lock-free ring
//! ([`RxSdu`], fixed size, no allocation per packet); the consumer end
//! ([`LeRxHandle`]) goes to the decode thread (pipeline's `le_rx`). A lost
//! SDU often produces no packet at all: the decoder conceals by sequence
//! numbers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use btstack_sys::consts::HCI_ISO_DATA_PACKET;

/// The longest SDU the ring carries (HCI_ISO_PAYLOAD_SIZE of BTstack)
pub const MAX_RX_SDU: usize = 400;

/// SDUs the ring holds (64 x 10 ms: far more than the decode thread ever
/// lags behind)
pub const RX_RING_SDUS: usize = 64;

/// Packet status flag: the SDU is valid
pub const ISO_STATUS_VALID: u8 = 0;
/// Packet status flag: possibly invalid (errors may be in it)
pub const ISO_STATUS_POSSIBLY_INVALID: u8 = 1;
/// Packet status flag: lost data (no SDU)
pub const ISO_STATUS_LOST: u8 = 2;

/// One HCI ISO data packet as BTstack delivers it (complete SDU)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoData<'a> {
    pub handle: u16,
    pub time_stamp: Option<u32>,
    pub sequence: u16,
    /// Packet status flag (0 valid, 1 possibly invalid, 2 lost)
    pub status: u8,
    pub sdu: &'a [u8],
}

/// Parses an HCI ISO data packet (header included); None when it is
/// malformed or a continuation fragment (BTstack passes only complete SDUs)
pub fn parse_iso_data(packet: &[u8]) -> Option<IsoData<'_>> {
    let w = u16::from_le_bytes([*packet.first()?, *packet.get(1)?]);
    let handle = w & 0x0FFF;
    let pb = (w >> 12) & 0x3;
    let ts = (w >> 14) & 0x1 != 0;
    // 0b00 first fragment (BTstack's reassembled SDU keeps that header),
    // 0b10 complete SDU
    if pb != 0b00 && pb != 0b10 {
        return None;
    }
    let load_len = usize::from(u16::from_le_bytes([*packet.get(2)?, *packet.get(3)?]) & 0x3FFF);
    let load = packet.get(4..4 + load_len)?;
    let (time_stamp, rest) = if ts {
        let t = load.get(..4)?;
        (Some(u32::from_le_bytes([t[0], t[1], t[2], t[3]])), &load[4..])
    } else {
        (None, load)
    };
    let h = rest.get(..4)?;
    let sequence = u16::from_le_bytes([h[0], h[1]]);
    let w2 = u16::from_le_bytes([h[2], h[3]]);
    let sdu_len = usize::from(w2 & 0x0FFF);
    let status = (w2 >> 14) as u8;
    let sdu = rest.get(4..4 + sdu_len)?;
    Some(IsoData { handle, time_stamp, sequence, status, sdu })
}

/// One received SDU in the ring (fixed size: moving it allocates nothing)
#[derive(Clone, Copy)]
pub struct RxSdu {
    /// Index of the source stream ([`LeRxHandle::streams`])
    pub stream: u8,
    pub sequence: u16,
    /// Packet status flag
    pub status: u8,
    pub len: u16,
    pub data: [u8; MAX_RX_SDU],
}

impl RxSdu {
    pub fn sdu(&self) -> &[u8] {
        &self.data[..usize::from(self.len).min(MAX_RX_SDU)]
    }
}

impl Default for RxSdu {
    fn default() -> Self {
        RxSdu { stream: 0, sequence: 0, status: 0, len: 0, data: [0; MAX_RX_SDU] }
    }
}

impl std::fmt::Debug for RxSdu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RxSdu")
            .field("stream", &self.stream)
            .field("sequence", &self.sequence)
            .field("status", &self.status)
            .field("len", &self.len)
            .finish()
    }
}

/// Counters of the receiving side (BTstack thread)
#[derive(Debug, Default)]
pub struct LeRxStats {
    /// ISO data packets of the stream's source CIS
    pub packets: AtomicU64,
    /// Packets with status "possibly invalid" or "lost"
    pub bad_status: AtomicU64,
    /// Dropped: the ring was full (the decode thread did not keep up)
    pub ring_full: AtomicU64,
    /// Malformed, too long, or of a CIS without a source direction
    pub ignored: AtomicU64,
}

/// One source direction of the stream (a source CIS, plan order)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeRxStream {
    pub device_id: u32,
    pub cis_id: u8,
    pub ase_id: u8,
    /// LC3 channels in the SDU
    pub channels: u8,
    /// Octets per frame and channel
    pub octets: u16,
}

/// The decode thread's end of the received SDUs
/// ([`crate::LeStreamStarted::rx`])
pub struct LeRxHandle {
    pub consumer: rtrb::Consumer<RxSdu>,
    /// Source streams in plan order (RxSdu::stream indexes them)
    pub streams: Vec<LeRxStream>,
    /// The source preset's LC3 parameters
    pub sample_rate: u32,
    pub frame_us: u32,
    /// The source preset's name, e.g. "32_2_1"
    pub preset: String,
    pub stats: Arc<LeRxStats>,
}

impl std::fmt::Debug for LeRxHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeRxHandle")
            .field("streams", &self.streams)
            .field("sample_rate", &self.sample_rate)
            .field("frame_us", &self.frame_us)
            .field("preset", &self.preset)
            .finish()
    }
}

/// The BTstack thread's end: parses ISO data and fills the ring
pub(crate) struct IsoRx {
    producer: rtrb::Producer<RxSdu>,
    /// CIS handle per stream index
    handles: Vec<u16>,
    stats: Arc<LeRxStats>,
    logged_full: bool,
}

impl IsoRx {
    pub(crate) fn new(producer: rtrb::Producer<RxSdu>, handles: &[u16], stats: Arc<LeRxStats>) -> IsoRx {
        tracing::info!("LE Audio: receiving the microphone on CIS {handles:04X?}");
        IsoRx { producer, handles: handles.to_vec(), stats, logged_full: false }
    }

    /// An HCI ISO data packet (true when it belonged to a source stream)
    pub(crate) fn on_packet(&mut self, packet: &[u8]) -> bool {
        let Some(d) = parse_iso_data(packet) else {
            self.stats.ignored.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let Some(index) = self.handles.iter().position(|&h| h == d.handle) else {
            self.stats.ignored.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if d.sdu.len() > MAX_RX_SDU {
            self.stats.ignored.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.stats.packets.fetch_add(1, Ordering::Relaxed);
        if d.status != ISO_STATUS_VALID {
            self.stats.bad_status.fetch_add(1, Ordering::Relaxed);
        }
        let mut sdu = RxSdu {
            stream: index as u8,
            sequence: d.sequence,
            status: d.status,
            len: d.sdu.len() as u16,
            data: [0; MAX_RX_SDU],
        };
        sdu.data[..d.sdu.len()].copy_from_slice(d.sdu);
        if self.producer.push(sdu).is_err() {
            self.stats.ring_full.fetch_add(1, Ordering::Relaxed);
            if !std::mem::replace(&mut self.logged_full, true) {
                tracing::warn!("LE Audio: microphone SDUs dropped: the decoder does not keep up");
            }
        }
        true
    }
}

impl Drop for IsoRx {
    fn drop(&mut self) {
        tracing::info!(
            "LE Audio: stopped receiving: {} SDU(s), {} with a bad status, {} dropped, {} ignored",
            self.stats.packets.load(Ordering::Relaxed),
            self.stats.bad_status.load(Ordering::Relaxed),
            self.stats.ring_full.load(Ordering::Relaxed),
            self.stats.ignored.load(Ordering::Relaxed)
        );
    }
}

/// Is this an HCI ISO data packet (and not an event)? The ISO handler gets
/// both; an event's first byte could look like a handle
pub(crate) fn is_iso_data(packet_type: u8) -> bool {
    packet_type == HCI_ISO_DATA_PACKET
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ISO data packet: handle, optional time stamp, sequence, status,
    /// SDU
    pub(crate) fn packet(handle: u16, ts: Option<u32>, seq: u16, status: u8, sdu: &[u8]) -> Vec<u8> {
        let mut load = Vec::new();
        if let Some(t) = ts {
            load.extend_from_slice(&t.to_le_bytes());
        }
        load.extend_from_slice(&seq.to_le_bytes());
        load.extend_from_slice(&((sdu.len() as u16) | (u16::from(status) << 14)).to_le_bytes());
        load.extend_from_slice(sdu);
        let w = handle | (0b10 << 12) | if ts.is_some() { 1 << 14 } else { 0 };
        let mut p = w.to_le_bytes().to_vec();
        p.extend_from_slice(&(load.len() as u16).to_le_bytes());
        p.extend_from_slice(&load);
        p
    }

    #[test]
    fn parses_iso_data_packets() {
        let p = packet(0x061, None, 7, 0, &[1, 2, 3]);
        assert_eq!(
            parse_iso_data(&p),
            Some(IsoData { handle: 0x61, time_stamp: None, sequence: 7, status: 0, sdu: &[1, 2, 3] })
        );
        let p = packet(0x06B, Some(0x1234_5678), 0xFFFF, 2, &[]);
        assert_eq!(
            parse_iso_data(&p),
            Some(IsoData { handle: 0x6B, time_stamp: Some(0x1234_5678), sequence: 0xFFFF, status: 2, sdu: &[] })
        );
        // First-fragment header (BTstack's reassembly buffer)
        let mut p = packet(0x061, None, 1, 0, &[9]);
        p[1] &= !0x30;
        assert_eq!(parse_iso_data(&p).map(|d| d.sdu), Some(&[9u8][..]));
        // Continuation fragments, short packets, lengths past the end
        p[1] |= 0x10;
        assert_eq!(parse_iso_data(&p), None);
        assert_eq!(parse_iso_data(&[0x61, 0x20, 4]), None);
        let mut p = packet(0x061, None, 1, 0, &[9, 9]);
        p.pop();
        assert_eq!(parse_iso_data(&p), None);
        assert_eq!(parse_iso_data(&[0x61, 0x20, 2, 0, 1, 0]), None);
    }

    #[test]
    fn fills_the_ring_by_stream() {
        let (producer, mut consumer) = rtrb::RingBuffer::new(2);
        let stats = Arc::new(LeRxStats::default());
        let mut rx = IsoRx::new(producer, &[0x60, 0x62], stats.clone());
        assert!(rx.on_packet(&packet(0x62, None, 5, 0, &[1, 2])));
        assert!(!rx.on_packet(&packet(0x61, None, 5, 0, &[1, 2])), "not a source CIS");
        assert!(!rx.on_packet(&[0x05, 0x00]));
        assert!(rx.on_packet(&packet(0x60, Some(1), 9, 1, &[3])));
        assert!(rx.on_packet(&packet(0x60, None, 10, 0, &[4])));
        let a = consumer.pop().unwrap();
        assert_eq!((a.stream, a.sequence, a.status, a.sdu()), (1, 5, 0, &[1u8, 2][..]));
        let b = consumer.pop().unwrap();
        assert_eq!((b.stream, b.sequence, b.status, b.sdu()), (0, 9, 1, &[3u8][..]));
        assert!(consumer.pop().is_err());
        assert_eq!(stats.packets.load(Ordering::Relaxed), 3);
        assert_eq!(stats.bad_status.load(Ordering::Relaxed), 1);
        assert_eq!(stats.ring_full.load(Ordering::Relaxed), 1);
        assert_eq!(stats.ignored.load(Ordering::Relaxed), 2);
        drop(rx);
        assert!(consumer.is_abandoned());
        assert!(is_iso_data(HCI_ISO_DATA_PACKET) && !is_iso_data(0x04));
    }
}

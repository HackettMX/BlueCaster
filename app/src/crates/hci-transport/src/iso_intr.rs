//! Developer mode experiment (Developer > Debug, issue #86): HCI ISO data
//! (LE Audio) over an interrupt interface of a MediaTek adapter instead of
//! the bulk pipes of interface 0.
//!
//! What is known: an MT7925 lost every ISO packet sent on the bulk OUT pipe
//! (no Number Of Completed Packets for its CIS handles), and its descriptors
//! list an interface 2 with an interrupt IN and OUT endpoint (64 bytes in
//! alternate setting 0, 512 in 1). That the controller takes ISO data there
//! is a guess this experiment is for; nothing else uses it.
//!
//! Off unless turned on, and only for a MediaTek adapter whose descriptors
//! have such an interface: the interface, its alternate setting and its
//! endpoints are taken from the descriptors, nothing is assumed. Outgoing
//! ISO packets go to its interrupt OUT endpoint, optionally zero-padded to
//! a length set in the same place; what arrives on its interrupt IN
//! endpoint is split into ISO packets by their length field (all-zero
//! padding dropped). Everything else stays on interface 0 as before.
//!
//! The log keeps apart what USB and the controller each did: packets
//! queued, USB transfers submitted, completed and failed here (a completed
//! transfer only says the adapter took the bytes); whether the controller
//! sent them shows in its Number Of Completed Packets for the CIS handles,
//! which the LE Audio sender counts.
//!
//! Closing stops the threads, puts the interface back to alternate setting
//! 0 and releases it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

use btstack_sys::Upstream;
use btstack_sys::consts::HCI_ISO_DATA_PACKET;
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, In, Interrupt, Out, TransferError};
use nusb::{Device, Endpoint, Interface, MaybeFuture};

use crate::reassembly::Reassembler;

const MEDIATEK_VID: u16 = 0x0E8D;
/// Interfaces 0 (HCI) and 1 (SCO) are never taken
const FIRST_CANDIDATE: u8 = 2;
const OUT_IN_FLIGHT: usize = 8;
const IN_TRANSFERS: usize = 3;
const IN_TRANSFER_SIZE: usize = 2048;
/// Incoming transfers whose first bytes are logged
const IN_LOGGED: u64 = 3;
/// Longest padding accepted (an ISO packet header and a 16-bit length)
pub const PAD_MAX: usize = 4096;
const POLL: Duration = Duration::from_millis(100);

/// The experiment's settings (Developer > Debug), read when the adapter
/// opens
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IsoInterfaceConfig {
    pub enabled: bool,
    /// Outgoing packets shorter than this are zero-padded to it; 0: none
    pub pad_to: usize,
}

static CONFIG: Mutex<IsoInterfaceConfig> = Mutex::new(IsoInterfaceConfig { enabled: false, pad_to: 0 });

pub fn set_config(config: IsoInterfaceConfig) {
    let config = IsoInterfaceConfig { pad_to: config.pad_to.min(PAD_MAX), ..config };
    *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = config;
}

fn config() -> IsoInterfaceConfig {
    *CONFIG.lock().unwrap_or_else(|e| e.into_inner())
}

/// An interface's alternate setting with an interrupt IN and OUT endpoint
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    interface: u8,
    alt: u8,
    in_addr: u8,
    out_addr: u8,
    out_max_packet: usize,
    interval: u8,
}

/// An endpoint as the descriptors list it: address, type, max packet,
/// interval
type EndpointDesc = (u8, TransferType, usize, u8);
/// An alternate setting: interface, alt, its endpoints
type AltSetting = (u8, u8, Vec<EndpointDesc>);

/// The alternate settings: the one to use is the setting with the largest
/// interrupt OUT packet among those with an interrupt IN and OUT endpoint,
/// on the lowest interface from 2 that has one
fn pick(settings: &[AltSetting]) -> Option<Candidate> {
    let mut best: Option<Candidate> = None;
    for (interface, alt, endpoints) in settings {
        if *interface < FIRST_CANDIDATE {
            continue;
        }
        let interrupt = |out: bool| {
            endpoints.iter().find(|(addr, kind, _, _)| *kind == TransferType::Interrupt && (addr & 0x80 == 0) == out)
        };
        let (Some(i), Some(o)) = (interrupt(false), interrupt(true)) else { continue };
        let c = Candidate {
            interface: *interface,
            alt: *alt,
            in_addr: i.0,
            out_addr: o.0,
            out_max_packet: o.2,
            interval: o.3,
        };
        best = match best {
            Some(b) if b.interface < c.interface => Some(b),
            Some(b) if b.interface == c.interface && b.out_max_packet >= c.out_max_packet => Some(b),
            _ => Some(c),
        };
    }
    best
}

fn candidate(device: &Device) -> Option<Candidate> {
    let config = device.active_configuration().ok()?;
    let settings: Vec<AltSetting> = config
        .interface_alt_settings()
        .map(|a| {
            let eps =
                a.endpoints().map(|e| (e.address(), e.transfer_type(), e.max_packet_size(), e.interval())).collect();
            (a.interface_number(), a.alternate_setting(), eps)
        })
        .collect();
    pick(&settings)
}

/// What USB did with the experiment's packets (the log at close)
#[derive(Default)]
struct Counters {
    queued: AtomicU64,
    submitted: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    bytes: AtomicU64,
    padded: AtomicU64,
    in_transfers: AtomicU64,
    in_packets: AtomicU64,
    in_errors: AtomicU64,
}

impl Counters {
    fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// The interface in use while the experiment is on
pub(crate) struct IsoInterface {
    interface: Interface,
    candidate: Candidate,
    pad_to: usize,
    out_tx: Option<mpsc::Sender<Buffer>>,
    writer: Thread,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    counters: Arc<Counters>,
}

impl IsoInterface {
    /// The interface claimed and its threads started; None (logged) when
    /// the experiment is off, the adapter is not MediaTek's or it has no
    /// such interface, or claiming it failed
    pub(crate) fn open(device: &Device, vid: u16, upstream: &Upstream) -> Option<IsoInterface> {
        let config = config();
        if !config.enabled {
            return None;
        }
        if vid != MEDIATEK_VID {
            tracing::info!(target: "hci", "USB: ISO over an interrupt interface: on, but not a MediaTek adapter; unused");
            return None;
        }
        let Some(c) = candidate(device) else {
            tracing::warn!(target: "hci", "USB: ISO over an interrupt interface: no interface with interrupt IN and OUT endpoints");
            return None;
        };
        tracing::info!(
            target: "hci",
            "USB: ISO over an interrupt interface (experiment): interface {} alt {}, IN 0x{:02x}, OUT 0x{:02x}, max packet {}, interval {}, padding {}",
            c.interface,
            c.alt,
            c.in_addr,
            c.out_addr,
            c.out_max_packet,
            c.interval,
            if config.pad_to > 0 { format!("to {} bytes", config.pad_to) } else { "none".into() }
        );
        let interface = match device.claim_interface(c.interface).wait() {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(target: "hci", "USB: cannot claim interface {}: {e}; ISO stays on the bulk pipe", c.interface);
                return None;
            }
        };
        if let Err(e) = interface.set_alt_setting(c.alt).wait() {
            tracing::warn!(target: "hci", "USB: interface {} alt {} not selected: {e}; ISO stays on the bulk pipe", c.interface, c.alt);
            return None;
        }
        tracing::info!(target: "hci", "USB: interface {} alt {} selected", c.interface, c.alt);
        let out = interface.endpoint::<Interrupt, Out>(c.out_addr);
        let input = interface.endpoint::<Interrupt, In>(c.in_addr);
        let (mut out, mut input) = match (out, input) {
            (Ok(o), Ok(i)) => (o, i),
            (o, i) => {
                tracing::warn!(
                    target: "hci",
                    "USB: interface {} endpoints not available (OUT {:?}, IN {:?}); ISO stays on the bulk pipe",
                    c.interface,
                    o.err(),
                    i.err()
                );
                let _ = interface.set_alt_setting(0).wait();
                return None;
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let (out_tx, out_rx) = mpsc::channel::<Buffer>();
        let mut threads = Vec::new();
        let reader = {
            let (stop, counters, upstream) = (stop.clone(), counters.clone(), upstream.clone());
            std::thread::Builder::new()
                .name("usb-iso-in".into())
                .spawn(move || read_loop(&mut input, &stop, &counters, &upstream))
        };
        let writer = {
            let (stop, counters) = (stop.clone(), counters.clone());
            std::thread::Builder::new().name("usb-iso-out".into()).spawn(move || {
                let _priority = crate::acl_out_priority();
                write_loop(&mut out, out_rx, &stop, &counters)
            })
        };
        let (reader, writer) = match (reader, writer) {
            (Ok(r), Ok(w)) => (r, w),
            (r, w) => {
                tracing::warn!(target: "hci", "USB: ISO interface threads not started; ISO stays on the bulk pipe");
                stop.store(true, Ordering::Release);
                for t in [r.ok(), w.ok()].into_iter().flatten() {
                    let _ = t.join();
                }
                let _ = interface.set_alt_setting(0).wait();
                return None;
            }
        };
        let writer_thread = writer.thread().clone();
        threads.push(reader);
        threads.push(writer);
        Some(IsoInterface {
            interface,
            candidate: c,
            pad_to: config.pad_to,
            out_tx: Some(out_tx),
            writer: writer_thread,
            stop,
            threads,
            counters,
        })
    }

    /// Queues one ISO packet (copied; padded if set); false if the writer
    /// is gone
    pub(crate) fn send(&self, packet: &[u8]) -> bool {
        let Some(tx) = &self.out_tx else { return false };
        let len = packet.len().max(self.pad_to);
        let mut buf = Buffer::new(len);
        buf.extend_from_slice(packet);
        if len > packet.len() {
            buf.extend_from_slice(&vec![0u8; len - packet.len()]);
            Counters::add(&self.counters.padded, 1);
        }
        Counters::add(&self.counters.queued, 1);
        if tx.send(buf).is_err() {
            return false;
        }
        self.writer.unpark();
        true
    }

    /// Stops the threads, then alternate setting 0 again
    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.out_tx = None;
        self.writer.unpark();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let c = &self.counters;
        tracing::info!(
            target: "hci",
            "USB: ISO interface {} closed: {} packets queued ({} padded), USB transfers {} submitted, {} completed ({} bytes), {} failed; {} packets in, {} read errors",
            self.candidate.interface,
            Counters::get(&c.queued),
            Counters::get(&c.padded),
            Counters::get(&c.submitted),
            Counters::get(&c.completed),
            Counters::get(&c.bytes),
            Counters::get(&c.failed),
            Counters::get(&c.in_packets),
            Counters::get(&c.in_errors),
        );
        if let Err(e) = self.interface.set_alt_setting(0).wait() {
            tracing::debug!("USB: interface {} back to alt 0: {e}", self.candidate.interface);
        }
    }
}

impl Drop for IsoInterface {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            self.stop();
        }
    }
}

/// Submits queued packets, up to [`OUT_IN_FLIGHT`] at a time, and counts
/// their completions; a stall clears the pipe, the packets in it are lost
fn write_loop(ep: &mut Endpoint<Interrupt, Out>, rx: mpsc::Receiver<Buffer>, stop: &AtomicBool, counters: &Counters) {
    let addr = ep.endpoint_address();
    let mut queue: VecDeque<Buffer> = VecDeque::new();
    let mut open = true;
    let mut first_failure_logged = false;
    while !stop.load(Ordering::Acquire) {
        while open {
            match rx.try_recv() {
                Ok(b) => queue.push_back(b),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => open = false,
            }
        }
        while ep.pending() < OUT_IN_FLIGHT
            && let Some(b) = queue.pop_front()
        {
            ep.submit(b);
            Counters::add(&counters.submitted, 1);
        }
        if ep.pending() == 0 {
            if !open {
                break;
            }
            std::thread::park_timeout(POLL);
            continue;
        }
        let Some(c) = ep.wait_next_complete(POLL) else { continue };
        match c.status {
            Ok(()) => {
                if Counters::get(&counters.completed) == 0 {
                    tracing::info!(target: "hci", "USB: first ISO write on 0x{addr:02x} completed ({} bytes)", c.actual_len);
                }
                Counters::add(&counters.completed, 1);
                Counters::add(&counters.bytes, c.actual_len as u64);
            }
            Err(TransferError::Cancelled) if stop.load(Ordering::Acquire) => break,
            Err(e) => {
                Counters::add(&counters.failed, 1);
                if !first_failure_logged {
                    first_failure_logged = true;
                    tracing::warn!(target: "hci", "USB: ISO write on 0x{addr:02x} failed: {e}");
                } else {
                    tracing::debug!("USB: ISO write on 0x{addr:02x} failed: {e:?}");
                }
                if matches!(e, TransferError::Stall) {
                    ep.cancel_all();
                    while ep.pending() > 0 && ep.wait_next_complete(Duration::from_secs(1)).is_some() {}
                    let _ = ep.clear_halt().wait();
                }
            }
        }
    }
    ep.cancel_all();
    while ep.pending() > 0 && ep.wait_next_complete(Duration::from_secs(1)).is_some() {}
}

/// Reads the interrupt IN endpoint into ISO packets for BTstack
fn read_loop(ep: &mut Endpoint<Interrupt, In>, stop: &AtomicBool, counters: &Counters, upstream: &Upstream) {
    let addr = ep.endpoint_address();
    let mps = ep.max_packet_size().max(1);
    let size = IN_TRANSFER_SIZE.div_ceil(mps) * mps;
    let mut iso = Reassembler::new(HCI_ISO_DATA_PACKET);
    let mut errors = 0u32;
    for _ in 0..IN_TRANSFERS {
        ep.submit(Buffer::new(size));
    }
    while !stop.load(Ordering::Acquire) {
        let Some(c) = ep.wait_next_complete(POLL) else { continue };
        match c.status {
            Ok(()) => {
                errors = 0;
                // The first transfers' start (ISO headers: handle, flags,
                // lengths), for telling what the controller sends here
                if Counters::get(&counters.in_transfers) < IN_LOGGED {
                    let head = &c.buffer[..c.actual_len.min(8)];
                    tracing::info!(target: "hci", "USB: ISO interface in: {} bytes, starting {head:02X?}", c.actual_len);
                }
                Counters::add(&counters.in_transfers, 1);
                iso.push(&c.buffer[..c.actual_len], |p| {
                    if p.len() >= 4 && p[..4] == [0, 0, 0, 0] {
                        return;
                    }
                    Counters::add(&counters.in_packets, 1);
                    upstream.deliver(HCI_ISO_DATA_PACKET, p)
                });
                iso.drop_zero_tail();
            }
            Err(TransferError::Cancelled) if stop.load(Ordering::Acquire) => break,
            Err(e) => {
                Counters::add(&counters.in_errors, 1);
                iso.reset();
                errors += 1;
                tracing::debug!("USB: ISO read on 0x{addr:02x} failed: {e:?}");
                if errors == 10 {
                    tracing::warn!(target: "hci", "USB: ISO reads on 0x{addr:02x} keep failing ({e}); reading stopped");
                    break;
                }
                if matches!(e, TransferError::Stall) {
                    let _ = ep.clear_halt().wait();
                }
            }
        }
        ep.submit(Buffer::new(size));
    }
    ep.cancel_all();
    while ep.pending() > 0 && ep.wait_next_complete(Duration::from_secs(1)).is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(addr: u8, kind: TransferType, mps: usize) -> EndpointDesc {
        (addr, kind, mps, 1)
    }

    #[test]
    fn picks_the_largest_interrupt_setting_from_interface_2() {
        use TransferType::*;
        let settings = vec![
            (0, 0, vec![ep(0x81, Interrupt, 16), ep(0x82, Bulk, 512), ep(0x02, Bulk, 512)]),
            (1, 1, vec![ep(0x83, Isochronous, 9), ep(0x03, Isochronous, 9)]),
            (2, 0, vec![ep(0x8A, Interrupt, 64), ep(0x0A, Interrupt, 64)]),
            (2, 1, vec![ep(0x8A, Interrupt, 512), ep(0x0A, Interrupt, 512)]),
        ];
        let c = pick(&settings).unwrap();
        assert_eq!((c.interface, c.alt, c.in_addr, c.out_addr, c.out_max_packet), (2, 1, 0x8A, 0x0A, 512));
    }

    #[test]
    fn nothing_without_interrupt_in_and_out_beyond_interface_1() {
        use TransferType::*;
        let settings = vec![
            (0, 0, vec![ep(0x81, Interrupt, 16), ep(0x02, Interrupt, 64)]),
            (2, 0, vec![ep(0x8A, Interrupt, 64)]),
            (3, 0, vec![ep(0x8B, Bulk, 64), ep(0x0B, Bulk, 64)]),
        ];
        assert_eq!(pick(&settings), None);
    }

    #[test]
    fn padding_is_capped() {
        set_config(IsoInterfaceConfig { enabled: true, pad_to: 100_000 });
        assert_eq!(config().pad_to, PAD_MAX);
        set_config(IsoInterfaceConfig::default());
    }
}

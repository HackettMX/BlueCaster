//! HCI over USB ("H2") on a controller bound to WinUSB, through `nusb`.
//!
//! Interface 0 carries HCI: commands on the default control pipe (class
//! request to the device), events on the interrupt IN pipe, ACL data on the
//! bulk pipes. SCO data (the headset microphone) uses the isochronous pipes
//! of interface 1, see "SCO" below. HCI ISO data
//! (LE Audio) shares the bulk pipes with ACL, as in v1 and Linux btusb: out
//! through the ACL slot; in, the ACL reader delivers it as ACL and BTstack
//! tells the two apart by the connection handle (a CIS handle).
//!
//! Recovery kept from v1's modified WinUSB transport:
//! - the event and ACL pipes are reset (clear halt) when the adapter is
//!   opened, so a halted endpoint or data left from a previous session does
//!   not keep the controller from answering HCI Reset
//! - a failed event read is retried (clear halt + new read), up to
//!   EVENT_IN_MAX_ERRORS in a row, instead of ending event delivery
//! - HCI commands time out (COMMAND_TIMEOUT), so a controller that stops
//!   accepting them does not block the stack
//! - close sends HCI Reset (bounded) and cancels everything, leaving the
//!   controller idle for the next session
//! - close waits for that reset's Command Complete (CLOSE_RESET_WAIT), so
//!   the next session's first command does not reach a controller that is
//!   still resetting: an AX211 that got HCI Reset while one from close was
//!   in progress answered one of the two and then nothing at all, until
//!   Windows restarted
//! - a failed ACL read is retried the same way (ACL_IN_MAX_ERRORS)
//! - ACL / ISO writes have no timeout, and a failed one only drops the packet
//!   (a stalled pipe also drops the writes behind it and is cleared)
//! - a transfer that fails for good (device gone, a lost command) is
//!   reported to BTstack's owner through [`Upstream::fail`]: BTstack itself
//!   cannot recover, it waits for the answer to a command that was lost
//! - an adapter that cannot be opened (another program holds it) is skipped
//!   for the next one the selector takes
//!
//! An Intel controller in its bootloader takes Secure Send (0xFC09) on the
//! bulk OUT pipe (the firmware loader sends it through the ACL slot) and
//! answers on the bulk IN pipe, as Linux btusb handles it: while
//! [`HciBackend::set_bootloader`] is on, the ACL reader delivers what it
//! reads as events, and close sends no HCI Reset: an AX211 bootloader
//! answered one with Unknown HCI Command and no command after it.
//!
//! BTstack treats this transport as asynchronous: `send` hands the packet to
//! a writer thread (one for commands, one for ACL / ISO) and returns. The
//! command writer reports each command as sent once it is out. ACL / ISO
//! packets are reported sent as soon as `send` has copied them, so BTstack
//! hands over the next one (the next fragment of a media packet) at once,
//! and the ACL writer keeps several bulk transfers in flight. v1 had one
//! ACL write in flight, finished on the run loop thread; v2 first waited for
//! each write on the writer thread and then for the run loop to pick up its
//! completion, so every ACL fragment took two thread hand-overs and the
//! pipe sat idle in between. The controller's ACL / ISO buffer count, which
//! BTstack keeps to, bounds how many packets are queued.
//!
//! # SCO
//!
//! A port of BTstack's platform/windows/hci_transport_h2_winusb.c (USB SCO
//! mode, [`HciBackend::is_h2`]), with the WinUSB isochronous calls in
//! win-sys (`usb_isoch`). Interface 1 is claimed at open when the adapter
//! has it; BTstack's set_sco_config selects its alternate setting for the
//! SCO links (8-bit voice: 1, 2, 3 for one to three links; 16-bit: 2, 4,
//! 5) and starts the "usb-sco" thread, or stops it and selects alternate
//! setting 0 when the last link is gone. The thread:
//! - keeps [`SCO_IN_TRANSFERS`] reads of 3 ms each in flight
//!   (WinUsb_ReadIsochPipeAsap, continuing the stream); the IN data is a
//!   byte stream of SCO packets (handle, length, payload) cut into
//!   isochronous packets, reassembled into whole SCO packets for BTstack
//! - writes each SCO packet BTstack sends (header included, 3 ms of audio:
//!   one isochronous packet per frame at full speed) as one
//!   WinUsb_WriteIsochPipeAsap, up to [`SCO_OUT_SLOTS`] in flight
//!
//! BTstack's send token in this mode is HCI_EVENT_SCO_CAN_SEND_NOW from the
//! transport: `send` copies the packet, reports it sent and hands out the
//! token again while fewer than [`SCO_OUT_SLOTS`] packets are queued or in
//! flight; otherwise the thread does when a write completes.
//!
//! Only a backend made [`UsbTransport::with_sco`] (the stack offers voice)
//! is H2 and claims interface 1 (is_h2 is read before open); otherwise SCO
//! is left to BTstack's plain path, which nothing uses. Without a usable
//! interface 1, `can_send` refuses SCO, SCO packets sent anyway are dropped
//! (and reported sent, so BTstack's buffer is free again) and
//! [`HciBackend::sco_ready`] is false, so bt-host gives the link up.
//!
//! Full speed (the usual Bluetooth adapter) has one isochronous packet per
//! 1 ms frame: a read is 3 packets, a write of one SCO packet is as many
//! packets as its length takes at the pipe's packet size (3 for the
//! alternate setting BTstack's payload sizes are made for). High speed has
//! one packet per service interval (bInterval) and WinUSB wants whole 1 ms
//! frames of packets per read ([`win_sys::usb_isoch::packets_for`]); writes
//! are left as on full speed (not verified on a high-speed adapter).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use btstack_sys::consts::{
    HCI_ACL_DATA_PACKET, HCI_COMMAND_DATA_PACKET, HCI_EVENT_PACKET, HCI_EVENT_SCO_CAN_SEND_NOW, HCI_ISO_DATA_PACKET,
    HCI_SCO_DATA_PACKET,
};
use btstack_sys::{ControlData, ControlKind, ControlRecipient, ControlRequest, HciBackend, Upstream, UsbDescriptors};
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient, TransferError};
use nusb::{Device, DeviceInfo, Endpoint, Interface, MaybeFuture, Speed};
use win_sys::usb_isoch::{self, Event, IsochIn, IsochOut, Win32Error};

use crate::reassembly::Reassembler;

/// HCI command control transfer timeout. A working controller accepts
/// commands at once, firmware download chunks included.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(2000);
/// HCI Reset sent on close
const CLOSE_RESET_TIMEOUT: Duration = Duration::from_millis(500);
/// Waiting for the Command Complete of the HCI Reset sent on close (an
/// AX211 took 40 to 110 ms)
const CLOSE_RESET_WAIT: Duration = Duration::from_millis(1000);
/// Consecutive failed event reads before event delivery is given up
const EVENT_IN_MAX_ERRORS: u32 = 10;
/// Consecutive failed ACL reads before ACL delivery is given up
const ACL_IN_MAX_ERRORS: u32 = 10;
/// How often the I/O threads look at the stop flag
const POLL: Duration = Duration::from_millis(100);
/// Transfers kept in flight per IN pipe
const IN_TRANSFERS: usize = 3;
const EVENT_TRANSFER_SIZE: usize = 512;
const ACL_TRANSFER_SIZE: usize = 4096;
/// Bulk OUT transfers kept in flight
const ACL_OUT_TRANSFERS: usize = 8;

/// SCO: the interface with the isochronous pipes
const SCO_INTERFACE: u8 = 1;
/// SCO IN transfers kept in flight (BTstack's ISOC_BUFFERS), 3 ms each
const SCO_IN_TRANSFERS: usize = 8;
/// Milliseconds of stream per SCO transfer: BTstack's USB SCO mode sends 3 ms
/// of audio per SCO packet
const SCO_TRANSFER_MS: u32 = 3;
/// SCO packets queued or in flight at most (BTstack's SCO_RING_BUFFER_COUNT:
/// 60 ms)
const SCO_OUT_SLOTS: usize = 20;
/// Set-interface retries on a busy device, and the pause between them
const SCO_ALT_TRIES: u32 = 10;
const SCO_ALT_RETRY: std::time::Duration = std::time::Duration::from_millis(10);
/// The longest SCO packet: header and a 255-byte payload
const SCO_OUT_SLOT_LEN: usize = 3 + 255;
/// Consecutive failed SCO reads after which the thread logs a warning
const SCO_IN_WARN_ERRORS: u32 = 50;

/// The writer slot of an outgoing packet: 0 commands (control pipe), 1 ACL
/// and ISO data (bulk OUT pipe), 2 SCO data (isochronous OUT pipe of
/// interface 1). BTstack sends one command at a time; ACL and ISO packets
/// as the controller's buffers allow, SCO packets as the SCO send token
/// allows (module docs).
fn out_slot(packet_type: u8) -> Option<usize> {
    match packet_type {
        HCI_COMMAND_DATA_PACKET => Some(0),
        HCI_ACL_DATA_PACKET | HCI_ISO_DATA_PACKET => Some(1),
        HCI_SCO_DATA_PACKET => Some(2),
        _ => None,
    }
}

// ---- SCO: pure parts ----

/// Interface 1's alternate setting for `connections` SCO links (Bluetooth
/// Core, USB transport: 1 to 3 links of 8 kHz voice, 8 or 16 bit; BTstack's
/// alt_setting_8_bit / alt_setting_16_bit, so the 3 ms payloads BTstack
/// sends fit). Voice setting bit 0x0020: 16-bit samples. None: no links
/// (alternate setting 0); more than three take the three-link setting.
fn sco_alt_setting(voice_setting: u16, connections: u8) -> Option<u8> {
    let i = (connections.checked_sub(1)? as usize).min(2);
    Some(if voice_setting & 0x0020 != 0 { [2, 4, 5][i] } else { [1, 2, 3][i] })
}

/// The isochronous packet size of alternate settings 1 to 6 as the USB
/// transport defines them (BTstack's iso_packet_size_for_alt_setting), for
/// a pipe whose size cannot be read
fn sco_packet_size(alt: u8) -> usize {
    [0, 9, 17, 25, 33, 49, 63].get(alt as usize).copied().unwrap_or(63)
}

/// Feeds the isochronous packets of one IN transfer into the SCO stream:
/// whole SCO packets go to `deliver`. A failed packet drops the partial SCO
/// packet (what follows cannot continue it). Returns the failed packets.
fn sco_in_transfer<'a>(
    sco: &mut Reassembler,
    packets: impl IntoIterator<Item = Result<&'a [u8], i32>>,
    mut deliver: impl FnMut(&[u8]),
) -> usize {
    let mut failed = 0;
    for p in packets {
        match p {
            Ok(data) if !data.is_empty() => sco.push(data, &mut deliver),
            Ok(_) => {}
            Err(_) => {
                failed += 1;
                sco.reset();
            }
        }
    }
    failed
}

/// Isochronous packets a write of `len` bytes takes at `packet_len` bytes
/// per packet (WinUSB cuts writes at MaximumBytesPerInterval)
fn sco_out_packets(len: usize, packet_len: usize) -> usize {
    len.div_ceil(packet_len.max(1))
}

/// SCO packets BTstack handed over that are not written yet (queued or in
/// flight), at most `cap`: BTstack gets its send token back while there is
/// room
struct ScoCredits {
    outstanding: AtomicUsize,
    cap: usize,
}

impl ScoCredits {
    const fn new(cap: usize) -> ScoCredits {
        ScoCredits { outstanding: AtomicUsize::new(0), cap }
    }

    fn room(&self) -> bool {
        self.outstanding.load(Ordering::Acquire) < self.cap
    }

    /// A packet accepted; whether there is room for another
    fn take(&self) -> bool {
        self.outstanding.fetch_add(1, Ordering::AcqRel) + 1 < self.cap
    }

    /// A packet written (or dropped); whether there is room now
    fn give(&self) -> bool {
        let mut n = self.outstanding.load(Ordering::Acquire);
        loop {
            let next = n.saturating_sub(1);
            match self.outstanding.compare_exchange_weak(n, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return next < self.cap,
                Err(now) => n = now,
            }
        }
    }

    fn clear(&self) {
        self.outstanding.store(0, Ordering::Release);
    }
}

/// Win32 errors after which the WinUSB handle no longer reaches the adapter
/// (as [`device_gone`]): ERROR_INVALID_HANDLE, ERROR_FILE_NOT_FOUND,
/// ERROR_BAD_COMMAND, ERROR_NO_SUCH_DEVICE, ERROR_DEVICE_NOT_CONNECTED
fn sco_device_gone(e: Win32Error) -> bool {
    matches!(e, 6 | 2 | 22 | 433 | 1167)
}

/// VID and PID of the adapter opened last (what a stack runs on)
static LAST_OPENED: Mutex<Option<(u16, u16)>> = Mutex::new(None);

/// ACL / ISO OUT writes since [`take_acl_out_timing`] last ran: the
/// longest from submit to completion (µs) and the most in flight at once
static ACL_OUT_LATENCY_US: AtomicU64 = AtomicU64::new(0);
static ACL_OUT_PENDING_MAX: AtomicU64 = AtomicU64::new(0);

/// How often the event reader logs the transfer counters (debug)
const STATS_EVERY: Duration = Duration::from_secs(10);

/// Transfer counters since the last debug summary
struct Stats {
    events_in: AtomicU64,
    events_in_bytes: AtomicU64,
    acl_in: AtomicU64,
    acl_in_bytes: AtomicU64,
    commands_out: AtomicU64,
    acl_out: AtomicU64,
    acl_out_bytes: AtomicU64,
    in_errors: AtomicU64,
    out_errors: AtomicU64,
    acl_out_pending_max: AtomicU64,
}

static STATS: Stats = Stats {
    events_in: AtomicU64::new(0),
    events_in_bytes: AtomicU64::new(0),
    acl_in: AtomicU64::new(0),
    acl_in_bytes: AtomicU64::new(0),
    commands_out: AtomicU64::new(0),
    acl_out: AtomicU64::new(0),
    acl_out_bytes: AtomicU64::new(0),
    in_errors: AtomicU64::new(0),
    out_errors: AtomicU64::new(0),
    acl_out_pending_max: AtomicU64::new(0),
};

impl Stats {
    fn add(counter: &AtomicU64, n: usize) {
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// The counters, starting them again
    fn take(&self) -> [u64; 10] {
        [
            &self.events_in,
            &self.events_in_bytes,
            &self.acl_in,
            &self.acl_in_bytes,
            &self.commands_out,
            &self.acl_out,
            &self.acl_out_bytes,
            &self.in_errors,
            &self.out_errors,
            &self.acl_out_pending_max,
        ]
        .map(|c| c.swap(0, Ordering::Relaxed))
    }

    /// Logs the counters (debug) and starts them again
    fn log(&self, period: Duration) {
        let s = self.take();
        tracing::debug!(
            "USB: last {:.1} s: events in {} ({} B), ACL in {} ({} B), commands out {}, ACL/ISO out {} ({} B), \
             errors in {} out {}, ACL out max pending {}",
            period.as_secs_f32(),
            s[0],
            s[1],
            s[2],
            s[3],
            s[4],
            s[5],
            s[6],
            s[7],
            s[8],
            s[9]
        );
    }
}

/// The longest ACL / ISO OUT write (submit to completion) and the most
/// writes in flight since the last call (core's stream health log): how
/// long the controller held data back over USB
pub fn take_acl_out_timing() -> (Duration, usize) {
    let latency = ACL_OUT_LATENCY_US.swap(0, Ordering::Relaxed);
    let pending = ACL_OUT_PENDING_MAX.swap(0, Ordering::Relaxed);
    (Duration::from_micros(latency), pending as usize)
}

/// VID and PID of the adapter a [`UsbTransport`] opened last; None before
/// the first open
pub fn last_opened() -> Option<(u16, u16)> {
    *LAST_OPENED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Which adapter to open
#[derive(Debug, Clone, Default)]
pub struct UsbSelector {
    /// Windows device instance ID (exact adapter), if chosen
    pub instance_id: Option<String>,
    /// VID:PID, if chosen
    pub vid_pid: Option<(u16, u16)>,
    /// VID:PID of controllers that are Bluetooth adapters even without the
    /// Bluetooth class code (vendor class FF/01/01, e.g. Realtek tables)
    pub known: Vec<(u16, u16)>,
    /// Instance ID of the adapter tried first when several are on WinUSB:
    /// the one the chipset setup (Realtek firmware) was made for
    pub preferred: Option<String>,
}

impl UsbSelector {
    /// The adapter named on the command line (`-u`): a Windows device
    /// instance ID (`USB\VID_0BDA&PID_8771\00E04C000001`), a device
    /// interface path as v1 and SetupAPI list them
    /// (`\\?\usb#vid_0bda&pid_8771#00e04c000001#{a5dcbf10-...}`), or a
    /// VID:PID (`0bda:8771`, the first such adapter)
    pub fn from_arg(arg: &str) -> Result<UsbSelector, String> {
        let arg = arg.trim();
        let bad = || {
            format!(
                "'{arg}' is not a USB device: use its instance ID (USB\\VID_xxxx&PID_xxxx\\...), \
                 device path (\\\\?\\usb#vid_xxxx&pid_xxxx#...) or VID:PID"
            )
        };
        if let Some(vp) = parse_vid_pid_pair(arg) {
            return Ok(UsbSelector { vid_pid: Some(vp), ..Default::default() });
        }
        let id = if arg.starts_with(r"\\?\") || arg.starts_with(r"\\.\") {
            instance_id_from_device_path(arg).ok_or_else(bad)?
        } else {
            arg.to_string()
        };
        let mut parts = id.split('\\');
        let (Some(bus), Some(hw), Some(serial), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(bad());
        };
        if !bus.eq_ignore_ascii_case("usb") || !hw.to_ascii_uppercase().starts_with("VID_") || serial.is_empty() {
            return Err(bad());
        }
        Ok(UsbSelector { instance_id: Some(id), ..Default::default() })
    }

    /// The selector picks this device: the named adapter, else any
    /// Bluetooth-class or known one
    fn takes(&self, c: &UsbCandidate) -> bool {
        if let Some(id) = &self.instance_id {
            c.instance_id.eq_ignore_ascii_case(id)
        } else if let Some(vp) = self.vid_pid {
            (c.vid, c.pid) == vp
        } else {
            c.bluetooth_class || c.known
        }
    }
}

/// `0bda:8771` (hex VID:PID)
fn parse_vid_pid_pair(s: &str) -> Option<(u16, u16)> {
    let (v, p) = s.split_once(':')?;
    if v.len() != 4 || p.len() != 4 {
        return None;
    }
    Some((u16::from_str_radix(v, 16).ok()?, u16::from_str_radix(p, 16).ok()?))
}

/// The device instance ID in a device interface path:
/// `\\?\usb#vid_0bda&pid_8771#00e04c000001#{guid}` is the interface of
/// `usb\vid_0bda&pid_8771\00e04c000001` (instance IDs compare without case)
pub fn instance_id_from_device_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\").or_else(|| path.strip_prefix(r"\\.\"))?;
    // The interface class GUID (and an optional reference string) follows
    // the last '#'
    let (id, guid) = rest.rsplit_once('#')?;
    if !guid.starts_with('{') || id.split('#').count() != 3 {
        return None;
    }
    Some(id.replace('#', "\\"))
}

/// A USB device that could be a Bluetooth controller
#[derive(Debug, Clone)]
pub struct UsbCandidate {
    pub vid: u16,
    pub pid: u16,
    pub instance_id: String,
    /// Driver service ("WinUSB", "BTHUSB", ...)
    pub driver: Option<String>,
    pub product: Option<String>,
    /// Bluetooth class code (E0/01/01) on the device or interface 0
    pub bluetooth_class: bool,
    /// In the selector's list of known controllers
    pub known: bool,
}

impl UsbCandidate {
    pub fn winusb(&self) -> bool {
        self.driver.as_deref().is_some_and(|d| d.eq_ignore_ascii_case("winusb"))
    }
}

fn is_bluetooth_class(class: u8, subclass: u8, protocol: u8) -> bool {
    class == 0xE0 && subclass == 0x01 && protocol == 0x01
}

/// VMware's virtual Bluetooth adapter (v1 skipped it as well)
fn is_vmware(info: &DeviceInfo) -> bool {
    info.vendor_id() == 0x0E0F
}

fn candidate(info: &DeviceInfo, known: &[(u16, u16)]) -> UsbCandidate {
    let bluetooth_class = is_bluetooth_class(info.class(), info.subclass(), info.protocol())
        || info
            .interfaces()
            .any(|i| i.interface_number() == 0 && is_bluetooth_class(i.class(), i.subclass(), i.protocol()));
    UsbCandidate {
        vid: info.vendor_id(),
        pid: info.product_id(),
        instance_id: info.instance_id().to_string_lossy().into_owned(),
        driver: info.driver().map(str::to_owned),
        product: info.product_string().map(str::to_owned),
        bluetooth_class,
        known: known.contains(&(info.vendor_id(), info.product_id())),
    }
}

/// Bluetooth controllers on USB (any driver), Bluetooth class or known VID:PID
pub fn list_candidates(known: &[(u16, u16)]) -> Vec<UsbCandidate> {
    let devices = match nusb::list_devices().wait() {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("USB: listing devices failed: {e}");
            return Vec::new();
        }
    };
    let list: Vec<UsbCandidate> = devices
        .filter(|d| !is_vmware(d))
        .map(|d| candidate(&d, known))
        .filter(|c| c.bluetooth_class || c.known)
        .collect();
    tracing::debug!("USB: {} candidate controller(s)", list.len());
    list
}

struct Opened {
    /// Kept for the control pipe (commands) and the endpoints' lifetime
    interface: Interface,
    /// The event pipe, read again on close for the reset's answer
    event_addr: u8,
    /// Its descriptors for a chip report
    device: Device,
    stop: Arc<AtomicBool>,
    /// The command writer; buffers of sent commands come back for reuse
    command_tx: mpsc::Sender<(u8, Vec<u8>)>,
    command_recycle_rx: mpsc::Receiver<Vec<u8>>,
    /// A command is with the writer (BTstack: one at a time)
    command_in_flight: Arc<AtomicBool>,
    /// The ACL / ISO writer, woken by unpark; transfer buffers come back
    acl_tx: mpsc::Sender<Buffer>,
    acl_recycle_rx: mpsc::Receiver<Buffer>,
    acl_writer: Thread,
    upstream: Upstream,
    /// The controller runs its bootloader: events on the bulk IN pipe, no
    /// HCI Reset on close
    bootloader: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    /// Interface 1 (SCO), when the adapter has it and it could be claimed
    sco: Option<Sco>,
}

/// What `send` and the SCO thread share
struct ScoShared {
    /// SCO packets from BTstack, not yet handed to WinUSB
    queue: Mutex<VecDeque<Vec<u8>>>,
    /// Buffers of written packets, reused
    free: Mutex<Vec<Vec<u8>>>,
    credits: ScoCredits,
    /// The pipes run (an alternate setting with SCO bandwidth is selected
    /// and the thread is up)
    running: AtomicBool,
    stop: AtomicBool,
    /// Wakes the thread: a packet queued, or stop
    wake: Event,
}

/// SCO over interface 1's isochronous pipes (module docs)
struct Sco {
    interface: Interface,
    high_speed: bool,
    shared: Arc<ScoShared>,
    /// The alternate setting in use, None: 0 (no SCO bandwidth)
    alt: Option<u8>,
    thread: Option<JoinHandle<()>>,
    /// Packets dropped because the pipes did not run (logged at stop)
    dropped: u64,
}

/// An isochronous endpoint of an alternate setting
#[derive(Debug, Clone, Copy)]
struct IsoEndpoint {
    address: u8,
    /// Bytes per service interval: one isochronous packet
    packet_len: usize,
    /// bInterval as in the descriptor
    interval: u8,
}

impl Sco {
    /// Claims interface 1 if the adapter has isochronous endpoints there;
    /// None (logged) otherwise
    fn claim(device: &Device) -> Option<Sco> {
        let has_iso = device.active_configuration().ok().is_some_and(|c| {
            c.interface_alt_settings().any(|a| {
                a.interface_number() == SCO_INTERFACE
                    && a.endpoints().any(|e| e.transfer_type() == TransferType::Isochronous)
            })
        });
        if !has_iso {
            tracing::info!(target: "hci", "USB: no isochronous interface {SCO_INTERFACE}: no SCO audio on this adapter");
            return None;
        }
        let interface = match device.claim_interface(SCO_INTERFACE).wait() {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(target: "hci", "USB: cannot claim interface {SCO_INTERFACE} (SCO audio): {e}");
                return None;
            }
        };
        // A session that ended without stopping may have left SCO bandwidth
        if let Err(e) = interface.set_alt_setting(0).wait() {
            tracing::debug!("USB: interface {SCO_INTERFACE} alt 0: {e}");
        }
        let speed = device.speed();
        let high_speed = matches!(speed, Some(Speed::High | Speed::Super | Speed::SuperPlus));
        tracing::debug!(
            "USB: interface {SCO_INTERFACE} claimed for SCO (speed {speed:?}, {} speed timing)",
            if high_speed { "high" } else { "full" }
        );
        let shared = Arc::new(ScoShared {
            queue: Mutex::new(VecDeque::with_capacity(SCO_OUT_SLOTS)),
            free: Mutex::new(Vec::with_capacity(SCO_OUT_SLOTS)),
            credits: ScoCredits::new(SCO_OUT_SLOTS),
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            wake: match Event::new(false) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(target: "hci", "USB: no event for the SCO thread (error {e}): no SCO audio");
                    return None;
                }
            },
        });
        Some(Sco { interface, high_speed, shared, alt: None, thread: None, dropped: 0 })
    }

    fn running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    /// The isochronous IN and OUT endpoints of alternate setting `alt`:
    /// WinUSB's pipe information, else the descriptors
    fn endpoints(&self, alt: u8) -> Result<(IsoEndpoint, IsoEndpoint), String> {
        let mut from_descriptors = (None, None);
        if let Some(a) = self.interface.descriptors().find(|a| a.alternate_setting() == alt) {
            for e in a.endpoints().filter(|e| e.transfer_type() == TransferType::Isochronous) {
                let ep = IsoEndpoint {
                    address: e.address(),
                    packet_len: e.max_packet_size() * e.packets_per_microframe() as usize,
                    interval: e.interval(),
                };
                let side = if e.address() & 0x80 != 0 { &mut from_descriptors.0 } else { &mut from_descriptors.1 };
                side.get_or_insert(ep);
            }
        }
        let (Some(mut ep_in), Some(mut ep_out)) = from_descriptors else {
            return Err(format!("interface {SCO_INTERFACE} alt {alt} lacks isochronous IN / OUT endpoints"));
        };
        match usb_isoch::pipes(&self.interface, alt) {
            Ok(pipes) => {
                for p in pipes.iter().filter(|p| p.isochronous) {
                    tracing::debug!(
                        "USB: interface {SCO_INTERFACE} alt {alt}: pipe 0x{:02x}, max packet {}, interval {}, {} bytes per interval",
                        p.id,
                        p.max_packet_size,
                        p.interval,
                        p.max_bytes_per_interval
                    );
                    for ep in [&mut ep_in, &mut ep_out] {
                        if ep.address == p.id && p.max_bytes_per_interval > 0 {
                            ep.packet_len = p.max_bytes_per_interval as usize;
                        }
                    }
                }
            }
            Err(e) => tracing::debug!("USB: pipe information of interface {SCO_INTERFACE} alt {alt}: error {e}"),
        }
        for ep in [&mut ep_in, &mut ep_out] {
            if ep.packet_len == 0 {
                ep.packet_len = sco_packet_size(alt);
            }
        }
        Ok((ep_in, ep_out))
    }

    /// Selects alternate setting `alt`. WinUSB answers ERROR_BUSY (170)
    /// now and then when an HCI command (a control transfer on the same
    /// device) is in flight: tried again a few times
    fn select_alt(&self, alt: u8) -> Result<(), String> {
        let mut tries = 0;
        loop {
            match self.interface.set_alt_setting(alt).wait() {
                Ok(()) => return Ok(()),
                Err(e) if tries < SCO_ALT_TRIES => {
                    tries += 1;
                    tracing::debug!("USB: alt {alt} of interface {SCO_INTERFACE}: {e} (try {tries})");
                    std::thread::sleep(SCO_ALT_RETRY);
                }
                Err(e) => return Err(format!("cannot select alt {alt}: {e}")),
            }
        }
    }

    /// Selects alternate setting `alt` and starts the pipes and the thread
    fn start(&mut self, alt: u8, upstream: &Upstream) -> Result<(), String> {
        self.select_alt(alt)?;
        self.alt = Some(alt);
        let (ep_in, ep_out) = self.endpoints(alt)?;
        let packets = usb_isoch::packets_for(self.high_speed, ep_in.interval, SCO_TRANSFER_MS);
        // Buffers are registered after the alternate setting is selected
        // (WinUSB registers them for the pipes of the current one)
        let input = IsochIn::new(&self.interface, ep_in.address, SCO_IN_TRANSFERS, packets, ep_in.packet_len)
            .map_err(|e| format!("cannot register the SCO IN buffer on 0x{:02x}: error {e}", ep_in.address))?;
        let output = IsochOut::new(&self.interface, ep_out.address, SCO_OUT_SLOTS, SCO_OUT_SLOT_LEN)
            .map_err(|e| format!("cannot register the SCO OUT buffer on 0x{:02x}: error {e}", ep_out.address))?;
        let s = &self.shared;
        s.queue.lock().unwrap_or_else(|e| e.into_inner()).clear();
        s.credits.clear();
        s.stop.store(false, Ordering::Release);
        s.running.store(true, Ordering::Release);
        tracing::info!(
            target: "hci",
            "USB: SCO on interface {SCO_INTERFACE} alt {alt}: in 0x{:02x} ({SCO_IN_TRANSFERS} x {packets} packets of {} B), \
             out 0x{:02x} (packets of {} B, up to {SCO_OUT_SLOTS} SCO packets queued)",
            ep_in.address,
            ep_in.packet_len,
            ep_out.address,
            ep_out.packet_len
        );
        let (shared, upstream) = (s.clone(), upstream.clone());
        let out_packet_len = ep_out.packet_len;
        let thread = std::thread::Builder::new()
            .name("usb-sco".into())
            .spawn(move || {
                // Audio has a deadline, as the ACL writer's media data
                let _priority = crate::acl_out_priority();
                sco_loop(input, output, out_packet_len, &shared, &upstream);
            })
            .map_err(|e| {
                s.running.store(false, Ordering::Release);
                format!("cannot start the SCO thread: {e}")
            })?;
        self.thread = Some(thread);
        Ok(())
    }

    /// Stops the thread (it cancels its transfers and unregisters the
    /// buffers) and selects alternate setting 0
    fn stop(&mut self) {
        let s = &self.shared;
        s.running.store(false, Ordering::Release);
        s.stop.store(true, Ordering::Release);
        s.wake.set();
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            tracing::debug!("USB: the SCO thread panicked");
        }
        s.queue.lock().unwrap_or_else(|e| e.into_inner()).clear();
        s.credits.clear();
        if self.dropped > 0 {
            tracing::debug!("USB: {} SCO packet(s) dropped while the pipes did not run", self.dropped);
            self.dropped = 0;
        }
        if self.alt.take().is_some() {
            match self.select_alt(0) {
                Ok(()) => tracing::info!(target: "hci", "USB: SCO stopped (interface {SCO_INTERFACE} alt 0)"),
                Err(e) => tracing::warn!(target: "hci", "USB: SCO stopped, but alt 0 failed: {e}"),
            }
        }
    }

    /// BTstack's set_sco_config: the alternate setting for the links, and
    /// the pipes started, restarted or stopped for it
    fn configure(&mut self, voice_setting: u16, connections: u8, upstream: &Upstream) {
        let alt = sco_alt_setting(voice_setting, connections);
        if alt == self.alt && (alt.is_none() || self.running()) {
            return;
        }
        self.stop();
        let Some(alt) = alt else { return };
        if let Err(e) = self.start(alt, upstream) {
            tracing::warn!(target: "hci", "USB: SCO audio cannot start: {e}");
            self.stop();
        }
    }

    /// `send` of a SCO packet (module docs)
    fn send(&mut self, packet: &[u8], upstream: &Upstream) {
        if !self.running() || packet.len() < 3 || packet.len() > SCO_OUT_SLOT_LEN {
            self.dropped += 1;
            tracing::trace!("USB: SCO packet of {} bytes dropped (pipes running {})", packet.len(), self.running());
            upstream.packet_sent();
            return;
        }
        let s = &self.shared;
        let mut buf = s.free.lock().unwrap_or_else(|e| e.into_inner()).pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(packet);
        // Counted before the thread can see it (it gives the credit back)
        let room = s.credits.take();
        s.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(buf);
        s.wake.set();
        // Copied: BTstack may reuse its buffer
        upstream.packet_sent();
        if room {
            sco_can_send_now(upstream);
        }
    }
}

/// BTstack's SCO send token (USB SCO mode)
fn sco_can_send_now(upstream: &Upstream) {
    upstream.deliver(HCI_EVENT_PACKET, &[HCI_EVENT_SCO_CAN_SEND_NOW, 0]);
}

/// The SCO IN stream: the reads in the order they were submitted (that is
/// the order they complete in), the slots whose resubmit failed, and which
/// reads follow a gap in the stream
struct ScoReader {
    input: IsochIn,
    order: VecDeque<usize>,
    idle: Vec<usize>,
    /// Submitted as soon as possible rather than right after the read
    /// before: data between the two was not read
    gap_before: Vec<bool>,
}

impl ScoReader {
    fn new(input: IsochIn) -> ScoReader {
        let n = input.transfers();
        ScoReader { input, order: VecDeque::with_capacity(n), idle: (0..n).collect(), gap_before: vec![false; n] }
    }

    /// Submits the idle reads: right after the last one in flight, or, with
    /// none in flight (the start, or the stream broke), as soon as possible.
    /// Error: device gone.
    fn submit_idle(&mut self) -> Result<(), Win32Error> {
        while let Some(&slot) = self.idle.first() {
            let follow = !self.order.is_empty();
            let mut result = self.input.submit(slot, follow);
            let mut gap = false;
            if result.is_err() && follow {
                // The stream cannot be continued (a read came back late):
                // start it again behind the reads in flight
                result = self.input.submit(slot, false);
                gap = true;
            }
            match result {
                Ok(()) => {
                    self.idle.remove(0);
                    self.order.push_back(slot);
                    self.gap_before[slot] = gap;
                }
                Err(e) => {
                    tracing::trace!("USB: SCO read {slot} could not be submitted: error {e}");
                    return if sco_device_gone(e) { Err(e) } else { Ok(()) };
                }
            }
        }
        Ok(())
    }

    /// The read to wait for
    fn oldest(&self) -> Option<usize> {
        self.order.front().copied()
    }

    /// The oldest read, if it has completed: its slot (idle again), its
    /// result and whether the stream had a gap before it
    fn complete(&mut self) -> Option<(usize, Result<(), Win32Error>, bool)> {
        let slot = self.oldest()?;
        let result = self.input.complete(slot)?;
        self.order.pop_front();
        self.idle.push(slot);
        Some((slot, result, std::mem::take(&mut self.gap_before[slot])))
    }
}

/// SCO counters since the last debug summary
#[derive(Default)]
struct ScoStats {
    in_packets: u64,
    in_bytes: u64,
    in_errors: u64,
    bad_iso_packets: u64,
    out_packets: u64,
    out_errors: u64,
}

/// The "usb-sco" thread (module docs). Waits for a queued packet, the
/// oldest read and the oldest write, in that order of the wait list.
fn sco_loop(input: IsochIn, mut output: IsochOut, out_packet_len: usize, shared: &ScoShared, upstream: &Upstream) {
    let mut sco = Reassembler::new(HCI_SCO_DATA_PACKET);
    let mut reader = ScoReader::new(input);
    let n_out = output.slots();
    let (mut out_next, mut out_oldest, mut out_in_flight) = (0usize, 0usize, 0usize);
    let mut in_errors_in_a_row = 0u32;
    let mut stats = ScoStats::default();
    let mut stats_at = Instant::now();
    let mut logged_out_layout = false;
    tracing::debug!("USB: SCO thread started ({} reads, {n_out} write slots)", reader.idle.len());
    'run: while !shared.stop.load(Ordering::Acquire) {
        if stats_at.elapsed() >= STATS_EVERY {
            tracing::debug!(
                "USB: SCO last {:.1} s: in {} packets ({} B), out {}, errors in {} out {}, failed isochronous packets {}",
                stats_at.elapsed().as_secs_f32(),
                stats.in_packets,
                stats.in_bytes,
                stats.out_packets,
                stats.in_errors,
                stats.out_errors,
                stats.bad_iso_packets
            );
            stats = ScoStats::default();
            stats_at = Instant::now();
        }
        // Reads: every slot in flight
        if let Err(e) = reader.submit_idle() {
            tracing::debug!("USB: SCO read: error {e}, the adapter is gone");
            break;
        }
        // Queued packets into free write slots (they complete in slot order)
        while out_in_flight < n_out {
            let Some(packet) = shared.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() else { break };
            if !logged_out_layout {
                logged_out_layout = true;
                tracing::debug!(
                    "USB: SCO out: {}-byte packets, {} isochronous packet(s) of {out_packet_len} B each",
                    packet.len(),
                    sco_out_packets(packet.len(), out_packet_len)
                );
            }
            let result = output.submit(out_next, &packet, out_in_flight > 0);
            shared.free.lock().unwrap_or_else(|e| e.into_inner()).push(packet);
            match result {
                Ok(()) => {
                    out_in_flight += 1;
                    out_next = (out_next + 1) % n_out;
                }
                Err(e) => {
                    // The slot stays free for the next packet
                    stats.out_errors += 1;
                    tracing::debug!("USB: SCO write could not be submitted: error {e}, packet dropped");
                    if shared.credits.give() {
                        sco_can_send_now(upstream);
                    }
                    if sco_device_gone(e) {
                        break 'run;
                    }
                }
            }
        }
        let (woken, in_index, out_index) = {
            let mut handles = vec![shared.wake.wait_handle()];
            let in_index = reader.oldest().map(|slot| {
                handles.push(reader.input.wait_handle(slot));
                handles.len() - 1
            });
            let out_index = (out_in_flight > 0).then(|| {
                handles.push(output.wait_handle(out_oldest));
                handles.len() - 1
            });
            (usb_isoch::wait_any(&handles, POLL), in_index, out_index)
        };
        let Some(woken) = woken else { continue };
        if Some(woken) == in_index {
            let Some((slot, result, gap)) = reader.complete() else { continue };
            if gap {
                sco.reset();
            }
            match result {
                Ok(()) => {
                    in_errors_in_a_row = 0;
                    let (mut n, mut bytes) = (0u64, 0u64);
                    let failed = sco_in_transfer(&mut sco, reader.input.packets(slot), |p| {
                        n += 1;
                        bytes += p.len() as u64;
                        upstream.deliver(HCI_SCO_DATA_PACKET, p)
                    });
                    stats.in_packets += n;
                    stats.in_bytes += bytes;
                    stats.bad_iso_packets += failed as u64;
                }
                Err(_) if shared.stop.load(Ordering::Acquire) => break,
                Err(e) => {
                    stats.in_errors += 1;
                    in_errors_in_a_row += 1;
                    sco.reset();
                    if sco_device_gone(e) {
                        tracing::debug!("USB: SCO read: error {e}, the adapter is gone");
                        break;
                    }
                    if in_errors_in_a_row == SCO_IN_WARN_ERRORS {
                        tracing::warn!(target: "hci", "USB: SCO reads keep failing (error {e})");
                    } else {
                        tracing::trace!("USB: SCO read {slot} failed: error {e}");
                    }
                }
            }
        } else if Some(woken) == out_index {
            let Some(result) = output.complete(out_oldest) else { continue };
            out_in_flight -= 1;
            out_oldest = (out_oldest + 1) % n_out;
            match result {
                Ok(_) => stats.out_packets += 1,
                Err(_) if shared.stop.load(Ordering::Acquire) => break,
                Err(e) => {
                    stats.out_errors += 1;
                    tracing::trace!("USB: SCO write failed: error {e}");
                    if sco_device_gone(e) {
                        tracing::debug!("USB: SCO write: error {e}, the adapter is gone");
                        break;
                    }
                }
            }
            if shared.credits.give() && shared.running.load(Ordering::Acquire) {
                sco_can_send_now(upstream);
            }
        }
    }
    shared.running.store(false, Ordering::Release);
    let (in_ok, out_ok) = (reader.input.abort(), output.abort());
    tracing::debug!(
        "USB: SCO thread stopped (stop flag {}, every transfer returned: in {in_ok}, out {out_ok})",
        shared.stop.load(Ordering::Acquire)
    );
}

pub struct UsbTransport {
    selector: UsbSelector,
    opened: Option<Opened>,
    /// SCO over interface 1 wanted (the stack offers voice): an H2
    /// transport, interface 1 claimed at open
    sco: bool,
}

impl UsbTransport {
    pub fn new(selector: UsbSelector) -> UsbTransport {
        UsbTransport { selector, opened: None, sco: false }
    }

    /// SCO audio over interface 1 (module docs, SCO); off by default. Set
    /// before the transport is installed: BTstack reads `is_h2` then.
    pub fn with_sco(mut self, sco: bool) -> UsbTransport {
        self.sco = sco;
        self
    }

    /// The WinUSB adapters the selector takes, in enumeration order
    fn find(&self) -> Result<Vec<DeviceInfo>, String> {
        let devices: Vec<DeviceInfo> = nusb::list_devices()
            .wait()
            .map_err(|e| format!("cannot list USB devices: {e}"))?
            .filter(|d| !is_vmware(d))
            .collect();
        let mut found = Vec::new();
        let mut wrong_driver = None;
        for info in devices {
            let c = candidate(&info, &self.selector.known);
            let takes = self.selector.takes(&c);
            tracing::debug!(
                "USB: {:04X}:{:04X} {} driver {} bluetooth class {} known {} product {:?}: {}",
                c.vid,
                c.pid,
                c.instance_id,
                c.driver.as_deref().unwrap_or("(none)"),
                c.bluetooth_class,
                c.known,
                c.product,
                match (takes, c.winusb()) {
                    (false, _) => "not selected",
                    (true, true) => "selected",
                    (true, false) => "selected, not on WinUSB",
                }
            );
            if !takes {
                continue;
            }
            if c.winusb() {
                found.push(info);
            } else {
                wrong_driver.get_or_insert(c);
            }
        }
        if !found.is_empty() {
            if let Some(p) = &self.selector.preferred {
                // Stable: the others keep their enumeration order
                found.sort_by_key(|info| !info.instance_id().to_string_lossy().eq_ignore_ascii_case(p));
            }
            return Ok(found);
        }
        Err(match wrong_driver {
            Some(c) => format!(
                "adapter {:04X}:{:04X} uses driver {} instead of WinUSB",
                c.vid,
                c.pid,
                c.driver.as_deref().unwrap_or("(none)")
            ),
            None => "no Bluetooth adapter with the WinUSB driver found".into(),
        })
    }
}

const MEDIATEK_VID: u16 = 0x0E8D;

/// GET_STATUS of an endpoint: halted, or no answer
fn halted(interface: &Interface, addr: u8) -> bool {
    let status = interface
        .control_in(
            ControlIn {
                control_type: ControlType::Standard,
                recipient: Recipient::Endpoint,
                request: 0x00,
                value: 0,
                index: addr.into(),
                length: 2,
            },
            COMMAND_TIMEOUT,
        )
        .wait();
    match status {
        Ok(s) => s.first().is_some_and(|b| b & 1 != 0),
        Err(e) => {
            tracing::debug!("USB: GET_STATUS of endpoint 0x{addr:02x}: {e}");
            true
        }
    }
}

/// Endpoint addresses of the HCI pipes on interface 0
fn hci_endpoints(interface: &Interface) -> Option<(u8, u8, u8)> {
    let desc = interface.descriptor()?;
    let (mut event_in, mut acl_in, mut acl_out) = (None, None, None);
    for ep in desc.endpoints() {
        let addr = ep.address();
        match (ep.transfer_type(), addr & 0x80 != 0) {
            (TransferType::Interrupt, true) => event_in = event_in.or(Some(addr)),
            (TransferType::Bulk, true) => acl_in = acl_in.or(Some(addr)),
            (TransferType::Bulk, false) => acl_out = acl_out.or(Some(addr)),
            _ => {}
        }
    }
    Some((event_in?, acl_in?, acl_out?))
}

/// The device's descriptors as the debug log shows them: speed, class and
/// each interface's alternate settings with their endpoints (interface 1,
/// SCO, has its isochronous alternate settings there)
fn log_layout(device: &Device) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let d = device.device_descriptor();
    tracing::debug!(
        "USB: device USB {:04X}, class {:02X}/{:02X}/{:02X}, ep0 max packet {}, version {:04X}, speed {:?}, {} configuration(s)",
        d.usb_version(),
        d.class(),
        d.subclass(),
        d.protocol(),
        d.max_packet_size_0(),
        d.device_version(),
        device.speed(),
        d.num_configurations()
    );
    let config = match device.active_configuration() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("USB: no active configuration: {e}");
            return;
        }
    };
    tracing::debug!(
        "USB: configuration {}, {} interface(s), attributes 0x{:02X}, max power {}",
        config.configuration_value(),
        config.num_interfaces(),
        config.attributes(),
        config.max_power()
    );
    for alt in config.interface_alt_settings() {
        tracing::debug!(
            "USB: interface {} alt {}: class {:02X}/{:02X}/{:02X}, {} endpoint(s)",
            alt.interface_number(),
            alt.alternate_setting(),
            alt.class(),
            alt.subclass(),
            alt.protocol(),
            alt.num_endpoints()
        );
        for ep in alt.endpoints() {
            tracing::debug!(
                "USB:   endpoint 0x{:02x} {:?} {:?}, max packet {} ({} per microframe), interval {}",
                ep.address(),
                ep.transfer_type(),
                ep.direction(),
                ep.max_packet_size(),
                ep.packets_per_microframe(),
                ep.interval()
            );
        }
    }
}

fn send_command(interface: &Interface, command: &[u8], timeout: Duration) -> Result<(), TransferError> {
    interface
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Device,
                request: 0,
                value: 0,
                index: 0,
                data: command,
            },
            timeout,
        )
        .wait()
}

/// Errors after which the device handle no longer reaches the adapter, as
/// v1's usb_device_gone: ERROR_DEVICE_NOT_CONNECTED, ERROR_NO_SUCH_DEVICE
/// and ERROR_FILE_NOT_FOUND (nusb's Disconnected), ERROR_BAD_COMMAND (22)
/// and ERROR_INVALID_HANDLE (6)
fn device_gone(e: &TransferError) -> bool {
    matches!(e, TransferError::Disconnected | TransferError::Unknown(22) | TransferError::Unknown(6))
}

/// The descriptors Windows keeps for the device (no I/O), with one line per
/// interface setting
fn descriptors(device: &Device) -> UsbDescriptors {
    let mut summary = Vec::new();
    let configurations = device
        .configurations()
        .map(|c| {
            for alt in c.interface_alt_settings() {
                let endpoints: Vec<String> = alt
                    .endpoints()
                    .map(|e| {
                        let kind = match e.transfer_type() {
                            TransferType::Control => "control",
                            TransferType::Isochronous => "isochronous",
                            TransferType::Bulk => "bulk",
                            TransferType::Interrupt => "interrupt",
                        };
                        format!("0x{:02X} {kind} {}", e.address(), e.max_packet_size())
                    })
                    .collect();
                summary.push(format!(
                    "interface {} alt {} class {:02X}/{:02X}/{:02X}: {}",
                    alt.interface_number(),
                    alt.alternate_setting(),
                    alt.class(),
                    alt.subclass(),
                    alt.protocol(),
                    if endpoints.is_empty() { "no endpoints".to_owned() } else { endpoints.join(", ") }
                ));
            }
            c.as_bytes().to_vec()
        })
        .collect();
    UsbDescriptors { device: device.device_descriptor().as_bytes().to_vec(), configurations, summary }
}

impl HciBackend for UsbTransport {
    fn is_async(&self) -> bool {
        true
    }

    fn open(&mut self, upstream: Upstream) -> Result<(), String> {
        self.close();
        // The first adapter that opens: one another program holds is
        // skipped, as v1's usb_open tried every device
        let mut last_error = String::new();
        let mut opened = None;
        tracing::debug!("USB: opening an adapter ({:?})", self.selector);
        for info in self.find()? {
            let (vid, pid) = (info.vendor_id(), info.product_id());
            tracing::debug!(
                "USB: trying {vid:04X}:{pid:04X} {} (bus {:?}, port chain {:?}, driver {:?})",
                info.instance_id().to_string_lossy(),
                info.bus_id(),
                info.port_chain(),
                info.driver()
            );
            let attempt =
                info.open().wait().map_err(|e| format!("cannot open {vid:04X}:{pid:04X}: {e}")).and_then(|device| {
                    log_layout(&device);
                    let interface = device
                        .claim_interface(0)
                        .wait()
                        .map_err(|e| {
                            // Seen right after WinUSB was assigned while
                            // Windows still had the old driver loaded (issue
                            // #81: "failed to initialize WinUSB (error 50)")
                            format!(
                                "cannot claim interface 0 of {vid:04X}:{pid:04X}: {e} (if WinUSB was just assigned: replug the adapter or restart Windows)"
                            )
                        })?;
                    let endpoints = hci_endpoints(&interface)
                        .ok_or_else(|| format!("{vid:04X}:{pid:04X} lacks the HCI event / ACL endpoints"))?;
                    Ok((device, interface, endpoints))
                });
            match attempt {
                Ok(o) => {
                    opened = Some((info, o));
                    break;
                }
                Err(e) => {
                    tracing::warn!(target: "hci", "USB: {e}; trying the next adapter, if any");
                    last_error = e;
                }
            }
        }
        let Some((info, (device, interface, (event_addr, acl_in_addr, acl_out_addr)))) = opened else {
            return Err(last_error);
        };
        let (vid, pid) = (info.vendor_id(), info.product_id());
        *LAST_OPENED.lock().unwrap_or_else(|e| e.into_inner()) = Some((vid, pid));
        let mut event_in = interface.endpoint::<Interrupt, In>(event_addr).map_err(|e| e.to_string())?;
        let mut acl_in = interface.endpoint::<Bulk, In>(acl_in_addr).map_err(|e| e.to_string())?;
        let mut acl_out = interface.endpoint::<Bulk, Out>(acl_out_addr).map_err(|e| e.to_string())?;
        tracing::debug!(
            "USB: interface 0 alt {}: event in 0x{event_addr:02x} (max packet {}), ACL in 0x{acl_in_addr:02x} (max packet {}), \
             ACL out 0x{acl_out_addr:02x} (max packet {}); pipe policies: nusb's (RAW_IO on IN pipes, no control pipe timeout)",
            interface.get_alt_setting(),
            event_in.max_packet_size(),
            acl_in.max_packet_size(),
            acl_out.max_packet_size()
        );
        // A halted endpoint or a data toggle left from an earlier session
        // keeps the controller from answering HCI Reset
        // A MediaTek controller seems to keep its data toggle on a clear: a
        // clear of a running pipe cost the first event after a restart
        // (#86). Its pipes are cleared only when halted.
        let keep = |addr: u8| vid == MEDIATEK_VID && !halted(&interface, addr);
        for (name, r) in [
            ("event", (!keep(event_addr)).then(|| event_in.clear_halt().wait())),
            ("ACL in", (!keep(acl_in_addr)).then(|| acl_in.clear_halt().wait())),
            ("ACL out", (!keep(acl_out_addr)).then(|| acl_out.clear_halt().wait())),
        ] {
            match r {
                Some(Err(e)) => {
                    tracing::warn!(target: "hci", "USB: clearing the {name} pipe failed: {e}");
                    tracing::debug!("USB: clear halt on the {name} pipe: {e:?}");
                }
                Some(Ok(())) => tracing::debug!("USB: {name} pipe cleared"),
                None => tracing::debug!("USB: {name} pipe not halted, left as it is"),
            }
        }
        // Interface 1 only when the stack offers voice
        let sco = if self.sco { Sco::claim(&device) } else { None };
        STATS.take();
        tracing::info!(
            target: "hci",
            "USB: opened {vid:04X}:{pid:04X} {} (event 0x{event_addr:02x}, ACL 0x{acl_in_addr:02x}/0x{acl_out_addr:02x})",
            info.instance_id().to_string_lossy()
        );

        let stop = Arc::new(AtomicBool::new(false));
        let bootloader = Arc::new(AtomicBool::new(false));
        let command_in_flight = Arc::new(AtomicBool::new(false));
        let (command_tx, command_rx) = mpsc::channel::<(u8, Vec<u8>)>();
        let (command_recycle_tx, command_recycle_rx) = mpsc::channel::<Vec<u8>>();
        let (acl_tx, acl_rx) = mpsc::channel::<Buffer>();
        let (acl_recycle_tx, acl_recycle_rx) = mpsc::channel::<Buffer>();
        let mut threads = Vec::new();
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new().name(name.into()).spawn(f).map_err(|e| e.to_string())
        };
        {
            let (stop, upstream) = (stop.clone(), upstream.clone());
            threads.push(spawn("usb-event-in", Box::new(move || event_loop(&mut event_in, &stop, &upstream)))?);
        }
        {
            let (stop, upstream, bootloader) = (stop.clone(), upstream.clone(), bootloader.clone());
            threads
                .push(spawn("usb-acl-in", Box::new(move || acl_in_loop(&mut acl_in, &stop, &bootloader, &upstream)))?);
        }
        {
            let (stop, interface, in_flight) = (stop.clone(), interface.clone(), command_in_flight.clone());
            let upstream = upstream.clone();
            threads.push(spawn(
                "usb-cmd-out",
                Box::new(move || {
                    let write = |_: u8, p: &[u8]| send_command(&interface, p, COMMAND_TIMEOUT);
                    writer_loop(write, command_rx, command_recycle_tx, &in_flight, &stop, &upstream)
                }),
            )?);
        }
        let acl_writer = {
            let (stop, upstream) = (stop.clone(), upstream.clone());
            let t = spawn(
                "usb-acl-out",
                Box::new(move || {
                    // Media data has a deadline: a busy PC must not hold
                    // the writer back
                    let _priority = crate::acl_out_priority();
                    acl_writer_loop(&mut acl_out, acl_rx, acl_recycle_tx, &stop, &upstream)
                }),
            )?;
            let thread = t.thread().clone();
            threads.push(t);
            thread
        };
        self.opened = Some(Opened {
            interface,
            event_addr,
            device,
            stop,
            command_tx,
            command_recycle_rx,
            command_in_flight,
            acl_tx,
            acl_recycle_rx,
            acl_writer,
            upstream,
            bootloader,
            threads,
            sco,
        });
        Ok(())
    }

    fn close(&mut self) {
        let Some(mut opened) = self.opened.take() else { return };
        tracing::debug!("USB: closing; stopping {} I/O thread(s)", opened.threads.len());
        let started = Instant::now();
        // Before interface 1 goes: its buffers are unregistered once the
        // SCO transfers have come back
        if let Some(sco) = opened.sco.as_mut() {
            sco.stop();
        }
        opened.stop.store(true, Ordering::Release);
        // The command writer finishes the command it has (bounded by its
        // timeout); the ACL writer cancels its transfers
        drop(opened.command_tx);
        drop(opened.acl_tx);
        opened.acl_writer.unpark();
        for t in opened.threads {
            let name = t.thread().name().unwrap_or("?").to_owned();
            if t.join().is_err() {
                tracing::debug!("USB: thread {name} panicked");
            }
        }
        tracing::debug!("USB: I/O threads stopped in {:?}", started.elapsed());
        // Leave the controller idle (no links, no scans, no queued events)
        // for the next session, as Linux does with HCI_QUIRK_RESET_ON_CLOSE;
        // not a bootloader (module docs)
        if opened.bootloader.load(Ordering::Acquire) {
            tracing::info!(target: "hci", "USB: controller in its bootloader, no HCI Reset on close");
        } else {
            match send_command(&opened.interface, &[0x03, 0x0C, 0x00], CLOSE_RESET_TIMEOUT) {
                Ok(()) => {
                    let at = Instant::now();
                    match reset_completed(&opened.interface, opened.event_addr, CLOSE_RESET_WAIT) {
                        Ok(()) => tracing::info!(
                            target: "hci",
                            "USB: HCI Reset on close done in {} ms",
                            at.elapsed().as_millis()
                        ),
                        Err(e) => tracing::warn!(target: "hci", "USB: HCI Reset sent on close, but {e}"),
                    }
                }
                Err(e) => tracing::warn!(target: "hci", "USB: HCI Reset on close failed: {e}"),
            }
        }
        tracing::info!(target: "hci", "USB: closed");
    }

    fn set_bootloader(&mut self, on: bool) {
        if let Some(o) = &self.opened {
            tracing::debug!(
                "USB: bootloader {on}: the bulk IN pipe carries {}",
                if on { "events" } else { "ACL data" }
            );
            o.bootloader.store(on, Ordering::Release);
        }
    }

    fn control(&mut self, request: &ControlRequest) -> Result<Vec<u8>, String> {
        let Some(o) = &self.opened else { return Err("the adapter is not open".into()) };
        let control_type = match request.kind {
            ControlKind::Standard => ControlType::Standard,
            ControlKind::Class => ControlType::Class,
            ControlKind::Vendor => ControlType::Vendor,
        };
        let recipient = match request.recipient {
            ControlRecipient::Device => Recipient::Device,
            ControlRecipient::Interface => Recipient::Interface,
            ControlRecipient::Endpoint => Recipient::Endpoint,
            ControlRecipient::Other => Recipient::Other,
        };
        let result = match &request.data {
            ControlData::In(length) => o
                .interface
                .control_in(
                    ControlIn {
                        control_type,
                        recipient,
                        request: request.request,
                        value: request.value,
                        index: request.index,
                        length: *length,
                    },
                    COMMAND_TIMEOUT,
                )
                .wait(),
            ControlData::Out(data) => o
                .interface
                .control_out(
                    ControlOut {
                        control_type,
                        recipient,
                        request: request.request,
                        value: request.value,
                        index: request.index,
                        data,
                    },
                    COMMAND_TIMEOUT,
                )
                .wait()
                .map(|()| Vec::new()),
        };
        result.map_err(|e| format!("{e:?}"))
    }

    fn usb_descriptors(&mut self) -> Option<UsbDescriptors> {
        let o = self.opened.as_ref()?;
        Some(descriptors(&o.device))
    }

    fn is_h2(&self) -> bool {
        // Decided before open: with SCO wanted (module docs, SCO)
        self.sco
    }

    fn sco_ready(&self) -> bool {
        self.opened.as_ref().and_then(|o| o.sco.as_ref()).is_some_and(Sco::running)
    }

    fn set_sco_config(&mut self, voice_setting: u16, connections: u8) {
        let Some(o) = &mut self.opened else { return };
        match &mut o.sco {
            Some(sco) => sco.configure(voice_setting, connections, &o.upstream),
            None if connections > 0 => {
                tracing::warn!(target: "hci", "USB: SCO link up, but the adapter's SCO interface is not available")
            }
            None => {}
        }
    }

    fn can_send(&mut self, packet_type: u8) -> bool {
        let Some(o) = &self.opened else { return false };
        match out_slot(packet_type) {
            Some(0) => !o.command_in_flight.load(Ordering::Acquire),
            Some(1) => true,
            Some(_) => o.sco.as_ref().is_some_and(|s| s.running() && s.shared.credits.room()),
            None => false,
        }
    }

    fn send(&mut self, packet_type: u8, packet: &[u8]) -> bool {
        let Some(o) = &mut self.opened else { return false };
        match out_slot(packet_type) {
            Some(0) => {
                tracing::trace!("USB: command 0x{:04X} out, {} bytes", le16(packet), packet.len());
                let mut buf = o.command_recycle_rx.try_recv().unwrap_or_default();
                buf.clear();
                buf.extend_from_slice(packet);
                o.command_in_flight.store(true, Ordering::Release);
                if o.command_tx.send((packet_type, buf)).is_err() {
                    o.command_in_flight.store(false, Ordering::Release);
                    return false;
                }
                true
            }
            Some(1) => {
                tracing::trace!(
                    "USB: {} out, handle 0x{:03X}, {} bytes",
                    if packet_type == HCI_ISO_DATA_PACKET { "ISO" } else { "ACL" },
                    le16(packet) & 0x0FFF,
                    packet.len()
                );
                let mut buf = o
                    .acl_recycle_rx
                    .try_recv()
                    .ok()
                    .filter(|b| b.capacity() >= packet.len())
                    .unwrap_or_else(|| Buffer::new(packet.len().max(ACL_TRANSFER_SIZE)));
                buf.clear();
                buf.extend_from_slice(packet);
                if o.acl_tx.send(buf).is_err() {
                    return false;
                }
                o.acl_writer.unpark();
                // Copied: BTstack may reuse its buffer and send the next one
                o.upstream.packet_sent();
                true
            }
            Some(_) => {
                match &mut o.sco {
                    Some(sco) => sco.send(packet, &o.upstream),
                    // Dropped, BTstack's buffer freed (module docs)
                    None => o.upstream.packet_sent(),
                }
                true
            }
            None => false,
        }
    }
}

impl Drop for UsbTransport {
    fn drop(&mut self) {
        self.close();
    }
}

/// The first two bytes little-endian (a command's opcode, an ACL / ISO
/// packet's handle and flags); 0 if shorter
fn le16(p: &[u8]) -> u16 {
    match p {
        [a, b, ..] => u16::from_le_bytes([*a, *b]),
        _ => 0,
    }
}

/// Reads the event pipe (the event reader has stopped) until the Command
/// Complete of HCI Reset; other events are dropped, the session is over.
/// Err: why it was not seen
fn reset_completed(interface: &Interface, event_addr: u8, wait: Duration) -> Result<(), String> {
    let mut ep = interface
        .endpoint::<Interrupt, In>(event_addr)
        .map_err(|e| format!("the event pipe 0x{event_addr:02x} is not available: {e}"))?;
    let deadline = Instant::now() + wait;
    let mut events = Reassembler::new(HCI_EVENT_PACKET);
    let mut result = Err(format!("no Command Complete within {} ms", wait.as_millis()));
    submit_in(&mut ep, EVENT_TRANSFER_SIZE);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Some(c) = ep.wait_next_complete(left) else { break };
        if let Err(e) = c.status {
            result = Err(format!("the event pipe failed: {e:?}"));
            break;
        }
        // Command Complete: 0x0E, length, command credits, opcode 0x0C03
        let mut done = false;
        events.push(&c.buffer[..c.actual_len], |p| done |= p.len() >= 5 && p[0] == 0x0E && p[3..5] == [0x03, 0x0C]);
        if done {
            result = Ok(());
            break;
        }
        submit_in(&mut ep, EVENT_TRANSFER_SIZE);
    }
    drain(&mut ep);
    result
}

fn submit_in<T: nusb::transfer::BulkOrInterrupt>(ep: &mut Endpoint<T, In>, size: usize) {
    // IN transfers must be a multiple of the max packet size
    let mps = ep.max_packet_size().max(1);
    let len = size.div_ceil(mps) * mps;
    ep.submit(Buffer::new(len));
}

fn drain<T: nusb::transfer::BulkOrInterrupt>(ep: &mut Endpoint<T, In>) {
    tracing::debug!("USB: cancelling {} IN transfer(s) on 0x{:02x}", ep.pending(), ep.endpoint_address());
    ep.cancel_all();
    while ep.pending() > 0 {
        if ep.wait_next_complete(Duration::from_secs(1)).is_none() {
            tracing::warn!(target: "hci", "USB: a cancelled IN transfer did not return");
            break;
        }
    }
}

fn event_loop(ep: &mut Endpoint<Interrupt, In>, stop: &AtomicBool, upstream: &Upstream) {
    let mut events = Reassembler::new(HCI_EVENT_PACKET);
    let mut errors = 0u32;
    let addr = ep.endpoint_address();
    tracing::debug!("USB: event reader started (0x{addr:02x}, {IN_TRANSFERS} x {EVENT_TRANSFER_SIZE} B)");
    let mut stats_at = Instant::now();
    for _ in 0..IN_TRANSFERS {
        submit_in(ep, EVENT_TRANSFER_SIZE);
    }
    while !stop.load(Ordering::Acquire) {
        if stats_at.elapsed() >= STATS_EVERY {
            STATS.log(stats_at.elapsed());
            stats_at = Instant::now();
        }
        let Some(c) = ep.wait_next_complete(POLL) else { continue };
        match c.status {
            Ok(()) => {
                errors = 0;
                events.push(&c.buffer[..c.actual_len], |p| {
                    tracing::trace!(
                        "USB: event 0x{:02X} in on 0x{addr:02x}, {} bytes",
                        p.first().copied().unwrap_or(0),
                        p.len()
                    );
                    Stats::add(&STATS.events_in, 1);
                    Stats::add(&STATS.events_in_bytes, p.len());
                    upstream.deliver(HCI_EVENT_PACKET, p)
                });
            }
            Err(TransferError::Cancelled) if stop.load(Ordering::Acquire) => break,
            Err(e) if device_gone(&e) => {
                tracing::debug!("USB: event read on 0x{addr:02x}: {e:?}, the adapter is gone");
                upstream.fail(&format!("HCI event read: {e}"));
                break;
            }
            Err(e) => {
                // One bad read must not end event delivery for the session
                errors += 1;
                Stats::add(&STATS.in_errors, 1);
                tracing::debug!("USB: event read on 0x{addr:02x} failed: {e:?} ({} bytes)", c.actual_len);
                events.reset();
                if errors > EVENT_IN_MAX_ERRORS {
                    upstream.fail(&format!("HCI event read failed {errors} times in a row: {e}"));
                    break;
                }
                tracing::warn!(target: "hci", "USB: HCI event read failed ({e}), resetting the pipe ({errors}/{EVENT_IN_MAX_ERRORS})");
                drain(ep);
                if let Err(e) = ep.clear_halt().wait() {
                    tracing::warn!(target: "hci", "USB: clearing the event pipe failed: {e}");
                } else {
                    tracing::debug!("USB: event pipe 0x{addr:02x} cleared, reading again");
                }
                for _ in 0..IN_TRANSFERS {
                    submit_in(ep, EVENT_TRANSFER_SIZE);
                }
                continue;
            }
        }
        submit_in(ep, EVENT_TRANSFER_SIZE);
    }
    drain(ep);
    STATS.log(stats_at.elapsed());
    tracing::debug!("USB: event reader stopped (stop flag {})", stop.load(Ordering::Acquire));
}

/// `bootloader`: what comes in is HCI events ([`HciBackend::set_bootloader`])
fn acl_in_loop(ep: &mut Endpoint<Bulk, In>, stop: &AtomicBool, bootloader: &AtomicBool, upstream: &Upstream) {
    let mut acl = Reassembler::new(HCI_ACL_DATA_PACKET);
    let mut events = Reassembler::new(HCI_EVENT_PACKET);
    let mut events_before = false;
    let mut errors = 0u32;
    let addr = ep.endpoint_address();
    tracing::debug!("USB: ACL reader started (0x{addr:02x}, {IN_TRANSFERS} x {ACL_TRANSFER_SIZE} B)");
    for _ in 0..IN_TRANSFERS {
        submit_in(ep, ACL_TRANSFER_SIZE);
    }
    while !stop.load(Ordering::Acquire) {
        let Some(c) = ep.wait_next_complete(POLL) else { continue };
        match c.status {
            Ok(()) if bootloader.load(Ordering::Acquire) => {
                errors = 0;
                if !events_before {
                    acl.reset();
                    events_before = true;
                }
                events.push(&c.buffer[..c.actual_len], |p| {
                    tracing::trace!("USB: event 0x{:02X} on the bulk IN pipe 0x{addr:02x}, {} bytes", p[0], p.len());
                    upstream.deliver(HCI_EVENT_PACKET, p)
                });
            }
            Ok(()) => {
                errors = 0;
                if events_before {
                    events.reset();
                    events_before = false;
                }
                acl.push(&c.buffer[..c.actual_len], |p| {
                    tracing::trace!(
                        "USB: ACL in on 0x{addr:02x}, handle 0x{:03X}, {} bytes",
                        le16(p) & 0x0FFF,
                        p.len()
                    );
                    Stats::add(&STATS.acl_in, 1);
                    Stats::add(&STATS.acl_in_bytes, p.len());
                    upstream.deliver(HCI_ACL_DATA_PACKET, p)
                });
            }
            Err(TransferError::Cancelled) if stop.load(Ordering::Acquire) => break,
            Err(e) if device_gone(&e) => {
                tracing::debug!("USB: ACL read on 0x{addr:02x}: {e:?}, the adapter is gone");
                upstream.fail(&format!("ACL read: {e}"));
                break;
            }
            Err(e) => {
                // As v1: reset the pipe and read on (a stall, a bad
                // transfer); only a run of failures gives up
                errors += 1;
                Stats::add(&STATS.in_errors, 1);
                tracing::debug!("USB: ACL read on 0x{addr:02x} failed: {e:?} ({} bytes)", c.actual_len);
                acl.reset();
                events.reset();
                if errors > ACL_IN_MAX_ERRORS {
                    upstream.fail(&format!("ACL read failed {errors} times in a row: {e}"));
                    break;
                }
                tracing::warn!(target: "hci", "USB: ACL read failed ({e}), resetting the pipe ({errors}/{ACL_IN_MAX_ERRORS})");
                drain(ep);
                if let Err(e) = ep.clear_halt().wait() {
                    tracing::warn!(target: "hci", "USB: clearing the ACL in pipe failed: {e}");
                } else {
                    tracing::debug!("USB: ACL in pipe 0x{addr:02x} cleared, reading again");
                }
                for _ in 0..IN_TRANSFERS {
                    submit_in(ep, ACL_TRANSFER_SIZE);
                }
                continue;
            }
        }
        submit_in(ep, ACL_TRANSFER_SIZE);
    }
    drain(ep);
    tracing::debug!("USB: ACL reader stopped (stop flag {})", stop.load(Ordering::Acquire));
}

/// What becomes of a failed write
#[derive(Debug, PartialEq, Eq)]
enum WriteFailure {
    /// The stack has to be restarted
    Fatal,
    /// Logged; the packet is lost and the stack goes on
    Dropped,
}

/// A lost command leaves BTstack waiting for its answer for good, so any
/// command failure is fatal. A lost ACL / ISO packet is not (L2CAP and the
/// media stream go on, as in v1): only a gone adapter is.
fn write_failure(packet_type: u8, e: &TransferError) -> WriteFailure {
    if packet_type == HCI_COMMAND_DATA_PACKET || device_gone(e) { WriteFailure::Fatal } else { WriteFailure::Dropped }
}

/// Wakes the ACL writer when one of its transfers completes
struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Writes ACL / ISO packets in the order BTstack hands them over, up to
/// [`ACL_OUT_TRANSFERS`] at a time. BTstack already had them as sent
/// ([`UsbTransport::send`]). Writes have no timeout, as v1's: a controller
/// may hold data for a while (a link going down, the air busy), which is not
/// a failure. The thread sleeps until a packet comes ([`UsbTransport::send`]
/// unparks it) or a transfer completes (the waker).
fn acl_writer_loop(
    ep: &mut Endpoint<Bulk, Out>,
    rx: mpsc::Receiver<Buffer>,
    recycle: mpsc::Sender<Buffer>,
    stop: &AtomicBool,
    upstream: &Upstream,
) {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut open = true;
    // Submit times of the transfers in flight (they complete in order)
    let mut submitted: VecDeque<Instant> = VecDeque::with_capacity(ACL_OUT_TRANSFERS);
    let addr = ep.endpoint_address();
    tracing::debug!("USB: ACL writer started (0x{addr:02x}, up to {ACL_OUT_TRANSFERS} in flight)");
    while !stop.load(Ordering::Acquire) {
        while open && ep.pending() < ACL_OUT_TRANSFERS {
            match rx.try_recv() {
                Ok(buf) => {
                    ep.submit(buf);
                    submitted.push_back(Instant::now());
                    ACL_OUT_PENDING_MAX.fetch_max(ep.pending() as u64, Ordering::Relaxed);
                    STATS.acl_out_pending_max.fetch_max(ep.pending() as u64, Ordering::Relaxed);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => open = false,
            }
        }
        if ep.pending() == 0 {
            if !open {
                break;
            }
            std::thread::park_timeout(POLL);
            continue;
        }
        let Poll::Ready(c) = ep.poll_next_complete(&mut cx) else {
            std::thread::park_timeout(POLL);
            continue;
        };
        let len = c.actual_len;
        let _ = recycle.send(c.buffer);
        if let Some(at) = submitted.pop_front() {
            ACL_OUT_LATENCY_US.fetch_max(at.elapsed().as_micros() as u64, Ordering::Relaxed);
            tracing::trace!("USB: ACL out on 0x{addr:02x} done, {len} bytes, {:?}, {:?}", at.elapsed(), c.status);
        }
        let Err(e) = c.status else {
            Stats::add(&STATS.acl_out, 1);
            Stats::add(&STATS.acl_out_bytes, len);
            continue;
        };
        Stats::add(&STATS.out_errors, 1);
        if stop.load(Ordering::Acquire) {
            break;
        }
        tracing::debug!("USB: ACL write on 0x{addr:02x} failed: {e:?} ({len} bytes, {} more pending)", ep.pending());
        if write_failure(HCI_ACL_DATA_PACKET, &e) == WriteFailure::Fatal {
            upstream.fail(&format!("ACL write: {e}"));
            break;
        }
        if matches!(e, TransferError::Stall) {
            // The writes behind it fail as well, and every later one until
            // the pipe is cleared
            let behind = cancel_out(ep, &recycle);
            submitted.clear();
            tracing::warn!(target: "hci", "USB: ACL write failed ({e}), {} packet(s) dropped", 1 + behind);
            if let Err(e) = ep.clear_halt().wait() {
                tracing::warn!(target: "hci", "USB: clearing the ACL out pipe failed: {e}");
            } else {
                tracing::debug!("USB: ACL out pipe 0x{addr:02x} cleared after a stall");
            }
        } else {
            tracing::warn!(target: "hci", "USB: ACL write failed ({e}), packet dropped");
        }
    }
    let cancelled = cancel_out(ep, &recycle);
    tracing::debug!("USB: ACL writer stopped ({cancelled} write(s) cancelled)");
}

/// Cancels the pending OUT transfers and waits for them; returns how many
/// were pending
fn cancel_out(ep: &mut Endpoint<Bulk, Out>, recycle: &mpsc::Sender<Buffer>) -> usize {
    let pending = ep.pending();
    ep.cancel_all();
    while ep.pending() > 0 {
        match ep.wait_next_complete(Duration::from_secs(1)) {
            Some(c) => {
                let _ = recycle.send(c.buffer);
            }
            None => {
                tracing::warn!(target: "hci", "USB: a cancelled ACL write did not return");
                break;
            }
        }
    }
    pending
}

/// Sends the commands in the order BTstack hands them over and reports each
/// as sent. Commands (control pipe) and ACL / ISO data (bulk OUT) have a
/// thread each, so data the controller holds back does not delay commands,
/// as in v1 where both were separate overlapped transfers.
fn writer_loop(
    mut write: impl FnMut(u8, &[u8]) -> Result<(), TransferError>,
    rx: mpsc::Receiver<(u8, Vec<u8>)>,
    recycle: mpsc::Sender<Vec<u8>>,
    in_flight: &AtomicBool,
    stop: &AtomicBool,
    upstream: &Upstream,
) {
    tracing::debug!("USB: command writer started");
    loop {
        let (packet_type, packet) = match rx.recv_timeout(POLL) {
            Ok(p) => p,
            Err(mpsc::RecvTimeoutError::Timeout) if !stop.load(Ordering::Acquire) => continue,
            Err(_) => break,
        };
        let (head, len, started) = (le16(&packet), packet.len(), Instant::now());
        let result = write(packet_type, &packet);
        in_flight.store(false, Ordering::Release);
        let _ = recycle.send(packet);
        match result {
            Ok(()) => {
                tracing::trace!("USB: command 0x{head:04X} written, {len} bytes, {:?}", started.elapsed());
                Stats::add(&STATS.commands_out, 1);
                upstream.packet_sent()
            }
            Err(e) => {
                let what = match packet_type {
                    HCI_COMMAND_DATA_PACKET => "HCI command",
                    HCI_ISO_DATA_PACKET => "ISO write",
                    _ => "ACL write",
                };
                Stats::add(&STATS.out_errors, 1);
                tracing::debug!(
                    "USB: {what} 0x{head:04X} ({len} bytes) on the control pipe failed after {:?}: {e:?}{}",
                    started.elapsed(),
                    if stop.load(Ordering::Acquire) { " (closing)" } else { "" }
                );
                if stop.load(Ordering::Acquire) {
                    break;
                }
                match write_failure(packet_type, &e) {
                    WriteFailure::Fatal => {
                        upstream.fail(&format!("{what}: {e}"));
                        if device_gone(&e) {
                            break;
                        }
                    }
                    WriteFailure::Dropped => {
                        tracing::warn!(target: "hci", "USB: {what} failed ({e}), packet dropped");
                    }
                }
                // Free BTstack's packet buffer anyway
                upstream.packet_sent();
            }
        }
    }
    tracing::debug!("USB: command writer stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_goes_through_the_acl_slot() {
        assert_eq!(out_slot(HCI_COMMAND_DATA_PACKET), Some(0));
        assert_eq!(out_slot(HCI_ACL_DATA_PACKET), Some(1));
        assert_eq!(out_slot(HCI_ISO_DATA_PACKET), Some(1));
        // SCO has the isochronous pipe; events are not sent
        assert_eq!(out_slot(HCI_SCO_DATA_PACKET), Some(2));
        assert_eq!(out_slot(HCI_EVENT_PACKET), None);
    }

    #[test]
    fn sco_alt_settings_as_btstack() {
        // 8-bit voice (CVSD 8-bit, transparent / mSBC): 1, 2, 3
        for (n, alt) in [(1, 1), (2, 2), (3, 3), (4, 3)] {
            assert_eq!(sco_alt_setting(0x0060 & !0x0020, n), Some(alt), "8-bit, {n} links");
            assert_eq!(sco_alt_setting(0x0003, n), Some(alt), "transparent, {n} links");
        }
        // 16-bit samples (0x0060: CVSD, 16-bit, 2's complement): 2, 4, 5
        for (n, alt) in [(1, 2), (2, 4), (3, 5), (7, 5)] {
            assert_eq!(sco_alt_setting(0x0060, n), Some(alt), "16-bit, {n} links");
        }
        assert_eq!(sco_alt_setting(0x0060, 0), None);
        assert_eq!(sco_alt_setting(0x0003, 0), None);
    }

    #[test]
    fn sco_packet_sizes_fit_btstack_payloads() {
        assert_eq!((0..=6).map(sco_packet_size).collect::<Vec<_>>(), [0, 9, 17, 25, 33, 49, 63]);
        assert_eq!(sco_packet_size(9), 63);
        // One 3 ms SCO packet is three full-speed packets: 3 + 24 bytes
        // (8-bit / transparent) at alt 1, 3 + 48 (16-bit) at alt 2
        assert_eq!(sco_out_packets(3 + 24, sco_packet_size(1)), 3);
        assert_eq!(sco_out_packets(3 + 48, sco_packet_size(2)), 3);
        assert_eq!(sco_out_packets(3 + 60, sco_packet_size(2)), 4);
        assert_eq!(sco_out_packets(10, 0), 10);
    }

    /// A SCO packet: handle 0x0101 (packet status flags 0), payload
    fn sco_packet(handle: u16, payload: &[u8]) -> Vec<u8> {
        let [lo, hi] = handle.to_le_bytes();
        [lo, hi, payload.len() as u8].into_iter().chain(payload.iter().copied()).collect()
    }

    fn feed(sco: &mut Reassembler, packets: &[Result<Vec<u8>, i32>]) -> (Vec<Vec<u8>>, usize) {
        let mut out = Vec::new();
        let failed =
            sco_in_transfer(sco, packets.iter().map(|p| p.as_deref().map_err(|e| *e)), |p| out.push(p.to_vec()));
        (out, failed)
    }

    #[test]
    fn sco_in_stream_split_over_iso_packets() {
        // Alt 2: 17-byte packets, one 51-byte SCO packet (16-bit, 3 ms) per
        // three of them, as a controller sends it
        let a = sco_packet(0x0101, &[0x11; 48]);
        let b = sco_packet(0x0101, &[0x22; 48]);
        let stream: Vec<u8> = a.iter().chain(&b).copied().collect();
        let iso: Vec<Result<Vec<u8>, i32>> = stream.chunks(17).map(|c| Ok(c.to_vec())).collect();
        let mut sco = Reassembler::new(HCI_SCO_DATA_PACKET);
        let (out, failed) = feed(&mut sco, &iso[..4]);
        assert_eq!((out, failed), (vec![a.clone()], 0));
        assert_eq!(sco.pending(), 17);
        let (out, _) = feed(&mut sco, &iso[4..]);
        assert_eq!(out, vec![b.clone()]);
        assert_eq!(sco.pending(), 0);
        // Not aligned to the isochronous packets, with empty packets between
        let mut iso: Vec<Result<Vec<u8>, i32>> = vec![Ok(stream[..5].to_vec()), Ok(Vec::new())];
        iso.extend(stream[5..].chunks(9).map(|c| Ok(c.to_vec())));
        iso.push(Ok(Vec::new()));
        let (out, failed) = feed(&mut sco, &iso);
        assert_eq!((out, failed), (vec![a, b], 0));
    }

    #[test]
    fn sco_in_failed_packet_drops_the_partial_one() {
        let a = sco_packet(0x0102, &[1; 24]);
        let b = sco_packet(0x0102, &[2; 24]);
        let mut sco = Reassembler::new(HCI_SCO_DATA_PACKET);
        // a's first 9 bytes, a failed packet (its middle lost), then b whole
        let iso = vec![Ok(a[..9].to_vec()), Err(-1), Ok(b[..9].to_vec()), Ok(b[9..18].to_vec()), Ok(b[18..].to_vec())];
        let (out, failed) = feed(&mut sco, &iso);
        assert_eq!((out, failed), (vec![b], 1));
        assert_eq!(sco.pending(), 0);
        // A header-only packet (no payload) is a packet as well
        let (out, _) = feed(&mut sco, &[Ok(vec![0x02, 0x01, 0])]);
        assert_eq!(out, vec![vec![0x02, 0x01, 0]]);
    }

    #[test]
    fn sco_credits_hand_out_the_token_while_there_is_room() {
        let c = ScoCredits::new(3);
        assert!(c.room());
        assert!(c.take());
        assert!(c.take());
        // The third fills it: no token from send
        assert!(!c.take());
        assert!(!c.room());
        // A write done: room again, the token from the SCO thread
        assert!(c.give());
        assert!(c.room());
        assert!(c.give());
        assert!(c.give());
        // Never below zero
        assert!(c.give());
        assert_eq!(c.outstanding.load(Ordering::Acquire), 0);
        c.take();
        c.clear();
        assert_eq!(c.outstanding.load(Ordering::Acquire), 0);
    }

    #[test]
    fn sco_device_gone_errors() {
        for e in [6, 2, 22, 433, 1167] {
            assert!(sco_device_gone(e), "{e}");
        }
        for e in [31, 87, 995, 997, 121] {
            assert!(!sco_device_gone(e), "{e}");
        }
    }

    #[test]
    fn only_a_gone_adapter_or_a_lost_command_is_fatal() {
        use TransferError::*;
        for data in [HCI_ACL_DATA_PACKET, HCI_ISO_DATA_PACKET] {
            for e in [Cancelled, Stall, Fault, InvalidArgument, Unknown(31)] {
                assert_eq!(write_failure(data, &e), WriteFailure::Dropped, "{data} {e:?}");
            }
            for e in [Disconnected, Unknown(22), Unknown(6)] {
                assert_eq!(write_failure(data, &e), WriteFailure::Fatal, "{data} {e:?}");
            }
        }
        for e in [Cancelled, Stall, Disconnected] {
            assert_eq!(write_failure(HCI_COMMAND_DATA_PACKET, &e), WriteFailure::Fatal);
        }
        assert!(device_gone(&Disconnected));
        assert!(!device_gone(&Stall));
        assert!(!device_gone(&Cancelled));
    }

    fn usb_candidate(vid: u16, pid: u16, bluetooth_class: bool, known: bool) -> UsbCandidate {
        UsbCandidate {
            vid,
            pid,
            instance_id: format!(r"USB\VID_{vid:04X}&PID_{pid:04X}\1"),
            driver: Some("WinUSB".into()),
            product: None,
            bluetooth_class,
            known,
        }
    }

    #[test]
    fn selector_takes() {
        let any = UsbSelector::default();
        assert!(any.takes(&usb_candidate(0x0BDA, 0x8771, true, false)));
        // A vendor-class adapter (ASUS BT400) once its VID:PID is known
        assert!(any.takes(&usb_candidate(0x0B05, 0x17CB, false, true)));
        assert!(!any.takes(&usb_candidate(0x0B05, 0x17CB, false, false)));
        let vp = UsbSelector { vid_pid: Some((0x0B05, 0x17CB)), ..Default::default() };
        assert!(vp.takes(&usb_candidate(0x0B05, 0x17CB, false, false)));
        assert!(!vp.takes(&usb_candidate(0x0BDA, 0x8771, true, false)));
        let id = UsbSelector::from_arg(r"usb\vid_0bda&pid_8771\1").unwrap();
        assert!(id.takes(&usb_candidate(0x0BDA, 0x8771, true, false)));
        assert!(!id.takes(&usb_candidate(0x0BDA, 0x8772, true, false)));
    }

    #[test]
    fn selector_from_instance_id() {
        let s = UsbSelector::from_arg(r"USB\VID_0BDA&PID_8771\00E04C000001").unwrap();
        assert_eq!(s.instance_id.as_deref(), Some(r"USB\VID_0BDA&PID_8771\00E04C000001"));
        assert!(s.vid_pid.is_none());
        let s = UsbSelector::from_arg(r" usb\vid_0bda&pid_8771\5&1a2b3c&0&4 ").unwrap();
        assert_eq!(s.instance_id.as_deref(), Some(r"usb\vid_0bda&pid_8771\5&1a2b3c&0&4"));
    }

    #[test]
    fn selector_from_device_path() {
        let s = UsbSelector::from_arg(r"\\?\usb#vid_0bda&pid_8771#00e04c000001#{a5dcbf10-6530-11d2-901f-00c04fb951ed}")
            .unwrap();
        assert_eq!(s.instance_id.as_deref(), Some(r"usb\vid_0bda&pid_8771\00e04c000001"));
        assert!(
            s.instance_id.unwrap().eq_ignore_ascii_case(r"USB\VID_0BDA&PID_8771\00E04C000001"),
            "matches the instance ID nusb reports"
        );
        assert_eq!(
            instance_id_from_device_path(
                r"\\.\USB#VID_0A12&PID_0001#6&2C3D&0&1#{dee824ef-729b-4a0e-9c14-b7117d33a817}"
            )
            .as_deref(),
            Some(r"USB\VID_0A12&PID_0001\6&2C3D&0&1")
        );
        assert!(instance_id_from_device_path(r"\\?\usb#vid_0bda&pid_8771#00e04c000001").is_none());
        assert!(instance_id_from_device_path(r"usb#vid_0bda&pid_8771#x#{guid}").is_none());
    }

    #[test]
    fn selector_from_vid_pid() {
        let s = UsbSelector::from_arg("0bda:8771").unwrap();
        assert_eq!(s.vid_pid, Some((0x0BDA, 0x8771)));
        assert!(s.instance_id.is_none());
    }

    #[test]
    fn bad_selectors() {
        for bad in [
            "",
            "hello",
            "0bda:87",
            r"PCI\VEN_8086&DEV_A0F0\3&11583659&0&A3",
            r"USB\VID_0BDA&PID_8771",
            r"\\?\hid#x#{g}",
        ] {
            assert!(UsbSelector::from_arg(bad).is_err(), "{bad}");
        }
    }
}

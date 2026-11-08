//! WinUSB isochronous transfers on an interface claimed through nusb (the
//! SCO pipes of a Bluetooth adapter's interface 1), which nusb does not
//! wrap.
//!
//! nusb opens the device file once and binds it to a thread-pool I/O
//! completion callback that takes every OVERLAPPED completing on that file
//! for one of its own transfers. The OVERLAPPED structures here therefore
//! carry an event handle with its low bit set: Windows then signals the
//! event and queues no completion packet (GetQueuedCompletionStatus docs),
//! so nusb never sees these transfers. The owner waits on the event
//! ([`wait_any`]) and collects the result with WinUsb_GetOverlappedResult.
//!
//! Lifetimes, as WinUSB wants them (WinUsb_RegisterIsochBuffer docs): a
//! buffer is registered for one pipe of the current alternate setting, so
//! after the alternate setting has been selected; it stays registered, at
//! a fixed address, while transfers on it are pending; it is unregistered
//! after they have all completed or been cancelled (WinUsb_AbortPipe). The
//! objects here keep a clone of the nusb [`Interface`], so the WinUSB
//! handle outlives them. Drop aborts the pipe, waits for every pending
//! transfer and only then unregisters and frees the buffer; a transfer that
//! does not come back (a driver that never completes it) leaks its buffer
//! instead of freeing memory the driver may still write.
//!
//! Errors are Win32 error codes (`GetLastError`).

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::time::Duration;

use nusb::Interface;
use windows::Win32::Devices::Usb::{
    USB_INTERFACE_DESCRIPTOR, USBD_ISO_PACKET_DESCRIPTOR, UsbdPipeTypeIsochronous, WINUSB_INTERFACE_HANDLE,
    WINUSB_PIPE_INFORMATION_EX, WinUsb_AbortPipe, WinUsb_GetOverlappedResult, WinUsb_QueryInterfaceSettings,
    WinUsb_QueryPipeEx, WinUsb_ReadIsochPipeAsap, WinUsb_RegisterIsochBuffer, WinUsb_UnregisterIsochBuffer,
    WinUsb_WriteIsochPipeAsap,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, HANDLE, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::PCWSTR;

/// A Win32 error code
pub type Win32Error = u32;

/// How long Drop / [`IsochIn::abort`] wait for an aborted transfer
const ABORT_WAIT: Duration = Duration::from_millis(1000);

const ERROR_INVALID_PARAMETER: Win32Error = 87;
const ERROR_BUSY: Win32Error = 170;

/// The Win32 error code in a `windows` error (HRESULT_FROM_WIN32), or the
/// HRESULT itself
fn code(e: &windows::core::Error) -> Win32Error {
    let h = e.code().0 as u32;
    if h & 0xFFFF_0000 == 0x8007_0000 { h & 0xFFFF } else { h }
}

/// A Win32 timeout in milliseconds, below INFINITE
fn millis(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128 - 1) as u32
}

// ---- Pure helpers ----

/// Isochronous packets for `ms` milliseconds of stream on an endpoint:
/// full speed has one packet per 1 ms frame (bInterval must be 1); high
/// speed and faster have one per 2^(bInterval-1) microframes of 125 µs,
/// and WinUSB wants the packet count of a transfer to be a multiple of
/// the packets per 1 ms frame (8 / period) there. At least one packet.
pub fn packets_for(high_speed: bool, interval: u8, ms: u32) -> usize {
    let ms = ms.max(1) as usize;
    if !high_speed {
        return ms;
    }
    // bInterval 1..=16 on high speed: a period of 2^(b-1) microframes
    let period = 1usize << (interval.clamp(1, 16) - 1);
    let microframes = ms * 8;
    let packets = microframes.div_ceil(period).max(1);
    let per_frame = (8 / period).max(1);
    packets.div_ceil(per_frame) * per_frame
}

// ---- Pipes ----

/// A pipe of an alternate setting, as WinUSB reports it
/// (WINUSB_PIPE_INFORMATION_EX)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeInfo {
    /// Endpoint address (bit 7: IN)
    pub id: u8,
    pub isochronous: bool,
    /// wMaxPacketSize as WinUSB has it
    pub max_packet_size: u16,
    /// The polling interval as WinUSB has it (frames on full speed,
    /// microframes on high speed)
    pub interval: u8,
    /// Bytes per service interval (high-bandwidth: packets per microframe
    /// times the packet size); the size of one isochronous packet
    pub max_bytes_per_interval: u32,
}

/// The pipes of alternate setting `alt` of a claimed interface
pub fn pipes(interface: &Interface, alt: u8) -> Result<Vec<PipeInfo>, Win32Error> {
    let handle = WINUSB_INTERFACE_HANDLE(interface.winusb_handle());
    let mut desc = USB_INTERFACE_DESCRIPTOR::default();
    // SAFETY: the handle belongs to `interface`, alive for the call; desc is
    // a valid out pointer.
    unsafe { WinUsb_QueryInterfaceSettings(handle, alt, &mut desc) }.map_err(|e| code(&e))?;
    let mut out = Vec::with_capacity(desc.bNumEndpoints as usize);
    for index in 0..desc.bNumEndpoints {
        let mut info = WINUSB_PIPE_INFORMATION_EX::default();
        // SAFETY: as above; info is a valid out pointer.
        unsafe { WinUsb_QueryPipeEx(handle, alt, index, &mut info) }.map_err(|e| code(&e))?;
        out.push(PipeInfo {
            id: info.PipeId,
            isochronous: info.PipeType == UsbdPipeTypeIsochronous,
            max_packet_size: info.MaximumPacketSize,
            interval: info.Interval,
            max_bytes_per_interval: info.MaximumBytesPerInterval,
        });
    }
    Ok(out)
}

// ---- Events and waiting ----

/// A Win32 event (manual or auto reset)
#[derive(Debug)]
pub struct Event(HANDLE);

// SAFETY: an event handle may be used from any thread.
unsafe impl Send for Event {}
// SAFETY: SetEvent / ResetEvent / waits are thread-safe.
unsafe impl Sync for Event {}

impl Event {
    /// An unsignalled event; `manual_reset` false: a wait resets it
    pub fn new(manual_reset: bool) -> Result<Event, Win32Error> {
        // SAFETY: no security attributes, no name.
        let h = unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }.map_err(|e| code(&e))?;
        Ok(Event(h))
    }

    pub fn set(&self) {
        // SAFETY: the handle is this object's open event.
        let _ = unsafe { SetEvent(self.0) };
    }

    fn reset(&self) {
        // SAFETY: as in set.
        let _ = unsafe { ResetEvent(self.0) };
    }

    /// For [`wait_any`]
    pub fn wait_handle(&self) -> WaitHandle<'_> {
        WaitHandle(self.0, PhantomData)
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is this object's, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Something to wait for, borrowed from its owner
#[derive(Debug, Clone, Copy)]
pub struct WaitHandle<'a>(HANDLE, PhantomData<&'a ()>);

/// Waits until one of `handles` (at most 64) is signalled: its index, or
/// None on timeout or failure
pub fn wait_any(handles: &[WaitHandle<'_>], timeout: Duration) -> Option<usize> {
    if handles.is_empty() || handles.len() > 64 {
        std::thread::sleep(timeout);
        return None;
    }
    let raw: Vec<HANDLE> = handles.iter().map(|h| h.0).collect();
    // SAFETY: every handle is borrowed from a live Event / transfer for the
    // call (WaitHandle's lifetime).
    let r = unsafe { WaitForMultipleObjects(&raw, false, millis(timeout)) };
    match r {
        WAIT_TIMEOUT | WAIT_FAILED => None,
        WAIT_EVENT(n) => {
            let i = n.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            (i < handles.len()).then_some(i)
        }
    }
}

// ---- One overlapped transfer slot ----

/// An OVERLAPPED at a fixed address with its manual-reset event
struct Slot {
    ov: NonNull<OVERLAPPED>,
    event: Event,
    pending: bool,
}

// SAFETY: the OVERLAPPED is owned by the slot and only used by its owner
// (and the kernel, while pending).
unsafe impl Send for Slot {}

impl Slot {
    fn new() -> Result<Slot, Win32Error> {
        let event = Event::new(true)?;
        let ov = NonNull::from(Box::leak(Box::new(OVERLAPPED::default())));
        Ok(Slot { ov, event, pending: false })
    }

    /// A fresh OVERLAPPED for the next transfer (the slot is idle): zeroed,
    /// its event unsignalled and tagged with the low bit (no completion
    /// packet, module docs)
    fn prepare(&mut self) -> *const OVERLAPPED {
        debug_assert!(!self.pending);
        self.event.reset();
        let tagged = HANDLE((self.event.0.0 as usize | 1) as *mut c_void);
        // SAFETY: ov is this slot's allocation and no transfer uses it (idle).
        unsafe { self.ov.as_ptr().write(OVERLAPPED { hEvent: tagged, ..Default::default() }) };
        self.ov.as_ptr()
    }

    /// The result of the pending transfer once it has completed: Ok(bytes)
    /// or the error; None while it is still pending (or none is)
    fn complete(&mut self, handle: WINUSB_INTERFACE_HANDLE, wait: Duration) -> Option<Result<u32, Win32Error>> {
        if !self.pending {
            return None;
        }
        // SAFETY: the slot's own event.
        if unsafe { WaitForSingleObject(self.event.0, millis(wait)) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut bytes = 0u32;
        // SAFETY: ov is the OVERLAPPED of the transfer submitted on `handle`;
        // the event is signalled, so it has completed and the call does not
        // block.
        let r = unsafe { WinUsb_GetOverlappedResult(handle, self.ov.as_ptr(), &mut bytes, false) };
        match r {
            Err(e) if code(&e) == ERROR_IO_INCOMPLETE.0 => None,
            Err(e) => {
                self.pending = false;
                Some(Err(code(&e)))
            }
            Ok(()) => {
                self.pending = false;
                Some(Ok(bytes))
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.pending {
            // The driver may still write it: leaked on purpose (and the
            // event stays open with it)
            std::mem::forget(std::mem::replace(&mut self.event, Event(HANDLE::default())));
            return;
        }
        // SAFETY: allocated by Box in new, no transfer uses it.
        drop(unsafe { Box::from_raw(self.ov.as_ptr()) });
    }
}

/// The outcome of submitting: Ok when pending (or done at once), else the
/// error
fn submitted(r: windows::core::Result<()>) -> Result<(), Win32Error> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if code(&e) == ERROR_IO_PENDING.0 => Ok(()),
        Err(e) => Err(code(&e)),
    }
}

// ---- A registered buffer ----

/// A heap buffer registered for one pipe, at a fixed address; the kernel
/// reads or writes parts of it while transfers are pending, so it is only
/// reached through raw pointers
struct Registered {
    interface: Interface,
    pipe: u8,
    data: NonNull<u8>,
    len: usize,
    handle: *mut c_void,
}

impl Registered {
    fn new(interface: &Interface, pipe: u8, len: usize) -> Result<Registered, Win32Error> {
        let data = NonNull::from(Box::leak(vec![0u8; len.max(1)].into_boxed_slice())).cast::<u8>();
        let mut handle = std::ptr::null_mut();
        let winusb = WINUSB_INTERFACE_HANDLE(interface.winusb_handle());
        // SAFETY: data points to len.max(1) bytes this object owns, at a fixed
        // address until Drop; no transfer uses them yet; handle is a valid
        // out pointer.
        let r = unsafe {
            WinUsb_RegisterIsochBuffer(
                winusb,
                pipe,
                std::slice::from_raw_parts_mut(data.as_ptr(), len.max(1)),
                &mut handle,
            )
        };
        if let Err(e) = r {
            // SAFETY: allocated above, never registered.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(data.as_ptr(), len.max(1))) });
            return Err(code(&e));
        }
        Ok(Registered { interface: interface.clone(), pipe, data, len: len.max(1), handle })
    }

    fn winusb(&self) -> WINUSB_INTERFACE_HANDLE {
        WINUSB_INTERFACE_HANDLE(self.interface.winusb_handle())
    }

    fn abort(&self) {
        // SAFETY: the interface's handle, alive (self.interface).
        if let Err(e) = unsafe { WinUsb_AbortPipe(self.winusb(), self.pipe) } {
            tracing::debug!("USB: abort of pipe 0x{:02x}: error {}", self.pipe, code(&e));
        }
    }

    /// Unregisters and frees (every transfer has come back), or leaks the
    /// buffer (`leak`: one is still pending)
    fn release(self, leak: bool) {
        if leak {
            tracing::error!(
                "USB: an isochronous transfer on pipe 0x{:02x} did not return; its {} byte buffer is left registered",
                self.pipe,
                self.len
            );
            std::mem::forget(self);
            return;
        }
        drop(self);
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        // SAFETY: the handle from RegisterIsochBuffer; no transfer is pending
        // (callers release with leak = true otherwise).
        if let Err(e) = unsafe { WinUsb_UnregisterIsochBuffer(self.handle) } {
            tracing::debug!("USB: unregistering the buffer of pipe 0x{:02x}: error {}", self.pipe, code(&e));
        }
        // SAFETY: allocated in new with this length; unregistered above, so
        // the driver no longer uses it.
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(self.data.as_ptr(), self.len)) });
    }
}

// SAFETY: the buffer and handle are owned by this object and only used by
// its owner; WinUSB calls on an interface handle are thread-safe (nusb
// relies on it as well).
unsafe impl Send for Registered {}

/// Aborts the pipe and waits for every pending slot; whether all returned
fn abort_and_wait(reg: &Registered, slots: &mut [Slot]) -> bool {
    if slots.iter().all(|s| !s.pending) {
        return true;
    }
    reg.abort();
    let handle = reg.winusb();
    let mut all = true;
    for s in slots.iter_mut().filter(|s| s.pending) {
        if s.complete(handle, ABORT_WAIT).is_none() {
            all = false;
        }
    }
    all
}

// ---- IN ----

/// A continuous isochronous IN stream: `transfers` transfers of `packets`
/// packets of `packet_len` bytes (the pipe's MaximumBytesPerInterval),
/// each in its own part of one registered buffer, read with
/// WinUsb_ReadIsochPipeAsap
pub struct IsochIn {
    reg: Option<Registered>,
    descriptors: NonNull<USBD_ISO_PACKET_DESCRIPTOR>,
    slots: Vec<Slot>,
    packets: usize,
    packet_len: usize,
}

// SAFETY: see Registered; the descriptors are owned like the buffer.
unsafe impl Send for IsochIn {}

impl IsochIn {
    /// Registers the buffer for IN pipe `pipe` of the current alternate
    /// setting
    pub fn new(
        interface: &Interface,
        pipe: u8,
        transfers: usize,
        packets: usize,
        packet_len: usize,
    ) -> Result<IsochIn, Win32Error> {
        let (transfers, packets, packet_len) = (transfers.max(1), packets.max(1), packet_len.max(1));
        let mut slots = Vec::with_capacity(transfers);
        for _ in 0..transfers {
            slots.push(Slot::new()?);
        }
        let reg = Registered::new(interface, pipe, transfers * packets * packet_len)?;
        let descriptors = NonNull::from(Box::leak(
            vec![USBD_ISO_PACKET_DESCRIPTOR::default(); transfers * packets].into_boxed_slice(),
        ))
        .cast();
        Ok(IsochIn { reg: Some(reg), descriptors, slots, packets, packet_len })
    }

    pub fn transfers(&self) -> usize {
        self.slots.len()
    }

    fn transfer_len(&self) -> usize {
        self.packets * self.packet_len
    }

    pub fn is_pending(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(|s| s.pending)
    }

    /// Reads into transfer `slot` (idle): `continue_stream` true: in the
    /// frame right after the transfer submitted before (fails if that is
    /// no longer possible), false: as soon as possible
    pub fn submit(&mut self, slot: usize, continue_stream: bool) -> Result<(), Win32Error> {
        let (len, packets) = (self.transfer_len(), self.packets);
        let Some(reg) = &self.reg else { return Err(ERROR_INVALID_PARAMETER) };
        let Some(s) = self.slots.get_mut(slot) else { return Err(ERROR_INVALID_PARAMETER) };
        if s.pending {
            return Err(ERROR_BUSY);
        }
        let ov = s.prepare();
        // SAFETY: the slot is idle, so the kernel uses neither its
        // descriptors nor its part of the buffer; the slice ends before
        // the call returns, the kernel then writes through the pointer it
        // was given while the transfer is pending; the descriptors stay
        // allocated until Drop has waited for every transfer.
        let descriptors =
            unsafe { std::slice::from_raw_parts_mut(self.descriptors.as_ptr().add(slot * packets), packets) };
        // SAFETY: the registered buffer handle; offset + len is inside the
        // registered buffer; ov is the slot's OVERLAPPED at a fixed address
        // with a tagged event (module docs).
        let r = unsafe {
            WinUsb_ReadIsochPipeAsap(
                reg.handle,
                (slot * len) as u32,
                len as u32,
                continue_stream,
                descriptors,
                Some(ov),
            )
        };
        submitted(r)?;
        s.pending = true;
        Ok(())
    }

    /// For [`wait_any`]: signalled when transfer `slot` completes
    pub fn wait_handle(&self, slot: usize) -> WaitHandle<'_> {
        self.slots[slot].event.wait_handle()
    }

    /// Transfer `slot` has completed: Ok, or its error; None while pending
    /// (or idle). Its packets are then in [`packets`](Self::packets).
    pub fn complete(&mut self, slot: usize) -> Option<Result<(), Win32Error>> {
        let handle = self.reg.as_ref()?.winusb();
        self.slots.get_mut(slot)?.complete(handle, Duration::ZERO).map(|r| r.map(|_| ()))
    }

    /// The packets of completed (idle) transfer `slot`, in order: the bytes
    /// received, or the packet's USBD status when it failed. Empty while
    /// the transfer is pending.
    pub fn packets(&self, slot: usize) -> Vec<Result<&[u8], i32>> {
        if slot >= self.slots.len() || self.slots[slot].pending {
            return Vec::new();
        }
        let len = self.transfer_len();
        // SAFETY: the slot is idle, so the kernel no longer writes its
        // descriptors or its part of the buffer; both stay allocated while
        // self lives (the returned slices borrow self).
        let descriptors =
            unsafe { std::slice::from_raw_parts(self.descriptors.as_ptr().add(slot * self.packets), self.packets) };
        let Some(reg) = &self.reg else { return Vec::new() };
        // SAFETY: as above, the slot's part of the registered buffer.
        let data = unsafe { std::slice::from_raw_parts(reg.data.as_ptr().add(slot * len), len) };
        descriptors
            .iter()
            .map(|d| {
                if d.Status != 0 {
                    return Err(d.Status);
                }
                let (off, n) = (d.Offset as usize, d.Length as usize);
                Ok(data.get(off..off.saturating_add(n)).unwrap_or(&[]))
            })
            .collect()
    }

    /// Cancels every pending transfer (WinUsb_AbortPipe) and waits for
    /// them; false if one did not return
    pub fn abort(&mut self) -> bool {
        match &self.reg {
            Some(reg) => abort_and_wait(reg, &mut self.slots),
            None => true,
        }
    }
}

impl Drop for IsochIn {
    fn drop(&mut self) {
        let all = self.abort();
        if let Some(reg) = self.reg.take() {
            reg.release(!all);
        }
        if all {
            // SAFETY: allocated in new with this length; no transfer is
            // pending, so the kernel no longer writes them.
            drop(unsafe {
                Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    self.descriptors.as_ptr(),
                    self.slots.len() * self.packets,
                ))
            });
        }
        // Slots: a pending one leaks itself
    }
}

// ---- OUT ----

/// An isochronous OUT stream: `slots` transfers of up to `slot_len` bytes,
/// each in its own part of one registered buffer, written with
/// WinUsb_WriteIsochPipeAsap. WinUSB cuts each transfer into packets of the
/// pipe's MaximumBytesPerInterval, one per service interval.
pub struct IsochOut {
    reg: Option<Registered>,
    slots: Vec<Slot>,
    slot_len: usize,
}

impl IsochOut {
    /// Registers the buffer for OUT pipe `pipe` of the current alternate
    /// setting
    pub fn new(interface: &Interface, pipe: u8, slots: usize, slot_len: usize) -> Result<IsochOut, Win32Error> {
        let (n, slot_len) = (slots.max(1), slot_len.max(1));
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            slots.push(Slot::new()?);
        }
        let reg = Registered::new(interface, pipe, n * slot_len)?;
        Ok(IsochOut { reg: Some(reg), slots, slot_len })
    }

    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    pub fn slot_len(&self) -> usize {
        self.slot_len
    }

    pub fn is_pending(&self, slot: usize) -> bool {
        self.slots.get(slot).is_some_and(|s| s.pending)
    }

    /// Writes `data` from slot `slot` (idle): `continue_stream` as in
    /// [`IsochIn::submit`]
    pub fn submit(&mut self, slot: usize, data: &[u8], continue_stream: bool) -> Result<(), Win32Error> {
        let slot_len = self.slot_len;
        let Some(reg) = &self.reg else { return Err(ERROR_INVALID_PARAMETER) };
        let Some(s) = self.slots.get_mut(slot) else { return Err(ERROR_INVALID_PARAMETER) };
        if data.is_empty() || data.len() > slot_len {
            return Err(ERROR_INVALID_PARAMETER);
        }
        if s.pending {
            return Err(ERROR_BUSY);
        }
        // SAFETY: the slot is idle, so the kernel does not read its part of
        // the buffer; other slots' parts are not touched.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), reg.data.as_ptr().add(slot * slot_len), data.len()) };
        let ov = s.prepare();
        // SAFETY: the registered buffer handle; the range is inside it; ov
        // is the slot's OVERLAPPED at a fixed address with a tagged event.
        let r = unsafe {
            WinUsb_WriteIsochPipeAsap(
                reg.handle,
                (slot * slot_len) as u32,
                data.len() as u32,
                continue_stream,
                Some(ov),
            )
        };
        submitted(r)?;
        s.pending = true;
        Ok(())
    }

    /// For [`wait_any`]: signalled when slot `slot`'s write completes
    pub fn wait_handle(&self, slot: usize) -> WaitHandle<'_> {
        self.slots[slot].event.wait_handle()
    }

    /// Slot `slot`'s write has completed: Ok(bytes), or its error; None
    /// while pending (or idle)
    pub fn complete(&mut self, slot: usize) -> Option<Result<u32, Win32Error>> {
        let handle = self.reg.as_ref()?.winusb();
        self.slots.get_mut(slot)?.complete(handle, Duration::ZERO)
    }

    /// Cancels every pending write and waits for them; false if one did not
    /// return
    pub fn abort(&mut self) -> bool {
        match &self.reg {
            Some(reg) => abort_and_wait(reg, &mut self.slots),
            None => true,
        }
    }
}

impl Drop for IsochOut {
    fn drop(&mut self) {
        let all = self.abort();
        if let Some(reg) = self.reg.take() {
            reg.release(!all);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_speed_has_one_packet_per_millisecond() {
        assert_eq!(packets_for(false, 1, 3), 3);
        assert_eq!(packets_for(false, 1, 1), 1);
        assert_eq!(packets_for(false, 1, 0), 1);
        // bInterval is not looked at on full speed
        assert_eq!(packets_for(false, 4, 3), 3);
    }

    #[test]
    fn high_speed_rounds_to_whole_frames() {
        // Every microframe: 8 per ms
        assert_eq!(packets_for(true, 1, 3), 24);
        // Every 2 / 4 / 8 microframes
        assert_eq!(packets_for(true, 2, 3), 12);
        assert_eq!(packets_for(true, 3, 3), 6);
        assert_eq!(packets_for(true, 4, 3), 3);
        // Every 16 microframes (2 ms): 2 packets cover 3 ms
        assert_eq!(packets_for(true, 5, 3), 2);
        // Out-of-range bIntervals are clamped
        assert_eq!(packets_for(true, 0, 3), 24);
        assert_eq!(packets_for(true, 200, 3), 1);
        for interval in 1..=4u8 {
            let per_frame = 8 >> (interval - 1);
            for ms in 1..=8 {
                assert_eq!(packets_for(true, interval, ms) % per_frame, 0, "interval {interval}, {ms} ms");
            }
        }
    }

    #[test]
    fn win32_codes_from_hresults() {
        let e = windows::core::Error::from(ERROR_IO_PENDING.to_hresult());
        assert_eq!(code(&e), 997);
        assert_eq!(millis(Duration::from_millis(3)), 3);
        assert!(millis(Duration::MAX) < u32::MAX);
    }

    #[test]
    fn events_wake_waiters() {
        let a = Event::new(false).unwrap();
        let b = Event::new(true).unwrap();
        assert_eq!(wait_any(&[a.wait_handle(), b.wait_handle()], Duration::ZERO), None);
        b.set();
        assert_eq!(wait_any(&[a.wait_handle(), b.wait_handle()], Duration::ZERO), Some(1));
        // Manual reset: still set
        assert_eq!(wait_any(&[b.wait_handle()], Duration::ZERO), Some(0));
        b.reset();
        a.set();
        assert_eq!(wait_any(&[a.wait_handle(), b.wait_handle()], Duration::ZERO), Some(0));
        // Auto reset: taken by the wait
        assert_eq!(wait_any(&[a.wait_handle()], Duration::ZERO), None);
    }
}

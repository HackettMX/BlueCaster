//! Cross-Transport Key Derivation from BR/EDR (issue #86): the LE keys of a
//! classic device, as Windows and Linux get them when they pair it. After a
//! classic link is encrypted with Secure Connections, SM (the patched sm.c,
//! see build.rs `patched_sm`) asks the device over the BR/EDR security
//! manager channel for its IRK and identity address, and both sides derive
//! the LE LTK from the link key. SM keeps them as an LE bond of the device's
//! identity address (le_device_db, the bond TLV file); which classic device
//! a bond came from is kept beside it in the same file (one tag per bond).
//!
//! Runs only while [`set_ctkd`] is on (developer mode) and SM is set up (LE
//! Audio on, [`Stack::le_init`]): after a new pairing, and on a connection
//! of a device without keys from it yet (once per device and run). A device
//! that does not list the channel is not asked; one that refuses, or does
//! not answer (SM's 30 s timeout), keeps its classic link as it is.
//!
//! The LE keys Windows got the same way can be taken over instead
//! ([`Stack::add_le_bond_of_classic`], issue #86): a device that refuses to
//! give them again over a link it is already paired on then has them too.

use std::cell::RefCell;
use std::ffi::c_int;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::callbacks::guard;
use crate::consts::*;
use crate::stack::addr_str;
use crate::{ConnectionType, Stack, ffi};

static ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Classic devices asked during this run (each once)
    static ASKED: RefCell<Vec<[u8; 6]>> = const { RefCell::new(Vec::new()) };
    /// What came of it for each classic device during this run
    static OUTCOMES: RefCell<Vec<([u8; 6], CtkdOutcome)>> = const { RefCell::new(Vec::new()) };
}

/// Why BlueCaster has no LE keys of a classic device (issue #86: "not
/// available" alone did not say)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtkdOutcome {
    /// SM is not set up (LE Audio off), so nothing is asked
    NoLe,
    /// Not asked during this run yet (no connection since)
    NotAsked,
    /// Asked, no answer yet
    Asking,
    /// The link is not encrypted with Secure Connections
    NotSecure,
    /// The device opened the connection; only one BlueCaster opens asks
    DeviceLed,
    /// The device does not take SMP over BR/EDR (no CID 7)
    NoSupport,
    /// The device refused
    Refused,
    /// The device did not answer within SM's 30 s
    NoAnswer,
    /// The device answered without keys
    NoneGiven,
}

/// What C tells about a device (bluecaster_rs_ctkd_note)
const NOTE_NOT_SECURE: c_int = 1;
const NOTE_DEVICE_LED: c_int = 2;
const NOTE_NO_SUPPORT: c_int = 3;

fn set_outcome(addr: [u8; 6], outcome: CtkdOutcome, weak: bool) {
    OUTCOMES.with(|o| {
        let mut o = o.borrow_mut();
        match o.iter_mut().find(|(a, _)| *a == addr) {
            // A skipped connection never hides what an earlier ask gave
            Some((_, v))
                if weak && !matches!(v, CtkdOutcome::NotAsked | CtkdOutcome::NotSecure | CtkdOutcome::DeviceLed) => {}
            Some((_, v)) => *v = outcome,
            None => o.push((addr, outcome)),
        }
    });
}

fn outcome_of(addr: &[u8; 6]) -> CtkdOutcome {
    OUTCOMES.with(|o| o.borrow().iter().find(|(a, _)| a == addr).map_or(CtkdOutcome::NotAsked, |(_, v)| *v))
}

/// Asks classic devices for their LE keys (from any thread; effective while
/// SM runs, i.e. with LE Audio on)
pub fn set_ctkd(enabled: bool) {
    if ENABLED.swap(enabled, Ordering::AcqRel) != enabled {
        tracing::debug!(target: "le", "CTKD: asking classic devices for LE keys {}", if enabled { "on" } else { "off" });
    }
}

pub fn ctkd_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub(crate) fn reset() {
    ASKED.with(|a| a.borrow_mut().clear());
    OUTCOMES.with(|o| o.borrow_mut().clear());
}

/// The LE keys BlueCaster has of a classic device
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeKeys {
    /// The identity address the device gave (BD_ADDR_TYPE_LE_PUBLIC /
    /// _RANDOM), its classic address when it gave none
    pub identity_addr_type: u8,
    pub identity_addr: [u8; 6],
    /// The device's IRK, most significant octet first as BTstack keeps it;
    /// None when it gave none
    pub irk: Option<[u8; 16]>,
    /// The LTK, most significant octet first; None when there is none
    pub ltk: Option<[u8; 16]>,
    /// The LTK's encryption key size (octets)
    pub key_size: u8,
}

/// Ours, beside le_device_db's bond `index`: the classic device it came from
fn tag(index: usize) -> u32 {
    (u32::from(b'A') << 24) | (u32::from(b'W') << 16) | (u32::from(b'K') << 8) | (index as u32 & 0xFF)
}

/// Classic address, identity address type, identity address
const MAPPING_LEN: usize = 13;

fn mapping(classic: &[u8; 6], identity_type: u8, identity: &[u8; 6]) -> [u8; MAPPING_LEN] {
    let mut v = [0u8; MAPPING_LEN];
    v[..6].copy_from_slice(classic);
    v[6] = identity_type;
    v[7..].copy_from_slice(identity);
    v
}

fn nonzero(k: [u8; 16]) -> Option<[u8; 16]> {
    (k != [0u8; 16]).then_some(k)
}

impl Stack {
    /// The LE keys of the classic device `addr`: from its key derivation,
    /// else an LE bond of the same public address. None without LE (SM) or
    /// keys.
    pub fn le_keys_of_classic(&self, addr: &[u8; 6]) -> Option<LeKeys> {
        if !self.le_initialized() {
            return None;
        }
        let count = self.le_device_db_max_count();
        let derived = (0..count).find(|&i| {
            let Some(v) = self.tlv_get(tag(i)) else { return false };
            v.len() == MAPPING_LEN
                && v[..6] == addr[..]
                && self.le_device_db_info(i).is_some_and(|e| v[..] == mapping(addr, e.addr_type, &e.addr)[..])
        });
        let index = derived.or_else(|| {
            (0..count).find(|&i| {
                self.le_device_db_info(i).is_some_and(|e| e.addr_type == BD_ADDR_TYPE_LE_PUBLIC && e.addr == *addr)
            })
        })?;
        let entry = self.le_device_db_info(index)?;
        let encryption = self.le_device_db_encryption(index);
        let ltk = encryption.and_then(|e| nonzero(e.ltk));
        Some(LeKeys {
            identity_addr_type: entry.addr_type,
            identity_addr: entry.addr,
            irk: nonzero(entry.irk),
            ltk,
            key_size: encryption.filter(|_| ltk.is_some()).map_or(0, |e| e.key_size),
        })
    }

    /// The LE keys of the classic device `addr`, else why there are none
    pub fn le_keys_or_why(&self, addr: &[u8; 6]) -> Result<LeKeys, CtkdOutcome> {
        if !self.le_initialized() {
            return Err(CtkdOutcome::NoLe);
        }
        self.le_keys_of_classic(addr).ok_or_else(|| outcome_of(addr))
    }

    /// Adds the LE bond `bond` of the classic device `classic` (keys taken
    /// from Windows) as SM would after CTKD. A device that has LE keys
    /// already keeps them (they are the newer ones). Needs LE (SM).
    pub fn add_le_bond_of_classic(&self, classic: &[u8; 6], bond: &ImportedLeBond) -> LeBondAdded {
        if !self.le_initialized() {
            return LeBondAdded::NoLe;
        }
        if self.le_keys_of_classic(classic).is_some() {
            return LeBondAdded::Kept;
        }
        let (mut addr, mut irk, mut ltk, mut rand) = (bond.identity_addr, bond.irk, bond.ltk, bond.rand);
        // SAFETY: on the BTstack thread after le_init; the buffers have the
        // sizes BTstack reads (6, 16, 8, 16 bytes) and are copied.
        let index =
            unsafe { ffi::le_device_db_add(c_int::from(bond.identity_addr_type), addr.as_mut_ptr(), irk.as_mut_ptr()) };
        let Ok(index) = usize::try_from(index) else {
            tracing::warn!(target: "le", "Windows' LE keys of {}: no room for the bond", addr_str(classic));
            return LeBondAdded::Failed;
        };
        // SAFETY: as above; index is the slot le_device_db_add returned.
        unsafe {
            ffi::le_device_db_encryption_set(
                index as c_int,
                bond.ediv,
                rand.as_mut_ptr(),
                ltk.as_mut_ptr(),
                c_int::from(bond.key_size),
                c_int::from(bond.authenticated),
                0,
                c_int::from(bond.secure_connection),
            );
        }
        for i in (0..self.le_device_db_max_count()).filter(|&i| i != index) {
            if self.tlv_get(tag(i)).is_some_and(|v| v.len() == MAPPING_LEN && v[..6] == classic[..]) {
                self.tlv_delete(tag(i));
            }
        }
        self.tlv_store(tag(index), &mapping(classic, bond.identity_addr_type, &bond.identity_addr));
        tracing::info!(
            target: "le",
            "Windows' LE keys of {} taken (identity {}, type {}, IRK {})",
            addr_str(classic),
            addr_str(&bond.identity_addr),
            bond.identity_addr_type,
            if bond.irk == [0u8; 16] { "none" } else { "taken" }
        );
        LeBondAdded::Added
    }
}

/// An LE bond to add ([`Stack::add_le_bond_of_classic`]); keys most
/// significant octet first, as BTstack keeps them
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportedLeBond {
    /// BD_ADDR_TYPE_LE_PUBLIC / _RANDOM
    pub identity_addr_type: u8,
    pub identity_addr: [u8; 6],
    /// All zero: none
    pub irk: [u8; 16],
    pub ltk: [u8; 16],
    pub key_size: u8,
    pub ediv: u16,
    /// Most significant octet first
    pub rand: [u8; 8],
    pub authenticated: bool,
    pub secure_connection: bool,
}

/// What [`Stack::add_le_bond_of_classic`] did
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeBondAdded {
    Added,
    /// The device has LE keys already
    Kept,
    /// SM is not set up (LE Audio off)
    NoLe,
    Failed,
}

/// SM events over a classic link: an LE bond created is noted with the
/// classic device's address, and what came of asking
pub(crate) fn on_sm_event(stack: &Stack, p: &[u8]) {
    let Some(&event) = p.first() else { return };
    if !matches!(event, SM_EVENT_IDENTITY_CREATED | SM_EVENT_PAIRING_STARTED | SM_EVENT_PAIRING_COMPLETE) || p.len() < 4
    {
        return;
    }
    let handle = u16::from_le_bytes([p[2], p[3]]);
    if stack.connection_type(handle) != ConnectionType::Acl {
        return;
    }
    let mut classic = [0u8; 6];
    // SAFETY: on the BTstack thread; classic has the 6 bytes written.
    if unsafe { ffi::bluecaster_connection_address(handle, classic.as_mut_ptr()) } == 0 {
        return;
    }
    match event {
        SM_EVENT_PAIRING_STARTED => set_outcome(classic, CtkdOutcome::Asking, false),
        SM_EVENT_PAIRING_COMPLETE if p.len() >= 13 => on_complete(stack, classic, p[11], p[12]),
        SM_EVENT_IDENTITY_CREATED if p.len() >= 20 => on_identity(stack, classic, p),
        _ => {}
    }
}

fn on_complete(stack: &Stack, classic: [u8; 6], status: u8, reason: u8) {
    // Keys given: whatever SM says at the end (issue #86: a device gave its
    // keys, but SM only ended the pairing at the disconnection, with 0x13)
    if stack.le_keys_of_classic(&classic).is_some() {
        if status != ERROR_CODE_SUCCESS {
            tracing::debug!(target: "le", "CTKD: {} has LE keys; SM ended with status {status:#04x}", addr_str(&classic));
        }
        return;
    }
    let outcome = match status {
        ERROR_CODE_SUCCESS => CtkdOutcome::NoneGiven,
        ERROR_CODE_CONNECTION_TIMEOUT => CtkdOutcome::NoAnswer,
        // The link went down meanwhile: asked again at its next connection
        ERROR_CODE_REMOTE_USER_TERMINATED_CONNECTION | ERROR_CODE_CONNECTION_TERMINATED_BY_LOCAL_HOST => {
            ASKED.with(|a| a.borrow_mut().retain(|x| *x != classic));
            CtkdOutcome::NotAsked
        }
        _ => CtkdOutcome::Refused,
    };
    set_outcome(classic, outcome, false);
    tracing::info!(target: "le", "CTKD: no LE keys from {} ({outcome:?}, status {status:#04x}, reason {reason:#04x})", addr_str(&classic));
}

fn on_identity(stack: &Stack, classic: [u8; 6], p: &[u8]) {
    let index = usize::from(u16::from_le_bytes([p[18], p[19]]));
    let Some(entry) = stack.le_device_db_info(index) else { return };
    // One bond per classic device: an older one of another identity goes
    for i in (0..stack.le_device_db_max_count()).filter(|&i| i != index) {
        if stack.tlv_get(tag(i)).is_some_and(|v| v.len() == MAPPING_LEN && v[..6] == classic[..]) {
            stack.tlv_delete(tag(i));
        }
    }
    stack.tlv_store(tag(index), &mapping(&classic, entry.addr_type, &entry.addr));
    tracing::info!(
        target: "le",
        "CTKD: {} gave its LE keys (identity {}, type {}, IRK {})",
        addr_str(&classic),
        addr_str(&entry.addr),
        entry.addr_type,
        if entry.irk == [0u8; 16] { "none" } else { "received" }
    );
}

#[unsafe(no_mangle)]
extern "C" fn bluecaster_rs_ctkd_enabled() -> c_int {
    guard(0, || ctkd_enabled() as c_int)
}

/// SM, after a classic link was encrypted with Secure Connections, as
/// Central: whether to ask the device (no pairing just now)
#[unsafe(no_mangle)]
extern "C" fn bluecaster_rs_ctkd_wanted(addr: *const u8) -> c_int {
    guard(0, || {
        if !ctkd_enabled() || addr.is_null() {
            return 0;
        }
        let mut a = [0u8; 6];
        // SAFETY: SM passes its bd_addr_t (6 bytes).
        unsafe { std::ptr::copy_nonoverlapping(addr, a.as_mut_ptr(), 6) };
        let stack = Stack::new();
        if stack.le_keys_of_classic(&a).is_some() || ASKED.with(|s| s.borrow().contains(&a)) {
            return 0;
        }
        ASKED.with(|s| s.borrow_mut().push(a));
        tracing::info!(target: "le", "CTKD: asking {} for its LE keys", addr_str(&a));
        1
    })
}

/// SM, on a classic link it does not ask over: why (NOTE_*)
#[unsafe(no_mangle)]
extern "C" fn bluecaster_rs_ctkd_note(addr: *const u8, what: c_int) {
    guard((), || {
        if addr.is_null() {
            return;
        }
        let mut a = [0u8; 6];
        // SAFETY: SM passes its bd_addr_t (6 bytes).
        unsafe { std::ptr::copy_nonoverlapping(addr, a.as_mut_ptr(), 6) };
        match what {
            NOTE_NOT_SECURE => set_outcome(a, CtkdOutcome::NotSecure, true),
            NOTE_DEVICE_LED => set_outcome(a, CtkdOutcome::DeviceLed, true),
            NOTE_NO_SUPPORT => {
                set_outcome(a, CtkdOutcome::NoSupport, false);
                tracing::info!(target: "le", "CTKD: {} does not take SMP over BR/EDR", addr_str(&a));
            }
            _ => {}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_layout() {
        let v = mapping(&[1, 2, 3, 4, 5, 6], 1, &[0xC1, 2, 3, 4, 5, 6]);
        assert_eq!(v, [1, 2, 3, 4, 5, 6, 1, 0xC1, 2, 3, 4, 5, 6]);
        assert_eq!(tag(3), u32::from_be_bytes(*b"AWK\x03"));
        assert_eq!(nonzero([0; 16]), None);
        assert_eq!(nonzero([1; 16]), Some([1; 16]));
    }
}

//! An LE bond taken from Windows (issue #86): added to le_device_db, found
//! as the classic device's LE keys, kept across a restart, and never put
//! over keys the device has already.

use std::sync::mpsc;
use std::time::Duration;

use btstack_sys::{App, HciBackend, ImportedLeBond, LeBondAdded, LeKeys, MemoryTlv, Remote, Source, Stack, Upstream};

/// A controller that never answers: LE init needs only hci_init
struct Silent;

impl HciBackend for Silent {
    fn is_async(&self) -> bool {
        false
    }
    fn open(&mut self, _upstream: Upstream) -> Result<(), String> {
        Ok(())
    }
    fn close(&mut self) {}
    fn send(&mut self, _packet_type: u8, _packet: &[u8]) -> bool {
        true
    }
}

struct Idle;

impl App for Idle {
    fn on_packet(&mut self, _s: &Stack, _source: Source, _packet_type: u8, _channel: u16, _p: &[u8]) {}
    fn on_wake(&mut self, _s: &Stack) {}
    fn on_timer(&mut self, _s: &Stack, _id: u32) {}
}

const CLASSIC: [u8; 6] = [0x58, 0x18, 0x62, 0x02, 0x7A, 0xD7];

fn bond(ltk_first: u8) -> ImportedLeBond {
    let mut ltk = [0x11u8; 16];
    ltk[0] = ltk_first;
    ImportedLeBond {
        identity_addr_type: 1,
        identity_addr: [0xC0, 0x18, 0x62, 0x02, 0x7A, 0xD7],
        irk: [0x22; 16],
        ltk,
        key_size: 16,
        ediv: 0,
        rand: [0; 8],
        authenticated: false,
        secure_connection: true,
    }
}

/// One run with LE on `tlv`: adds `add` (if any), returns what that gave
/// and the classic device's LE keys afterwards
fn one_run(tlv: &MemoryTlv, add: Option<ImportedLeBond>) -> (Option<LeBondAdded>, Option<LeKeys>) {
    let tlv = tlv.clone();
    let (tx, rx) = mpsc::channel::<(Remote, Option<LeBondAdded>, Option<LeKeys>)>();
    let thread = std::thread::spawn(move || {
        btstack_sys::run(move |s, remote| {
            assert!(s.set_le_hidden(false));
            s.hci_init(Box::new(Silent));
            s.set_link_key_store(None);
            s.l2cap_init();
            s.le_init(Box::new(tlv.clone()));
            let added = add.map(|b| s.add_le_bond_of_classic(&CLASSIC, &b));
            tx.send((remote, added, s.le_keys_of_classic(&CLASSIC))).unwrap();
            Box::new(Idle)
        })
        .unwrap();
    });
    let (remote, added, keys) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    remote.request_exit();
    thread.join().unwrap();
    assert!(!btstack_sys::poisoned());
    (added, keys)
}

#[test]
fn a_windows_bond_is_the_classic_devices_le_keys() {
    let tlv = MemoryTlv::default();
    let expected = LeKeys {
        identity_addr_type: 1,
        identity_addr: [0xC0, 0x18, 0x62, 0x02, 0x7A, 0xD7],
        irk: Some([0x22; 16]),
        ltk: Some(bond(0xAB).ltk),
        key_size: 16,
    };
    assert_eq!(one_run(&tlv, Some(bond(0xAB))), (Some(LeBondAdded::Added), Some(expected)));
    // Kept across a restart; a second import does not replace the keys
    assert_eq!(one_run(&tlv, None), (None, Some(expected)));
    assert_eq!(one_run(&tlv, Some(bond(0xCD))), (Some(LeBondAdded::Kept), Some(expected)));
}

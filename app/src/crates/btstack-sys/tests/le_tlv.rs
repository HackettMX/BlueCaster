//! The LE bond store across BTstack restarts in one process (core restarts
//! BTstack between streams): what the first run stored is read back by the
//! second, through the TlvStore handed to le_init.

use std::sync::mpsc;
use std::time::Duration;

use btstack_sys::{App, HciBackend, MemoryTlv, Remote, Source, Stack, Upstream};

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

const TAG: u32 = 0x4132_5754;

/// One run with LE on `tlv`: stores `store` under TAG, returns what TAG held
/// right after le_init
fn one_run(tlv: &MemoryTlv, store: Option<&'static [u8]>) -> Option<Vec<u8>> {
    let tlv = tlv.clone();
    let (tx, rx) = mpsc::channel::<(Remote, Option<Vec<u8>>)>();
    let thread = std::thread::spawn(move || {
        btstack_sys::run(move |s, remote| {
            assert!(s.set_le_hidden(false));
            s.hci_init(Box::new(Silent));
            s.set_link_key_store(None);
            s.l2cap_init();
            s.le_init(Box::new(tlv.clone()));
            let held = s.tlv_get(TAG);
            if let Some(value) = store {
                assert!(s.tlv_store(TAG, value));
            }
            tx.send((remote, held)).unwrap();
            Box::new(Idle)
        })
        .unwrap();
    });
    let (remote, held) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    remote.request_exit();
    thread.join().unwrap();
    assert!(!btstack_sys::poisoned());
    held
}

#[test]
fn bonds_survive_a_restart() {
    let tlv = MemoryTlv::default();
    assert_eq!(one_run(&tlv, Some(b"bond")), None);
    // The second run (in the same process) reads the same store
    assert_eq!(one_run(&tlv, None).as_deref(), Some(&b"bond"[..]));
}

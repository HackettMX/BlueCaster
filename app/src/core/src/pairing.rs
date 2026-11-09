//! Developer mode: a classic device's pairing as BlueCaster keeps it (the
//! link key, and the LE keys derived from it while LE Audio is on) and the
//! services the device lists. For everyone: taking over the link keys
//! Windows' Bluetooth kept for the adapter BlueCaster now uses (issue #86:
//! devices paired in Windows then connect without pairing again; the audio
//! ones are added to the saved devices with Windows' names).
//!
//! Windows keeps its keys where only SYSTEM may read them: the elevated
//! helper reads them (bluecaster-driver.exe `windows-keys`, one administrator
//! prompt). A key BlueCaster has already is never replaced: the device was
//! paired with BlueCaster later, so Windows' key is the old one. The LE keys
//! Windows got of a classic device (CTKD) come along when its link key is
//! Windows' one.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bluecaster_core::paths;
use bt_host::ServicesError;
use bt_host::link_keys::FileLinkKeyStore;
use ipc::{
    DevicePairing, DeviceService, ErrorInfo, LeKeysInfo, LeKeysMissing, LinkKeyInfo, Msg, ServiceList, WindowsImport,
};
use serde_json::Value;

use crate::state::CoreState;

/// Windows keeps no key type: HCI's "unauthenticated combination key from
/// P-192". BTstack refuses a link without Secure Connections whose key type
/// says P-256, never one that says P-192, so this type holds whichever
/// kind of pairing Windows made.
const WINDOWS_KEY_TYPE: i32 = 4;

fn err(code: &str, key: &str) -> ErrorInfo {
    ErrorInfo { code: code.into(), message: Msg::new(key) }
}

fn developer(state: &CoreState) -> Result<(), ErrorInfo> {
    if state.service.settings().debug_mode { Ok(()) } else { Err(err("developer.off", "ipc.error.developer_only")) }
}

fn address(address: &str) -> Result<[u8; 6], ErrorInfo> {
    bt_host::parse_addr(address).ok_or_else(|| err("device.invalid_address", "error.invalid_address_format"))
}

/// The link keys file as it is now (`pairing\link_keys.txt`, not the
/// settings folder itself: issue #86, 3.0.0-beta.1 showed no key)
fn link_keys(config: &std::path::Path) -> FileLinkKeyStore {
    FileLinkKeyStore::open(&bluecaster_core::paths::link_keys_file(config))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// device.pairing: the link keys file is written in full on every change,
/// also while a stack runs, so it is read from there
pub fn pairing(state: &CoreState, addr: &str) -> Result<Value, ErrorInfo> {
    developer(state)?;
    let addr = address(addr)?;
    let store = link_keys(&state.config_dir());
    let link_key = store
        .entries()
        .iter()
        .find(|e| e.addr == addr)
        .map(|e| LinkKeyInfo { key: hex(&e.key), key_type: e.key_type.clamp(0, 255) as u8 });
    // Held by the running stack (its bond file is open there)
    let (le_keys, le_keys_missing) = match state.service.host().and_then(|h| h.le_keys(addr)) {
        Some(Ok(k)) => (Some(le_keys_info(k)), None),
        Some(Err(why)) => (None, Some(missing(why))),
        None => (None, Some(LeKeysMissing::NoStack)),
    };
    Ok(serde_json::to_value(DevicePairing { link_key, le_keys, le_keys_missing }).unwrap_or(Value::Null))
}

fn missing(why: bt_host::CtkdOutcome) -> LeKeysMissing {
    use bt_host::CtkdOutcome as O;
    match why {
        O::NoLe => LeKeysMissing::NoLe,
        O::NotAsked => LeKeysMissing::NotAsked,
        O::Asking => LeKeysMissing::Asking,
        O::NotSecure => LeKeysMissing::NotSecure,
        O::DeviceLed => LeKeysMissing::DeviceLed,
        O::NoSupport => LeKeysMissing::NoSupport,
        O::Refused => LeKeysMissing::Refused,
        O::NoAnswer => LeKeysMissing::NoAnswer,
        O::NoneGiven => LeKeysMissing::NoneGiven,
    }
}

/// BTstack keeps the LTK and IRK most significant octet first: shown least
/// significant first, the order SMP and HCI carry them in (as the link key)
fn le_keys_info(k: bt_host::LeKeys) -> LeKeysInfo {
    let wire = |key: [u8; 16]| {
        let mut key = key;
        key.reverse();
        hex(&key)
    };
    LeKeysInfo {
        long_term_key: k.ltk.map(wire),
        key_size: k.key_size,
        identity_resolving_key: k.irk.map(wire),
        identity_address: bt_host::addr_str(&k.identity_addr),
        identity_random: k.identity_addr_type & 0x01 != 0,
    }
}

/// Each device's services from its last answer (by upper-case address),
/// shown while it is not connected; kept in the settings folder so a
/// restart keeps them too (issue #86)
type Seen = BTreeMap<String, Vec<DeviceService>>;

/// One reader or writer of the file at a time (each request runs on a
/// thread of its own)
static SEEN_FILE: Mutex<()> = Mutex::new(());

fn load_seen(config: &Path) -> Seen {
    std::fs::read_to_string(paths::device_services_file(config))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_seen(config: &Path, seen: &Seen) {
    let path = paths::device_services_file(config);
    let written = serde_json::to_string_pretty(seen).map_err(std::io::Error::other).and_then(|text| {
        std::fs::create_dir_all(paths::devices_dir(config))?;
        std::fs::write(&path, text)
    });
    if let Err(e) = written {
        tracing::warn!("cannot keep the services in {}: {e}", path.display());
    }
}

fn last_services(config: &Path, addr: &[u8; 6]) -> Option<Vec<DeviceService>> {
    let _one = SEEN_FILE.lock().unwrap_or_else(|e| e.into_inner());
    load_seen(config).remove(&bt_host::addr_str(addr).to_uppercase())
}

fn keep_services(config: &Path, addr: &[u8; 6], services: &[DeviceService]) {
    let _one = SEEN_FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut seen = load_seen(config);
    let key = bt_host::addr_str(addr).to_uppercase();
    if seen.get(&key).map(Vec::as_slice) != Some(services) {
        seen.insert(key, services.to_vec());
        save_seen(config, &seen);
    }
}

/// A forgotten device's services go with it (an LE Audio entry's kept
/// GATT services too)
pub fn forget_services(config: &Path, address: &str, le_audio: bool) {
    if le_audio {
        bt_host::le_gatt_cache::remove(&le_cache_file(config), address);
        return;
    }
    let _one = SEEN_FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut seen = load_seen(config);
    if seen.remove(&address.to_uppercase()).is_some() {
        save_seen(config, &seen);
    }
}

fn list(services: Vec<DeviceService>, from_last_connection: bool) -> Value {
    serde_json::to_value(ServiceList { services, from_last_connection }).unwrap_or(Value::Null)
}

fn le_cache_file(config: &Path) -> std::path::PathBuf {
    paths::pairing_dir(config).join(bt_host::le_gatt_cache::FILE_NAME)
}

/// An LE Audio entry's services: the GATT services kept at its last
/// connection (bt_host::le_gatt_cache), "from the last connection" unless
/// it is connected now
fn le_services(state: &CoreState, addr: &[u8; 6]) -> Result<Value, ErrorInfo> {
    let text = bt_host::addr_str(addr).to_uppercase();
    let Some(uuids) = bt_host::le_gatt_cache::service_uuids(&le_cache_file(&state.config_dir()), &text) else {
        return Err(err("device.not_connected", "ipc.error.services_not_connected"));
    };
    let connected =
        state.service.host().is_some_and(|h| h.le_devices().iter().any(|d| d.connected && d.addr.addr == *addr));
    let services = uuids.into_iter().map(|u| DeviceService { uuids: vec![u], name: None }).collect();
    Ok(list(services, !connected))
}

/// device.services: over the link the running stack has to the device; not
/// connected, the list it gave last (not developer mode only since issue
/// #86: the tag is out of the way, the keys stay in developer mode). An LE
/// Audio entry: its kept GATT services
pub fn services(state: &CoreState, addr: &str, le_audio: bool) -> Result<Value, ErrorInfo> {
    let addr = address(addr)?;
    if le_audio {
        return le_services(state, &addr);
    }
    let config = state.config_dir();
    let not_connected = || match last_services(&config, &addr) {
        Some(last) => Ok(list(last, true)),
        None => Err(err("device.not_connected", "ipc.error.services_not_connected")),
    };
    let Some(host) = state.service.host() else {
        return not_connected();
    };
    let records = match host.device_services(addr) {
        Ok(r) => r,
        Err(ServicesError::NotConnected) => return not_connected(),
        Err(ServicesError::Busy) => return Err(err("device.busy", "ipc.error.services_busy")),
        Err(e @ (ServicesError::NoAnswer | ServicesError::Failed(_))) => {
            tracing::info!("services of {}: {e:?}", bt_host::addr_str(&addr));
            return Err(err("device.no_answer", "ipc.error.services_no_answer"));
        }
    };
    let services: Vec<DeviceService> =
        records.into_iter().map(|r| DeviceService { uuids: r.classes, name: r.name }).collect();
    keep_services(&config, &addr, &services);
    Ok(list(services, false))
}

/// One key of the helper's answer
fn parse_key(v: &Value) -> Option<([u8; 6], [u8; 16])> {
    let addr = bt_host::parse_addr(v.get("address")?.as_str()?)?;
    let text = v.get("key")?.as_str()?;
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    let mut key = [0u8; 16];
    for (i, k) in key.iter_mut().enumerate() {
        *k = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some((addr, key))
}

/// One LE device of the helper's answer (Windows' values), as BTstack keeps
/// its bond: LTK and IRK reversed (Windows: least significant octet first)
fn parse_le_key(v: &Value) -> Option<([u8; 6], bt_host::ImportedLeBond)> {
    let address = bt_host::parse_addr(v.get("address")?.as_str()?)?;
    let key = |name: &str| -> Option<[u8; 16]> {
        let text = v.get(name)?.as_str()?;
        if text.len() != 32 || !text.is_ascii() {
            return None;
        }
        let mut key = [0u8; 16];
        for (i, k) in key.iter_mut().enumerate() {
            *k = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
        }
        key.reverse();
        Some(key)
    };
    let number = |name: &str| v.get(name).and_then(Value::as_u64);
    // 0 public, 1 random (static); Windows writes nothing else
    let identity_addr_type = match number("address_type").unwrap_or(0) {
        t @ (0 | 1) => t as u8,
        _ => return None,
    };
    let ltk = key("ltk")?;
    let key_size = number("key_length").filter(|k| (7..=16).contains(k)).map_or(16, |k| k as u8);
    let ediv = u16::try_from(number("ediv").unwrap_or(0)).ok()?;
    let rand = match v.get("erand").and_then(Value::as_str) {
        Some(text) => u64::from_str_radix(text, 16).ok()?,
        None => 0,
    };
    let auth_req = number("auth_req").unwrap_or(0);
    Some((
        address,
        bt_host::ImportedLeBond {
            identity_addr_type,
            identity_addr: address,
            irk: key("irk").unwrap_or_default(),
            ltk,
            key_size,
            ediv,
            rand: rand.to_be_bytes(),
            // SMP AuthReq: MITM, Secure Connections (keys of LE Secure
            // Connections have no EDIV and Rand)
            authenticated: auth_req & 0x04 != 0,
            secure_connection: auth_req & 0x08 != 0 || (ediv == 0 && rand == 0),
        },
    ))
}

/// The classic device among `classic` an LE device of Windows' belongs to:
/// the same address, else the one Windows' device nodes put it with
fn classic_of(le: [u8; 6], classic: &[[u8; 6]], pairs: &[([u8; 6], [u8; 6])]) -> Option<[u8; 6]> {
    if classic.contains(&le) {
        return Some(le);
    }
    pairs.iter().find(|(l, c)| *l == le && classic.contains(c)).map(|(_, c)| *c)
}

/// Windows' LE keys of the classic devices `same_pairing` (issue #86: a
/// device that already gave Windows its LE keys may refuse to give them
/// again on a link it is paired on). Added to the running stack's LE bonds,
/// else kept until a stack with LE starts.
fn import_le_keys(state: &CoreState, result: &Value, same_pairing: &[[u8; 6]], pairs: &[([u8; 6], [u8; 6])]) {
    let le: Vec<([u8; 6], bt_host::ImportedLeBond)> =
        result.get("le_keys").and_then(Value::as_array).into_iter().flatten().filter_map(parse_le_key).collect();
    let pending = bluecaster_core::paths::windows_le_keys_file(&state.config_dir());
    let host = state.service.host();
    for (le_addr, bond) in le {
        let Some(classic) = classic_of(le_addr, same_pairing, pairs) else {
            tracing::info!("Windows' LE keys of {}: no classic device of this pairing", bt_host::addr_str(&le_addr));
            continue;
        };
        match host.as_ref().and_then(|h| h.add_le_bond(classic, bond)) {
            // Kept: BlueCaster's own are the newer ones
            Some(bt_host::LeBondAdded::Added | bt_host::LeBondAdded::Kept | bt_host::LeBondAdded::Failed) => {}
            // LE off (or no stack): added when it starts
            Some(bt_host::LeBondAdded::NoLe) | None => {
                bt_host::le_import::add(&pending, bt_host::le_import::PendingLeBond { classic, bond });
                tracing::info!("Windows' LE keys of {}: kept until LE starts", bt_host::addr_str(&classic));
            }
        }
    }
}

/// device.import_windows_pairings (not developer mode only since issue #86:
/// it only reads Windows' keys, after an administrator prompt)
pub fn import_windows(state: &Arc<CoreState>, adapter: &str, only: Option<&[String]>) -> Result<Value, ErrorInfo> {
    let radio = bt_host::addr_str(&address(adapter)?);
    // The devices ticked in the setup's list (issue #86)
    let only: Option<Vec<[u8; 6]>> = only.map(|list| list.iter().map(|a| address(a)).collect()).transpose()?;
    let (code, result) = crate::driver::run(&state.config_dir(), &["windows-keys", &radio])?;
    if code != 0 || result.get("ok").and_then(Value::as_bool) != Some(true) {
        tracing::warn!("Windows' keys of {radio}: helper exit {code}");
        return Err(err("adapter.import_failed", "ipc.error.import_failed"));
    }
    let adapter_found = result.get("radio_found").and_then(Value::as_bool) == Some(true);
    let keys: Vec<([u8; 6], [u8; 16])> = result
        .get("keys")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_key)
        .filter(|(a, _)| only.as_ref().is_none_or(|o| o.contains(a)))
        .collect();
    let dir = state.config_dir();
    let have: Vec<([u8; 6], [u8; 16])> = link_keys(&dir).entries().iter().map(|e| (e.addr, e.key)).collect();
    let (mut imported, mut kept) = (Vec::new(), Vec::new());
    // The devices whose key now is Windows' (taken now or before): Windows'
    // LE keys of them come from the same pairing
    let mut same_pairing: Vec<[u8; 6]> = Vec::new();
    let host = state.service.host();
    for (addr, key) in keys {
        if let Some((_, ours)) = have.iter().find(|(a, _)| *a == addr) {
            kept.push(bt_host::addr_str(&addr));
            if *ours == key {
                same_pairing.push(addr);
            }
            continue;
        }
        // A running stack holds the keys in memory and writes them in full
        let stored = match &host {
            Some(h) => h.store_link_key(addr, key, WINDOWS_KEY_TYPE),
            None => {
                bt_host::link_keys::store(&bluecaster_core::paths::link_keys_file(&dir), &addr, &key, WINDOWS_KEY_TYPE);
                true
            }
        };
        if stored {
            imported.push(bt_host::addr_str(&addr));
            same_pairing.push(addr);
        } else {
            tracing::warn!("Windows' key of {} not stored: the stack did not answer", bt_host::addr_str(&addr));
        }
    }
    let le_pairs = state.adapter_instance(adapter).map(|id| win_sys::bt_paired::le_of_classic_through(&id));
    import_le_keys(state, &result, &same_pairing, le_pairs.as_deref().unwrap_or_default());
    // A kept key (taken before, its device since forgotten) still adds its
    // device: the button stays while one is unsaved
    let have_key: Vec<String> = imported.iter().chain(&kept).cloned().collect();
    let added = match state.adapter_instance(adapter) {
        Some(instance_id) => add_audio_devices(state, &instance_id, &have_key),
        None => Vec::new(),
    };
    tracing::info!(
        "Windows' keys of {radio}: adapter found {adapter_found}, {} taken, {} kept, {} added",
        imported.len(),
        kept.len(),
        added.len()
    );
    Ok(serde_json::to_value(WindowsImport { adapter_found, imported, kept, added }).unwrap_or(Value::Null))
}

/// The audio devices Windows paired through the adapter's USB device
/// `instance_id` that have a key in `with_key` and are not saved yet, saved
/// with the name Windows knows them by (they then show on Audio Devices,
/// ready to connect); the addresses added. Read from Windows' device nodes,
/// which stay while the adapter is on WinUSB (Windows' device search needs
/// a running radio).
fn add_audio_devices(state: &Arc<CoreState>, instance_id: &str, with_key: &[String]) -> Vec<String> {
    let windows = win_sys::bt_paired::paired_through(instance_id);
    let dir = state.config_dir();
    let saved: Vec<String> =
        bluecaster_core::devices::load(&dir).into_iter().map(|d| d.address.to_uppercase()).collect();
    let mut added = Vec::new();
    for d in windows.iter().filter(|d| d.is_a2dp_sink()) {
        let address = bt_host::addr_str(&d.address);
        if !with_key.contains(&address) || saved.contains(&address.to_uppercase()) {
            continue;
        }
        match bluecaster_core::devices::save(&dir, &address, d.name.trim()) {
            Ok(()) => added.push(address),
            Err(e) => tracing::warn!("Windows' device {address} not saved: {e}"),
        }
    }
    if !added.is_empty() {
        state.refresh_devices();
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_keys_shown_in_wire_order() {
        let mut ltk = [0u8; 16];
        ltk[0] = 0xAB; // most significant, as BTstack keeps it
        let k = bt_host::LeKeys {
            identity_addr_type: 1,
            identity_addr: [0xC0, 0x11, 0x22, 0x33, 0x44, 0x55],
            irk: None,
            ltk: Some(ltk),
            key_size: 16,
        };
        let info = le_keys_info(k);
        assert_eq!(info.long_term_key.as_deref(), Some("000000000000000000000000000000ab"));
        assert_eq!(info.identity_resolving_key, None);
        assert_eq!(info.identity_address, "C0:11:22:33:44:55");
        assert!(info.identity_random);
        assert_eq!(info.key_size, 16);
    }

    #[test]
    fn link_keys_are_read_from_the_pairing_folder() {
        let config = std::env::temp_dir().join(format!("bluecaster-pairing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        let addr = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        bt_host::link_keys::store(&bluecaster_core::paths::link_keys_file(&config), &addr, &[7; 16], 4);
        let found: Vec<[u8; 6]> = link_keys(&config).entries().iter().map(|e| e.addr).collect();
        let _ = std::fs::remove_dir_all(&config);
        assert_eq!(found, vec![addr]);
    }

    #[test]
    fn windows_le_keys_become_btstack_bonds() {
        let v = serde_json::json!({
            "address": "C0:11:22:33:44:55",
            "address_type": 1,
            "ltk": "0102030405060708090a0b0c0d0e0f10",
            "key_length": 0,
            "ediv": 0,
            "erand": "0000000000000000",
            "irk": "ff000000000000000000000000000000",
            "auth_req": 0x2D,
        });
        let (addr, b) = parse_le_key(&v).unwrap();
        assert_eq!(addr, [0xC0, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!((b.identity_addr_type, b.identity_addr), (1, addr));
        // Most significant octet first for BTstack
        assert_eq!((b.ltk[0], b.ltk[15]), (0x10, 0x01));
        assert_eq!((b.irk[0], b.irk[15]), (0x00, 0xFF));
        assert_eq!(b.key_size, 16);
        assert!(b.authenticated && b.secure_connection);
        // Legacy pairing: EDIV and Rand kept, Rand most significant first
        let mut legacy = v.clone();
        legacy["ediv"] = 0x1234.into();
        legacy["erand"] = "0102030405060708".into();
        legacy["auth_req"] = 0x01.into();
        legacy["key_length"] = 7.into();
        legacy.as_object_mut().unwrap().remove("irk");
        let (_, b) = parse_le_key(&legacy).unwrap();
        assert_eq!((b.ediv, b.rand, b.key_size), (0x1234, [1, 2, 3, 4, 5, 6, 7, 8], 7));
        assert!(!b.authenticated && !b.secure_connection);
        assert_eq!(b.irk, [0; 16]);
        // No LTK, an unknown address type
        let mut bad = v.clone();
        bad.as_object_mut().unwrap().remove("ltk");
        assert!(parse_le_key(&bad).is_none());
        let mut bad = v;
        bad["address_type"] = 2.into();
        assert!(parse_le_key(&bad).is_none());
    }

    #[test]
    fn le_keys_go_with_their_classic_device() {
        let classic = [[0x58, 1, 1, 1, 1, 1], [0x58, 2, 2, 2, 2, 2]];
        let pairs = [([0xC0, 1, 1, 1, 1, 1], [0x58, 1, 1, 1, 1, 1]), ([0xC0, 3, 3, 3, 3, 3], [0x58, 3, 3, 3, 3, 3])];
        // The same address, a node pair, a device of another pairing
        assert_eq!(classic_of([0x58, 2, 2, 2, 2, 2], &classic, &pairs), Some([0x58, 2, 2, 2, 2, 2]));
        assert_eq!(classic_of([0xC0, 1, 1, 1, 1, 1], &classic, &pairs), Some([0x58, 1, 1, 1, 1, 1]));
        assert_eq!(classic_of([0xC0, 3, 3, 3, 3, 3], &classic, &pairs), None);
        assert_eq!(classic_of([0xC0, 9, 9, 9, 9, 9], &classic, &pairs), None);
    }

    #[test]
    fn last_services_outlive_the_core_until_forgotten() {
        let config = std::env::temp_dir().join(format!("bluecaster-services-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        let addr = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x0F];
        let services = vec![DeviceService { uuids: vec!["0000110b-0000-1000-8000-00805f9b34fb".into()], name: None }];
        assert_eq!(last_services(&config, &addr), None);
        keep_services(&config, &addr, &services);
        // Read back from the file, as after a restart
        assert_eq!(last_services(&config, &addr), Some(services));
        // Forgetting the LE Audio entry leaves the classic one's list
        forget_services(&config, "aa:bb:cc:dd:ee:0f", true);
        assert!(last_services(&config, &addr).is_some());
        forget_services(&config, "aa:bb:cc:dd:ee:0f", false);
        let left = last_services(&config, &addr);
        let _ = std::fs::remove_dir_all(&config);
        assert_eq!(left, None);
    }

    #[test]
    fn helper_keys_are_read_strictly() {
        let good = serde_json::json!({ "address": "00:A2:D0:00:00:02", "key": "00112233445566778899aabbccddeeff" });
        let (addr, key) = parse_key(&good).unwrap();
        assert_eq!(addr, [0x00, 0xA2, 0xD0, 0x00, 0x00, 0x02]);
        assert_eq!((key[0], key[15]), (0x00, 0xFF));
        for bad in [
            serde_json::json!({ "address": "00:A2:D0:00:00", "key": "00112233445566778899aabbccddeeff" }),
            serde_json::json!({ "address": "00:A2:D0:00:00:02", "key": "00112233445566778899aabbccddee" }),
            serde_json::json!({ "address": "00:A2:D0:00:00:02", "key": "0011223344556677889\u{e9}abbccddeeff" }),
            serde_json::json!({ "address": "00:A2:D0:00:00:02" }),
        ] {
            assert_eq!(parse_key(&bad), None, "{bad}");
        }
    }
}

//! WinUSB assignment and restore through the elevated helper
//! bluecaster-driver.exe (the core itself never runs as administrator).
//!
//! The helper writes its JSON result to a file we name (an elevated process
//! started with ShellExecuteEx gives no stdout). The "previous driver"
//! record of an assignment is kept in the settings folder's
//! `adapters\driver\`, per adapter, for the restore (and the uninstaller).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ipc::{ErrorInfo, Msg};
use serde_json::Value;
use win_sys::bt_radios::WindowsRadio;
use win_sys::elevation::{self, ElevationError};

use crate::state::CoreState;

fn err(code: &str, key: &str) -> ErrorInfo {
    ErrorInfo { code: code.into(), message: Msg::new(key) }
}

fn helper() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("bluecaster-driver.exe")))
        .unwrap_or_else(|| "bluecaster-driver.exe".into())
}

/// File of the assignment record of an adapter
fn record_path(state: &CoreState, instance_id: &str) -> PathBuf {
    bluecaster_core::paths::driver_record_file(&state.config_dir(), instance_id)
}

pub(crate) fn run(config: &Path, args: &[&str]) -> Result<(u32, Value), ErrorInfo> {
    let out = std::env::temp_dir().join(format!("bluecaster-driver-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let mut all: Vec<&str> = args.to_vec();
    let out_s = out.to_string_lossy().into_owned();
    all.extend(["--out", &out_s]);
    // The elevated run's log next to the core's (the helper appends to it,
    // in a folder that must exist)
    let log = bluecaster_core::paths::driver_log_file(config, &bluecaster_core::paths::today());
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let log_s = log.to_string_lossy().into_owned();
    all.extend(["--log", &log_s]);
    if crate::logging::verbose_now().is_some() {
        all.push("--verbose");
    }
    let code = elevation::run_elevated(&helper(), &all).map_err(|e| match e {
        ElevationError::Cancelled => err("adapter.uac_cancelled", "error.uac_cancelled"),
        _ => {
            tracing::error!("driver helper: {e}");
            err("adapter.driver_failed", "error.driver_failed")
        }
    })?;
    let json: Option<Value> =
        std::fs::read_to_string(&out).ok().and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok());
    let _ = std::fs::remove_file(&out);
    // windows-keys and restore give link keys back: not for the log
    let shown = json.as_ref().map(without_keys);
    tracing::info!("driver helper {} -> exit {code}: {shown:?}", shown_args(args));
    Ok((code, json.unwrap_or(Value::Null)))
}

/// A helper's command line as the log may show it: the command and the
/// option names, no address, instance ID or path
fn shown_args(args: &[&str]) -> String {
    args.iter()
        .enumerate()
        .filter(|(i, a)| *i == 0 || a.starts_with("--"))
        .map(|(_, a)| *a)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A helper's result as the log may show it: lists of keys by their number
fn without_keys(result: &Value) -> Value {
    let mut shown = result.clone();
    for field in
        ["keys", "le_keys", "replaced_keys", "replaced_le_keys", "keys_written", "keys_same", "le_written", "le_same"]
    {
        if let Some(list) = result.get(field).and_then(Value::as_array) {
            shown[field] = Value::from(list.len());
        }
    }
    // The adapter's instance ID ends in its USB serial number, often its
    // Bluetooth address
    for pointer in ["/instance_id", "/state/instance_id"] {
        if let Some(id) = shown.pointer_mut(pointer).filter(|id| id.is_string()) {
            *id = Value::from("<adapter>");
        }
    }
    shown
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// BlueCaster's LE bonds as Windows keeps LE keys (`windows-keys`'
/// format): LTK and IRK least significant octet first, ERand as a number
fn le_keys_for_windows(bonds: &[bt_host::le_bond_file::StoredLeBond]) -> Vec<Value> {
    bonds
        .iter()
        .map(|b| {
            let wire = |key: [u8; 16]| {
                let mut key = key;
                key.reverse();
                hex(&key)
            };
            serde_json::json!({
                "address": bt_host::addr_str(&b.addr),
                "address_type": b.addr_type,
                "ltk": wire(b.ltk),
                "key_length": b.key_size,
                "ediv": b.ediv,
                "erand": format!("{:016x}", u64::from_be_bytes(b.rand)),
                "irk": (b.irk != [0; 16]).then(|| wire(b.irk)),
            })
        })
        .collect()
}

/// Developer > Debug: the restore hands Windows the LE keys too (issue #86)
fn le_keys_to_windows(state: &CoreState) -> bool {
    let s = state.service.settings();
    s.debug_mode && s.dev_debug.le_keys_to_windows
}

/// BlueCaster's LE bonds from its bond file; None when it cannot be read
fn stored_le_bonds(config: &Path) -> Option<Vec<bt_host::le_bond_file::StoredLeBond>> {
    match bt_host::le_bond_file::read(&bluecaster_core::paths::le_bonds_file(config)) {
        Ok(bonds) => Some(bonds),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
        Err(e) => {
            tracing::warn!("LE bonds not readable: {}", e.kind());
            None
        }
    }
}

/// For `restore --keys`: the adapter's address and BlueCaster's link keys
/// (and its LE keys while developer mode asks for them), written to the
/// pairing folder (never in a bundle); None without any. Plain text: the
/// elevated helper may run as another administrator, whom the current
/// user's DPAPI key does not open; the file is there only while it runs.
fn keys_for_windows(state: &CoreState, instance_id: &str) -> Option<PathBuf> {
    let (_, s) = state.snapshot();
    let radio = s.adapter.adapters.iter().find(|a| a.instance_id == instance_id)?.address.clone()?;
    let config = state.config_dir();
    let store = bt_host::link_keys::FileLinkKeyStore::open(&bluecaster_core::paths::link_keys_file(&config));
    let keys: Vec<Value> = store
        .entries()
        .iter()
        .map(|e| serde_json::json!({ "address": bt_host::addr_str(&e.addr), "key": hex(&e.key) }))
        .collect();
    let le = if le_keys_to_windows(state) {
        let bonds = stored_le_bonds(&config).unwrap_or_default();
        tracing::info!("driver: developer mode hands Windows {} LE bond(s) as well", bonds.len());
        le_keys_for_windows(&bonds)
    } else {
        Vec::new()
    };
    if keys.is_empty() && le.is_empty() {
        return None;
    }
    let path = bluecaster_core::paths::keys_for_windows_file(&config);
    let text = serde_json::to_vec(&serde_json::json!({ "radio": radio, "keys": keys, "le_keys": le })).ok()?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(&path, text) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("the keys for Windows could not be written ({}); Windows keeps its own", e.kind());
            None
        }
    }
}

/// Windows' keys that BlueCaster's replaced, added to the backup in the
/// pairing folder (per device and kind the one replaced last), encrypted
/// for the Windows user: Windows kept them readable by SYSTEM only. The
/// log gets numbers only; the LE line only when LE keys were asked for.
fn keep_replaced_keys(config: &Path, instance_id: &str, result: &Value, le_asked: bool) {
    if let Some(e) = result.get("keys_error").and_then(Value::as_str) {
        tracing::warn!("driver: keys not written to Windows: {e}");
    }
    let count = |field: &str| result.get(field).and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let number = |field: &str| result.get(field).and_then(Value::as_u64).unwrap_or(0);
    if result.get("keys_written").is_some() {
        tracing::info!(
            "driver: link keys to Windows: {} written ({} read back as written), {} the same already",
            count("keys_written"),
            number("keys_verified"),
            count("keys_same")
        );
    }
    if le_asked && result.get("le_written").is_some() {
        tracing::info!(
            "driver: LE keys to Windows: {} written ({} read back as written), {} the same already",
            count("le_written"),
            number("le_verified"),
            count("le_same")
        );
    }
    let entries: Vec<(&str, &Value)> = [("link", "replaced_keys"), ("le", "replaced_le_keys")]
        .into_iter()
        .flat_map(|(kind, field)| {
            result.get(field).and_then(Value::as_array).into_iter().flatten().map(move |k| (kind, k))
        })
        .collect();
    if entries.is_empty() {
        return;
    }
    let path = bluecaster_core::paths::replaced_windows_keys_file(config);
    let mut backup: Vec<Value> =
        win_sys::dpapi::read_secret(&path).ok().and_then(|t| serde_json::from_slice(&t).ok()).unwrap_or_default();
    let date = bluecaster_core::paths::today();
    let kind_of = |b: &Value| b.get("kind").and_then(Value::as_str).unwrap_or("link").to_string();
    for (kind, k) in entries {
        let address = k.get("address").and_then(Value::as_str).unwrap_or_default();
        backup.retain(|b| !(b.get("address").and_then(Value::as_str) == Some(address) && kind_of(b) == kind));
        let mut entry = k.clone();
        entry["kind"] = Value::String(kind.to_string());
        entry["adapter"] = Value::String(instance_id.to_string());
        entry["date"] = Value::String(date.clone());
        backup.push(entry);
    }
    if let Err(e) = win_sys::dpapi::write_secret(&path, &serde_json::to_vec_pretty(&backup).unwrap_or_default()) {
        tracing::warn!("Windows' replaced keys could not be kept ({})", e.kind());
    }
}

pub fn assign(state: &Arc<CoreState>, instance_id: &str) -> Result<Value, ErrorInfo> {
    // The stack must let go of the adapter (it may be ours already)
    state.service.release_adapter();
    let (code, result) = run(&state.config_dir(), &["assign", instance_id])?;
    if code != 0 {
        state.refresh_adapters();
        // Some environments refuse DiInstallDevice with the inbox winusb.inf;
        // Zadig's signed package is the way out there (docs/setup.md#zadig)
        return Err(err("adapter.driver_failed", "error.driver_assign_failed"));
    }
    let path = record_path(state, instance_id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, serde_json::to_vec_pretty(&result).unwrap_or_default()) {
        tracing::warn!("cannot keep the driver record {}: {e}", path.display());
    }
    let mut adapters = state.refresh_adapters();
    state.driver_changed();
    // Even after the helper restarted the device, Windows may load WinUSB
    // only after a replug or a restart (issue #81: until then opening the
    // adapter fails with "failed to initialize WinUSB (error 50)")
    if result.get("reboot_required").and_then(Value::as_bool) == Some(true) {
        tracing::warn!(
            "driver: WinUSB takes effect for {instance_id} after the adapter is replugged or Windows restarts"
        );
        for a in adapters.iter_mut().filter(|a| a.instance_id == instance_id) {
            a.restart_needed = true;
        }
    }
    Ok(serde_json::to_value(adapters).unwrap_or(Value::Null))
}

/// Devices (upper-case address) and what to compare of their keys
type KeyList = Vec<(String, String)>;

/// How `ours` and `windows` agree, device by device
fn count_keys(ours: &KeyList, windows: &KeyList) -> ipc::KeyCounts {
    let mut c = ipc::KeyCounts::default();
    for (address, key) in ours {
        match windows.iter().find(|(a, _)| a == address) {
            Some((_, k)) if k == key => c.same += 1,
            Some(_) => c.differ += 1,
            None => c.only_bluecaster += 1,
        }
    }
    c.only_windows = windows.iter().filter(|(a, _)| !ours.iter().any(|(o, _)| o == a)).count() as u32;
    c
}

/// An LE device's keys as compared: LTK, EDIV and ERand in Windows' form
fn le_entry(v: &Value) -> Option<(String, String)> {
    let address = v.get("address")?.as_str()?.to_uppercase();
    let text = |name: &str| v.get(name).and_then(Value::as_str).unwrap_or_default().to_lowercase();
    let ediv = v.get("ediv").and_then(Value::as_u64).unwrap_or(0);
    let erand = match text("erand") {
        e if e.is_empty() => "0000000000000000".to_string(),
        e => e,
    };
    Some((address, format!("{} {ediv} {erand}", text("ltk"))))
}

/// dev.compare_windows_keys: BlueCaster's keys against Windows' for the
/// adapter `adapter` (the helper reads Windows', one administrator
/// prompt). Numbers only, in the answer and in the log.
pub fn compare_windows_keys(state: &Arc<CoreState>, adapter: &str) -> Result<Value, ErrorInfo> {
    if !state.service.settings().debug_mode {
        return Err(err("developer.off", "ipc.error.developer_only"));
    }
    let radio = bt_host::parse_addr(adapter)
        .map(|a| bt_host::addr_str(&a))
        .ok_or_else(|| err("device.invalid_address", "error.invalid_address_format"))?;
    let (code, result) = run(&state.config_dir(), &["windows-keys", &radio])?;
    if code != 0 || result.get("ok").and_then(Value::as_bool) != Some(true) {
        tracing::warn!("keys compared with Windows: helper exit {code}");
        return Err(err("adapter.import_failed", "ipc.error.import_failed"));
    }
    let list = |field: &str| result.get(field).and_then(Value::as_array).cloned().unwrap_or_default();
    let windows_classic: KeyList = list("keys")
        .iter()
        .filter_map(|k| Some((k.get("address")?.as_str()?.to_uppercase(), k.get("key")?.as_str()?.to_lowercase())))
        .collect();
    let windows_le: KeyList = list("le_keys").iter().filter_map(le_entry).collect();
    let config = state.config_dir();
    let store = bt_host::link_keys::FileLinkKeyStore::open(&bluecaster_core::paths::link_keys_file(&config));
    let ours_classic: KeyList =
        store.entries().iter().map(|e| (bt_host::addr_str(&e.addr).to_uppercase(), hex(&e.key))).collect();
    let le = stored_le_bonds(&config).map(|bonds| {
        let ours: KeyList = le_keys_for_windows(&bonds).iter().filter_map(le_entry).collect();
        count_keys(&ours, &windows_le)
    });
    let names = |field: &str| -> Vec<ipc::WindowsValueName> {
        list(field).iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect()
    };
    let comparison = ipc::KeysComparison {
        adapter_found: result.get("radio_found").and_then(Value::as_bool) == Some(true),
        classic: count_keys(&ours_classic, &windows_classic),
        le,
        radio_values: names("radio_values"),
        le_values: names("le_values"),
    };
    tracing::info!("keys compared with Windows: {comparison:?}");
    Ok(serde_json::to_value(comparison).unwrap_or(Value::Null))
}

pub fn restore(state: &Arc<CoreState>, instance_id: &str) -> Result<Value, ErrorInfo> {
    state.service.release_adapter();
    let path = record_path(state, instance_id);
    let previous = path.exists().then(|| format!("@{}", path.display()));
    let mut args = vec!["restore", instance_id];
    if let Some(p) = previous.as_deref() {
        args.extend(["--previous", p]);
    }
    // BlueCaster's pairings go to Windows too: the devices it paired since
    // gave up Windows' older keys (issue #86)
    let keys = keys_for_windows(state, instance_id);
    let keys_arg = keys.as_ref().map(|p| p.to_string_lossy().into_owned());
    if let Some(k) = keys_arg.as_deref() {
        args.extend(["--keys", k]);
    }
    let ran = run(&state.config_dir(), &args);
    if let Some(k) = &keys {
        let _ = std::fs::remove_file(k);
    }
    let (code, result) = ran?;
    keep_replaced_keys(&state.config_dir(), instance_id, &result, le_keys_to_windows(state));
    state.refresh_adapters();
    match code {
        0 => {
            let _ = std::fs::remove_file(&path);
            state.driver_changed();
            if let Some(other) = other_radio_in_use(instance_id) {
                return Err(ErrorInfo {
                    code: "adapter.restore_other_radio".into(),
                    message: Msg { key: "error.driver_restore_other_radio".into(), args: vec![other] },
                });
            }
            Ok(serde_json::to_value(state.snapshot().1.adapter.adapters).unwrap_or(Value::Null))
        }
        // Done, but not on the recorded driver (e.g. Zadig's WinUSB package
        // still in the driver store outranks Windows' Bluetooth driver)
        5 => Err(err("adapter.restore_unverified", "error.driver_restore_unverified")),
        _ => Err(err("adapter.driver_failed", "error.driver_failed")),
    }
}

/// After a restore that left the adapter on its driver but not working:
/// the name of another radio Windows' Bluetooth runs. Windows runs one
/// radio at a time (BTHUSB event 6, the adapter fails with Code 31), so
/// the restored one stays off until that one is disabled.
fn other_radio_in_use(instance_id: &str) -> Option<String> {
    let state = win_sys::driver::driver_state(instance_id).ok()?;
    let name = blocking_radio(instance_id, state.problem_code, &win_sys::bt_radios::radios())?;
    tracing::warn!(
        "driver: {instance_id} is back on {} but has problem code {:?}; Windows runs {name:?}",
        state.driver.service,
        state.problem_code
    );
    Some(name)
}

/// The radio that keeps a failed adapter off: one not on that adapter
fn blocking_radio(instance_id: &str, problem: Option<u32>, radios: &[WindowsRadio]) -> Option<String> {
    problem?;
    let other = radios.iter().find(|r| !r.on_device(instance_id))?;
    Some(if other.name.is_empty() {
        other.instance_ids.first().cloned().unwrap_or_default()
    } else {
        other.name.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(name: &str, ids: &[&str]) -> WindowsRadio {
        WindowsRadio {
            instance_ids: ids.iter().map(|s| s.to_string()).collect(),
            name: name.into(),
            address: None,
            hci_version: None,
            lmp_version: None,
        }
    }

    const TPLINK: &str = r"USB\VID_2357&PID_0604\E848B8C82000";
    const INTEL: &str = r"USB\VID_8087&PID_0033\5&3B72E4CF&0&14";

    #[test]
    fn replaced_windows_keys_are_kept_but_never_logged() {
        let config = std::env::temp_dir().join(format!("bluecaster-replaced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(bluecaster_core::paths::pairing_dir(&config)).unwrap();
        let result = |key: &str| {
            serde_json::json!({
                "ok": true,
                "keys_written": ["AA:BB:CC:DD:EE:FF"],
                "keys_same": [],
                "replaced_keys": [{ "address": "AA:BB:CC:DD:EE:FF", "key": key }],
            })
        };
        let shown = without_keys(&result("11").clone()).to_string();
        assert!(!shown.contains("\"11\"") && !shown.contains("AA:BB"), "{shown}");
        keep_replaced_keys(&config, TPLINK, &result("11"), false);
        // Replaced again: the one replaced last is kept
        keep_replaced_keys(&config, TPLINK, &result("22"), false);
        // Nothing replaced: the backup stays
        keep_replaced_keys(&config, TPLINK, &serde_json::json!({ "keys_written": [] }), false);
        let path = bluecaster_core::paths::replaced_windows_keys_file(&config);
        let on_disk = std::fs::read(&path).unwrap();
        let backup: Vec<Value> = serde_json::from_slice(&win_sys::dpapi::read_secret(&path).unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&config);
        assert!(win_sys::dpapi::is_encrypted(&on_disk));
        assert!(!String::from_utf8_lossy(&on_disk).contains("AA:BB"));
        assert_eq!(backup.len(), 1);
        assert_eq!(backup[0]["key"], "22");
        assert_eq!(backup[0]["adapter"], TPLINK);
        assert_eq!(backup[0]["kind"], "link");
    }

    #[test]
    fn keys_are_counted_without_naming_them() {
        let list = |v: &[(&str, &str)]| v.iter().map(|(a, k)| (a.to_string(), k.to_string())).collect::<KeyList>();
        let ours = list(&[("A", "1"), ("B", "2"), ("C", "3")]);
        let windows = list(&[("A", "1"), ("B", "9"), ("D", "4")]);
        assert_eq!(
            count_keys(&ours, &windows),
            ipc::KeyCounts { same: 1, differ: 1, only_bluecaster: 1, only_windows: 1 }
        );
        // An LE device: LTK, EDIV and ERand; a missing ERand is 0
        let w = serde_json::json!({ "address": "c0:11:22:33:44:55", "ltk": "AB", "ediv": 0 });
        let o =
            serde_json::json!({ "address": "C0:11:22:33:44:55", "ltk": "ab", "ediv": 0, "erand": "0000000000000000" });
        assert_eq!(le_entry(&w), le_entry(&o));
        let other = serde_json::json!({ "address": "C0:11:22:33:44:55", "ltk": "ab", "ediv": 1 });
        assert_ne!(le_entry(&other), le_entry(&o));
    }

    #[test]
    fn le_bonds_go_to_windows_in_its_order() {
        let mut ltk = [0u8; 16];
        ltk[0] = 0xAB; // most significant, as BTstack keeps it
        let bond = bt_host::le_bond_file::StoredLeBond {
            addr_type: 1,
            addr: [0xC0, 0x11, 0x22, 0x33, 0x44, 0x55],
            irk: [0; 16],
            ltk,
            ediv: 0x1234,
            rand: [1, 2, 3, 4, 5, 6, 7, 8],
            key_size: 16,
            authenticated: false,
            secure_connection: false,
        };
        let v = &le_keys_for_windows(&[bond])[0];
        assert_eq!(v["address"], "C0:11:22:33:44:55");
        assert_eq!(v["ltk"], "000000000000000000000000000000ab");
        assert_eq!(v["erand"], "0102030405060708");
        assert_eq!(
            (v["ediv"].as_u64(), v["key_length"].as_u64(), v["address_type"].as_u64()),
            (Some(0x1234), Some(16), Some(1))
        );
        // No IRK
        assert!(v["irk"].is_null());
    }

    #[test]
    fn another_radio_keeps_the_restored_adapter_off() {
        let intel = [radio("Intel Wireless Bluetooth", &[INTEL, r"USB\ROOT_HUB30\4&1"])];
        assert_eq!(blocking_radio(TPLINK, Some(31), &intel).as_deref(), Some("Intel Wireless Bluetooth"));
        // Working after the restore: nothing to say
        assert_eq!(blocking_radio(TPLINK, None, &intel), None);
        // Failed, but Windows runs no other radio: not this cause
        assert_eq!(blocking_radio(TPLINK, Some(31), &[radio("TP-Link", &[TPLINK])]), None);
        assert_eq!(blocking_radio(TPLINK, Some(31), &[]), None);
        // A radio without a name is named by its device
        assert_eq!(blocking_radio(TPLINK, Some(31), &[radio("", &[INTEL])]).as_deref(), Some(INTEL));
    }
}

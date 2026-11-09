//! bluecaster-driver.exe: puts a USB Bluetooth adapter on Windows' WinUSB driver
//! and back (win-sys::driver), for the core service and the UI.
//!
//! ```text
//! bluecaster-driver list [--all] [--out <file>]
//! bluecaster-driver state <instance-id> [--out <file>]
//! bluecaster-driver assign <instance-id> [--out <file>]
//! bluecaster-driver restore <instance-id> [--previous <json> | --previous @<file>] [--keys <file>] [--out <file>]
//! bluecaster-driver restore --records <dir> [--unrecorded] [--out <file>]
//! bluecaster-driver unrecorded --records <dir> [--out <file>]
//! bluecaster-driver windows-keys <radio-address> [--out <file>]
//! ```
//!
//! The result is one JSON object on stdout (and in `--out`), logs go to
//! stderr (and to the file of `--log <file>`, at debug level with
//! `--verbose`: every command takes both), nothing is asked interactively. `assign` and `restore` need
//! administrator rights; `list` and `state` do not.
//!
//! `windows-keys` reads the link keys and LE keys Windows' Bluetooth keeps for the radio
//! with that address (win-sys::bt_keys), for the core to take over the
//! devices paired with it. It needs administrator rights too; the keys go to
//! stdout and `--out` only, never to the log.
//!
//! `restore --keys <file>` first writes BlueCaster's link keys from that file
//! (`{"radio": "<address>", "keys": [{"address", "key"}]}`) to Windows,
//! for the devices Windows paired with that radio too, while the adapter is
//! still on WinUSB: Windows' Bluetooth reads them when the radio starts, so
//! those devices connect in Windows without pairing again. The keys Windows
//! had are given back as `replaced_keys` (stdout and `--out` only). With
//! `le_keys` in the file (as `windows-keys` prints them; developer mode
//! only) the LE keys of the devices Windows keeps LE keys of are written
//! too, Windows' old ones given back as `replaced_le_keys`. Each value
//! written is read back (`keys_verified`, `le_verified`); the log gets
//! numbers only.
//!
//! Elevation: the exe has no manifest asking for administrator rights, so
//! `list` and `state` run without a UAC prompt. The caller starts `assign`
//! and `restore` with win-sys::elevation::run_elevated (ShellExecuteEx
//! "runas"). An elevated process started that way has no stdout the caller
//! can read, hence `--out <file>`: the caller names a file and reads the
//! JSON from it after the exit code. Run unelevated, `assign` and `restore`
//! stop at once with exit code 3.
//!
//! `assign` prints the driver the adapter had before as `previous`; the
//! caller keeps it and passes it back to `restore --previous` (the whole
//! `assign` result is accepted too), which then reports whether the same
//! driver came back.
//!
//! `restore --records <dir>` restores every adapter that has an assignment
//! record in `<dir>` (the core keeps them in `<settings>\driver\`, one
//! `assign` result per adapter) and deletes the records of the adapters
//! given back. The uninstaller uses it: one UAC prompt for all adapters.
//! With `--unrecorded` it also gives back the Bluetooth adapters on WinUSB
//! that have no record (put there by Zadig, v1 or by hand): Windows chooses
//! their driver. `unrecorded --records <dir>` lists those without a UAC
//! prompt (exit code 4 when there is none), for the uninstaller to offer it
//! (issue #86).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use win_sys::driver::{self, DriverError, DriverInfo, DriverRecord, DriverState};
use win_sys::usb_adapters::{self, UsbDevice};

/// Exit codes, also in the JSON as "exit_code"
mod exit {
    pub const OK: u8 = 0;
    /// The operation failed (details in "error")
    pub const FAILED: u8 = 1;
    /// Bad command line
    pub const USAGE: u8 = 2;
    /// assign / restore without administrator rights
    pub const NOT_ELEVATED: u8 = 3;
    /// No such device, or not a USB device
    pub const NO_DEVICE: u8 = 4;
    /// Done, but the result is not what was asked for: after assign the
    /// adapter is not on WinUSB, after restore it did not come back or has
    /// another driver than the recorded one
    pub const UNVERIFIED: u8 = 5;
}

const USAGE: &str = "usage:
  bluecaster-driver list [--all] [--out <file>]
  bluecaster-driver state <instance-id> [--out <file>]
  bluecaster-driver assign <instance-id> [--out <file>]
  bluecaster-driver restore <instance-id> [--previous <json> | --previous @<file>] [--keys <file>] [--out <file>]
  bluecaster-driver restore --records <dir> [--unrecorded] [--out <file>]
  bluecaster-driver unrecorded --records <dir> [--out <file>]
  bluecaster-driver windows-keys <radio-address> [--out <file>]
every command: [--log <file>] [--verbose]";

#[derive(Debug, PartialEq)]
enum Command {
    List { all: bool },
    State { instance_id: String },
    Assign { instance_id: String },
    Restore { instance_id: String, previous: Option<String>, keys: Option<PathBuf> },
    RestoreRecords { dir: PathBuf, unrecorded: bool },
    Unrecorded { dir: PathBuf },
    WindowsKeys { radio: [u8; 6] },
}

#[derive(Debug, PartialEq)]
struct Args {
    command: Command,
    out: Option<PathBuf>,
}

/// A log file this big is moved to `<name>.old` before the next run writes
/// to it
const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

/// `--log <file>` (append the log to it as well) and `--verbose` (debug
/// level): the core passes them so that the elevated run's log ends up in
/// its logs folder. They are taken out before the command is parsed.
fn log_args(argv: Vec<String>) -> (Option<PathBuf>, bool, Vec<String>) {
    let (mut log, mut verbose, mut rest) = (None, false, Vec::new());
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--log" => log = it.next().map(PathBuf::from),
            "--verbose" => verbose = true,
            _ => rest.push(a),
        }
    }
    (log, verbose, rest)
}

fn init_logging(log: Option<&std::path::Path>, verbose: bool) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let level = if verbose { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let file = log.and_then(|path| {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
            let _ = std::fs::rename(path, path.with_extension("log.old"));
        }
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
    });
    let file_layer =
        file.map(|f| tracing_subscriber::fmt::layer().with_writer(std::sync::Mutex::new(f)).with_ansi(false));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_ansi(false))
        .with(file_layer)
        .try_init();
}

fn main() -> ExitCode {
    let (log, verbose, argv) = log_args(std::env::args().skip(1).collect());
    init_logging(log.as_deref(), verbose);
    tracing::info!(
        "bluecaster-driver {} ({}), elevated {}: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        win_sys::elevation::is_elevated(),
        // The command and the option names: no address or path in the log
        argv.iter()
            .enumerate()
            .filter(|(i, a)| *i == 0 || a.starts_with("--"))
            .map(|(_, a)| a.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );

    let (code, result, out) = match parse_args(&argv) {
        Ok(args) => {
            let (code, result) = run(&args.command);
            (code, result, args.out)
        }
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            (exit::USAGE, error_json("usage", &message), None)
        }
    };
    let mut result = result;
    result["exit_code"] = json!(code);
    let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into());
    // Link keys are secrets: the log gets their number only
    tracing::info!("exit {code}: {}", without_keys(&result));
    println!("{text}");
    if let Some(path) = out
        && let Err(e) = std::fs::write(&path, &text)
    {
        eprintln!("could not write {}: {e}", path.display());
        return ExitCode::from(exit::FAILED);
    }
    ExitCode::from(code)
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut positional = Vec::new();
    let mut out = None;
    let mut previous = None;
    let mut records = None;
    let mut keys = None;
    let mut all = false;
    let mut unrecorded = false;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = Some(PathBuf::from(it.next().ok_or("--out needs a file")?)),
            "--previous" => previous = Some(it.next().ok_or("--previous needs a value")?.clone()),
            "--records" => records = Some(PathBuf::from(it.next().ok_or("--records needs a folder")?)),
            "--keys" => keys = Some(PathBuf::from(it.next().ok_or("--keys needs a file")?)),
            "--all" => all = true,
            "--unrecorded" => unrecorded = true,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => positional.push(s.to_string()),
        }
    }
    let mut positional = positional.into_iter();
    let command = positional.next().ok_or("no command")?;
    let mut instance_id = || positional.next().ok_or(format!("{command} needs an instance ID"));
    let command = match command.as_str() {
        "list" => Command::List { all },
        "state" => Command::State { instance_id: instance_id()? },
        "assign" => Command::Assign { instance_id: instance_id()? },
        "restore" if previous.is_none() && keys.is_none() && records.is_some() => {
            let dir = records.take().unwrap_or_default();
            Command::RestoreRecords { dir, unrecorded: std::mem::take(&mut unrecorded) }
        }
        "unrecorded" => Command::Unrecorded { dir: records.take().ok_or("unrecorded needs --records <dir>")? },
        "restore" => Command::Restore { instance_id: instance_id()?, previous: previous.take(), keys: keys.take() },
        "windows-keys" => {
            let a = positional.next().ok_or("windows-keys needs the radio's address")?;
            Command::WindowsKeys { radio: parse_address(&a).ok_or(format!("bad address {a}"))? }
        }
        other => return Err(format!("unknown command {other}")),
    };
    if let Some(extra) = positional.next() {
        return Err(format!("unexpected argument {extra}"));
    }
    if previous.is_some() {
        return Err("--previous is for restore only".into());
    }
    if keys.is_some() {
        return Err("--keys is for restore of one adapter only".into());
    }
    if records.is_some() {
        return Err("--records is for restore without an instance ID or --previous only".into());
    }
    if unrecorded {
        return Err("--unrecorded is for restore --records only".into());
    }
    Ok(Args { command, out })
}

fn run(command: &Command) -> (u8, Value) {
    match command {
        Command::List { all } => list(*all),
        Command::State { instance_id } => match driver::driver_state(instance_id) {
            Ok(state) => (exit::OK, json!({ "ok": true, "command": "state", "state": StateJson::from(&state) })),
            Err(e) => driver_error("state", &e),
        },
        Command::Assign { instance_id } => assign(instance_id),
        Command::Restore { instance_id, previous, keys } => restore(instance_id, previous.as_deref(), keys.as_deref()),
        Command::RestoreRecords { dir, unrecorded } => restore_records(dir, *unrecorded),
        Command::Unrecorded { dir } => list_unrecorded(dir),
        Command::WindowsKeys { radio } => windows_keys(radio),
    }
}

/// "XX:XX:XX:XX:XX:XX" (or with dashes) in display order
fn parse_address(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    let mut a = [0u8; 6];
    if parts.len() != 6 {
        return None;
    }
    for (b, p) in a.iter_mut().zip(parts) {
        if p.len() != 2 {
            return None;
        }
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    Some(a)
}

fn windows_keys(radio: &[u8; 6]) -> (u8, Value) {
    if !win_sys::elevation::is_elevated() {
        return (exit::NOT_ELEVATED, error_json("not_elevated", "windows-keys needs administrator rights"));
    }
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    match win_sys::bt_keys::read_keys(radio) {
        Ok(keys) => {
            let list: Vec<Value> = keys
                .iter()
                .flat_map(|k| &k.link_keys)
                .map(|k| json!({ "address": win_sys::bt_paired::format_address(&k.address), "key": hex(&k.key) }))
                .collect();
            // As Windows keeps them: LTK and IRK least significant octet
            // first, ERand as a number (16 hex digits)
            let le: Vec<Value> = keys
                .iter()
                .flat_map(|k| &k.le_keys)
                .map(|k| {
                    json!({
                        "address": win_sys::bt_paired::format_address(&k.address),
                        "address_type": k.address_type,
                        "ltk": hex(&k.ltk),
                        "key_length": k.key_length,
                        "ediv": k.ediv,
                        "erand": k.erand.map(|r| format!("{r:016x}")),
                        "irk": k.irk.map(|i| hex(&i)),
                        "auth_req": k.auth_req,
                    })
                })
                .collect();
            // Names, types and sizes only: what Windows keeps, not the data
            let names = |values: &[win_sys::bt_keys::ValueName]| -> Vec<Value> {
                values
                    .iter()
                    .map(|v| json!({ "name": v.name, "kind": v.kind, "size": v.size, "count": v.count }))
                    .collect()
            };
            (
                exit::OK,
                json!({
                    "ok": true,
                    "command": "windows-keys",
                    "radio_found": keys.is_some(),
                    "keys": list,
                    "le_keys": le,
                    "radio_values": keys.as_ref().map(|k| names(&k.radio_values)).unwrap_or_default(),
                    "le_values": keys.as_ref().map(|k| names(&k.le_values)).unwrap_or_default(),
                }),
            )
        }
        Err(e) => (exit::FAILED, error_json("registry", &e.to_string())),
    }
}

fn list(all: bool) -> (u8, Value) {
    match usb_adapters::enumerate(true) {
        Ok(devices) => {
            let adapters: Vec<AdapterJson> =
                devices.iter().filter(|d| all || d.looks_like_bluetooth()).map(AdapterJson::from).collect();
            (
                exit::OK,
                json!({
                    "ok": true,
                    "command": "list",
                    "elevated": win_sys::elevation::is_elevated(),
                    "adapters": adapters,
                }),
            )
        }
        Err(e) => (exit::FAILED, error_json("enumeration", &e.to_string())),
    }
}

fn assign(instance_id: &str) -> (u8, Value) {
    match driver::assign_winusb(instance_id) {
        Ok(outcome) => {
            let on_winusb = outcome.state.kind == driver::DriverKind::WinUsb;
            let code = if on_winusb { exit::OK } else { exit::UNVERIFIED };
            (
                code,
                json!({
                    "ok": on_winusb,
                    "command": "assign",
                    "instance_id": instance_id,
                    "previous": RecordJson::from(&outcome.previous),
                    "already_winusb": outcome.already_winusb,
                    "class_changed": outcome.class_changed,
                    "reboot_required": outcome.reboot_required,
                    "state": StateJson::from(&outcome.state),
                    "disconnected": outcome.disconnected,
                }),
            )
        }
        Err(e) => driver_error("assign", &e),
    }
}

fn restore(instance_id: &str, previous: Option<&str>, keys: Option<&std::path::Path>) -> (u8, Value) {
    let previous = match previous.map(parse_previous).transpose() {
        Ok(p) => p,
        Err(message) => return (exit::USAGE, error_json("usage", &message)),
    };
    let keys = match keys.map(parse_keys).transpose() {
        Ok(k) => k,
        Err(message) => return (exit::USAGE, error_json("usage", &message)),
    };
    // Written while the adapter is still on WinUSB: Windows' Bluetooth
    // reads the keys when it starts the radio
    let written = keys.and_then(|(radio, keys)| {
        let on_winusb = driver::driver_state(instance_id).is_ok_and(|s| s.kind == driver::DriverKind::WinUsb);
        if !on_winusb || !win_sys::elevation::is_elevated() {
            return None;
        }
        Some(win_sys::bt_keys::write_keys(&radio, &keys.link, &keys.le).map_err(|e| {
            tracing::warn!("restore: writing the keys to Windows failed: {e}");
            e.to_string()
        }))
    });
    match driver::restore(instance_id, previous.as_ref()) {
        Ok(outcome) => {
            let verified = outcome.state.as_ref().is_some_and(|s| s.present && s.kind != driver::DriverKind::WinUsb)
                && outcome.matches_previous != Some(false);
            let code = if verified { exit::OK } else { exit::UNVERIFIED };
            let mut result = json!({
                "ok": verified,
                "command": "restore",
                "instance_id": instance_id,
                "already_restored": outcome.already_restored,
                "reboot_required": outcome.reboot_required,
                "matches_previous": outcome.matches_previous,
                "state": outcome.state.as_ref().map(StateJson::from),
            });
            add_written_keys(&mut result, written);
            (code, result)
        }
        Err(e) => {
            let (code, mut result) = driver_error("restore", &e);
            add_written_keys(&mut result, written);
            (code, result)
        }
    }
}

/// BlueCaster's keys from the file of `restore --keys`
#[derive(Debug, Default)]
struct KeysToWrite {
    link: Vec<win_sys::bt_keys::WindowsLinkKey>,
    /// Only when developer mode asked for them (issue #86)
    le: Vec<win_sys::bt_keys::WindowsLeKey>,
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// The file of `restore --keys`: the radio, BlueCaster's link keys and, if
/// asked for, its LE keys as Windows keeps them (`windows-keys`' format)
fn parse_keys(path: &std::path::Path) -> Result<([u8; 6], KeysToWrite), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse_keys_text(&text)
}

fn parse_keys_text(text: &str) -> Result<([u8; 6], KeysToWrite), String> {
    let value: Value =
        serde_json::from_str(text.trim_start_matches('\u{FEFF}')).map_err(|e| format!("--keys is not JSON: {e}"))?;
    let radio =
        value.get("radio").and_then(Value::as_str).and_then(parse_address).ok_or("--keys names no radio address")?;
    let list = |name: &str| value.get(name).and_then(Value::as_array).cloned().unwrap_or_default();
    let link = list("keys")
        .iter()
        .filter_map(|k| {
            Some(win_sys::bt_keys::WindowsLinkKey {
                address: parse_address(k.get("address")?.as_str()?)?,
                key: unhex(k.get("key")?.as_str()?)?,
            })
        })
        .collect();
    let le = list("le_keys")
        .iter()
        .filter_map(|k| {
            let number = |name: &str| k.get(name).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
            Some(win_sys::bt_keys::WindowsLeKey {
                address: parse_address(k.get("address")?.as_str()?)?,
                address_type: number("address_type"),
                ltk: unhex(k.get("ltk")?.as_str()?)?,
                key_length: number("key_length"),
                ediv: number("ediv"),
                erand: k.get("erand").and_then(Value::as_str).and_then(|t| u64::from_str_radix(t, 16).ok()),
                irk: k.get("irk").and_then(Value::as_str).and_then(unhex),
                auth_req: None,
            })
        })
        .collect();
    Ok((radio, KeysToWrite { link, le }))
}

/// What writing the keys did, in the restore's result: the devices written
/// and Windows' keys they replaced (for the core's backup)
fn add_written_keys(result: &mut Value, written: Option<Result<Option<win_sys::bt_keys::WrittenKeys>, String>>) {
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let address = |a: &[u8; 6]| win_sys::bt_paired::format_address(a);
    match written {
        None => {}
        Some(Err(e)) => result["keys_error"] = json!(e),
        Some(Ok(None)) => result["keys_written"] = json!([]),
        Some(Ok(Some(w))) => {
            result["keys_written"] = json!(w.replaced.iter().map(|k| address(&k.address)).collect::<Vec<_>>());
            result["keys_same"] = json!(w.same.iter().map(address).collect::<Vec<_>>());
            result["keys_verified"] = json!(w.verified);
            result["replaced_keys"] = json!(
                w.replaced
                    .iter()
                    .map(|k| json!({ "address": address(&k.address), "key": hex(&k.key) }))
                    .collect::<Vec<_>>()
            );
            result["le_written"] = json!(w.le_replaced.iter().map(|k| address(&k.address)).collect::<Vec<_>>());
            result["le_same"] = json!(w.le_same.iter().map(address).collect::<Vec<_>>());
            result["le_verified"] = json!(w.le_verified);
            // As Windows kept them, for the core's backup
            result["replaced_le_keys"] = json!(
                w.le_replaced
                    .iter()
                    .map(|k| {
                        json!({
                            "address": address(&k.address),
                            "address_type": k.address_type,
                            "ltk": hex(&k.ltk),
                            "key_length": k.key_length,
                            "ediv": k.ediv,
                            "erand": k.erand.map(|r| format!("{r:016x}")),
                            "irk": k.irk.map(|i| hex(&i)),
                            "auth_req": k.auth_req,
                        })
                    })
                    .collect::<Vec<_>>()
            );
        }
    }
}

/// The result as the log may show it: lists of keys and of devices by
/// their number only (no address or key in a log)
fn without_keys(result: &Value) -> Value {
    let mut shown = result.clone();
    for field in
        ["keys", "le_keys", "replaced_keys", "replaced_le_keys", "keys_written", "keys_same", "le_written", "le_same"]
    {
        if let Some(list) = result.get(field).and_then(Value::as_array) {
            shown[field] = json!(list.len());
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

/// What to do with one file of `restore --records`
#[derive(Debug, PartialEq)]
enum RecordPlan {
    Restore {
        instance_id: String,
    },
    /// Nothing to restore; `remove`: the record is of no use any more
    Skip {
        reason: &'static str,
        remove: bool,
    },
}

fn plan_record(text: &str) -> RecordPlan {
    let Ok(value) = serde_json::from_str::<Value>(text.trim_start_matches('\u{FEFF}')) else {
        return RecordPlan::Skip { reason: "not_a_record", remove: false };
    };
    let Some(instance_id) = value.get("instance_id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return RecordPlan::Skip { reason: "not_a_record", remove: false };
    };
    // The adapter was on WinUSB (Zadig's, or another tool's) before BlueCaster
    // was asked to assign it: nothing of BlueCaster's to undo
    if value.get("already_winusb").and_then(Value::as_bool) == Some(true) {
        return RecordPlan::Skip { reason: "winusb_before", remove: true };
    }
    RecordPlan::Restore { instance_id: instance_id.to_string() }
}

/// The more telling exit code of two adapters
fn worse(a: u8, b: u8) -> u8 {
    let rank = |c: u8| match c {
        exit::OK => 0,
        exit::NO_DEVICE => 1,
        exit::UNVERIFIED => 2,
        exit::FAILED => 3,
        _ => 4,
    };
    if rank(b) > rank(a) { b } else { a }
}

/// The records in `dir` and what to do with each (no folder: none)
fn read_records(dir: &std::path::Path) -> Result<Vec<(PathBuf, RecordPlan)>, (u8, Value)> {
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err((exit::FAILED, error_json("records", &format!("cannot read {}: {e}", dir.display())))),
    };
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|p| {
            let plan = match std::fs::read_to_string(&p) {
                Ok(text) => plan_record(&text),
                Err(_) => RecordPlan::Skip { reason: "unreadable", remove: false },
            };
            (p, plan)
        })
        .collect())
}

/// A Bluetooth adapter on WinUSB: by its Bluetooth class, or its interface
/// read through WinUSB. Not `looks_like_bluetooth`, which takes any WinUSB
/// device: a device put on WinUSB for something else is not ours to touch.
fn winusb_bluetooth(d: &UsbDevice) -> bool {
    d.driver_kind == driver::DriverKind::WinUsb
        && (d.has_bluetooth_compat_id()
            || d.usb_class == Some(usb_adapters::UsbClass::BLUETOOTH)
            || d.winusb_bluetooth().is_some())
}

/// Of the Bluetooth adapters on WinUSB, those no record gives back
fn without_record(on_winusb: Vec<String>, plans: &[(PathBuf, RecordPlan)]) -> Vec<String> {
    let recorded: Vec<&str> = plans
        .iter()
        .filter_map(|(_, plan)| match plan {
            RecordPlan::Restore { instance_id } => Some(instance_id.as_str()),
            RecordPlan::Skip { .. } => None,
        })
        .collect();
    on_winusb.into_iter().filter(|id| !recorded.iter().any(|r| r.eq_ignore_ascii_case(id))).collect()
}

fn unrecorded_adapters(plans: &[(PathBuf, RecordPlan)]) -> Result<Vec<String>, (u8, Value)> {
    let devices =
        usb_adapters::enumerate(true).map_err(|e| (exit::FAILED, error_json("enumeration", &e.to_string())))?;
    let ids = devices.iter().filter(|d| winusb_bluetooth(d)).map(|d| d.instance_id.clone()).collect();
    Ok(without_record(ids, plans))
}

/// `unrecorded --records <dir>`: the Bluetooth adapters on WinUSB without
/// a record; exit code 4 when there is none
fn list_unrecorded(dir: &std::path::Path) -> (u8, Value) {
    let ids = match read_records(dir).and_then(|plans| unrecorded_adapters(&plans)) {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let code = if ids.is_empty() { exit::NO_DEVICE } else { exit::OK };
    (code, json!({ "ok": true, "command": "unrecorded", "adapters": ids }))
}

/// `restore --records <dir>`: every recorded adapter; a record is deleted
/// once its adapter is back on its own driver. An unplugged adapter keeps
/// its record (exit code 4 for the whole run). `unrecorded`: also the
/// Bluetooth adapters on WinUSB without a record, Windows choosing their
/// driver.
fn restore_records(dir: &std::path::Path, unrecorded: bool) -> (u8, Value) {
    let plans = match read_records(dir) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let others = if unrecorded {
        match unrecorded_adapters(&plans) {
            Ok(ids) => ids,
            Err(e) => return e,
        }
    } else {
        Vec::new()
    };
    if (!others.is_empty() || plans.iter().any(|(_, plan)| matches!(plan, RecordPlan::Restore { .. })))
        && !win_sys::elevation::is_elevated()
    {
        return driver_error("restore", &DriverError::NotElevated);
    }
    let mut code = exit::OK;
    let mut adapters = Vec::new();
    for (path, plan) in plans {
        let (one, mut result, remove) = match plan {
            RecordPlan::Restore { instance_id } => {
                let (one, result) = restore(&instance_id, Some(&format!("@{}", path.display())), None);
                (one, result, one == exit::OK)
            }
            RecordPlan::Skip { reason, remove } => (exit::OK, json!({ "ok": true, "skipped": reason }), remove),
        };
        if remove && let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("cannot delete the record {}: {e}", path.display());
        }
        result["record"] = json!(path.display().to_string());
        result["exit_code"] = json!(one);
        code = worse(code, one);
        adapters.push(result);
    }
    for id in others {
        let (one, mut result) = restore(&id, None, None);
        result["unrecorded"] = json!(true);
        result["exit_code"] = json!(one);
        code = worse(code, one);
        adapters.push(result);
    }
    (
        code,
        json!({
            "ok": code == exit::OK,
            "command": "restore",
            "records": dir.display().to_string(),
            "adapters": adapters,
        }),
    )
}

/// The record from `--previous`: JSON text or @file, either the record
/// itself or a whole `assign` result holding it as "previous"
fn parse_previous(arg: &str) -> Result<DriverRecord, String> {
    let text = match arg.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?,
        None => arg.to_string(),
    };
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{FEFF}'))
        .map_err(|e| format!("--previous is not JSON: {e}"))?;
    let record = match value.get("previous") {
        Some(inner) if inner.is_object() => inner.clone(),
        _ => value,
    };
    let record: RecordJson =
        serde_json::from_value(record).map_err(|e| format!("--previous is not a driver record: {e}"))?;
    Ok(record.into())
}

fn driver_error(command: &str, e: &DriverError) -> (u8, Value) {
    let (code, kind) = match e {
        DriverError::NotElevated => (exit::NOT_ELEVATED, "not_elevated"),
        DriverError::DeviceNotFound(_) => (exit::NO_DEVICE, "device_not_found"),
        DriverError::NotUsb(_) => (exit::NO_DEVICE, "not_usb"),
        DriverError::WinUsbInfMissing(_) => (exit::FAILED, "winusb_inf_missing"),
        DriverError::WinUsbModelNotFound { .. } => (exit::FAILED, "winusb_model_not_found"),
        DriverError::Win32 { .. } => (exit::FAILED, "win32"),
    };
    let rollback = match e {
        DriverError::WinUsbModelNotFound { rollback, .. } | DriverError::Win32 { rollback, .. } => {
            Some(format!("{rollback:?}").to_ascii_lowercase())
        }
        _ => None,
    };
    tracing::error!("{command}: {e}");
    let mut v = error_json(kind, &e.to_string());
    v["command"] = json!(command);
    if let Some(r) = rollback {
        v["error"]["rollback"] = json!(r);
    }
    (code, v)
}

fn error_json(kind: &str, message: &str) -> Value {
    json!({ "ok": false, "error": { "kind": kind, "message": message } })
}

/// The driver record as JSON (passed back to `restore --previous`)
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct RecordJson {
    inf_name: String,
    version: String,
    service: String,
    provider: String,
    description: String,
    class_guid: String,
}

impl From<&DriverRecord> for RecordJson {
    fn from(r: &DriverRecord) -> Self {
        RecordJson {
            inf_name: r.inf_name.clone(),
            version: r.version.clone(),
            service: r.service.clone(),
            provider: r.provider.clone(),
            description: r.description.clone(),
            class_guid: r.class_guid.clone(),
        }
    }
}

impl From<RecordJson> for DriverRecord {
    fn from(r: RecordJson) -> Self {
        DriverRecord {
            inf_name: r.inf_name,
            version: r.version,
            service: r.service,
            provider: r.provider,
            description: r.description,
            class_guid: r.class_guid,
        }
    }
}

#[derive(Serialize)]
struct DriverJson {
    service: String,
    inf_name: String,
    inf_section: String,
    version: String,
    provider: String,
    description: String,
    class_name: String,
    class_guid: String,
}

impl From<&DriverInfo> for DriverJson {
    fn from(d: &DriverInfo) -> Self {
        DriverJson {
            service: d.service.clone(),
            inf_name: d.inf_name.clone(),
            inf_section: d.inf_section.clone(),
            version: d.version.clone(),
            provider: d.provider.clone(),
            description: d.description.clone(),
            class_name: d.class_name.clone(),
            class_guid: d.class_guid.clone(),
        }
    }
}

#[derive(Serialize)]
struct StateJson {
    instance_id: String,
    present: bool,
    kind: &'static str,
    driver: DriverJson,
    problem_code: Option<u32>,
    interface_guids: Vec<String>,
    bluecaster_interface: bool,
}

impl From<&DriverState> for StateJson {
    fn from(s: &DriverState) -> Self {
        StateJson {
            instance_id: s.instance_id.clone(),
            present: s.present,
            kind: s.kind.as_str(),
            driver: DriverJson::from(&s.driver),
            problem_code: s.problem_code,
            interface_guids: s.interface_guids.clone(),
            bluecaster_interface: s.has_bluecaster_interface(),
        }
    }
}

#[derive(Serialize)]
struct ProbeJson {
    ok: bool,
    /// Interface 0 class, "E0/01/01"
    class: Option<String>,
    hci_endpoints: Option<bool>,
    /// Why it could not be read ("open failed (error 5)": in use)
    error: Option<String>,
}

#[derive(Serialize)]
struct AdapterJson {
    instance_id: String,
    /// "0BDA:8771"
    id: String,
    vid: u16,
    pid: u16,
    name: String,
    bus_reported_name: Option<String>,
    manufacturer: String,
    /// From the compatible IDs, "E0/01/01"
    usb_class: Option<String>,
    kind: &'static str,
    driver: DriverJson,
    problem_code: Option<u32>,
    /// WinUSB devices: interface 0 read through WinUSB
    probe: Option<ProbeJson>,
    /// "standard" (E0/01/01) or "vendor" (FF/01/01 with HCI endpoints),
    /// from the WinUSB probe
    bluetooth_interface: Option<&'static str>,
    looks_like_bluetooth: bool,
}

impl From<&UsbDevice> for AdapterJson {
    fn from(d: &UsbDevice) -> Self {
        AdapterJson {
            instance_id: d.instance_id.clone(),
            id: format!("{:04X}:{:04X}", d.vid, d.pid),
            vid: d.vid,
            pid: d.pid,
            name: d.name.clone(),
            bus_reported_name: d.bus_reported_name.clone(),
            manufacturer: d.manufacturer.clone(),
            usb_class: d.usb_class.map(|c| c.to_string()),
            kind: d.driver_kind.as_str(),
            driver: DriverJson::from(&d.driver),
            problem_code: d.problem_code,
            probe: d.probe.as_ref().map(|p| match p {
                Ok(p) => ProbeJson {
                    ok: true,
                    class: Some(p.class.to_string()),
                    hci_endpoints: Some(p.hci_endpoints),
                    error: None,
                },
                Err(e) => ProbeJson { ok: false, class: None, hci_endpoints: None, error: Some(e.clone()) },
            }),
            bluetooth_interface: d.winusb_bluetooth().map(|vendor| if vendor { "vendor" } else { "standard" }),
            looks_like_bluetooth: d.looks_like_bluetooth(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_file_gives_link_and_le_keys() {
        let ltk = "00112233445566778899aabbccddeeff";
        let text = format!(
            r#"{{"radio":"AA:BB:CC:DD:EE:FF",
                "keys":[{{"address":"11:22:33:44:55:66","key":"{ltk}"}},{{"address":"bad","key":"{ltk}"}}],
                "le_keys":[{{"address":"C0:11:22:33:44:55","address_type":1,"ltk":"{ltk}","key_length":16,
                             "ediv":4660,"erand":"0102030405060708","irk":null}}]}}"#
        );
        let (radio, keys) = parse_keys_text(&text).unwrap();
        assert_eq!(radio, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(keys.link.len(), 1);
        assert_eq!(keys.le.len(), 1);
        let le = &keys.le[0];
        assert_eq!((le.address_type, le.key_length, le.ediv), (Some(1), Some(16), Some(4660)));
        assert_eq!(le.erand, Some(0x0102_0304_0506_0708));
        assert_eq!(le.irk, None);
        assert_eq!(le.ltk[15], 0xFF);
        // A file without LE keys writes none
        let (_, keys) = parse_keys_text(r#"{"radio":"AA:BB:CC:DD:EE:FF","keys":[]}"#).unwrap();
        assert!(keys.le.is_empty());
        assert!(parse_keys_text(r#"{"keys":[]}"#).is_err());
    }

    #[test]
    fn no_address_or_key_reaches_the_log() {
        let result = json!({
            "ok": true,
            "keys_written": ["11:22:33:44:55:66"],
            "keys_same": [],
            "keys_verified": 1,
            "replaced_keys": [{ "address": "11:22:33:44:55:66", "key": "00" }],
            "le_written": ["C0:11:22:33:44:55"],
            "le_same": [],
            "replaced_le_keys": [{ "address": "C0:11:22:33:44:55", "ltk": "00" }],
        });
        let shown = without_keys(&result).to_string();
        assert!(!shown.contains("11:22") && !shown.contains("\"00\""), "{shown}");
        assert_eq!(without_keys(&result)["keys_written"], 1);
        assert_eq!(without_keys(&result)["le_written"], 1);
    }

    fn args(s: &[&str]) -> Result<Args, String> {
        parse_args(&s.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn unrecorded_are_those_no_record_restores() {
        let plans = vec![
            (PathBuf::from("a.json"), RecordPlan::Restore { instance_id: r"USB\VID_0BDA&PID_8771\A".into() }),
            (PathBuf::from("b.json"), RecordPlan::Skip { reason: "winusb_before", remove: true }),
        ];
        let on_winusb = vec![r"usb\vid_0bda&pid_8771\a".to_string(), r"USB\VID_0A12&PID_0001\B".to_string()];
        assert_eq!(without_record(on_winusb, &plans), [r"USB\VID_0A12&PID_0001\B"]);
        assert!(without_record(Vec::new(), &plans).is_empty());
    }

    #[test]
    fn log_options_come_out_first() {
        let argv: Vec<String> = ["assign", "--log", r"C:\l\bluecaster-driver.log", "X", "--verbose", "--out", "o.json"]
            .map(String::from)
            .into();
        let (log, verbose, rest) = log_args(argv);
        assert_eq!(log, Some(PathBuf::from(r"C:\l\bluecaster-driver.log")));
        assert!(verbose);
        assert_eq!(rest, ["assign", "X", "--out", "o.json"]);
        let (log, verbose, _) = log_args(vec!["list".into()]);
        assert_eq!((log, verbose), (None, false));
    }

    #[test]
    fn command_line() {
        assert_eq!(args(&["list"]).unwrap(), Args { command: Command::List { all: false }, out: None });
        assert_eq!(
            args(&["list", "--all", "--out", r"C:\t\r.json"]).unwrap(),
            Args { command: Command::List { all: true }, out: Some(PathBuf::from(r"C:\t\r.json")) }
        );
        assert_eq!(
            args(&["assign", r"USB\VID_0BDA&PID_8771\1"]).unwrap().command,
            Command::Assign { instance_id: r"USB\VID_0BDA&PID_8771\1".into() }
        );
        assert_eq!(
            args(&["restore", "X", "--previous", "{}"]).unwrap().command,
            Command::Restore { instance_id: "X".into(), previous: Some("{}".into()), keys: None }
        );
        assert_eq!(
            args(&["restore", "X"]).unwrap().command,
            Command::Restore { instance_id: "X".into(), previous: None, keys: None }
        );
        assert_eq!(
            args(&["restore", "X", "--keys", r"C:\u\keys.json"]).unwrap().command,
            Command::Restore { instance_id: "X".into(), previous: None, keys: Some(PathBuf::from(r"C:\u\keys.json")) }
        );
        assert!(args(&["restore", "--records", "D", "--keys", "k.json"]).is_err());
        assert!(args(&["assign", "X", "--keys", "k.json"]).is_err());
        assert!(args(&[]).is_err());
        assert!(args(&["assign"]).is_err());
        assert!(args(&["assign", "X", "Y"]).is_err());
        assert!(args(&["assign", "X", "--previous", "{}"]).is_err());
        assert!(args(&["list", "--out"]).is_err());
        assert!(args(&["list", "--force"]).is_err());
        assert!(args(&["format"]).is_err());
        assert_eq!(
            args(&["restore", "--records", r"C:\u\BlueCaster\driver", "--out", "o.json"]).unwrap(),
            Args {
                command: Command::RestoreRecords { dir: PathBuf::from(r"C:\u\BlueCaster\driver"), unrecorded: false },
                out: Some(PathBuf::from("o.json"))
            }
        );
        assert!(args(&["restore", "--records"]).is_err());
        assert!(args(&["restore", "X", "--records", "D"]).is_err());
        assert!(args(&["restore", "--records", "D", "--previous", "{}"]).is_err());
        assert!(args(&["list", "--records", "D"]).is_err());
        assert_eq!(
            args(&["restore", "--records", "D", "--unrecorded"]).unwrap().command,
            Command::RestoreRecords { dir: PathBuf::from("D"), unrecorded: true }
        );
        assert_eq!(
            args(&["unrecorded", "--records", "D"]).unwrap().command,
            Command::Unrecorded { dir: PathBuf::from("D") }
        );
        assert!(args(&["unrecorded"]).is_err());
        assert!(args(&["restore", "X", "--unrecorded"]).is_err());
        assert!(args(&["list", "--unrecorded"]).is_err());
        assert_eq!(
            args(&["windows-keys", "00:1A:7D:da:71:13"]).unwrap().command,
            Command::WindowsKeys { radio: [0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13] }
        );
        assert!(args(&["windows-keys"]).is_err());
        assert!(args(&["windows-keys", "00:1A:7D:DA:71"]).is_err());
        assert!(args(&["windows-keys", "00:1A:7D:DA:71:1G"]).is_err());
    }

    #[test]
    fn record_plans() {
        let assigned = json!({
            "ok": true, "command": "assign", "instance_id": r"USB\VID_2357&PID_0604\E848B8C82000",
            "already_winusb": false, "previous": { "inf_name": "oem91.inf", "service": "BTHUSB" },
        });
        assert_eq!(
            plan_record(&format!("\u{FEFF}{assigned}")),
            RecordPlan::Restore { instance_id: r"USB\VID_2357&PID_0604\E848B8C82000".into() }
        );
        let mut before = assigned.clone();
        before["already_winusb"] = json!(true);
        assert_eq!(plan_record(&before.to_string()), RecordPlan::Skip { reason: "winusb_before", remove: true });
        assert_eq!(plan_record("null"), RecordPlan::Skip { reason: "not_a_record", remove: false });
        assert_eq!(plan_record("{}"), RecordPlan::Skip { reason: "not_a_record", remove: false });
        assert_eq!(plan_record("garbage"), RecordPlan::Skip { reason: "not_a_record", remove: false });
    }

    #[test]
    fn worst_exit_code() {
        assert_eq!(worse(exit::OK, exit::OK), exit::OK);
        assert_eq!(worse(exit::OK, exit::NO_DEVICE), exit::NO_DEVICE);
        assert_eq!(worse(exit::UNVERIFIED, exit::NO_DEVICE), exit::UNVERIFIED);
        assert_eq!(worse(exit::UNVERIFIED, exit::FAILED), exit::FAILED);
        assert_eq!(worse(exit::NOT_ELEVATED, exit::FAILED), exit::NOT_ELEVATED);
    }

    #[test]
    fn records_folder_without_work() {
        // A missing folder, and records with nothing to restore: no elevation
        // needed, the useless record goes, the unreadable one stays
        let (code, v) = restore_records(std::path::Path::new(r"Z:\no\such\folder"), false);
        assert_eq!(code, exit::OK);
        assert_eq!(v["adapters"].as_array().unwrap().len(), 0);
        let dir = std::env::temp_dir().join(format!("bluecaster_driver_records_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), r#"{"instance_id":"USB\\X","already_winusb":true}"#).unwrap();
        std::fs::write(dir.join("b.json"), "not json").unwrap();
        std::fs::write(dir.join("c.txt"), "ignored").unwrap();
        let (code, v) = restore_records(&dir, false);
        assert_eq!(code, exit::OK);
        assert_eq!(v["adapters"].as_array().unwrap().len(), 2);
        assert!(!dir.join("a.json").exists());
        assert!(dir.join("b.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn previous_record_roundtrip() {
        let record = DriverRecord {
            inf_name: "oem28.inf".into(),
            version: "24.40.11.1".into(),
            service: "BTHUSB".into(),
            provider: "Intel Corporation".into(),
            description: "インテル(R) ワイヤレス Bluetooth(R)".into(),
            class_guid: "{E0CBF06C-CD8B-4647-BB8A-263B43F0F974}".into(),
        };
        let text = serde_json::to_string(&RecordJson::from(&record)).unwrap();
        assert_eq!(parse_previous(&text).unwrap(), record);
        // The whole assign result is accepted too
        let assign_result = json!({ "ok": true, "command": "assign", "previous": RecordJson::from(&record) });
        assert_eq!(parse_previous(&assign_result.to_string()).unwrap(), record);
        // Missing fields are empty, unknown ones ignored
        let partial = parse_previous(r#"{"inf_name":"bth.inf","service":"BTHUSB","extra":1}"#).unwrap();
        assert_eq!(partial.inf_name, "bth.inf");
        assert_eq!(partial.version, "");
        assert!(parse_previous("not json").is_err());
        assert!(parse_previous(r#"{"inf_name": 5}"#).is_err());
    }

    #[test]
    fn previous_from_file() {
        let path = std::env::temp_dir().join(format!("bluecaster_driver_prev_{}.json", std::process::id()));
        // With a BOM, as PowerShell's Out-File writes it
        std::fs::write(&path, "\u{FEFF}{\"inf_name\":\"bth.inf\",\"version\":\"10.0.1\",\"service\":\"BTHUSB\"}")
            .unwrap();
        let record = parse_previous(&format!("@{}", path.display())).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(record.service, "BTHUSB");
        assert!(parse_previous("@Z:\\missing\\file.json").is_err());
    }

    #[test]
    fn errors_map_to_exit_codes() {
        assert_eq!(driver_error("assign", &DriverError::NotElevated).0, exit::NOT_ELEVATED);
        assert_eq!(driver_error("assign", &DriverError::DeviceNotFound("X".into())).0, exit::NO_DEVICE);
        let (code, v) = driver_error(
            "assign",
            &DriverError::WinUsbModelNotFound { class: "{X}".into(), rollback: driver::Rollback::Restored },
        );
        assert_eq!(code, exit::FAILED);
        assert_eq!(v["error"]["kind"], "winusb_model_not_found");
        assert_eq!(v["error"]["rollback"], "restored");
        assert_eq!(v["ok"], false);
    }
}

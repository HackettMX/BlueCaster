//! LE keys taken from Windows (issue #86) that wait for LE: the import runs
//! while LE Audio may be off, and only SM keeps LE bonds. The next stack
//! with LE adds them ([`apply`]) and empties the file. One line per bond,
//! hex:
//!
//! ```text
//! <classic address> <identity type> <identity address> <IRK> <LTK> <key size> <EDIV> <Rand> <authenticated> <secure connection>
//! ```
//!
//! Addresses as `XX:XX:..`, keys and Rand most significant octet first (as
//! BTstack keeps them). Lives in `pairing\`, which no bundle takes,
//! encrypted for the Windows user ([`win_sys::dpapi`]).

use std::path::Path;

use btstack_sys::{ImportedLeBond, LeBondAdded, Stack};

use crate::events::{addr_str, parse_addr};

/// A bond and the classic device it belongs to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingLeBond {
    pub classic: [u8; 6],
    pub bond: ImportedLeBond,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

fn format_line(p: &PendingLeBond) -> String {
    let b = &p.bond;
    format!(
        "{} {} {} {} {} {} {} {} {} {}\n",
        addr_str(&p.classic),
        b.identity_addr_type,
        addr_str(&b.identity_addr),
        hex(&b.irk),
        hex(&b.ltk),
        b.key_size,
        b.ediv,
        hex(&b.rand),
        u8::from(b.authenticated),
        u8::from(b.secure_connection)
    )
}

fn parse_line(line: &str) -> Option<PendingLeBond> {
    let f: Vec<&str> = line.split_whitespace().collect();
    let [classic, ty, identity, irk, ltk, key_size, ediv, rand, auth, sc] = f[..] else { return None };
    Some(PendingLeBond {
        classic: parse_addr(classic)?,
        bond: ImportedLeBond {
            identity_addr_type: ty.parse().ok().filter(|t: &u8| *t <= 1)?,
            identity_addr: parse_addr(identity)?,
            irk: unhex(irk)?,
            ltk: unhex(ltk)?,
            key_size: key_size.parse().ok().filter(|k: &u8| (7..=16).contains(k))?,
            ediv: ediv.parse().ok()?,
            rand: unhex(rand)?,
            authenticated: auth == "1",
            secure_connection: sc == "1",
        },
    })
}

pub fn load(file: &Path) -> Vec<PendingLeBond> {
    win_sys::dpapi::read_secret(file)
        .map(|b| String::from_utf8_lossy(&b).lines().filter_map(parse_line).collect())
        .unwrap_or_default()
}

/// Keeps `bond` for the next stack with LE (one per classic device: a
/// newer import replaces it)
pub fn add(file: &Path, bond: PendingLeBond) {
    let mut all = load(file);
    all.retain(|p| p.classic != bond.classic);
    all.push(bond);
    let text: String = all.iter().map(format_line).collect();
    if let Err(e) = win_sys::dpapi::write_secret(file, text.as_bytes()) {
        tracing::error!("Windows' LE keys: cannot write the file: {e}");
    }
}

/// After le_init: adds the waiting bonds and removes the file (a bond SM
/// could not take is not tried again: the device can still give its keys
/// itself)
pub(crate) fn apply(stack: &Stack, file: &Path) {
    let pending = load(file);
    if pending.is_empty() {
        return;
    }
    for p in &pending {
        let added = stack.add_le_bond_of_classic(&p.classic, &p.bond);
        tracing::info!(target: "le", "Windows' LE keys of {}: {added:?}", addr_str(&p.classic));
        if added == LeBondAdded::NoLe {
            return;
        }
    }
    if let Err(e) = std::fs::remove_file(file) {
        tracing::warn!("Windows' LE keys: cannot remove the file: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PendingLeBond {
        let mut ltk = [0u8; 16];
        ltk[0] = 0xAB;
        PendingLeBond {
            classic: [0x00, 0xA2, 0xD0, 0x00, 0x00, 0x02],
            bond: ImportedLeBond {
                identity_addr_type: 1,
                identity_addr: [0xC0, 0x11, 0x22, 0x33, 0x44, 0x55],
                irk: [7; 16],
                ltk,
                key_size: 16,
                ediv: 0x1234,
                rand: [1, 2, 3, 4, 5, 6, 7, 8],
                authenticated: false,
                secure_connection: true,
            },
        }
    }

    #[test]
    fn lines_round_trip() {
        let p = sample();
        assert_eq!(parse_line(&format_line(&p)), Some(p));
        assert_eq!(parse_line("00:A2:D0:00:00:02 2 C0:11:22:33:44:55"), None);
        let bad_type = format_line(&p).replacen(" 1 ", " 2 ", 1);
        assert_eq!(parse_line(&bad_type), None);
    }

    #[test]
    fn one_bond_per_classic_device() {
        let file = std::env::temp_dir().join(format!("bluecaster-le-import-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let mut p = sample();
        add(&file, p);
        p.bond.ediv = 1;
        add(&file, p);
        let all = load(&file);
        let encrypted = win_sys::dpapi::is_encrypted(&std::fs::read(&file).unwrap());
        let _ = std::fs::remove_file(&file);
        assert!(encrypted);
        assert_eq!(all, vec![p]);
    }
}

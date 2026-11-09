//! `le_audio_bonds.tlv`: the LE TLV BTstack keeps (the bonds of
//! le_device_db_tlv, the GATT server's client configurations), held by
//! [`TlvFile`] while a stack runs and read without one ([`read`]: the
//! driver restore hands the bonds to Windows, developer mode, issue #86).
//!
//! The content, as btstack_tlv_windows wrote it in v1 and earlier v3
//! builds: the 8-byte header "BTstack\0", then entries of a big-endian tag,
//! a big-endian length and the value; a later entry of a tag replaces an
//! earlier one, length 0 deletes it. A bond is the tag "BTD" + index with
//! le_device_db_entry_t as its value (60 bytes; 100 with signed writes,
//! whose extra fields are not read). On disk it is encrypted for the
//! Windows user ([`win_sys::dpapi`]); a plain file of those builds is read
//! and rewritten encrypted.

use std::path::{Path, PathBuf};

/// One bond as BTstack keeps it: addresses in display order, keys and Rand
/// most significant octet first
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredLeBond {
    /// 0 public, 1 random
    pub addr_type: u8,
    pub addr: [u8; 6],
    /// All zero: none
    pub irk: [u8; 16],
    pub ltk: [u8; 16],
    pub ediv: u16,
    pub rand: [u8; 8],
    pub key_size: u8,
    pub authenticated: bool,
    pub secure_connection: bool,
}

const MAGIC: &[u8] = b"BTstack";
const HEADER_LEN: usize = 8;
/// btstack_tlv_windows refused longer values
const MAX_VALUE: usize = 2048;
/// sizeof(le_device_db_entry_t) without ENABLE_LE_SIGNED_WRITE
const ENTRY_LEN: usize = 60;
const ENTRY_LEN_SIGNED: usize = 100;

fn is_bond_tag(tag: u32) -> bool {
    tag >> 8 == u32::from_be_bytes([0, b'B', b'T', b'D'])
}

/// The bonds with an LTK in the file `path`; Err when it cannot be read
/// (NotFound also when it cannot be decrypted)
pub fn read(path: &Path) -> std::io::Result<Vec<StoredLeBond>> {
    Ok(parse(&win_sys::dpapi::read_secret(path)?))
}

/// The entries of the file's bytes in the order they were stored last (an
/// unreadable tail ends them, as it ended btstack_tlv_windows' reading)
fn entries(data: &[u8]) -> Vec<(u32, Vec<u8>)> {
    if data.len() < HEADER_LEN || &data[..MAGIC.len()] != MAGIC {
        return Vec::new();
    }
    let mut entries: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut at = HEADER_LEN;
    while at + 8 <= data.len() {
        let tag = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
        let len = u32::from_be_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]) as usize;
        at += 8;
        if len > MAX_VALUE || at + len > data.len() {
            break;
        }
        entries.retain(|(t, _)| *t != tag);
        if len > 0 {
            entries.push((tag, data[at..at + len].to_vec()));
        }
        at += len;
    }
    entries
}

/// The file's bytes of `entries` (each once, nothing deleted)
fn format(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"BTstack\0".to_vec();
    for (tag, value) in entries {
        out.extend(tag.to_be_bytes());
        out.extend((value.len() as u32).to_be_bytes());
        out.extend(value);
    }
    out
}

/// The bonds of the file's bytes, in tag order
pub fn parse(data: &[u8]) -> Vec<StoredLeBond> {
    let mut entries = entries(data);
    entries.sort_by_key(|(t, _)| *t);
    entries.into_iter().filter(|(t, _)| is_bond_tag(*t)).filter_map(|(_, v)| entry(&v)).collect()
}

/// The LE TLV of a running stack: in memory, the whole file rewritten
/// (encrypted, temp file + rename) on every change
pub struct TlvFile {
    path: PathBuf,
    entries: Vec<(u32, Vec<u8>)>,
}

impl TlvFile {
    /// Loads `path` (missing, unreadable or not decryptable: empty)
    pub fn open(path: &Path) -> TlvFile {
        let entries = match win_sys::dpapi::read_secret(path) {
            Ok(data) => entries(&data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!("LE Audio: the bonds file cannot be read ({}), starting without bonds", e.kind());
                Vec::new()
            }
        };
        let bonds = entries.iter().filter(|(t, _)| is_bond_tag(*t)).count();
        tracing::info!("LE Audio: {bonds} bond(s) loaded");
        TlvFile { path: path.to_path_buf(), entries }
    }

    fn save(&self) {
        if let Err(e) = win_sys::dpapi::write_secret(&self.path, &format(&self.entries)) {
            tracing::error!("LE Audio: cannot write the bonds file: {e}");
        }
    }
}

impl btstack_sys::TlvStore for TlvFile {
    fn get(&mut self, tag: u32) -> Option<Vec<u8>> {
        self.entries.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.clone())
    }

    fn store(&mut self, tag: u32, value: &[u8]) {
        if value.len() > MAX_VALUE {
            tracing::warn!("LE Audio: TLV value of {} bytes not stored", value.len());
            return;
        }
        if self.entries.iter().any(|(t, v)| *t == tag && v == value) {
            return;
        }
        self.entries.retain(|(t, _)| *t != tag);
        if !value.is_empty() {
            self.entries.push((tag, value.to_vec()));
        }
        self.save();
    }

    fn delete(&mut self, tag: u32) {
        let before = self.entries.len();
        self.entries.retain(|(t, _)| *t != tag);
        if self.entries.len() != before {
            self.save();
        }
    }
}

/// le_device_db_entry_t (little-endian, MSVC layout): seq_nr u32, addr_type
/// int, addr[6], irk[16], ltk[16], ediv u16, rand[8], key_size,
/// authenticated, authorized, secure_connection
fn entry(v: &[u8]) -> Option<StoredLeBond> {
    if v.len() != ENTRY_LEN && v.len() != ENTRY_LEN_SIGNED {
        return None;
    }
    let addr_type = i32::from_le_bytes(v[4..8].try_into().ok()?);
    let ltk: [u8; 16] = v[30..46].try_into().ok()?;
    if ltk == [0; 16] || !(0..=1).contains(&addr_type) {
        return None;
    }
    Some(StoredLeBond {
        addr_type: addr_type as u8,
        addr: v[8..14].try_into().ok()?,
        irk: v[14..30].try_into().ok()?,
        ltk,
        ediv: u16::from_le_bytes([v[46], v[47]]),
        rand: v[48..56].try_into().ok()?,
        key_size: v[56],
        authenticated: v[57] != 0,
        secure_connection: v[59] != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bond_value(addr: [u8; 6], ltk: u8, signed: bool) -> Vec<u8> {
        let mut v = vec![0u8; if signed { ENTRY_LEN_SIGNED } else { ENTRY_LEN }];
        v[0..4].copy_from_slice(&7u32.to_le_bytes());
        v[4..8].copy_from_slice(&0i32.to_le_bytes());
        v[8..14].copy_from_slice(&addr);
        v[14..30].copy_from_slice(&[0x11; 16]);
        v[30..46].copy_from_slice(&[ltk; 16]);
        v[46..48].copy_from_slice(&0x1234u16.to_le_bytes());
        v[48..56].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        v[56] = 16;
        v[57] = 1;
        v[59] = 1;
        v
    }

    fn tlv(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = b"BTstack\0".to_vec();
        for (tag, value) in entries {
            out.extend(tag.to_be_bytes());
            out.extend((value.len() as u32).to_be_bytes());
            out.extend(value);
        }
        out
    }

    const BTD0: u32 = u32::from_be_bytes([b'B', b'T', b'D', 0]);
    const BTD1: u32 = u32::from_be_bytes([b'B', b'T', b'D', 1]);

    #[test]
    fn bonds_come_out_as_btstack_keeps_them() {
        let a = [0xC0, 1, 2, 3, 4, 5];
        let b = [0xD0, 1, 2, 3, 4, 5];
        let data = tlv(&[
            (BTD0, bond_value(a, 0x22, false)),
            // Another store's tag: not a bond
            (u32::from_be_bytes(*b"GATT"), vec![1, 2, 3]),
            (BTD1, bond_value(b, 0x33, true)),
            // Stored again: the later one counts
            (BTD0, bond_value(a, 0x44, false)),
        ]);
        let bonds = parse(&data);
        assert_eq!(bonds.len(), 2);
        assert_eq!(bonds[0].addr, a);
        assert_eq!(bonds[0].ltk, [0x44; 16]);
        assert_eq!(bonds[0].irk, [0x11; 16]);
        assert_eq!((bonds[0].ediv, bonds[0].rand, bonds[0].key_size), (0x1234, [1, 2, 3, 4, 5, 6, 7, 8], 16));
        assert!(bonds[0].authenticated && bonds[0].secure_connection);
        assert_eq!(bonds[1].addr, b);
    }

    #[test]
    fn deleted_empty_and_broken_entries_are_left_out() {
        let a = [0xC0, 1, 2, 3, 4, 5];
        // Deleted (length 0)
        assert!(parse(&tlv(&[(BTD0, bond_value(a, 0x22, false)), (BTD0, vec![])])).is_empty());
        // No LTK
        assert!(parse(&tlv(&[(BTD0, bond_value(a, 0, false))])).is_empty());
        // Another size
        assert!(parse(&tlv(&[(BTD0, vec![0; 59])])).is_empty());
        // No header, or a cut file
        assert!(parse(b"nothing").is_empty());
        let mut cut = tlv(&[(BTD0, bond_value(a, 0x22, false))]);
        cut.truncate(cut.len() - 1);
        assert!(parse(&cut).is_empty());
    }

    #[test]
    fn plain_bond_files_are_kept_encrypted() {
        use btstack_sys::TlvStore;
        let dir = std::env::temp_dir().join(format!("bluecaster-tlv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("le_audio_bonds.tlv");
        let a = [0xC0, 1, 2, 3, 4, 5];
        let b = [0xD0, 1, 2, 3, 4, 5];
        let gatt = u32::from_be_bytes(*b"GATT");
        // As btstack_tlv_windows left it: appended, a tag stored twice
        let plain = tlv(&[(BTD0, bond_value(a, 0x22, false)), (gatt, vec![1]), (BTD0, bond_value(a, 0x44, false))]);
        std::fs::write(&file, &plain).unwrap();

        let mut store = TlvFile::open(&file);
        let on_disk = std::fs::read(&file).unwrap();
        assert!(win_sys::dpapi::is_encrypted(&on_disk));
        assert!(!on_disk.windows(16).any(|w| w == [0x44; 16]));
        assert_eq!(store.get(BTD0).unwrap(), bond_value(a, 0x44, false).as_slice());
        assert_eq!(store.get(gatt).unwrap(), [1]);

        store.store(BTD1, &bond_value(b, 0x33, false));
        store.delete(gatt);
        store.store(BTD0, &[]);
        assert_eq!(store.get(BTD0), None);
        let bonds = read(&file).unwrap();
        assert_eq!(bonds.len(), 1);
        assert_eq!(bonds[0].ltk, [0x33; 16]);
        assert_eq!(TlvFile::open(&file).entries, vec![(BTD1, bond_value(b, 0x33, false))]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Files only this Windows user can read: the pairing keys at rest,
//! encrypted with DPAPI (CryptProtectData, current-user scope).
//!
//! An encrypted file is [`MAGIC`] followed by the DPAPI blob. A file without
//! it is an older plain-text one: [`read_secret`] returns its bytes and
//! rewrites it encrypted. A file that cannot be decrypted (a profile copied
//! to another user or PC, a reset password) is set aside as
//! `<name>.undecryptable` and read as missing, so the keys are paired anew
//! instead of the file being overwritten.
//!
//! Logs name the file only (no folder: it holds the user name), never its
//! content.

use std::io;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;

/// Start of an encrypted file (none of the plain-text formats starts so)
pub const MAGIC: &[u8; 8] = b"BCDPAPI1";

/// Bound into every blob: another program's DPAPI blobs of this user do not
/// decrypt as ours, nor ours as theirs
const ENTROPY: &[u8] = b"BlueCaster pairing keys";

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr().cast_mut() }
}

/// Copies DPAPI's output and frees it
fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI returned cbData bytes at pbData, allocated with
    // LocalAlloc; they are copied before LocalFree.
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        LocalFree(Some(HLOCAL(out.pbData.cast())));
        v
    }
}

/// `data` encrypted for the current Windows user
pub fn protect(data: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input and entropy point to live slices for the call; out
    // receives a LocalAlloc'd buffer that take() frees.
    unsafe {
        CryptProtectData(&input, PCWSTR::null(), Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
    }
    .map_err(io::Error::other)?;
    Ok(take(out))
}

/// The data of a [`protect`]ed blob; an error when another user (or PC)
/// encrypted it, or it is damaged
pub fn unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in protect()
    unsafe { CryptUnprotectData(&input, None, Some(&entropy), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) }
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(take(out))
}

/// Whether the file's bytes are an encrypted file
pub fn is_encrypted(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

fn name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// The content of the secret file `path`. A plain-text file is returned as
/// it is and rewritten encrypted. Errors: NotFound when there is no file
/// (also after one that cannot be decrypted was set aside), other I/O errors
/// as they come.
pub fn read_secret(path: &Path) -> io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let Some(blob) = bytes.strip_prefix(MAGIC.as_slice()) else {
        // Written by an older version: encrypt it now
        if bytes.is_empty() {
            return Ok(bytes);
        }
        match write_secret(path, &bytes) {
            Ok(()) => tracing::info!("{}: encrypted (was plain text)", name(path)),
            Err(e) => tracing::warn!("{}: still plain text, cannot encrypt it ({})", name(path), e.kind()),
        }
        return Ok(bytes);
    };
    match unprotect(blob) {
        Ok(data) => Ok(data),
        Err(e) => {
            let aside = with_suffix(path, ".undecryptable");
            let moved = std::fs::rename(path, &aside).is_ok();
            tracing::warn!(
                "{}: cannot be decrypted by this Windows user ({e}); its keys are not used{}",
                name(path),
                if moved { ", file kept as .undecryptable" } else { "" }
            );
            Err(io::Error::new(io::ErrorKind::NotFound, "secret file not decryptable"))
        }
    }
}

/// Writes `data` encrypted to `path` (folder made, temp file + rename)
pub fn write_secret(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut out = MAGIC.to_vec();
    out.extend(protect(data)?);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = with_suffix(path, ".tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bluecaster-dpapi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip() {
        let secret = b"00-A2-D0-00-00-02 00112233445566778899aabbccddeeff 4\n";
        let blob = protect(secret).unwrap();
        assert!(!blob.windows(secret.len()).any(|w| w == secret));
        assert_eq!(unprotect(&blob).unwrap(), secret);
        assert_eq!(unprotect(&protect(b"").unwrap()).unwrap(), b"");
    }

    #[test]
    fn plain_files_are_migrated() {
        let d = dir("migrate");
        let file = d.join("keys.txt");
        std::fs::write(&file, "plain key\n").unwrap();
        assert_eq!(read_secret(&file).unwrap(), b"plain key\n");
        let on_disk = std::fs::read(&file).unwrap();
        assert!(is_encrypted(&on_disk));
        assert!(!String::from_utf8_lossy(&on_disk).contains("plain key"));
        assert_eq!(read_secret(&file).unwrap(), b"plain key\n");
        write_secret(&file, b"new").unwrap();
        assert_eq!(read_secret(&file).unwrap(), b"new");
        assert!(!d.join("keys.txt.tmp").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn undecryptable_files_are_set_aside() {
        let d = dir("foreign");
        let file = d.join("keys.txt");
        let mut foreign = MAGIC.to_vec();
        foreign.extend([1, 2, 3, 4]);
        std::fs::write(&file, &foreign).unwrap();
        assert_eq!(read_secret(&file).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!file.exists());
        assert_eq!(std::fs::read(d.join("keys.txt.undecryptable")).unwrap(), foreign);
        assert_eq!(read_secret(&file).unwrap_err().kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&d);
    }
}

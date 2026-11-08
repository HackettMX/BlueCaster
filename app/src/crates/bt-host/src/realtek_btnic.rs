//! Realtek chips whose firmware only Realtek's Windows driver has: the
//! RTL8852BD (0BDA:B853, issue #374), experimental and untested on hardware
//!
//! linux-firmware has no file for this chip: its ROM (ROM version 3) is not
//! in rtl8852bu_fw.bin, so BTstack's Realtek download finds no patch for it.
//! Realtek's Windows driver (RtkBtFilter2, 18.4045.1.3025) carries the
//! firmware as `rtl8852bd_mp_chip_new.dat`, a container that starts with
//! "BTNIC003", and downloads it in a sequence of its own. Everything below
//! comes from that driver and its file; no other driver's source was used.
//!
//! The container:
//! - "BTNIC003", an 8-byte version, a u32 (0), the section count (u32 at
//!   0x14); the sections follow from 0x18
//! - a section: opcode (u32: 1 = patch, 2 = signed patch), length (u32), a
//!   u32 (0), then `length` bytes: a 0x408-byte description and the payload
//! - the description: project ID (u16 at 0, 0x14 for the RTL8852B family),
//!   the ROM version + 1 (byte 2), the key ID (byte 3), 0 (byte 4); two
//!   register writes (address and value, u32 each, at 0x3DF / 0x3E7 and
//!   0x3EF / 0x3F7; address 0 or 0xFFFFFFFF: none); the order of the
//!   section (byte 0x3FF); the payload length (u32 at 0x400)
//!
//! The sequence, after Read Local Version found the ROM code (LMP
//! subversion 0x8852):
//! 1. vendor command 0xFC66 (no parameters), then 200 ms
//! 2. 0xFC6D: the ROM version (first byte after the status)
//! 3. 0xFC61 {0x21, 0xB000ADA4}: the key ID (first byte after the status)
//! 4. the sections of this project, ROM version and key ID, by their order;
//!    for each: its register write as 0xFC62 {0x21, address, value}, then
//!    the payload in 0xFC20 commands of an index byte and up to 252 bytes.
//!    The index starts at 1 for each section, goes from 0x7F back to 1, and
//!    the controller answers each with the index it got
//! 5. 0xFC20 {0x80}: the end of the download, answered with 0x80
//!
//! The description itself is never sent, nor any config file. What the
//! driver does that this file does not: signed sections (opcode 2) and the
//! second register write (0x3EF), which this chip's file has none of; such
//! a file is refused before anything is sent.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::bringup::{Action, ChipLoader, FileReader, HciEvent, LoadError, Outcome, hci_command, parse_event};

/// The chips this loader serves: USB VID, PID, name, the driver's file
pub const CHIPS: &[(u16, u16, &str, &str)] = &[(0x0BDA, 0xB853, "RTL8852BD", "rtl8852bd_mp_chip_new.dat")];

/// The chip of a USB VID:PID, if this loader serves it
pub fn chip(vid: u16, pid: u16) -> Option<(&'static str, &'static str)> {
    CHIPS.iter().find(|c| (c.0, c.1) == (vid, pid)).map(|c| (c.2, c.3))
}

const OP_READ_LOCAL_VERSION: u16 = 0x1001;
const OP_PREPARE: u16 = 0xFC66;
const OP_READ_ROM_VERSION: u16 = 0xFC6D;
const OP_READ_REGISTER: u16 = 0xFC61;
const OP_WRITE_REGISTER: u16 = 0xFC62;
const OP_DOWNLOAD: u16 = 0xFC20;

/// The LMP subversion while the ROM code runs
const ROM_SUBVERSION: u16 = 0x8852;
/// The RTL8852B family's project ID in the sections
const PROJECT_8852B: u16 = 0x14;
/// The register 0xFC61 reads the key ID from, and the access type the
/// driver gives with it and with 0xFC62
const KEY_ID_REGISTER: u32 = 0xB000_ADA4;
const REGISTER_ACCESS: u8 = 0x21;
/// The index of the end-of-download command
const LAST_INDEX: u8 = 0x80;
const CHUNK: usize = 252;

const MAGIC: &[u8; 8] = b"BTNIC003";
const SECTIONS_AT: usize = 0x18;
const SECTION_HEADER: usize = 12;
const DESCRIPTION: usize = 0x408;

pub const COMMAND_TIMEOUT_MS: u32 = 2000;
/// After 0xFC66, as the driver waits
pub const PREPARE_WAIT_MS: u32 = 200;
/// The end-of-download answer
pub const LAST_TIMEOUT_MS: u32 = 1500;

/// A section of a BTNIC003 file
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub opcode: u32,
    pub project: u16,
    /// The ROM version + 1
    pub eco: u8,
    pub key_id: u8,
    pub reserved: u8,
    pub order: u8,
    /// The register write before the payload (address, value)
    pub write: Option<(u32, u32)>,
    /// The second one (not supported)
    pub write2: Option<(u32, u32)>,
    /// Where the payload is in the file
    pub payload: std::ops::Range<usize>,
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn write_at(desc: &[u8], at: usize) -> Option<(u32, u32)> {
    let address = u32_at(desc, at)?;
    let value = u32_at(desc, at + 8)?;
    (address != 0 && address != u32::MAX).then_some((address, value))
}

/// The sections of a BTNIC003 file; an error if it is none or cut short
pub fn sections(data: &[u8]) -> Result<Vec<Section>, &'static str> {
    if !data.starts_with(MAGIC) {
        return Err("not a BTNIC003 file");
    }
    let count = u32_at(data, 0x14).ok_or("cut short")? as usize;
    let mut at = SECTIONS_AT;
    let mut found = Vec::new();
    for _ in 0..count {
        let opcode = u32_at(data, at).ok_or("cut short")?;
        let len = u32_at(data, at + 4).ok_or("cut short")? as usize;
        let body = at + SECTION_HEADER;
        let end = body.checked_add(len).filter(|&e| e <= data.len()).ok_or("cut short")?;
        if len < DESCRIPTION {
            return Err("a section is too short");
        }
        let desc = &data[body..body + DESCRIPTION];
        let payload_len = u32_at(desc, 0x400).ok_or("cut short")? as usize;
        if payload_len > len - DESCRIPTION {
            return Err("a section's payload is longer than the section");
        }
        found.push(Section {
            opcode,
            project: u16::from_le_bytes([desc[0], desc[1]]),
            eco: desc[2],
            key_id: desc[3],
            reserved: desc[4],
            order: desc[0x3FF],
            write: write_at(desc, 0x3DF),
            write2: write_at(desc, 0x3EF),
            payload: body + DESCRIPTION..body + DESCRIPTION + payload_len,
        });
        at = end;
    }
    Ok(found)
}

/// The sections for a chip with this ROM version and key ID, in download
/// order; Err if there are none or they need what this loader cannot do
pub fn select(sections: &[Section], rom_version: u8, key_id: u8) -> Result<Vec<&Section>, &'static str> {
    let mut chosen: Vec<&Section> = sections
        .iter()
        .filter(|s| {
            s.project == PROJECT_8852B
                && s.eco == rom_version.wrapping_add(1)
                && s.key_id == key_id
                && s.reserved == 0
                && matches!(s.opcode, 1 | 2)
        })
        .collect();
    if chosen.is_empty() {
        return Err("no patch for this chip's ROM version and key ID");
    }
    if chosen.iter().any(|s| s.opcode != 1) {
        return Err("signed sections, which BlueCaster cannot send yet");
    }
    if chosen.iter().any(|s| s.write2.is_some()) {
        return Err("a second register write, which BlueCaster cannot send yet");
    }
    chosen.sort_by_key(|s| s.order);
    Ok(chosen)
}

/// What one download command expects back
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expect {
    /// A successful Command Complete of 0xFC62
    Written,
    /// The index echoed by 0xFC20
    Index(u8),
}

/// The commands of the download, built from the file before any is sent
fn download_commands(data: &[u8], chosen: &[&Section]) -> Vec<(Vec<u8>, Expect)> {
    let mut commands = Vec::new();
    for s in chosen {
        if let Some((address, value)) = s.write {
            let mut p = vec![REGISTER_ACCESS];
            p.extend_from_slice(&address.to_le_bytes());
            p.extend_from_slice(&value.to_le_bytes());
            commands.push((hci_command(OP_WRITE_REGISTER, &p), Expect::Written));
        }
        let mut index = 1u8;
        for part in data[s.payload.clone()].chunks(CHUNK) {
            let mut p = vec![index];
            p.extend_from_slice(part);
            commands.push((hci_command(OP_DOWNLOAD, &p), Expect::Index(index)));
            index = if index >= 0x7F { 1 } else { index + 1 };
        }
    }
    commands.push((hci_command(OP_DOWNLOAD, &[LAST_INDEX]), Expect::Index(LAST_INDEX)));
    commands
}

enum State {
    Idle,
    Version,
    Prepare,
    PrepareWait,
    RomVersion,
    KeyId { rom_version: u8 },
    Downloading { file: String, commands: Vec<(Vec<u8>, Expect)>, next: usize },
    Done,
}

pub struct Loader {
    read_file: FileReader,
    chip: &'static str,
    file: &'static str,
    state: State,
    started: Instant,
}

impl Loader {
    /// The loader of a chip in [`CHIPS`] (its name and file)
    pub fn new(read_file: FileReader, chip: &'static str, file: &'static str) -> Loader {
        Loader { read_file, chip, file, state: State::Idle, started: Instant::now() }
    }

    fn finish(&mut self, result: Result<Outcome, LoadError>) -> Vec<Action> {
        tracing::debug!(
            "realtek {}: loader finished after {} ms: {result:?}",
            self.chip,
            self.started.elapsed().as_millis()
        );
        self.state = State::Done;
        vec![Action::Done(result)]
    }

    fn send(&mut self, state: State, opcode: u16, params: &[u8]) -> Vec<Action> {
        self.state = state;
        vec![Action::Command(hci_command(opcode, params)), Action::Timer(COMMAND_TIMEOUT_MS)]
    }

    fn on_version(&mut self, ret: &[u8]) -> Vec<Action> {
        let Some(subversion) = ret.get(6..8).map(|v| u16::from_le_bytes([v[0], v[1]])) else {
            return self.finish(Err(LoadError::BadVersion));
        };
        let revision = ret.get(1..3).map_or(0, |v| u16::from_le_bytes([v[0], v[1]]));
        tracing::info!(
            "realtek {}: HCI version {}, revision 0x{revision:04X}, LMP subversion 0x{subversion:04X}",
            self.chip,
            ret[0]
        );
        if subversion != ROM_SUBVERSION {
            return self.finish(Ok(Outcome::AlreadyRunning));
        }
        self.send(State::Prepare, OP_PREPARE, &[])
    }

    fn after_prepare(&mut self) -> Vec<Action> {
        self.state = State::PrepareWait;
        vec![Action::Timer(PREPARE_WAIT_MS)]
    }

    fn on_key_id(&mut self, rom_version: u8, key_id: u8) -> Vec<Action> {
        tracing::info!("realtek {}: ROM version {rom_version}, key ID {key_id}", self.chip);
        let file = self.file.to_owned();
        let Some(data) = (self.read_file)(&file) else { return self.finish(Err(LoadError::MissingFile(file))) };
        let all = match sections(&data) {
            Ok(s) => s,
            Err(why) => return self.finish(Err(LoadError::BadFile { file, why })),
        };
        let chosen = match select(&all, rom_version, key_id) {
            Ok(c) => c,
            Err(why) => {
                tracing::info!("realtek {}: {file} sections {all:?}", self.chip);
                return self.finish(Err(LoadError::BadFile { file, why }));
            }
        };
        let bytes: usize = chosen.iter().map(|s| s.payload.len()).sum();
        let commands = download_commands(&data, &chosen);
        tracing::info!(
            "realtek {}: {file} ({} bytes, version {:02X?}), {} of {} sections, {bytes} bytes in {} commands",
            self.chip,
            data.len(),
            &data[8..16],
            chosen.len(),
            all.len(),
            commands.len()
        );
        self.state = State::Downloading { file, commands, next: 0 };
        self.send_next()
    }

    fn send_next(&mut self) -> Vec<Action> {
        let State::Downloading { commands, next, .. } = &self.state else { return Vec::new() };
        let Some((command, expect)) = commands.get(*next) else { return Vec::new() };
        let timeout = if *expect == Expect::Index(LAST_INDEX) { LAST_TIMEOUT_MS } else { COMMAND_TIMEOUT_MS };
        vec![Action::Command(command.clone()), Action::Timer(timeout)]
    }

    fn on_download_answer(&mut self, opcode: u16, ret: &[u8]) -> Vec<Action> {
        let State::Downloading { file, commands, next } = &mut self.state else { return Vec::new() };
        let expect = commands[*next].1.clone();
        let ok = match expect {
            Expect::Written => opcode == OP_WRITE_REGISTER,
            Expect::Index(i) => opcode == OP_DOWNLOAD && ret.first() == Some(&i),
        };
        if !ok {
            if opcode != OP_DOWNLOAD {
                return Vec::new();
            }
            // The index the controller got is not the one sent
            let got = ret.first().copied().unwrap_or(0);
            tracing::warn!("realtek: download command {} answered with index 0x{got:02X}, sent {expect:?}", *next);
            return self.finish(Err(LoadError::Rejected(got)));
        }
        *next += 1;
        if *next == commands.len() {
            let firmware = std::mem::take(file);
            return self.finish(Ok(Outcome::Loaded { firmware, detail: None }));
        }
        self.send_next()
    }

    fn on_complete(&mut self, opcode: u16, status: u8, ret: &[u8]) -> Vec<Action> {
        match (&self.state, opcode) {
            // Whatever it answers, as the driver goes on
            (State::Prepare, OP_PREPARE) => {
                tracing::debug!("realtek {}: 0xFC66 status 0x{status:02X}", self.chip);
                self.after_prepare()
            }
            (_, _) if status != 0 && !matches!(self.state, State::Idle | State::Done) => {
                self.finish(Err(LoadError::Command { opcode, status }))
            }
            (State::Version, OP_READ_LOCAL_VERSION) => self.on_version(ret),
            (State::RomVersion, OP_READ_ROM_VERSION) => match ret.first() {
                Some(&rom_version) => self.send(
                    State::KeyId { rom_version },
                    OP_READ_REGISTER,
                    &[&[REGISTER_ACCESS][..], &KEY_ID_REGISTER.to_le_bytes()].concat(),
                ),
                None => self.finish(Err(LoadError::BadVersion)),
            },
            (&State::KeyId { rom_version }, OP_READ_REGISTER) => match ret.first() {
                Some(&key_id) => self.on_key_id(rom_version, key_id),
                None => self.finish(Err(LoadError::BadVersion)),
            },
            (State::Downloading { .. }, OP_WRITE_REGISTER | OP_DOWNLOAD) => self.on_download_answer(opcode, ret),
            // A late answer: ignored
            _ => Vec::new(),
        }
    }
}

impl ChipLoader for Loader {
    fn vendor(&self) -> &'static str {
        "Realtek"
    }

    fn start(&mut self) -> Vec<Action> {
        tracing::info!("realtek {}: checking its firmware (experimental), {}", self.chip, self.file);
        self.send(State::Version, OP_READ_LOCAL_VERSION, &[])
    }

    fn on_event(&mut self, event: &[u8]) -> Vec<Action> {
        match parse_event(event) {
            HciEvent::Complete { opcode, status, ret } => self.on_complete(opcode, status, ret),
            HciEvent::Status { opcode, status } if status != 0 => match self.state {
                State::Prepare if opcode == OP_PREPARE => self.after_prepare(),
                State::Idle | State::Done => Vec::new(),
                _ => self.finish(Err(LoadError::Command { opcode, status })),
            },
            _ => Vec::new(),
        }
    }

    fn on_timeout(&mut self) -> Vec<Action> {
        match self.state {
            // Not every ROM may answer it: go on as after an answer
            State::Prepare => self.after_prepare(),
            State::PrepareWait => self.send(State::RomVersion, OP_READ_ROM_VERSION, &[]),
            State::Version => self.finish(Err(LoadError::Timeout("Read Local Version"))),
            State::RomVersion => self.finish(Err(LoadError::Timeout("read ROM version"))),
            State::KeyId { .. } => self.finish(Err(LoadError::Timeout("read key ID"))),
            State::Downloading { .. } => self.finish(Err(LoadError::Timeout("firmware download"))),
            State::Idle | State::Done => Vec::new(),
        }
    }

    fn missing_file_hint(&self, file: &str) -> String {
        format!("{file} (from Realtek's Windows Bluetooth driver)")
    }

    fn step(&self) -> &'static str {
        match self.state {
            State::Downloading { .. } => "download",
            _ => "",
        }
    }

    fn progress(&self) -> Option<(usize, usize)> {
        match &self.state {
            State::Downloading { commands, next, .. } => Some((*next, commands.len())),
            _ => None,
        }
    }
}

// ---- Realtek's Windows driver ----

/// Where Realtek's Windows driver keeps the file on this PC, newest first:
/// any folder of the driver store that has it (OEM packages name their INF
/// differently), System32\drivers, and the folder the driver reads updates
/// from. All stay after the adapter moved to WinUSB.
fn driver_files(windows: &Path, name: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let store = windows.join("System32").join("DriverStore").join("FileRepository");
    if let Ok(entries) = std::fs::read_dir(store) {
        found.extend(entries.flatten().map(|e| e.path().join(name)).filter(|p| p.is_file()));
    }
    let program_data =
        std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    for p in
        [windows.join("System32").join("drivers").join(name), program_data.join("Realtek").join("Patch").join(name)]
    {
        if p.is_file() {
            found.push(p);
        }
    }
    let modified = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    found.sort_by_key(|p| std::cmp::Reverse(modified(p)));
    found
}

/// A firmware file from Realtek's Windows driver, if it is installed
pub fn from_windows_driver(name: &str) -> Option<Vec<u8>> {
    if name.contains(['\\', '/']) {
        return None;
    }
    let windows = std::env::var_os("SystemRoot").map(PathBuf::from)?;
    driver_files(&windows, name).iter().find_map(|path| {
        let data = std::fs::read(path).ok().filter(|d| d.starts_with(MAGIC))?;
        tracing::info!("realtek: {name} from Realtek's driver ({}), {} bytes", path.display(), data.len());
        Some(data)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bringup::sim::{self, SimController, command_complete};

    /// A test section: opcode, eco, key ID, order, register write, payload
    type TestSection = (u32, u8, u8, u8, Option<(u32, u32)>, Vec<u8>);

    /// A BTNIC003 file of these sections
    fn container(sections: &[TestSection]) -> Vec<u8> {
        let mut f = MAGIC.to_vec();
        f.extend_from_slice(&[0x2E, 0x28, 0x35, 0x01, 0xCF, 0x34, 0x03, 0x00]);
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(&(sections.len() as u32).to_le_bytes());
        for (opcode, eco, key, order, write, payload) in sections {
            let mut desc = vec![0u8; DESCRIPTION];
            desc[0..2].copy_from_slice(&PROJECT_8852B.to_le_bytes());
            desc[2] = *eco;
            desc[3] = *key;
            if let Some((a, v)) = write {
                desc[0x3DF..0x3E3].copy_from_slice(&a.to_le_bytes());
                desc[0x3E7..0x3EB].copy_from_slice(&v.to_le_bytes());
            }
            desc[0x3FF] = *order;
            desc[0x400..0x404].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            f.extend_from_slice(&opcode.to_le_bytes());
            f.extend_from_slice(&((DESCRIPTION + payload.len()) as u32).to_le_bytes());
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&desc);
            f.extend_from_slice(payload);
        }
        f
    }

    fn sample_file() -> Vec<u8> {
        container(&[
            (1, 4, 0, 1, Some((0x8012_00CC, 0x8010_E990)), vec![0xB1; 10]),
            (1, 4, 0, 0, Some((0x8012_00CC, 0x8010_F720)), vec![0xA1; 300]),
            // Another ROM version: never sent
            (1, 2, 0, 0, None, vec![0xEE; 20]),
        ])
    }

    /// An RTL8852BD in its ROM: ROM version 3, key ID 0
    struct Chip {
        subversion: u16,
        rom_version: u8,
        /// Answer the end of the download with this index
        last_answer: u8,
    }

    impl SimController for Chip {
        fn command(&mut self, c: &[u8]) -> Vec<Vec<u8>> {
            let opcode = sim::opcode(c);
            let p = sim::params(c);
            match opcode {
                OP_READ_LOCAL_VERSION => {
                    let mut ret = vec![0x0B, 0x00, 0x00, 0x0B, 0x5D, 0x00];
                    ret.extend_from_slice(&self.subversion.to_le_bytes());
                    vec![command_complete(opcode, 0, &ret)]
                }
                OP_PREPARE => vec![command_complete(opcode, 0, &[])],
                OP_READ_ROM_VERSION => vec![command_complete(opcode, 0, &[self.rom_version])],
                OP_READ_REGISTER => vec![command_complete(opcode, 0, &[0x00, 0, 0, 0])],
                OP_WRITE_REGISTER => vec![command_complete(opcode, 0, &[])],
                OP_DOWNLOAD if p == [LAST_INDEX] => vec![command_complete(opcode, 0, &[self.last_answer])],
                OP_DOWNLOAD => vec![command_complete(opcode, 0, &[p[0]])],
                _ => Vec::new(),
            }
        }
    }

    fn run(chip: &mut Chip, file: Option<Vec<u8>>) -> sim::SimRun {
        let mut loader = Loader::new(Box::new(move |_| file.clone()), "RTL8852BD", "rtl8852bd_mp_chip_new.dat");
        sim::run(&mut loader, chip)
    }

    #[test]
    fn the_sections_are_read_from_the_container() {
        let f = sample_file();
        let s = sections(&f).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].eco, s[0].order, s[0].write), (4, 1, Some((0x8012_00CC, 0x8010_E990))));
        assert_eq!(&f[s[1].payload.clone()], &[0xA1; 300][..]);
        assert_eq!(s[2].write, None);
        assert!(sections(&f[..f.len() - 1]).is_err());
        assert!(sections(b"RTBTCore").is_err());
    }

    #[test]
    fn only_the_rom_versions_sections_are_chosen_in_their_order() {
        let f = sample_file();
        let all = sections(&f).unwrap();
        let chosen = select(&all, 3, 0).unwrap();
        assert_eq!(chosen.iter().map(|s| s.order).collect::<Vec<_>>(), [0, 1]);
        assert!(select(&all, 3, 1).is_err(), "another key ID");
        assert!(select(&all, 5, 0).is_err(), "another ROM version");
    }

    #[test]
    fn the_download_follows_the_drivers_sequence() {
        let mut chip = Chip { subversion: ROM_SUBVERSION, rom_version: 3, last_answer: LAST_INDEX };
        let r = run(&mut chip, Some(sample_file()));
        assert_eq!(r.result, Ok(Outcome::Loaded { firmware: "rtl8852bd_mp_chip_new.dat".into(), detail: None }));
        assert_eq!(
            r.opcodes(),
            [
                OP_READ_LOCAL_VERSION,
                OP_PREPARE,
                OP_READ_ROM_VERSION,
                OP_READ_REGISTER,
                OP_WRITE_REGISTER,
                OP_DOWNLOAD,
                OP_DOWNLOAD,
                OP_WRITE_REGISTER,
                OP_DOWNLOAD,
                OP_DOWNLOAD
            ]
        );
        assert_eq!(r.timers[..3], [COMMAND_TIMEOUT_MS, COMMAND_TIMEOUT_MS, PREPARE_WAIT_MS]);
        assert_eq!(r.commands[3].params(), [0x21, 0xA4, 0xAD, 0x00, 0xB0]);
        // The order-0 section first, its register write, then its payload
        // from index 1: 252 bytes, then 48
        assert_eq!(r.commands[4].params(), [0x21, 0xCC, 0x00, 0x12, 0x80, 0x20, 0xF7, 0x10, 0x80]);
        assert_eq!(r.commands[5].params()[0], 1);
        assert_eq!(r.commands[5].params().len(), 253);
        assert_eq!(r.commands[6].params()[0], 2);
        assert_eq!(&r.commands[6].params()[1..], &[0xA1; 48][..]);
        // The next section starts at index 1 again
        assert_eq!(r.commands[7].params(), [0x21, 0xCC, 0x00, 0x12, 0x80, 0x90, 0xE9, 0x10, 0x80]);
        assert_eq!(r.commands[8].params(), [&[1u8][..], &[0xB1; 10]].concat());
        assert_eq!(r.commands[9].params(), [LAST_INDEX]);
        assert_eq!(*r.timers.last().unwrap(), LAST_TIMEOUT_MS);
        assert!(!r.bootloader);
    }

    #[test]
    fn the_index_goes_from_0x7f_back_to_1() {
        let f = container(&[(1, 4, 0, 0, None, vec![0x55; CHUNK * 130])]);
        let all = sections(&f).unwrap();
        let commands = download_commands(&f, &select(&all, 3, 0).unwrap());
        let indexes: Vec<u8> = commands.iter().map(|(c, _)| c[3]).collect();
        assert_eq!(indexes[126], 0x7F);
        assert_eq!(indexes[127], 1);
        assert_eq!(*indexes.last().unwrap(), LAST_INDEX);
    }

    #[test]
    fn running_firmware_is_left_alone() {
        let mut chip = Chip { subversion: 0x950E, rom_version: 3, last_answer: LAST_INDEX };
        let r = run(&mut chip, Some(sample_file()));
        assert_eq!(r.result, Ok(Outcome::AlreadyRunning));
        assert_eq!(r.opcodes(), [OP_READ_LOCAL_VERSION]);
    }

    #[test]
    fn nothing_is_sent_without_a_patch_for_the_rom() {
        let mut chip = Chip { subversion: ROM_SUBVERSION, rom_version: 5, last_answer: LAST_INDEX };
        let r = run(&mut chip, Some(sample_file()));
        assert!(matches!(r.result, Err(LoadError::BadFile { .. })), "{:?}", r.result);
        assert!(!r.opcodes().contains(&OP_DOWNLOAD));
        let r = run(&mut Chip { subversion: ROM_SUBVERSION, rom_version: 3, last_answer: LAST_INDEX }, None);
        assert_eq!(r.result, Err(LoadError::MissingFile("rtl8852bd_mp_chip_new.dat".into())));
    }

    #[test]
    fn a_wrong_end_answer_fails_the_download() {
        let mut chip = Chip { subversion: ROM_SUBVERSION, rom_version: 3, last_answer: 0x05 };
        let r = run(&mut chip, Some(sample_file()));
        assert_eq!(r.result, Err(LoadError::Rejected(0x05)));
    }

    #[test]
    fn a_silent_prepare_command_is_passed() {
        struct Quiet(Chip);
        impl SimController for Quiet {
            fn command(&mut self, c: &[u8]) -> Vec<Vec<u8>> {
                if sim::opcode(c) == OP_PREPARE { Vec::new() } else { self.0.command(c) }
            }
        }
        let mut chip = Quiet(Chip { subversion: ROM_SUBVERSION, rom_version: 3, last_answer: LAST_INDEX });
        let mut loader = Loader::new(Box::new(move |_| Some(sample_file())), "RTL8852BD", "rtl8852bd_mp_chip_new.dat");
        let r = sim::run(&mut loader, &mut chip);
        assert!(matches!(r.result, Ok(Outcome::Loaded { .. })), "{:?}", r.result);
    }

    #[test]
    fn the_rtl8852bd_is_a_realtek_adapter_outside_btstacks_table() {
        use crate::chips::{self, ChipVendor};
        // No BTstack Realtek init for it: that would look for rtl8852bu_fw
        assert_eq!(chips::resolve_realtek_pid(0x0BDA, 0xB853, crate::realtek_usb_firmware()), None);
        assert_eq!(chips::vendor_for_usb(0x0BDA, 0xB853, None), ChipVendor::Realtek);
        assert_eq!(
            chips::display_name(0x0BDA, 0xB853, None, ChipVendor::Realtek),
            "RTL8852BD (0x0BDA:0xB853) [experimental]"
        );
        assert_eq!(chips::chip_support_tag(ChipVendor::Realtek, 0x0BDA, 0xB853), Some("experimental"));
        assert_eq!(chips::chip_support_tag(ChipVendor::Realtek, 0x0BDA, 0xB85B), None);
        assert_eq!(chip(0x0BDA, 0x4853), None);
    }

    /// Against Realtek's real file: `RTL8852BD_DAT=<path to
    /// rtl8852bd_mp_chip_new.dat> cargo test -p bt-host real_btnic -- --ignored`
    #[test]
    #[ignore = "needs Realtek's rtl8852bd_mp_chip_new.dat (RTL8852BD_DAT)"]
    fn real_btnic_file() {
        let data = std::fs::read(std::env::var("RTL8852BD_DAT").unwrap()).unwrap();
        let all = sections(&data).unwrap();
        for s in &all {
            println!("{:?} payload {} bytes", s, s.payload.len());
        }
        let chosen = select(&all, 3, 0).unwrap();
        let commands = download_commands(&data, &chosen);
        println!("ROM version 3, key 0: {} sections, {} commands", chosen.len(), commands.len());
        let mut chip = Chip { subversion: ROM_SUBVERSION, rom_version: 3, last_answer: LAST_INDEX };
        let r = run(&mut chip, Some(data));
        assert!(matches!(r.result, Ok(Outcome::Loaded { .. })), "{:?}", r.result);
    }
}

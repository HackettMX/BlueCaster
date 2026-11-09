//! Developer > Debug: one HCI command typed in as hex, sent once after HCI
//! init on a MediaTek controller (issue #86: trying a vendor command whose
//! effect is not documented). Nothing is sent unless one is entered; its
//! answer is logged. The controller forgets it on the next HCI Reset (each
//! session's start and close), so taking it out is the way back.

use std::sync::Mutex;

static COMMAND: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// The command for the next sessions (None: none)
pub fn set(packet: Option<Vec<u8>>) {
    *COMMAND.lock().unwrap_or_else(|e| e.into_inner()) = packet;
}

pub(crate) fn get() -> Option<Vec<u8>> {
    COMMAND.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// "FD98 08 01": the opcode as four hex digits, then each parameter byte
/// as two, separated by spaces; the command packet (opcode little-endian,
/// parameter length, parameters). Ok(None): nothing entered.
pub fn parse(text: &str) -> Result<Option<Vec<u8>>, String> {
    let mut tokens = text.split_whitespace();
    let Some(op) = tokens.next() else { return Ok(None) };
    if op.len() != 4 {
        return Err(format!("opcode \"{op}\": four hex digits"));
    }
    let opcode = u16::from_str_radix(op, 16).map_err(|_| format!("opcode \"{op}\" is not hex"))?;
    let params = tokens
        .map(|t| match t.len() {
            2 => u8::from_str_radix(t, 16).map_err(|_| format!("\"{t}\" is not hex")),
            _ => Err(format!("parameter \"{t}\": two hex digits")),
        })
        .collect::<Result<Vec<u8>, String>>()?;
    let len = u8::try_from(params.len()).map_err(|_| "more than 255 parameter bytes".to_string())?;
    let mut packet = opcode.to_le_bytes().to_vec();
    packet.push(len);
    packet.extend(params);
    Ok(Some(packet))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_opcode_and_parameters() {
        assert_eq!(parse("FD98 08 01"), Ok(Some(vec![0x98, 0xFD, 0x02, 0x08, 0x01])));
        assert_eq!(parse("  0c03 "), Ok(Some(vec![0x03, 0x0C, 0x00])));
        assert_eq!(parse(""), Ok(None));
    }

    #[test]
    fn refuses_malformed_input() {
        assert!(parse("FD9 08").is_err());
        assert!(parse("FD98 8").is_err());
        assert!(parse("FD98 GG").is_err());
        assert!(parse(&format!("FD98{}", " 00".repeat(256))).is_err());
    }
}

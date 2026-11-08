//! Status and error messages as translation keys with arguments: the UI
//! translates them (app/lang/*.json, shared with the UI); logs and the CLI use
//! the English text.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

/// A message of app/lang (key, printf-style arguments)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Msg {
    pub key: &'static str,
    pub args: Vec<String>,
    /// An error that stopped the stream over its codec: which codec, and
    /// why ([`Msg::codec_stop`])
    #[serde(skip)]
    pub codec_stop: Option<CodecStop>,
}

/// A stream stopped over its codec (issue #86): the tray and the
/// connection cards name the cause in a few words, and choosing another
/// codec for that device connects it again
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecStop {
    pub codec: codec::CodecKind,
    pub kind: CodecStopKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecStopKind {
    /// The device does not offer the codec, or refused it when asked anyway
    Unsupported,
    /// The device refused settings of the codec it does not list
    SettingsUnsupported,
    /// Another source holds the device's endpoint of the codec
    InUse,
    /// The device refused the configuration of a codec it lists
    Refused,
}

impl Msg {
    pub fn new(key: &'static str) -> Msg {
        Msg { key, args: Vec::new(), codec_stop: None }
    }

    pub fn with(key: &'static str, args: &[&dyn fmt::Display]) -> Msg {
        Msg { key, args: args.iter().map(|a| a.to_string()).collect(), codec_stop: None }
    }

    /// `key` naming `codec`, for a stop that `codec` caused
    pub fn codec_stop(key: &'static str, codec: codec::CodecKind, kind: CodecStopKind) -> Msg {
        Msg { codec_stop: Some(CodecStop { codec, kind }), ..Msg::with(key, &[&codec.name()]) }
    }

    /// The English text
    pub fn english(&self) -> String {
        let template = english_strings().get(self.key).map(String::as_str).unwrap_or(self.key);
        format_printf(template, &self.args)
    }
}

impl fmt::Display for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.english())
    }
}

/// Text of `key` in `lang` (en, ja, de, fr, es, ko, zh-CN, zh-TW), English
/// when it has no translation, `default` when not even English has it. For
/// the few texts the core shows itself (tray menu, notifications).
pub fn translate(lang: &str, key: &str, default: &str) -> String {
    static LANGS: OnceLock<HashMap<&'static str, HashMap<String, String>>> = OnceLock::new();
    let langs = LANGS.get_or_init(|| {
        let files: [(&str, &str); 7] = [
            ("ja", include_str!("../../../../../app/lang/ja.json")),
            ("de", include_str!("../../../../../app/lang/de.json")),
            ("fr", include_str!("../../../../../app/lang/fr.json")),
            ("es", include_str!("../../../../../app/lang/es.json")),
            ("ko", include_str!("../../../../../app/lang/ko.json")),
            ("zh-CN", include_str!("../../../../../app/lang/zh-CN.json")),
            ("zh-TW", include_str!("../../../../../app/lang/zh-TW.json")),
        ];
        files.iter().map(|(l, text)| (*l, serde_json::from_str(text).unwrap_or_default())).collect()
    });
    langs
        .get(lang)
        .and_then(|m| m.get(key))
        .or_else(|| english_strings().get(key))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn english_strings() -> &'static HashMap<String, String> {
    static STRINGS: OnceLock<HashMap<String, String>> = OnceLock::new();
    STRINGS.get_or_init(|| serde_json::from_str(include_str!("../../../../../app/lang/en.json")).unwrap_or_default())
}

/// Fills %s / %d / %u / %zu / %.1f placeholders in order (the formats the
/// language files use); %% is a percent sign
pub fn format_printf(template: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut args = args.iter();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }
        // Flags, width, precision, length: up to the conversion letter
        let mut spec = String::new();
        while let Some(&n) = chars.peek() {
            chars.next();
            if n.is_ascii_alphabetic() && !matches!(n, 'z' | 'l' | 'h') {
                break;
            }
            spec.push(n);
        }
        out.push_str(args.next().map(String::as_str).unwrap_or(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printf_placeholders() {
        let s = format_printf(
            "try %d/%d, %zu found, %.1f kHz, 100%%",
            &["2".into(), "3".into(), "4".into(), "48.0".into()],
        );
        assert_eq!(s, "try 2/3, 4 found, 48.0 kHz, 100%");
    }

    #[test]
    fn english_text() {
        let m = Msg::with("status.connecting_retry", &[&2, &3]);
        assert_eq!(m.english(), "Connection failed, trying again (2/3)...");
    }

    #[test]
    fn a_codec_stop_names_its_codec() {
        let m = Msg::codec_stop("error.codec_not_supported", codec::CodecKind::Ldac, CodecStopKind::Unsupported);
        assert_eq!(m.english(), "Device does not support LDAC");
        assert_eq!(m.codec_stop, Some(CodecStop { codec: codec::CodecKind::Ldac, kind: CodecStopKind::Unsupported }));
        assert_eq!(Msg::new("error.codec_configure").codec_stop, None);
    }
}

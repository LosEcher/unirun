//! Encoding pipeline: BOM detection, UTF-16 decode, UTF-8 fast path, legacy
//! code-page fallback, lossy last resort. The agent always receives valid
//! Unicode text plus a stable `encoding` label.
//!
//! The label is **decode provenance**, not a verified fact: `utf-8` means the
//! bytes were valid UTF-8, `gbk` means they were decoded through CP936 because
//! they were not valid UTF-8, `utf-8-lossy` means nothing decoded cleanly and
//! unmappable bytes became U+FFFD. An agent that must be certain should prefer
//! evidence it can check itself.
//!
//! Why the legacy fallback exists: PowerShell 5.1 writes *stderr* through the
//! OEM code page even with the golden recipe applied (`win-exec/README.md:81`),
//! and Windows OpenSSH answers `uname` with a GBK error string instead of
//! "unsupported". Labelling those bytes `utf-8-lossy` lost the text; they are
//! decodable.

/// Result of decoding one byte stream.
pub struct Decoded {
    pub text: String,
    pub encoding: &'static str,
}

/// A legacy code page unirun can decode, with the label it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyCodePage {
    /// CP936 / GBK (superset of GB2312) — the mainland-Chinese ANSI and OEM page.
    Gbk,
    /// CP950 / Big5 — the traditional-Chinese page.
    Big5,
    /// CP437 — the US OEM page (console output on English Windows).
    Cp437,
    /// CP850 — the Western-European OEM page.
    Cp850,
    /// CP1252 — the Western-European ANSI page.
    Windows1252,
}

impl LegacyCodePage {
    pub fn label(self) -> &'static str {
        match self {
            LegacyCodePage::Gbk => "gbk",
            LegacyCodePage::Big5 => "big5",
            LegacyCodePage::Cp437 => "cp437",
            LegacyCodePage::Cp850 => "cp850",
            LegacyCodePage::Windows1252 => "windows-1252",
        }
    }

    /// Parse a code-page name. Accepts the names an operator or recipe is
    /// likely to write, since `.unirun/recipe.toml` and `--output-encoding`
    /// both take free text.
    pub fn from_name(name: &str) -> Option<LegacyCodePage> {
        let n = name.trim().to_ascii_lowercase().replace('_', "-");
        match n.as_str() {
            "gbk" | "cp936" | "ms936" | "gb2312" | "gb-2312" | "gb18030" | "ansi-cn" => {
                Some(LegacyCodePage::Gbk)
            }
            "big5" | "cp950" | "ms950" => Some(LegacyCodePage::Big5),
            "cp437" | "ibm437" | "oem-us" | "oem" => Some(LegacyCodePage::Cp437),
            "cp850" | "ibm850" | "oem-latin1" => Some(LegacyCodePage::Cp850),
            "cp1252" | "windows-1252" | "win1252" | "latin1" | "latin-1" | "ansi" => {
                Some(LegacyCodePage::Windows1252)
            }
            _ => None,
        }
    }
}

/// Code pages tried automatically when the bytes are not valid UTF-8.
///
/// Only one entry, and it is deliberately the mainland-Chinese page: CP936 and
/// CP950 (Big5) occupy overlapping byte ranges, so a Big5 string usually
/// decodes through GBK *without errors* — there is no way to tell them apart
/// from the bytes alone (`中文` in Big5 is `A4 A4 A4 E5`, which GBK happily
/// renders as `いゅ`). Big5 is therefore hint-only: declare
/// `encoding = "big5"` in the recipe or pass `--output-encoding big5`.
///
/// The single-byte pages are hint-only for a different reason: they map every
/// byte value, so a clean decode carries no evidence at all.
pub const AUTO_CODE_PAGES: [LegacyCodePage; 1] = [LegacyCodePage::Gbk];

/// Decode raw child output to Unicode text.
///
/// Strategy:
/// 1. BOM sniffing (UTF-8 / UTF-16LE / UTF-16BE) — self-describing evidence.
/// 2. Explicit hint (legacy page) → decode through it and label it; pure ASCII
///    is still reported as `utf-8`, since every page we support agrees there.
/// 3. Valid UTF-8 fast path (with the BOM-less UTF-16LE NUL heuristic).
/// 4. Auto fallback: CP936/GBK if it decodes with zero errors.
/// 5. Lossy fallback (`utf-8-lossy`): never fail, but label it so agents can
///    treat the text as best-effort.
pub fn decode(bytes: &[u8]) -> Decoded {
    decode_with(bytes, None)
}

/// [`decode`], with an optional explicit code-page hint.
///
/// The hint is applied *after* BOM evidence and *before* the valid-UTF-8 fast
/// path, so a project that declares `encoding = "gbk"` gets GBK even when the
/// byte stream happens to be valid UTF-8 by accident.
pub fn decode_with(bytes: &[u8], hint: Option<&str>) -> Decoded {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let rest = &bytes[3..];
        return decode_as_utf8(rest);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Decoded {
            text: decode_utf16(&bytes[2..], true),
            encoding: "utf-16le",
        };
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Decoded {
            text: decode_utf16(&bytes[2..], false),
            encoding: "utf-16be",
        };
    }
    if let Some(hint) = hint {
        let normalized = hint.trim().to_ascii_lowercase();
        // An explicit UTF-8 declaration disables the CP936 guess below: an
        // operator who says "this fleet is UTF-8" wants the lossy label, not a
        // code page chosen for them.
        if matches!(normalized.as_str(), "utf-8" | "utf8") {
            return decode_as_utf8(bytes);
        }
        if let Some(page) = LegacyCodePage::from_name(&normalized) {
            if bytes.is_ascii() {
                return Decoded {
                    text: String::from_utf8_lossy(bytes).into_owned(),
                    encoding: "utf-8",
                };
            }
            let (text, _) = decode_legacy(bytes, page);
            return Decoded {
                text,
                encoding: page.label(),
            };
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => {
            // UTF-16LE without BOM (e.g. the WSL launcher's "no installed
            // distributions" message) is *valid* UTF-8 full of NULs — detect
            // the NUL pattern and decode it properly instead of handing the
            // agent NUL-garbage labeled "utf-8".
            if looks_utf16le_without_bom(bytes) {
                Decoded {
                    text: decode_utf16(bytes, true),
                    encoding: "utf-16le",
                }
            } else {
                Decoded {
                    text: s.to_string(),
                    encoding: "utf-8",
                }
            }
        }
        Err(_) => {
            for page in AUTO_CODE_PAGES {
                let (text, had_errors) = decode_legacy(bytes, page);
                if !had_errors {
                    return Decoded {
                        text,
                        encoding: page.label(),
                    };
                }
            }
            Decoded {
                text: String::from_utf8_lossy(bytes).into_owned(),
                encoding: "utf-8-lossy",
            }
        }
    }
}

fn decode_as_utf8(bytes: &[u8]) -> Decoded {
    match std::str::from_utf8(bytes) {
        Ok(s) => Decoded {
            text: s.to_string(),
            encoding: "utf-8",
        },
        Err(_) => Decoded {
            text: String::from_utf8_lossy(bytes).into_owned(),
            encoding: "utf-8-lossy",
        },
    }
}

/// Decode through one legacy page. `had_errors` reports whether the page could
/// not map every byte/sequence (always `false` for the single-byte pages,
/// which map all 256 values by construction).
fn decode_legacy(bytes: &[u8], page: LegacyCodePage) -> (String, bool) {
    match page {
        LegacyCodePage::Gbk => {
            let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
            (text.into_owned(), had_errors)
        }
        LegacyCodePage::Big5 => {
            let (text, _, had_errors) = encoding_rs::BIG5.decode(bytes);
            (text.into_owned(), had_errors)
        }
        LegacyCodePage::Windows1252 => {
            let (text, _, had_errors) = encoding_rs::WINDOWS_1252.decode(bytes);
            (text.into_owned(), had_errors)
        }
        LegacyCodePage::Cp437 => (decode_single_byte(bytes, &CP437_HIGH), false),
        LegacyCodePage::Cp850 => (decode_single_byte(bytes, &CP850_HIGH), false),
    }
}

/// Decode a single-byte OEM page: ASCII passes through, 0x80–0xFF come from
/// the table. Generated from the Unicode consortium's CP437/CP850 mappings
/// (the same values `MultiByteToWideChar(CP_OEMCP, …)` produces).
fn decode_single_byte(bytes: &[u8], high: &[u16; 128]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b < 0x80 {
            out.push(b as char);
        } else {
            let u = high[(b - 0x80) as usize];
            out.push(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
        }
    }
    out
}

/// Normalize CRLF to LF in decoded text (Windows shells emit `\r\n`;
/// agents should see the same line endings on every platform).
pub fn normalize_line_endings(text: &str) -> String {
    if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    }
}

/// BOM-less UTF-16LE heuristic: even byte count with NULs at (at least two)
/// odd byte positions — the signature of ASCII-range UTF-16LE text.
fn looks_utf16le_without_bom(bytes: &[u8]) -> bool {
    if bytes.len() < 4 || !bytes.len().is_multiple_of(2) {
        return false;
    }
    let nul_at_odd = bytes
        .iter()
        .enumerate()
        .filter(|(i, b)| i % 2 == 1 && **b == 0)
        .count();
    nul_at_odd >= 2
}

fn decode_utf16(units: &[u8], little_endian: bool) -> String {
    let mut out = String::new();
    let mut chars = units.as_chunks::<2>().0.iter().map(|c| {
        let u = u16::from_le_bytes([c[0], c[1]]);
        if little_endian {
            u
        } else {
            u16::from_be_bytes([c[0], c[1]])
        }
    });
    // Surrogate pair handling.
    while let Some(u) = chars.next() {
        if (0xD800..=0xDBFF).contains(&u) {
            if let Some(low) = chars.next() {
                if (0xDC00..=0xDFFF).contains(&low) {
                    let cp = 0x10000 + (((u as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c);
                        continue;
                    }
                }
            }
            out.push('\u{FFFD}');
        } else if (0xDC00..=0xDFFF).contains(&u) {
            out.push('\u{FFFD}');
        } else if let Some(c) = char::from_u32(u as u32) {
            out.push(c);
        } else {
            out.push('\u{FFFD}');
        }
    }
    // Trailing odd byte.
    if units.len() % 2 == 1 {
        out.push('\u{FFFD}');
    }
    out
}

/// High half of CP437 (0x80–0xFF), from the Unicode CP437 mapping.
const CP437_HIGH: [u16; 128] = [
    0x00C7, 0x00FC, 0x00E9, 0x00E2, 0x00E4, 0x00E0, 0x00E5, 0x00E7, 0x00EA, 0x00EB, 0x00E8, 0x00EF,
    0x00EE, 0x00EC, 0x00C4, 0x00C5, 0x00C9, 0x00E6, 0x00C6, 0x00F4, 0x00F6, 0x00F2, 0x00FB, 0x00F9,
    0x00FF, 0x00D6, 0x00DC, 0x00A2, 0x00A3, 0x00A5, 0x20A7, 0x0192, 0x00E1, 0x00ED, 0x00F3, 0x00FA,
    0x00F1, 0x00D1, 0x00AA, 0x00BA, 0x00BF, 0x2310, 0x00AC, 0x00BD, 0x00BC, 0x00A1, 0x00AB, 0x00BB,
    0x2591, 0x2592, 0x2593, 0x2502, 0x2524, 0x2561, 0x2562, 0x2556, 0x2555, 0x2563, 0x2551, 0x2557,
    0x255D, 0x255C, 0x255B, 0x2510, 0x2514, 0x2534, 0x252C, 0x251C, 0x2500, 0x253C, 0x255E, 0x255F,
    0x255A, 0x2554, 0x2569, 0x2566, 0x2560, 0x2550, 0x256C, 0x2567, 0x2568, 0x2564, 0x2565, 0x2559,
    0x2558, 0x2552, 0x2553, 0x256B, 0x256A, 0x2518, 0x250C, 0x2588, 0x2584, 0x258C, 0x2590, 0x2580,
    0x03B1, 0x00DF, 0x0393, 0x03C0, 0x03A3, 0x03C3, 0x00B5, 0x03C4, 0x03A6, 0x0398, 0x03A9, 0x03B4,
    0x221E, 0x03C6, 0x03B5, 0x2229, 0x2261, 0x00B1, 0x2265, 0x2264, 0x2320, 0x2321, 0x00F7, 0x2248,
    0x00B0, 0x2219, 0x00B7, 0x221A, 0x207F, 0x00B2, 0x25A0, 0x00A0,
];

/// High half of CP850 (0x80–0xFF), from the Unicode CP850 mapping.
const CP850_HIGH: [u16; 128] = [
    0x00C7, 0x00FC, 0x00E9, 0x00E2, 0x00E4, 0x00E0, 0x00E5, 0x00E7, 0x00EA, 0x00EB, 0x00E8, 0x00EF,
    0x00EE, 0x00EC, 0x00C4, 0x00C5, 0x00C9, 0x00E6, 0x00C6, 0x00F4, 0x00F6, 0x00F2, 0x00FB, 0x00F9,
    0x00FF, 0x00D6, 0x00DC, 0x00F8, 0x00A3, 0x00D8, 0x00D7, 0x0192, 0x00E1, 0x00ED, 0x00F3, 0x00FA,
    0x00F1, 0x00D1, 0x00AA, 0x00BA, 0x00BF, 0x00AE, 0x00AC, 0x00BD, 0x00BC, 0x00A1, 0x00AB, 0x00BB,
    0x2591, 0x2592, 0x2593, 0x2502, 0x2524, 0x00C1, 0x00C2, 0x00C0, 0x00A9, 0x2563, 0x2551, 0x2557,
    0x255D, 0x00A2, 0x00A5, 0x2510, 0x2514, 0x2534, 0x252C, 0x251C, 0x2500, 0x253C, 0x00E3, 0x00C3,
    0x255A, 0x2554, 0x2569, 0x2566, 0x2560, 0x2550, 0x256C, 0x00A4, 0x00F0, 0x00D0, 0x00CA, 0x00CB,
    0x00C8, 0x0131, 0x00CD, 0x00CE, 0x00CF, 0x2518, 0x250C, 0x2588, 0x2584, 0x00A6, 0x00CC, 0x2580,
    0x00D3, 0x00DF, 0x00D4, 0x00D2, 0x00F5, 0x00D5, 0x00B5, 0x00FE, 0x00DE, 0x00DA, 0x00DB, 0x00D9,
    0x00FD, 0x00DD, 0x00AF, 0x00B4, 0x00AD, 0x00B1, 0x2017, 0x00BE, 0x00B6, 0x00A7, 0x00F7, 0x00B8,
    0x00B0, 0x00A8, 0x00B7, 0x00B9, 0x00B3, 0x00B2, 0x25A0, 0x00A0,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// GBK bytes for 「你好」 — the exact case from the audit: previously
    /// reported as `���` with `encoding: "utf-8-lossy"`.
    const GBK_NIHAO: [u8; 4] = [0xC4, 0xE3, 0xBA, 0xC3];

    #[test]
    fn utf8_plain() {
        let d = decode("中文 OK".as_bytes());
        assert_eq!(d.text, "中文 OK");
        assert_eq!(d.encoding, "utf-8");
    }

    #[test]
    fn utf8_bom_stripped() {
        let mut v = vec![0xEF, 0xBB, 0xBF];
        v.extend_from_slice("hi".as_bytes());
        let d = decode(&v);
        assert_eq!(d.text, "hi");
        assert_eq!(d.encoding, "utf-8");
    }

    #[test]
    fn utf16le_with_bom() {
        let mut v = vec![0xFF, 0xFE];
        for u in "中文".encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        let d = decode(&v);
        assert_eq!(d.text, "中文");
        assert_eq!(d.encoding, "utf-16le");
    }

    #[test]
    fn invalid_utf8_lossy_labeled() {
        // NB: 0xFF 0xFE would be read as a UTF-16LE BOM; use a genuinely
        // invalid UTF-8 sequence (0xC3 without continuation) instead.
        let d = decode(&[0xC3, b'(', b'a', 0xFF]);
        assert!(d.text.contains('a'));
        assert_eq!(d.encoding, "utf-8-lossy");
    }

    #[test]
    fn surrogate_pairs() {
        let mut v = vec![0xFF, 0xFE];
        // U+1F600 😀 = D83D DE00
        v.extend_from_slice(&0xD83Du16.to_le_bytes());
        v.extend_from_slice(&0xDE00u16.to_le_bytes());
        let d = decode(&v);
        assert_eq!(d.text, "😀");
    }

    #[test]
    fn bomless_utf16le_detected_by_nul_pattern() {
        // WSL launcher output: UTF-16LE without BOM, *valid* UTF-8 with NULs.
        let msg = "Windows Subsystem for Linux has no installed distributions";
        let mut bytes = Vec::new();
        for u in msg.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let d = decode(&bytes);
        assert_eq!(d.text, msg);
        assert_eq!(d.encoding, "utf-16le");
    }

    #[test]
    fn plain_utf8_with_single_nul_stays_utf8() {
        // `printf 'a\0b'` — odd length, not a UTF-16 stream.
        let d = decode(b"a\0b");
        assert_eq!(d.encoding, "utf-8");
        assert!(d.text.contains('\0'));
    }

    /// The headline regression: GBK Chinese used to come back as U+FFFD.
    #[test]
    fn gbk_chinese_decodes_automatically() {
        let d = decode(&GBK_NIHAO);
        assert_eq!(d.text, "你好");
        assert_eq!(d.encoding, "gbk");
    }

    /// Big5 and GBK overlap, so Big5 is *hint-only*: without a hint these bytes
    /// decode "successfully" as GBK mojibake (`いゅ`), which is exactly why the
    /// label must be read as provenance rather than truth.
    #[test]
    fn big5_requires_a_hint_and_says_so() {
        let big5 = [0xA4, 0xA4, 0xA4, 0xE5]; // 「中文」 in Big5
        assert_eq!(decode(&big5).encoding, "gbk", "auto path assumes GBK");

        let d = decode_with(&big5, Some("big5"));
        assert_eq!(d.text, "中文");
        assert_eq!(d.encoding, "big5");
    }

    /// A page that maps every byte, or that cannot map this input, must not be
    /// guessed at: without a hint the last resort stays `utf-8-lossy`.
    #[test]
    fn single_byte_pages_need_an_explicit_hint() {
        // 0x93 alone is an incomplete GBK lead byte, but a real CP1252 quote.
        let bytes = [0x93];
        assert_eq!(decode(&bytes).encoding, "utf-8-lossy");

        let hinted = decode_with(&bytes, Some("cp1252"));
        assert_eq!(hinted.text, "\u{201C}");
        assert_eq!(hinted.encoding, "windows-1252");
    }

    #[test]
    fn oem_pages_decode_through_the_hint() {
        // CP437: 0xDB is a full block, 0xB0 a light shade.
        let d = decode_with(&[0xDB, 0xB0], Some("cp437"));
        assert_eq!(d.text, "\u{2588}\u{2591}");
        assert_eq!(d.encoding, "cp437");

        // CP850 differs from CP437 in the high half: 0x9B is 'ø' (CP437: '¢').
        let d = decode_with(&[0x9B], Some("cp850"));
        assert_eq!(d.text, "ø");
        assert_eq!(d.encoding, "cp850");
        assert_eq!(decode_with(&[0x9B], Some("cp437")).text, "¢");
    }

    /// A hint wins over an accidental valid-UTF-8 reading (a project that
    /// declares `encoding = "gbk"` means it).
    #[test]
    fn hint_wins_over_the_utf8_fast_path() {
        // These two GBK bytes happen to also be valid UTF-8: 0xC3 0xA9 = 'é'.
        let bytes = [0xC3, 0xA9];
        assert_eq!(decode(&bytes).text, "é");
        let hinted = decode_with(&bytes, Some("gbk"));
        assert_eq!(hinted.encoding, "gbk");
        assert_ne!(hinted.text, "é");
    }

    /// ASCII is identical under every page, so it stays `utf-8` even when a
    /// project declares a legacy convention.
    #[test]
    fn ascii_stays_utf8_even_with_a_hint() {
        let d = decode_with(b"plain ascii", Some("gbk"));
        assert_eq!(d.text, "plain ascii");
        assert_eq!(d.encoding, "utf-8");
    }

    /// BOM evidence beats a hint: the bytes describe themselves.
    #[test]
    fn bom_beats_a_conflicting_hint() {
        let mut v = vec![0xFF, 0xFE];
        for u in "hi".encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        let d = decode_with(&v, Some("gbk"));
        assert_eq!(d.encoding, "utf-16le");
        assert_eq!(d.text, "hi");
    }

    /// `utf-8` is a real hint value: it switches the CP936 guess off.
    #[test]
    fn explicit_utf8_disables_the_gbk_guess() {
        assert_eq!(decode(&GBK_NIHAO).encoding, "gbk");
        let forced = decode_with(&GBK_NIHAO, Some("utf-8"));
        assert_eq!(forced.encoding, "utf-8-lossy");
    }

    #[test]
    fn code_page_names_are_forgiving() {
        assert_eq!(
            LegacyCodePage::from_name("CP936"),
            Some(LegacyCodePage::Gbk)
        );
        assert_eq!(
            LegacyCodePage::from_name("gb2312"),
            Some(LegacyCodePage::Gbk)
        );
        assert_eq!(
            LegacyCodePage::from_name("windows_1252"),
            Some(LegacyCodePage::Windows1252)
        );
        assert_eq!(
            LegacyCodePage::from_name("oem"),
            Some(LegacyCodePage::Cp437)
        );
        assert_eq!(LegacyCodePage::from_name("nonsense"), None);
    }
}

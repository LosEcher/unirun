//! Encoding pipeline: BOM detection, UTF-16 decode, UTF-8 fast path,
//! lossy fallback. The agent always receives valid Unicode text plus a
//! stable `encoding` label so it can tell "clean UTF-8" from "best-effort".

/// Result of decoding one byte stream.
pub struct Decoded {
    pub text: String,
    pub encoding: &'static str,
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

/// Decode raw child output to Unicode text.
///
/// Strategy:
/// 1. BOM sniffing (UTF-8 / UTF-16LE / UTF-16BE) — PowerShell 5.1 emits
///    UTF-16LE with BOM via `[Console]::OutputEncoding` in some configs.
/// 2. Valid UTF-8 fast path.
/// 3. Lossy fallback (`utf-8-lossy`): never fail, but label it so agents
///    can treat the text as best-effort (e.g. OEM/GBK mojibake on Windows).
pub fn decode(bytes: &[u8]) -> Decoded {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let rest = &bytes[3..];
        return match std::str::from_utf8(rest) {
            Ok(s) => Decoded {
                text: s.to_string(),
                encoding: "utf-8",
            },
            Err(_) => Decoded {
                text: String::from_utf8_lossy(rest).into_owned(),
                encoding: "utf-8-lossy",
            },
        };
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

fn decode_utf16(units: &[u8], little_endian: bool) -> String {
    let mut out = String::new();
    let mut chars = units.chunks_exact(2).map(|c| {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

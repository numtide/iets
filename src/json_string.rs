//! Quoted JSON tokens shared by fromJSON and the derivation reader.
#[derive(Clone, Copy)]
pub(crate) enum StringError {
    End,
    Backslash,
    Hex { start: usize },
    HighSurrogate,
    LowSurrogate,
    Control,
    Utf8,
}

impl StringError {
    pub(crate) fn reason(self, store: bool) -> &'static str {
        match self {
            Self::End => "missing closing quote",
            Self::Backslash => "forbidden character after backslash",
            Self::Hex { .. } => "'\\u' must be followed by 4 hex digits",
            Self::HighSurrogate if store => {
                "surrogate U+D800..U+DBFF must be followed by U+DC00..U+DFFF"
            }
            Self::HighSurrogate => "surrogate must be followed by a low surrogate",
            Self::LowSurrogate if store => "surrogate U+DC00..U+DFFF must follow U+D800..U+DBFF",
            Self::LowSurrogate => "low surrogate must follow a high surrogate",
            Self::Control => "control character must be escaped",
            Self::Utf8 => "ill-formed UTF-8 byte",
        }
    }
}

fn hex4(input: &[u8], cursor: &mut usize) -> Result<u32, StringError> {
    let start = *cursor;
    let mut value = 0;
    for _ in 0..4 {
        let digit = input
            .get(*cursor)
            .and_then(|&c| char::from(c).to_digit(16))
            .ok_or(StringError::Hex { start })?;
        value = value * 16 + digit;
        *cursor += 1;
    }
    Ok(value)
}

/// At entry the cursor points to the opening quote. On failure it records
/// the consumed byte count. A malformed hex digit is inspected but not consumed.
pub(crate) fn decode(input: &[u8], cursor: &mut usize) -> Result<String, StringError> {
    *cursor += 1;
    let mut out = String::new();
    loop {
        let c = *input.get(*cursor).ok_or(StringError::End)?;
        *cursor += 1;
        match c {
            b'"' => return Ok(out),
            b'\\' => {
                let e = *input.get(*cursor).ok_or(StringError::Backslash)?;
                *cursor += 1;
                out.push(match e {
                    b'"' | b'\\' | b'/' => char::from(e),
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'u' => {
                        let mut cp = hex4(input, cursor)?;
                        if (0xd800..0xdc00).contains(&cp) {
                            if !input[*cursor..].starts_with(b"\\u") {
                                return Err(StringError::HighSurrogate);
                            }
                            *cursor += 2;
                            let low = hex4(input, cursor)?;
                            if !(0xdc00..0xe000).contains(&low) {
                                return Err(StringError::HighSurrogate);
                            }
                            cp = 0x10000 + ((cp - 0xd800) << 10) + low - 0xdc00;
                        } else if (0xdc00..0xe000).contains(&cp) {
                            return Err(StringError::LowSurrogate);
                        }
                        char::from_u32(cp).expect("valid scalar")
                    }
                    _ => return Err(StringError::Backslash),
                });
            }
            0..=0x1f => return Err(StringError::Control),
            _ => {
                let start = *cursor - 1;
                let tail = &input[*cursor..];
                // Validate incrementally, so an early bad byte does not cause a
                // search through an arbitrary suffix. Finish a scalar crossing
                // the block boundary (at most three UTF-8 continuation bytes).
                let block = &tail[..tail.len().min(256)];
                let n = block
                    .iter()
                    .position(|&b| b < 0x20 || matches!(b, b'"' | b'\\'))
                    .unwrap_or_else(|| {
                        block.len()
                            + tail[block.len()..]
                                .iter()
                                .take(3)
                                .take_while(|b| **b & 0xc0 == 0x80)
                                .count()
                    });
                let end = *cursor + n;
                let text = str::from_utf8(&input[start..end]).map_err(|e| {
                    *cursor = start + e.valid_up_to() + 1;
                    StringError::Utf8
                })?;
                out.push_str(text);
                *cursor = end;
            }
        }
    }
}

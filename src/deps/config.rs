use anyhow::{bail, Result};
use serde_json::Value;

/// Replace JSONC comments and trailing commas with spaces so serde reports
/// errors at their original line and column. Quoted strings remain untouched.
pub(super) fn parse_jsonc(source: &[u8]) -> Result<Value> {
    let mut json = source.to_vec();
    let mut i = 0;
    if json.starts_with(&[0xef, 0xbb, 0xbf]) {
        json[..3].fill(b' ');
        i = 3;
    }
    let mut previous = None;
    let mut commas = Vec::new();
    while i < json.len() {
        match json[i] {
            b'"' => {
                i += 1;
                while i < json.len() {
                    match json[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                previous = Some(b'"');
            }
            b'/' if json.get(i + 1) == Some(&b'/') => {
                while i < json.len() && !matches!(json[i], b'\r' | b'\n') {
                    json[i] = b' ';
                    i += 1;
                }
            }
            b'/' if json.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                while i + 1 < json.len() && &json[i..i + 2] != b"*/" {
                    i += 1;
                }
                if i + 1 >= json.len() {
                    let prefix = &source[..start];
                    let line = prefix.iter().filter(|&&b| b == b'\n').count() + 1;
                    let column = prefix
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .map_or(start + 1, |newline| start - newline);
                    bail!("unterminated block comment at line {line} column {column}");
                }
                i += 2;
                for byte in &mut json[start..i] {
                    if !matches!(*byte, b'\r' | b'\n') {
                        *byte = b' ';
                    }
                }
            }
            byte if byte.is_ascii_whitespace() => i += 1,
            byte => {
                if byte == b',' && !matches!(previous, None | Some(b'[' | b'{' | b':' | b',')) {
                    commas.push(i);
                }
                previous = Some(byte);
                i += 1;
            }
        }
    }
    for comma in commas {
        if matches!(
            json[comma + 1..].iter().find(|b| !b.is_ascii_whitespace()),
            Some(b']' | b'}')
        ) {
            json[comma] = b' ';
        }
    }
    Ok(serde_json::from_slice(&json)?)
}

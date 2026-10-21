use super::{
    config::{join, literal_component as component},
    text::Source,
    *,
};

pub(super) fn extract(language: &str, src: &[u8]) -> Extraction {
    let text = String::from_utf8_lossy(src);
    if src.len() > 1_048_576 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let source = Source::new(&text);
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let mut facts: Vec<Fact> = Vec::new();
    let mut spans = Vec::new();
    let mut section = String::new();
    let mut section_index: Option<usize> = None;
    let mut offset = 0;
    let mut i = 0;
    while i < lines.len() {
        if facts.len() > 10_000 {
            return (Vec::new(), Vec::new(), Vec::new());
        }
        let start = offset;
        let raw = lines[i].trim_end_matches(['\r', '\n']);
        offset += lines[i].len();
        i += 1;
        let mut logical = raw.to_string();
        let mut positions: Vec<usize> = (start..start + raw.len()).collect();
        if language == "properties" && !raw.trim_start().starts_with(['#', '!']) {
            while logical.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
                && i < lines.len()
            {
                logical.pop();
                positions.pop();
                let next = lines[i].trim_end_matches(['\r', '\n']);
                let content = next.trim_start_matches([' ', '\t', '\u{c}']);
                let leading = next.len() - content.len();
                logical.push_str(content);
                positions.extend(offset + leading..offset + next.len());
                offset += lines[i].len();
                i += 1;
            }
        }
        let raw = logical.as_str();
        let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#')
            || (language == "ini" && trimmed.starts_with(';'))
            || (language == "properties" && trimmed.starts_with('!'))
        {
            spans.push((start, offset, Span::Comment));
            continue;
        }
        if language == "ini" && trimmed.starts_with('[') {
            if let Some(end) = trimmed.find(']') {
                let suffix = trimmed[end + 1..].trim();
                if !suffix.is_empty() && !suffix.starts_with([';', '#']) {
                    continue;
                }
                if let Some(index) = section_index {
                    facts[index].end_byte = start;
                    facts[index].end_line = source
                        .fact("", "", start, start, "")
                        .start_line
                        .saturating_sub(1)
                        .max(facts[index].start_line);
                    facts[index].end_col = 0;
                }
                section = trimmed[1..end].trim().into();
                if !section.is_empty() {
                    section_index = Some(facts.len());
                    facts.push(source.fact(
                        "key",
                        &component(&section),
                        start,
                        text.len(),
                        "lines/ini",
                    ));
                }
            }
            continue;
        }
        let key_line = if language == "dotenv" {
            trimmed
                .strip_prefix("export ")
                .unwrap_or(trimmed)
                .trim_start()
        } else {
            trimmed
        };
        let separator = if language == "properties" {
            let mut escaped = false;
            key_line.char_indices().find_map(|(n, c)| {
                if escaped {
                    escaped = false;
                    return None;
                }
                if c == '\\' {
                    escaped = true;
                    return None;
                }
                (c == '=' || c == ':' || c.is_ascii_whitespace()).then_some(n)
            })
        } else {
            key_line.find(if language == "dotenv" {
                &['='][..]
            } else {
                &['=', ':'][..]
            })
        };
        let Some(separator) =
            separator.or_else(|| (language == "properties").then_some(key_line.len()))
        else {
            continue;
        };
        let raw_key = &key_line[..separator];
        let raw_key = if language == "properties" {
            raw_key
        } else {
            raw_key.trim_end()
        };
        if raw_key.is_empty() {
            continue;
        }
        if language == "dotenv"
            && !raw_key
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
        {
            continue;
        }
        let key = if language == "properties" {
            unescape(raw_key)
        } else {
            raw_key.to_string()
        };
        let parent = if section.is_empty() {
            String::new()
        } else {
            component(&section)
        };
        let name = join(&parent, &component(&key));
        let value = key_line[separator..]
            .trim_start_matches(|c: char| c == '=' || c == ':' || c.is_ascii_whitespace());
        let value_start = positions
            .get(raw.len() - value.len())
            .copied()
            .unwrap_or(offset);
        if language == "dotenv" && value.starts_with(['\'', '"']) {
            let quote = value.as_bytes()[0];
            let mut current = &value[1..];
            while !closing_quote(current, quote) && i < lines.len() {
                current = lines[i];
                offset += lines[i].len();
                i += 1;
            }
        } else if language == "ini" {
            let indent = raw.len() - raw.trim_start().len();
            while i < lines.len()
                && lines[i].len() - lines[i].trim_start().len() > indent
                && !lines[i].trim().is_empty()
                && !lines[i].trim_start().starts_with(['#', ';'])
            {
                offset += lines[i].len();
                i += 1;
            }
        }
        let mut fact = source.fact("key", &name, start, offset, &format!("lines/{language}"));
        fact.owner = (!section.is_empty()).then(|| component(&section));
        facts.push(fact);
        if value_start < offset {
            spans.push((value_start, offset, Span::Str));
        }
        if facts.len() > 10_000 {
            return (Vec::new(), Vec::new(), Vec::new());
        }
    }
    if facts.len() > 10_000 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    (facts, spans, Vec::new())
}

fn closing_quote(text: &str, quote: u8) -> bool {
    let mut escaped = false;
    for b in text.bytes() {
        if escaped {
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == quote {
            return true;
        }
    }
    false
}

fn unescape(text: &str) -> String {
    let mut chars = text.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(c) = chars.next() else {
            out.push('\\');
            break;
        };
        match c {
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            'f' => out.push('\u{c}'),
            'u' => {
                let digits: String = chars.by_ref().take(4).collect();
                if let Some(c) = u32::from_str_radix(&digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    out.push(c);
                } else {
                    out.push_str("\\u");
                    out.push_str(&digits);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

use super::{text::Source, *};

pub(super) fn extract(language: &str, src: &[u8]) -> Extraction {
    if src.len() > 1_048_576 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let text = String::from_utf8_lossy(src);
    let source = Source::new(&text);
    let mut facts = Vec::new();
    let mut headings: Vec<(usize, usize)> = Vec::new();
    let mut block: Option<(String, Option<(String, usize)>)> = None;
    let mut pending: Option<(String, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let raw = line.trim_end_matches(['\r', '\n']);
        let lower = raw.to_ascii_lowercase();
        if let Some((closing, _)) = &block {
            if (language == "org" && lower.trim() == closing)
                || (language == "asciidoc" && raw == closing)
            {
                if let Some((_, Some((name, begin)))) = block.take() {
                    facts.push(source.fact("block", &name, begin, offset, language));
                }
            }
            continue;
        }
        if language == "org" {
            if raw.trim_start().starts_with("# ") || raw.trim() == "#" {
                continue;
            }
            if let Some(kind) = lower
                .trim_start()
                .strip_prefix("#+begin_")
                .and_then(|s| s.split_whitespace().next())
            {
                block = Some((format!("#+end_{kind}"), pending.take()));
                continue;
            }
            if let Some(name) = raw
                .get(..7)
                .filter(|s| s.eq_ignore_ascii_case("#+name:"))
                .map(|_| raw[7..].trim())
            {
                pending = (!name.is_empty()).then(|| (name.into(), start));
                continue;
            }
            if lower.starts_with("#+include:") || lower.starts_with("#+setupfile:") {
                if let Some(rest) = raw.split_once(':').map(|(_, s)| s.trim()) {
                    let target = if let Some(rest) = rest.strip_prefix('"') {
                        rest.split('"').next().unwrap_or("")
                    } else {
                        rest.split_whitespace().next().unwrap_or("")
                    };
                    if let Some(f) = source.import(target, start, offset, language) {
                        facts.push(f);
                    }
                }
            }
            let mut at = 0;
            while let Some(pos) = raw[at..].find("[[file:") {
                let begin = at + pos + 7;
                let Some(end) = raw[begin..].find(']') else {
                    break;
                };
                let target = raw[begin..begin + end].split("::").next().unwrap_or("");
                if let Some(f) = source.import(target, start + begin, start + begin + end, language)
                {
                    facts.push(f);
                }
                at = begin + end + 1;
            }
        } else {
            if raw.starts_with("//") && !raw.starts_with("////") {
                continue;
            }
            if let Some(name) = raw.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
                pending = Some((name.split(',').next().unwrap_or(name).into(), start));
                continue;
            }
            if raw.len() >= 4
                && raw.bytes().all(|c| c == raw.as_bytes()[0])
                && matches!(raw.as_bytes()[0], b'-' | b'.' | b'+' | b'/')
            {
                block = Some((raw.into(), pending.take()));
                continue;
            }
            if let Some(rest) = raw.strip_prefix("include::") {
                if let Some((target, _)) = rest.split_once('[') {
                    if let Some(f) = source.import(target, start, offset, language) {
                        facts.push(f);
                    }
                }
            }
        }
        let marker = if language == "org" { b'*' } else { b'=' };
        let level = raw.bytes().take_while(|&b| b == marker).count();
        if level > 0 && raw.as_bytes().get(level) == Some(&b' ') {
            let name = raw[level..].trim();
            if !name.is_empty() {
                while headings.last().is_some_and(|(old, _)| *old >= level) {
                    let (_, index) = headings.pop().unwrap();
                    let f: &mut Fact = &mut facts[index];
                    let ended = source.fact("heading", &f.name, f.start_byte, start, language);
                    f.end_byte = ended.end_byte;
                    f.end_line = ended.end_line;
                    f.end_col = ended.end_col;
                }
                headings.push((level, facts.len()));
                let begin = pending.take().map(|(_, begin)| begin).unwrap_or(start);
                facts.push(source.fact("heading", name, begin, text.len(), language));
            }
        } else if !raw.starts_with('[') && !raw.is_empty() {
            pending = None;
        }
        if facts.len() > 10_000 {
            return (Vec::new(), Vec::new(), Vec::new());
        }
    }
    if let Some((_, Some((name, start)))) = block {
        facts.push(source.fact("block", &name, start, text.len(), language));
    }
    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    if facts.len() > 10_000 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    (facts, Vec::new(), Vec::new())
}

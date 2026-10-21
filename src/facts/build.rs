use super::{text::Source, *};
use std::collections::{HashSet, VecDeque};

pub(super) fn extract(language: &str, src: &[u8]) -> Extraction {
    if src.len() > 1_048_576 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let text = String::from_utf8_lossy(src);
    let source = Source::new(&text);
    let mut facts: Vec<Fact> = Vec::new();
    let mut relations = Vec::new();
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let mut offset = 0;
    let mut i = 0;
    let mut stages = Vec::<String>::new();
    let mut stage: Option<usize> = None;
    let mut rules: Vec<usize> = Vec::new();
    let mut macro_depth = 0usize;
    let mut macro_index = None;
    let mut heredocs = VecDeque::<(String, bool)>::new();
    let mut escape = '\\';
    let mut recipe_prefix = '\t';
    while i < lines.len() {
        if facts.len() > 10_000 || relations.len() > 10_000 {
            return (Vec::new(), Vec::new(), Vec::new());
        }
        let start = offset;
        let physical = lines[i].trim_end_matches(['\r', '\n']);
        offset += lines[i].len();
        i += 1;
        if let Some((delimiter, tabs)) = heredocs.front() {
            if (if *tabs {
                physical.trim_start_matches('\t')
            } else {
                physical
            }) == delimiter
            {
                heredocs.pop_front();
            }
            continue;
        }
        if language == "make" && physical.starts_with(recipe_prefix) {
            let mut continued = physical.ends_with('\\');
            while continued && i < lines.len() {
                continued = lines[i].trim_end_matches(['\r', '\n']).ends_with('\\');
                offset += lines[i].len();
                i += 1;
            }
            continue;
        }
        if language == "dockerfile" && physical.trim_start().starts_with('#') {
            if let Some(value) = physical
                .trim()
                .strip_prefix("# escape=")
                .and_then(|s| s.chars().next())
            {
                if matches!(value, '\\' | '`') {
                    escape = value;
                }
            }
            continue;
        }
        let mut logical = physical.to_string();
        while logical.ends_with(escape) && i < lines.len() {
            logical.pop();
            logical.push(' ');
            logical.push_str(lines[i].trim());
            offset += lines[i].len();
            i += 1;
        }
        let line = logical.trim();
        if line.starts_with('#') {
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if language == "dockerfile" {
            let (instruction, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();
            let parsed = shell_words::split(rest).unwrap_or_default();
            let words: Vec<_> = parsed.iter().map(String::as_str).collect();
            match instruction.to_ascii_uppercase().as_str() {
                "FROM" => {
                    if let Some(index) = stage {
                        finish(&source, &mut facts[index], start, language);
                    }
                    let image = words
                        .iter()
                        .find(|s| !s.starts_with("--"))
                        .copied()
                        .unwrap_or("");
                    let name = words
                        .windows(2)
                        .find(|w| w[0].eq_ignore_ascii_case("as"))
                        .map(|w| w[1].to_string())
                        .unwrap_or_else(|| stages.len().to_string());
                    if let Some(parent) = stages.iter().find(|s| s.eq_ignore_ascii_case(image)) {
                        relations.push(Relation {
                            subtype: name.clone(),
                            supertype: parent.clone(),
                            relation: "depends-on",
                            start_line: source.line(start),
                        });
                    }
                    stage = Some(facts.len());
                    stages.push(name.clone());
                    let mut fact = source.fact("stage", &name, start, text.len(), language);
                    fact.signature = cap(line, MAX_SIG);
                    facts.push(fact);
                }
                "ARG" | "ENV" => {
                    let keys: Vec<_> = if instruction.eq_ignore_ascii_case("ARG") {
                        words
                            .first()
                            .map(|s| s.split('=').next().unwrap_or(""))
                            .into_iter()
                            .collect()
                    } else if words.first().is_some_and(|s| !s.contains('=')) {
                        words.first().copied().into_iter().collect()
                    } else {
                        words
                            .iter()
                            .filter_map(|s| s.split_once('=').map(|(key, _)| key))
                            .collect()
                    };
                    for key in keys {
                        if key.is_empty()
                            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        {
                            continue;
                        }
                        let name = stage
                            .map(|index| format!("{}.{}", facts[index].name, key))
                            .unwrap_or_else(|| key.to_string());
                        let mut fact = source.fact("key", &name, start, offset, language);
                        fact.owner = stage.map(|index| facts[index].name.clone());
                        facts.push(fact);
                    }
                }
                "COPY" | "RUN" => {
                    if let Some(current) = stage {
                        for word in words.iter().take_while(|word| word.starts_with("--")) {
                            let target = (instruction
                                .eq_ignore_ascii_case("COPY")
                                .then(|| word.strip_prefix("--from="))
                                .flatten())
                            .or_else(|| {
                                word.strip_prefix("--mount=").and_then(|s| {
                                    s.split(',').find_map(|p| p.strip_prefix("from="))
                                })
                            });
                            if let Some(target) = target {
                                let parent = target
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|n| stages.get(n))
                                    .or_else(|| {
                                        stages.iter().find(|s| s.eq_ignore_ascii_case(target))
                                    });
                                if let Some(parent) = parent.filter(|s| *s != &facts[current].name)
                                {
                                    relations.push(Relation {
                                        subtype: facts[current].name.clone(),
                                        supertype: parent.clone(),
                                        relation: "depends-on",
                                        start_line: source.line(start),
                                    });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            if matches!(
                instruction.to_ascii_uppercase().as_str(),
                "RUN" | "COPY" | "ADD"
            ) && !rest.starts_with('[')
            {
                heredocs.extend(heredoc_delimiters(rest));
            }
        } else {
            let mut directive = line;
            while let Some(rest) = directive
                .strip_prefix("export ")
                .or_else(|| directive.strip_prefix("override "))
                .or_else(|| directive.strip_prefix("private "))
            {
                directive = rest.trim_start();
            }
            if directive.split('#').next().unwrap_or("").trim() == "endef" {
                macro_depth = macro_depth.saturating_sub(1);
                if macro_depth == 0 {
                    if let Some(index) = macro_index.take() {
                        finish(&source, &mut facts[index], offset, language);
                    }
                }
                continue;
            }
            if directive.starts_with("define ") {
                if macro_depth == 0 {
                    for index in rules.drain(..) {
                        finish(&source, &mut facts[index], start, language);
                    }
                }
                macro_depth += 1;
                if macro_depth == 1 {
                    let name = directive[7..].split_whitespace().next().unwrap_or("");
                    macro_index = Some(facts.len());
                    facts.push(source.fact("key", name, start, text.len(), language));
                }
                continue;
            }
            if macro_depth > 0 {
                continue;
            }
            for index in rules.drain(..) {
                finish(&source, &mut facts[index], start, language);
            }
            let words: Vec<_> = line.split_whitespace().collect();
            if matches!(
                words.first().copied(),
                Some("include" | "-include" | "sinclude")
            ) {
                for target in words.iter().skip(1).take_while(|s| !s.starts_with('#')) {
                    if literal(target) {
                        if let Some(f) = source.import(target, start, offset, language) {
                            facts.push(f);
                        }
                    }
                }
                continue;
            }
            let line = line
                .strip_prefix("export ")
                .or_else(|| line.strip_prefix("override "))
                .unwrap_or(line);
            let assignment = line.find('=');
            let colon = line.find(':');
            if let Some(eq) = assignment.filter(|eq| {
                colon.is_none_or(|colon| colon >= *eq || line[colon..*eq].chars().all(|c| c == ':'))
            }) {
                let name = line[..eq]
                    .trim()
                    .trim_end_matches([':', '?', '+', '!'])
                    .trim_end();
                if literal(name) && !name.contains(char::is_whitespace) {
                    facts.push(source.fact("key", name, start, offset, language));
                    if name == ".RECIPEPREFIX" {
                        recipe_prefix = line[eq + 1..].trim_start().chars().next().unwrap_or('\t');
                    }
                }
                continue;
            }
            let Some((targets, rest)) = line.split_once(':') else {
                continue;
            };
            // Target-specific assignments and static-pattern rules need expansion.
            if rest.contains('=') || rest.trim_start_matches(':').contains(':') {
                continue;
            }
            let prerequisites = rest
                .trim_start_matches(':')
                .split(';')
                .next()
                .unwrap_or("")
                .split('#')
                .next()
                .unwrap_or("");
            for target in targets.split_whitespace().filter(|s| literal(s)) {
                if matches!(
                    target,
                    ".PHONY"
                        | ".SUFFIXES"
                        | ".DEFAULT"
                        | ".PRECIOUS"
                        | ".INTERMEDIATE"
                        | ".SECONDARY"
                        | ".SECONDEXPANSION"
                        | ".DELETE_ON_ERROR"
                        | ".IGNORE"
                        | ".LOW_RESOLUTION_TIME"
                        | ".SILENT"
                        | ".EXPORT_ALL_VARIABLES"
                        | ".NOTPARALLEL"
                        | ".ONESHELL"
                        | ".POSIX"
                        | ".WAIT"
                ) {
                    continue;
                }
                rules.push(facts.len());
                let mut fact = source.fact("target", target, start, text.len(), language);
                fact.signature = cap(line, MAX_SIG);
                facts.push(fact);
                for dependency in prerequisites
                    .split_whitespace()
                    .filter(|s| literal(s) && *s != "|")
                {
                    relations.push(Relation {
                        subtype: target.into(),
                        supertype: dependency.into(),
                        relation: "depends-on",
                        start_line: source.line(start),
                    });
                }
            }
        }
        if facts.len() > 10_000 || relations.len() > 10_000 {
            return (Vec::new(), Vec::new(), Vec::new());
        }
    }
    if language == "make" {
        let targets: HashSet<_> = facts
            .iter()
            .filter(|f| f.kind == "target")
            .map(|f| f.name.as_str())
            .collect();
        relations.retain(|r| targets.contains(r.supertype.as_str()));
    }
    if facts.len() > 10_000 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    (facts, Vec::new(), relations)
}

fn literal(text: &str) -> bool {
    !text.is_empty() && !text.contains(['$', '%', '*', '?', '[', ']', '{', '}', '\\'])
}

fn finish(source: &Source<'_>, fact: &mut Fact, end: usize, language: &str) {
    let ended = source.fact(&fact.kind, &fact.name, fact.start_byte, end, language);
    fact.end_byte = ended.end_byte;
    fact.end_line = ended.end_line;
    fact.end_col = ended.end_col;
}

fn heredoc_delimiters(text: &str) -> Vec<(String, bool)> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut quote = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && quote != Some(b'\'') {
            i += 2;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if matches!(b, b'\'' | b'"') {
            quote = Some(b);
            i += 1;
            continue;
        }
        if b != b'<' || bytes.get(i + 1) != Some(&b'<') {
            i += 1;
            continue;
        }
        i += 2;
        let tabs = bytes.get(i) == Some(&b'-');
        if tabs {
            i += 1;
        }
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        let start = i;
        let mut word_quote = None;
        while let Some(&b) = bytes.get(i) {
            if let Some(q) = word_quote {
                if b == q {
                    word_quote = None;
                }
            } else if matches!(b, b'\'' | b'"') {
                word_quote = Some(b);
            } else if b.is_ascii_whitespace() || matches!(b, b';' | b'<' | b'>' | b'&' | b'|') {
                break;
            }
            i += 1;
        }
        if let Ok(words) = shell_words::split(&text[start..i]) {
            if let Some(word) = words.into_iter().next().filter(|s| !s.is_empty()) {
                result.push((word, tabs));
            }
        }
    }
    result
}

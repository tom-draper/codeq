//! Line-wise extractors for documentation formats: Markdown, reStructuredText
//! and Jinja templates. None uses a Tree-sitter grammar; structure comes from
//! scanning lines, so they live apart from the grammar-driven extractors.

use super::*;

// --- Markdown ----------------------------------------------------------------

/// Markdown structure, parsed line-wise rather than with a grammar.
///
/// Docs are the first thing read in an unfamiliar repo and the last thing codeq
/// knew anything about. Treating a **heading as a definition** is the whole
/// feature: a heading has a name, a byte range (down to the next heading of the
/// same or higher level) and a nesting depth, which is exactly the fact model
/// every other language produces. `outline`, `at`, `read --symbol`, the folded
/// `read`, and the enclosing-`[symbol]` annotation on search hits all then work
/// on prose for free.
///
/// Deliberately not Tree-sitter: headings are unambiguous line-wise as long as
/// fenced blocks are tracked (so a `# comment` inside a shell example is not a
/// heading), and this avoids a grammar dependency for the one format where the
/// structure is trivial. `source` records which was used.
///
/// Also emitted:
/// - **Spans.** A fenced code block is `Str`, prose is `Comment`, so
///   `--in strings` searches the examples and `--in comments` searches the prose.
///   Markdown has no "code" in the `--in code` sense, so that filter is
///   correctly empty here rather than quietly matching everything.
/// - **Imports.** A link or image with a local target (`](./x.md)`,
///   `![](assets/logo.png)`, and reference definitions `[l]: ./x.md`) becomes an
///   import, so `deps`, `importers` and cycle detection cover documentation and
///   its assets. External URLs and bare `#anchor` links are not file references
///   and are skipped.
pub(super) fn markdown_facts(src: &[u8]) -> Extraction {
    let text = String::from_utf8_lossy(src);
    let mut facts: Vec<Fact> = Vec::new();
    let mut spans: Vec<(usize, usize, Span)> = Vec::new();

    // (line_index, byte_start, byte_end_excluding_newline, text)
    let mut lines: Vec<(usize, usize, &str)> = Vec::new();
    let mut off = 0usize;
    for l in text.split_inclusive('\n') {
        let trimmed = l.strip_suffix('\n').unwrap_or(l);
        let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
        lines.push((off, off + trimmed.len(), trimmed));
        off += l.len();
    }

    // A heading found in the scan: (line index, level, name, byte start).
    struct Head {
        line: usize,
        level: usize,
        name: String,
        start: usize,
    }
    let mut heads: Vec<Head> = Vec::new();

    // Prose/code classification, accumulated as runs and merged at the end.
    let mut classes: Vec<Option<Span>> = vec![None; lines.len()];

    let mut fence: Option<(char, usize)> = None; // (char, run length)
    let mut in_frontmatter = false;

    for (i, (start, end, raw)) in lines.iter().enumerate() {
        let t = raw.trim_start();
        let indent = raw.len() - t.len();

        // YAML/TOML frontmatter at the very top is metadata, not prose or
        // headings; its `---` must not be read as a setext underline.
        if i == 0 && (t == "---" || t == "+++") {
            in_frontmatter = true;
            classes[i] = Some(Span::Str);
            continue;
        }
        if in_frontmatter {
            classes[i] = Some(Span::Str);
            if t == "---" || t == "+++" {
                in_frontmatter = false;
            }
            continue;
        }

        // Fenced code blocks. A closing fence is the same character, at least as
        // long as the opener, and carries no info string.
        let fence_run = |s: &str, c: char| s.chars().take_while(|&x| x == c).count();
        if let Some((fc, flen)) = fence {
            classes[i] = Some(Span::Str);
            let run = fence_run(t, fc);
            if run >= flen && t[run..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if indent < 4 && (t.starts_with("```") || t.starts_with("~~~")) {
            let fc = t.chars().next().unwrap();
            fence = Some((fc, fence_run(t, fc)));
            classes[i] = Some(Span::Str);
            continue;
        }

        classes[i] = Some(Span::Comment); // ordinary prose

        // ATX heading: up to 3 spaces of indent, 1-6 `#`, then a space.
        if indent < 4 && t.starts_with('#') {
            let level = t.chars().take_while(|&c| c == '#').count();
            let rest = &t[level..];
            if (1..=6).contains(&level) && (rest.is_empty() || rest.starts_with(' ')) {
                // A trailing run of `#` is decoration, not part of the name.
                let name = rest.trim().trim_end_matches('#').trim().to_string();
                if !name.is_empty() {
                    heads.push(Head {
                        line: i,
                        level,
                        name,
                        start: *start,
                    });
                }
                continue;
            }
        }

        // Setext underline: `===` or `---` directly under paragraph text. The
        // preceding line must be ordinary prose, otherwise `---` is a thematic
        // break (or a list marker), not a heading.
        if indent < 4
            && !t.is_empty()
            && (t.chars().all(|c| c == '=') || t.chars().all(|c| c == '-'))
        {
            if let Some(prev) = i.checked_sub(1) {
                let prev_raw = lines[prev].2.trim();
                let prev_is_prose = !prev_raw.is_empty()
                    && classes[prev] == Some(Span::Comment)
                    && !heads.iter().any(|h| h.line == prev)
                    && !prev_raw.starts_with(['-', '*', '+', '>', '#']);
                if prev_is_prose {
                    let level = if t.starts_with('=') { 1 } else { 2 };
                    heads.push(Head {
                        line: prev,
                        level,
                        name: prev_raw.to_string(),
                        start: lines[prev].0,
                    });
                    continue;
                }
            }
        }

        // File references: inline links/images and reference definitions.
        collect_md_links(raw, *start, i + 1, &mut facts);
        let _ = end;
    }

    // A heading owns everything down to the next heading of the same or higher
    // level, which is what makes containment (and therefore outline nesting)
    // work without tracking a stack.
    let eof = text.len();
    for (idx, h) in heads.iter().enumerate() {
        let end_byte = heads[idx + 1..]
            .iter()
            .find(|o| o.level <= h.level)
            .map(|o| o.start)
            .unwrap_or(eof);
        let end_line = heads[idx + 1..]
            .iter()
            .find(|o| o.level <= h.level)
            .map(|o| o.line)
            .unwrap_or(lines.len());
        // The section's first prose line doubles as its summary, the way a
        // docstring does for a function.
        let doc = lines[(h.line + 1).min(lines.len())..end_line]
            .iter()
            .map(|(_, _, t)| t.trim())
            .find(|t| !t.is_empty())
            .map(|t| cap(&collapse_ws(t), MAX_DOC));
        facts.push(Fact {
            kind: "heading".to_string(),
            is_derived: false,
            name: h.name.clone(),
            start_byte: h.start,
            end_byte: end_byte.max(h.start),
            start_line: h.line + 1,
            start_col: 0,
            end_line: end_line.max(h.line + 1),
            end_col: 0,
            signature: cap(&collapse_ws(lines[h.line].2.trim()), MAX_SIG),
            doc,
            source: "lines/markdown".to_string(),
            confidence: 1.0,
            visibility: None,
            owner: None,
            shape: None,
            shape_nodes: 0,
            is_test: false,
            decorator: None,
            contract: None,
        });
    }

    // Merge the per-line classes into runs.
    let mut i = 0usize;
    while i < lines.len() {
        let c = classes[i];
        if c.is_none() {
            i += 1;
            continue;
        }
        let start = lines[i].0;
        let mut j = i;
        while j + 1 < lines.len() && classes[j + 1] == c {
            j += 1;
        }
        let end = if j + 1 < lines.len() {
            lines[j + 1].0
        } else {
            eof
        };
        spans.push((start, end, c.unwrap()));
        i = j + 1;
    }

    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    (facts, spans, Vec::new())
}

/// Local file references on one markdown line: inline `[t](path)` / `![a](path)`
/// and reference definitions `[label]: path`. External URLs and pure `#anchor`
/// links name no file, so they carry no edge.
///
/// Anything inside an inline code span is an *example* of a link, not a link:
/// documentation about markdown is full of `` `[t](./guide.md)` `` that must not
/// become a dependency edge (or a broken one). Found by dogfooding, when
/// `deps --external` reported this repo's own README as having two dead links.
fn collect_md_links(raw: &str, line_start: usize, line_no: usize, facts: &mut Vec<Fact>) {
    let mut push = |target: &str, at: usize| {
        let t = target.trim();
        // Strip an optional title: `(path "Title")`.
        let t = t.split_whitespace().next().unwrap_or(t);
        let t = t.trim_matches(|c| c == '<' || c == '>');
        // Drop any anchor, keeping the file part (`./x.md#section` -> `./x.md`).
        let file = t.split('#').next().unwrap_or(t);
        if file.is_empty() || is_external_url(t) {
            return;
        }
        facts.push(Fact {
            kind: "import".to_string(),
            is_derived: false,
            name: file.to_string(),
            start_byte: line_start + at,
            end_byte: line_start + at + target.len(),
            start_line: line_no,
            start_col: at,
            end_line: line_no,
            end_col: at + target.len(),
            signature: String::new(),
            doc: None,
            source: "lines/markdown".to_string(),
            confidence: 1.0,
            visibility: None,
            owner: None,
            shape: None,
            shape_nodes: 0,
            is_test: false,
            decorator: None,
            contract: None,
        });
    };

    // Byte ranges covered by inline code spans on this line.
    let code_spans = inline_code_spans(raw);
    let in_code = |at: usize| code_spans.iter().any(|(s, e)| at >= *s && at < *e);

    let bytes = raw.as_bytes();
    // Reference definition: `[label]: target`.
    if let Some(rest) = raw.trim_start().strip_prefix('[') {
        if let Some((_, after)) = rest.split_once("]:") {
            let at = raw.len() - after.len();
            if !in_code(at) {
                push(after, at);
            }
            return;
        }
    }
    // Inline `](target)`, which covers both links and images.
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            if let Some(close) = raw[i + 2..].find(')') {
                if !in_code(i) {
                    push(&raw[i + 2..i + 2 + close], i + 2);
                }
                i += 2 + close;
                continue;
            }
        }
        i += 1;
    }
}

/// Byte ranges of inline code spans (`` `x` ``, ``` ``x`` ```) on one line.
fn inline_code_spans(raw: &str) -> Vec<(usize, usize)> {
    let bytes = raw.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
        let open = i;
        let mut j = i + ticks;
        // The closing run must be the same length, as in CommonMark.
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let run = bytes[j..].iter().take_while(|&&b| b == b'`').count();
                if run == ticks {
                    spans.push((open, j + run));
                    break;
                }
                j += run;
            } else {
                j += 1;
            }
        }
        i = if j < bytes.len() {
            j + ticks
        } else {
            bytes.len()
        };
    }
    spans
}

fn is_external_url(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("ftp://")
        || lower.starts_with("//")
        || t.starts_with('#')
}

// --- reStructuredText -------------------------------------------------------

/// RST's structure is simple enough to extract line-wise, as Markdown's is:
/// a heading is text immediately followed by a repeated adornment character.
/// The first appearance of an adornment determines its nesting level, which is
/// the convention Docutils uses in practice. Local document roles and include,
/// image, and hyperlink-target directives become imports so Flask's docs take
/// part in `outline`, `read`, and `deps` instead of being opaque assets.
pub(super) fn rst_facts(src: &[u8]) -> Extraction {
    let text = String::from_utf8_lossy(src);
    let mut lines: Vec<(usize, usize, &str)> = Vec::new();
    let mut off = 0usize;
    for line in text.split_inclusive('\n') {
        let raw = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or(line);
        lines.push((off, off + raw.len(), raw));
        off += line.len();
    }
    struct Head {
        line: usize,
        level: usize,
        name: String,
        start: usize,
    }
    let mut heads = Vec::<Head>::new();
    let mut adornments = Vec::<char>::new();
    let mut facts = Vec::new();
    let mut toctree_indent: Option<usize> = None;
    for (i, (start, _, raw)) in lines.iter().enumerate() {
        let t = raw.trim();
        let indent = raw.len() - raw.trim_start().len();
        if let Some(base_indent) = toctree_indent {
            if t.is_empty() || t.starts_with(':') {
                // Blank lines and directive options belong to the toctree.
            } else if indent > base_indent {
                if let Some(module) = rst_import_target(t, true) {
                    facts.push(line_fact(
                        "import",
                        &module,
                        LineSpan {
                            start_byte: *start + indent,
                            end_byte: *start + indent + t.len(),
                            start_line: i + 1,
                            end_line: i + 1,
                        },
                        String::new(),
                        "lines/rst",
                    ));
                }
                continue;
            } else {
                toctree_indent = None;
            }
        }
        if t.starts_with(".. toctree::") {
            toctree_indent = Some(indent);
        }
        if i > 0 && is_rst_adornment(t) {
            let prev = lines[i - 1].2.trim();
            if !prev.is_empty() && !is_rst_adornment(prev) {
                let marker = t.chars().next().unwrap();
                let level = match adornments.iter().position(|c| *c == marker) {
                    Some(n) => n + 1,
                    None => {
                        adornments.push(marker);
                        adornments.len()
                    }
                };
                heads.push(Head {
                    line: i - 1,
                    level,
                    name: prev.to_string(),
                    start: lines[i - 1].0,
                });
            }
        }
        collect_rst_links(raw, *start, i + 1, &mut facts);
    }
    let eof = text.len();
    for (i, head) in heads.iter().enumerate() {
        let next = heads[i + 1..]
            .iter()
            .find(|other| other.level <= head.level);
        let end_byte = next.map(|h| h.start).unwrap_or(eof);
        let end_line = next.map(|h| h.line + 1).unwrap_or(lines.len());
        facts.push(line_fact(
            "heading",
            &head.name,
            LineSpan {
                start_byte: head.start,
                end_byte,
                start_line: head.line + 1,
                end_line,
            },
            cap(&collapse_ws(lines[head.line].2), MAX_SIG),
            "lines/rst",
        ));
    }
    let spans = if src.is_empty() {
        Vec::new()
    } else {
        vec![(0, src.len(), Span::Comment)]
    };
    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    (facts, spans, Vec::new())
}

fn is_rst_adornment(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.len() >= 3
        && matches!(
            first,
            '=' | '-' | '~' | '^' | '"' | '\'' | '`' | ':' | '.' | '*' | '+' | '#'
        )
        && chars.all(|c| c == first)
}

fn collect_rst_links(raw: &str, line_start: usize, line_no: usize, facts: &mut Vec<Fact>) {
    let mut push = |target: &str, at: usize, doc_role: bool| {
        let Some(module) = rst_import_target(target, doc_role) else {
            return;
        };
        facts.push(line_fact(
            "import",
            &module,
            LineSpan {
                start_byte: line_start + at,
                end_byte: line_start + at + target.len(),
                start_line: line_no,
                end_line: line_no,
            },
            String::new(),
            "lines/rst",
        ));
    };
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();
    for directive in [".. include::", ".. image::", ".. figure::"] {
        if let Some(target) = trimmed.strip_prefix(directive) {
            let at = indent + directive.len();
            push(target, at, false);
            return;
        }
    }
    if let Some(rest) = trimmed.strip_prefix(".. _") {
        if let Some((_, target)) = rest.split_once(':') {
            push(target, raw.len() - target.len(), false);
        }
    }
    let mut from = 0usize;
    while let Some(pos) = raw[from..].find(":doc:`") {
        let start = from + pos + 6;
        if let Some(end) = raw[start..].find('`') {
            push(&raw[start..start + end], start, true);
            from = start + end + 1;
        } else {
            break;
        }
    }
    // Conventional RST hyperlink: `caption <relative/file.rst>`_.
    let mut from = 0usize;
    while let Some(pos) = raw[from..].find('<') {
        let start = from + pos + 1;
        if let Some(end) = raw[start..].find(">`_") {
            push(&raw[start..start + end], start, false);
            from = start + end + 3;
        } else {
            break;
        }
    }
}

fn rst_import_target(target: &str, doc_role: bool) -> Option<String> {
    let target = target
        .trim()
        .trim_matches(|c| c == '`' || c == '"' || c == '\'');
    let target = target
        .rsplit_once('<')
        .and_then(|(_, tail)| tail.strip_suffix('>'))
        .unwrap_or(target);
    let target = target
        .trim_start_matches('~')
        .split('#')
        .next()
        .unwrap_or(target)
        .trim();
    if target.is_empty() || is_external_url(target) || target.starts_with(':') {
        return None;
    }
    Some(
        if doc_role && !target.rsplit('/').next().unwrap_or(target).contains('.') {
            format!("{target}.rst")
        } else {
            target.to_string()
        },
    )
}

// --- Jinja ------------------------------------------------------------------

/// Template files are not general HTML: the useful repository structure is in
/// Jinja statements. Extract macros and blocks as definitions, and template
/// composition (`extends`, `include`, and macro imports) as local file edges.
/// A small scanner is intentionally preferable to an HTML grammar here: it
/// preserves byte locations and handles Flask's `.html` templates as well as
/// `.j2`/`.jinja` files without pretending to understand rendered HTML.
pub(super) fn jinja_facts(src: &[u8]) -> Extraction {
    let text = String::from_utf8_lossy(src);
    struct Open {
        kind: &'static str,
        name: String,
        start: usize,
        line: usize,
        signature: String,
    }
    let mut open = Vec::<Open>::new();
    let mut facts = Vec::new();
    let mut at = 0usize;
    while let Some(rel) = text[at..].find("{%") {
        let start = at + rel;
        let Some(close_rel) = text[start + 2..].find("%}") else {
            break;
        };
        let end = start + 2 + close_rel + 2;
        let body = text[start + 2..start + 2 + close_rel].trim();
        let line = line_number(src, start);
        let word = body.split_whitespace().next().unwrap_or("");
        match word {
            "extends" | "include" | "import" | "from" => {
                if let Some((target, offset)) = jinja_quoted_target(body) {
                    facts.push(line_fact(
                        "import",
                        target,
                        LineSpan {
                            start_byte: start + 2 + offset,
                            end_byte: start + 2 + offset + target.len(),
                            start_line: line,
                            end_line: line,
                        },
                        String::new(),
                        "lines/jinja",
                    ));
                }
            }
            "block" | "macro" => {
                if let Some(name) = body.split_whitespace().nth(1) {
                    let name = name.split('(').next().unwrap_or(name).trim_end_matches('-');
                    if !name.is_empty() {
                        open.push(Open {
                            kind: if word == "block" { "block" } else { "function" },
                            name: name.to_string(),
                            start,
                            line,
                            signature: format!("{{% {body} %}}"),
                        });
                    }
                }
            }
            "endblock" | "endmacro" => {
                let kind = if word == "endblock" {
                    "block"
                } else {
                    "function"
                };
                if let Some(pos) = open.iter().rposition(|o| o.kind == kind) {
                    let entry = open.remove(pos);
                    facts.push(line_fact(
                        entry.kind,
                        &entry.name,
                        LineSpan {
                            start_byte: entry.start,
                            end_byte: end,
                            start_line: entry.line,
                            end_line: line,
                        },
                        entry.signature,
                        "lines/jinja",
                    ));
                }
            }
            _ => {}
        }
        at = end;
    }
    let eof_line = line_number(src, src.len());
    for entry in open {
        facts.push(line_fact(
            entry.kind,
            &entry.name,
            LineSpan {
                start_byte: entry.start,
                end_byte: src.len(),
                start_line: entry.line,
                end_line: eof_line,
            },
            entry.signature,
            "lines/jinja",
        ));
    }
    let spans = if src.is_empty() {
        Vec::new()
    } else {
        vec![(0, src.len(), Span::Comment)]
    };
    facts.sort_by_key(|f| (f.start_byte, f.end_byte));
    (facts, spans, Vec::new())
}

fn jinja_quoted_target(body: &str) -> Option<(&str, usize)> {
    for quote in ['\'', '"'] {
        let Some(start) = body.find(quote) else {
            continue;
        };
        if let Some(end) = body[start + 1..].find(quote) {
            return Some((&body[start + 1..start + 1 + end], start + 1));
        }
    }
    None
}

fn line_number(src: &[u8], offset: usize) -> usize {
    src[..offset.min(src.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// Where a line-oriented fact sits: byte range and 1-based line range.
struct LineSpan {
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
}

fn line_fact(kind: &str, name: &str, span: LineSpan, signature: String, source: &str) -> Fact {
    let LineSpan {
        start_byte,
        end_byte,
        start_line,
        end_line,
    } = span;
    Fact {
        kind: kind.to_string(),
        is_derived: false,
        name: name.to_string(),
        start_byte,
        end_byte,
        start_line,
        start_col: 0,
        end_line,
        end_col: 0,
        signature,
        doc: None,
        source: source.to_string(),
        confidence: 1.0,
        visibility: None,
        owner: None,
        shape: None,
        shape_nodes: 0,
        is_test: false,
        decorator: None,
        contract: None,
    }
}

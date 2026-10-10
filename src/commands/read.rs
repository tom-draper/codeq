//! Reading source: `outline` and `read`.

use std::io::Write;

use super::*;

#[derive(Serialize)]
struct OutlineRow {
    name: String,
    kind: String,
    /// Nesting level derived from byte containment (methods inside a class, …).
    depth: usize,
    #[serde(rename = "line")]
    start_line: i64,
    end_line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<String>,
}

struct OutlineDef {
    name: String,
    kind: String,
    start_byte: i64,
    end_byte: i64,
    start_line: i64,
    end_line: i64,
    signature: Option<String>,
    doc: Option<String>,
}

/// One definition in an owner's file, as loaded for containment resolution.
pub(super) struct FileFact {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) start_byte: i64,
    pub(super) end_byte: i64,
    pub(super) line: i64,
    pub(super) signature: Option<String>,
    pub(super) doc: Option<String>,
}

/// `outline <file>` — the structure of a file top to bottom: every definition
/// in source order, with its signature and a nesting depth so containment
/// (methods within a class, nested functions) is visible.
pub fn cmd_outline(cwd: &Path, file: &str, docs: bool, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // Resolve to one file the same way `read` does: exact path wins, else a
    // unique suffix match. Matching `path LIKE '%/README.md'` as well as the
    // exact path merged every README in the repo into one interleaved outline.
    let rel = resolve_file(&s, file)?;
    let mut stmt = s.db.conn.prepare(
        "SELECT name, kind, start_byte, end_byte, start_line, end_line, signature, doc
         FROM facts
         WHERE kind <> 'import' AND path = ?1
         ORDER BY start_byte, end_byte DESC",
    )?;
    let defs: Vec<OutlineDef> = stmt
        .query_map([&rel], |r| {
            let kind: String = r.get(1)?;
            Ok(OutlineDef {
                name: r.get(0)?,
                start_byte: r.get(2)?,
                end_byte: r.get(3)?,
                start_line: r.get(4)?,
                end_line: r.get(5)?,
                signature: declaration_header(&kind, r.get(6)?),
                kind,
                doc: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    // Depth = number of other defs whose byte range strictly encloses this one.
    let data: Vec<OutlineRow> = defs
        .iter()
        .map(|d| {
            let depth = defs
                .iter()
                .filter(|o| {
                    !std::ptr::eq(*o, d)
                        && o.start_byte <= d.start_byte
                        && o.end_byte >= d.end_byte
                        && (o.start_byte != d.start_byte || o.end_byte != d.end_byte)
                })
                .count();
            OutlineRow {
                name: d.name.clone(),
                kind: d.kind.clone(),
                depth,
                start_line: d.start_line,
                end_line: d.end_line,
                signature: d.signature.clone(),
                doc: if docs { d.doc.clone() } else { None },
            }
        })
        .collect();

    s.emit(data)
}

/// Bodies of function/method definitions this many lines or longer are folded.
pub(crate) const MIN_FOLD: usize = 4;

/// `read <file>` — a folding file reader (text output, not JSON). By default it
/// shows the file with long function/method bodies collapsed to their signature
/// plus a marker with the exact hidden line range, so an agent grasps a large
/// file's whole shape cheaply and expands only what it needs. `--lines A-B` and
/// `--symbol NAME` return verbatim, line-numbered slices to read or edit.
///
/// Several files, and several ranges of one, are a single call: `read a.rs:10-40
/// b.rs:5-9,80-90`. That is the shape of the `sed -n 'A,Bp'` an agent would
/// otherwise run once per slice, and a round trip costs more than its bytes.
pub fn cmd_read(
    cwd: &Path,
    files: &[String],
    lines: &[String],
    symbol: Option<&str>,
    full: bool,
    raw: bool,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let flag_ranges = lines.iter().map(parse_range).collect::<Result<Vec<_>>>()?;
    let mut targets: Vec<(String, Vec<(usize, usize)>)> = Vec::new();
    for arg in files {
        targets.push(match resolve_file(&s, arg) {
            Ok(rel) => (rel, Vec::new()),
            // Not a file as written: `path:A-B`, the form a search row, a stack
            // frame and a compiler error all print. The whole argument is tried
            // first, so a real file with a colon in its name still wins.
            Err(e) => match arg.rsplit_once(':').and_then(|(f, spec)| {
                let ranges = spec.split(',').map(parse_range).collect::<Result<Vec<_>>>().ok()?;
                Some((f, ranges))
            }) {
                Some((f, ranges)) => (resolve_file(&s, f)?, ranges),
                None => return Err(e),
            },
        });
    }
    if targets.len() > 1 && (!flag_ranges.is_empty() || symbol.is_some()) {
        return Err(anyhow!(
            "--lines/--symbol address one file; with several, write the range on each: a.rs:10-40 b.rs:5-9"
        ));
    }
    if raw && targets.len() > 1 {
        return Err(anyhow!("--raw yields one exact block; it takes one file"));
    }
    let many = targets.len() > 1;
    for (i, (rel, inline)) in targets.iter().enumerate() {
        if many {
            // One blank line between files, and a header naming each.
            if i > 0 {
                println!();
            }
            println!("# {rel}");
        }
        let ranges = if inline.is_empty() { &flag_ranges } else { inline };
        if !inline.is_empty() && !flag_ranges.is_empty() {
            return Err(anyhow!("a range was given twice: on the path and with --lines"));
        }
        read_one(&s, rel, ranges, symbol, full, raw)?;
    }
    Ok(())
}

fn read_one(
    s: &Session,
    rel: &str,
    ranges: &[(usize, usize)],
    symbol: Option<&str>,
    full: bool,
    raw: bool,
) -> Result<()> {
    let rel = rel.to_string();
    let bytes =
        std::fs::read(s.repo.root.join(&rel)).map_err(|e| anyhow!("cannot read {rel}: {e}"))?;
    // Match the index: drop a leading UTF-8 BOM so line/column numbering here
    // agrees with what search and the symbol facts report.
    let source_bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let text = String::from_utf8_lossy(source_bytes);
    let file_lines: Vec<&str> = text.lines().collect();
    // Keep line terminators for --raw: splitting with `lines()` and printing
    // each line would silently rewrite CRLF, invalid UTF-8, or a missing final
    // newline.
    let raw_lines: Vec<&[u8]> = source_bytes
        .split_inclusive(|byte| *byte == b'\n')
        .collect();
    let n = file_lines.len();
    let show = |a: usize, b: usize| -> Result<()> {
        if raw {
            print_range_raw(&raw_lines, a, b)?;
        } else {
            print_range(&file_lines, a, b)
        }
        Ok(())
    };

    // Verbatim line ranges.
    if !ranges.is_empty() {
        // `--raw` is one exact block for an editor's old_string; two ranges
        // concatenated would be text that exists nowhere in the file.
        if raw && ranges.len() > 1 {
            return Err(anyhow!("--raw yields one exact block; it takes one range"));
        }
        for (i, (a, b)) in ranges.iter().enumerate() {
            // grep's separator between non-adjacent blocks.
            if i > 0 {
                println!("--");
            }
            show(*a, *b)?;
        }
        return Ok(());
    }

    // Verbatim definition(s) by name.
    if let Some(name) = symbol {
        let mut stmt = s.db.conn.prepare(
            "SELECT start_line, end_line FROM facts
             WHERE path = ?1 AND name = ?2 AND kind <> 'import' ORDER BY start_line",
        )?;
        let mut ranges: Vec<(i64, i64)> = stmt
            .query_map((&rel, name), |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        // A markdown heading is stored bare, but `outline` renders its signature —
        // the raw source line, `## Heading` — so copying a heading out of
        // `outline` and into `--symbol` is the obvious workflow and it failed.
        // Accept the copied form: strip the leading `#` run and retry. Nothing in
        // any supported language declares a name starting with `#`, so this can
        // only ever rescue the case it is for.
        if ranges.is_empty() {
            let bare = name.trim_start_matches('#').trim();
            if bare != name && !bare.is_empty() {
                ranges = stmt
                    .query_map((&rel, bare), |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
            }
        }
        if ranges.is_empty() {
            return Err(anyhow!("no definition named '{name}' in {rel}"));
        }
        // `--raw` yields exact bytes for an editor's old_string, so a single
        // clean block is what's wanted — refuse an ambiguous multi-match rather
        // than emit two concatenated definitions.
        if raw && ranges.len() > 1 {
            return Err(anyhow!(
                "{} definitions named '{name}' in {rel}; --raw needs one — use --lines A-B",
                ranges.len()
            ));
        }
        for (a, b) in &ranges {
            show(*a as usize, *b as usize)?;
            if !raw {
                println!(); // blank between/after defs, as the folded view does
            }
        }
        return Ok(());
    }

    // Whole file when asked, or when we can't parse it to know what to fold.
    if full || raw || !crate::facts::has_spans(language_for(&rel).unwrap_or("")) {
        show(1, n)?;
        return Ok(());
    }

    // Folded view: hide the bodies of function/method definitions.
    let mut folded = vec![false; n];
    {
        let mut stmt = s.db.conn.prepare(
            "SELECT start_line, end_line FROM facts
             WHERE path = ?1 AND kind IN ('function', 'method')",
        )?;
        let rows = stmt.query_map([&rel], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (sl, el) = row?;
            let (sl, el) = (sl.max(1) as usize, el.max(1) as usize);
            if el >= sl && el - sl >= MIN_FOLD {
                for ln in (sl + 1)..=el {
                    if ln - 1 < n {
                        folded[ln - 1] = true;
                    }
                }
            }
        }

        // A markdown heading owns everything down to the next heading of the
        // same or higher level, so folding its whole range would collapse the
        // document to its title. Fold the prose *between* consecutive headings
        // instead, which leaves the full heading tree visible: a table of
        // contents with each section's length, and `--symbol` to open one.
        let mut heads = s.db.conn.prepare(
            "SELECT start_line FROM facts
             WHERE path = ?1 AND kind = 'heading' ORDER BY start_line",
        )?;
        let starts: Vec<usize> = heads
            .query_map([&rel], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .map(|l| l.max(1) as usize)
            .collect();
        for (i, &sl) in starts.iter().enumerate() {
            let el = starts.get(i + 1).map(|&s| s - 1).unwrap_or(n);
            if el > sl && el - sl >= MIN_FOLD {
                for ln in (sl + 1)..=el.min(n) {
                    folded[ln - 1] = true;
                }
            }
        }
    }
    print_folded(&file_lines, &folded);
    Ok(())
}

fn parse_range(spec: impl AsRef<str>) -> Result<(usize, usize)> {
    let spec = spec.as_ref();
    // A bare number is that one line: `a.rs:42`, as a search row prints it.
    let (a, b) = spec.split_once('-').unwrap_or((spec, spec));
    let a: usize = a
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid line range"))?;
    let b: usize = b
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid line range"))?;
    Ok((a.max(1), b.max(a)))
}

fn print_range(lines: &[&str], a: usize, b: usize) {
    for i in a.max(1)..=b {
        if let Some(l) = lines.get(i - 1) {
            println!("{i}\t{l}");
        }
    }
}

/// Verbatim lines with no line-number prefix — exact source, ready to drop into
/// an editor's search text (the `--raw` view).
fn print_range_raw(lines: &[&[u8]], a: usize, b: usize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for i in a.max(1)..=b {
        if let Some(l) = lines.get(i - 1) {
            stdout.write_all(l)?;
        }
    }
    Ok(())
}

fn print_folded(lines: &[&str], folded: &[bool]) {
    let n = lines.len();
    let mut i = 0;
    while i < n {
        if folded[i] {
            let start = i;
            while i < n && folded[i] {
                i += 1;
            }
            let count = i - start;
            // Range printed in the exact syntax `--lines` accepts, so the marker
            // is a copy-pasteable next command rather than something to retype.
            println!("\t... {count} lines ({}-{})", start + 1, i);
        } else {
            println!("{}\t{}", i + 1, lines[i]);
            i += 1;
        }
    }
}

/// Resolve a file argument (repo-relative path or basename) to an indexed path.
pub(super) fn resolve_file(s: &Session, arg: &str) -> Result<String> {
    if s.db
        .conn
        .query_row("SELECT 1 FROM files WHERE path = ?1", [arg], |_| Ok(()))
        .is_ok()
    {
        return Ok(arg.to_string());
    }
    let suffix = format!("%/{}", escape_like_literal(arg));
    let mut stmt =
        s.db.conn
            .prepare("SELECT path FROM files WHERE path LIKE ?1 ESCAPE '\\' ORDER BY path")?;
    let matches: Vec<String> = stmt
        .query_map([suffix], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        // "file not found" describes the lookup that failed; the useful message
        // describes the state that caused it. An absolute path into a *different*
        // checkout exists on disk and is simply not in this index, and being told
        // it does not exist sends you checking the path instead of the repo. Same
        // class as the empty-result notes: the branch is known, so name it.
        0 => {
            let elsewhere =
                std::path::Path::new(arg).is_absolute() && std::path::Path::new(arg).exists();
            if elsewhere {
                Err(anyhow!(
                    "'{arg}' exists but is not in this index (rooted at {}); run codeq from \
                     that file's repository, or point at it with --root",
                    s.repo.root.display(),
                ))
            } else {
                Err(anyhow!("file not found in this index: {arg}"))
            }
        }
        _ => Err(anyhow!("'{arg}' is ambiguous: {}", matches.join(", "))),
    }
}

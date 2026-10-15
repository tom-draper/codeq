//! The content-search engine shared by `search`, `references`, `unused`, and
//! `astgrep`: pull candidate file bodies from the trigram FTS index, run a regex
//! over them, classify matches by code/comment/string span, and shape the output
//! as matches / files / counts. No filesystem reads — everything comes from the
//! index. Commands in [`crate::commands`] drive it; it depends only on a
//! `rusqlite::Connection`, not the whole session.
//!
//! Two exceptions scan something other than the index: [`scan_stdin`] a buffer
//! piped in on stdin, so codeq can stand in for grep in a pipeline, and
//! [`scan_unindexed`] a path the caller named that the index does not hold.
//! Both share [`scan_doc`] with the indexed path, so all three answer
//! identically.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use regex_syntax::hir::literal::{ExtractKind, Extractor as LiteralExtractor};
use serde::Serialize;

use crate::callsite::CallMap;
use crate::facts::{has_spans, Extractor, Span};

const MAX_LINE: usize = 240;

/// One matched line, shared by `search` and `references`.
#[derive(Serialize, Clone)]
pub(crate) struct Hit {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) col: usize,
    #[serde(skip_serializing)]
    pub(crate) byte: usize,
    /// The innermost definition enclosing this match, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) symbol: Option<String>,
    /// For a call site, the receiver it is called on, as written: `db` in
    /// `db.query()`, `helpers` in `helpers::stem()`. Only populated when a
    /// position filter had the file parsed anyway.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) receiver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    /// True when this line is where `symbol` is declared, so the line already
    /// names it. Text output leaves the `[symbol]` tag off such a row.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) decl: bool,
    /// The matched line, trailing whitespace trimmed and capped.
    pub(crate) text: String,
    /// Context lines before the match (with -B/-C), nearest last.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) before: Vec<String>,
    /// Context lines after the match (with -A/-C).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) after: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct FileCount {
    pub(crate) path: String,
    pub(crate) count: usize,
}

/// Output shape depends on the mode: full matches, matching file paths, or
/// per-file counts. `truncated` is true when `--limit` cut it off.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum SearchData {
    Matches {
        matches: Vec<Hit>,
        truncated: bool,
    },
    Files {
        files: Vec<String>,
        truncated: bool,
    },
    Counts {
        counts: Vec<FileCount>,
        truncated: bool,
    },
}

impl SearchData {
    /// Whether the result holds no rows, whichever shape it took. `search`
    /// returns three different containers depending on `-l`/`-c`, so a caller
    /// asking "did this find anything" should not have to match on all three.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            SearchData::Matches { matches, .. } => matches.is_empty(),
            SearchData::Files { files, .. } => files.is_empty(),
            SearchData::Counts { counts, .. } => counts.is_empty(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum OutMode {
    Matches,
    Files,
    Count,
    /// grep's `-L`: the files with no match. Reported as `files`, like `-l`.
    FilesWithout,
}

/// Restrict matches by the syntactic class of their position.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum InFilter {
    Any,
    Code,
    Comments,
    Strings,
}

impl InFilter {
    pub(crate) fn parse(s: &str) -> Result<InFilter> {
        Ok(match s {
            "any" => InFilter::Any,
            "code" => InFilter::Code,
            "comment" | "comments" => InFilter::Comments,
            "string" | "strings" => InFilter::Strings,
            other => return Err(anyhow!("--in must be code|comments|strings, got '{other}'")),
        })
    }

    fn accepts(self, cls: Option<Span>) -> bool {
        match self {
            InFilter::Any => true,
            InFilter::Code => cls.is_none(),
            InFilter::Comments => cls == Some(Span::Comment),
            InFilter::Strings => cls == Some(Span::Str),
        }
    }

    /// Whether a file codeq cannot classify must be skipped entirely.
    ///
    /// True for the `--in` filters, which are positive claims about syntax that
    /// cannot be made without cached spans.
    fn requires_spans(self) -> bool {
        matches!(
            self,
            InFilter::Code | InFilter::Comments | InFilter::Strings
        )
    }
}

/// SQL selecting a file's cached comment/string spans, sorted. Statement text
/// rather than a helper that runs it: both callers prepare once and step it per
/// file, so sharing the query keeps one notion of a span without moving the
/// `prepare` into a hot loop.
pub(crate) const SPANS_SQL: &str = "SELECT start_byte, end_byte, kind FROM spans
     WHERE file_id = (SELECT id FROM files WHERE path = ?1) ORDER BY start_byte";

/// Map a [`SPANS_SQL`] row to `(start, end, class)`.
pub(crate) fn span_row(r: &rusqlite::Row) -> rusqlite::Result<(usize, usize, Span)> {
    Ok((
        r.get::<_, i64>(0)? as usize,
        r.get::<_, i64>(1)? as usize,
        Span::from_i64(r.get::<_, i64>(2)?).unwrap_or(Span::Str),
    ))
}

/// The class of byte offset `off` given sorted, non-overlapping `spans`.
pub(crate) fn class_at(spans: &[(usize, usize, Span)], off: usize) -> Option<Span> {
    let idx = spans.partition_point(|(s, _, _)| *s <= off);
    if idx == 0 {
        return None;
    }
    let (s, e, c) = spans[idx - 1];
    if s <= off && off < e {
        Some(c)
    } else {
        None
    }
}

/// Which files a scan covers. Every filter is applied per candidate row, and a
/// file must pass all of them.
#[derive(Clone, Copy, Default)]
pub(crate) struct ScanScope<'a> {
    /// Path substring, as `--path` takes it.
    pub path: Option<&'a str>,
    /// Exact language name.
    pub language: Option<&'a str>,
    /// A predicate over the repo-relative path, for scopes a substring cannot
    /// express (globs, several positional paths).
    pub accept: Option<&'a dyn Fn(&str) -> bool>,
}

pub(crate) struct WordRegex {
    ordinary: regex::Regex,
    dollar: regex::Regex,
}

impl WordRegex {
    pub(crate) fn new(expr: &str, ignore_case: bool) -> Result<Self> {
        let hir = regex_syntax::ParserBuilder::new()
            .case_insensitive(ignore_case)
            .multi_line(true)
            .build()
            .parse(expr)?;
        let compile = |extra: &str| {
            regex::Regex::new(&format!(
                "({hir})(?:\\z|[^\\p{{Alphabetic}}\\p{{Number}}_{extra}])"
            ))
        };
        Ok(Self {
            ordinary: compile("")?,
            dollar: compile("$")?,
        })
    }

    fn matches<'a>(
        &'a self,
        body: &'a str,
        lang: Option<&'a str>,
        spans: &'a [(usize, usize, Span)],
    ) -> impl Iterator<Item = regex::Match<'a>> + 'a {
        // The suffix participates in alternative selection. Return only the
        // capture and resume at its end so adjacent matches can share a separator.
        let valid = move |m: &regex::Match<'_>| {
            has_identifier_boundaries(
                body.as_bytes(),
                m.start(),
                m.end(),
                lang,
                class_at(spans, m.start()) == Some(Span::Str),
            )
        };
        let advance = move |at: usize| at + body[at..].chars().next().map_or(1, char::len_utf8);
        let mut at = 0;
        let mut previous_end = None;
        std::iter::from_fn(move || {
            while at <= body.len() {
                let ordinary = self.ordinary.captures_at(body, at).and_then(|c| c.get(1));
                let dollar = if dollar_binds(lang) {
                    self.dollar.captures_at(body, at).and_then(|c| c.get(1))
                } else {
                    None
                };
                let start = ordinary
                    .into_iter()
                    .chain(dollar)
                    .map(|m| m.start())
                    .min()?;
                let candidate = ordinary
                    .filter(|m| m.start() == start && valid(m))
                    .or_else(|| dollar.filter(|m| m.start() == start && valid(m)));
                let Some(m) = candidate else {
                    at = advance(start);
                    continue;
                };
                let repeated_empty = m.is_empty() && previous_end == Some(m.end());
                at = if m.is_empty() {
                    advance(m.end())
                } else {
                    m.end()
                };
                previous_end = Some(m.end());
                if !repeated_empty {
                    return Some(m);
                }
            }
            None
        })
    }
}

pub(crate) struct ScanOpts {
    pub(crate) before: usize,
    pub(crate) after: usize,
    pub(crate) out: OutMode,
    pub(crate) in_filter: InFilter,
    pub(crate) limit: usize,
    /// Keep only matches in call position — immediately followed by `(` — so a
    /// bare identifier / field / variable of the same name is excluded.
    pub(crate) call_only: bool,
    /// Keep only matches qualified by this receiver: `RECV.name`, `RECV::name`,
    /// or `RECV->name`. For disambiguating a method/associated name by its type.
    pub(crate) qualifier: Option<String>,
    /// Require identifier boundaries around a match. Unlike regex `\b`, this
    /// treats `$` as part of an identifier so JavaScript names such as
    /// `$ZodError` are matched as one symbol.
    pub(crate) identifier_boundary: bool,
    pub(crate) word_regex: Option<WordRegex>,
    /// Mirror of `--max-per-file`: matches past the Nth in a file still get
    /// collected (the envelope needs them to report each file's true count) but
    /// they do not spend `limit`. Without this the two budgets fight — the scan
    /// stops after `limit` *raw* matches, so a cap of 3 that should have surveyed
    /// the whole repo instead surveys the first `limit/3` files and reports their
    /// partial counts as totals.
    pub(crate) per_file: Option<usize>,
    /// grep's `-o`: a row per occurrence carrying only the matched text, where
    /// the default is a row per line carrying the line.
    pub(crate) only_matching: bool,
    /// grep's `-v`: the lines the pattern does *not* match.
    pub(crate) invert: bool,
}

impl ScanOpts {
    /// Whether a file with no occurrence of the pattern can still contribute.
    /// Then the literal prefilter — which keeps only files that could match —
    /// would drop exactly the files being asked for, so it must not be used.
    pub(crate) fn wants_non_matching(&self) -> bool {
        self.invert || self.out == OutMode::FilesWithout
    }
}

/// Languages whose lexer allows `$` *inside an identifier*, so `$ZodError` and
/// `$props` are single names and `references ZodError` must not match within
/// one.
///
/// An allowlist, because that is the shape of the fact: this is a property of
/// each grammar, not a heuristic with exceptions. It started as a denylist —
/// bind everywhere except PHP, Bash and PowerShell — which quietly asserted
/// that `$` binds in every language nobody had thought about yet. It does not.
/// Rust `$bytes` is a macro metavariable, `Outer$Inner` in a JVM config file is
/// a binary name, `$(H1)` in a Makefile is an expansion, and `$db` in PHP is a
/// sigil: in every one of those `$` is punctuation and the name beside it is a
/// real occurrence that `rg -w` finds. Getting the default backwards cost the
/// superset guarantee in thirteen places across five languages.
const DOLLAR_BINDS: &[&str] = &[
    "javascript",
    "typescript",
    "tsx",
    "svelte",
    "vue",
    "java",
    "kotlin",
    "scala",
];

/// A file codeq assigned no language is punctuation-agnostic: it has no lexer to
/// appeal to, so it follows `rg -w` rather than guessing.
fn dollar_binds(lang: Option<&str>) -> bool {
    lang.is_some_and(|l| DOLLAR_BINDS.contains(&l))
}

/// Whether a char continues an identifier, so a match touching it is not
/// word-bounded.
///
/// Unicode-aware on purpose. This tested `b >= 0x80` — "any non-ASCII byte is a
/// letter" — which is right for a CJK ideograph and wrong for CJK punctuation,
/// and the two are neighbours in real prose: `console。` in a Chinese README
/// made codeq reject a boundary that `rg -w` accepts, because the full stop is
/// three bytes above 0x7F. `char::is_alphanumeric` is the question actually
/// being asked.
fn is_ident_char(c: char, dollar_binds: bool) -> bool {
    c == '_' || (dollar_binds && c == '$') || c.is_alphanumeric()
}

/// The char whose encoding starts at `at`.
fn char_at(bytes: &[u8], at: usize) -> Option<char> {
    let b = *bytes.get(at)?;
    let len = match b {
        0x00..=0x7F => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        _ => 4,
    };
    std::str::from_utf8(bytes.get(at..at + len)?)
        .ok()?
        .chars()
        .next()
}

/// The char ending immediately before `at`. The buffer is always a `&str`'s
/// bytes, so walking back over continuation bytes lands on a lead byte.
fn char_before(bytes: &[u8], at: usize) -> Option<char> {
    if at == 0 || at > bytes.len() {
        return None;
    }
    let mut i = at - 1;
    while i > 0 && bytes[i] & 0b1100_0000 == 0b1000_0000 {
        i -= 1;
    }
    std::str::from_utf8(bytes.get(i..at)?).ok()?.chars().next()
}

pub(crate) fn has_identifier_boundaries(
    bytes: &[u8],
    start: usize,
    end: usize,
    lang: Option<&str>,
    in_string: bool,
) -> bool {
    // `$` is identifier syntax only where a lexer is lexing identifiers. Inside
    // a string literal it is almost always something else: a JVM binary name
    // (`Java17RecordTest$PrivateRecord` in an expected-message string), a
    // template placeholder (`"$error.message"`), a shell expansion. Treating
    // those as one identifier lost real occurrences that `rg -w` finds, in
    // exactly the languages where `$` does bind in code.
    let binds = dollar_binds(lang) && !in_string;
    !char_before(bytes, start).is_some_and(|c| is_ident_char(c, binds))
        && !char_at(bytes, end).is_some_and(|c| is_ident_char(c, binds))
}

/// Whether an offset is not in a cached comment span. Used by `unused`, whose
/// conservative rule counts code and strings as uses but never comments.
pub(crate) fn is_not_comment(spans: &[(usize, usize, Span)], off: usize) -> bool {
    class_at(spans, off) != Some(Span::Comment)
}

/// Whether an offset sits in a cached string literal. Used to decide whether `$`
/// is identifier syntax there — see [`has_identifier_boundaries`].
pub(crate) fn is_in_string(spans: &[(usize, usize, Span)], off: usize) -> bool {
    class_at(spans, off) == Some(Span::Str)
}

/// Whether the match is a markup instantiation — `<Name …/>` or `</Name>`.
///
/// A Svelte or Vue component is *used* by writing it as an element, which no
/// grammar reports as a call expression: tree-sitter is right that this is not a
/// call node, and codeq is right that it is a usage. So this is checked
/// separately from, and in addition to, the parse tree — otherwise `--calls` on a
/// component returns nothing for a component that is plainly used, which is how
/// this regressed when call detection moved onto the parse tree.
pub(crate) fn is_markup_instantiation(bytes: &[u8], start: usize) -> bool {
    let mut i = start;
    while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
        i -= 1;
    }
    if i > 0 && bytes[i - 1] == b'/' {
        i -= 1; // a closing tag
    }
    i > 0 && bytes[i - 1] == b'<'
}

/// Whether the match is an *invocation* rather than a bare mention.
///
/// `Name(` is the call-syntax languages. But codeq now indexes formats where
/// invocation is spelled differently and `Name(` never appears: a Svelte or Vue
/// component is invoked as `<Name …/>`, so `--calls` on one used to return
/// nothing at all for a component that is plainly used. Both forms count, since
/// the question `--calls` answers — "where is this actually used, not merely
/// named?" — is the same one.
pub(crate) fn is_call_position(bytes: &[u8], start: usize, end: usize) -> bool {
    if is_markup_instantiation(bytes, start) {
        return true;
    }
    let mut j = end;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    j < bytes.len() && bytes[j] == b'('
}

/// Whether the match starting at byte `off` is qualified by `recv` — preceded by
/// `recv.` / `recv::` / `recv->` at a word boundary.
///
/// Whitespace, including newlines, is skipped on both sides of the separator, so
/// a wrapped method chain reads the same as a single-line call:
///
/// ```text
/// db.query()          db . query()          db
///                                               .query()
/// ```
///
/// Still textual rather than a parsed receiver expression: `a.b.query()` counts
/// for `--on b` (the nearest qualifier is what is tested) and a call-valued
/// receiver like `conn().query()` is not matched at all.
fn has_qualifier(bytes: &[u8], off: usize, recv: &str, lang: Option<&str>) -> bool {
    let skip_ws_back = |mut i: usize| {
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        i
    };

    let q = recv.as_bytes();
    let sep_end = skip_ws_back(off);
    for sep in [b"::".as_slice(), b"->".as_slice(), b".".as_slice()] {
        if sep_end < sep.len() || &bytes[sep_end - sep.len()..sep_end] != sep {
            continue;
        }
        let qend = skip_ws_back(sep_end - sep.len());
        if qend >= q.len()
            && &bytes[qend - q.len()..qend] == q
            && (qend == q.len()
                || !char_before(bytes, qend - q.len())
                    .is_some_and(|c| is_ident_char(c, dollar_binds(lang))))
        {
            return true;
        }
    }
    false
}

pub(crate) struct FileDef {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
}

/// Below this many definitions a linear scan beats building the tree.
const DEF_INDEX_MIN: usize = 24;

/// One file's definitions, indexed to answer "which definition encloses this
/// byte" in `O(log n)` rather than by scanning every definition per match.
///
/// Definitions are ordered by `(start_byte, load order)` and a max-`end_byte`
/// tree is laid over that order. The innermost containing definition is the
/// rightmost entry that starts at or before the offset and ends after it, so it
/// is one descent. That keeps the scan's tie rule exactly: the greatest start
/// wins, and among equal starts the one loaded last.
#[derive(Default)]
pub(crate) struct DefIndex {
    defs: Vec<FileDef>,
    /// Indices into `defs`, ordered by `(start_byte, index)`.
    order: Vec<u32>,
    /// `defs[order[i]].start_byte`, for the offset's partition point.
    starts: Vec<usize>,
    /// Segment tree of `end_byte` maxima over `order`; empty for small files.
    tree: Vec<usize>,
    leaves: usize,
}

impl From<Vec<FileDef>> for DefIndex {
    fn from(defs: Vec<FileDef>) -> Self {
        if defs.len() < DEF_INDEX_MIN {
            return DefIndex {
                defs,
                ..DefIndex::default()
            };
        }
        let mut order: Vec<u32> = (0..defs.len() as u32).collect();
        order.sort_by_key(|&i| (defs[i as usize].start_byte, i));
        let starts = order.iter().map(|&i| defs[i as usize].start_byte).collect();
        let leaves = defs.len().next_power_of_two();
        let mut tree = vec![0usize; leaves * 2];
        for (slot, &i) in order.iter().enumerate() {
            tree[leaves + slot] = defs[i as usize].end_byte;
        }
        for node in (1..leaves).rev() {
            tree[node] = tree[node * 2].max(tree[node * 2 + 1]);
        }
        DefIndex {
            defs,
            order,
            starts,
            tree,
            leaves,
        }
    }
}

impl DefIndex {
    /// The innermost definition whose byte range contains `off`.
    pub(crate) fn innermost(&self, off: usize) -> Option<&FileDef> {
        if self.tree.is_empty() {
            return self
                .defs
                .iter()
                .filter(|d| d.start_byte <= off && off < d.end_byte)
                .max_by_key(|d| d.start_byte);
        }
        // Entries [0, p) start at or before `off`; of those, take the rightmost
        // that has not ended yet.
        let p = self.starts.partition_point(|&s| s <= off);
        let slot = self.rightmost_ending_after(1, 0, self.leaves, p, off)?;
        Some(&self.defs[self.order[slot] as usize])
    }

    /// The rightmost slot in `[0, limit)` whose end exceeds `off`, searching the
    /// subtree `node` that covers `[lo, hi)`.
    fn rightmost_ending_after(
        &self,
        node: usize,
        lo: usize,
        hi: usize,
        limit: usize,
        off: usize,
    ) -> Option<usize> {
        if lo >= limit || self.tree[node] <= off {
            return None;
        }
        if hi - lo == 1 {
            return Some(lo);
        }
        let mid = (lo + hi) / 2;
        self.rightmost_ending_after(node * 2 + 1, mid, hi, limit, off)
            .or_else(|| self.rightmost_ending_after(node * 2, lo, mid, limit, off))
    }
}

/// Byte offset of the start of each line.
pub(crate) fn line_starts_of(text: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    v.extend(memchr::memchr_iter(b'\n', text.as_bytes()).map(|i| i + 1));
    v
}

/// How many lines `text` has, as grep counts them: a trailing newline ends the
/// last line instead of starting an empty one, and empty text has none.
/// `line_starts` always holds that phantom start, so a position past the last
/// real line must be checked against this, not against `line_starts.len()`.
fn line_count(text: &str, line_starts: &[usize]) -> usize {
    if text.is_empty() || text.ends_with('\n') {
        line_starts.len() - 1
    } else {
        line_starts.len()
    }
}

/// The text of line `idx` (0-based), trailing whitespace trimmed and capped.
fn line_text(text: &str, line_starts: &[usize], idx: usize) -> String {
    let start = line_starts[idx];
    let end = line_starts
        .get(idx + 1)
        .map(|&s| s - 1)
        .unwrap_or(text.len());
    cap_str(text[start..end.min(text.len())].trim_end(), MAX_LINE)
}

/// Context lines for the indices in `range` that exist.
fn context_lines(text: &str, line_starts: &[usize], range: std::ops::Range<usize>) -> Vec<String> {
    let n = line_count(text, line_starts);
    range
        .filter(|&i| i < n)
        .map(|i| line_text(text, line_starts, i))
        .collect()
}

pub(crate) fn load_file_defs(stmt: &mut rusqlite::Statement, path: &str) -> Result<DefIndex> {
    let defs: Vec<FileDef> = stmt
        .query_map([path], |r| {
            Ok(FileDef {
                name: r.get(0)?,
                kind: r.get(1)?,
                start_byte: r.get::<_, i64>(2)? as usize,
                end_byte: r.get::<_, i64>(3)? as usize,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(defs.into())
}

/// Run `re` over candidate bodies and shape the output per `opts.out`.
/// Deterministic order (path, then line). When `opts.in_filter` is not `Any`,
/// matches are classified via cached spans; files in a language we cannot parse
/// are excluded (never guessed).
/// Stream candidate file bodies from the trigram FTS index and run `re` over each,
/// shaping the output per `opts.out`. Deterministic order (path, then line).
///
/// Bodies are read from disk one candidate at a time and scanned in place —
/// never collected — so a query over a common substring (which can match a large
/// fraction of a big repo) holds one body at a time instead of materializing all
/// of them (which spiked memory into the hundreds of MB).
/// The trigram prefilter is a safe superset; the precise regex confirms. With
/// no literal, every text file is a candidate.
/// `path`/`language` filters apply per row; when `opts.in_filter` isn't `Any`,
/// matches are classified via cached spans (files in an unparseable language are
/// skipped, never guessed).
pub(crate) fn run_search(
    conn: &rusqlite::Connection,
    root: &Path,
    re: &regex::Regex,
    prefilter: Option<&[String]>,
    scope: ScanScope,
    opts: &ScanOpts,
) -> Result<SearchData> {
    let (acc, _) = run_search_acc(conn, root, re, prefilter, scope, opts)?;
    Ok(acc.finish_for(opts))
}

/// How many hits a scan collects before stopping: one past `--limit`, so the
/// result can say it was truncated.
fn scan_cap(opts: &ScanOpts) -> usize {
    if opts.limit == 0 {
        usize::MAX
    } else if opts.out == OutMode::Matches && opts.per_file.is_some() {
        // Per-file capping needs the rest of the final file to report its true
        // count, so preserve that scan behavior when both caps are combined.
        opts.limit
    } else {
        opts.limit.saturating_add(1)
    }
}

/// [`run_search`] without the final step, for a caller with more documents to
/// scan into the same result ([`scan_unindexed`]). The flag is true when the
/// scan stopped at the limit, so there is nothing left to add.
pub(crate) fn run_search_acc(
    conn: &rusqlite::Connection,
    root: &Path,
    re: &regex::Regex,
    prefilter: Option<&[String]>,
    scope: ScanScope,
    opts: &ScanOpts,
) -> Result<(ScanAcc, bool)> {
    let cap = scan_cap(opts);
    let filtering = opts.in_filter != InFilter::Any;

    // Candidates are every text file, or — given a prefilter — the ones whose
    // trigrams could hold a match. Each literal's clause is an AND of its
    // trigrams; any one literal suffices, so the clauses are OR-ed.
    let clauses = prefilter.filter(|l| !l.is_empty() && !opts.wants_non_matching());
    let mut sql = String::from("SELECT path, language FROM files WHERE is_text = 1");
    if clauses.is_some() {
        sql.push_str(" AND id IN (SELECT rowid FROM content_fts WHERE content_fts MATCH ?1)");
    }
    sql.push_str(" ORDER BY path");
    let query = clauses.map(|c| c.join(" OR "));
    let params: Vec<&dyn rusqlite::ToSql> =
        query.iter().map(|q| q as &dyn rusqlite::ToSql).collect();

    let mut def_stmt = conn.prepare(
        "SELECT name, kind, start_byte, end_byte FROM facts
         WHERE path = ?1 AND kind <> 'import'",
    )?;
    // Comment/string spans are cached in the index (extracted once), so --in is
    // a lookup, not a per-query re-parse.
    let mut span_stmt = conn.prepare(SPANS_SQL)?;
    let mut call_stmt = if opts.call_only || opts.qualifier.is_some() {
        Some(conn.prepare(
            "SELECT start_byte, receiver FROM calls
             WHERE file_id = (SELECT id FROM files WHERE path = ?1)",
        )?)
    } else {
        None
    };

    let mut acc = ScanAcc::default();
    let mut stopped = false;

    let mut fts_stmt = conn.prepare(&sql)?;
    let mut rows = fts_stmt.query(params.as_slice())?;
    'outer: while let Some(row) = rows.next()? {
        let rel = row.get_ref(0)?.as_str()?;
        if let Some(pf) = scope.path {
            if !rel.contains(pf) {
                continue;
            }
        }
        if let Some(accept) = scope.accept {
            if !accept(rel) {
                continue;
            }
        }
        let lang: Option<String> = row.get(1)?;
        if let Some(lf) = scope.language {
            if lang.as_deref() != Some(lf) {
                continue;
            }
        }
        // The index holds trigrams, not text: the file itself is the body.
        let Some(body) = crate::index::load_body(root, rel) else {
            continue;
        };
        let body = body.as_str();

        // A `$`-binding language also needs spans, with no `--in` filter in
        // sight: `$` is identifier syntax in code and something else inside a
        // string, and only the spans tell the two apart. Narrow on purpose —
        // word-bounded queries over JavaScript/TypeScript/Java-family files —
        // so an ordinary search pays nothing for it.
        let needs_spans_for_dollar = opts.identifier_boundary && dollar_binds(lang.as_deref());
        // Load cached spans if filtering; skip files in a language we can't parse.
        let spans = if filtering || needs_spans_for_dollar {
            if !has_spans(lang.as_deref().unwrap_or("")) {
                if filtering && opts.in_filter.requires_spans() {
                    continue;
                }
                // Unclassifiable: treat every position as code rather than drop
                // the file, so an exclusion filter cannot lose real matches.
                Vec::new()
            } else {
                let srows = span_stmt.query_map([rel], span_row)?;
                srows.collect::<std::result::Result<Vec<_>, _>>()?
            }
        } else {
            Vec::new()
        };
        // Defs are only loaded once a match survives the filters, so a candidate
        // that matches the substring but not `\bword\b` (or is filtered by
        // --calls/--on) costs neither a body scan nor a defs query.
        let calls = if let Some(stmt) = call_stmt
            .as_mut()
            .filter(|_| crate::callsite::supports_syntax_calls(lang.as_deref().unwrap_or("")))
        {
            let rows = stmt.query_map([rel], |r| {
                Ok((r.get::<_, usize>(0)?, r.get::<_, Option<String>>(1)?))
            })?;
            Some(rows.collect::<rusqlite::Result<CallMap>>()?)
        } else {
            None
        };
        let mut load_defs = |path: &str| load_file_defs(&mut def_stmt, path).unwrap_or_default();
        if scan_doc(
            rel,
            lang.as_deref(),
            body,
            &spans,
            calls.as_ref(),
            &mut load_defs,
            re,
            opts,
            cap,
            &mut acc,
        ) {
            stopped = true;
            break 'outer;
        }
    }
    drop(rows);
    drop(fts_stmt);

    Ok((acc, stopped))
}

/// Scan files that are on disk but not in the index — gitignored build output,
/// a log, a path outside the repository — into `acc`. `(label, path)` pairs: the
/// label is the path as the caller wrote it, which is what each row reports.
///
/// A directory is walked with every ignore rule off, since being ignored is why
/// it is here. Nothing was parsed, so rows carry no enclosing symbol and the
/// caller must not have asked for `--in`. Binary and unreadable files are
/// skipped, as the index skips them.
pub(crate) fn scan_unindexed(
    targets: &[(String, PathBuf)],
    accept: &dyn Fn(&str, &Path) -> bool,
    re: &regex::Regex,
    opts: &ScanOpts,
    acc: &mut ScanAcc,
) {
    let cap = scan_cap(opts);
    let scan_file = |label: &str, path: &Path, acc: &mut ScanAcc| -> bool {
        if !accept(label, path) {
            return false;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        if crate::index::looks_binary(&bytes) {
            return false;
        }
        let body = String::from_utf8_lossy(&bytes);
        scan_doc(
            label,
            None,
            &body,
            &[],
            None,
            &mut |_| DefIndex::default(),
            re,
            opts,
            cap,
            acc,
        )
    };
    for (label, path) in targets {
        if path.is_file() {
            if scan_file(label, path, acc) {
                return;
            }
            continue;
        }
        let walk = ignore::WalkBuilder::new(path)
            .standard_filters(false)
            .sort_by_file_name(|a, b| a.cmp(b))
            .filter_entry(|e| e.file_name() != ".git" && e.file_name() != ".codeq")
            .build();
        for entry in walk.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rest = entry.path().strip_prefix(path).unwrap_or(entry.path());
            let shown = Path::new(label).join(rest);
            if scan_file(&shown.to_string_lossy(), entry.path(), acc) {
                return;
            }
        }
    }
}

/// Results accumulated across the documents a scan visits.
#[derive(Default)]
pub(crate) struct ScanAcc {
    files: Vec<String>,
    counts: Vec<FileCount>,
    hits: Vec<Hit>,
    truncated: bool,
    /// Hits that count against `limit`. Equal to `hits.len()` unless
    /// `ScanOpts::per_file` is set, when the overflow a file keeps only so its
    /// true count can be reported is excluded.
    kept: usize,
}

impl ScanAcc {
    /// [`ScanAcc::finish`] with the arguments a plain search derives from its
    /// options.
    pub(crate) fn finish_for(self, opts: &ScanOpts) -> SearchData {
        self.finish(
            opts.out,
            opts.limit,
            opts.out == OutMode::Matches && opts.per_file.is_some(),
        )
    }

    pub(crate) fn finish(
        mut self,
        out: OutMode,
        limit: usize,
        preserve_file_survey: bool,
    ) -> SearchData {
        let cap = if limit == 0 { usize::MAX } else { limit };
        let mut truncated = self.truncated;
        match out {
            OutMode::Files | OutMode::FilesWithout => {
                if self.files.len() > cap {
                    self.files.truncate(cap);
                    truncated = true;
                }
                SearchData::Files {
                    files: self.files,
                    truncated,
                }
            }
            OutMode::Count => {
                if self.counts.len() > cap {
                    self.counts.truncate(cap);
                    truncated = true;
                }
                SearchData::Counts {
                    counts: self.counts,
                    truncated,
                }
            }
            OutMode::Matches => SearchData::Matches {
                matches: {
                    if !preserve_file_survey && self.hits.len() > cap {
                        self.hits.truncate(cap);
                        truncated = true;
                    }
                    self.hits
                },
                truncated,
            },
        }
    }
}

/// Scan one document — an indexed file, or a buffer piped in on stdin — for
/// `re`, applying every [`ScanOpts`] filter and appending to `acc`. Returns true
/// when the limit is reached and the caller should stop.
///
/// This is the single place matching happens, so a piped buffer and an indexed
/// file are guaranteed to answer identically: same regex semantics, same
/// `--in`/`--calls`/`--on` filters, same one-row-per-line rule, same context
/// lines. The only difference is where `spans` and `defs` come from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_doc(
    path: &str,
    lang: Option<&str>,
    body: &str,
    spans: &[(usize, usize, Span)],
    calls: Option<&CallMap>,
    load_defs: &mut dyn FnMut(&str) -> DefIndex,
    re: &regex::Regex,
    opts: &ScanOpts,
    cap: usize,
    acc: &mut ScanAcc,
) -> bool {
    let matches = || -> Box<dyn Iterator<Item = regex::Match<'_>> + '_> {
        match &opts.word_regex {
            Some(word) => Box::new(word.matches(body, lang, spans)),
            None => Box::new(re.find_iter(body)),
        }
    };
    let filtering = opts.in_filter != InFilter::Any;
    let accept = |off: usize| !filtering || opts.in_filter.accepts(class_at(spans, off));
    // A trailing newline ends the last line. An empty match sitting after it
    // (`^$`, `.*`) is on no line, so grep does not report it either.
    let ends_in_newline = body.is_empty() || body.ends_with('\n');
    let bounded = |m: &regex::Match<'_>| {
        if ends_in_newline && m.start() == body.len() {
            return false;
        }
        !opts.identifier_boundary
            || has_identifier_boundaries(
                body.as_bytes(),
                m.start(),
                m.end(),
                lang,
                class_at(spans, m.start()) == Some(Span::Str),
            )
    };
    // Line starts are only needed once there's a match (Files mode never needs
    // them). Compute lazily, so a non-matching candidate — and every candidate
    // in Files mode — skips the full-body newline scan.
    let mut starts: Option<Vec<usize>> = None;

    if opts.invert {
        return scan_inverted(
            path,
            body,
            load_defs,
            opts,
            cap,
            acc,
            |m| bounded(m) && accept(m.start()),
            matches(),
        );
    }

    match opts.out {
        OutMode::Files | OutMode::FilesWithout => {
            let found = matches().any(|m| bounded(&m) && accept(m.start()));
            if found == (opts.out == OutMode::Files) {
                acc.files.push(path.to_string());
                if acc.files.len() >= cap {
                    acc.truncated = true;
                    return true;
                }
            }
        }
        OutMode::Count => {
            let mut last = usize::MAX;
            let mut n = 0usize;
            for m in matches() {
                if !bounded(&m) {
                    continue;
                }
                if !accept(m.start()) {
                    continue;
                }
                let s = starts.get_or_insert_with(|| line_starts_of(body));
                let li = s.partition_point(|&x| x <= m.start()) - 1;
                if li != last {
                    last = li;
                    n += 1;
                }
            }
            if n > 0 {
                acc.counts.push(FileCount {
                    path: path.to_string(),
                    count: n,
                });
                if acc.counts.len() >= cap {
                    acc.truncated = true;
                    return true;
                }
            }
        }
        OutMode::Matches => {
            let bytes = body.as_bytes();
            let mut defs: Option<DefIndex> = None;
            let mut last_line = usize::MAX;
            let mut kept_here = 0usize;
            let mut stop = false;
            for m in matches() {
                if !bounded(&m) {
                    continue;
                }
                let off = m.start();
                if !accept(off) {
                    continue;
                }
                // With a parse of this file, "is this a call" and "what is it
                // called on" are answered by the tree. Without one (no grammar
                // for the language), fall back to the byte-level tests rather
                // than drop every hit.
                match calls {
                    Some(map) => {
                        if opts.call_only
                            && !map.contains_key(&off)
                            && !is_markup_instantiation(bytes, off)
                        {
                            continue;
                        }
                        if let Some(recv) = &opts.qualifier {
                            if map.get(&off).and_then(|r| r.as_deref()) != Some(recv.as_str()) {
                                continue;
                            }
                        }
                    }
                    None => {
                        // The byte-level fallback takes the match *start* as well
                        // as its end: a Svelte/Vue component is invoked as
                        // `<Name …/>`, which is only detectable by looking left of
                        // the name.
                        if opts.call_only && !is_call_position(bytes, m.start(), m.end()) {
                            continue;
                        }
                        if let Some(recv) = &opts.qualifier {
                            if !has_qualifier(bytes, off, recv, lang) {
                                continue;
                            }
                        }
                    }
                }
                let s = starts.get_or_insert_with(|| line_starts_of(body));
                let line_idx = s.partition_point(|&x| x <= off) - 1;
                if line_idx == last_line && !opts.only_matching {
                    continue; // one row per line
                }
                last_line = line_idx;

                let d = defs.get_or_insert_with(|| load_defs(path));
                let enclosing = d.innermost(off);
                acc.hits.push(Hit {
                    path: path.to_string(),
                    line: line_idx + 1,
                    col: off - s[line_idx],
                    byte: off,
                    symbol: enclosing.map(|d| d.name.clone()),
                    receiver: calls.and_then(|m| m.get(&off).cloned()).flatten(),
                    kind: enclosing.map(|d| d.kind.clone()),
                    decl: enclosing
                        .is_some_and(|d| s.partition_point(|&x| x <= d.start_byte) - 1 == line_idx),
                    text: if opts.only_matching {
                        cap_str(m.as_str().lines().next().unwrap_or(""), MAX_LINE)
                    } else {
                        line_text(body, s, line_idx)
                    },
                    before: context_lines(body, s, line_idx.saturating_sub(opts.before)..line_idx),
                    after: context_lines(body, s, (line_idx + 1)..(line_idx + 1 + opts.after)),
                });
                // Past this file's cap the hit is carried for its count only, so
                // it must not consume `limit` — otherwise the survey stops early
                // on rows nobody will ever see.
                if opts.per_file.is_none_or(|n| kept_here < n) {
                    kept_here += 1;
                    acc.kept += 1;
                    if acc.kept >= cap {
                        acc.truncated = true;
                        // Under a per-file cap, stopping mid-file would report
                        // this file's partial count as its total — the one file
                        // in the survey that lies. Finish counting it, then stop.
                        if opts.per_file.is_none() {
                            return true;
                        }
                        stop = true;
                    }
                }
            }
            if stop {
                return true;
            }
        }
    }
    false
}

/// `-v`: report the lines of `body` that hold no accepted match. A line is the
/// unit, as in grep, so `-c` counts those lines and `-l`/`-L` ask whether the
/// file has any.
#[allow(clippy::too_many_arguments)]
fn scan_inverted<'a>(
    path: &str,
    body: &str,
    load_defs: &mut dyn FnMut(&str) -> DefIndex,
    opts: &ScanOpts,
    cap: usize,
    acc: &mut ScanAcc,
    accepted: impl Fn(&regex::Match<'_>) -> bool,
    matches: impl Iterator<Item = regex::Match<'a>>,
) -> bool {
    let starts = line_starts_of(body);
    let n = line_count(body, &starts);
    let mut matched = vec![false; starts.len()];
    for m in matches.filter(|m| accepted(m)) {
        // A match spanning lines takes every line it touches out of the result.
        let first = starts.partition_point(|&x| x <= m.start()) - 1;
        let last = starts.partition_point(|&x| x < m.end().max(m.start() + 1)) - 1;
        matched[first..=last].iter_mut().for_each(|b| *b = true);
    }
    let free = (0..n).filter(|&i| !matched[i]);
    match opts.out {
        OutMode::Files | OutMode::FilesWithout => {
            if (free.count() > 0) == (opts.out == OutMode::Files) {
                acc.files.push(path.to_string());
                if acc.files.len() >= cap {
                    acc.truncated = true;
                    return true;
                }
            }
        }
        OutMode::Count => {
            let count = free.count();
            if count > 0 {
                acc.counts.push(FileCount {
                    path: path.to_string(),
                    count,
                });
                if acc.counts.len() >= cap {
                    acc.truncated = true;
                    return true;
                }
            }
        }
        OutMode::Matches => {
            let mut defs: Option<DefIndex> = None;
            let mut kept_here = 0usize;
            let mut stop = false;
            for i in free {
                let d = defs.get_or_insert_with(|| load_defs(path));
                let enclosing = d.innermost(starts[i]);
                acc.hits.push(Hit {
                    path: path.to_string(),
                    line: i + 1,
                    col: 0,
                    byte: starts[i],
                    symbol: enclosing.map(|d| d.name.clone()),
                    receiver: None,
                    kind: enclosing.map(|d| d.kind.clone()),
                    decl: false,
                    text: line_text(body, &starts, i),
                    before: context_lines(body, &starts, i.saturating_sub(opts.before)..i),
                    after: context_lines(body, &starts, (i + 1)..(i + 1 + opts.after)),
                });
                // The same two budgets as the ordinary scan: see there.
                if opts.per_file.is_none_or(|n| kept_here < n) {
                    kept_here += 1;
                    acc.kept += 1;
                    if acc.kept >= cap {
                        acc.truncated = true;
                        if opts.per_file.is_none() {
                            return true;
                        }
                        stop = true;
                    }
                }
            }
            return stop;
        }
    }
    false
}

/// The label used for the piped buffer where a result would otherwise carry a
/// path. Parenthesized like `map`'s `(root)`, so it can't be mistaken for a
/// file named `stdin` and stays greppable.
pub(crate) const STDIN_LABEL: &str = "(stdin)";

/// Scan a buffer piped in on stdin instead of the index.
///
/// Never inferred from whether stdin is a TTY: an agent's shell usually has
/// stdin closed or on /dev/null, so auto-detection would silently turn every
/// ordinary `codeq search` into a search of an empty buffer. The caller must
/// opt in explicitly, and this is the only way to reach this function.
///
/// `language` is what buys anything over grep: with it, the buffer is parsed
/// once with Tree-sitter, which gives `--in code|comments|strings` and the
/// enclosing `[symbol]` on each match. Without it the scan is still exact, but
/// those two are unavailable, and `--in` is rejected rather than silently
/// ignored.
pub(crate) fn scan_stdin(
    re: &regex::Regex,
    opts: &ScanOpts,
    language: Option<&str>,
    label: &str,
) -> Result<SearchData> {
    // Nothing is piped in, so reading would block forever. grep would hang here;
    // for an agent a hang is worse than an error, so say what went wrong.
    if std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "--stdin given but nothing is piped in (stdin is a terminal)"
        ));
    }
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf)?;
    // Lossy rather than an error: piped input is often a log or a mixed-encoding
    // file, and a stray byte shouldn't lose the whole search.
    let body = String::from_utf8_lossy(&buf).into_owned();

    let filtering = opts.in_filter != InFilter::Any;
    let (mut defs, spans, sites) = match language {
        Some(lang) => {
            if filtering && !has_spans(lang) {
                return Err(anyhow!(
                    "--in needs comment/string spans, which codeq does not extract for '{lang}'"
                ));
            }
            let mut ex = Extractor::default();
            // No path: stdin has no filename. Only single-file component
            // formats use it (to name the component after its file), and a
            // piped Svelte buffer has no file to be named after.
            let ((facts, spans, _), _, sites) = ex.extract(lang, "", body.as_bytes())?;
            let defs: DefIndex = facts
                .into_iter()
                .filter(|f| f.kind != "import")
                .map(|f| FileDef {
                    name: f.name,
                    kind: f.kind,
                    start_byte: f.start_byte,
                    end_byte: f.end_byte,
                })
                .collect::<Vec<_>>()
                .into();
            (defs, spans, sites)
        }
        None => {
            if filtering {
                return Err(anyhow!(
                    "--in needs to know the language of piped input: add --language <lang>"
                ));
            }
            (DefIndex::default(), Vec::new(), Vec::new())
        }
    };

    let cap = if opts.limit == 0 {
        usize::MAX
    } else if opts.out == OutMode::Matches && opts.per_file.is_some() {
        opts.limit
    } else {
        opts.limit.saturating_add(1)
    };
    let calls = if opts.call_only || opts.qualifier.is_some() {
        language
            .filter(|l| crate::callsite::supports_syntax_calls(l))
            .map(|_| {
                sites
                    .into_iter()
                    .map(|site| (site.start_byte, site.receiver))
                    .collect::<CallMap>()
            })
    } else {
        None
    };
    let mut acc = ScanAcc::default();
    let mut load_defs = |_: &str| std::mem::take(&mut defs);
    scan_doc(
        label,
        language,
        &body,
        &spans,
        calls.as_ref(),
        &mut load_defs,
        re,
        opts,
        cap,
        &mut acc,
    );
    Ok(acc.finish(
        opts.out,
        opts.limit,
        opts.out == OutMode::Matches && opts.per_file.is_some(),
    ))
}

/// Most trigrams one literal contributes to the prefilter. Any subset of a
/// literal's trigrams is still a superset filter, and past a few dozen each
/// extra one costs a posting-list intersection to exclude almost nothing.
const MAX_TRIGRAMS: usize = 32;

/// The content-index clause for one literal: an AND of its trigrams, each a
/// quoted FTS5 string so the tokenizer folds it exactly as it folded the file.
/// A file containing the literal contains every one of its trigrams, so the
/// clause never drops a match; a file holding them all apart is a false
/// candidate the real matcher rejects.
///
/// `None` when the literal cannot be prefiltered and the caller must full-scan:
/// shorter than a trigram, or holding a NUL, at which the tokenizer stops.
pub(crate) fn trigram_clause(lit: &str) -> Option<String> {
    let chars: Vec<char> = lit.chars().collect();
    if chars.len() < 3 || chars.contains(&'\0') {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let terms: Vec<String> = chars
        .windows(3)
        .filter(|w| seen.insert(w.to_vec()))
        .take(MAX_TRIGRAMS)
        .map(|w| {
            let tri: String = w.iter().collect();
            format!("\"{}\"", tri.replace('"', "\"\""))
        })
        .collect();
    Some(format!("({})", terms.join(" AND ")))
}

/// A single-literal prefilter (for `--fixed` search and `references`).
pub(crate) fn prefilter_of(lit: &str) -> Option<Vec<String>> {
    trigram_clause(lit).map(|p| vec![p])
}

/// Build a prefilter for a regex: a set of literals such that *every* match must
/// contain at least one of them (as a substring), OR-ed as trigram clauses. Uses
/// regex-syntax prefix/suffix literal extraction, then an inner-literal fallback.
/// Returns `None` when no safe prefilter exists — the caller then full-scans,
/// which is always correct.
///
/// Correctness: a finite prefix (or suffix) literal set means every match starts
/// (or ends) with one of the literals, so it contains one as a substring; the
/// OR of their clauses is a true superset and never drops a match.
pub(crate) fn regex_prefilter(pattern: &str) -> Option<Vec<String>> {
    let hir = regex_syntax::parse(pattern).ok()?;
    usable_literals(ExtractKind::Prefix, &hir)
        .or_else(|| usable_literals(ExtractKind::Suffix, &hir))
        .or_else(|| inner_required_literal(&hir))
}

/// The longest literal that must appear in *every* match, found by walking the
/// HIR. This catches a literal buried in the middle (e.g. `.*needle.*`) that
/// prefix/suffix extraction misses. A required literal is a safe single-term
/// prefilter; when there isn't one >= 3 chars we return None and full-scan.
fn inner_required_literal(hir: &regex_syntax::hir::Hir) -> Option<Vec<String>> {
    fn flush(cur: &mut Vec<u8>, best: &mut Vec<u8>) {
        if cur.len() > best.len() {
            *best = cur.clone();
        }
        cur.clear();
    }
    // Accumulate contiguous literals; anything that isn't an unconditional
    // literal (a class, an optional/zero repetition, an alternation, a look)
    // breaks the run. Required containers (concat, capture, min>=1 repetition)
    // are walked so their literals count too.
    fn walk(hir: &regex_syntax::hir::Hir, cur: &mut Vec<u8>, best: &mut Vec<u8>) {
        use regex_syntax::hir::HirKind;
        match hir.kind() {
            HirKind::Literal(lit) => cur.extend_from_slice(&lit.0[..]),
            HirKind::Concat(subs) => {
                for s in subs {
                    walk(s, cur, best);
                }
            }
            HirKind::Capture(c) => walk(&c.sub, cur, best),
            HirKind::Repetition(r) if r.min >= 1 => {
                flush(cur, best);
                walk(&r.sub, cur, best);
                flush(cur, best);
            }
            _ => flush(cur, best),
        }
    }
    let mut best = Vec::new();
    let mut cur = Vec::new();
    walk(hir, &mut cur, &mut best);
    flush(&mut cur, &mut best);
    let s = std::str::from_utf8(&best).ok()?;
    trigram_clause(s).map(|p| vec![p])
}

fn usable_literals(kind: ExtractKind, hir: &regex_syntax::hir::Hir) -> Option<Vec<String>> {
    let mut ex = LiteralExtractor::new();
    ex.kind(kind);
    let seq = ex.extract(hir);
    let lits = seq.literals()?;
    if lits.is_empty() || lits.len() > 64 {
        return None;
    }
    let mut terms = Vec::with_capacity(lits.len());
    for lit in lits {
        let s = std::str::from_utf8(lit.as_bytes()).ok()?;
        // Any alternative shorter than a trigram is unfilterable, which would
        // make the whole OR unsafe — bail to a full scan instead.
        terms.push(trigram_clause(s)?);
    }
    Some(terms)
}

/// Truncate `s` to `max` chars, appending an ellipsis when cut.
fn cap_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::{DefIndex, FileDef};

    fn linear(defs: &[FileDef], off: usize) -> Option<usize> {
        defs.iter()
            .enumerate()
            .filter(|(_, d)| d.start_byte <= off && off < d.end_byte)
            .max_by_key(|(_, d)| d.start_byte)
            .map(|(i, _)| i)
    }

    /// Deterministic xorshift, so the test needs no RNG dependency.
    fn next(state: &mut u64) -> usize {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 33) as usize
    }

    #[test]
    fn def_index_agrees_with_a_linear_scan() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for count in [0usize, 1, 5, 23, 24, 25, 100, 400] {
            for _ in 0..8 {
                // Nested, overlapping, adjacent and equal-start ranges, in
                // arbitrary load order: the tie rule is what is being checked.
                let defs: Vec<FileDef> = (0..count)
                    .map(|i| {
                        let start = next(&mut state) % 500;
                        let end = start + next(&mut state) % 120;
                        FileDef {
                            name: format!("d{i}"),
                            kind: "function".into(),
                            start_byte: start,
                            end_byte: end,
                        }
                    })
                    .collect();
                let expected: Vec<Option<String>> = (0..640)
                    .map(|off| linear(&defs, off).map(|i| defs[i].name.clone()))
                    .collect();
                let index = DefIndex::from(defs);
                for (off, want) in expected.iter().enumerate() {
                    let got = index.innermost(off).map(|d| d.name.clone());
                    assert_eq!(&got, want, "count {count} offset {off}");
                }
            }
        }
    }
}

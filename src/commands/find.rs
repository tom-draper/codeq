//! Text and syntax search: `search`, `references`, `astgrep`.

use super::*;

pub fn cmd_astgrep(
    cwd: &Path,
    pattern: &str,
    language: &str,
    path: Option<String>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // Gather the indexed source of every file in the requested language (the
    // stored FTS body is the whole file). Structural search re-parses each.
    //
    // The path filter is applied in Rust, NOT as `AND path LIKE ?` in this query.
    // `content_fts` is an FTS5 table with `tokenize = 'trigram'`, and the trigram
    // tokenizer *takes over* `LIKE` on the table — so a `LIKE` against `path`,
    // which is `UNINDEXED` and therefore has no trigram data, matches nothing at
    // all. Every `--path` form returned an empty result with exit 0: not a
    // narrowed answer, an emptied one. It looked like "astgrep can't express
    // that", which is what the reporter concluded before giving up on the
    // command. `search` was never affected because it LIKEs `body` (which *is*
    // the indexed column) and filters paths in Rust, as here.
    let mut stmt =
        s.db.conn
            .prepare("SELECT path, body FROM content_fts WHERE language = ?1 ORDER BY path")?;
    let mut scanned = 0usize;
    // Stream the bodies into the matcher one at a time (it re-parses each and
    // keeps only matches), so a big single-language repo isn't all held in memory
    // at once — the astgrep matcher takes an iterator, so no Vec is needed.
    let files = stmt
        .query_map([&language], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .filter(|(p, _): &(String, String)| path.as_deref().is_none_or(|q| p.contains(q)))
        .inspect(|_| scanned += 1);

    let mut matches = crate::astgrep::search(language, pattern, files)?;
    let truncated = limit > 0 && matches.len() > limit;
    if truncated {
        matches.truncate(limit);
    }

    // Annotate each match with the definition it sits inside (like `search`), so
    // the reader needn't a follow-up `at`. Matches are already sorted by path, so
    // load each file's defs once.
    let mut def_stmt = s.db.conn.prepare(
        "SELECT name, kind, start_byte, end_byte FROM facts
         WHERE path = ?1 AND kind <> 'import'",
    )?;
    let mut cur_path: Option<String> = None;
    let mut defs: Vec<FileDef> = Vec::new();

    #[derive(Serialize)]
    struct AstRow {
        path: String,
        line: usize,
        col: usize,
        end_line: usize,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
        #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
        binds: serde_json::Map<String, serde_json::Value>,
    }
    #[derive(Serialize)]
    struct AstData {
        matches: Vec<AstRow>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    }
    let mut rows: Vec<AstRow> = Vec::with_capacity(matches.len());
    for m in matches {
        if cur_path.as_deref() != Some(m.path.as_str()) {
            defs = load_file_defs(&mut def_stmt, &m.path)?;
            cur_path = Some(m.path.clone());
        }
        let symbol = innermost(&defs, m.start_byte).map(|d| d.name.clone());
        let binds = m
            .binds
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::from(v)))
            .collect();
        rows.push(AstRow {
            path: m.path,
            line: m.start_line,
            col: m.start_col,
            end_line: m.end_line,
            text: m.text,
            symbol,
            binds,
        });
    }
    // An empty result has two very different causes, and they used to look
    // identical: the pattern matched nothing, or `--path` selected no files at
    // all. The second is the one that wastes an afternoon, so name it.
    let note = if rows.is_empty() {
        match path.as_deref() {
            Some(p) if scanned == 0 => Some(format!(
                "no indexed {language} file matches --path {p}; the pattern was never run"
            )),
            // The pattern parsed and ran and found nothing. Silence would be
            // right for `search`, where an empty grep is unambiguous — but
            // astgrep is structurally exact, and its own docs concede that a
            // wrong node kind and a genuine miss look identical. Point at the
            // command that tells them apart.
            _ => Some(format!(
                "pattern ran over {scanned} {language} file(s) and matched nothing; if it should have matched, `astgrep --explain <file>:<line>` shows the node kinds to write it against"
            )),
        }
    } else {
        None
    };
    s.emit_noted(
        AstData {
            matches: rows,
            truncated,
        },
        note,
    )
}

/// `astgrep --explain <file>:<line>` — the parse tree covering one line: the node
/// kinds and field names a pattern has to be written against.
///
/// The gap this closes: `astgrep` demands exact node kinds and codeq offered no
/// way to learn them, so writing a pattern meant leaving codeq for a tree-sitter
/// playground — and a wrong guess comes back as an empty result that reads
/// exactly like "no matches".
pub fn cmd_astgrep_explain(cwd: &Path, location: &str, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let (path, line_str) = location
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("expected <path>:<line>, got '{location}'"))?;
    let line: usize = line_str
        .parse()
        .map_err(|_| anyhow!("invalid line number in '{location}'"))?;
    if line == 0 {
        return Err(anyhow!("line numbers start at 1"));
    }
    let rel = resolve_file(&s, path)?;

    // Language and body both come from the index, so `--explain` needs no
    // `--language`: the file already knows what it is.
    let (language, body): (Option<String>, String) = s.db.conn.query_row(
        "SELECT language, body FROM content_fts WHERE path = ?1",
        [&rel],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let language = language.ok_or_else(|| {
        anyhow!("'{rel}' has no language codeq classifies, so it has no parse tree")
    })?;
    if crate::astgrep::ts_language(&language).is_none() {
        return Err(anyhow!(
            "no grammar for '{language}', so `astgrep` cannot match in '{rel}' either \
             — see `codeq capabilities`"
        ));
    }

    #[derive(Serialize)]
    struct ExplainRow {
        path: String,
        line: usize,
        col: usize,
        depth: usize,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        field: Option<String>,
        /// The source this node covers. Deliberately not `text`: that key means
        /// "this row *is* the matched source" to the renderer, and here the
        /// payload is the node kind — the preview is context for it.
        src: String,
    }

    let nodes = crate::astgrep::explain(&language, &body, line)?;
    let data: Vec<ExplainRow> = nodes
        .into_iter()
        .map(|n| ExplainRow {
            path: rel.clone(),
            line: n.line,
            col: n.col,
            depth: n.depth,
            kind: n.kind,
            field: n.field,
            src: n.text,
        })
        .collect();
    let note = data.is_empty().then(|| {
        format!("'{rel}' has no parse tree covering line {line} (past the end of the file?)")
    });
    s.emit_noted(data, note)
}

/// Include/exclude glob filter over repo-relative paths, ripgrep-style: a `!`
/// prefix excludes; a pattern with no `/` matches by basename in any directory;
/// a trailing `/` matches a whole directory.
struct GlobFilter {
    includes: globset::GlobSet,
    excludes: globset::GlobSet,
    has_includes: bool,
}

impl GlobFilter {
    fn build(globs: &[String]) -> Result<GlobFilter> {
        let mut inc = globset::GlobSetBuilder::new();
        let mut exc = globset::GlobSetBuilder::new();
        let mut has_includes = false;
        for g in globs {
            let (neg, pat) = match g.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, g.as_str()),
            };
            let normalized = Self::normalize(pat);
            let glob = globset::GlobBuilder::new(&normalized)
                .literal_separator(true)
                .build()
                .map_err(|e| anyhow!("invalid glob '{g}': {e}"))?;
            if neg {
                exc.add(glob);
            } else {
                inc.add(glob);
                has_includes = true;
            }
        }
        Ok(GlobFilter {
            includes: inc.build()?,
            excludes: exc.build()?,
            has_includes,
        })
    }

    /// A bare name (no `/`) matches in any directory; a trailing `/` means a
    /// whole directory subtree.
    fn normalize(pat: &str) -> String {
        let base = if let Some(dir) = pat.strip_suffix('/') {
            format!("{dir}/**")
        } else {
            pat.to_string()
        };
        if base.contains('/') {
            base
        } else {
            format!("**/{base}")
        }
    }

    fn accepts(&self, path: &str) -> bool {
        if self.excludes.is_match(path) {
            return false;
        }
        !self.has_includes || self.includes.is_match(path)
    }
}

/// Options for the `search` command surface, kept in a struct so the CLI can
/// grow without a huge argument list.
pub struct SearchArgs<'a> {
    pub pattern: &'a str,
    /// Positional paths, as grep takes them: `codeq search TODO src`. Matched as
    /// path substrings (the same rule as `path`), OR'd together.
    pub paths: &'a [String],
    pub fixed: bool,
    pub ignore_case: bool,
    pub word: bool,
    pub path: Option<&'a str>,
    pub language: Option<&'a str>,
    pub globs: &'a [String],
    pub in_kind: Option<&'a str>,
    pub before: usize,
    pub after: usize,
    pub files_with_matches: bool,
    pub count: bool,
    pub limit: usize,
    /// Search a buffer piped in on stdin instead of the repository index.
    /// Explicit only, never inferred from whether stdin is a TTY.
    pub stdin: bool,
}

/// `search <pattern>` — content search over indexed, non-binary files, with each
/// match annotated by its enclosing definition. Regex by default (linear-time,
/// no catastrophic backtracking); `--fixed` for a literal string. A strict
/// superset of grep: same regex engine, plus structure grep cannot see.
pub fn cmd_search(cwd: &Path, args: SearchArgs, out: Out) -> Result<()> {
    let expr = if args.fixed {
        regex::escape(args.pattern)
    } else {
        args.pattern.to_string()
    };
    // Match line-by-line semantics like grep/ripgrep: the scan runs over each
    // whole file body, so `^`/`$` must anchor to line boundaries, not the start
    // and end of the buffer. Without this, `^use` matches only a file that
    // begins with `use` (near-zero) instead of every line starting with it.
    let re = regex::RegexBuilder::new(&expr)
        .case_insensitive(args.ignore_case)
        .multi_line(true)
        .build()
        .map_err(|e| anyhow!("invalid pattern: {e}"))?;

    // Prefilter candidate files via the trigram index: a fixed search by its
    // whole literal, a regex by its required literals. Either may be None (no
    // usable >=3-char literal), in which case we scan every body — still from
    // the index, never the filesystem, and always correct.
    let match_query = if args.fixed {
        like_of(args.pattern)
    } else {
        regex_prefilter(args.pattern)
    };
    // `--glob` and grep-style positional paths both become per-row path
    // predicates for the streaming scan, AND'd together: `-g` says which files
    // are eligible by shape, a positional says which by location.
    let glob = if args.globs.is_empty() {
        None
    } else {
        Some(GlobFilter::build(args.globs)?)
    };

    let out_mode = if args.files_with_matches {
        OutMode::Files
    } else if args.count {
        OutMode::Count
    } else {
        OutMode::Matches
    };
    let in_filter = match args.in_kind {
        Some(k) => InFilter::parse(k)?,
        None => InFilter::Any,
    };

    let opts = ScanOpts {
        before: args.before,
        after: args.after,
        out: out_mode,
        in_filter,
        limit: args.limit,
        call_only: false,
        qualifier: None,
        identifier_boundary: args.word,
        per_file: out.max_per_file,
    };

    // Piped input never touches the index: no repo discovery, no refresh, no
    // database. That keeps codeq usable as a plain filter outside a repository,
    // and keeps this branch entirely off the indexed path the MCP server uses.
    if args.stdin {
        let data = scan_stdin(&re, &opts, args.language, search::STDIN_LABEL)?;
        return emit_sessionless(&data, out);
    }

    let s = Session::open(cwd, false, out)?;
    let scoped = args.path.is_some() || !args.paths.is_empty() || args.language.is_some();

    // A positional is a path substring over the index. One that selects no
    // indexed file is either a real path the index does not hold — gitignored
    // build output, a log, a file outside the repository — or a mistake. grep
    // would search the first, so codeq does too, from disk; either way the
    // caller is told, because an empty answer here used to read as "no match"
    // when the pattern had never been run.
    let mut paths: Vec<String> = Vec::new();
    let mut unindexed: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    let root = s
        .repo
        .root
        .canonicalize()
        .unwrap_or_else(|_| s.repo.root.clone());
    let mut indexed =
        s.db.conn
            .prepare("SELECT EXISTS(SELECT 1 FROM files WHERE instr(path, ?1) > 0)")?;
    for given in args.paths {
        // A path that exists inside the repository is matched by its
        // repo-relative form, so `./src`, an absolute path and a path given
        // from a subdirectory all select what they name.
        let on_disk = cwd.join(given).canonicalize().ok();
        let rel = on_disk
            .as_deref()
            .and_then(|p| p.strip_prefix(&root).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| given.clone());
        if indexed.query_row([&rel], |r| r.get::<_, bool>(0))? {
            paths.push(rel);
        } else if let Some(p) = on_disk {
            unindexed.push((given.clone(), p));
        } else {
            missing.push(given);
        }
    }
    drop(indexed);

    let skip_index = !args.paths.is_empty() && paths.is_empty();
    let accept = (glob.is_some() || !args.paths.is_empty()).then_some({
        |p: &str| {
            glob.as_ref().is_none_or(|gf| gf.accepts(p))
                && (args.paths.is_empty() || paths.iter().any(|q| p.contains(q.as_str())))
        }
    });
    let path_accept = accept.as_ref().map(|f| f as &dyn Fn(&str) -> bool);
    let (mut acc, stopped) = if skip_index {
        (search::ScanAcc::default(), false)
    } else {
        run_search_acc(
            &s.db.conn,
            &re,
            match_query.as_deref(),
            args.path,
            args.language,
            path_accept,
            &opts,
        )?
    };
    // Nothing was parsed for these files, so `--in` has no spans to classify
    // with and `--language` no language to compare: both exclude them.
    let classified = in_filter != InFilter::Any || args.language.is_some();
    if !stopped && !classified {
        scan_unindexed(&unindexed, &re, &opts, &mut acc);
    }
    let data = acc.finish_for(&opts);
    let quoted = |ps: &mut dyn Iterator<Item = &str>| {
        ps.map(|p| format!("'{p}'")).collect::<Vec<_>>().join(", ")
    };
    let mut path_notes: Vec<String> = Vec::new();
    if !unindexed.is_empty() {
        let which = quoted(&mut unindexed.iter().map(|(l, _)| l.as_str()));
        path_notes.push(if classified {
            format!("{which} not in the index (ignored or outside the repo), so --in/--language cannot apply — not searched")
        } else {
            format!("{which} not in the index (ignored or outside the repo) — searched from disk, without [symbol]")
        });
    }
    if !missing.is_empty() {
        path_notes.push(format!(
            "{} matches no indexed file and no path on disk — the pattern was never run there",
            quoted(&mut missing.iter().copied())
        ));
    }
    // A bare pattern miss stays silent — an empty `search zzz` is unambiguous and
    // a note on every one would be noise. What is ambiguous is a *filter* that
    // selected no file: then the pattern never ran, and "no matches" is a
    // statement about the filter rather than about the code.
    let note = if scoped && data.is_empty() {
        let mut stmt = s.db.conn.prepare(
            "SELECT COUNT(*) FROM files WHERE (?1 IS NULL OR path LIKE ?2 ESCAPE '\\') AND (?3 IS NULL OR language = ?3)",
        )?;
        let like = args.path.map(|p| format!("%{}%", escape_like_literal(p)));
        let n: i64 = stmt.query_row(rusqlite::params![args.path, like, args.language], |r| {
            r.get(0)
        })?;
        (n == 0).then(|| {
            let mut which: Vec<String> = Vec::new();
            if let Some(p) = args.path {
                which.push(format!("--path {p}"));
            }
            if let Some(l) = args.language {
                which.push(format!("--language {l}"));
            }
            format!(
                "no indexed file matches {} — the pattern was never run",
                which.join(" and ")
            )
        })
    } else {
        None
    };
    let note = note.or((!path_notes.is_empty()).then(|| path_notes.join("; ")));
    s.emit_noted(data, note)
}

#[derive(Serialize)]
struct ReferenceRow {
    #[serde(flatten)]
    hit: Hit,
    /// True when this line is the definition site of the symbol (not a use).
    #[serde(skip_serializing_if = "is_false")]
    definition: bool,
    /// Import-scoped attribution (only under `--resolved`): same-file / imported
    /// / ambiguous.
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
}

#[derive(Serialize)]
struct ReferencesResult {
    references: Vec<ReferenceRow>,
    truncated: bool,
}

/// Arguments of `references`.
pub struct ReferencesArgs<'a> {
    pub name: &'a str,
    pub path: Option<&'a str>,
    pub language: Option<&'a str>,
    pub limit: usize,
    pub resolved: bool,
    pub call_only: bool,
    pub qualifier: Option<&'a str>,
}

/// `references <name>` — word-boundary occurrences of an exact symbol name,
/// each annotated with its enclosing definition, and definition sites flagged.
/// These are *textual* references (not type-resolved) — honest and deterministic,
/// but far more precise than a substring grep.
pub fn cmd_references(cwd: &Path, args: ReferencesArgs, out: Out) -> Result<()> {
    let ReferencesArgs { name, path, language, limit, resolved, call_only, qualifier } = args;
    let per_file = out.max_per_file;
    let s = Session::open(cwd, false, out)?;

    let expr = regex::escape(name);
    let re = regex::Regex::new(&expr).map_err(|e| anyhow!("invalid name: {e}"))?;
    // The name is a substring of every reference, so the trigram index narrows
    // to files that contain it (the \b matcher then confirms word boundaries).
    let match_query = like_of(name);
    // A call site or qualified access is *code* — a doc-comment example like
    // `fn build()` is not one — so the position filters imply code-only.
    let in_filter = if call_only || qualifier.is_some() {
        InFilter::Code
    } else {
        InFilter::Any
    };
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter,
        // Include alias hits below, then apply one global cap to the combined
        // result. Per-alias caps would make --limit depend on import spelling.
        limit: 0,
        call_only,
        qualifier: qualifier.map(str::to_string),
        identifier_boundary: true,
        per_file,
    };
    let (mut hits, mut truncated) = match run_search(
        &s.db.conn,
        &re,
        match_query.as_deref(),
        path,
        language,
        None,
        &opts,
    )? {
        SearchData::Matches { matches, truncated } => (matches, truncated),
        _ => unreachable!("references always uses Matches mode"),
    };
    hits.extend(alias_hits(&s, name, path, language, &opts)?);
    hits.sort_by(|a, b| (a.path.as_str(), a.line, a.col).cmp(&(b.path.as_str(), b.line, b.col)));
    if limit > 0 && hits.len() > limit {
        hits.truncate(limit);
        truncated = true;
    }

    // Where this exact name is declared. Hits are sorted above, which is what
    // `DeclSites` needs to tell a declaration from a recursive use.
    let mut defsites = DeclSites::load(&s.db.conn, name)?;

    let references: Vec<ReferenceRow> = hits
        .into_iter()
        .map(|h| {
            let definition = defsites.is_declaration(&h.path, h.line as i64);
            ReferenceRow {
                hit: h,
                definition,
                via: None,
            }
        })
        // A definition is not a call. `fn foo(` and `def foo(` are followed by
        // `(` just as a call is, so the byte-level test cannot tell them apart —
        // but the definition sites are already known exactly, so drop them.
        // Same for `--on`: a declaration is not a qualified use.
        .filter(|r| !((call_only || qualifier.is_some()) && r.definition))
        .collect();
    let references = resolve_rows(
        &s,
        name,
        resolved,
        references,
        |r| r.hit.path.as_str(),
        |r, v| r.via = v,
    )?;

    let note = note_if_empty(&s, &references, name, "references", path)?;
    s.emit_noted(
        ReferencesResult {
            references,
            truncated,
        },
        note,
    )
}

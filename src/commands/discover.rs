use super::*;
use std::collections::BTreeSet;

#[derive(Serialize)]
struct Candidate {
    name: String,
    kind: String,
    path: String,
    line: i64,
    byte: usize,
    end_byte: usize,
    score: usize,
    reasons: Vec<String>,
}

fn terms(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut split = String::new();
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_uppercase()
            && i > 0
            && (chars[i - 1].is_lowercase()
                || chars[i - 1].is_numeric()
                || (chars[i - 1].is_uppercase()
                    && chars.get(i + 1).is_some_and(|c| c.is_lowercase())))
        {
            split.push(' ');
        }
        split.push(ch);
    }
    split
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub fn cmd_discover(
    cwd: &Path,
    query: &str,
    path: Option<&str>,
    limit: usize,
    source: bool,
    max_tokens: u64,
    out: Out,
) -> Result<()> {
    let query_terms: BTreeSet<String> = terms(query)
        .into_iter()
        .filter(|t| {
            ![
                "a", "an", "the", "to", "for", "of", "in", "and", "with", "where", "how",
            ]
            .contains(&t.as_str())
        })
        .collect();
    if query_terms.is_empty() {
        return Err(anyhow!("discovery needs at least one searchable word"));
    }
    let s = Session::open(cwd, false, out)?;
    let mut stmt = s.db.conn.prepare(
        "SELECT name, kind, path, start_line, start_byte, end_byte, signature, doc
         FROM facts WHERE kind <> 'import' AND (?1 IS NULL OR instr(path, ?1) > 0)",
    )?;
    let mut candidates = Vec::new();
    let rows = stmt.query_map([path], |r| {
        Ok((
            Candidate {
                name: r.get(0)?,
                kind: r.get(1)?,
                path: r.get(2)?,
                line: r.get(3)?,
                byte: r.get(4)?,
                end_byte: r.get(5)?,
                score: 0,
                reasons: Vec::new(),
            },
            r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            r.get::<_, Option<String>>(7)?.unwrap_or_default(),
        ))
    })?;
    for row in rows {
        let (mut candidate, signature, doc) = row?;
        let fields = [
            ("name", &candidate.name, 8),
            ("signature", &signature, 4),
            ("docs", &doc, 2),
            ("path", &candidate.path, 1),
        ];
        let mut matched = BTreeSet::new();
        for (field, value, weight) in fields {
            let words: HashSet<String> = terms(value).into_iter().collect();
            for term in &query_terms {
                if words.contains(term) {
                    candidate.score += weight;
                    candidate.reasons.push(format!("{field}:{term}"));
                    matched.insert(term);
                }
            }
        }
        if !matched.is_empty() {
            candidate.score += matched.len() * 16;
            candidates.push(candidate);
        }
    }
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.path.cmp(&b.path))
            .then(a.byte.cmp(&b.byte))
            .then(a.name.cmp(&b.name))
    });
    let truncated = limit > 0 && candidates.len() > limit;
    if limit > 0 {
        candidates.truncate(limit);
    }
    let note = candidates
        .is_empty()
        .then(|| "no indexed definition matches the query words".to_string());
    if !source {
        return s.emit_noted(
            serde_json::json!({"candidates": candidates, "truncated": truncated}),
            note,
        );
    }
    let seeds: Vec<_> = candidates
        .iter()
        .map(|c| context::Seed {
            name: c.name.clone(),
            path: c.path.clone(),
            start_byte: Some(c.byte),
            role: "match",
            forced: false,
        })
        .collect();
    let assembled = context::source_for_ranked(&s.db, &s.repo, &seeds, max_tokens)?;
    let omitted_truncated = assembled.omitted_truncated;
    let (budget, source, omitted) = assembled.into_source();
    s.emit_noted(
        serde_json::json!({"candidates": candidates, "truncated": truncated,
        "source": source, "budget": budget, "omitted": omitted,
        "omitted_truncated": omitted_truncated}),
        note,
    )
}

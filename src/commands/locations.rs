//! Ordered location and stack-frame resolution for `at`.

use super::*;
use std::io::BufRead;

struct Location {
    path: String,
    line: i64,
}

#[derive(Serialize)]
struct Frame {
    name: String,
    status: &'static str,
    data: Vec<super::symbols::DefinitionRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Serialize)]
struct Locations {
    results: Vec<Frame>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<context::ContextResult>,
    ignored_lines: usize,
}

pub fn cmd_at_locations(
    cwd: &Path,
    locations: &[String],
    stdin: bool,
    source_budget: Option<u64>,
    out: Out,
) -> Result<()> {
    if !stdin && locations.len() == 1 && source_budget.is_none() {
        // Keep the established single-location result shape and lookup errors.
        let location = parse_location(&locations[0])?;
        return super::explore::cmd_at(cwd, &format!("{}:{}", location.path, location.line), out);
    }
    let mut inputs = Vec::new();
    let mut ignored_lines = 0;
    if stdin {
        let python = regex::Regex::new(r#"^\s*File "([^"]+)", line ([0-9]+)(?:,|$)"#)?;
        for line in std::io::stdin().lock().lines() {
            let line = line?;
            let trimmed = line.trim();
            if let Some(captures) = python.captures(trimmed) {
                inputs.push((
                    line.clone(),
                    parse_location(&format!("{}:{}", &captures[1], &captures[2])).ok(),
                ));
            } else if let Some(location) = stack_location(trimmed) {
                inputs.push((line.clone(), parse_location(location).ok()));
            } else if trimmed.starts_with("at ")
                && (trimmed.ends_with("(Native Method)") || trimmed.ends_with("(Unknown Source)"))
            {
                inputs.push((line, None));
            } else if !trimmed.is_empty() {
                ignored_lines += 1;
            }
        }
    } else {
        for input in locations {
            inputs.push((input.clone(), Some(parse_location(input)?)));
        }
    }
    if inputs.is_empty() {
        return Err(anyhow!("no locations recognized in input"));
    }
    let s = Session::open(cwd, false, out)?;
    let paths = all_file_paths(&s.db.conn)?;
    let mut results = Vec::new();
    let mut seeds = Vec::new();
    for (name, location) in inputs {
        let mut frame = Frame {
            name,
            status: "unresolved",
            data: Vec::new(),
            note: None,
        };
        if let Some(location) = location {
            match frame_path(&s, &paths, &location.path) {
                Ok(rel) => {
                    let (mut definitions, note) =
                        super::explore::at_definitions(&s, rel, location.line)?;
                    let selected =
                        super::explore::definition_at(&s, &format!("{rel}:{}", location.line))?;
                    definitions
                        .retain(|row| selected.start_bytes.contains(&(row.start_byte as usize)));
                    frame.status = if definitions.is_empty() {
                        "unresolved"
                    } else {
                        "resolved"
                    };
                    frame.data = definitions;
                    frame.note = note;
                    if source_budget.is_some() && frame.status == "resolved" {
                        for row in &frame.data {
                            seeds.push(context::Seed {
                                name: row.name.clone(),
                                path: rel.to_string(),
                                role: "frame",
                                forced: false,
                                start_byte: Some(row.start_byte as usize),
                            });
                        }
                    }
                }
                Err((status, note)) => {
                    frame.status = status;
                    frame.note = Some(note);
                }
            }
        } else {
            frame.note = Some("frame has no valid source location".to_string());
        }
        results.push(frame);
    }
    let source = source_budget
        .map(|budget| context::source_for_ranked(&s.db, &s.repo, &seeds, budget))
        .transpose()?;
    let mut notes: Vec<String> = results
        .iter()
        .filter_map(|frame| {
            frame
                .note
                .as_ref()
                .map(|note| format!("{}: {note}", frame.name.trim()))
        })
        .collect();
    if ignored_lines > 0 {
        notes.push(format!("ignored {ignored_lines} non-location input lines"));
    }
    s.emit_noted(
        Locations {
            results,
            source,
            ignored_lines,
        },
        (!notes.is_empty()).then(|| notes.join("; ")),
    )
}

fn parse_location(input: &str) -> Result<Location> {
    let (prefix, last) = input.rsplit_once(':').ok_or_else(|| {
        anyhow!("expected <path>:<line> or <path>:<line>:<column>, got '{input}'")
    })?;
    let (path, line) = match prefix.rsplit_once(':') {
        Some((path, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => {
            let column: u64 = last
                .parse()
                .map_err(|_| anyhow!("invalid column in '{input}'"))?;
            if column == 0 {
                return Err(anyhow!("column must be positive in '{input}'"));
            }
            (path, line)
        }
        _ => (prefix, last),
    };
    let line: i64 = line
        .parse()
        .map_err(|_| anyhow!("invalid line number in '{input}'"))?;
    if path.is_empty() || line <= 0 {
        return Err(anyhow!(
            "expected a nonempty path and positive line in '{input}'"
        ));
    }
    Ok(Location {
        path: path.replace('\\', "/"),
        line,
    })
}

fn stack_location(input: &str) -> Option<&str> {
    let input = input
        .strip_prefix("at ")
        .or_else(|| input.strip_prefix("--> "))
        .or_else(|| input.strip_prefix("::: "))
        .unwrap_or(input)
        .trim_start();
    let input = input
        .strip_suffix(')')
        .and_then(|frame| frame.split_once('('))
        .map_or(input, |(_, location)| location);
    // A compiler diagnostic ends the location with ': ', a frame ends at the column.
    let input = input
        .split_once(": ")
        .map_or(input, |(location, _)| location);
    let input = input.trim_end_matches(':');
    let (_, last) = input.rsplit_once(':')?;
    if last.is_empty() || !last.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let prefix = input.rsplit_once(':')?.0;
    if prefix.is_empty() || prefix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(input.strip_prefix("file://").unwrap_or(input))
}

fn frame_path<'a>(
    s: &Session,
    paths: &'a [String],
    path: &str,
) -> std::result::Result<&'a str, (&'static str, String)> {
    let path_arg = Path::new(path);
    let absolute = path_arg.is_absolute() || path.as_bytes().get(1) == Some(&b':');
    if !absolute {
        if let Some(exact) = paths.iter().find(|p| p.as_str() == path) {
            return Ok(exact);
        }
    }
    if let (Ok(file), Ok(root)) = (s.cwd.join(path).canonicalize(), s.repo.root.canonicalize()) {
        if let Ok(relative) = file.strip_prefix(root) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            if let Some(exact) = paths.iter().find(|p| p.as_str() == relative) {
                return Ok(exact);
            }
        } else {
            return Err(("unresolved", format!("outside this repository: {path}")));
        }
    }
    let suffix = format!("/{path}");
    let matches: Vec<&str> = if absolute {
        Vec::new()
    } else {
        paths
            .iter()
            .filter(|p| p.ends_with(&suffix))
            .map(String::as_str)
            .collect()
    };
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err((
            "unresolved",
            format!("file not found in this index: {path}"),
        )),
        _ => Err((
            "ambiguous",
            format!("'{path}' is ambiguous: {}", matches.join(", ")),
        )),
    }
}

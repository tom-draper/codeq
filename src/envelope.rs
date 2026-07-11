//! The `codeq.result.v1` output envelope.
//!
//! Designed for the actual consumer: an AI agent paying tokens per byte. So:
//!
//! - **Compact by default.** Pretty-printing is opt-in (`--meta`/`--pretty`);
//!   the default is single-line JSON.
//! - **Meta only when it carries signal.** Because codeq refreshes on every
//!   read, answers are almost always fresh — so `repo`/`index` blocks are
//!   omitted unless something is noteworthy (files reparsed, staleness, a
//!   partial failure) or the caller asks with `--meta`.
//! - **Projectable.** `--fields` drops columns the agent did not ask for.

use serde::Serialize;
use serde_json::Value;

pub const RESULT_SCHEMA: &str = "codeq.result.v1";

/// Output rendering options, threaded from the CLI.
#[derive(Clone, Copy, Default)]
pub struct Render<'a> {
    /// Force the full `repo`/`index` envelope even when nothing is noteworthy.
    pub meta: bool,
    /// Pretty-print (implies including meta for human reading).
    pub pretty: bool,
    /// If set, keep only these keys on each object in `data`.
    pub fields: Option<&'a [String]>,
}

pub struct Envelope<T: Serialize> {
    repo: RepoMeta,
    index: IndexMeta,
    data: T,
}

impl<T: Serialize> Envelope<T> {
    pub fn new(repo: RepoMeta, index: IndexMeta, data: T) -> Self {
        Envelope { repo, index, data }
    }

    /// Whether the meta blocks carry signal worth the tokens.
    fn noteworthy(&self) -> bool {
        self.index.reindexed_files > 0
            || !self.index.fresh
            || self.index.partial_failures > 0
    }

    pub fn print(&self, r: Render) -> anyhow::Result<()> {
        let include_meta = r.meta || r.pretty || self.noteworthy();

        let mut out = serde_json::Map::new();
        out.insert("schema".into(), Value::from(RESULT_SCHEMA));
        if include_meta {
            out.insert("repo".into(), serde_json::to_value(&self.repo)?);
            out.insert("index".into(), serde_json::to_value(&self.index)?);
        }

        let mut data = serde_json::to_value(&self.data)?;
        if let Some(fields) = r.fields {
            project(&mut data, fields);
        }
        out.insert("data".into(), data);

        let value = Value::Object(out);
        if r.pretty {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            println!("{}", serde_json::to_string(&value)?);
        }
        Ok(())
    }
}

/// Retain only `fields` on each object; works on an array of objects or a
/// single object. Non-object data is left untouched.
fn project(data: &mut Value, fields: &[String]) {
    match data {
        Value::Array(rows) => {
            for row in rows {
                project_obj(row, fields);
            }
        }
        obj @ Value::Object(_) => project_obj(obj, fields),
        _ => {}
    }
}

fn project_obj(v: &mut Value, fields: &[String]) {
    if let Value::Object(map) = v {
        map.retain(|k, _| fields.iter().any(|f| f == k));
    }
}

#[derive(Serialize)]
pub struct RepoMeta {
    pub root: String,
    pub vcs: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}

#[derive(Serialize)]
pub struct IndexMeta {
    pub schema: i64,
    pub fresh: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub reindexed_files: u64,
    /// Files that failed to parse/read during this refresh; 0 in the happy path.
    #[serde(skip_serializing_if = "is_zero")]
    pub partial_failures: u64,
    pub elapsed_ms: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

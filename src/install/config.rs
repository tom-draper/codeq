//! Edit only the reserved MCP entry. TOML comments and unrelated keys survive.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item, Table};

pub(super) enum Config {
    Json(Value),
    Toml(DocumentMut),
}

impl Config {
    pub fn parse(bytes: Option<&[u8]>, toml: bool) -> Result<Self> {
        let text = std::str::from_utf8(bytes.unwrap_or_default()).context("config is not UTF-8")?;
        if toml {
            let mut doc = text
                .parse::<DocumentMut>()
                .context("config is not valid TOML")?;
            if !doc.contains_key("mcp_servers") {
                doc["mcp_servers"] = Item::Table(Table::new());
            }
            if doc["mcp_servers"].as_table_like().is_none() {
                bail!("mcp_servers is not a table");
            }
            Ok(Self::Toml(doc))
        } else {
            let mut root: Value = if text.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(text).context("config is not valid JSON")?
            };
            let obj = root
                .as_object_mut()
                .context("config root is not a JSON object")?;
            let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
            if !servers.is_object() {
                bail!("mcpServers is not a JSON object");
            }
            Ok(Self::Json(root))
        }
    }

    pub fn entry(&self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Json(root) => root["mcpServers"]
                .get("codeq")
                .map(serde_json::to_vec)
                .transpose()
                .map_err(Into::into),
            Self::Toml(doc) => {
                let Some(item) = doc["mcp_servers"].get("codeq") else {
                    return Ok(None);
                };
                let mut fragment = DocumentMut::new();
                fragment["codeq"] = item.clone();
                Ok(Some(fragment.to_string().into_bytes()))
            }
        }
    }

    pub fn set(&mut self, bytes: Option<&[u8]>) -> Result<()> {
        match self {
            Self::Json(root) => {
                let servers = root["mcpServers"].as_object_mut().unwrap();
                match bytes {
                    Some(b) => {
                        servers.insert("codeq".into(), serde_json::from_slice(b)?);
                    }
                    None => {
                        servers.remove("codeq");
                    }
                }
            }
            Self::Toml(doc) => {
                let servers = doc["mcp_servers"].as_table_like_mut().unwrap();
                match bytes {
                    Some(b) => {
                        let mut fragment = std::str::from_utf8(b)?.parse::<DocumentMut>()?;
                        servers.insert(
                            "codeq",
                            fragment.remove("codeq").context("invalid MCP backup")?,
                        );
                    }
                    None => {
                        servers.remove("codeq");
                    }
                }
            }
        }
        Ok(())
    }

    pub fn desired(&self, bin: &str) -> Result<Vec<u8>> {
        match self {
            Self::Json(_) => Ok(serde_json::to_vec(
                &json!({"command": bin, "args": ["mcp"]}),
            )?),
            Self::Toml(_) => {
                let mut doc = DocumentMut::new();
                doc["codeq"]["command"] = toml_edit::value(bin);
                let mut args = toml_edit::Array::new();
                args.push("mcp");
                doc["codeq"]["args"] = toml_edit::value(args);
                Ok(doc.to_string().into_bytes())
            }
        }
    }

    pub fn render(&self) -> Result<Vec<u8>> {
        match self {
            Self::Json(root) => {
                Ok(format!("{}\n", serde_json::to_string_pretty(root)?).into_bytes())
            }
            Self::Toml(doc) => Ok(doc.to_string().into_bytes()),
        }
    }

    pub fn command(&self) -> Result<String> {
        let entry = match self {
            Self::Json(root) => root["mcpServers"]["codeq"].clone(),
            Self::Toml(doc) => {
                let value: Value = toml_edit::de::from_str(&doc.to_string())?;
                value["mcp_servers"]["codeq"].clone()
            }
        };
        if entry["args"] != json!(["mcp"])
            || entry.get("env").is_some()
            || entry.get("env_vars").is_some()
            || entry.get("cwd").is_some()
            || entry.get("disabled") == Some(&json!(true))
            || entry.get("enabled") == Some(&json!(false))
        {
            bail!("codeq registration is customized or disabled; startup was not probed");
        }
        let command = entry["command"]
            .as_str()
            .context("no codeq command registered")?;
        if !std::path::Path::new(command).is_absolute() {
            bail!("codeq command must be an absolute path");
        }
        Ok(command.to_owned())
    }
}

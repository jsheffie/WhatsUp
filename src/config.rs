use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::chrome::OpenMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardConfig {
    pub title: String,
    pub refresh_ms: u64,
    pub open: OpenMode,
    pub browser_app: String,
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub label: String,
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub tone: Tone,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Gray,
    Black,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Empty,
    Text(String),
    Link(Link),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub label: String,
    pub url: String,
    pub port: u16,
}

impl DashboardConfig {
    pub fn ports(&self) -> BTreeSet<u16> {
        self.links().map(|link| link.port).collect()
    }

    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.groups
            .iter()
            .flat_map(|group| group.rows.iter())
            .flat_map(|row| row.cells.iter())
            .filter_map(|cell| match cell {
                Cell::Link(link) => Some(link),
                _ => None,
            })
    }

    pub fn allows_url(&self, url: &str) -> bool {
        self.links().any(|link| link.url == url)
    }
}

pub fn parse_config(text: &str) -> Result<DashboardConfig, String> {
    let value: Value = serde_json::from_str(text).map_err(|err| format!("invalid json: {err}"))?;
    let raw: RawConfig =
        serde_json::from_value(value).map_err(|err| format!("invalid config: {err}"))?;
    raw.into_dashboard()
}

pub fn load_config(path: &Path) -> Result<DashboardConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    parse_config(&text)
}

pub fn config_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

/// Cargo runs use the repo file. The packaged app uses Application Support.
pub fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(path) = std::env::var_os("WHATSUP_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = dev_config_path() {
        return Ok(path);
    }
    user_config_path()
}

pub fn user_config_path() -> Result<PathBuf, String> {
    let dir = app_support_dir()?;
    fs::create_dir_all(&dir).map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    let path = dir.join("config.json");
    if !path.exists() {
        fs::write(&path, crate::DEFAULT_CONFIG_JSON)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    }
    Ok(path)
}

pub fn app_support_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("WhatsUp"))
}

fn dev_config_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let root = exe.parent()?.parent()?.parent()?;
    let candidate = root.join("config").join("whatsup.json");
    candidate.is_file().then_some(candidate)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_title")]
    title: String,
    #[serde(default = "default_refresh_ms")]
    refresh_ms: u64,
    #[serde(default)]
    open: OpenMode,
    #[serde(default = "default_browser")]
    browser_app: String,
    groups: Vec<RawGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    id: String,
    label: String,
    columns: Vec<String>,
    rows: Vec<RawRow>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRow {
    #[serde(default)]
    tone: Option<String>,
    cells: Vec<Value>,
}

fn default_title() -> String {
    "WhatsUp".to_string()
}

fn default_refresh_ms() -> u64 {
    2000
}

fn default_browser() -> String {
    "Google Chrome".to_string()
}

impl RawConfig {
    fn into_dashboard(self) -> Result<DashboardConfig, String> {
        if self.title.trim().is_empty() {
            return Err("title is empty".into());
        }
        if self.browser_app.trim().is_empty() {
            return Err("browser_app is empty".into());
        }
        if self.groups.is_empty() {
            return Err("groups is empty".into());
        }
        let refresh_ms = self.refresh_ms.clamp(500, 60_000);
        let mut seen_ids = BTreeSet::new();
        let mut groups = Vec::with_capacity(self.groups.len());
        for group in self.groups {
            if !seen_ids.insert(group.id.clone()) {
                return Err(format!("duplicate group id {}", group.id));
            }
            groups.push(group.into_group()?);
        }
        Ok(DashboardConfig {
            title: self.title,
            refresh_ms,
            open: self.open,
            browser_app: self.browser_app,
            groups,
        })
    }
}

impl RawGroup {
    fn into_group(self) -> Result<Group, String> {
        if self.id.trim().is_empty() || self.label.trim().is_empty() {
            return Err("group id and label are required".into());
        }
        if self.columns.is_empty() {
            return Err(format!("group {} has no columns", self.id));
        }
        if self.rows.is_empty() {
            return Err(format!("group {} has no rows", self.id));
        }
        let mut rows = Vec::with_capacity(self.rows.len());
        for (index, row) in self.rows.into_iter().enumerate() {
            rows.push(row.into_row(&self.id, index, self.columns.len())?);
        }
        Ok(Group {
            id: self.id,
            label: self.label,
            columns: self.columns,
            rows,
        })
    }
}

impl RawRow {
    fn into_row(self, group_id: &str, index: usize, column_count: usize) -> Result<Row, String> {
        if self.cells.len() != column_count {
            return Err(format!(
                "group {group_id} row {index} has {} cells, expected {column_count}",
                self.cells.len()
            ));
        }
        let tone = match self.tone.as_deref() {
            None => {
                if index.is_multiple_of(2) {
                    Tone::Gray
                } else {
                    Tone::Black
                }
            }
            Some("gray") => Tone::Gray,
            Some("black") => Tone::Black,
            Some(other) => {
                return Err(format!(
                    "group {group_id} row {index} tone {other} is not gray or black"
                ))
            }
        };
        let mut cells = Vec::with_capacity(self.cells.len());
        for (cell_index, value) in self.cells.into_iter().enumerate() {
            cells.push(parse_cell(group_id, index, cell_index, value)?);
        }
        Ok(Row { tone, cells })
    }
}

fn parse_cell(group_id: &str, row: usize, column: usize, value: Value) -> Result<Cell, String> {
    let at = format!("group {group_id} row {row} column {column}");
    match value {
        Value::Null => Ok(Cell::Empty),
        Value::String(text) => {
            if text.trim().is_empty() {
                Ok(Cell::Empty)
            } else {
                Ok(Cell::Text(text))
            }
        }
        Value::Object(map) => {
            let label = required_string(&map, "label", &at)?;
            let url = required_string(&map, "url", &at)?;
            validate_url(&url, &at)?;
            let port = match map.get("port") {
                None | Some(Value::Null) => port_from_url(&url)?,
                Some(Value::Number(number)) => number
                    .as_u64()
                    .and_then(|value| u16::try_from(value).ok())
                    .filter(|port| *port > 0)
                    .ok_or_else(|| format!("{at} port is not a tcp port"))?,
                Some(_) => return Err(format!("{at} port must be a number")),
            };
            let url_port = port_from_url(&url)?;
            if port != url_port {
                return Err(format!(
                    "{at} port {port} does not match the port in {url} ({url_port})"
                ));
            }
            let known = ["label", "url", "port"];
            for key in map.keys() {
                if !known.contains(&key.as_str()) {
                    return Err(format!("{at} has unknown field {key}"));
                }
            }
            Ok(Cell::Link(Link { label, url, port }))
        }
        _ => Err(format!("{at} must be a string, a link, or null")),
    }
}

fn required_string(
    map: &serde_json::Map<String, Value>,
    key: &str,
    at: &str,
) -> Result<String, String> {
    match map.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        _ => Err(format!("{at} needs a {key}")),
    }
}

fn validate_url(url: &str, at: &str) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("{at} url must start with http:// or https://"));
    }
    if url
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '"' | '\\' | '`'))
    {
        return Err(format!(
            "{at} url contains a character Chrome cannot be given"
        ));
    }
    Ok(())
}

pub fn port_from_url(url: &str) -> Result<u16, String> {
    let (scheme, rest) = url
        .strip_prefix("https://")
        .map(|rest| ("https", rest))
        .or_else(|| url.strip_prefix("http://").map(|rest| ("http", rest)))
        .ok_or_else(|| format!("{url} is not an http url"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let hostport = authority.split('@').next_back().unwrap_or(authority);
    let port_text = if let Some(rest) = hostport.strip_prefix('[') {
        rest.split_once("]:").map(|(_, port)| port)
    } else {
        hostport.rsplit_once(':').and_then(|(host, port)| {
            if host.is_empty() || port.is_empty() {
                None
            } else if port.chars().all(|ch| ch.is_ascii_digit()) {
                Some(port)
            } else {
                None
            }
        })
    };
    match port_text {
        Some(text) => text
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| format!("{url} has an invalid port")),
        None if scheme == "https" => Ok(443),
        None => Ok(80),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses_example_stacks() {
        let config = parse_config(crate::DEFAULT_CONFIG_JSON).unwrap();
        let ports: Vec<u16> = config.ports().into_iter().collect();
        assert_eq!(ports, vec![1313, 3000, 3002, 5173, 8000, 8002]);
        assert_eq!(config.links().count(), 7);
        let find = |label: &str| config.links().find(|link| link.label == label).unwrap();
        let frontend = find("frontend");
        assert_eq!(frontend.url, "http://localhost:3000");
        assert_eq!(frontend.port, 3000);
        let admin = find("admin");
        assert_eq!(admin.url, "http://localhost:8000/admin/");
        assert_eq!(admin.port, 8000);
        let docs = find("api docs");
        assert_eq!(docs.url, "http://localhost:8000/docs/");
        assert_eq!(docs.port, 8000);
        // Port omitted in the JSON: derived from the URL.
        let blog = find("blog");
        assert_eq!(blog.url, "http://localhost:1313/");
        assert_eq!(blog.port, 1313);
    }

    #[test]
    fn derives_port_when_omitted_and_rejects_a_mismatch() {
        let config = parse_config(
            r#"{
                "groups": [{
                    "id": "g",
                    "label": "G",
                    "columns": ["Name"],
                    "rows": [{ "cells": [{ "label": "web", "url": "http://localhost:3000/path" }] }]
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(config.links().next().unwrap().port, 3000);

        let error = parse_config(
            r#"{
                "groups": [{
                    "id": "g",
                    "label": "G",
                    "columns": ["Name"],
                    "rows": [{ "cells": [{ "label": "web", "url": "http://localhost:3000", "port": 3001 }] }]
                }]
            }"#,
        )
        .unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn port_from_url_covers_defaults_and_ipv6() {
        assert_eq!(port_from_url("http://localhost").unwrap(), 80);
        assert_eq!(port_from_url("https://example.com/admin/").unwrap(), 443);
        assert_eq!(port_from_url("http://[::1]:2023/admin/").unwrap(), 2023);
    }
}

use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::browser::OpenMode;

/// Where WhatsUp's SSH tunnel to a remote machine listens by default.
pub const DEFAULT_SOCKS_PORT: u16 = 47655;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardConfig {
    pub title: String,
    pub refresh_ms: u64,
    pub open: OpenMode,
    pub browser_app: String,
    pub groups: Vec<Group>,
    pub remotes: Vec<Remote>,
    /// Short hostname -> label shown on the dashboard (e.g. a Mac's
    /// generated hostname -> "megan").
    pub machine_names: BTreeMap<String, String>,
}

/// Another machine whose stacks are mirrored on this dashboard, reached
/// through an SSH SOCKS tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub ssh: String,
    pub browser_profile: String,
    pub socks_port: u16,
    /// Absolute path of the remote's herdr socket, forwarded over the tunnel.
    pub herdr_socket: Option<String>,
    /// Where the remote machine's own WhatsUp listens (on its loopback);
    /// stops of its rows are forwarded there.
    pub whatsup_port: u16,
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
    /// Machines this row belongs to; `None` shows it on every machine.
    pub machines: Option<Vec<String>>,
}

impl Row {
    /// The distinct ports of the row's links, ascending.
    pub fn ports(&self) -> Vec<u16> {
        let ports: BTreeSet<u16> = self
            .cells
            .iter()
            .filter_map(|cell| match cell {
                Cell::Link(link) => Some(link.port),
                _ => None,
            })
            .collect();
        ports.into_iter().collect()
    }

    pub fn shown_on(&self, machine: &str) -> bool {
        self.machines.as_ref().is_none_or(|machines| {
            machines
                .iter()
                .any(|name| name.eq_ignore_ascii_case(machine))
        })
    }
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
    /// Text followed by the labels of the machine's herdr workspaces whose
    /// names start with `prefix`.
    Herdr(HerdrCell),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrCell {
    pub text: String,
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub label: String,
    pub url: String,
    pub host: String,
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

    /// The links of the rows shown on `machine`.
    pub fn links_on<'a>(&'a self, machine: &'a str) -> impl Iterator<Item = &'a Link> + 'a {
        self.groups
            .iter()
            .flat_map(|group| group.rows.iter())
            .filter(move |row| row.shown_on(machine))
            .flat_map(|row| row.cells.iter())
            .filter_map(|cell| match cell {
                Cell::Link(link) => Some(link),
                _ => None,
            })
    }

    /// Whether any row shown on `machine` has a herdr cell.
    pub fn has_herdr_cells_on(&self, machine: &str) -> bool {
        self.groups
            .iter()
            .flat_map(|group| group.rows.iter())
            .filter(|row| row.shown_on(machine))
            .flat_map(|row| row.cells.iter())
            .any(|cell| matches!(cell, Cell::Herdr(_)))
    }

    pub fn allows_url_on(&self, url: &str, machine: &str) -> bool {
        self.links_on(machine).any(|link| link.url == url)
    }

    /// The dashboard label for the machine with this short hostname.
    pub fn machine_label(&self, hostname: &str) -> String {
        self.machine_names
            .get(hostname)
            .cloned()
            .unwrap_or_else(|| hostname.to_string())
    }

    /// Remotes to tunnel to from the machine labelled `local_label`; a remote
    /// naming this machine itself is skipped.
    pub fn active_remotes(&self, local_label: &str) -> Vec<Remote> {
        self.remotes
            .iter()
            .filter(|remote| !remote.name.eq_ignore_ascii_case(local_label))
            .cloned()
            .collect()
    }

    pub fn remote(&self, name: &str) -> Option<&Remote> {
        self.remotes.iter().find(|remote| remote.name == name)
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

/// Cargo runs use the repo file. Installed builds use the per-user config dir.
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
    let dir = config_dir()?;
    fs::create_dir_all(&dir).map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    let path = dir.join("config.json");
    if !path.exists() {
        fs::write(&path, crate::DEFAULT_CONFIG_JSON)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    }
    Ok(path)
}

/// Where the user's config.json lives.
///
/// macOS: `~/Library/Application Support/WhatsUp`.
/// Elsewhere: `$XDG_CONFIG_HOME/whatsup`, else `~/.config/whatsup`.
pub fn config_dir() -> Result<PathBuf, String> {
    platform_dirs(&env_lookup).map(|dirs| dirs.config)
}

/// Where the single-instance file lives.
///
/// macOS: next to the config. Elsewhere: `$XDG_RUNTIME_DIR/whatsup`, which the
/// session clears on logout, else `$XDG_STATE_HOME/whatsup` (`~/.local/state`).
pub fn runtime_dir() -> Result<PathBuf, String> {
    platform_dirs(&env_lookup).map(|dirs| dirs.runtime)
}

#[derive(Debug, PartialEq, Eq)]
struct Dirs {
    config: PathBuf,
    runtime: PathBuf,
}

fn env_lookup(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key)
}

#[cfg(target_os = "macos")]
fn platform_dirs(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<Dirs, String> {
    mac_dirs(env)
}

#[cfg(not(target_os = "macos"))]
fn platform_dirs(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<Dirs, String> {
    xdg_dirs(env)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_dirs(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<Dirs, String> {
    let home = home(env)?;
    let dir = home
        .join("Library")
        .join("Application Support")
        .join("WhatsUp");
    Ok(Dirs {
        config: dir.clone(),
        runtime: dir,
    })
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn xdg_dirs(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<Dirs, String> {
    // The XDG spec says relative values are invalid and must be ignored.
    let absolute = |key: &str| {
        env(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let config = match absolute("XDG_CONFIG_HOME") {
        Some(base) => base,
        None => home(env)?.join(".config"),
    };
    let runtime = match absolute("XDG_RUNTIME_DIR") {
        Some(base) => base,
        None => match absolute("XDG_STATE_HOME") {
            Some(base) => base,
            None => home(env)?.join(".local").join("state"),
        },
    };
    Ok(Dirs {
        config: config.join("whatsup"),
        runtime: runtime.join("whatsup"),
    })
}

fn home(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    env("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
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
    #[serde(default)]
    remotes: Vec<RawRemote>,
    #[serde(default)]
    machine_names: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRemote {
    name: String,
    ssh: String,
    browser_profile: String,
    #[serde(default)]
    socks_port: Option<u16>,
    #[serde(default)]
    herdr_socket: Option<String>,
    #[serde(default)]
    whatsup_port: Option<u16>,
}

impl RawRemote {
    fn into_remote(self) -> Result<Remote, String> {
        let at = format!("remote {}", self.name);
        if self.name.trim().is_empty() {
            return Err("remote name is empty".into());
        }
        if self.ssh.is_empty()
            || self.ssh.starts_with('-')
            || self
                .ssh
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(format!("{at} ssh must be one ssh host name"));
        }
        if self.browser_profile.trim().is_empty() {
            return Err(format!("{at} browser_profile is empty"));
        }
        let socks_port = self.socks_port.unwrap_or(DEFAULT_SOCKS_PORT);
        if socks_port == 0 || socks_port == crate::PREFERRED_PORT {
            return Err(format!(
                "{at} socks_port must be a free tcp port other than {}",
                crate::PREFERRED_PORT
            ));
        }
        if let Some(socket) = &self.herdr_socket {
            // It becomes the remote half of an ssh `-L local:remote` forward.
            if !socket.starts_with('/')
                || socket
                    .chars()
                    .any(|ch| ch == ':' || ch.is_whitespace() || ch.is_control())
            {
                return Err(format!(
                    "{at} herdr_socket must be an absolute path without ':' or spaces"
                ));
            }
        }
        let whatsup_port = self.whatsup_port.unwrap_or(crate::PREFERRED_PORT);
        if whatsup_port == 0 {
            return Err(format!("{at} whatsup_port must be a tcp port"));
        }
        Ok(Remote {
            name: self.name,
            ssh: self.ssh,
            browser_profile: self.browser_profile,
            socks_port,
            herdr_socket: self.herdr_socket,
            whatsup_port,
        })
    }
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
    #[serde(default)]
    machines: Option<Vec<String>>,
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
        let mut seen_remotes = BTreeSet::new();
        let mut remotes = Vec::with_capacity(self.remotes.len());
        for remote in self.remotes {
            if !seen_remotes.insert(remote.name.to_ascii_lowercase()) {
                return Err(format!("duplicate remote {}", remote.name));
            }
            remotes.push(remote.into_remote()?);
        }
        Ok(DashboardConfig {
            title: self.title,
            refresh_ms,
            open: self.open,
            browser_app: self.browser_app,
            groups,
            remotes,
            machine_names: self.machine_names,
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
        if let Some(machines) = &self.machines {
            if machines.is_empty() || machines.iter().any(|name| name.trim().is_empty()) {
                return Err(format!(
                    "group {group_id} row {index} machines must list machine names"
                ));
            }
        }
        let mut cells = Vec::with_capacity(self.cells.len());
        for (cell_index, value) in self.cells.into_iter().enumerate() {
            cells.push(parse_cell(group_id, index, cell_index, value)?);
        }
        Ok(Row {
            tone,
            cells,
            machines: self.machines,
        })
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
        Value::Object(map) if map.contains_key("text") => parse_herdr_cell(&map, &at),
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
            let host = host_from_url(&url)?;
            Ok(Cell::Link(Link {
                label,
                url,
                host,
                port,
            }))
        }
        _ => Err(format!("{at} must be a string, a link, or null")),
    }
}

fn parse_herdr_cell(map: &serde_json::Map<String, Value>, at: &str) -> Result<Cell, String> {
    for key in map.keys() {
        if !["text", "herdr"].contains(&key.as_str()) {
            return Err(format!("{at} has unknown field {key}"));
        }
    }
    let text = required_string(map, "text", at)?;
    let prefix = required_string(map, "herdr", at)?;
    if text.trim().is_empty() {
        return Err(format!("{at} text is empty"));
    }
    if prefix.is_empty()
        || prefix
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(format!(
            "{at} herdr must be a workspace name prefix without spaces"
        ));
    }
    Ok(Cell::Herdr(HerdrCell { text, prefix }))
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

/// The scheme and the `host[:port]` part of an http(s) url.
fn scheme_and_hostport(url: &str) -> Result<(&'static str, &str), String> {
    let (scheme, rest) = url
        .strip_prefix("https://")
        .map(|rest| ("https", rest))
        .or_else(|| url.strip_prefix("http://").map(|rest| ("http", rest)))
        .ok_or_else(|| format!("{url} is not an http url"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let hostport = authority.split('@').next_back().unwrap_or(authority);
    Ok((scheme, hostport))
}

/// The lowercased host of an http(s) url; IPv6 hosts come without brackets.
pub fn host_from_url(url: &str) -> Result<String, String> {
    let (_, hostport) = scheme_and_hostport(url)?;
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        rest.split_once(']').map(|(host, _)| host)
    } else {
        Some(hostport.split(':').next().unwrap_or(hostport))
    };
    match host {
        Some(host) if !host.is_empty() => Ok(host.to_ascii_lowercase()),
        _ => Err(format!("{url} has no host")),
    }
}

pub fn port_from_url(url: &str) -> Result<u16, String> {
    let (scheme, hostport) = scheme_and_hostport(url)?;
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
        assert_eq!(frontend.host, "localhost");
        assert!(config.remotes.is_empty());
        assert!(config.machine_names.is_empty());
    }

    fn with_top_level(extra: &str) -> String {
        format!(
            r#"{{
                {extra}
                "groups": [{{
                    "id": "g",
                    "label": "G",
                    "columns": ["Name"],
                    "rows": [{{ "cells": [{{ "label": "web", "url": "http://localhost:3000" }}] }}]
                }}]
            }}"#
        )
    }

    fn with_cells(cells: &str) -> String {
        format!(
            r#"{{
                "groups": [{{
                    "id": "g",
                    "label": "G",
                    "columns": ["Workspace"],
                    "rows": [{{ "cells": [{cells}] }}]
                }}]
            }}"#
        )
    }

    #[test]
    fn herdr_cell_parses() {
        let config = parse_config(&with_cells(r#"{ "text": "jds", "herdr": "h1" }"#)).unwrap();
        assert_eq!(
            config.groups[0].rows[0].cells[0],
            Cell::Herdr(HerdrCell {
                text: "jds".into(),
                prefix: "h1".into(),
            })
        );
    }

    #[test]
    fn herdr_cell_rejects_blank_or_unknown() {
        let bad = [
            r#"{ "text": " ", "herdr": "h1" }"#,
            r#"{ "text": "jds", "herdr": "" }"#,
            r#"{ "text": "jds", "herdr": "h 1" }"#,
            r#"{ "text": "jds", "herdr": "h1", "x": 1 }"#,
            r#"{ "text": "jds" }"#,
        ];
        for cell in bad {
            let error = parse_config(&with_cells(cell)).expect_err(cell);
            assert!(error.contains("group g row 0 column 0"), "{cell}: {error}");
        }
    }

    #[test]
    fn remote_herdr_socket_must_be_absolute() {
        let remote = |extra: &str| {
            with_top_level(&format!(
                r#""remotes": [{{ "name": "lucy", "ssh": "lucy", "browser_profile": "P"{extra} }}],"#
            ))
        };
        assert_eq!(
            parse_config(&remote("")).unwrap().remotes[0].herdr_socket,
            None
        );
        assert_eq!(
            parse_config(&remote(
                r#", "herdr_socket": "/home/jds/.config/herdr/herdr.sock""#
            ))
            .unwrap()
            .remotes[0]
                .herdr_socket
                .as_deref(),
            Some("/home/jds/.config/herdr/herdr.sock")
        );
        for bad in [
            r#", "herdr_socket": ".config/herdr/herdr.sock""#,
            r#", "herdr_socket": "/home/a:b.sock""#,
            r#", "herdr_socket": "/home/a b.sock""#,
        ] {
            assert!(parse_config(&remote(bad)).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn has_herdr_cells_on_respects_row_machines() {
        let config = parse_config(
            r#"{
                "groups": [{
                    "id": "g",
                    "label": "G",
                    "columns": ["Workspace"],
                    "rows": [
                        { "machines": ["lucy"], "cells": [{ "text": "jds7", "herdr": "h7" }] },
                        { "cells": ["plain"] }
                    ]
                }]
            }"#,
        )
        .unwrap();
        assert!(config.has_herdr_cells_on("lucy"));
        assert!(!config.has_herdr_cells_on("megan"));
    }

    #[test]
    fn parses_remotes_with_defaults() {
        let config = parse_config(&with_top_level(
            r#""remotes": [{ "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy" }],"#,
        ))
        .unwrap();
        assert_eq!(
            config.remotes,
            vec![Remote {
                name: "lucy".into(),
                ssh: "lucy".into(),
                browser_profile: "megan:Lucy".into(),
                socks_port: 47655,
                herdr_socket: None,
                whatsup_port: 47654,
            }]
        );
        assert_eq!(DEFAULT_SOCKS_PORT, 47655);

        let custom = parse_config(&with_top_level(
            r#""remotes": [{ "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy", "socks_port": 40000 }],"#,
        ))
        .unwrap();
        assert_eq!(custom.remotes[0].socks_port, 40000);
        let moved = parse_config(&with_top_level(
            r#""remotes": [{ "name": "lucy", "ssh": "lucy", "browser_profile": "P", "whatsup_port": 5000 }],"#,
        ))
        .unwrap();
        assert_eq!(moved.remotes[0].whatsup_port, 5000);
        assert_eq!(custom.remote("lucy").unwrap().socks_port, 40000);
        assert!(custom.remote("megan").is_none());
    }

    #[test]
    fn rejects_bad_remotes() {
        let bad = [
            r#"{ "name": "lucy", "ssh": "lucy", "browser_profile": "P" }, { "name": "lucy", "ssh": "x", "browser_profile": "P" }"#,
            r#"{ "name": " ", "ssh": "lucy", "browser_profile": "P" }"#,
            r#"{ "name": "lucy", "ssh": "", "browser_profile": "P" }"#,
            r#"{ "name": "lucy", "ssh": "lucy", "browser_profile": "" }"#,
            r#"{ "name": "lucy", "ssh": "-oProxyCommand=evil", "browser_profile": "P" }"#,
            r#"{ "name": "lucy", "ssh": "lucy host", "browser_profile": "P" }"#,
            r#"{ "name": "lucy", "ssh": "lucy", "browser_profile": "P", "socks_port": 0 }"#,
            r#"{ "name": "lucy", "ssh": "lucy", "browser_profile": "P", "socks_port": 47654 }"#,
            r#"{ "name": "lucy", "ssh": "lucy", "browser_profile": "P", "extra": 1 }"#,
            r#"{ "name": "lucy", "ssh": "lucy", "browser_profile": "P", "whatsup_port": 0 }"#,
        ];
        for remotes in bad {
            let text = with_top_level(&format!(r#""remotes": [{remotes}],"#));
            assert!(parse_config(&text).is_err(), "accepted {remotes}");
        }
    }

    #[test]
    fn machine_label_maps_hostnames() {
        let config = parse_config(&with_top_level(
            r#""machine_names": { "Jeffs-MacBook-Pro-2": "megan" },"#,
        ))
        .unwrap();
        assert_eq!(config.machine_label("Jeffs-MacBook-Pro-2"), "megan");
        assert_eq!(config.machine_label("lucy"), "lucy");
    }

    #[test]
    fn active_remotes_skip_the_local_machine() {
        let config = parse_config(&with_top_level(
            r#""remotes": [{ "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy" }],"#,
        ))
        .unwrap();
        assert!(config.active_remotes("lucy").is_empty());
        assert!(config.active_remotes("Lucy").is_empty());
        assert_eq!(config.active_remotes("megan").len(), 1);
    }

    #[test]
    fn rows_can_be_limited_to_machines() {
        let config = parse_config(
            r#"{
                "groups": [{
                    "id": "g",
                    "label": "G",
                    "columns": ["Name"],
                    "rows": [
                        { "cells": [{ "label": "both", "url": "http://localhost:3000" }] },
                        { "machines": ["lucy"], "cells": [{ "label": "lucy only", "url": "http://localhost:3009" }] }
                    ]
                }]
            }"#,
        )
        .unwrap();
        let rows = &config.groups[0].rows;
        assert!(rows[0].shown_on("megan") && rows[0].shown_on("lucy"));
        assert!(rows[1].shown_on("lucy") && rows[1].shown_on("Lucy"));
        assert!(!rows[1].shown_on("megan"));
        let ports = |machine: &str| -> Vec<u16> {
            config.links_on(machine).map(|link| link.port).collect()
        };
        assert_eq!(ports("megan"), [3000]);
        assert_eq!(ports("lucy"), [3000, 3009]);
        assert!(config.allows_url_on("http://localhost:3009", "lucy"));
        assert!(!config.allows_url_on("http://localhost:3009", "megan"));
        // Every link is still known to the config as a whole.
        assert_eq!(config.links().count(), 2);

        for bad in [
            r#""machines": []"#,
            r#""machines": [" "]"#,
            r#""machines": "lucy""#,
        ] {
            let text = format!(
                r#"{{ "groups": [{{ "id": "g", "label": "G", "columns": ["Name"],
                    "rows": [{{ {bad}, "cells": ["x"] }}] }}] }}"#
            );
            assert!(parse_config(&text).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn row_ports_are_distinct_and_sorted() {
        let config = parse_config(
            r#"{
                "groups": [{
                    "id": "g",
                    "label": "G",
                    "columns": ["Name", "A", "B", "C"],
                    "rows": [{ "cells": [
                        "stack",
                        { "label": "admin", "url": "http://localhost:8004/admin/" },
                        { "label": "portal", "url": "http://localhost:3004" },
                        { "label": "schema", "url": "http://localhost:8004/schema/" }
                    ] }, { "cells": ["text only", null, null, null] }]
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(config.groups[0].rows[0].ports(), [3004, 8004]);
        assert!(config.groups[0].rows[1].ports().is_empty());
    }

    #[test]
    fn host_from_url_cases() {
        assert_eq!(
            host_from_url("http://jds4.local.me:3004/").unwrap(),
            "jds4.local.me"
        );
        assert_eq!(host_from_url("http://LOCALHOST:4004").unwrap(), "localhost");
        assert_eq!(host_from_url("http://[::1]:2023/admin/").unwrap(), "::1");
        assert_eq!(host_from_url("https://u@h.example/x").unwrap(), "h.example");
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

    fn fake_env(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.into())
        }
    }

    #[test]
    fn xdg_dirs_follow_the_spec_and_fall_back_to_home() {
        let set = fake_env(&[
            ("HOME", "/home/me"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        assert_eq!(
            xdg_dirs(&set).unwrap(),
            Dirs {
                config: PathBuf::from("/cfg/whatsup"),
                runtime: PathBuf::from("/run/user/1000/whatsup"),
            }
        );

        let bare = fake_env(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "relative")]);
        assert_eq!(
            xdg_dirs(&bare).unwrap(),
            Dirs {
                config: PathBuf::from("/home/me/.config/whatsup"),
                runtime: PathBuf::from("/home/me/.local/state/whatsup"),
            }
        );

        assert!(xdg_dirs(&fake_env(&[])).is_err());
    }

    #[test]
    fn mac_dirs_keep_everything_in_application_support() {
        let dirs = mac_dirs(&fake_env(&[("HOME", "/Users/me")])).unwrap();
        let expected = PathBuf::from("/Users/me/Library/Application Support/WhatsUp");
        assert_eq!(dirs.config, expected);
        assert_eq!(dirs.runtime, expected);
    }

    #[test]
    fn port_from_url_covers_defaults_and_ipv6() {
        assert_eq!(port_from_url("http://localhost").unwrap(), 80);
        assert_eq!(port_from_url("https://example.com/admin/").unwrap(), 443);
        assert_eq!(port_from_url("http://[::1]:2023/admin/").unwrap(), 2023);
    }
}

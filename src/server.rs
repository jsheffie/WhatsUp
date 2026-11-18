use std::collections::{BTreeMap, BTreeSet};
use std::net::ToSocketAddrs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::browser::{open_in_browser, open_in_profile, OpenMode};
use crate::config::{
    config_mtime, load_config, Cell, DashboardConfig, Group, Link, Remote, Row, Tone,
};
use crate::herdr::{list_workspaces, matching_labels, HerdrWorkspace, QUERY_BUDGET};
use crate::probe::probe_ports;
use crate::socks::{post_json, probe_remote, PostError, Reach};
use crate::stop::{Action, PortResult, Stopper};
use crate::tunnel::{TunnelState, Tunnels};
use crate::DASHBOARD_HTML;

pub trait BrowserOpener: Send + Sync {
    fn open(&self, app: &str, url: &str, mode: OpenMode) -> Result<(), String>;
    fn open_in_profile(&self, app: &str, profile: &str, url: &str) -> Result<(), String>;
}

pub struct SystemOpener;

impl BrowserOpener for SystemOpener {
    fn open(&self, app: &str, url: &str, mode: OpenMode) -> Result<(), String> {
        open_in_browser(app, url, mode)
    }

    fn open_in_profile(&self, app: &str, profile: &str, url: &str) -> Result<(), String> {
        open_in_profile(app, profile, url)
    }
}

struct Inner {
    config_path: std::path::PathBuf,
    config: Mutex<DashboardConfig>,
    mtime: Mutex<Option<SystemTime>>,
    config_error: Mutex<Option<String>>,
    opener: Arc<dyn BrowserOpener>,
    shutdown: watch::Sender<bool>,
    /// This machine's short hostname; `machine_names` in the config turns it
    /// into the label shown on the dashboard.
    hostname: String,
    tunnels: Arc<Tunnels>,
    /// This machine's herdr socket, if herdr labels can be read here.
    local_herdr: Option<std::path::PathBuf>,
    stopper: Arc<dyn Stopper>,
    /// Rows being stopped right now, as (group id, row index).
    stopping: Arc<Mutex<BTreeSet<(String, usize)>>>,
    /// How long a stop forwarded to a remote WhatsUp may take.
    forward_timeout: Duration,
}

/// What the dashboard does to the machine it runs on.
pub struct Actions {
    pub opener: Arc<dyn BrowserOpener>,
    pub stopper: Arc<dyn Stopper>,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

impl AppState {
    pub fn new(
        config_path: std::path::PathBuf,
        config: DashboardConfig,
        actions: Actions,
        shutdown: watch::Sender<bool>,
        hostname: String,
        tunnels: Arc<Tunnels>,
        local_herdr: Option<std::path::PathBuf>,
    ) -> Self {
        let mtime = config_mtime(&config_path);
        tunnels.reconcile(&config.active_remotes(&config.machine_label(&hostname)));
        Self {
            inner: Arc::new(Inner {
                config_path,
                config: Mutex::new(config),
                mtime: Mutex::new(mtime),
                config_error: Mutex::new(None),
                opener: actions.opener,
                shutdown,
                hostname,
                tunnels,
                local_herdr,
                stopper: actions.stopper,
                stopping: Arc::default(),
                forward_timeout: FORWARD_TIMEOUT,
            }),
        }
    }

    /// Replaces how long a forwarded stop may take (for tests). Only before
    /// the state is shared.
    pub fn with_forward_timeout(mut self, timeout: Duration) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("the state is not shared yet")
            .forward_timeout = timeout;
        self
    }

    fn local_label(&self, config: &DashboardConfig) -> String {
        config.machine_label(&self.inner.hostname)
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.inner.shutdown.subscribe()
    }

    fn reload(&self) -> (DashboardConfig, Option<String>) {
        let latest = config_mtime(&self.inner.config_path);
        let mut mtime = self.inner.mtime.lock().expect("mtime lock");
        if *mtime != latest {
            match load_config(&self.inner.config_path) {
                Ok(config) => {
                    let local = self.local_label(&config);
                    self.inner.tunnels.reconcile(&config.active_remotes(&local));
                    *self.inner.config.lock().expect("config lock") = config;
                    *self.inner.config_error.lock().expect("error lock") = None;
                    *mtime = latest;
                }
                Err(err) => {
                    *self.inner.config_error.lock().expect("error lock") = Some(err);
                }
            }
        }
        let config = self.inner.config.lock().expect("config lock").clone();
        let error = self.inner.config_error.lock().expect("error lock").clone();
        (config, error)
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/dashboard", get(dashboard))
        .route("/api/open", post(open_link))
        .route("/api/stop", post(stop_row))
        .route("/api/quit", post(quit))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn dashboard(State(state): State<AppState>) -> Json<Value> {
    let (config, error) = state.reload();
    let path = state.inner.config_path.display().to_string();
    let local = state.local_label(&config);
    let remotes = config.active_remotes(&local);

    // Start every probe before awaiting any, so a slow remote never delays
    // the local status (each remote probe is bounded by its timeouts).
    let ports: std::collections::BTreeSet<u16> =
        config.links_on(&local).map(|link| link.port).collect();
    let local_probe = tokio::task::spawn_blocking(move || probe_ports(ports));
    let remote_probes: Vec<_> = remotes
        .iter()
        .map(|remote| {
            // The remote's own WhatsUp is probed with its links: it is what
            // stops that machine's rows.
            let socks_port = remote.socks_port;
            let targets: Vec<(String, u16)> = remote_links(&config, &remote.name)
                .into_iter()
                .chain([remote_whatsup(remote)])
                .collect();
            tokio::task::spawn_blocking(move || probe_remote(socks_port, targets))
        })
        .collect();
    // herdr workspace lists, queried alongside the probes; each is bounded
    // by QUERY_BUDGET and any failure just means no labels.
    let query_herdr = |machine: &str, socket: Option<std::path::PathBuf>| {
        let socket = socket.filter(|_| config.has_herdr_cells_on(machine));
        socket
            .map(|socket| tokio::spawn(async move { list_workspaces(&socket, QUERY_BUDGET).await }))
    };
    let local_herdr = query_herdr(&local, state.inner.local_herdr.clone());
    let remote_herdr: Vec<_> = remotes
        .iter()
        .map(|remote| query_herdr(&remote.name, state.inner.tunnels.herdr_socket(&remote.name)))
        .collect();
    let local_status = local_probe.await.expect("port probe");
    let mut machines = vec![MachineStatus {
        name: local,
        local: true,
        offline: false,
        note: None,
        up: Box::new(move |link: &Link| local_status.get(&link.port).copied().unwrap_or(false)),
        counts: (0, 0),
        can_stop: true,
        herdr: herdr_result(local_herdr).await,
    }];
    machines[0].counts = count_local(&config, &machines[0]);
    for ((remote, probe), herdr) in remotes.iter().zip(remote_probes).zip(remote_herdr) {
        let reach = probe.await.expect("remote probe");
        let offline = reach.values().any(|reach| *reach == Reach::Offline);
        let tunnel_error = match state.inner.tunnels.state(&remote.name) {
            Some(TunnelState::Waiting { last_error }) => Some(last_error),
            _ => None,
        };
        // A failing tunnel is worth saying even when probes work: then they
        // go through a proxy something else left on the port.
        let note = match (offline, tunnel_error) {
            (true, Some(error)) => Some(format!("{} offline: {error}", remote.name)),
            (true, None) => Some(format!("{} offline", remote.name)),
            (false, Some(error)) => Some(format!("{} tunnel not running: {error}", remote.name)),
            (false, None) => None,
        };
        let links = remote_links(&config, &remote.name);
        let up = |target: &(String, u16)| !offline && reach.get(target) == Some(&Reach::Up);
        let counts = (links.iter().filter(|link| up(link)).count(), links.len());
        let can_stop = up(&remote_whatsup(remote));
        machines.push(MachineStatus {
            name: remote.name.clone(),
            local: false,
            offline,
            note,
            up: Box::new(move |link: &Link| {
                !offline && reach.get(&(link.host.clone(), link.port)) == Some(&Reach::Up)
            }),
            counts,
            can_stop,
            herdr: herdr_result(herdr).await,
        });
    }
    Json(view(&config, &machines, &path, error))
}

/// The distinct (host, port) targets of the links shown on `machine`.
fn remote_links(config: &DashboardConfig, machine: &str) -> BTreeSet<(String, u16)> {
    config
        .links_on(machine)
        .map(|link| (link.host.clone(), link.port))
        .collect()
}

/// Where `remote`'s own WhatsUp listens, as seen from that machine.
fn remote_whatsup(remote: &Remote) -> (String, u16) {
    ("127.0.0.1".to_string(), remote.whatsup_port)
}

/// One machine's column of status: how its links are judged up, and the
/// counts shown in the summary.
struct MachineStatus {
    name: String,
    local: bool,
    offline: bool,
    note: Option<String>,
    up: Box<dyn Fn(&Link) -> bool + Send>,
    counts: (usize, usize),
    /// Whether this dashboard can stop the machine's rows.
    can_stop: bool,
    /// The machine's herdr workspaces, when they could be read.
    herdr: Option<Vec<HerdrWorkspace>>,
}

async fn herdr_result(
    query: Option<tokio::task::JoinHandle<Option<Vec<HerdrWorkspace>>>>,
) -> Option<Vec<HerdrWorkspace>> {
    query?.await.ok().flatten()
}

/// Local status is per port, as before: distinct ports, and how many are up.
fn count_local(config: &DashboardConfig, machine: &MachineStatus) -> (usize, usize) {
    let mut by_port: BTreeMap<u16, bool> = BTreeMap::new();
    for link in config.links_on(&machine.name) {
        *by_port.entry(link.port).or_default() |= (machine.up)(link);
    }
    (by_port.values().filter(|up| **up).count(), by_port.len())
}

#[derive(Deserialize)]
struct OpenRequest {
    url: String,
    #[serde(default)]
    machine: Option<String>,
}

async fn open_link(State(state): State<AppState>, Json(request): Json<OpenRequest>) -> Response {
    let (config, _) = state.reload();
    let bad =
        |error: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    let local = state.local_label(&config);
    let machine = request.machine.as_deref().unwrap_or(&local);
    if !config.allows_url_on(&request.url, machine) {
        return bad("that url is not in the dashboard config".into());
    }
    let opener = &state.inner.opener;
    let result = match request.machine.as_deref() {
        None => opener.open(&config.browser_app, &request.url, config.open),
        Some(name) if name == local => opener.open(&config.browser_app, &request.url, config.open),
        Some(name) => {
            let remotes = config.active_remotes(&local);
            let Some(remote) = remotes.iter().find(|remote| remote.name == name) else {
                return bad(format!("unknown machine {name}"));
            };
            opener.open_in_profile(&config.browser_app, &remote.browser_profile, &request.url)
        }
    };
    match result {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": err }))).into_response(),
    }
}

#[derive(Deserialize)]
struct StopRequest {
    #[serde(default)]
    machine: Option<String>,
    group: String,
    row: usize,
    ports: Vec<u16>,
}

const CONFIG_CHANGED: &str = "the dashboard config changed; reload and try again";

/// How long a forwarded stop may take: the remote's own worst case (the
/// docker listing, a docker stop, a kill) and some room.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(90);

fn error_response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(json!({ "error": error.into() }))).into_response()
}

/// Stops are for this dashboard's own page (or a WhatsUp forwarding one),
/// not for any site the browser happens to have open: the request must be
/// addressed to loopback by name, and come from this origin if it names one.
fn from_this_dashboard(headers: &HeaderMap) -> bool {
    let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let Some(host) = text("host") else {
        return false;
    };
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    let loopback = ["127.0.0.1", "localhost", "::1"]
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed));
    loopback && text("origin").is_none_or(|origin| origin == format!("http://{host}"))
}

/// Why a stop must not act for a link to `host`, if so: only a name that
/// resolves, and only to loopback, is this machine.
fn not_this_machine(host: &str) -> Option<String> {
    let addresses: Vec<_> = match (host, 0).to_socket_addrs() {
        Ok(addresses) => addresses.collect(),
        Err(_) => Vec::new(),
    };
    if addresses.is_empty() {
        Some(format!("{host} does not resolve on this machine"))
    } else if addresses.iter().all(|address| address.ip().is_loopback()) {
        None
    } else {
        Some(format!("{host} is not this machine"))
    }
}

/// For each of `row`'s ports that has a link to another machine, why it is
/// refused.
fn foreign_hosts(row: &Row) -> BTreeMap<u16, String> {
    let mut foreign = BTreeMap::new();
    for cell in &row.cells {
        if let Cell::Link(link) = cell {
            if let Some(reason) = not_this_machine(&link.host) {
                foreign.entry(link.port).or_insert(reason);
            }
        }
    }
    foreign
}

/// Releases a row's "being stopped" mark when the stop ends, however it ends.
struct Stopping {
    rows: Arc<Mutex<BTreeSet<(String, usize)>>>,
    key: (String, usize),
}

impl Drop for Stopping {
    fn drop(&mut self) {
        if let Ok(mut rows) = self.rows.lock() {
            rows.remove(&self.key);
        }
    }
}

fn stop_response(results: &[PortResult]) -> Response {
    let failures: Vec<String> = results
        .iter()
        .filter_map(|result| {
            result
                .error
                .as_ref()
                .map(|error| format!("port {}: {error}", result.port))
        })
        .collect();
    let results: Vec<Value> = results
        .iter()
        .map(|result| {
            json!({
                "port": result.port,
                "action": result.action.as_str(),
                "detail": result.detail,
                "error": result.error,
            })
        })
        .collect();
    if failures.is_empty() {
        Json(json!({ "ok": true, "results": results })).into_response()
    } else {
        let body = json!({ "error": failures.join("; "), "results": results });
        (StatusCode::BAD_GATEWAY, Json(body)).into_response()
    }
}

async fn stop_row(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    if !from_this_dashboard(&headers) {
        return error_response(StatusCode::FORBIDDEN, "stops must come from this dashboard");
    }
    let bad = |error: String| error_response(StatusCode::BAD_REQUEST, error);
    // Parsed here, not by an extractor, so every reply is JSON.
    let request: StopRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(err) => return bad(format!("not a stop request: {err}")),
    };
    let (config, _) = state.reload();
    let local = state.local_label(&config);
    let wanted = request.machine.as_deref().unwrap_or(&local);
    let remotes = config.active_remotes(&local);
    let remote = remotes
        .iter()
        .find(|remote| remote.name.eq_ignore_ascii_case(wanted));
    let machine = match remote {
        Some(remote) => remote.name.clone(),
        None if wanted.eq_ignore_ascii_case(&local) => local.clone(),
        None => return bad(format!("unknown machine {wanted}")),
    };

    // A group or row the page saw and this config lacks: the config changed
    // since the page was painted (or, forwarded, differs between machines).
    let changed = || error_response(StatusCode::CONFLICT, CONFIG_CHANGED);
    let Some(group) = config.groups.iter().find(|group| group.id == request.group) else {
        return changed();
    };
    let Some(row) = group.rows.get(request.row) else {
        return changed();
    };
    let ports = row.ports();
    if ports.is_empty() {
        return bad("that row has nothing to stop".into());
    }
    let seen: BTreeSet<u16> = request.ports.iter().copied().collect();
    if !row.shown_on(&machine) || !seen.into_iter().eq(ports.iter().copied()) {
        return changed();
    }

    if let Some(remote) = remote {
        let body = json!({
            "machine": remote.name,
            "group": request.group,
            "row": request.row,
            "ports": ports,
        })
        .to_string();
        let (socks_port, whatsup_port) = (remote.socks_port, remote.whatsup_port);
        let timeout = state.inner.forward_timeout;
        let reply = tokio::task::spawn_blocking(move || {
            post_json(socks_port, whatsup_port, "/api/stop", &body, timeout)
        })
        .await
        .expect("forwarded stop");
        let name = &remote.name;
        return match reply {
            Ok((status, body)) => match (
                StatusCode::from_u16(status),
                serde_json::from_str::<Value>(&body),
            ) {
                (Ok(status), Ok(mut body))
                    if status != StatusCode::NOT_FOUND && body.is_object() =>
                {
                    // Say whose error it is. This dashboard already checked
                    // the row, so a "config changed" from the remote means
                    // the two machines read different configs.
                    if let Some(error) = body.get("error").and_then(Value::as_str) {
                        body["error"] = Value::String(if error == CONFIG_CHANGED {
                            format!(
                                "{name}'s WhatsUp has a different config for that row; update the config on both machines"
                            )
                        } else {
                            format!("{name}: {error}")
                        });
                    }
                    (status, Json(body)).into_response()
                }
                _ => error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("WhatsUp on {name} cannot stop stacks; update it"),
                ),
            },
            Err(PostError::Connect) => error_response(
                StatusCode::BAD_GATEWAY,
                format!("WhatsUp is not running on {name}"),
            ),
            Err(PostError::Reply(_)) => error_response(
                StatusCode::BAD_GATEWAY,
                format!("{name} did not answer in time; it may still be stopping"),
            ),
        };
    }

    let key = (group.id.clone(), request.row);
    if !state
        .inner
        .stopping
        .lock()
        .expect("stopping lock")
        .insert(key.clone())
    {
        return error_response(StatusCode::CONFLICT, "already stopping");
    }
    let mark = Stopping {
        rows: state.inner.stopping.clone(),
        key,
    };
    let stopper = state.inner.stopper.clone();
    let row = row.clone();
    // A blocking task runs to its end even when the client goes away, and
    // the mark is released there, not here.
    let results = tokio::task::spawn_blocking(move || {
        let _mark = mark;
        let foreign = foreign_hosts(&row);
        let ours: Vec<u16> = ports
            .iter()
            .copied()
            .filter(|port| !foreign.contains_key(port))
            .collect();
        let mut stopped = if ours.is_empty() {
            Vec::new()
        } else {
            stopper.stop(&ours)
        }
        .into_iter();
        ports
            .iter()
            .map(|port| match foreign.get(port) {
                Some(reason) => PortResult {
                    port: *port,
                    action: Action::None,
                    detail: String::new(),
                    error: Some(reason.clone()),
                },
                None => stopped.next().expect("one result per port"),
            })
            .collect::<Vec<_>>()
    })
    .await
    .expect("stop task");
    stop_response(&results)
}

async fn quit(State(state): State<AppState>) -> Json<Value> {
    let shutdown = state.inner.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = shutdown.send(true);
    });
    Json(json!({ "ok": true }))
}

fn view(
    config: &DashboardConfig,
    machines: &[MachineStatus],
    config_path: &str,
    config_error: Option<String>,
) -> Value {
    let local = &machines[0];
    json!({
        "title": config.title,
        "refresh_ms": config.refresh_ms,
        "config_path": config_path,
        "config_error": config_error,
        "checked_at_ms": now_ms(),
        "ports_up": local.counts.0,
        "ports_total": local.counts.1,
        "machine": local.name,
        "show_machine": !config.remotes.is_empty() || !config.machine_names.is_empty(),
        "machines": machines.iter().map(|machine| json!({
            "name": machine.name,
            "local": machine.local,
            "offline": machine.offline,
            "note": machine.note,
            "ports_up": machine.counts.0,
            "ports_total": machine.counts.1,
            "can_stop": machine.can_stop,
        })).collect::<Vec<_>>(),
        "groups": machines
            .iter()
            .flat_map(|machine| {
                config
                    .groups
                    .iter()
                    .filter(move |group| group.rows.iter().any(|row| row.shown_on(&machine.name)))
                    .map(move |group| group_view(group, machine))
            })
            .collect::<Vec<_>>(),
    })
}

fn group_view(group: &Group, machine: &MachineStatus) -> Value {
    json!({
        "id": group.id,
        "key": format!("{}/{}", machine.name, group.id),
        "machine": machine.name,
        "label": group.label,
        "columns": group.columns,
        "rows": group.rows.iter().enumerate().filter(|(_, row)| row.shown_on(&machine.name)).map(|(index, row)| {
            json!({
                // The row's place in the config, which /api/stop takes back
                // together with the ports to prove it means the same row.
                "index": index,
                "ports": row.ports(),
                "tone": match row.tone {
                    Tone::Gray => "gray",
                    Tone::Black => "black",
                },
                "cells": row.cells.iter().map(|cell| match cell {
                    Cell::Empty => json!({ "type": "empty" }),
                    Cell::Text(text) => json!({ "type": "text", "text": text }),
                    Cell::Herdr(cell) => {
                        let labels = machine
                            .herdr
                            .as_deref()
                            .map(|workspaces| matching_labels(workspaces, &cell.prefix))
                            .unwrap_or_default();
                        if labels.is_empty() {
                            json!({ "type": "text", "text": cell.text })
                        } else {
                            json!({ "type": "text", "text": cell.text, "herdr": labels })
                        }
                    }
                    Cell::Link(link) => json!({
                        "type": "link",
                        "label": link.label,
                        "url": link.url,
                        "port": link.port,
                        "up": (machine.up)(link),
                    }),
                }).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener as StdListener, TcpStream};
    use std::path::PathBuf;
    use std::time::Duration;

    struct Recording(Mutex<Vec<String>>);

    impl BrowserOpener for Recording {
        fn open(&self, app: &str, url: &str, mode: OpenMode) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("{app}|{url}|{mode:?}"));
            Ok(())
        }

        fn open_in_profile(&self, app: &str, profile: &str, url: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{app}|{url}|profile:{profile}"));
            Ok(())
        }
    }

    const HOSTNAME: &str = "testhost";

    /// Records the ports it is asked to stop. With a `gate`, each stop waits
    /// for one message first; with `fail`, port 8004 reports an error.
    #[derive(Default)]
    struct RecordingStopper {
        calls: Mutex<Vec<Vec<u16>>>,
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        fail: bool,
    }

    impl RecordingStopper {
        fn calls(&self) -> Vec<Vec<u16>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Stopper for RecordingStopper {
        fn stop(&self, ports: &[u16]) -> Vec<PortResult> {
            self.calls.lock().unwrap().push(ports.to_vec());
            if let Some(gate) = self.gate.lock().unwrap().as_ref() {
                let _ = gate.recv();
            }
            ports
                .iter()
                .map(|port| PortResult {
                    port: *port,
                    action: Action::Process,
                    detail: "pgid 7".into(),
                    error: (self.fail && *port == 8004)
                        .then(|| "docker compose -p p1 stop: boom".to_string()),
                })
                .collect()
        }
    }

    /// Serves `config_json` with a recording opener. Tunnels run a stub that
    /// stays up like a healthy ssh, so probes go to whatever SOCKS port the
    /// config names.
    async fn serve(name: &str, config_json: &str) -> (SocketAddr, Arc<Recording>, PathBuf) {
        let (_, program) = crate::tunnel::testing::running_stub(&format!("server-{name}"));
        serve_with(name, config_json, program).await
    }

    async fn serve_with(
        name: &str,
        config_json: &str,
        program: PathBuf,
    ) -> (SocketAddr, Arc<Recording>, PathBuf) {
        serve_herdr(name, config_json, program, std::env::temp_dir(), None).await
    }

    /// Like `serve_with`, with herdr forwards bound in `socket_dir` and the
    /// local herdr at `local_herdr`.
    async fn serve_herdr(
        name: &str,
        config_json: &str,
        program: PathBuf,
        socket_dir: PathBuf,
        local_herdr: Option<PathBuf>,
    ) -> (SocketAddr, Arc<Recording>, PathBuf) {
        let stopper = Arc::new(RecordingStopper::default());
        serve_all(name, config_json, program, socket_dir, local_herdr, stopper).await
    }

    /// Like `serve_herdr`, with the stopper the test wants to watch.
    async fn serve_all(
        name: &str,
        config_json: &str,
        program: PathBuf,
        socket_dir: PathBuf,
        local_herdr: Option<PathBuf>,
        stopper: Arc<RecordingStopper>,
    ) -> (SocketAddr, Arc<Recording>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("whatsup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        std::fs::write(&config_path, config_json).unwrap();
        let config = load_config(&config_path).unwrap();
        let (shutdown, _rx) = watch::channel(false);
        let opener = Arc::new(Recording(Mutex::new(Vec::new())));
        let tunnels = Arc::new(Tunnels::new(program, socket_dir));
        let state = AppState::new(
            config_path,
            config,
            Actions {
                opener: opener.clone(),
                stopper,
            },
            shutdown,
            HOSTNAME.to_string(),
            tunnels,
            local_herdr,
        )
        .with_forward_timeout(Duration::from_millis(500));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        (addr, opener, dir)
    }

    fn remote_config(socks_port: u16) -> String {
        format!(
            r#"{{
                "remotes": [{{ "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy", "socks_port": {socks_port} }}],
                "groups": [{{
                    "id": "g",
                    "label": "Services",
                    "columns": ["Up", "Down"],
                    "rows": [{{
                        "cells": [
                            {{ "label": "up", "url": "http://localhost:3000/" }},
                            {{ "label": "down", "url": "http://jds.local.me:3001/" }}
                        ]
                    }}]
                }}]
            }}"#
        )
    }

    fn herdr_config(remote: &str) -> String {
        format!(
            r#"{{
                {remote}
                "groups": [{{
                    "id": "g",
                    "label": "Stacks",
                    "columns": ["Workspace", "Portal"],
                    "rows": [{{
                        "cells": [
                            {{ "text": "jds", "herdr": "h1" }},
                            {{ "label": "portal", "url": "http://localhost:1/" }}
                        ]
                    }}]
                }}]
            }}"#
        )
    }

    /// A short socket path in the temp dir (macOS limits them to ~104 bytes).
    fn herdr_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wu-{name}-{}.sock", std::process::id()))
    }

    fn workspace_cell(json: &Value, section: usize) -> Value {
        json["groups"][section]["rows"][0]["cells"][0].clone()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_herdr_label_in_workspace_cell() {
        let socket = herdr_path("local");
        crate::herdr::testing::serve_labels(&socket, &[(3, "h10-e2e-testing"), (2, "h1-herberth")]);
        let (_, program) = crate::tunnel::testing::running_stub("server-herdr-local");
        let (addr, _opener, dir) = serve_herdr(
            "herdr-local",
            &herdr_config(""),
            program,
            std::env::temp_dir(),
            Some(socket.clone()),
        )
        .await;
        assert_eq!(
            workspace_cell(&dashboard_json(addr).await, 0),
            json!({ "type": "text", "text": "jds", "herdr": ["h1-herberth"] })
        );
        let _ = std::fs::remove_file(socket);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_herdr_socket_gives_plain_text() {
        let (addr, _opener, dir) = serve("herdr-none", &herdr_config("")).await;
        assert_eq!(
            workspace_cell(&dashboard_json(addr).await, 0),
            json!({ "type": "text", "text": "jds" })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn herdr_down_does_not_delay_dashboard() {
        let socket = herdr_path("silent");
        crate::herdr::testing::serve_silence(&socket);
        let (_, program) = crate::tunnel::testing::running_stub("server-herdr-silent");
        let (addr, _opener, dir) = serve_herdr(
            "herdr-silent",
            &herdr_config(""),
            program,
            std::env::temp_dir(),
            Some(socket.clone()),
        )
        .await;
        let started = std::time::Instant::now();
        let json = dashboard_json(addr).await;
        assert!(started.elapsed() < crate::herdr::QUERY_BUDGET + Duration::from_millis(500));
        assert_eq!(
            workspace_cell(&json, 0),
            json!({ "type": "text", "text": "jds" })
        );
        let _ = std::fs::remove_file(socket);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn lucy_with_herdr(socks_port: u16) -> String {
        format!(
            r#""remotes": [{{ "name": "lucy", "ssh": "lucy", "browser_profile": "P", "socks_port": {socks_port}, "herdr_socket": "/home/jds/.config/herdr/herdr.sock" }}],"#
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_herdr_label_through_forward() {
        let (stub_dir, program) = crate::tunnel::testing::running_stub("hr");
        let socks_port = 47697;
        crate::herdr::testing::serve_labels(
            &stub_dir.join(format!("herdr-{socks_port}.sock")),
            &[(2, "h1-remote")],
        );
        let (addr, _opener, dir) = serve_herdr(
            "herdr-remote",
            &herdr_config(&lucy_with_herdr(socks_port)),
            program,
            stub_dir.clone(),
            None,
        )
        .await;
        // The tunnel stub needs a moment to count as running.
        let mut json = dashboard_json(addr).await;
        for _ in 0..20 {
            if json["groups"][1]["rows"][0]["cells"][0]["herdr"].is_array() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            json = dashboard_json(addr).await;
        }
        assert_eq!(json["groups"][1]["machine"], "lucy");
        assert_eq!(
            workspace_cell(&json, 1),
            json!({ "type": "text", "text": "jds", "herdr": ["h1-remote"] })
        );
        assert_eq!(
            workspace_cell(&json, 0),
            json!({ "type": "text", "text": "jds" })
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(stub_dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_cells_plain_while_tunnel_not_running() {
        let (stub_dir, program) = crate::tunnel::testing::stub("hd", "exit 1");
        let socks_port = 47698;
        crate::herdr::testing::serve_labels(
            &stub_dir.join(format!("herdr-{socks_port}.sock")),
            &[(2, "h1-stale")],
        );
        let (addr, _opener, dir) = serve_herdr(
            "herdr-down",
            &herdr_config(&lucy_with_herdr(socks_port)),
            program,
            stub_dir.clone(),
            None,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let json = dashboard_json(addr).await;
        assert_eq!(json["groups"][1]["machine"], "lucy");
        assert_eq!(
            workspace_cell(&json, 1),
            json!({ "type": "text", "text": "jds" })
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(stub_dir);
    }

    async fn dashboard_json(addr: SocketAddr) -> Value {
        let body = tokio::task::spawn_blocking(move || http(addr, "GET", "/api/dashboard", None).1)
            .await
            .unwrap();
        serde_json::from_str(body_of(&body)).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_remotes_view_is_unchanged() {
        let (addr, _opener, dir) = serve("plain", crate::DEFAULT_CONFIG_JSON).await;
        let json = dashboard_json(addr).await;
        for key in [
            "title",
            "refresh_ms",
            "config_path",
            "config_error",
            "checked_at_ms",
            "ports_up",
            "ports_total",
            "groups",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        assert_eq!(json["machine"], HOSTNAME);
        assert_eq!(json["show_machine"], false);
        assert_eq!(
            json["machines"],
            serde_json::json!([{
                "name": HOSTNAME,
                "local": true,
                "offline": false,
                "note": null,
                "ports_up": json["ports_up"],
                "ports_total": json["ports_total"],
                "can_stop": true,
            }])
        );
        assert_eq!(json["groups"][0]["machine"], HOSTNAME);
        assert_eq!(json["groups"][0]["key"], format!("{HOSTNAME}/web-stacks"));
        assert_eq!(json["groups"][0]["id"], "web-stacks");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_groups_follow_local_groups() {
        let (socks_port, _requests) = crate::socks::fake::start(|host, port| {
            if host == "localhost" && port == 3000 {
                crate::socks::fake::Act::Reply(0)
            } else {
                crate::socks::fake::Act::Close
            }
        });
        let (addr, _opener, dir) = serve("remote", &remote_config(socks_port)).await;
        let json = dashboard_json(addr).await;
        let keys: Vec<&str> = json["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| group["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, [format!("{HOSTNAME}/g").as_str(), "lucy/g"]);
        // Configs that know about machines always say whose dashboard this is,
        // so lucy's own page opened through a proxied window is recognizable.
        assert_eq!(json["show_machine"], true);
        let lucy = &json["groups"][1];
        assert_eq!(lucy["machine"], "lucy");
        assert_eq!(lucy["rows"][0]["cells"][0]["up"], true);
        assert_eq!(lucy["rows"][0]["cells"][1]["up"], false);
        let machine = &json["machines"][1];
        assert_eq!(machine["name"], "lucy");
        assert_eq!(machine["local"], false);
        assert_eq!(machine["offline"], false);
        assert_eq!(machine["note"], Value::Null);
        assert_eq!(machine["ports_up"], 1);
        assert_eq!(machine["ports_total"], 2);
        // lucy's own WhatsUp did not answer, and is not one of her ports.
        assert_eq!(machine["can_stop"], false);
        assert_eq!(json["machines"][0]["can_stop"], true);
        assert_eq!(lucy["rows"][0]["index"], 0);
        assert_eq!(lucy["rows"][0]["ports"], json!([3000, 3001]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_can_stop_when_its_whatsup_answers() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Reply(0));
        let (addr, _opener, dir) = serve("can-stop", &remote_config(socks_port)).await;
        let json = dashboard_json(addr).await;
        let machine = &json["machines"][1];
        assert_eq!(machine["can_stop"], true);
        assert_eq!(machine["ports_up"], 2);
        assert_eq!(machine["ports_total"], 2, "its WhatsUp is not counted");
        let _ = std::fs::remove_dir_all(dir);
    }

    const STOP_CONFIG: &str = r#"{
        "groups": [{
            "id": "g",
            "label": "Stacks",
            "columns": ["Name", "Portal", "Admin", "Schema"],
            "rows": [
                { "cells": ["text only", null, null, null] },
                { "cells": [
                    "jds4",
                    { "label": "portal", "url": "http://localhost:3004" },
                    { "label": "admin", "url": "http://localhost:8004/admin/" },
                    { "label": "schema", "url": "http://localhost:8004/schema/" }
                ] },
                { "machines": ["lucy"], "cells": [
                    "jds7",
                    { "label": "portal7", "url": "http://localhost:3007" },
                    null,
                    null
                ] },
                { "cells": [
                    "elsewhere",
                    { "label": "far", "url": "http://192.0.2.1:8004/" },
                    { "label": "near", "url": "http://127.0.0.1:3004/" },
                    null
                ] }
            ]
        }]
    }"#;

    async fn serve_stop(name: &str, stopper: Arc<RecordingStopper>) -> (SocketAddr, PathBuf) {
        let (_, program) = crate::tunnel::testing::running_stub(&format!("stop-{name}"));
        let (addr, _opener, dir) = serve_all(
            &format!("stop-{name}"),
            STOP_CONFIG,
            program,
            std::env::temp_dir(),
            None,
            stopper,
        )
        .await;
        (addr, dir)
    }

    /// POSTs `body` to /api/stop with the given Host (default: the address)
    /// and Origin; returns the status and the JSON reply.
    async fn stop(addr: SocketAddr, body: &str, headers: &[(&str, &str)]) -> (u16, Value) {
        let body = body.to_string();
        let host = headers
            .iter()
            .find(|(name, _)| *name == "Host")
            .map(|(_, value)| value.to_string())
            .unwrap_or_else(|| addr.to_string());
        let extra: String = headers
            .iter()
            .filter(|(name, _)| *name != "Host")
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            write!(
                stream,
                "POST /api/stop HTTP/1.1\r\nHost: {host}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
            (
                status,
                serde_json::from_str(body_of(&response)).unwrap_or(Value::Null),
            )
        })
        .await
        .unwrap()
    }

    const JDS4: &str = r#"{"group":"g","row":1,"ports":[3004,8004]}"#;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_runs_the_rows_ports() {
        let stopper = Arc::new(RecordingStopper::default());
        let (addr, dir) = serve_stop("runs", stopper.clone()).await;
        let (status, json) = stop(addr, JDS4, &[]).await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(stopper.calls(), [vec![3004, 8004]]);
        assert_eq!(json["ok"], true);
        assert_eq!(
            json["results"],
            json!([
                { "port": 3004, "action": "process", "detail": "pgid 7", "error": null },
                { "port": 8004, "action": "process", "detail": "pgid 7", "error": null },
            ])
        );
        // The machine may be named, in any case; the port order is free.
        let named = r#"{"machine":"TestHost","group":"g","row":1,"ports":[8004,3004]}"#;
        assert_eq!(stop(addr, named, &[]).await.0, 200);
        assert_eq!(stopper.calls().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_rejects_a_changed_row() {
        let stopper = Arc::new(RecordingStopper::default());
        let (addr, dir) = serve_stop("changed", stopper.clone()).await;
        for body in [
            r#"{"group":"g","row":1,"ports":[3004]}"#,
            r#"{"group":"g","row":1,"ports":[3004,8004,9000]}"#,
            r#"{"group":"g","row":1,"ports":[3005,8005]}"#,
        ] {
            let (status, json) = stop(addr, body, &[]).await;
            assert_eq!(status, 409, "{body}");
            assert_eq!(
                json["error"],
                "the dashboard config changed; reload and try again"
            );
        }
        // So has a row that is no longer on this machine, or gone.
        for body in [
            r#"{"group":"g","row":2,"ports":[3007]}"#,
            r#"{"group":"g","row":9,"ports":[3004,8004]}"#,
            r#"{"group":"nope","row":1,"ports":[3004,8004]}"#,
        ] {
            assert_eq!(stop(addr, body, &[]).await.0, 409, "{body}");
        }
        assert!(stopper.calls().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_rejects_bad_rows() {
        let stopper = Arc::new(RecordingStopper::default());
        let (addr, dir) = serve_stop("bad", stopper.clone()).await;
        for body in [
            r#"{"group":"g","row":0,"ports":[]}"#,
            r#"{"machine":"lucy","group":"g","row":2,"ports":[3007]}"#,
            r#"{"group":"g","row":"one","ports":[3004,8004]}"#,
            "not json",
        ] {
            let (status, json) = stop(addr, body, &[]).await;
            assert_eq!(status, 400, "{body}: {json}");
            assert!(json["error"].is_string(), "{body}");
        }
        assert!(stopper.calls().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_guards_host_and_origin() {
        let stopper = Arc::new(RecordingStopper::default());
        let (addr, dir) = serve_stop("guard", stopper.clone()).await;
        let port = addr.port();
        let rebound = format!("evil.example:{port}");
        let own = format!("http://127.0.0.1:{port}");
        let local = format!("localhost:{port}");
        let local_origin = format!("http://localhost:{port}");
        let forbidden: [&[(&str, &str)]; 4] = [
            &[("Host", &rebound)],
            &[("Origin", "http://evil.example")],
            &[("Origin", "null")],
            &[("Host", &local), ("Origin", &own)],
        ];
        for headers in forbidden {
            assert_eq!(stop(addr, JDS4, headers).await.0, 403, "{headers:?}");
        }
        assert!(stopper.calls().is_empty());
        let allowed: [&[(&str, &str)]; 3] = [
            &[("Origin", &own)],
            &[("Host", &local), ("Origin", &local_origin)],
            &[("Host", "[::1]:47654")],
        ];
        for headers in allowed {
            assert_eq!(stop(addr, JDS4, headers).await.0, 200, "{headers:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn until(what: &str, mut ready: impl FnMut() -> bool) {
        for _ in 0..200 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never happened: {what}");
    }

    fn gated() -> (Arc<RecordingStopper>, std::sync::mpsc::Sender<()>) {
        let (release, gate) = std::sync::mpsc::channel();
        let stopper = Arc::new(RecordingStopper {
            gate: Mutex::new(Some(gate)),
            ..Default::default()
        });
        (stopper, release)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_refuses_a_row_already_stopping() {
        let (stopper, release) = gated();
        let (addr, dir) = serve_stop("twice", stopper.clone()).await;
        let first = tokio::spawn(async move { stop(addr, JDS4, &[]).await });
        until("the first stop started", || stopper.calls().len() == 1).await;
        let (status, json) = stop(addr, JDS4, &[]).await;
        assert_eq!(status, 409);
        assert_eq!(json["error"], "already stopping");
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap().0, 200);
        release.send(()).unwrap();
        assert_eq!(stop(addr, JDS4, &[]).await.0, 200);
        assert_eq!(stopper.calls().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_finishes_after_the_client_leaves() {
        let (stopper, release) = gated();
        let (addr, dir) = serve_stop("leaves", stopper.clone()).await;
        {
            let mut stream = TcpStream::connect(addr).unwrap();
            write!(
                stream,
                "POST /api/stop HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{JDS4}",
                JDS4.len()
            )
            .unwrap();
            until("the stop started", || stopper.calls().len() == 1).await;
        }
        // The client is gone and the stop is still running: the row stays
        // marked until the stop itself ends.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(stop(addr, JDS4, &[]).await.0, 409);
        release.send(()).unwrap();
        release.send(()).unwrap();
        let mut status = 409;
        for _ in 0..100 {
            status = stop(addr, JDS4, &[]).await.0;
            if status != 409 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(status, 200);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_reports_failures() {
        let stopper = Arc::new(RecordingStopper {
            fail: true,
            ..Default::default()
        });
        let (addr, dir) = serve_stop("fails", stopper.clone()).await;
        let (status, json) = stop(addr, JDS4, &[]).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "port 8004: docker compose -p p1 stop: boom");
        assert_eq!(json["results"][0]["error"], Value::Null);
        assert_eq!(
            json["results"][1]["error"],
            "docker compose -p p1 stop: boom"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_refuses_a_host_that_is_not_this_machine() {
        let stopper = Arc::new(RecordingStopper::default());
        let (addr, dir) = serve_stop("foreign", stopper.clone()).await;
        let body = r#"{"group":"g","row":3,"ports":[3004,8004]}"#;
        let (status, json) = stop(addr, body, &[]).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "port 8004: 192.0.2.1 is not this machine");
        assert_eq!(
            not_this_machine("no-such-host.invalid").as_deref(),
            Some("no-such-host.invalid does not resolve on this machine")
        );
        assert_eq!(not_this_machine("localhost"), None);
        assert_eq!(not_this_machine("::1"), None);
        // The port that is on this machine was still stopped.
        assert_eq!(stopper.calls(), [vec![3004]]);
        assert_eq!(json["results"][0]["action"], "process");
        assert_eq!(json["results"][1]["action"], "none");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// megan's view of lucy, whose WhatsUp is at `whatsup_port` behind the
    /// SOCKS proxy at `socks_port`. On lucy the same file names her as local.
    fn forward_config(socks_port: u16, whatsup_port: u16, machine_names: &str) -> String {
        STOP_CONFIG.replacen(
            '{',
            &format!(
                r#"{{ {machine_names}
                "remotes": [{{ "name": "lucy", "ssh": "lucy", "browser_profile": "P",
                    "socks_port": {socks_port}, "whatsup_port": {whatsup_port} }}],"#
            ),
            1,
        )
    }

    async fn serve_config(
        name: &str,
        config: &str,
        stopper: Arc<RecordingStopper>,
    ) -> (SocketAddr, PathBuf) {
        let (_, program) = crate::tunnel::testing::running_stub(name);
        let (addr, _opener, dir) =
            serve_all(name, config, program, std::env::temp_dir(), None, stopper).await;
        (addr, dir)
    }

    const JDS7: &str = r#"{"machine":"Lucy","group":"g","row":2,"ports":[3007]}"#;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stop_is_forwarded_to_the_remote() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Relay);
        let lucy_stopper = Arc::new(RecordingStopper::default());
        let (lucy, lucy_dir) = serve_config(
            "fwd-lucy",
            &forward_config(1, 1, r#""machine_names": { "testhost": "lucy" },"#),
            lucy_stopper.clone(),
        )
        .await;
        let megan_stopper = Arc::new(RecordingStopper::default());
        let (megan, megan_dir) = serve_config(
            "fwd-megan",
            &forward_config(socks_port, lucy.port(), ""),
            megan_stopper.clone(),
        )
        .await;
        let (status, json) = stop(megan, JDS7, &[]).await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["results"][0]["port"], 3007);
        assert_eq!(lucy_stopper.calls(), [vec![3007]]);
        assert!(megan_stopper.calls().is_empty());
        // A row both machines have is lucy's too.
        let shared = r#"{"machine":"lucy","group":"g","row":1,"ports":[3004,8004]}"#;
        assert_eq!(stop(megan, shared, &[]).await.0, 200);
        assert_eq!(lucy_stopper.calls().len(), 2);
        let _ = std::fs::remove_dir_all(lucy_dir);
        let _ = std::fs::remove_dir_all(megan_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forwarded_errors_name_the_remote() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Relay);
        // lucy's checkout of the shared config is behind: her row 2 differs.
        let lucy_config = forward_config(1, 1, r#""machine_names": { "testhost": "lucy" },"#)
            .replace("3007", "3008");
        let lucy_stopper = Arc::new(RecordingStopper {
            fail: true,
            ..Default::default()
        });
        let (lucy, lucy_dir) = serve_config("skew-lucy", &lucy_config, lucy_stopper.clone()).await;
        let (megan, megan_dir) = serve_config(
            "skew-megan",
            &forward_config(socks_port, lucy.port(), ""),
            Arc::new(RecordingStopper::default()),
        )
        .await;
        let (status, json) = stop(megan, JDS7, &[]).await;
        assert_eq!(status, 409);
        assert_eq!(
            json["error"],
            "lucy's WhatsUp has a different config for that row; update the config on both machines"
        );
        assert!(lucy_stopper.calls().is_empty());
        let failing = r#"{"machine":"lucy","group":"g","row":1,"ports":[3004,8004]}"#;
        let (status, json) = stop(megan, failing, &[]).await;
        assert_eq!(status, 502);
        assert_eq!(
            json["error"],
            "lucy: port 8004: docker compose -p p1 stop: boom"
        );
        let _ = std::fs::remove_dir_all(lucy_dir);
        let _ = std::fs::remove_dir_all(megan_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_says_when_the_remote_is_not_running() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Close);
        let stopper = Arc::new(RecordingStopper::default());
        let (megan, dir) = serve_config(
            "fwd-down",
            &forward_config(socks_port, 47654, ""),
            stopper.clone(),
        )
        .await;
        let (status, json) = stop(megan, JDS7, &[]).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "WhatsUp is not running on lucy");
        assert!(stopper.calls().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// megan forwarding to something on lucy's WhatsUp port that answers
    /// each request with `reply` (or, with `None`, never).
    async fn stop_through(name: &str, reply: Option<&'static str>) -> (u16, Value) {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Relay);
        let other = StdListener::bind("127.0.0.1:0").unwrap();
        let other_port = other.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = other.accept().unwrap();
            let mut seen = [0u8; 1024];
            let _ = stream.read(&mut seen);
            match reply {
                Some(reply) => {
                    let _ = stream.write_all(reply.as_bytes());
                }
                None => std::thread::sleep(Duration::from_secs(5)),
            }
        });
        let (megan, dir) = serve_config(
            name,
            &forward_config(socks_port, other_port, ""),
            Arc::new(RecordingStopper::default()),
        )
        .await;
        let reply = stop(megan, JDS7, &[]).await;
        let _ = std::fs::remove_dir_all(dir);
        reply
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_says_when_the_remote_cannot_stop() {
        // An older WhatsUp has no /api/stop; anything else on the port does
        // not speak JSON.
        for (name, reply) in [
            (
                "fwd-old",
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n",
            ),
            (
                "fwd-other",
                "HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello",
            ),
        ] {
            let (status, json) = stop_through(name, Some(reply)).await;
            assert_eq!(status, 502, "{name}");
            assert_eq!(
                json["error"],
                "WhatsUp on lucy cannot stop stacks; update it"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_says_when_the_remote_takes_too_long() {
        let started = std::time::Instant::now();
        let (status, json) = stop_through("fwd-slow", None).await;
        assert_eq!(status, 502);
        assert_eq!(
            json["error"],
            "lucy did not answer in time; it may still be stopping"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn machine_rows_show_only_on_their_machine() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Reply(0));
        let config = format!(
            r#"{{
                "remotes": [{{ "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy", "socks_port": {socks_port} }}],
                "groups": [{{
                    "id": "g",
                    "label": "Services",
                    "columns": ["Up", "Down"],
                    "rows": [
                        {{ "cells": [
                            {{ "label": "up", "url": "http://localhost:3000/" }},
                            {{ "label": "down", "url": "http://jds.local.me:3001/" }}
                        ] }},
                        {{ "machines": ["lucy"], "cells": [
                            {{ "label": "lucy only", "url": "http://localhost:3009/" }},
                            null
                        ] }}
                    ]
                }}]
            }}"#
        );
        let (addr, opener, dir) = serve("rows", &config).await;
        let json = dashboard_json(addr).await;
        let rows = |key: &str| {
            json["groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|group| group["key"] == key)
                .map(|group| group["rows"].as_array().unwrap().len())
                .unwrap()
        };
        assert_eq!(rows(&format!("{HOSTNAME}/g")), 1);
        assert_eq!(rows("lucy/g"), 2);
        assert_eq!(
            json["ports_total"], 2,
            "the lucy-only port is not probed locally"
        );
        assert_eq!(json["machines"][1]["ports_total"], 3);

        let post = move |body: &'static str| {
            tokio::task::spawn_blocking(move || http(addr, "POST", "/api/open", Some(body)).0)
        };
        assert_eq!(
            post(r#"{"url":"http://localhost:3009/"}"#).await.unwrap(),
            400
        );
        assert_eq!(
            post(r#"{"url":"http://localhost:3009/","machine":"lucy"}"#)
                .await
                .unwrap(),
            200
        );
        assert_eq!(opener.0.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failing_tunnel_is_reported_even_when_probes_work() {
        // Probes succeed through a proxy someone else left on the port, while
        // our own ssh keeps failing: say so instead of looking healthy.
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Reply(0));
        let (_, program) = crate::tunnel::testing::stub(
            "server-taken",
            "echo 'bind [127.0.0.1]:1: Address already in use' >&2\nexit 255",
        );
        let (addr, _opener, dir) = serve_with("taken", &remote_config(socks_port), program).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let json = dashboard_json(addr).await;
        let machine = &json["machines"][1];
        assert_eq!(machine["offline"], false);
        let note = machine["note"].as_str().unwrap_or_default();
        assert!(note.contains("Address already in use"), "{machine}");
        assert_eq!(json["groups"][1]["rows"][0]["cells"][0]["up"], true);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dashboard_is_not_slowed_by_a_dead_remote() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Silent);
        let (addr, _opener, dir) = serve("dead", &remote_config(socks_port)).await;
        let started = std::time::Instant::now();
        let json = dashboard_json(addr).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let machine = &json["machines"][1];
        assert_eq!(machine["offline"], true);
        assert!(
            machine["note"]
                .as_str()
                .unwrap()
                .starts_with("lucy offline"),
            "{machine}"
        );
        assert_eq!(json["groups"][1]["rows"][0]["cells"][0]["up"], false);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_routes_by_machine() {
        let (addr, opener, dir) = serve("open", &remote_config(1)).await;
        let post = move |body: &'static str| {
            tokio::task::spawn_blocking(move || http(addr, "POST", "/api/open", Some(body)).0)
        };
        assert_eq!(
            post(r#"{"url":"http://localhost:3000/"}"#).await.unwrap(),
            200
        );
        assert_eq!(
            post(r#"{"url":"http://localhost:3000/","machine":"testhost"}"#)
                .await
                .unwrap(),
            200
        );
        assert_eq!(
            post(r#"{"url":"http://localhost:3000/","machine":"lucy"}"#)
                .await
                .unwrap(),
            200
        );
        assert_eq!(
            post(r#"{"url":"http://localhost:3000/","machine":"nope"}"#)
                .await
                .unwrap(),
            400
        );
        assert_eq!(
            post(r#"{"url":"http://example.com/","machine":"lucy"}"#)
                .await
                .unwrap(),
            400
        );
        assert_eq!(
            opener.0.lock().unwrap().as_slice(),
            [
                "Google Chrome|http://localhost:3000/|Tab".to_string(),
                "Google Chrome|http://localhost:3000/|Tab".to_string(),
                "Google Chrome|http://localhost:3000/|profile:megan:Lucy".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dashboard_status_follows_the_listener_and_open_stays_on_the_config() {
        let live = StdListener::bind(("127.0.0.1", 0)).unwrap();
        let live_port = live.local_addr().unwrap().port();
        // Port 1 needs root to bind and is never handed out as an ephemeral
        // port, so unlike a freed port it cannot be grabbed by a test
        // running in parallel.
        let closed_port = 1;

        let dir = std::env::temp_dir().join(format!("whatsup-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        std::fs::write(
            &config_path,
            format!(
                r#"{{
                    "open": "window",
                    "groups": [{{
                        "id": "g",
                        "label": "Services",
                        "columns": ["Name", "Up", "Down"],
                        "rows": [{{
                            "cells": [
                                "sample",
                                {{ "label": "live", "url": "http://127.0.0.1:{live_port}/" }},
                                {{ "label": "dead", "url": "http://127.0.0.1:{closed_port}/" }}
                            ]
                        }}]
                    }}]
                }}"#
            ),
        )
        .unwrap();
        let config = load_config(&config_path).unwrap();
        let (shutdown, _rx) = watch::channel(false);
        let opener = Arc::new(Recording(Mutex::new(Vec::new())));
        let state = AppState::new(
            config_path.clone(),
            config,
            Actions {
                opener: opener.clone(),
                stopper: Arc::new(RecordingStopper::default()),
            },
            shutdown,
            HOSTNAME.to_string(),
            Arc::new(Tunnels::new(PathBuf::from("true"), std::env::temp_dir())),
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });

        let body = http(addr, "GET", "/api/dashboard", None).1;
        let json: Value = serde_json::from_str(body_of(&body)).unwrap();
        let cells = &json["groups"][0]["rows"][0]["cells"];
        assert_eq!(cells[1]["up"], true);
        assert_eq!(cells[1]["port"], live_port);
        assert_eq!(cells[2]["up"], false);
        assert_eq!(json["ports_up"], 1);
        assert_eq!(json["ports_total"], 2);

        let (status, _) = http(
            addr,
            "POST",
            "/api/open",
            Some(r#"{"url":"http://example.com"}"#),
        );
        assert_eq!(status, 400);

        let live_url = format!("http://127.0.0.1:{live_port}/");
        let (status, _) = http(
            addr,
            "POST",
            "/api/open",
            Some(&format!(r#"{{"url":"{live_url}"}}"#)),
        );
        assert_eq!(status, 200);
        assert_eq!(
            opener.0.lock().unwrap().as_slice(),
            [format!("Google Chrome|{live_url}|Window")]
        );
        let _keep = live;
        let _ = std::fs::remove_dir_all(&dir);
        let _ = PathBuf::from(&config_path);
    }

    fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        let body = body.unwrap_or("");
        let extra = if method == "POST" {
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            )
        } else {
            String::new()
        };
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra}\r\n{body}"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, response)
    }

    fn body_of(response: &str) -> &str {
        response.split("\r\n\r\n").nth(1).unwrap_or("")
    }

    #[test]
    fn index_is_the_dashboard_document() {
        assert!(DASHBOARD_HTML.contains("WhatsUp"));
    }
}

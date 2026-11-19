use std::collections::{BTreeMap, BTreeSet};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
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
use crate::load::{self, Load};
use crate::probe::{port_is_open, probe_ports, CONNECT_TIMEOUT};
use crate::socks::{get_json, post_json, probe_remote, PostError, Reach};
use crate::start::{PortStart, StartKind, Starter};
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
    starter: Arc<dyn Starter>,
    /// Whether a link to (host, port) is up, which a start skips.
    probe: Probe,
    /// Rows being started or stopped right now, as (group id, row index),
    /// with what is happening to them ("starting" or "stopping").
    busy: Arc<Mutex<BTreeMap<(String, usize), &'static str>>>,
    /// How long a stop or start forwarded to a remote WhatsUp may take.
    stop_forward_timeout: Duration,
    start_forward_timeout: Duration,
    /// This machine's latest load sample.
    load: LoadReader,
}

type Probe = Arc<dyn Fn(&str, u16) -> bool + Send + Sync>;

/// Reads this machine's latest load sample.
pub type LoadReader = Arc<dyn Fn() -> Load + Send + Sync>;

/// What the dashboard does to the machine it runs on.
pub struct Actions {
    pub opener: Arc<dyn BrowserOpener>,
    pub stopper: Arc<dyn Stopper>,
    pub starter: Arc<dyn Starter>,
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
                starter: actions.starter,
                probe: Arc::new(is_up),
                busy: Arc::default(),
                stop_forward_timeout: STOP_FORWARD_TIMEOUT,
                start_forward_timeout: START_FORWARD_TIMEOUT,
                load: Arc::new(Load::default),
            }),
        }
    }

    fn unshared(&mut self) -> &mut Inner {
        Arc::get_mut(&mut self.inner).expect("the state is not shared yet")
    }

    /// Replaces how long a forwarded stop or start may take (for tests).
    /// Only before the state is shared.
    pub fn with_forward_timeout(mut self, timeout: Duration) -> Self {
        let inner = self.unshared();
        inner.stop_forward_timeout = timeout;
        inner.start_forward_timeout = timeout;
        self
    }

    /// Replaces how a start decides a link is already up (for tests). Only
    /// before the state is shared.
    pub fn with_probe(mut self, probe: Probe) -> Self {
        self.unshared().probe = probe;
        self
    }

    /// Replaces where this machine's load comes from (the sampler in main,
    /// a fixture in tests). Only before the state is shared.
    pub fn with_load(mut self, load: LoadReader) -> Self {
        self.unshared().load = load;
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
        .route("/api/load", get(machine_load))
        .route("/api/open", post(open_link))
        .route("/api/start", post(start_row))
        .route("/api/stop", post(stop_row))
        .route("/api/quit", post(quit))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// This machine's latest load sample, which a remote dashboard shows.
async fn machine_load(State(state): State<AppState>) -> Json<Load> {
    Json((state.inner.load)())
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
            // starts and stops that machine's rows.
            let socks_port = remote.socks_port;
            let targets: Vec<(String, u16)> = remote_links(&config, &remote.name)
                .into_iter()
                .chain([remote_whatsup(remote)])
                .collect();
            tokio::task::spawn_blocking(move || probe_remote(socks_port, targets))
        })
        .collect();
    // Each remote's load from its own WhatsUp, fetched alongside the probes.
    let remote_loads: Vec<_> = remotes
        .iter()
        .map(|remote| {
            let (socks_port, whatsup_port) = (remote.socks_port, remote.whatsup_port);
            tokio::task::spawn_blocking(move || {
                get_json(socks_port, whatsup_port, "/api/load", LOAD_READ_TIMEOUT)
            })
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
        can_act: true,
        herdr: herdr_result(local_herdr).await,
        load: Some((state.inner.load)()),
        load_note: None,
    }];
    machines[0].counts = count_local(&config, &machines[0]);
    let fetched = remote_probes
        .into_iter()
        .zip(remote_herdr)
        .zip(remote_loads);
    for (remote, ((probe, herdr), load)) in remotes.iter().zip(fetched) {
        let reach = probe.await.expect("remote probe");
        let (load, load_note) = remote_load(&remote.name, load.await.expect("remote load"));
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
        let can_act = up(&remote_whatsup(remote));
        machines.push(MachineStatus {
            name: remote.name.clone(),
            local: false,
            offline,
            note,
            up: Box::new(move |link: &Link| {
                !offline && reach.get(&(link.host.clone(), link.port)) == Some(&Reach::Up)
            }),
            counts,
            can_act,
            herdr: herdr_result(herdr).await,
            load,
            load_note,
        });
    }
    Json(view(&config, &machines, &path, error))
}

/// How long a remote WhatsUp may take to send its load once connected.
const LOAD_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// A remote's load from its `/api/load` reply. An older WhatsUp (404) gets
/// a note; anything else missing is already explained by the machine's own
/// offline or can_act state.
fn remote_load(
    name: &str,
    reply: Result<(u16, String), PostError>,
) -> (Option<Load>, Option<String>) {
    match reply {
        Ok((200, body)) => (serde_json::from_str(&body).ok(), None),
        Ok((404, _)) => (
            None,
            Some(format!("update WhatsUp on {name} to see its load")),
        ),
        _ => (None, None),
    }
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
    /// Whether this dashboard can start and stop the machine's rows.
    can_act: bool,
    /// The machine's herdr workspaces, when they could be read.
    herdr: Option<Vec<HerdrWorkspace>>,
    /// The machine's CPU, memory and disk use, when known.
    load: Option<Load>,
    /// Why a remote machine's load is missing, when the user can fix it.
    load_note: Option<String>,
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

/// A start or stop of one row, as the dashboard page (or a WhatsUp
/// forwarding it) sends it.
#[derive(Deserialize)]
struct RowRequest {
    #[serde(default)]
    machine: Option<String>,
    group: String,
    row: usize,
    ports: Vec<u16>,
}

/// What a row request does. Start and Stop share every check, the busy mark
/// and the forwarding; they differ in the work and the words.
#[derive(Clone, Copy)]
enum Act {
    Start,
    Stop,
}

impl Act {
    fn verb(self) -> &'static str {
        match self {
            Act::Start => "start",
            Act::Stop => "stop",
        }
    }

    /// What a row is doing while this runs, as "already ..." says it.
    fn doing(self) -> &'static str {
        match self {
            Act::Start => "starting",
            Act::Stop => "stopping",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Act::Start => "/api/start",
            Act::Stop => "/api/stop",
        }
    }
}

pub(crate) const CONFIG_CHANGED: &str = "the dashboard config changed; reload and try again";

/// How long a forwarded stop may take: the remote's own worst case (the
/// docker listing, a docker stop, a kill) and some room.
const STOP_FORWARD_TIMEOUT: Duration = Duration::from_secs(90);

/// How long a forwarded start may take: the remote's own worst case (about
/// 130 s: the docker listing and a compose start beside two herdr links)
/// and some room.
const START_FORWARD_TIMEOUT: Duration = Duration::from_secs(180);

fn error_response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(json!({ "error": error.into() }))).into_response()
}

/// Starts and stops are for this dashboard's own page (or a WhatsUp
/// forwarding one), not for any site the browser happens to have open: the
/// request must be addressed to loopback by name, and come from this origin
/// if it names one.
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

/// Why a start or stop must not act for a link to `host`, if so: only a
/// name that resolves, and only to loopback, is this machine.
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

/// A port left alone because of `reason`.
fn refused(port: u16, reason: &str) -> PortResult {
    PortResult {
        port,
        action: Action::None,
        detail: String::new(),
        error: Some(reason.to_string()),
    }
}

/// Whether a link to `host` on `port` is up, agreeing with the dashboard's
/// green: localhost answers on `port` (IPv4 or IPv6), or the link's host
/// does.
fn is_up(host: &str, port: u16) -> bool {
    port_is_open(port) || link_is_up(host, port)
}

/// Whether any address `host` resolves to accepts a TCP connection on `port`.
fn link_is_up(host: &str, port: u16) -> bool {
    (host, port).to_socket_addrs().is_ok_and(|mut addresses| {
        addresses.any(|address| TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_ok())
    })
}

/// What starting `row` takes on this machine: an entry for each port to
/// start, in port order, and the results already decided: a link to
/// another machine (not probed), or a port that is `up` already.
fn plan_start(row: &Row, up: impl Fn(&str, u16) -> bool) -> (Vec<PortStart>, Vec<PortResult>) {
    let foreign = foreign_hosts(row);
    let links: Vec<&Link> = row
        .cells
        .iter()
        .filter_map(|cell| match cell {
            Cell::Link(link) => Some(link),
            _ => None,
        })
        .collect();
    let (mut plan, mut decided) = (Vec::new(), Vec::new());
    for port in row.ports() {
        if let Some(reason) = foreign.get(&port) {
            decided.push(refused(port, reason));
            continue;
        }
        let on_port: Vec<&Link> = links
            .iter()
            .copied()
            .filter(|link| link.port == port)
            .collect();
        let hosts: BTreeSet<&str> = on_port.iter().map(|link| link.host.as_str()).collect();
        if hosts.into_iter().any(|host| up(host, port)) {
            decided.push(PortResult {
                port,
                action: Action::None,
                detail: "already up".into(),
                error: None,
            });
            continue;
        }
        // The config allows one start command per port, and only in a row
        // with a herdr cell to name the workspace.
        let command = on_port
            .iter()
            .find_map(|link| Some((link, link.start.as_ref()?)));
        let kind = match (command, row.herdr_prefix()) {
            (Some((_, start)), Some(prefix)) => StartKind::Herdr {
                prefix: prefix.to_string(),
                label: start.tab.clone(),
                dir: PathBuf::from(&start.dir),
                run: start.run.clone(),
            },
            _ => StartKind::Docker,
        };
        plan.push(PortStart { port, kind });
    }
    (plan, decided)
}

/// Starts what serves `row`'s ports that are down; one result per port, in
/// port order.
fn start_ports(
    starter: &dyn Starter,
    row: &Row,
    up: &dyn Fn(&str, u16) -> bool,
) -> Vec<PortResult> {
    let (plan, decided) = plan_start(row, up);
    let started = if plan.is_empty() {
        Vec::new()
    } else {
        starter.start(&plan)
    };
    let mut results: Vec<PortResult> = decided.into_iter().chain(started).collect();
    results.sort_by_key(|result| result.port);
    results
}

/// Stops what serves `row`'s ports; one result per port, in port order.
fn stop_ports(stopper: &dyn Stopper, row: &Row) -> Vec<PortResult> {
    let ports = row.ports();
    let foreign = foreign_hosts(row);
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
            Some(reason) => refused(*port, reason),
            None => stopped.next().expect("one result per port"),
        })
        .collect()
}

/// Releases a row's busy mark when its start or stop ends, however it ends.
struct Busy {
    rows: Arc<Mutex<BTreeMap<(String, usize), &'static str>>>,
    key: (String, usize),
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Ok(mut rows) = self.rows.lock() {
            rows.remove(&self.key);
        }
    }
}

fn action_response(results: &[PortResult]) -> Response {
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

async fn start_row(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    act_on_row(state, headers, body, Act::Start).await
}

async fn stop_row(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    act_on_row(state, headers, body, Act::Stop).await
}

async fn act_on_row(state: AppState, headers: HeaderMap, body: String, act: Act) -> Response {
    let verb = act.verb();
    if !from_this_dashboard(&headers) {
        return error_response(
            StatusCode::FORBIDDEN,
            format!("{verb}s must come from this dashboard"),
        );
    }
    let bad = |error: String| error_response(StatusCode::BAD_REQUEST, error);
    // Parsed here, not by an extractor, so every reply is JSON.
    let request: RowRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(err) => return bad(format!("not a {verb} request: {err}")),
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
        return bad(format!("that row has nothing to {verb}"));
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
        let timeout = match act {
            Act::Start => state.inner.start_forward_timeout,
            Act::Stop => state.inner.stop_forward_timeout,
        };
        return forward(act, remote, body, timeout).await;
    }

    let key = (group.id.clone(), request.row);
    let mark = {
        let mut busy = state.inner.busy.lock().expect("busy lock");
        if let Some(doing) = busy.get(&key) {
            return error_response(StatusCode::CONFLICT, format!("already {doing}"));
        }
        busy.insert(key.clone(), act.doing());
        Busy {
            rows: state.inner.busy.clone(),
            key,
        }
    };
    let inner = state.inner.clone();
    let row = row.clone();
    // A blocking task runs to its end even when the client goes away, and
    // the mark is released there, not here.
    let results = tokio::task::spawn_blocking(move || {
        let _mark = mark;
        match act {
            Act::Start => start_ports(&*inner.starter, &row, &*inner.probe),
            Act::Stop => stop_ports(&*inner.stopper, &row),
        }
    })
    .await
    .expect("row task");
    action_response(&results)
}

/// Sends a start or stop of one of `remote`'s rows to that machine's own
/// WhatsUp, and says whose error it is when it fails.
async fn forward(act: Act, remote: &Remote, body: String, timeout: Duration) -> Response {
    let (socks_port, whatsup_port) = (remote.socks_port, remote.whatsup_port);
    let path = act.path();
    let reply = tokio::task::spawn_blocking(move || {
        post_json(socks_port, whatsup_port, path, &body, timeout)
    })
    .await
    .expect("forwarded request");
    let name = &remote.name;
    match reply {
        Ok((status, body)) => match (
            StatusCode::from_u16(status),
            serde_json::from_str::<Value>(&body),
        ) {
            (Ok(status), Ok(mut body)) if status != StatusCode::NOT_FOUND && body.is_object() => {
                // Say whose error it is. This dashboard already checked the
                // row, so a "config changed" from the remote means the two
                // machines read different configs.
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
                format!("WhatsUp on {name} cannot {} stacks; update it", act.verb()),
            ),
        },
        Err(PostError::Connect) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("WhatsUp is not running on {name}"),
        ),
        Err(PostError::Timeout) => error_response(
            StatusCode::BAD_GATEWAY,
            format!(
                "{name} did not answer in time; it may still be {}",
                act.doing()
            ),
        ),
        Err(PostError::Reply(reason)) => error_response(
            StatusCode::BAD_GATEWAY,
            format!("{name} did not reply to the {}: {reason}", act.verb()),
        ),
    }
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
            "can_act": machine.can_act,
            "load": machine.load.as_ref().map(load::view),
            "load_note": machine.load_note,
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
        "hidden_columns": group.hidden_columns,
        "rows": group.rows.iter().enumerate().filter(|(_, row)| row.shown_on(&machine.name)).map(|(index, row)| {
            json!({
                // The row's place in the config, which /api/start and /api/stop
                // take back together with the ports to prove it means the
                // same row.
                "index": index,
                "id": row.id,
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

    /// Records the plans it is asked to start. With a `gate`, each start
    /// waits for one message first; with `fail`, that port reports "boom".
    #[derive(Default)]
    struct RecordingStarter {
        plans: Mutex<Vec<Vec<PortStart>>>,
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        fail: Option<u16>,
    }

    impl RecordingStarter {
        fn plans(&self) -> Vec<Vec<PortStart>> {
            self.plans.lock().unwrap().clone()
        }
    }

    impl Starter for RecordingStarter {
        fn start(&self, plan: &[PortStart]) -> Vec<PortResult> {
            self.plans.lock().unwrap().push(plan.to_vec());
            if let Some(gate) = self.gate.lock().unwrap().as_ref() {
                let _ = gate.recv();
            }
            plan.iter()
                .map(|entry| PortResult {
                    port: entry.port,
                    action: match entry.kind {
                        StartKind::Docker => Action::Compose,
                        StartKind::Herdr { .. } => Action::Herdr,
                    },
                    detail: "started".into(),
                    error: (self.fail == Some(entry.port)).then(|| "boom".to_string()),
                })
                .collect()
        }
    }

    /// What a test server does instead of acting on this machine. Unless
    /// `real_probe`, every port counts as down, so the dev servers running
    /// here cannot change a start's plan.
    #[derive(Default)]
    struct Fakes {
        stopper: Arc<RecordingStopper>,
        starter: Arc<RecordingStarter>,
        real_probe: bool,
        load: Option<LoadReader>,
    }

    impl Fakes {
        fn stopping(stopper: Arc<RecordingStopper>) -> Self {
            Self {
                stopper,
                ..Default::default()
            }
        }

        fn starting(starter: Arc<RecordingStarter>) -> Self {
            Self {
                starter,
                ..Default::default()
            }
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
        let fakes = Fakes::default();
        serve_all(name, config_json, program, socket_dir, local_herdr, fakes).await
    }

    /// Like `serve_herdr`, with the fakes the test wants to watch.
    async fn serve_all(
        name: &str,
        config_json: &str,
        program: PathBuf,
        socket_dir: PathBuf,
        local_herdr: Option<PathBuf>,
        fakes: Fakes,
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
        let mut state = AppState::new(
            config_path,
            config,
            Actions {
                opener: opener.clone(),
                stopper: fakes.stopper,
                starter: fakes.starter,
            },
            shutdown,
            HOSTNAME.to_string(),
            tunnels,
            local_herdr,
        )
        .with_forward_timeout(Duration::from_millis(500));
        if !fakes.real_probe {
            state = state.with_probe(Arc::new(|_, _| false));
        }
        if let Some(load) = fakes.load {
            state = state.with_load(load);
        }
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
                "can_act": true,
                "load": { "cpu": null, "memory": null, "disk": null },
                "load_note": null,
            }])
        );
        assert_eq!(json["groups"][0]["machine"], HOSTNAME);
        assert_eq!(json["groups"][0]["key"], format!("{HOSTNAME}/web-stacks"));
        assert_eq!(json["groups"][0]["id"], "web-stacks");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn load_fixture() -> crate::load::Load {
        crate::load::Load {
            cpu: Some(crate::load::Cpu {
                percent: 23.0,
                cores: 12,
            }),
            memory: Some(crate::load::Memory {
                percent: 50.0,
                total: 36 << 30,
                swap_used: 468 << 20,
            }),
            disk: Some(crate::load::Disk {
                percent: 88.4,
                total: 460 << 30,
                free: 53 << 30,
                path: "/Users/jds".into(),
            }),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_load_is_in_the_view() {
        let (_, program) = crate::tunnel::testing::running_stub("server-local-load");
        let fakes = Fakes {
            load: Some(Arc::new(load_fixture)),
            ..Default::default()
        };
        let (addr, _opener, dir) = serve_all(
            "local-load",
            crate::DEFAULT_CONFIG_JSON,
            program,
            std::env::temp_dir(),
            None,
            fakes,
        )
        .await;
        let json = dashboard_json(addr).await;
        let machine = &json["machines"][0];
        assert_eq!(machine["load"]["disk"]["level"], "warn");
        assert_eq!(machine["load"]["cpu"]["percent"], 23.0);
        assert_eq!(machine["load_note"], Value::Null);
        let body = tokio::task::spawn_blocking(move || http(addr, "GET", "/api/load", None).1)
            .await
            .unwrap();
        let served: crate::load::Load = serde_json::from_str(body_of(&body)).unwrap();
        assert_eq!(served, load_fixture());
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
        assert_eq!(machine["can_act"], false);
        assert_eq!(json["machines"][0]["can_act"], true);
        assert_eq!(lucy["rows"][0]["index"], 0);
        assert_eq!(lucy["rows"][0]["ports"], json!([3000, 3001]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_can_act_when_its_whatsup_answers() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Reply(0));
        let (addr, _opener, dir) = serve("can-act", &remote_config(socks_port)).await;
        let json = dashboard_json(addr).await;
        let machine = &json["machines"][1];
        assert_eq!(machine["can_act"], true);
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
                { "id": "jds4", "cells": [
                    { "text": "jds4", "herdr": "h4" },
                    { "label": "portal", "url": "http://localhost:3004",
                      "start": { "dir": "~/workspace/huvr4", "run": "tt4a" } },
                    { "label": "admin", "url": "http://localhost:8004/admin/" },
                    { "label": "schema", "url": "http://localhost:8004/schema/" }
                ] },
                { "id": "jds7", "machines": ["lucy"], "cells": [
                    { "text": "jds7", "herdr": "h7" },
                    { "label": "portal7", "url": "http://localhost:3007",
                      "start": { "dir": "~/workspace/huvr7", "run": "tt7a" } },
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
        }, {
            "id": "pc",
            "label": "Pipeline",
            "columns": ["Workspace", "Portal", "Calc docs", "Calc", "Admin", "Schema", "Api"],
            "rows": [
                { "id": "jds2", "cells": [
                    { "text": "jds2", "herdr": "h2" },
                    { "label": "portal", "url": "http://localhost:3002",
                      "start": { "dir": "~/workspace/huvr2", "run": "tt2a && cd frontend && npm start" } },
                    { "label": "calc docs", "url": "http://localhost:4002/docs/" },
                    { "label": "tt-calc-app", "url": "http://localhost:4002",
                      "start": { "dir": "~/workspace/tt-calc-app2", "run": "ttc2a" } },
                    { "label": "admin", "url": "http://localhost:8002/admin/" },
                    { "label": "schema", "url": "http://localhost:8002/schema/" },
                    { "label": "api", "url": "http://localhost:8002/api/" }
                ] }
            ]
        }]
    }"#;

    async fn serve_stop(name: &str, stopper: Arc<RecordingStopper>) -> (SocketAddr, PathBuf) {
        serve_config(
            &format!("stop-{name}"),
            STOP_CONFIG,
            Fakes::stopping(stopper),
        )
        .await
    }

    /// POSTs `body` to /api/stop with the given Host (default: the address)
    /// and Origin; returns the status and the JSON reply.
    async fn stop(addr: SocketAddr, body: &str, headers: &[(&str, &str)]) -> (u16, Value) {
        send(addr, "/api/stop", body, headers).await
    }

    /// Like `stop`, to /api/start.
    async fn start(addr: SocketAddr, body: &str, headers: &[(&str, &str)]) -> (u16, Value) {
        send(addr, "/api/start", body, headers).await
    }

    async fn send(
        addr: SocketAddr,
        path: &'static str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (u16, Value) {
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
                "POST {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

    async fn serve_config(name: &str, config: &str, fakes: Fakes) -> (SocketAddr, PathBuf) {
        let (_, program) = crate::tunnel::testing::running_stub(name);
        let (addr, _opener, dir) =
            serve_all(name, config, program, std::env::temp_dir(), None, fakes).await;
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
            Fakes::stopping(lucy_stopper.clone()),
        )
        .await;
        let megan_stopper = Arc::new(RecordingStopper::default());
        let (megan, megan_dir) = serve_config(
            "fwd-megan",
            &forward_config(socks_port, lucy.port(), ""),
            Fakes::stopping(megan_stopper.clone()),
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
        let (lucy, lucy_dir) = serve_config(
            "skew-lucy",
            &lucy_config,
            Fakes::stopping(lucy_stopper.clone()),
        )
        .await;
        let (megan, megan_dir) = serve_config(
            "skew-megan",
            &forward_config(socks_port, lucy.port(), ""),
            Fakes::default(),
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
            Fakes::stopping(stopper.clone()),
        )
        .await;
        let (status, json) = stop(megan, JDS7, &[]).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "WhatsUp is not running on lucy");
        assert!(stopper.calls().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// megan forwarding to something on lucy's WhatsUp port that answers
    /// each request to `path` with `reply` (or, with `None`, never).
    async fn forward_through(
        name: &str,
        path: &'static str,
        reply: Option<&'static str>,
    ) -> (u16, Value) {
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
            Fakes::default(),
        )
        .await;
        let reply = send(megan, path, JDS7, &[]).await;
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
            let (status, json) = forward_through(name, "/api/stop", Some(reply)).await;
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
        let (status, json) = forward_through("fwd-slow", "/api/stop", None).await;
        assert_eq!(status, 502);
        assert_eq!(
            json["error"],
            "lucy did not answer in time; it may still be stopping"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// A stand-in for lucy's WhatsUp on every connection: `reply` to each
    /// request, or (`None`) accept and never answer.
    fn fake_remote(reply: Option<&'static str>) -> u16 {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                std::thread::spawn(move || {
                    let mut seen = [0u8; 1024];
                    let _ = stream.read(&mut seen);
                    match reply {
                        Some(reply) => {
                            let _ = stream.write_all(reply.as_bytes());
                        }
                        None => std::thread::sleep(Duration::from_secs(5)),
                    }
                });
            }
        });
        port
    }

    /// megan's dashboard with lucy's WhatsUp at `whatsup_port`.
    async fn megan_dashboard(name: &str, whatsup_port: u16) -> Value {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Relay);
        let fakes = Fakes {
            load: Some(Arc::new(load_fixture)),
            ..Default::default()
        };
        let (megan, dir) =
            serve_config(name, &forward_config(socks_port, whatsup_port, ""), fakes).await;
        let json = dashboard_json(megan).await;
        let _ = std::fs::remove_dir_all(dir);
        json
    }

    fn lucy_of(json: &Value) -> &Value {
        json["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|machine| machine["name"] == "lucy")
            .expect("lucy")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remote_load_is_shown() {
        let lucy_load = || crate::load::Load {
            memory: Some(crate::load::Memory {
                percent: 58.3,
                total: 32 << 30,
                swap_used: 11 << 30,
            }),
            ..Default::default()
        };
        let fakes = Fakes {
            load: Some(Arc::new(lucy_load)),
            ..Default::default()
        };
        let (lucy, lucy_dir) = serve_config(
            "load-lucy",
            &forward_config(1, 1, r#""machine_names": { "testhost": "lucy" },"#),
            fakes,
        )
        .await;
        let json = megan_dashboard("load-megan", lucy.port()).await;
        let lucy_view = lucy_of(&json);
        assert_eq!(lucy_view["load"]["memory"]["percent"], 58.3);
        assert_eq!(lucy_view["load"]["memory"]["level"], "ok");
        assert_eq!(lucy_view["load"]["cpu"], Value::Null);
        assert_eq!(lucy_view["load_note"], Value::Null);
        assert_eq!(json["machines"][0]["load"]["disk"]["level"], "warn");
        let _ = std::fs::remove_dir_all(lucy_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn older_remote_gets_an_update_note() {
        let old = fake_remote(Some("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n"));
        let json = megan_dashboard("load-old", old).await;
        let lucy_view = lucy_of(&json);
        assert_eq!(lucy_view["load"], Value::Null);
        assert_eq!(
            lucy_view["load_note"],
            "update WhatsUp on lucy to see its load"
        );
        // A note, never an error: the dashboard shows no banner for it.
        assert_eq!(json["config_error"], Value::Null);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dead_remote_whatsup_adds_nothing() {
        let closed = StdListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let json = megan_dashboard("load-dead", port).await;
        let lucy_view = lucy_of(&json);
        assert_eq!(lucy_view["load"], Value::Null);
        assert_eq!(lucy_view["load_note"], Value::Null);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn silent_remote_does_not_hold_up_the_dashboard() {
        let silent = fake_remote(None);
        let started = std::time::Instant::now();
        let json = megan_dashboard("load-silent", silent).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(lucy_of(&json)["load"], Value::Null);
        assert_eq!(json["machines"][0]["load"]["cpu"]["percent"], 23.0);
    }

    #[test]
    fn remote_load_reads_only_a_good_reply() {
        assert_eq!(remote_load("lucy", Ok((200, "hello".into()))), (None, None));
        assert_eq!(remote_load("lucy", Ok((500, "{}".into()))), (None, None));
        assert_eq!(remote_load("lucy", Err(PostError::Timeout)), (None, None));
        assert_eq!(
            remote_load(
                "lucy",
                Ok((200, r#"{"cpu":null,"memory":null,"disk":null}"#.into()))
            ),
            (Some(crate::load::Load::default()), None)
        );
    }

    fn herdr(prefix: &str, label: &str, dir: &str, run: &str) -> StartKind {
        StartKind::Herdr {
            prefix: prefix.into(),
            label: label.into(),
            dir: PathBuf::from(dir),
            run: run.into(),
        }
    }

    fn entry(port: u16, kind: StartKind) -> PortStart {
        PortStart { port, kind }
    }

    /// jds2's plan when nothing is up: herdr for the links with a start
    /// command (4002's is not its first link), docker for the three on 8002.
    fn jds2_plan() -> Vec<PortStart> {
        vec![
            entry(
                3002,
                herdr(
                    "h2",
                    "Portal",
                    "~/workspace/huvr2",
                    "tt2a && cd frontend && npm start",
                ),
            ),
            entry(
                4002,
                herdr("h2", "Calc", "~/workspace/tt-calc-app2", "ttc2a"),
            ),
            entry(8002, StartKind::Docker),
        ]
    }

    fn stop_rows() -> DashboardConfig {
        crate::config::parse_config(STOP_CONFIG).unwrap()
    }

    #[test]
    fn plan_start_uses_herdr_for_start_commands_and_docker_for_the_rest() {
        let config = stop_rows();
        let (plan, decided) = plan_start(&config.groups[1].rows[0], |_, _| false);
        assert_eq!(plan, jds2_plan());
        assert_eq!(decided, []);
    }

    #[test]
    fn plan_start_skips_ports_that_are_up() {
        let config = stop_rows();
        let (plan, decided) = plan_start(&config.groups[1].rows[0], |host, port| {
            host == "localhost" && port == 4002
        });
        let mut expected = jds2_plan();
        expected.remove(1);
        assert_eq!(plan, expected);
        assert_eq!(
            decided,
            [PortResult {
                port: 4002,
                action: Action::None,
                detail: "already up".into(),
                error: None,
            }]
        );
    }

    #[test]
    fn plan_start_refuses_foreign_hosts_without_probing_them() {
        let config = stop_rows();
        let probed = std::cell::RefCell::new(Vec::new());
        let (plan, decided) = plan_start(&config.groups[0].rows[3], |host, port| {
            probed.borrow_mut().push((host.to_string(), port));
            false
        });
        assert_eq!(plan, [entry(3004, StartKind::Docker)]);
        assert_eq!(
            decided,
            [PortResult {
                port: 8004,
                action: Action::None,
                detail: String::new(),
                error: Some("192.0.2.1 is not this machine".into()),
            }]
        );
        assert_eq!(probed.into_inner(), [("127.0.0.1".to_string(), 3004)]);
    }

    #[test]
    fn link_is_up_probes_the_link_host() {
        // A server on 127.0.0.1 behind a name that may resolve to ::1 first.
        let v4 = StdListener::bind("127.0.0.1:0").unwrap();
        let v4_port = v4.local_addr().unwrap().port();
        assert!(link_is_up("localhost", v4_port));
        assert!(is_up("localhost", v4_port));
        // A vite on ::1 only, behind 127.0.0.1: its link host misses it, and
        // the dashboard's own probe (both loopbacks) still sees it.
        // 127.0.0.1 on that port is held bound and not listening, so a
        // parallel test cannot be handed it and start answering there.
        let (_v6, v6_port, _v4_closed) = loop {
            let v6 = StdListener::bind("[::1]:0").unwrap();
            let port = v6.local_addr().unwrap().port();
            if let Some(closed) = bind_without_listening(port) {
                break (v6, port, closed);
            }
        };
        assert!(!link_is_up("127.0.0.1", v6_port));
        assert!(is_up("127.0.0.1", v6_port));
        assert!(!is_up("localhost", 1));
    }

    /// Binds 127.0.0.1:`port` without listening: connects there are
    /// refused, and nothing else can bind it while the socket lives. None
    /// when the port is taken.
    fn bind_without_listening(port: u16) -> Option<std::os::fd::OwnedFd> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let size = std::mem::size_of::<libc::sockaddr_in>();
        // SAFETY: a fresh descriptor, owned at once, and a zeroed
        // sockaddr_in filled in field by field and passed with its size.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return None;
            }
            let socket = OwnedFd::from_raw_fd(fd);
            let mut address: libc::sockaddr_in = std::mem::zeroed();
            #[cfg(target_os = "macos")]
            {
                address.sin_len = size as u8;
            }
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            address.sin_port = port.to_be();
            address.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
            let bound = libc::bind(
                socket.as_raw_fd(),
                (&address as *const libc::sockaddr_in).cast::<libc::sockaddr>(),
                size as libc::socklen_t,
            );
            (bound == 0).then_some(socket)
        }
    }

    const JDS2: &str = r#"{"group":"pc","row":0,"ports":[3002,4002,8002]}"#;

    async fn serve_start(name: &str, starter: Arc<RecordingStarter>) -> (SocketAddr, PathBuf) {
        serve_config(
            &format!("start-{name}"),
            STOP_CONFIG,
            Fakes::starting(starter),
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_runs_the_rows_plan() {
        let starter = Arc::new(RecordingStarter::default());
        let (addr, dir) = serve_start("runs", starter.clone()).await;
        let (status, json) = start(addr, JDS2, &[]).await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(starter.plans(), [jds2_plan()]);
        assert_eq!(
            json,
            json!({ "ok": true, "results": [
                { "port": 3002, "action": "herdr", "detail": "started", "error": null },
                { "port": 4002, "action": "herdr", "detail": "started", "error": null },
                { "port": 8002, "action": "compose", "detail": "started", "error": null },
            ] })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_skips_ports_that_are_up() {
        let live = StdListener::bind("127.0.0.1:0").unwrap();
        let live_port = live.local_addr().unwrap().port();
        // Port 1 is never listened on (see the dashboard status test).
        let config = format!(
            r#"{{ "groups": [{{ "id": "g", "label": "G", "columns": ["Down", "Up"],
                "rows": [{{ "cells": [
                    {{ "label": "down", "url": "http://localhost:1/" }},
                    {{ "label": "up", "url": "http://127.0.0.1:{live_port}/" }}
                ] }}] }}] }}"#
        );
        let starter = Arc::new(RecordingStarter::default());
        let fakes = Fakes {
            starter: starter.clone(),
            real_probe: true,
            ..Default::default()
        };
        let (addr, dir) = serve_config("start-up", &config, fakes).await;
        let body = format!(r#"{{"group":"g","row":0,"ports":[1,{live_port}]}}"#);
        let (status, json) = start(addr, &body, &[]).await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(starter.plans(), [vec![entry(1, StartKind::Docker)]]);
        assert_eq!(
            json["results"],
            json!([
                { "port": 1, "action": "compose", "detail": "started", "error": null },
                { "port": live_port, "action": "none", "detail": "already up", "error": null },
            ])
        );
        let _keep = live;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_reports_joined_errors() {
        let starter = Arc::new(RecordingStarter {
            fail: Some(4002),
            ..Default::default()
        });
        let (addr, dir) = serve_start("fails", starter).await;
        let (status, json) = start(addr, JDS2, &[]).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "port 4002: boom");
        assert_eq!(json["results"][0]["error"], Value::Null);
        assert_eq!(json["results"][1]["error"], "boom");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_refuses_other_sites() {
        let starter = Arc::new(RecordingStarter::default());
        let (addr, dir) = serve_start("guard", starter.clone()).await;
        let forbidden: [&[(&str, &str)]; 2] =
            [&[("Origin", "http://evil.test")], &[("Host", "evil.test")]];
        for headers in forbidden {
            let (status, json) = start(addr, JDS2, headers).await;
            assert_eq!(status, 403, "{headers:?}");
            assert_eq!(json["error"], "starts must come from this dashboard");
        }
        assert!(starter.plans().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_row_checks() {
        let starter = Arc::new(RecordingStarter::default());
        let (addr, dir) = serve_start("checks", starter.clone()).await;
        let (status, json) = start(addr, "not json", &[]).await;
        assert_eq!(status, 400);
        let error = json["error"].as_str().unwrap();
        assert!(error.starts_with("not a start request: "), "{error}");
        for (body, error) in [
            (
                r#"{"machine":"nope","group":"pc","row":0,"ports":[3002,4002,8002]}"#,
                "unknown machine nope",
            ),
            (
                r#"{"group":"g","row":0,"ports":[]}"#,
                "that row has nothing to start",
            ),
        ] {
            assert_eq!(
                start(addr, body, &[]).await,
                (400, json!({ "error": error }))
            );
        }
        for body in [
            r#"{"group":"pc","row":0,"ports":[3002,4002]}"#,
            r#"{"group":"pc","row":1,"ports":[3002,4002,8002]}"#,
        ] {
            let (status, json) = start(addr, body, &[]).await;
            assert_eq!(status, 409, "{body}");
            assert_eq!(json["error"], CONFIG_CHANGED);
        }
        assert!(starter.plans().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_and_stop_share_the_busy_mark() {
        let (stopper, release_stop) = gated();
        let (release_start, gate) = std::sync::mpsc::channel();
        let starter = Arc::new(RecordingStarter {
            gate: Mutex::new(Some(gate)),
            ..Default::default()
        });
        let fakes = Fakes {
            stopper: stopper.clone(),
            starter: starter.clone(),
            real_probe: false,
            load: None,
        };
        let (addr, dir) = serve_config("busy", STOP_CONFIG, fakes).await;

        let stopping = tokio::spawn(async move { stop(addr, JDS4, &[]).await });
        until("the stop started", || stopper.calls().len() == 1).await;
        let (status, json) = start(addr, JDS4, &[]).await;
        assert_eq!((status, &json["error"]), (409, &json!("already stopping")));
        release_stop.send(()).unwrap();
        assert_eq!(stopping.await.unwrap().0, 200);

        let starting = tokio::spawn(async move { start(addr, JDS4, &[]).await });
        until("the start started", || starter.plans().len() == 1).await;
        let (status, json) = stop(addr, JDS4, &[]).await;
        assert_eq!((status, &json["error"]), (409, &json!("already starting")));
        release_start.send(()).unwrap();
        assert_eq!(starting.await.unwrap().0, 200);

        assert_eq!(stopper.calls().len(), 1);
        assert_eq!(starter.plans().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_refuses_foreign_hosts() {
        let starter = Arc::new(RecordingStarter::default());
        let (addr, dir) = serve_start("foreign", starter.clone()).await;
        let body = r#"{"group":"g","row":3,"ports":[3004,8004]}"#;
        let (status, json) = start(addr, body, &[]).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "port 8004: 192.0.2.1 is not this machine");
        assert_eq!(starter.plans(), [vec![entry(3004, StartKind::Docker)]]);
        assert_eq!(json["results"][0]["action"], "compose");
        assert_eq!(json["results"][1]["action"], "none");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn start_forwards_to_the_remote() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Relay);
        let lucy_starter = Arc::new(RecordingStarter::default());
        let (lucy, lucy_dir) = serve_config(
            "start-fwd-lucy",
            &forward_config(1, 1, r#""machine_names": { "testhost": "lucy" },"#),
            Fakes::starting(lucy_starter.clone()),
        )
        .await;
        let megan_starter = Arc::new(RecordingStarter::default());
        let (megan, megan_dir) = serve_config(
            "start-fwd-megan",
            &forward_config(socks_port, lucy.port(), ""),
            Fakes::starting(megan_starter.clone()),
        )
        .await;
        let (status, json) = start(megan, JDS7, &[]).await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(
            json["results"],
            json!([{ "port": 3007, "action": "herdr", "detail": "started", "error": null }])
        );
        assert_eq!(
            lucy_starter.plans(),
            [vec![entry(
                3007,
                herdr("h7", "Portal", "~/workspace/huvr7", "tt7a")
            )]]
        );
        assert!(megan_starter.plans().is_empty());
        let _ = std::fs::remove_dir_all(lucy_dir);
        let _ = std::fs::remove_dir_all(megan_dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_forward_to_old_whatsup() {
        let old = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n";
        let (status, json) = forward_through("start-fwd-old", "/api/start", Some(old)).await;
        assert_eq!(status, 502);
        assert_eq!(
            json["error"],
            "WhatsUp on lucy cannot start stacks; update it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_forward_errors() {
        let (socks_port, _requests) =
            crate::socks::fake::start(|_, _| crate::socks::fake::Act::Close);
        let starter = Arc::new(RecordingStarter::default());
        let (megan, dir) = serve_config(
            "start-fwd-down",
            &forward_config(socks_port, 47654, ""),
            Fakes::starting(starter.clone()),
        )
        .await;
        let (status, json) = start(megan, JDS7, &[]).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "WhatsUp is not running on lucy");
        assert!(starter.plans().is_empty());
        let _ = std::fs::remove_dir_all(dir);

        let started = std::time::Instant::now();
        let (status, json) = forward_through("start-fwd-slow", "/api/start", None).await;
        assert_eq!(status, 502);
        assert_eq!(
            json["error"],
            "lucy did not answer in time; it may still be starting"
        );
        assert!(started.elapsed() < Duration::from_secs(3));

        // Connected and closed without a reply, or answered with something
        // that is not HTTP: not a timeout, so say what happened.
        for (name, reply, reason) in [
            (
                "start-fwd-closed",
                "",
                "the connection closed without a reply",
            ),
            ("start-fwd-garbage", "hello\r\n", "the reply is not http"),
        ] {
            let (status, json) = forward_through(name, "/api/start", Some(reply)).await;
            assert_eq!(status, 502, "{name}");
            assert_eq!(
                json["error"],
                format!("lucy did not reply to the start: {reason}"),
                "{name}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dashboard_has_row_ids_and_can_act() {
        let (addr, dir) = serve_start("dashboard", Arc::default()).await;
        let json = dashboard_json(addr).await;
        let machine = &json["machines"][0];
        assert_eq!(machine["can_act"], true);
        assert!(machine.get("can_stop").is_none(), "{machine}");
        let rows = &json["groups"][0]["rows"];
        assert_eq!(rows[0].get("id"), Some(&Value::Null));
        assert_eq!(rows[1]["id"], "jds4");
        assert_eq!(json["groups"][1]["rows"][0]["id"], "jds2");
        let _ = std::fs::remove_dir_all(dir);
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
                starter: Arc::new(RecordingStarter::default()),
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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::browser::{open_in_browser, open_in_profile, OpenMode};
use crate::config::{config_mtime, load_config, Cell, DashboardConfig, Group, Link, Tone};
use crate::probe::probe_ports;
use crate::socks::{probe_remote, Reach};
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
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

impl AppState {
    pub fn new(
        config_path: std::path::PathBuf,
        config: DashboardConfig,
        opener: Arc<dyn BrowserOpener>,
        shutdown: watch::Sender<bool>,
        hostname: String,
        tunnels: Arc<Tunnels>,
    ) -> Self {
        let mtime = config_mtime(&config_path);
        tunnels.reconcile(&config.active_remotes(&config.machine_label(&hostname)));
        Self {
            inner: Arc::new(Inner {
                config_path,
                config: Mutex::new(config),
                mtime: Mutex::new(mtime),
                config_error: Mutex::new(None),
                opener,
                shutdown,
                hostname,
                tunnels,
            }),
        }
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
    let ports: Vec<u16> = config.ports().into_iter().collect();
    let local_probe = tokio::task::spawn_blocking(move || probe_ports(ports));
    let targets: Vec<(String, u16)> = config
        .links()
        .map(|link| (link.host.clone(), link.port))
        .collect();
    let remote_probes: Vec<_> = remotes
        .iter()
        .map(|remote| {
            let socks_port = remote.socks_port;
            let targets = targets.clone();
            tokio::task::spawn_blocking(move || probe_remote(socks_port, targets))
        })
        .collect();
    let local_status = local_probe.await.expect("port probe");
    let mut machines = vec![MachineStatus {
        name: local,
        local: true,
        offline: false,
        note: None,
        up: Box::new(move |link: &Link| local_status.get(&link.port).copied().unwrap_or(false)),
        counts: (0, 0),
    }];
    machines[0].counts = count_local(&config, &machines[0]);
    for (remote, probe) in remotes.iter().zip(remote_probes) {
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
        let counts = (
            if offline {
                0
            } else {
                reach.values().filter(|reach| **reach == Reach::Up).count()
            },
            reach.len(),
        );
        machines.push(MachineStatus {
            name: remote.name.clone(),
            local: false,
            offline,
            note,
            up: Box::new(move |link: &Link| {
                !offline && reach.get(&(link.host.clone(), link.port)) == Some(&Reach::Up)
            }),
            counts,
        });
    }
    Json(view(&config, &machines, &path, error))
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
}

/// Local status is per port, as before: distinct ports, and how many are up.
fn count_local(config: &DashboardConfig, machine: &MachineStatus) -> (usize, usize) {
    let mut by_port: BTreeMap<u16, bool> = BTreeMap::new();
    for link in config.links() {
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
    if !config.allows_url(&request.url) {
        return bad("that url is not in the dashboard config".into());
    }
    let local = state.local_label(&config);
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
        "machines": machines.iter().map(|machine| json!({
            "name": machine.name,
            "local": machine.local,
            "offline": machine.offline,
            "note": machine.note,
            "ports_up": machine.counts.0,
            "ports_total": machine.counts.1,
        })).collect::<Vec<_>>(),
        "groups": machines
            .iter()
            .flat_map(|machine| config.groups.iter().map(move |group| group_view(group, machine)))
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
        "rows": group.rows.iter().map(|row| {
            json!({
                "tone": match row.tone {
                    Tone::Gray => "gray",
                    Tone::Black => "black",
                },
                "cells": row.cells.iter().map(|cell| match cell {
                    Cell::Empty => json!({ "type": "empty" }),
                    Cell::Text(text) => json!({ "type": "text", "text": text }),
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
        let dir = std::env::temp_dir().join(format!("whatsup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        std::fs::write(&config_path, config_json).unwrap();
        let config = load_config(&config_path).unwrap();
        let (shutdown, _rx) = watch::channel(false);
        let opener = Arc::new(Recording(Mutex::new(Vec::new())));
        let tunnels = Arc::new(Tunnels::new(program));
        let state = AppState::new(
            config_path,
            config,
            opener.clone(),
            shutdown,
            HOSTNAME.to_string(),
            tunnels,
        );
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
        assert_eq!(
            json["machines"],
            serde_json::json!([{
                "name": HOSTNAME,
                "local": true,
                "offline": false,
                "note": null,
                "ports_up": json["ports_up"],
                "ports_total": json["ports_total"],
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
            opener.clone(),
            shutdown,
            HOSTNAME.to_string(),
            Arc::new(Tunnels::new(PathBuf::from("true"))),
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

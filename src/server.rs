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

use crate::browser::{open_in_browser, OpenMode};
use crate::config::{config_mtime, load_config, Cell, DashboardConfig, Tone};
use crate::probe::probe_ports;
use crate::DASHBOARD_HTML;

pub trait BrowserOpener: Send + Sync {
    fn open(&self, app: &str, url: &str, mode: OpenMode) -> Result<(), String>;
}

pub struct SystemOpener;

impl BrowserOpener for SystemOpener {
    fn open(&self, app: &str, url: &str, mode: OpenMode) -> Result<(), String> {
        open_in_browser(app, url, mode)
    }
}

struct Inner {
    config_path: std::path::PathBuf,
    config: Mutex<DashboardConfig>,
    mtime: Mutex<Option<SystemTime>>,
    config_error: Mutex<Option<String>>,
    opener: Arc<dyn BrowserOpener>,
    shutdown: watch::Sender<bool>,
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
    ) -> Self {
        let mtime = config_mtime(&config_path);
        Self {
            inner: Arc::new(Inner {
                config_path,
                config: Mutex::new(config),
                mtime: Mutex::new(mtime),
                config_error: Mutex::new(None),
                opener,
                shutdown,
            }),
        }
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
    let ports: Vec<u16> = config.ports().into_iter().collect();
    let status = tokio::task::spawn_blocking(move || probe_ports(ports))
        .await
        .expect("port probe");
    Json(view(&config, &status, &path, error))
}

#[derive(Deserialize)]
struct OpenRequest {
    url: String,
}

async fn open_link(State(state): State<AppState>, Json(request): Json<OpenRequest>) -> Response {
    let (config, _) = state.reload();
    if !config.allows_url(&request.url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "that url is not in the dashboard config" })),
        )
            .into_response();
    }
    match state
        .inner
        .opener
        .open(&config.browser_app, &request.url, config.open)
    {
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
    status: &std::collections::BTreeMap<u16, bool>,
    config_path: &str,
    config_error: Option<String>,
) -> Value {
    let mut up = 0;
    let mut total = 0;
    for port_up in status.values() {
        total += 1;
        if *port_up {
            up += 1;
        }
    }
    json!({
        "title": config.title,
        "refresh_ms": config.refresh_ms,
        "config_path": config_path,
        "config_error": config_error,
        "checked_at_ms": now_ms(),
        "ports_up": up,
        "ports_total": total,
        "groups": config.groups.iter().map(|group| {
            json!({
                "id": group.id,
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
                                "up": status.get(&link.port).copied().unwrap_or(false),
                            }),
                        }).collect::<Vec<_>>(),
                    })
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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dashboard_status_follows_the_listener_and_open_stays_on_the_config() {
        let live = StdListener::bind(("127.0.0.1", 0)).unwrap();
        let live_port = live.local_addr().unwrap().port();
        let closed = StdListener::bind(("127.0.0.1", 0)).unwrap();
        let closed_port = closed.local_addr().unwrap().port();

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
        let state = AppState::new(config_path.clone(), config, opener.clone(), shutdown);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        drop(closed);

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

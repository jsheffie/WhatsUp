use std::env;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;

use whatsup::chrome::open_in_browser;
use whatsup::config::{load_config, resolve_config_path};
use whatsup::instance;
use whatsup::server::{router, AppState, SystemOpener};
use whatsup::PREFERRED_PORT;

struct Args {
    config: Option<PathBuf>,
    no_open: bool,
    port: Option<u16>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("whatsup: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args(env::args().skip(1))?;
    let config_path = resolve_config_path(args.config)?;
    let config = load_config(&config_path)?;
    let instance_path = instance::instance_path()?;

    if let Some(running) = instance::read_running(&instance_path)? {
        if running.pid != std::process::id() && process_is_running(running.pid) {
            if !args.no_open {
                let url = format!("http://127.0.0.1:{}/", running.port);
                open_in_browser(&config.browser_app, &url, config.open)?;
            }
            println!(
                "WhatsUp is already running at http://127.0.0.1:{}/",
                running.port
            );
            return Ok(());
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("could not start the runtime: {err}"))?;

    runtime.block_on(async move {
        let preferred = args.port.unwrap_or(PREFERRED_PORT);
        let listener = bind_listener(preferred).await?;
        let port = listener
            .local_addr()
            .map_err(|err| format!("could not read the dashboard port: {err}"))?
            .port();
        let lock = match instance::claim(&instance_path, port)? {
            Ok(lock) => lock,
            Err(running) => {
                drop(listener);
                if !args.no_open {
                    let url = format!("http://127.0.0.1:{}/", running.port);
                    open_in_browser(&config.browser_app, &url, config.open)?;
                }
                println!(
                    "WhatsUp is already running at http://127.0.0.1:{}/",
                    running.port
                );
                return Ok(());
            }
        };

        let url = format!("http://127.0.0.1:{port}/");
        println!("WhatsUp listening on {url}");
        println!("Config: {}", config_path.display());

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let state = AppState::new(
            config_path,
            config.clone(),
            Arc::new(SystemOpener),
            shutdown_tx,
        );
        let app = router(state);
        let server = axum::serve(listener, app).with_graceful_shutdown(async move {
            let _ = shutdown_rx.wait_for(|stop| *stop).await;
        });

        if !args.no_open {
            let browser = config.browser_app.clone();
            let mode = config.open;
            let page = url.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if let Err(err) = open_in_browser(&browser, &page, mode) {
                    eprintln!("whatsup: started, but could not open {page} in Chrome: {err}");
                }
            });
        }

        let mut terminate = signal(SignalKind::terminate())
            .map_err(|err| format!("could not listen for terminate: {err}"))?;
        tokio::select! {
            result = server => {
                result.map_err(|err| format!("server stopped: {err}"))?;
            }
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        drop(lock);
        Ok(())
    })
}

async fn bind_listener(preferred: u16) -> Result<TcpListener, String> {
    match TcpListener::bind((Ipv4Addr::LOCALHOST, preferred)).await {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|err| format!("could not bind the dashboard: {err}")),
    }
}

fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        config: None,
        no_open: false,
        port: None,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-open" => parsed.no_open = true,
            "--config" => {
                let path = args.next().ok_or("--config needs a path")?;
                parsed.config = Some(PathBuf::from(path));
            }
            "--port" => {
                let text = args.next().ok_or("--port needs a port")?;
                let port: u16 = text.parse().map_err(|_| format!("invalid port {text}"))?;
                if port == 0 {
                    return Err("port must be greater than 0".into());
                }
                parsed.port = Some(port);
            }
            "--help" | "-h" => {
                println!(
                    "WhatsUp\n\n  whatsup [--config PATH] [--port PORT] [--no-open]\n\nThe dashboard config is JSON. cargo run reads config/whatsup.json.\nThe Mac app reads ~/Library/Application Support/WhatsUp/config.json."
                );
                std::process::exit(0);
            }
            other if other.starts_with("--config=") => {
                parsed.config = Some(PathBuf::from(other.trim_start_matches("--config=")));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(parsed)
}

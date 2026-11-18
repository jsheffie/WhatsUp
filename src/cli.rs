//! `whatsup list`, `whatsup start` and `whatsup stop`: a command line client
//! of the running WhatsUp's HTTP API, for people and agents.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::http::{exchange, PostError};
use crate::instance;

/// The command line forms, as `whatsup --help` and usage errors show them.
pub const USAGE: &str = concat!(
    "  whatsup list  [--json] [--port <port>]\n",
    "  whatsup start <row> [--machine <name>] [--wait] [--timeout <secs>] [--json] [--port <port>]\n",
    "  whatsup stop  <row> [--machine <name>] [--json] [--port <port>]\n",
    "\n",
    "<row> is a row id or <group>/<index>. After list, start or stop, --port names\n",
    "the port of the running WhatsUp; it is read from the instance file otherwise."
);

const NOT_RUNNING: &str = "WhatsUp is not running; start it first";
/// How long `start --wait` waits unless `--timeout` says otherwise.
const DEFAULT_WAIT: Duration = Duration::from_secs(180);
/// The longest `--timeout` taken: a day.
const MAX_WAIT_SECS: u64 = 86_400;
/// Loopback connects at once; this only bounds a wedged listener.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// `list` wraps a row's ports to stay within this many columns.
const LIST_WIDTH: usize = 100;
/// `list` aligns the ports after titles up to this long.
const TITLE_WIDTH: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    List {
        json: bool,
    },
    Start {
        row: String,
        machine: Option<String>,
        wait: bool,
        timeout: Duration,
        json: bool,
    },
    Stop {
        row: String,
        machine: Option<String>,
        json: bool,
    },
}

/// A row as a start or stop request names it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    machine: String,
    group: String,
    index: u64,
    ports: Vec<u16>,
    /// How messages name the row: its id, else its first text, else
    /// `group/index` (as the dashboard does).
    name: String,
}

/// How long to wait for what. Tests shorten them.
struct Settings {
    dashboard_read: Duration,
    action_read: Duration,
    poll: Duration,
}

const SETTINGS: Settings = Settings {
    dashboard_read: Duration::from_secs(10),
    action_read: Duration::from_secs(210),
    poll: Duration::from_secs(2),
};

/// Why a command ends early: the exit code and what to say on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Failure {
        Failure {
            code,
            message: message.into(),
        }
    }
}

pub fn is_command(arg: &str) -> bool {
    matches!(arg, "list" | "start" | "stop")
}

/// Runs a command line (`args` starts with the command) and returns its exit
/// code: 0 success, 1 the action failed or timed out, 2 usage, unknown row
/// or WhatsUp not running.
pub fn run(args: Vec<String>) -> u8 {
    run_with(
        args,
        &SETTINGS,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

fn run_with(
    args: Vec<String>,
    settings: &Settings,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let outcome = parse(args)
        .map_err(|error| Failure::new(2, format!("{error}\n\nUsage:\n{USAGE}")))
        .and_then(|(command, port)| execute(&command, port, settings, out, err));
    match outcome {
        Ok(()) => 0,
        Err(failure) => {
            let _ = writeln!(err, "whatsup: {}", failure.message);
            failure.code
        }
    }
}

fn parse(args: Vec<String>) -> Result<(Command, Option<u16>), String> {
    let mut args = args.into_iter();
    let name = args.next().unwrap_or_default();
    if !is_command(&name) {
        return Err(format!("unknown command {name}"));
    }
    let (mut json, mut wait, mut port, mut machine, mut timeout, mut row) =
        (false, false, None, None, None, None);
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{arg} needs {what}"));
        match arg.as_str() {
            "--help" | "-h" => return Ok((Command::Help, None)),
            "--json" => json = true,
            "--port" => {
                let text = value("a port")?;
                let number = text.parse::<u16>().ok().filter(|port| *port > 0);
                port = Some(number.ok_or_else(|| format!("invalid port {text}"))?);
            }
            "--machine" if name != "list" => machine = Some(value("a machine name")?),
            "--wait" if name == "start" => wait = true,
            "--timeout" if name == "start" => {
                let text = value("a number of seconds")?;
                let secs = text
                    .parse::<u64>()
                    .ok()
                    .filter(|secs| *secs <= MAX_WAIT_SECS);
                let secs = secs.ok_or_else(|| {
                    format!("invalid timeout {text}; give at most {MAX_WAIT_SECS} seconds")
                })?;
                timeout = Some(Duration::from_secs(secs));
            }
            flag if flag.starts_with('-') => return Err(format!("{name} has no option {flag}")),
            _ if name != "list" && row.is_none() => row = Some(arg.clone()),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    let command = match name.as_str() {
        "list" => Command::List { json },
        "start" => Command::Start {
            row: row.ok_or("start needs a row")?,
            machine,
            wait,
            timeout: timeout.unwrap_or(DEFAULT_WAIT),
            json,
        },
        _ => Command::Stop {
            row: row.ok_or("stop needs a row")?,
            machine,
            json,
        },
    };
    Ok((command, port))
}

fn execute(
    command: &Command,
    port: Option<u16>,
    settings: &Settings,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), Failure> {
    let connect = || {
        let port = whatsup_port(port, instance::instance_path, instance::process_alive)?;
        Ok::<_, Failure>(Client { port, settings })
    };
    match command {
        Command::Help => {
            let _ = writeln!(out, "Usage:\n{USAGE}");
            Ok(())
        }
        Command::List { json } => {
            let board = connect()?.dashboard()?;
            let _ = if *json {
                writeln!(out, "{:#}", list_json(&board))
            } else {
                write!(out, "{}", format_list(&board))
            };
            Ok(())
        }
        Command::Start {
            row,
            machine,
            wait,
            timeout,
            json,
        } => {
            let client = connect()?;
            let (board, target) = client.target(row, machine.as_deref())?;
            let reply = client.act(Act::Start, &target)?;
            report(&reply, *json, out)?;
            let started = started_ports(&reply.value);
            if !*wait || started.is_empty() {
                return Ok(());
            }
            client.wait_until_up(board, &target, &started, *timeout)?;
            // With --json, stdout holds only the reply.
            let up = format!("{} is up", target.name);
            let _ = if *json {
                writeln!(err, "{up}")
            } else {
                writeln!(out, "{up}")
            };
            Ok(())
        }
        Command::Stop { row, machine, json } => {
            let client = connect()?;
            let (_, target) = client.target(row, machine.as_deref())?;
            let reply = client.act(Act::Stop, &target)?;
            report(&reply, *json, out)
        }
    }
}

/// The port of the running WhatsUp: `--port`, else the instance file's when
/// its pid is alive.
fn whatsup_port(
    flag: Option<u16>,
    instance_path: impl FnOnce() -> Result<PathBuf, String>,
    alive: impl Fn(u32) -> bool,
) -> Result<u16, Failure> {
    if let Some(port) = flag {
        return Ok(port);
    }
    // No instance file, or one that cannot be read, means no WhatsUp either.
    let running = instance_path()
        .and_then(|path| instance::read_running(&path))
        .ok()
        .flatten();
    match running {
        Some(running) if alive(running.pid) => Ok(running.port),
        _ => Err(Failure::new(2, NOT_RUNNING)),
    }
}

#[derive(Debug, Clone, Copy)]
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

    fn doing(self) -> &'static str {
        match self {
            Act::Start => "starting",
            Act::Stop => "stopping",
        }
    }
}

/// A start or stop reply from WhatsUp: its body as sent, and parsed.
struct Reply {
    ok: bool,
    text: String,
    value: Value,
}

struct Client<'a> {
    port: u16,
    settings: &'a Settings,
}

impl Client<'_> {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        read_timeout: Duration,
    ) -> Result<(u16, String), PostError> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(|_| PostError::Connect)?;
        exchange(stream, method, self.port, path, body, read_timeout)
    }

    fn dashboard(&self) -> Result<Value, Failure> {
        match self.request("GET", "/api/dashboard", None, self.settings.dashboard_read) {
            Ok((200, body)) => serde_json::from_str::<Value>(&body)
                .ok()
                .filter(Value::is_object)
                .ok_or_else(|| Failure::new(1, "the dashboard reply is not JSON")),
            Ok((status, _)) => Err(Failure::new(
                1,
                format!("the dashboard answered with status {status}"),
            )),
            Err(PostError::Connect) => Err(Failure::new(2, NOT_RUNNING)),
            Err(PostError::Timeout) => Err(Failure::new(1, "the dashboard did not answer in time")),
            Err(PostError::Reply(error)) => Err(Failure::new(
                1,
                format!("could not read the dashboard: {error}"),
            )),
        }
    }

    /// Reads the dashboard and finds the row there.
    fn target(&self, row: &str, machine: Option<&str>) -> Result<(Value, Target), Failure> {
        let board = self.dashboard()?;
        let target = resolve(&board, row, machine).map_err(|error| Failure::new(2, error))?;
        Ok((board, target))
    }

    fn act(&self, act: Act, target: &Target) -> Result<Reply, Failure> {
        let body = json!({
            "machine": target.machine,
            "group": target.group,
            "row": target.index,
            "ports": target.ports,
        })
        .to_string();
        let path = format!("/api/{}", act.verb());
        match self.request("POST", &path, Some(&body), self.settings.action_read) {
            Ok((status, text)) => match serde_json::from_str::<Value>(&text) {
                Ok(value) if status != 404 && value.is_object() => Ok(Reply {
                    ok: (200..300).contains(&status),
                    text,
                    value,
                }),
                // A WhatsUp from before this command line.
                _ => Err(Failure::new(
                    1,
                    format!(
                        "the running WhatsUp cannot {} stacks; restart it",
                        act.verb()
                    ),
                )),
            },
            Err(PostError::Connect) => Err(Failure::new(2, NOT_RUNNING)),
            Err(PostError::Timeout) => Err(Failure::new(
                1,
                format!(
                    "WhatsUp did not answer in time; it may still be {}",
                    act.doing()
                ),
            )),
            Err(PostError::Reply(error)) => Err(Failure::new(
                1,
                format!("WhatsUp did not reply to the {}: {error}", act.verb()),
            )),
        }
    }

    /// Polls the dashboard until every one of `ports` is up on the target
    /// row, or `timeout` passes. `board` is the dashboard read before the
    /// start; a later read that fails keeps the last one that worked.
    fn wait_until_up(
        &self,
        mut board: Value,
        target: &Target,
        ports: &[u16],
        timeout: Duration,
    ) -> Result<(), Failure> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.dashboard() {
                Ok(fresh) => board = fresh,
                // WhatsUp went away; nothing will show the ports coming up.
                Err(failure) if failure.code == 2 => return Err(failure),
                Err(_) => {}
            }
            let row = find_row(&board, target);
            let down: Vec<u16> = ports
                .iter()
                .copied()
                .filter(|port| !row.is_some_and(|row| port_up(row, *port)))
                .collect();
            if down.is_empty() {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                let names: Vec<String> = down
                    .iter()
                    .map(|port| {
                        let label = row.and_then(|row| labels(row, *port).first().copied());
                        format!("{} ({port})", label.unwrap_or("port"))
                    })
                    .collect();
                return Err(Failure::new(
                    1,
                    format!(
                        "still down after {} s: {}",
                        timeout.as_secs(),
                        names.join(", ")
                    ),
                ));
            }
            thread::sleep(self.settings.poll.min(deadline - now));
        }
    }
}

/// Prints a start or stop reply, and fails when it is an error.
fn report(reply: &Reply, json: bool, out: &mut dyn Write) -> Result<(), Failure> {
    let _ = if json {
        writeln!(out, "{}", reply.text.trim_end())
    } else {
        write!(out, "{}", format_results(&reply.value))
    };
    if reply.ok {
        Ok(())
    } else {
        let error = reply.value["error"].as_str();
        Err(Failure::new(1, error.unwrap_or("the request failed")))
    }
}

/// One line per port: the port, what was done (or `error`), and how.
fn format_results(reply: &Value) -> String {
    let lines: Vec<(String, &str, &str)> = items(reply, "results")
        .map(|result| {
            let port = result["port"].to_string();
            match result["error"].as_str() {
                Some(error) => (port, "error", error),
                None => (port, text(result, "action"), text(result, "detail")),
            }
        })
        .collect();
    let port_width = lines.iter().map(|line| line.0.len()).max().unwrap_or(0);
    let action_width = lines.iter().map(|line| line.1.len()).max().unwrap_or(0);
    lines
        .iter()
        .map(|(port, action, how)| {
            let line = format!("{port:<port_width$}  {action:<action_width$} {how}");
            format!("{}\n", line.trim_end())
        })
        .collect()
}

/// The ports a start reply says it started: an action other than `none`,
/// and no error.
fn started_ports(reply: &Value) -> Vec<u16> {
    items(reply, "results")
        .filter(|result| result["error"].is_null() && text(result, "action") != "none")
        .filter_map(|result| port_number(&result["port"]))
        .collect()
}

fn resolve(dashboard: &Value, row: &str, machine: Option<&str>) -> Result<Target, String> {
    let machines: Vec<&str> = items(dashboard, "machines")
        .map(|machine| text(machine, "name"))
        .collect();
    let machine = match machine {
        None => text(dashboard, "machine"),
        Some(wanted) => machines
            .iter()
            .copied()
            .find(|name| name.eq_ignore_ascii_case(wanted))
            .ok_or_else(|| {
                format!(
                    "unknown machine {wanted}; machines: {}",
                    machines.join(", ")
                )
            })?,
    };
    let named = |group: &Value, candidate: &Value| {
        let id = candidate["id"].as_str();
        id.is_some_and(|id| id.eq_ignore_ascii_case(row))
            || row_key(group, candidate).eq_ignore_ascii_case(row)
    };
    let mut elsewhere = false;
    for group in items(dashboard, "groups") {
        for candidate in items(group, "rows").filter(|candidate| named(group, candidate)) {
            let Some(index) = candidate["index"].as_u64() else {
                continue;
            };
            if text(group, "machine") == machine {
                return Ok(Target {
                    machine: machine.to_string(),
                    group: text(group, "id").to_string(),
                    index,
                    ports: row_ports(candidate),
                    name: row_name(group, candidate),
                });
            }
            elsewhere = true;
        }
    }
    if elsewhere {
        return Err(format!("{row} is not on {machine}"));
    }
    let known: Vec<String> = rows_on(dashboard, machine)
        .map(|(group, row)| row_id(group, row))
        .collect();
    if known.is_empty() {
        Err(format!("unknown row {row}; {machine} shows no rows"))
    } else {
        Err(format!(
            "unknown row {row}; rows on {machine}: {}",
            known.join(", ")
        ))
    }
}

/// The rows `list --json` prints, machine by machine.
fn list_rows(dashboard: &Value) -> Vec<Value> {
    items(dashboard, "machines")
        .flat_map(|machine| {
            let name = text(machine, "name");
            let can_act = machine["can_act"].as_bool().unwrap_or(false);
            rows_on(dashboard, name).map(move |(group, row)| {
                let ports: Vec<Value> = row_ports(row)
                    .into_iter()
                    .map(|port| json!({ "port": port, "up": port_up(row, port), "labels": labels(row, port) }))
                    .collect();
                json!({
                    "machine": name,
                    "id": row["id"],
                    "group": group["id"],
                    "index": row["index"],
                    "title": title(row),
                    "can_act": can_act,
                    "ports": ports,
                })
            })
        })
        .collect()
}

fn list_json(dashboard: &Value) -> Value {
    json!({ "machines": dashboard["machines"], "rows": list_rows(dashboard) })
}

/// Per machine, a summary line and then one line per row: its id (or
/// `group/index`), its first text, and each port up or down with its labels.
fn format_list(dashboard: &Value) -> String {
    let rows = list_rows(dashboard);
    let mut blocks = Vec::new();
    if let Some(error) = dashboard["config_error"].as_str() {
        blocks.push(format!("config error: {error}\n"));
    }
    for machine in items(dashboard, "machines") {
        let name = text(machine, "name");
        let mut block = format!(
            "{name} {}/{} up",
            machine["ports_up"].as_u64().unwrap_or(0),
            machine["ports_total"].as_u64().unwrap_or(0)
        );
        if machine["offline"].as_bool() == Some(true) {
            block.push_str(" (offline)");
        } else if machine["local"].as_bool() != Some(true)
            && machine["can_act"].as_bool() != Some(true)
        {
            block.push_str(" (WhatsUp not running there)");
        }
        block.push('\n');
        let mine: Vec<(String, &str, &Value)> = rows
            .iter()
            .filter(|row| row["machine"] == name)
            .map(|row| {
                let key = match row["id"].as_str() {
                    Some(id) => id.to_string(),
                    None => format!("{}/{}", text(row, "group"), row["index"]),
                };
                (key, row["title"].as_str().unwrap_or(""), row)
            })
            .collect();
        let width = |text: &str| text.chars().count();
        let key_width = mine.iter().map(|row| width(&row.0)).max().unwrap_or(0);
        let title_width = mine
            .iter()
            .map(|row| width(row.1))
            .max()
            .unwrap_or(0)
            .min(TITLE_WIDTH);
        let ports_column = 2 + key_width + 2 + title_width + 2;
        for (key, title, row) in &mine {
            let mut line = format!("  {key:<key_width$}  {title:<title_width$}");
            for (n, port) in items(row, "ports").enumerate() {
                let labels: Vec<&str> = items(port, "labels").filter_map(Value::as_str).collect();
                let state = if port["up"].as_bool() == Some(true) {
                    "up"
                } else {
                    "down"
                };
                let item = match labels.join("/") {
                    labels if labels.is_empty() => format!("{} {state}", port["port"]),
                    labels => format!("{labels}:{} {state}", port["port"]),
                };
                if n > 0 && width(&line) + 2 + width(&item) > LIST_WIDTH {
                    block.push_str(line.trim_end());
                    block.push('\n');
                    line = " ".repeat(ports_column);
                } else {
                    line.push_str("  ");
                }
                line.push_str(&item);
            }
            block.push_str(line.trim_end());
            block.push('\n');
        }
        blocks.push(block);
    }
    blocks.join("\n")
}

// ---- reading the dashboard JSON ----

fn items<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value[key].as_array().into_iter().flatten()
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

fn port_number(value: &Value) -> Option<u16> {
    value.as_u64().and_then(|port| u16::try_from(port).ok())
}

/// The rows shown on `machine`, each with its group.
fn rows_on<'a>(
    dashboard: &'a Value,
    machine: &'a str,
) -> impl Iterator<Item = (&'a Value, &'a Value)> {
    items(dashboard, "groups")
        .filter(move |group| text(group, "machine") == machine)
        .flat_map(|group| items(group, "rows").map(move |row| (group, row)))
}

fn find_row<'a>(dashboard: &'a Value, target: &Target) -> Option<&'a Value> {
    items(dashboard, "groups")
        .filter(|group| {
            text(group, "machine") == target.machine && text(group, "id") == target.group
        })
        .flat_map(|group| items(group, "rows"))
        .find(|row| row["index"].as_u64() == Some(target.index))
}

fn row_ports(row: &Value) -> Vec<u16> {
    items(row, "ports").filter_map(port_number).collect()
}

fn links_on(row: &Value, port: u16) -> impl Iterator<Item = &Value> {
    items(row, "cells").filter(move |cell| {
        text(cell, "type") == "link" && port_number(&cell["port"]) == Some(port)
    })
}

/// Up when any link of the row on that port is up, as the dashboard decides.
fn port_up(row: &Value, port: u16) -> bool {
    links_on(row, port).any(|cell| cell["up"].as_bool() == Some(true))
}

fn labels(row: &Value, port: u16) -> Vec<&str> {
    links_on(row, port)
        .map(|cell| text(cell, "label"))
        .collect()
}

/// The row's first text cell.
fn title(row: &Value) -> Option<&str> {
    items(row, "cells")
        .find(|cell| text(cell, "type") == "text")
        .and_then(|cell| cell["text"].as_str())
}

/// `group/index`, which names any row.
fn row_key(group: &Value, row: &Value) -> String {
    format!("{}/{}", text(group, "id"), row["index"])
}

/// The row's id, else `group/index`.
fn row_id(group: &Value, row: &Value) -> String {
    row["id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| row_key(group, row))
}

fn row_name(group: &Value, row: &Value) -> String {
    row["id"]
        .as_str()
        .or_else(|| title(row).filter(|title| !title.is_empty()))
        .map(str::to_string)
        .unwrap_or_else(|| row_key(group, row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::io::Read;
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    fn link(label: &str, port: u16, up: bool) -> Value {
        json!({ "type": "link", "label": label, "port": port, "up": up, "url": "http://x" })
    }

    fn text(text: &str) -> Value {
        json!({ "type": "text", "text": text })
    }

    /// megan (local) shows two pipelinecore rows and an id-less personal
    /// row; lucy is reachable but its WhatsUp is not; jeeves is offline.
    /// `up` says whether jds2's ports are up.
    fn dashboard(up: bool) -> Value {
        json!({
            "machine": "megan",
            "config_error": null,
            "machines": [
                { "name": "megan", "local": true, "offline": false, "note": null,
                  "ports_up": 1, "ports_total": 6, "can_act": true },
                { "name": "lucy", "local": false, "offline": false, "note": null,
                  "ports_up": 0, "ports_total": 2, "can_act": false },
                { "name": "jeeves", "local": false, "offline": true, "note": "jeeves offline",
                  "ports_up": 0, "ports_total": 0, "can_act": false }
            ],
            "groups": [
                { "id": "pipelinecore", "key": "megan/pipelinecore", "machine": "megan",
                  "label": "PipelineCORE", "columns": ["Directories", "Workspace", "A", "B", "C", "D", "E"],
                  "rows": [
                    { "index": 0, "id": "jds", "ports": [3000, 4000, 8000], "tone": "gray", "cells": [
                        text("huvr, tt-calc-app"),
                        { "type": "text", "text": "jds", "herdr": ["h1"] },
                        link("jds portal", 3000, true),
                        link("tt-calc-app", 4000, false),
                        link("jds admin", 8000, false),
                        link("jds schema", 8000, false),
                        link("jds swagger", 8000, false)
                    ] },
                    { "index": 2, "id": "jds2", "ports": [3002, 4002], "tone": "black", "cells": [
                        text("huvr2, tt-calc-app2"),
                        text("jds2"),
                        link("jds2 portal", 3002, up),
                        link("tt-calc-app2", 4002, up)
                    ] }
                  ] },
                { "id": "personal", "key": "megan/personal", "machine": "megan",
                  "label": "Personal", "columns": ["Project", "Directory", "Portal"],
                  "rows": [
                    { "index": 0, "id": null, "ports": [5173], "tone": "gray", "cells": [
                        { "type": "empty" },
                        text("blog"),
                        link("blog", 5173, false)
                    ] }
                  ] },
                { "id": "pipelinecore", "key": "lucy/pipelinecore", "machine": "lucy",
                  "label": "PipelineCORE", "columns": ["Directories", "Workspace", "A"],
                  "rows": [
                    { "index": 0, "id": "jds", "ports": [3000], "tone": "gray", "cells": [
                        text("huvr"), text("jds"), link("jds portal", 3000, false)
                    ] },
                    { "index": 7, "id": "jds7", "ports": [3007], "tone": "black", "cells": [
                        text("huvr7"), text("jds7"), link("jds7 portal", 3007, false)
                    ] }
                  ] }
            ]
        })
    }

    // ---- parsing ----

    #[test]
    fn parse_commands() {
        assert_eq!(
            parse(strings(&["list"])),
            Ok((Command::List { json: false }, None))
        );
        assert_eq!(
            parse(strings(&["list", "--json", "--port", "47000"])),
            Ok((Command::List { json: true }, Some(47000)))
        );
        assert_eq!(
            parse(strings(&[
                "start",
                "--wait",
                "jds2",
                "--machine",
                "lucy",
                "--timeout",
                "30",
                "--json",
                "--port",
                "5000"
            ])),
            Ok((
                Command::Start {
                    row: "jds2".into(),
                    machine: Some("lucy".into()),
                    wait: true,
                    timeout: Duration::from_secs(30),
                    json: true,
                },
                Some(5000)
            ))
        );
        assert_eq!(
            parse(strings(&["start", "jds2"])),
            Ok((
                Command::Start {
                    row: "jds2".into(),
                    machine: None,
                    wait: false,
                    timeout: Duration::from_secs(180),
                    json: false,
                },
                None
            ))
        );
        assert_eq!(
            parse(strings(&["stop", "pipelinecore/3"])),
            Ok((
                Command::Stop {
                    row: "pipelinecore/3".into(),
                    machine: None,
                    json: false,
                },
                None
            ))
        );
        assert_eq!(
            parse(strings(&["stop", "--help"])),
            Ok((Command::Help, None))
        );
        for bad in [
            &["start"][..],
            &["stop", "--json"],
            &["stop", "jds", "--wait"],
            &["list", "--bogus"],
            &["list", "jds"],
            &["start", "jds", "jds2"],
            &["start", "jds", "--port"],
            &["start", "jds", "--port", "0"],
            &["start", "jds", "--port", "http"],
            &["start", "jds", "--timeout", "soon"],
            &["start", "jds", "--timeout", "86401"],
            &["start", "jds", "--timeout", "18446744073709551615"],
            &["stop", "jds", "--machine"],
        ] {
            assert!(parse(strings(bad)).is_err(), "{bad:?}");
        }
        assert!(parse(strings(&["start"])).unwrap_err().contains("row"));
        assert!(matches!(
            parse(strings(&["start", "jds", "--timeout", "86400"])),
            Ok((Command::Start { timeout, .. }, None)) if timeout == Duration::from_secs(86400)
        ));
        assert!(parse(strings(&["list", "--bogus"]))
            .unwrap_err()
            .contains("--bogus"));
        assert!(is_command("list") && is_command("start") && is_command("stop"));
        assert!(!is_command("--port") && !is_command("quit"));
    }

    #[test]
    fn usage_errors_exit_2_with_the_usage() {
        let fake = Fake::serve(vec![]);
        for args in [&["stop"][..], &["list", "--verbose"]] {
            let (code, out, err) = fake.run(args);
            assert_eq!(code, 2, "{args:?}");
            assert_eq!(out, "");
            assert!(err.starts_with("whatsup: "), "{err}");
            assert!(err.contains(USAGE), "{err}");
        }
        let (code, out, err) = fake.run(&["start", "--help"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.contains(USAGE), "{out}");
        assert!(fake.requests().is_empty());
    }

    // ---- rows ----

    fn target(machine: &str, group: &str, index: u64, ports: &[u16], name: &str) -> Target {
        Target {
            machine: machine.into(),
            group: group.into(),
            index,
            ports: ports.to_vec(),
            name: name.into(),
        }
    }

    #[test]
    fn resolve_rows() {
        let board = dashboard(false);
        let jds2 = target("megan", "pipelinecore", 2, &[3002, 4002], "jds2");
        assert_eq!(resolve(&board, "JDS2", None), Ok(jds2.clone()));
        assert_eq!(resolve(&board, "pipelinecore/2", None), Ok(jds2.clone()));
        assert_eq!(resolve(&board, "jds2", Some("megan")), Ok(jds2));
        assert_eq!(
            resolve(&board, "personal/0", None),
            Ok(target("megan", "personal", 0, &[5173], "blog"))
        );
        assert_eq!(
            resolve(&board, "jds", Some("LUCY")),
            Ok(target("lucy", "pipelinecore", 0, &[3000], "jds"))
        );
        // The config index, not the place among the rows lucy shows.
        assert_eq!(
            resolve(&board, "pipelinecore/7", Some("lucy")),
            Ok(target("lucy", "pipelinecore", 7, &[3007], "jds7"))
        );
        assert_eq!(
            resolve(&board, "jds2", Some("lucy")),
            Err("jds2 is not on lucy".to_string())
        );
        // As typed, not by the row's title.
        assert_eq!(
            resolve(&board, "personal/0", Some("lucy")),
            Err("personal/0 is not on lucy".to_string())
        );
        assert_eq!(
            resolve(&board, "jds9", None),
            Err("unknown row jds9; rows on megan: jds, jds2, personal/0".to_string())
        );
        assert_eq!(
            resolve(&board, "pipelinecore/1", None),
            Err("unknown row pipelinecore/1; rows on megan: jds, jds2, personal/0".to_string())
        );
        assert_eq!(
            resolve(&board, "jds", Some("mars")),
            Err("unknown machine mars; machines: megan, lucy, jeeves".to_string())
        );

        // The default machine is the dashboard's own.
        let mut on_lucy = dashboard(false);
        on_lucy["machine"] = json!("lucy");
        assert_eq!(
            resolve(&on_lucy, "jds7", None),
            Ok(target("lucy", "pipelinecore", 7, &[3007], "jds7"))
        );
    }

    // ---- list ----

    #[test]
    fn list_formats_rows_and_summary() {
        let expected = "\
megan 1/6 up
  jds         huvr, tt-calc-app    jds portal:3000 up  tt-calc-app:4000 down
                                   jds admin/jds schema/jds swagger:8000 down
  jds2        huvr2, tt-calc-app2  jds2 portal:3002 down  tt-calc-app2:4002 down
  personal/0  blog                 blog:5173 down

lucy 0/2 up (WhatsUp not running there)
  jds   huvr   jds portal:3000 down
  jds7  huvr7  jds7 portal:3007 down

jeeves 0/0 up (offline)
";
        let board = dashboard(false);
        assert_eq!(format_list(&board), expected);
        assert!(format_list(&board).lines().all(|line| line.len() <= 100));

        let mut broken = dashboard(false);
        broken["config_error"] = json!("groups is empty");
        assert!(format_list(&broken).starts_with("config error: groups is empty\n\nmegan 1/6 up\n"));
    }

    #[test]
    fn list_json_has_the_spec_shape() {
        let board = dashboard(true);
        let listed = list_json(&board);
        assert_eq!(listed["machines"], board["machines"]);
        let rows = listed["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0],
            json!({
                "machine": "megan", "id": "jds", "group": "pipelinecore", "index": 0,
                "title": "huvr, tt-calc-app", "can_act": true,
                "ports": [
                    { "port": 3000, "up": true, "labels": ["jds portal"] },
                    { "port": 4000, "up": false, "labels": ["tt-calc-app"] },
                    { "port": 8000, "up": false, "labels": ["jds admin", "jds schema", "jds swagger"] }
                ]
            })
        );
        assert_eq!(rows[2]["id"], Value::Null);
        assert_eq!(rows[2]["title"], "blog");
        assert_eq!(rows[4]["machine"], "lucy");
        assert_eq!(rows[4]["index"], 7);
        assert_eq!(rows[4]["can_act"], false);
    }

    #[test]
    fn list_runs_against_the_dashboard() {
        let fake = Fake::serve(vec![("/api/dashboard", vec![(200, dashboard(false))])]);
        let (code, out, err) = fake.run(&["list"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, format_list(&dashboard(false)));
        let (code, out, _) = fake.run(&["list", "--json"]);
        assert_eq!(code, 0);
        let printed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(printed, list_json(&dashboard(false)));
        assert!(fake
            .requests()
            .iter()
            .all(|request| request.starts_with("GET /api/dashboard HTTP/1.1")));
    }

    // ---- start and stop against a fake WhatsUp ----

    /// A WhatsUp stand-in on an ephemeral port that serves canned replies by
    /// path. Each path's replies are given in turn and the last one repeats;
    /// other paths get an empty 404, as axum gives. The statuses `CLOSE` and
    /// `SILENT` stand for no reply at all.
    struct Fake {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
    }

    /// Close the connection without a reply, as a handler that panicked.
    const CLOSE: u16 = 0;
    /// Say nothing for a second, then close.
    const SILENT: u16 = 1;

    const FAST: Settings = Settings {
        dashboard_read: Duration::from_secs(2),
        action_read: Duration::from_secs(2),
        poll: Duration::from_millis(20),
    };

    impl Fake {
        fn serve(routes: Vec<(&'static str, Vec<(u16, Value)>)>) -> Fake {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut routes: HashMap<&str, VecDeque<(u16, Value)>> = routes
                .into_iter()
                .map(|(path, replies)| (path, replies.into()))
                .collect();
            let log = seen.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let Some(request) = read_request(&mut stream) else {
                        continue;
                    };
                    let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                    log.lock().unwrap().push(request);
                    let (status, body) = match routes.get_mut(path.as_str()) {
                        Some(replies) if replies.len() > 1 => replies.pop_front().unwrap(),
                        Some(replies) => replies[0].clone(),
                        None => (404, Value::Null),
                    };
                    match status {
                        CLOSE => continue,
                        SILENT => {
                            thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                        _ => {}
                    }
                    let body = if body.is_null() {
                        String::new()
                    } else {
                        body.to_string()
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            Fake { port, seen }
        }

        fn run(&self, args: &[&str]) -> (u8, String, String) {
            self.run_with(&FAST, args)
        }

        fn run_with(&self, settings: &Settings, args: &[&str]) -> (u8, String, String) {
            // Always a --port: never the instance file, never the real WhatsUp.
            let mut args = strings(args);
            if !args.iter().any(|arg| arg == "--port") {
                args.extend(["--port".to_string(), self.port.to_string()]);
            }
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run_with(args, settings, &mut out, &mut err);
            (
                code,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        }

        /// Each request: its request line, then its body when it has one.
        fn requests(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }

        fn count(&self, path: &str) -> usize {
            let prefix = format!(" {path} ");
            self.requests()
                .iter()
                .filter(|request| request.contains(&prefix))
                .count()
        }
    }

    /// Reads a request head and its Content-Length body, and returns the
    /// request line followed by the body (on the next line, when present).
    fn read_request(stream: &mut TcpStream) -> Option<String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut seen = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(at) = seen.windows(4).position(|window| window == b"\r\n\r\n") {
                break at;
            }
            let read = stream.read(&mut chunk).ok().filter(|read| *read > 0)?;
            seen.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&seen[..head_end]).to_string();
        let length: usize = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);
        while seen.len() < head_end + 4 + length {
            let read = stream.read(&mut chunk).ok().filter(|read| *read > 0)?;
            seen.extend_from_slice(&chunk[..read]);
        }
        let body = String::from_utf8_lossy(&seen[head_end + 4..]).to_string();
        let line = head.lines().next().unwrap_or("").to_string();
        assert!(
            !head.to_ascii_lowercase().contains("\norigin:"),
            "the guard rejects an Origin: {head}"
        );
        Some(if body.is_empty() {
            line
        } else {
            format!("{line}\n{body}")
        })
    }

    fn result(port: u16, action: &str, detail: &str, error: Option<&str>) -> Value {
        json!({ "port": port, "action": action, "detail": detail, "error": error })
    }

    /// The JSON body of the one request sent to `path`.
    fn body_sent(fake: &Fake, path: &str) -> Value {
        let prefix = format!("POST {path} HTTP/1.1\n");
        let requests = fake.requests();
        let request = requests
            .iter()
            .find(|request| request.starts_with(&prefix))
            .unwrap_or_else(|| panic!("no POST {path} in {requests:?}"));
        serde_json::from_str(&request[prefix.len()..]).unwrap()
    }

    #[test]
    fn start_prints_results_and_exits_0() {
        let reply = json!({ "ok": true, "results": [
            result(3002, "herdr", "herdr tab jds2 portal in h2", None),
            result(4002, "compose", "docker compose -p huvr_jds2 start", None)
        ] });
        let fake = Fake::serve(vec![
            ("/api/dashboard", vec![(200, dashboard(false))]),
            ("/api/start", vec![(200, reply.clone())]),
        ]);
        let (code, out, err) = fake.run(&["start", "jds2"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(
            out,
            "3002  herdr   herdr tab jds2 portal in h2\n4002  compose docker compose -p huvr_jds2 start\n"
        );
        assert_eq!(
            body_sent(&fake, "/api/start"),
            json!({ "machine": "megan", "group": "pipelinecore", "row": 2, "ports": [3002, 4002] })
        );

        // --json prints the reply as is.
        let (code, out, err) = fake.run(&["start", "jds2", "--json"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), reply);
    }

    #[test]
    fn start_error_exits_1() {
        let failed = json!({
            "error": "port 4002: no start command for tt-calc-app2",
            "results": [
                result(3002, "herdr", "herdr tab jds2 portal in h2", None),
                result(4002, "none", "", Some("no start command for tt-calc-app2"))
            ]
        });
        let fake = Fake::serve(vec![
            ("/api/dashboard", vec![(200, dashboard(false))]),
            ("/api/start", vec![(502, failed.clone())]),
        ]);
        let (code, out, err) = fake.run(&["start", "jds2", "--wait"]);
        assert_eq!(code, 1);
        assert_eq!(
            out,
            "3002  herdr herdr tab jds2 portal in h2\n4002  error no start command for tt-calc-app2\n"
        );
        assert_eq!(
            err,
            "whatsup: port 4002: no start command for tt-calc-app2\n"
        );
        // An error reply does not wait.
        assert_eq!(fake.count("/api/dashboard"), 1);

        let (code, out, err) = fake.run(&["start", "jds2", "--json"]);
        assert_eq!(code, 1);
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), failed);
        assert_eq!(
            err,
            "whatsup: port 4002: no start command for tt-calc-app2\n"
        );

        let busy = Fake::serve(vec![
            ("/api/dashboard", vec![(200, dashboard(false))]),
            (
                "/api/start",
                vec![(409, json!({ "error": "already stopping" }))],
            ),
        ]);
        let (code, out, err) = busy.run(&["start", "jds2"]);
        assert_eq!((code, out.as_str()), (1, ""));
        assert_eq!(err, "whatsup: already stopping\n");
    }

    #[test]
    fn an_older_whatsup_cannot_start() {
        let fake = Fake::serve(vec![("/api/dashboard", vec![(200, dashboard(false))])]);
        let (code, out, err) = fake.run(&["start", "jds2"]);
        assert_eq!((code, out.as_str()), (1, ""));
        assert_eq!(
            err,
            "whatsup: the running WhatsUp cannot start stacks; restart it\n"
        );
    }

    #[test]
    fn unknown_row_exits_2() {
        let fake = Fake::serve(vec![("/api/dashboard", vec![(200, dashboard(false))])]);
        let (code, out, err) = fake.run(&["stop", "jds2", "--machine", "lucy"]);
        assert_eq!((code, out.as_str()), (2, ""));
        assert_eq!(err, "whatsup: jds2 is not on lucy\n");
        let (code, _, err) = fake.run(&["start", "nope"]);
        assert_eq!(code, 2);
        assert!(
            err.starts_with("whatsup: unknown row nope; rows on megan: "),
            "{err}"
        );
        assert_eq!(fake.count("/api/start") + fake.count("/api/stop"), 0);
    }

    #[test]
    fn a_start_without_a_reply_says_why() {
        let closed = Fake::serve(vec![
            ("/api/dashboard", vec![(200, dashboard(false))]),
            ("/api/start", vec![(CLOSE, Value::Null)]),
        ]);
        let (code, out, err) = closed.run(&["start", "jds2"]);
        assert_eq!((code, out.as_str()), (1, ""));
        assert_eq!(
            err,
            "whatsup: WhatsUp did not reply to the start: the connection closed without a reply\n"
        );

        let silent = Fake::serve(vec![
            ("/api/dashboard", vec![(200, dashboard(false))]),
            ("/api/stop", vec![(SILENT, Value::Null)]),
        ]);
        let quick = Settings {
            action_read: Duration::from_millis(200),
            ..FAST
        };
        let (code, out, err) = silent.run_with(&quick, &["stop", "jds2"]);
        assert_eq!((code, out.as_str()), (1, ""));
        assert_eq!(
            err,
            "whatsup: WhatsUp did not answer in time; it may still be stopping\n"
        );
    }

    fn started_jds2() -> Value {
        json!({ "ok": true, "results": [
            result(3002, "herdr", "herdr tab jds2 portal in h2", None),
            result(4002, "compose", "docker compose -p huvr_jds2 start", None)
        ] })
    }

    #[test]
    fn wait_succeeds_when_ports_come_up() {
        let fake = Fake::serve(vec![
            (
                "/api/dashboard",
                vec![
                    (200, dashboard(false)),
                    (200, dashboard(false)),
                    (200, dashboard(true)),
                ],
            ),
            ("/api/start", vec![(200, started_jds2())]),
        ]);
        let (code, out, err) = fake.run(&["start", "jds2", "--wait", "--timeout", "5"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert!(out.ends_with("\njds2 is up\n"), "{out}");
        assert_eq!(fake.count("/api/dashboard"), 3);
    }

    #[test]
    fn wait_watches_only_the_ports_it_started() {
        let reply = json!({ "ok": true, "results": [
            result(3002, "none", "already up", None),
            result(4002, "none", "already up", None)
        ] });
        let fake = Fake::serve(vec![
            ("/api/dashboard", vec![(200, dashboard(false))]),
            ("/api/start", vec![(200, reply)]),
        ]);
        let (code, out, err) = fake.run(&["start", "jds2", "--wait"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, "3002  none already up\n4002  none already up\n");
        assert_eq!(fake.count("/api/dashboard"), 1);
    }

    #[test]
    fn wait_times_out_exit_1() {
        let fake = Fake::serve(vec![
            ("/api/dashboard", vec![(200, dashboard(false))]),
            ("/api/start", vec![(200, started_jds2())]),
        ]);
        let started = std::time::Instant::now();
        let (code, out, err) = fake.run(&["start", "jds2", "--wait", "--timeout", "1"]);
        let elapsed = started.elapsed();
        assert_eq!(code, 1);
        assert!(!out.contains("is up"), "{out}");
        assert_eq!(
            err,
            "whatsup: still down after 1 s: jds2 portal (3002), tt-calc-app2 (4002)\n"
        );
        assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }

    #[test]
    fn stop_acts_immediately() {
        let reply = json!({ "ok": true, "results": [
            result(3000, "process", "stopped node (pid 42)", None)
        ] });
        let fake = Fake::serve(vec![
            ("/api/dashboard", vec![(200, dashboard(false))]),
            ("/api/stop", vec![(200, reply)]),
        ]);
        let (code, out, err) = fake.run(&["stop", "jds", "--machine", "Lucy"]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, "3000  process stopped node (pid 42)\n");
        assert_eq!(
            body_sent(&fake, "/api/stop"),
            json!({ "machine": "lucy", "group": "pipelinecore", "row": 0, "ports": [3000] })
        );
        assert_eq!(fake.count("/api/dashboard"), 1);
    }

    #[test]
    fn no_whatsup_exits_2() {
        let closed = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        let port = closed.to_string();
        for args in [
            &["list", "--port", &port][..],
            &["start", "jds2", "--port", &port],
            &["stop", "jds2", "--port", &port],
        ] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run_with(strings(args), &FAST, &mut out, &mut err);
            assert_eq!(code, 2, "{args:?}");
            assert!(out.is_empty());
            assert_eq!(
                String::from_utf8(err).unwrap(),
                "whatsup: WhatsUp is not running; start it first\n"
            );
        }
    }

    #[test]
    fn a_stale_instance_file_is_not_running() {
        let dir = std::env::temp_dir().join(format!("whatsup-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("instance.json");
        let not_running = Err(Failure {
            code: 2,
            message: NOT_RUNNING.into(),
        });

        // No instance file at all.
        assert_eq!(
            whatsup_port(None, || Ok(path.clone()), |_| true),
            not_running
        );

        // A pid that has exited.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(&path, format!("{{\"pid\":{dead},\"port\":47999}}\n")).unwrap();
        assert_eq!(
            whatsup_port(None, || Ok(path.clone()), instance::process_alive),
            not_running
        );

        // A live one, and --port over the instance file.
        assert_eq!(whatsup_port(None, || Ok(path.clone()), |_| true), Ok(47999));
        assert_eq!(
            whatsup_port(Some(5000), || Ok(path.clone()), |_| true),
            Ok(5000)
        );

        // An unreadable instance file is not a running WhatsUp either.
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(
            whatsup_port(None, || Ok(path.clone()), |_| true),
            not_running
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

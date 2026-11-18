//! Reads the workspace list from a herdr server over its Unix socket, and
//! matches workspace labels to dashboard cells by name prefix. A blocking
//! client also writes to this machine's herdr, for Start.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use serde::Deserialize;
use serde_json::{json, Value};

/// How long one workspace query may take in total, connect to reply.
pub const QUERY_BUDGET: Duration = Duration::from_secs(1);
/// A reply larger than this is not herdr's.
const MAX_REPLY: usize = 1024 * 1024;
const REQUEST: &[u8] = b"{\"id\":\"whatsup\",\"method\":\"workspace.list\",\"params\":{}}\n";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HerdrWorkspace {
    pub number: u64,
    pub label: String,
}

/// Whether `label` names a workspace for `prefix`: the label is the prefix,
/// or continues it after a separator (`h1`, `h1-herberth`, but not `h10`).
pub fn label_matches(label: &str, prefix: &str) -> bool {
    let (Some(head), Some(rest)) = (label.get(..prefix.len()), label.get(prefix.len()..)) else {
        return false;
    };
    head.eq_ignore_ascii_case(prefix)
        && rest
            .chars()
            .next()
            .is_none_or(|next| !next.is_ascii_alphanumeric())
}

/// Labels of the workspaces matching `prefix`, in herdr's sidebar order.
pub fn matching_labels(workspaces: &[HerdrWorkspace], prefix: &str) -> Vec<String> {
    let mut matched: Vec<&HerdrWorkspace> = workspaces
        .iter()
        .filter(|workspace| label_matches(&workspace.label, prefix))
        .collect();
    matched.sort_by_key(|workspace| workspace.number);
    matched
        .into_iter()
        .map(|workspace| workspace.label.clone())
        .collect()
}

#[derive(Deserialize)]
struct Reply {
    result: ReplyResult,
}

#[derive(Deserialize)]
struct ReplyResult {
    #[serde(rename = "type")]
    kind: String,
    workspaces: Vec<HerdrWorkspace>,
}

/// The workspaces in one `workspace.list` reply line; `None` for an error
/// reply or anything else.
pub fn parse_reply(line: &str) -> Option<Vec<HerdrWorkspace>> {
    let reply: Reply = serde_json::from_str(line).ok()?;
    (reply.result.kind == "workspace_list").then_some(reply.result.workspaces)
}

/// Asks the herdr server on `socket` for its workspaces. `None` when there
/// is no server, it fails, or the whole exchange (connect included: a herdr
/// that stops accepting blocks connects on Linux) takes longer than `budget`.
pub async fn list_workspaces(socket: &Path, budget: Duration) -> Option<Vec<HerdrWorkspace>> {
    tokio::time::timeout(budget, exchange(socket))
        .await
        .ok()
        .flatten()
}

async fn exchange(socket: &Path) -> Option<Vec<HerdrWorkspace>> {
    let mut stream = UnixStream::connect(socket).await.ok()?;
    stream.write_all(REQUEST).await.ok()?;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        reply.extend_from_slice(&chunk[..read]);
        if let Some(end) = reply.iter().position(|byte| *byte == b'\n') {
            return parse_reply(std::str::from_utf8(&reply[..end]).ok()?);
        }
        if reply.len() > MAX_REPLY {
            return None;
        }
    }
}

/// herdr's default-session socket on this machine.
pub fn local_socket(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let absolute = |key: &str| {
        env(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    absolute("XDG_CONFIG_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".config")))
        .map(|config| config.join("herdr").join("herdr.sock"))
}

/// A blocking client for a herdr socket: one connection per call.
#[derive(Clone, Debug)]
pub struct HerdrClient {
    pub socket: PathBuf,
    /// How long one call may take in total, connect to reply.
    pub timeout: Duration,
}

/// Why a herdr call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// No connection: no socket, refused, or the connect timed out. Nothing
    /// was sent.
    NoConnection,
    /// Connected, but no herdr answered: too slow, closed early, or a reply
    /// that is not herdr's. The request may have been carried out.
    NoAnswer,
    /// herdr answered with an error, or without what the call asks for.
    Failed(String),
}

impl HerdrClient {
    /// Sends one request and returns its `result`. The whole exchange,
    /// connect included, takes at most `timeout`.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let deadline = Instant::now() + self.timeout;
        let left = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
                .ok_or(CallError::NoAnswer)
        };
        let mut stream = connect_within(&self.socket, self.timeout)?;
        let request = json!({ "id": "whatsup", "method": method, "params": params });
        stream
            .set_write_timeout(Some(left()?))
            .map_err(|_| CallError::NoAnswer)?;
        // `write_all` retries a write a signal interrupted.
        stream
            .write_all(format!("{request}\n").as_bytes())
            .map_err(|_| CallError::NoAnswer)?;
        read_reply(&mut stream, |stream| {
            stream
                .set_read_timeout(Some(left()?))
                .map_err(|_| CallError::NoAnswer)
        })
    }

    /// Every workspace: id, sidebar number and label.
    pub fn workspaces(&self) -> Result<Vec<(String, u64, String)>, CallError> {
        let result = self.call("workspace.list", json!({}))?;
        let list = result["workspaces"]
            .as_array()
            .ok_or_else(|| unexpected("workspace.list"))?;
        Ok(list
            .iter()
            .filter_map(|workspace| {
                Some((
                    workspace["workspace_id"].as_str()?.to_string(),
                    workspace["number"].as_u64()?,
                    workspace["label"].as_str()?.to_string(),
                ))
            })
            .collect())
    }

    /// Creates an unfocused workspace: its id, its first tab and that tab's
    /// pane.
    pub fn create_workspace(
        &self,
        label: &str,
        cwd: &Path,
    ) -> Result<(String, String, String), CallError> {
        let method = "workspace.create";
        let result = self.call(
            method,
            json!({ "label": label, "cwd": cwd.to_string_lossy(), "focus": false }),
        )?;
        Ok((
            text_at(&result, "/workspace/workspace_id", method)?,
            text_at(&result, "/tab/tab_id", method)?,
            text_at(&result, "/root_pane/pane_id", method)?,
        ))
    }

    /// The tabs of workspace `ws`: id and label.
    pub fn tabs(&self, ws: &str) -> Result<Vec<(String, String)>, CallError> {
        let result = self.call("tab.list", json!({ "workspace_id": ws }))?;
        let list = result["tabs"]
            .as_array()
            .ok_or_else(|| unexpected("tab.list"))?;
        Ok(list
            .iter()
            .filter_map(|tab| {
                Some((
                    tab["tab_id"].as_str()?.to_string(),
                    tab["label"].as_str()?.to_string(),
                ))
            })
            .collect())
    }

    /// Creates an unfocused tab in `ws`: its id and its pane.
    pub fn create_tab(
        &self,
        ws: &str,
        cwd: &Path,
        label: &str,
    ) -> Result<(String, String), CallError> {
        let method = "tab.create";
        let result = self.call(
            method,
            json!({
                "workspace_id": ws,
                "cwd": cwd.to_string_lossy(),
                "label": label,
                "focus": false,
            }),
        )?;
        Ok((
            text_at(&result, "/tab/tab_id", method)?,
            text_at(&result, "/root_pane/pane_id", method)?,
        ))
    }

    pub fn rename_tab(&self, tab: &str, label: &str) -> Result<(), CallError> {
        self.call("tab.rename", json!({ "tab_id": tab, "label": label }))
            .map(drop)
    }

    /// The panes of workspace `ws`, in `pane.list` order: id and tab.
    pub fn panes(&self, ws: &str) -> Result<Vec<(String, String)>, CallError> {
        let result = self.call("pane.list", json!({ "workspace_id": ws }))?;
        let list = result["panes"]
            .as_array()
            .ok_or_else(|| unexpected("pane.list"))?;
        Ok(list
            .iter()
            .filter_map(|pane| {
                Some((
                    pane["pane_id"].as_str()?.to_string(),
                    pane["tab_id"].as_str()?.to_string(),
                ))
            })
            .collect())
    }

    /// Whether `pane`'s shell is at its prompt: its foreground process group
    /// is the shell itself. Unknown counts as not idle.
    pub fn idle(&self, pane: &str) -> Result<bool, CallError> {
        let result = self.call("pane.process_info", json!({ "pane_id": pane }))?;
        let info = &result["process_info"];
        let shell = info["shell_pid"].as_u64();
        let foreground = info["foreground_process_group_id"].as_u64();
        Ok(shell.is_some() && shell == foreground)
    }

    /// The last `lines` lines of `pane`, as printed: soft-wrapped rows are
    /// joined back, so a line longer than the pane is still one line.
    pub fn read(&self, pane: &str, lines: u32) -> Result<String, CallError> {
        let method = "pane.read";
        let result = self.call(
            method,
            json!({ "pane_id": pane, "source": "recent_unwrapped", "lines": lines }),
        )?;
        text_at(&result, "/read/text", method)
    }

    /// Types `text` into `pane` and presses Enter.
    pub fn type_line(&self, pane: &str, text: &str) -> Result<(), CallError> {
        self.call(
            "pane.send_input",
            json!({ "pane_id": pane, "text": text, "keys": ["Enter"] }),
        )
        .map(drop)
    }
}

/// Connects to `socket` within `timeout`. A std connect has no timeout of
/// its own and blocks forever on Linux against a herdr whose accept queue
/// is full, so it runs on a helper thread that is left behind on timeout.
fn connect_within(
    socket: &Path,
    timeout: Duration,
) -> Result<std::os::unix::net::UnixStream, CallError> {
    let (connected_tx, connected_rx) = std::sync::mpsc::channel();
    let socket = socket.to_path_buf();
    std::thread::spawn(move || {
        let _ = connected_tx.send(std::os::unix::net::UnixStream::connect(socket));
    });
    match connected_rx.recv_timeout(timeout) {
        Ok(Ok(stream)) => Ok(stream),
        _ => Err(CallError::NoConnection),
    }
}

/// Reads one reply line from `stream`. `before_read` runs before every read;
/// it bounds the read by what is left of the call's deadline, or fails when
/// nothing is left.
fn read_reply<S: Read>(
    stream: &mut S,
    mut before_read: impl FnMut(&mut S) -> Result<(), CallError>,
) -> Result<Value, CallError> {
    let mut reply = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        before_read(stream)?;
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            // A signal arrived (EINTR): read again, within the deadline.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(CallError::NoAnswer),
        };
        if read == 0 {
            return Err(CallError::NoAnswer);
        }
        reply.extend_from_slice(&chunk[..read]);
        if let Some(end) = reply.iter().position(|byte| *byte == b'\n') {
            return call_result(&reply[..end]);
        }
        if reply.len() > MAX_REPLY {
            return Err(CallError::NoAnswer);
        }
    }
}

/// The `result` of one reply line, or its `error.message`.
fn call_result(line: &[u8]) -> Result<Value, CallError> {
    let mut reply: Value = serde_json::from_slice(line).map_err(|_| CallError::NoAnswer)?;
    if let Some(result) = reply.get_mut("result") {
        return Ok(result.take());
    }
    match reply.get("error") {
        Some(error) => Err(CallError::Failed(
            error["message"]
                .as_str()
                .or(error["code"].as_str())
                .unwrap_or("unknown error")
                .to_string(),
        )),
        None => Err(CallError::NoAnswer),
    }
}

fn unexpected(method: &str) -> CallError {
    CallError::Failed(format!("unexpected reply to {method}"))
}

fn text_at(result: &Value, pointer: &str, method: &str) -> Result<String, CallError> {
    result
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| unexpected(method))
}

#[cfg(test)]
pub(crate) mod testing {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};

    /// A short, unique socket path (macOS limits these to ~104 bytes).
    pub fn socket_path() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "wu-herdr-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A listener on `path` that queues at most one connection and never
    /// accepts.
    pub fn tiny_backlog_listener(path: &Path) -> UnixListener {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::FromRawFd;
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0);
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (slot, byte) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
                *slot = *byte as libc::c_char;
            }
            let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            assert_eq!(
                libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len),
                0
            );
            assert_eq!(libc::listen(fd, 0), 0);
            UnixListener::from_raw_fd(fd)
        }
    }

    /// A herdr that stopped accepting, with its accept queue full: a
    /// blocking connect to it waits forever on Linux (macOS does not block).
    pub fn stuck_herdr(path: &Path) -> UnixListener {
        let listener = tiny_backlog_listener(path);
        for _ in 0..4 {
            let path = path.to_path_buf();
            std::thread::spawn(move || {
                let held = UnixStream::connect(path);
                std::thread::sleep(Duration::from_secs(30));
                drop(held);
            });
        }
        std::thread::sleep(Duration::from_millis(300));
        listener
    }

    /// A herdr server on `path` answering every `workspace.list` with
    /// `workspaces` (number, label). Runs for the rest of the test process.
    pub fn serve_labels(path: &Path, workspaces: &[(u64, &str)]) {
        let list: Vec<_> = workspaces
            .iter()
            .map(|(number, label)| {
                serde_json::json!({ "workspace_id": format!("w{number}"), "number": number, "label": label })
            })
            .collect();
        let reply = serde_json::json!({
            "id": "whatsup",
            "result": { "type": "workspace_list", "workspaces": list },
        })
        .to_string();
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_ok() {
                    let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
                }
            }
        });
    }

    /// A herdr server on `path` that accepts and never answers.
    pub fn serve_silence(path: &Path) {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
    }

    struct FakeWorkspace {
        id: String,
        number: u64,
        label: String,
    }

    struct FakeTab {
        id: String,
        workspace: String,
        label: String,
    }

    struct FakePane {
        id: String,
        tab: String,
        workspace: String,
        shell_pid: Option<u32>,
        foreground: Option<u32>,
        text: String,
        /// Columns `recent` reads wrap at; `None` never wraps.
        width: Option<usize>,
    }

    #[derive(Default)]
    struct FakeState {
        workspaces: Vec<FakeWorkspace>,
        tabs: Vec<FakeTab>,
        panes: Vec<FakePane>,
        next_id: u64,
        requests: Vec<(String, Value)>,
        on_type: Option<fn(&str) -> String>,
        /// Methods answered with an error: method and message.
        failures: Vec<(String, String)>,
        /// Methods whose next replies are withheld: method and how many.
        withheld: Vec<(String, usize)>,
        /// The method after whose first reply the socket file is removed,
        /// and that file.
        vanish: Option<(String, PathBuf)>,
    }

    const SHELL_PID: u32 = 100;
    /// What a new pane shows: its shell's prompt.
    const PROMPT: &str = "% ";

    impl FakeState {
        fn id(&mut self, kind: &str) -> String {
            self.next_id += 1;
            format!("{kind}{}", self.next_id)
        }

        fn add_tab(&mut self, workspace: &str, label: &str) -> (String, String) {
            let tab = self.id("t");
            self.tabs.push(FakeTab {
                id: tab.clone(),
                workspace: workspace.into(),
                label: label.into(),
            });
            let pane = self.add_pane(&tab);
            (tab, pane)
        }

        fn add_pane(&mut self, tab: &str) -> String {
            let workspace = self.tab(tab).workspace.clone();
            let pane = self.id("p");
            self.panes.push(FakePane {
                id: pane.clone(),
                tab: tab.into(),
                workspace,
                shell_pid: Some(SHELL_PID),
                foreground: Some(SHELL_PID),
                text: PROMPT.into(),
                width: None,
            });
            pane
        }

        fn tab(&self, id: &str) -> &FakeTab {
            self.tabs.iter().find(|tab| tab.id == id).expect("tab")
        }

        fn pane(&mut self, id: &str) -> Result<&mut FakePane, String> {
            self.panes
                .iter_mut()
                .find(|pane| pane.id == id)
                .ok_or_else(|| format!("pane {id} not found"))
        }

        fn has_workspace(&self, id: &str) -> Result<(), String> {
            match self.workspaces.iter().any(|workspace| workspace.id == id) {
                true => Ok(()),
                false => Err(format!("workspace {id} not found")),
            }
        }

        fn answer(&mut self, method: &str, params: &Value) -> Result<Value, String> {
            if let Some((_, message)) = self.failures.iter().find(|(seen, _)| seen == method) {
                return Err(message.clone());
            }
            let text = |key: &str| params[key].as_str().unwrap_or_default().to_string();
            match method {
                "workspace.list" => {
                    let list: Vec<Value> = self
                        .workspaces
                        .iter()
                        .map(|w| json!({ "workspace_id": w.id, "number": w.number, "label": w.label }))
                        .collect();
                    Ok(json!({ "type": "workspace_list", "workspaces": list }))
                }
                "workspace.create" => {
                    let workspace = self.id("w");
                    let number = self.workspaces.iter().map(|w| w.number).max().unwrap_or(0) + 1;
                    self.workspaces.push(FakeWorkspace {
                        id: workspace.clone(),
                        number,
                        label: text("label"),
                    });
                    let (tab, pane) = self.add_tab(&workspace, "1");
                    Ok(json!({
                        "type": "workspace_created",
                        "workspace": { "workspace_id": workspace, "number": number, "label": text("label") },
                        "tab": { "tab_id": tab, "workspace_id": workspace, "number": 1, "label": "1" },
                        "root_pane": { "pane_id": pane, "tab_id": tab, "workspace_id": workspace },
                    }))
                }
                "tab.list" => {
                    let workspace = text("workspace_id");
                    self.has_workspace(&workspace)?;
                    let list: Vec<Value> = self
                        .tabs
                        .iter()
                        .filter(|tab| tab.workspace == workspace)
                        .map(|tab| json!({ "tab_id": tab.id, "label": tab.label, "workspace_id": tab.workspace }))
                        .collect();
                    Ok(json!({ "type": "tab_list", "tabs": list }))
                }
                "tab.create" => {
                    let workspace = text("workspace_id");
                    self.has_workspace(&workspace)?;
                    let (tab, pane) = self.add_tab(&workspace, &text("label"));
                    Ok(json!({
                        "type": "tab_created",
                        "tab": { "tab_id": tab, "workspace_id": workspace, "label": text("label") },
                        "root_pane": { "pane_id": pane, "tab_id": tab, "workspace_id": workspace },
                    }))
                }
                "tab.rename" => {
                    let id = text("tab_id");
                    let tab = self
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.id == id)
                        .ok_or_else(|| format!("tab {id} not found"))?;
                    tab.label = text("label");
                    Ok(json!({ "type": "ok" }))
                }
                "pane.list" => {
                    let workspace = text("workspace_id");
                    self.has_workspace(&workspace)?;
                    let list: Vec<Value> = self
                        .panes
                        .iter()
                        .filter(|pane| pane.workspace == workspace)
                        .map(|pane| json!({ "pane_id": pane.id, "tab_id": pane.tab, "workspace_id": pane.workspace }))
                        .collect();
                    Ok(json!({ "type": "pane_list", "panes": list }))
                }
                "pane.process_info" => {
                    let pane = self.pane(&text("pane_id"))?;
                    Ok(json!({
                        "type": "pane_process_info",
                        "process_info": {
                            "pane_id": pane.id,
                            "shell_pid": pane.shell_pid,
                            "foreground_process_group_id": pane.foreground,
                            "foreground_processes": [],
                        },
                    }))
                }
                "pane.read" => {
                    let lines = params["lines"].as_u64().unwrap_or(u64::MAX) as usize;
                    let source = text("source");
                    let pane = self.pane(&text("pane_id"))?;
                    // `recent` gives screen rows, soft-wrapped at the pane's
                    // width; `recent_unwrapped` gives the logical lines.
                    let all: Vec<String> = match (source.as_str(), pane.width) {
                        ("recent", Some(width)) => pane
                            .text
                            .split('\n')
                            .flat_map(|line| {
                                let chars: Vec<char> = line.chars().collect();
                                match chars.is_empty() {
                                    true => vec![String::new()],
                                    false => chars
                                        .chunks(width)
                                        .map(|row| row.iter().collect())
                                        .collect(),
                                }
                            })
                            .collect(),
                        ("recent" | "recent_unwrapped", _) => {
                            pane.text.split('\n').map(String::from).collect()
                        }
                        (other, _) => return Err(format!("unknown source {other}")),
                    };
                    let recent = all[all.len().saturating_sub(lines)..].join("\n");
                    Ok(
                        json!({ "type": "pane_read", "read": { "pane_id": pane.id, "text": recent } }),
                    )
                }
                "pane.send_input" => {
                    let typed = text("text");
                    let output = self.on_type.map(|on_type| on_type(&typed));
                    let pane = self.pane(&text("pane_id"))?;
                    // The shell echoes the line after its prompt, then the
                    // command prints its output and the next prompt follows.
                    pane.text.push_str(&typed);
                    if params["keys"] == json!(["Enter"]) {
                        pane.text.push('\n');
                        if let Some(output) = output {
                            pane.text.push_str(&format!("{output}\n{PROMPT}"));
                        }
                    }
                    Ok(json!({ "type": "ok" }))
                }
                other => Err(format!("unknown method {other}")),
            }
        }
    }

    /// A scripted herdr server: workspaces, tabs and panes kept in memory,
    /// every request recorded. Runs until the test process ends.
    pub struct FakeHerdr {
        path: PathBuf,
        state: Arc<Mutex<FakeState>>,
    }

    impl FakeHerdr {
        pub fn start(path: &Path) -> FakeHerdr {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path).unwrap();
            let state = Arc::new(Mutex::new(FakeState::default()));
            let shared = state.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let state = shared.clone();
                    std::thread::spawn(move || serve(stream, &state));
                }
            });
            FakeHerdr {
                path: path.to_path_buf(),
                state,
            }
        }

        fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.state.lock().unwrap()
        }

        /// Adds a workspace with no tabs; returns its id.
        pub fn add_workspace(&self, number: u64, label: &str) -> String {
            let mut state = self.state();
            let id = state.id("w");
            state.workspaces.push(FakeWorkspace {
                id: id.clone(),
                number,
                label: label.into(),
            });
            id
        }

        /// Adds a tab with one idle pane to `workspace`: the tab and pane ids.
        pub fn add_tab(&self, workspace: &str, label: &str) -> (String, String) {
            self.state().add_tab(workspace, label)
        }

        /// Splits `tab`: adds another idle pane to it; returns its id.
        pub fn split(&self, tab: &str) -> String {
            self.state().add_pane(tab)
        }

        /// Marks `pane` as running a command in the foreground.
        pub fn set_busy(&self, pane: &str) {
            self.set_shell(pane, Some(SHELL_PID), Some(SHELL_PID + 1));
        }

        /// Sets what `pane.process_info` reports for `pane`.
        pub fn set_shell(&self, pane: &str, shell_pid: Option<u32>, foreground: Option<u32>) {
            let mut state = self.state();
            let pane = state.pane(pane).unwrap();
            pane.shell_pid = shell_pid;
            pane.foreground = foreground;
        }

        /// Sets what `pane` shows.
        pub fn set_text(&self, pane: &str, text: &str) {
            self.state().pane(pane).unwrap().text = text.into();
        }

        /// Makes `recent` reads of `pane` wrap at `columns`.
        pub fn set_width(&self, pane: &str, columns: usize) {
            self.state().pane(pane).unwrap().width = Some(columns);
        }

        /// Sets the output a typed line produces, from that line.
        pub fn on_type(&self, output: fn(&str) -> String) {
            self.state().on_type = Some(output);
        }

        /// Answers every later `method` request with an error `message`.
        pub fn fail(&self, method: &str, message: &str) {
            self.state().failures.push((method.into(), message.into()));
        }

        /// Carries out the next `times` `method` requests but never answers
        /// them, as if each reply were lost.
        pub fn withhold(&self, method: &str, times: usize) {
            self.state().withheld.push((method.into(), times));
        }

        /// Removes the socket file once the next `method` request is
        /// answered, so later connects fail as if herdr had quit.
        pub fn vanish_after(&self, method: &str) {
            self.state().vanish = Some((method.into(), self.path.clone()));
        }

        /// Every request so far: method and params.
        pub fn requests(&self) -> Vec<(String, Value)> {
            self.state().requests.clone()
        }

        /// The methods of every request so far.
        pub fn methods(&self) -> Vec<String> {
            self.requests()
                .into_iter()
                .map(|(method, _)| method)
                .collect()
        }
    }

    impl Drop for FakeHerdr {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn serve(stream: UnixStream, state: &Mutex<FakeState>) {
        let reader = BufReader::new(&stream);
        for line in reader.lines() {
            let Ok(line) = line else { return };
            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                return;
            };
            let method = request["method"].as_str().unwrap_or_default().to_string();
            let params = request["params"].clone();
            let reply = {
                let mut state = state.lock().unwrap();
                state.requests.push((method.clone(), params.clone()));
                let answer = state.answer(&method, &params);
                let withheld = state
                    .withheld
                    .iter_mut()
                    .find(|(seen, left)| *seen == method && *left > 0);
                if let Some((_, left)) = withheld {
                    // No reply: the client gives up and closes, ending this
                    // connection.
                    *left -= 1;
                    continue;
                }
                if let Some((_, path)) = state.vanish.take_if(|(seen, _)| *seen == method) {
                    let _ = std::fs::remove_file(path);
                }
                match answer {
                    Ok(result) => json!({ "id": request["id"], "result": result }),
                    Err(message) => json!({
                        "id": request["id"],
                        "error": { "code": "invalid_request", "message": message },
                    }),
                }
            };
            if (&stream)
                .write_all(format!("{reply}\n").as_bytes())
                .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{socket_path, stuck_herdr, FakeHerdr};
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    const FIXTURE: &str = r#"{"id":"whatsup","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1","number":2,"label":"h1","focused":false,"pane_count":1,"agent_status":"unknown","future_field":1}]}}"#;

    fn ws(number: u64, label: &str) -> HerdrWorkspace {
        HerdrWorkspace {
            number,
            label: label.into(),
        }
    }

    /// Serves one connection on a fresh socket with `serve`.
    fn fake_herdr(
        serve: impl FnOnce(std::os::unix::net::UnixStream) + Send + 'static,
    ) -> std::path::PathBuf {
        let path = socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve(stream);
            }
        });
        path
    }

    #[test]
    fn prefix_boundaries() {
        for (label, prefix) in [
            ("h1", "h1"),
            ("h1-herberth", "h1"),
            ("H1-x", "h1"),
            ("h1 x", "h1"),
            ("h1_x", "h1"),
            ("h1.x", "h1"),
        ] {
            assert!(
                label_matches(label, prefix),
                "{label} should match {prefix}"
            );
        }
        for (label, prefix) in [
            ("h10-e2e-testing", "h1"),
            ("h1x", "h1"),
            ("xh1", "h1"),
            ("h", "h1"),
        ] {
            assert!(
                !label_matches(label, prefix),
                "{label} should not match {prefix}"
            );
        }
    }

    #[test]
    fn matching_orders_by_number() {
        let workspaces = [ws(5, "h6-b"), ws(3, "h6"), ws(4, "h60")];
        assert_eq!(matching_labels(&workspaces, "h6"), vec!["h6", "h6-b"]);
    }

    #[test]
    fn parse_reply_reads_real_shape() {
        assert_eq!(parse_reply(FIXTURE), Some(vec![ws(2, "h1")]));
        assert_eq!(
            parse_reply(r#"{"id":"whatsup","error":{"code":"invalid_request","message":"x"}}"#),
            None
        );
        assert_eq!(parse_reply("not json"), None);
        assert_eq!(
            parse_reply(r#"{"id":"whatsup","result":{"type":"pane_list","workspaces":[]}}"#),
            None
        );
    }

    #[tokio::test]
    async fn list_workspaces_round_trip() {
        let (sent_tx, sent_rx) = std::sync::mpsc::channel();
        let path = fake_herdr(move |stream| {
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            sent_tx.send(line).unwrap();
            (&stream)
                .write_all(format!("{FIXTURE}\n").as_bytes())
                .unwrap();
        });
        assert_eq!(
            list_workspaces(&path, QUERY_BUDGET).await,
            Some(vec![ws(2, "h1")])
        );
        assert_eq!(
            sent_rx.recv().unwrap(),
            "{\"id\":\"whatsup\",\"method\":\"workspace.list\",\"params\":{}}\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn list_workspaces_eof_is_fast_none() {
        let path = fake_herdr(drop);
        let started = Instant::now();
        assert_eq!(list_workspaces(&path, QUERY_BUDGET).await, None);
        assert!(started.elapsed() < Duration::from_millis(200));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn list_workspaces_silent_server_times_out() {
        let path = fake_herdr(|stream| {
            std::thread::sleep(Duration::from_secs(2));
            drop(stream);
        });
        let started = Instant::now();
        assert_eq!(
            list_workspaces(&path, Duration::from_millis(300)).await,
            None
        );
        assert!(started.elapsed() < Duration::from_millis(600));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn list_workspaces_full_accept_queue_times_out() {
        let path = socket_path();
        let listener = stuck_herdr(&path);
        let started = Instant::now();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let query = path.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(list_workspaces(&query, Duration::from_millis(300)));
            let _ = done_tx.send(result);
        });
        let result = done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("list_workspaces hung on a full accept queue");
        assert_eq!(result, None);
        assert!(started.elapsed() < Duration::from_millis(600));
        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn list_workspaces_missing_socket_is_none() {
        assert_eq!(list_workspaces(&socket_path(), QUERY_BUDGET).await, None);
    }

    fn client(socket: &std::path::Path, timeout: Duration) -> HerdrClient {
        HerdrClient {
            socket: socket.to_path_buf(),
            timeout,
        }
    }

    #[test]
    fn call_returns_result_or_error_message() {
        let path = socket_path();
        let fake = FakeHerdr::start(&path);
        let ws = fake.add_workspace(4, "h1-x");
        let herdr = client(&path, QUERY_BUDGET);
        assert_eq!(
            herdr.workspaces(),
            Ok(vec![(ws.clone(), 4, "h1-x".to_string())])
        );
        assert_eq!(
            herdr.call("tab.list", json!({ "workspace_id": "nope" })),
            Err(CallError::Failed("workspace nope not found".into()))
        );
        assert_eq!(
            fake.requests()[0],
            ("workspace.list".to_string(), json!({}))
        );
    }

    #[test]
    fn call_reads_split_reply_and_sends_one_line() {
        let (sent_tx, sent_rx) = std::sync::mpsc::channel();
        let path = fake_herdr(move |stream| {
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            sent_tx.send(line).unwrap();
            let reply = r#"{"id":"whatsup","result":{"type":"ok"}}"#;
            let (head, tail) = reply.split_at(10);
            (&stream).write_all(head.as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            (&stream).write_all(format!("{tail}\n").as_bytes()).unwrap();
        });
        assert_eq!(
            client(&path, QUERY_BUDGET)
                .call("tab.rename", json!({ "tab_id": "t1", "label": "web" })),
            Ok(json!({ "type": "ok" }))
        );
        let sent: Value = serde_json::from_str(&sent_rx.recv().unwrap()).unwrap();
        assert_eq!(sent["method"], "tab.rename");
        assert_eq!(sent["params"], json!({ "tab_id": "t1", "label": "web" }));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn call_without_a_connection_or_an_answer() {
        // No socket: nothing was sent.
        assert_eq!(
            client(&socket_path(), QUERY_BUDGET).call("workspace.list", json!({})),
            Err(CallError::NoConnection)
        );
        let closed = fake_herdr(drop);
        assert_eq!(
            client(&closed, QUERY_BUDGET).call("workspace.list", json!({})),
            Err(CallError::NoAnswer)
        );
        let garbage = fake_herdr(|stream| {
            let _ = (&stream).write_all(b"not json\n");
        });
        assert_eq!(
            client(&garbage, QUERY_BUDGET).call("workspace.list", json!({})),
            Err(CallError::NoAnswer)
        );
        let _ = std::fs::remove_file(closed);
        let _ = std::fs::remove_file(garbage);
    }

    #[test]
    fn call_to_a_silent_herdr_times_out() {
        let path = fake_herdr(|stream| {
            std::thread::sleep(Duration::from_secs(2));
            drop(stream);
        });
        let started = Instant::now();
        assert_eq!(
            client(&path, Duration::from_millis(300)).call("workspace.list", json!({})),
            Err(CallError::NoAnswer)
        );
        assert!(started.elapsed() < Duration::from_millis(600));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn call_to_a_full_accept_queue_times_out() {
        let path = socket_path();
        let listener = stuck_herdr(&path);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let herdr = client(&path, Duration::from_millis(300));
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = done_tx.send(herdr.call("workspace.list", json!({})));
        });
        let result = done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("call hung on a full accept queue");
        // Whether the connect itself times out (Linux) or gets through and
        // the reply never comes depends on the platform; either way the call
        // ends within its timeout.
        assert!(
            matches!(result, Err(CallError::NoConnection | CallError::NoAnswer)),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_millis(600));
        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn idle_needs_both_ids_and_equal() {
        let path = socket_path();
        let fake = FakeHerdr::start(&path);
        let ws = fake.add_workspace(1, "h1");
        let (_, pane) = fake.add_tab(&ws, "web");
        let herdr = client(&path, QUERY_BUDGET);
        assert_eq!(herdr.idle(&pane), Ok(true));
        for (shell, foreground) in [
            (Some(100), Some(101)),
            (None, Some(100)),
            (Some(100), None),
            (None, None),
        ] {
            fake.set_shell(&pane, shell, foreground);
            assert_eq!(herdr.idle(&pane), Ok(false), "{shell:?} {foreground:?}");
        }
    }

    /// A reader that plays back `steps`: bytes, or an error of that kind.
    struct Scripted(std::collections::VecDeque<Result<&'static [u8], std::io::ErrorKind>>);

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front() {
                Some(Ok(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Some(Err(kind)) => Err(kind.into()),
                None => Ok(0),
            }
        }
    }

    #[test]
    fn read_reply_retries_an_interrupted_read() {
        // A signal can interrupt a read (EINTR); std does not retry it.
        use std::io::ErrorKind::{Interrupted, TimedOut};
        let mut stream = Scripted(
            [
                Ok(&b"{\"id\":\"whatsup\",\"res"[..]),
                Err(Interrupted),
                Ok(&b"ult\":{\"type\":\"ok\"}}\n"[..]),
            ]
            .into(),
        );
        let mut armed = 0;
        let reply = read_reply(&mut stream, |_| {
            armed += 1;
            Ok(())
        });
        assert_eq!(reply, Ok(json!({ "type": "ok" })));
        assert_eq!(armed, 3, "each retry is bounded by the deadline again");

        let mut stream = Scripted([Err(Interrupted), Err(TimedOut)].into());
        assert_eq!(
            read_reply(&mut stream, |_| Ok(())),
            Err(CallError::NoAnswer)
        );
        // Once the deadline has passed, an interrupted read is not retried.
        let mut stream = Scripted([Err(Interrupted), Ok(&b"{}\n"[..])].into());
        let mut deadline_left = 1;
        let reply = read_reply(&mut stream, |_| match deadline_left {
            0 => Err(CallError::NoAnswer),
            _ => {
                deadline_left -= 1;
                Ok(())
            }
        });
        assert_eq!(reply, Err(CallError::NoAnswer));
    }

    #[test]
    fn local_socket_prefers_absolute_xdg() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            local_socket(&env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/home/a")])),
            Some(PathBuf::from("/x/herdr/herdr.sock"))
        );
        assert_eq!(
            local_socket(&env(&[("XDG_CONFIG_HOME", "rel"), ("HOME", "/home/a")])),
            Some(PathBuf::from("/home/a/.config/herdr/herdr.sock"))
        );
        assert_eq!(local_socket(&env(&[])), None);
    }
}

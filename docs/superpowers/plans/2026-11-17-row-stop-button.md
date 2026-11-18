# Row Stop Button Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every dashboard row gets a two-click Stop button that stops whatever serves the row's ports, on the row's machine.

**Architecture:**
- A new `src/stop.rs` resolves each port to a target (docker compose project, docker container, process group, nothing) and stops it. It sits behind a `Stopper` trait so the server tests record calls.
- `POST /api/stop` checks the row against the config, then either runs the local `Stopper` or forwards the same JSON through the remote's SOCKS tunnel to the WhatsUp on that machine.
- `/api/dashboard` adds `index` and `ports` to rows and `can_stop` to machines; `dashboard.html` renders the button and keeps its state outside the DOM.

**Tech Stack:** Rust (axum, tokio, serde_json, libc), `lsof` / `ss` / `docker` as external commands, vanilla JS in `assets/dashboard.html`.

**Spec:** `docs/superpowers/specs/2026-11-17-row-stop-button-design.md`

## Global Constraints

- Docker first, then process, then nothing; see the spec's "What Stop does to one port" for the exact commands. Never `docker compose down`. Never `docker ps --filter publish=`.
- Grace: SIGTERM, up to 5 s for the port to close, SIGKILL, up to 2 s more. Docker stop commands are killed after 60 s, the `docker ps` listing after 5 s (then docker owns nothing).
- Group signal only when the group is one job (every member is a listener, an ancestor of one, or a descendant of one); otherwise the listener pids alone.
- A stop runs to the end when the client disconnects; the in-flight guard is released by the stop itself, not by the handler.
- A port whose link host does not resolve to loopback only is refused: `port N: <host> is not this machine`.
- Never signal: WhatsUp's own pid; a group that is WhatsUp's own group or has id 0 or 1 (signal the pid alone); a process whose name contains `docker`.
- `docker` lookup order: `PATH`, `/usr/local/bin`, `/opt/homebrew/bin`, `~/.docker/bin`, `/Applications/Docker.app/Contents/Resources/bin`.
- API statuses: 200 ok, 400 bad row, 403 Host/Origin, 409 config changed or already stopping, 502 a target failed or the remote is unreachable. Error strings as written in the spec.
- Forward: `Host: 127.0.0.1:<whatsup_port>`, `Connection: close`, no `Origin`, read timeout 90 s.
- Armed state lasts 4 s. Button text: `Stop`, `Confirm`, `Stopping...`.
- Never use the em dash. Commits have no agent co-author. Branch: `jds-7-row-stop-button`.

## Review Focus

1. A port held by Docker Desktop while `docker` cannot be queried: the stop must refuse, not kill `com.docker.backend`. Test in Task 2 (`docker_named_listener_is_refused`).
2. A listener that ignores SIGTERM: killed after the grace period, and reported ok. Test in Task 2.
3. Config edited between paint and click (row moved): 409, nothing stopped. Test in Task 4.
4. Remote WhatsUp not running, or too old (404): 502 with the spec's message, and `can_stop` false. Tests in Task 4.
5. The table rebuilds mid-stop: the button must still read `Stopping...`. Checked in the browser in Task 5.

---

### Task 1: Config and SOCKS plumbing

**Files:**
- Modify: `src/config.rs` (`Remote`, `RawRemote`, `Row`, tests)
- Modify: `src/socks.rs` (connect split out, `post_json`, fake `Act::Relay`, tests)
- Modify: `src/tunnel.rs` (the `Remote { .. }` literals)

**Interfaces (produces):**
- `Remote.whatsup_port: u16` (default `crate::PREFERRED_PORT`; `0` is an error).
- `Row::ports(&self) -> Vec<u16>`: distinct link ports, ascending.
- `socks::socks_connect(socks_port: u16, host: &str, port: u16, timeout: Duration) -> Result<TcpStream, Reach>` (`Err` is `Down` or `Offline`); `socks_reach` is rewritten on top of it with unchanged behavior.
- `socks::post_json(socks_port: u16, port: u16, path: &str, body: &str, read_timeout: Duration) -> Result<(u16, String), PostError>` (`PostError::Connect` when the tunnel or the port did not connect, `PostError::Reply` when it connected but no complete HTTP reply came): connects to `127.0.0.1:port` through the tunnel (connect stages use `REMOTE_TIMEOUT`), sends the request from Global Constraints, reads to EOF, returns status and body.
- `socks::fake::Act::Relay`: reply success, then copy bytes both ways to the real `host:port`.

- [ ] **Step 1:** Tests: `parses_remotes_with_defaults` expects `whatsup_port: 47654`; a remote with `"whatsup_port": 5000` parses and `0` is rejected; `row_ports_are_distinct_and_sorted` (links on 8004, 3004, 8004 give `[3004, 8004]`); `post_json_through_relay` (a one-shot local HTTP listener behind `Act::Relay` sees `POST /api/stop`, the Host header and the body, and its `409` + body come back); `post_json_fails_without_a_listener` (`Act::Close` gives `Err`).
- [ ] **Step 2:** Run `cargo test`; the new tests fail to compile.
- [ ] **Step 3:** Implement. Run `cargo test`: all pass.
- [ ] **Step 4:** Commit `config, socks: whatsup_port and a JSON POST through the tunnel (#7)`.

### Task 2: `src/stop.rs`

**Files:**
- Create: `src/stop.rs`; Modify: `src/lib.rs` (`pub mod stop;`), `Cargo.toml` (`libc` moves to `[dependencies]`)

**Interfaces (produces):**

```rust
pub struct PortResult { pub port: u16, pub action: Action, pub detail: String, pub error: Option<String> }
pub enum Action { Process, Compose, Container, None }   // as_str(): "process" | "compose" | "container" | "none"
pub trait Stopper: Send + Sync { fn stop(&self, ports: &[u16]) -> Vec<PortResult>; }
pub struct SystemStopper { pub docker: Option<PathBuf>, pub grace: Duration, pub kill_wait: Duration, pub command_timeout: Duration, pub list_timeout: Duration }
impl SystemStopper { pub fn new() -> Self }             // finds docker, 5 s / 2 s / 60 s / 5 s
fn group_is_one_job(table: &[(i32, i32, i32)] /*pid, ppid, pgid*/, group: i32, listeners: &[i32]) -> bool
pub fn find_docker(path: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf>
fn parse_listener_pids(output: &str) -> Vec<u32>         // accepts both `p123` lines (lsof -Fp) and `pid=123` (ss)
fn docker_owner(ps_output: &str, port: u16) -> Option<(String /*id*/, String /*project, may be empty*/)>
```

Results come back one per requested port, in request order; ports sharing a target share its outcome.

- [ ] **Step 1:** Parser tests, with the spec's real samples:
  - `parse_listener_pids("p25348\nf25\n")` is `[25348]`; the lucy `ss` line with `users:(("node",pid=1175308,fd=19))` is `[1175308]`; an `ss` line with two `pid=` gives both; the docker-proxy line without `users:` gives `[]`.
  - `docker_owner`: `0.0.0.0:8004->8000/tcp, [::]:8004->8000/tcp` owns 8004 and not 8000; `0.0.0.0:9000-9002->9000-9002/tcp` owns 9001; `5432/tcp` owns nothing; an empty project comes back as `""`.
  - `find_docker` prefers `PATH`, then the fixed directories (use temp dirs).
- [ ] **Step 2:** Process tests. The child is this test binary re-run with `--exact stop::tests::helper_listener --nocapture` and env `WHATSUP_TEST_LISTEN=<file>` (it binds `127.0.0.1:0`, writes the port to the file, sleeps; with `WHATSUP_TEST_IGNORE_TERM=1` it first ignores SIGTERM). Spawn it through `sh -c '"$0" "$@" & wait'` with `process_group(0)` so `sh` is its group mate.
  - `stops_the_listeners_process_group`: result is `Action::Process`, no error; the port is closed; both `sh` and the child are gone.
  - `kills_a_listener_that_ignores_term` (`grace` 300 ms): no error, port closed.
  - `group_is_one_job` unit cases: shell > just > pnpm > vite all in one group is one job; the same group plus an unrelated sibling is not; and `a_shared_group_gets_pid_only_signals` (two helper listeners backgrounded by one `sh`: stopping the first port leaves the second listening).
  - `closed_port_is_not_running`: port 1 gives `Action::None`, no error.
  - `docker_named_listener_is_refused`: unit test of the guard function `refusal(pid, name, own_pid) -> Option<String>` with name `com.docker.backend`, and with `pid == own_pid`.
- [ ] **Step 3:** Docker tests with a fake `docker` script (prints a canned `ps` table for `ps`, appends its arguments to a log file otherwise):
  - `compose_project_is_stopped_once`: ports 8004 and 8005 both map to project `p1`; the log has exactly one `compose -p p1 stop`; both results are `Action::Compose` with detail `p1`.
  - `container_without_project_is_stopped`: log has `stop <id>`.
  - `failing_docker_command_is_an_error`: the fake exits 1 with stderr `boom`; the result's error contains `boom`.
  - `slow_command_is_killed` (`command_timeout` 300 ms, fake sleeps 5 s): error mentions the timeout, test finishes in under 2 s.
  - `a_hung_docker_listing_does_not_block_processes` (`list_timeout` 200 ms, the fake's `ps` sleeps): a real helper listener is still stopped.
- [ ] **Step 4:** Implement; `cargo test stop::` passes on macOS. Commit `stop: find and stop what serves a port (#7)`.

### Task 3: Dashboard JSON

**Files:** Modify `src/server.rs`

- [ ] **Step 1:** Tests: every row in `/api/dashboard` has `index` (config index, so a lucy-only second row is `1` in lucy's section) and `ports`; `machines[0].can_stop` is `true`; with a fake SOCKS that answers everything, the remote's `can_stop` is `true` and `ports_total` is unchanged; when the fake closes `127.0.0.1:47654` only, `can_stop` is `false` and `offline` is `false`. Update `no_remotes_view_is_unchanged` for the new machine key.
- [ ] **Step 2:** Implement: add `("127.0.0.1", whatsup_port)` to each remote's probe targets, compute counts from the link targets only, add `can_stop` to `MachineStatus`.
- [ ] **Step 3:** `cargo test` passes. Commit `dashboard: rows carry index and ports, machines carry can_stop (#7)`.

### Task 4: `POST /api/stop`

**Files:** Modify `src/server.rs`, `src/main.rs`

**Interfaces:** `AppState::new` gains a last parameter `stopper: Arc<dyn Stopper>`; `main.rs` passes `Arc::new(SystemStopper::new())`. `Inner` gains `stopping: Mutex<BTreeSet<(String, usize)>>`.

- [ ] **Step 1:** Tests with a recording `Stopper` (optionally blocking on a channel, optionally returning an error result):
  - `stop_runs_the_rows_ports`: 200, recorder saw the row's ports, body has `ok: true` and `results`.
  - `stop_rejects_a_changed_row`: wrong `ports` gives 409 and the spec's message; nothing recorded.
  - `stop_rejects_bad_rows`: unknown group, row out of range, a row with no links, a lucy-only row asked for the local machine: 400.
  - `stop_guards_host_and_origin`: `Host: evil.example` is 403; `Origin: http://evil.example` is 403; `Origin: http://127.0.0.1:<port>` with a matching Host is 200.
  - `stop_refuses_a_row_already_stopping`: second request while the first blocks is 409 `already stopping`; after release a third is 200.
  - `stop_refuses_a_host_that_is_not_this_machine`: a row linking `http://192.0.2.1:8004/` gives 502 with `port 8004: 192.0.2.1 is not this machine`; nothing recorded.
  - `stop_finishes_after_the_client_leaves`: the client closes its socket while the recorder blocks; after release the recorder has run and a new request is 200 (not 409).
  - `stop_reports_failures`: a result with an error gives 502 and `error` contains `port 8004:`.
  - `stop_is_forwarded_to_the_remote`: two servers; the second has `machine_names` mapping the test hostname to `lucy`; the first reaches it through `Act::Relay` with `whatsup_port` set to its port. The second's recorder saw the ports, the first's saw nothing, status and body are relayed.
  - `stop_says_when_the_remote_is_not_running` (`Act::Close`) and `stop_says_when_the_remote_cannot_stop` (relay to a one-shot listener answering 404): 502 with the spec's messages.
- [ ] **Step 2:** Implement the handler. Local stops run in `spawn_blocking`; the forward too. The in-flight entry is removed by a guard on drop.
- [ ] **Step 3:** `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` all clean. Commit `server: /api/stop, local or forwarded to the remote WhatsUp (#7)`.

### Task 5: The button

**Files:** Modify `assets/dashboard.html`, `README.md`

- [ ] **Step 0:** Sticky action errors: a `actionError` variable beside the config error; `banner` shows `config_error || actionError`; set by failed stops and opens, cleared when a stop or open starts or succeeds and when the banner is clicked.
- [ ] **Step 1:** Add the last column and the button with the four states from the spec. State map `stopState: Map<"machine/group/index", "armed" | "stopping">`; `buildTable` reads it; a state change repaints only that button when it is still in the DOM.
- [ ] **Step 2:** README: a "Stopping a stack" section (what Stop does, the two clicks, that a remote's WhatsUp must be running, `whatsup_port`).
- [ ] **Step 3:** Run `cargo run -- --port 47700 --no-open --config <the shared config>` and check in the browser (megan:Personal profile): idle, disabled with tooltip, armed, back to idle after 4 s, column alignment in every group, the 1280 px and narrow layouts. Commit `dashboard: Stop button on each row (#7)`.

### Task 6: Linux, install, real-machine checks

- [ ] **Step 1:** Push the branch; on lucy fetch it, `cargo test` (this is where `ss` is exercised), `cargo clippy`.
- [ ] **Step 2:** Install on both: megan `packaging/build-app.sh` and replace `~/Applications/WhatsUp.app`; lucy `packaging/install-linux.sh`. Restart both WhatsUps.
- [ ] **Step 3:** Run the spec's real-machine checks 1 to 6. Only stop stacks the user has agreed to lose, or throwaway listeners started for the test on a row's port.
- [ ] **Step 4:** Merge to `main` as the earlier features were (`Merge branch 'jds-7-row-stop-button': ...`).

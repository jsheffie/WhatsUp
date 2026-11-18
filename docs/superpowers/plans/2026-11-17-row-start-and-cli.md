# Row Start Button and Command Line Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A one-click Start button per row that starts what is down (docker
from existing containers, other servers in herdr tabs), forwarded to the
remote WhatsUp for remote rows, plus `whatsup list|start|stop` on the
command line.

**Architecture:** The server works out which ports are down and builds a
start plan for each (`PortStart`). A `Starter` trait runs the plan:
`SystemStarter` starts docker and herdr on two threads, and tests record
the plan. `/api/start` shares Stop's row check, guard, in-flight mark and
forwarding through one generalised handler. The command line is a
blocking HTTP client in the same binary that talks to the running
WhatsUp.

**Tech Stack:** Rust (axum 0.8, tokio, serde_json, libc), std blocking
sockets for herdr and HTTP, vanilla JS in `assets/dashboard.html`.

**Spec:** `docs/superpowers/specs/2026-11-17-row-start-and-cli-design.md`

## Global Constraints

- Branch `jds-9-row-start-and-cli`; commit messages end with `(#9)`; no
  co-author lines; never the em dash in code, docs or copy.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
  stay clean after every task.
- Timeouts: docker listing 5 s per command, docker start 120 s per command,
  herdr call 1 s, herdr link budget 30 s, prompt wait 10 s polled every
  200 ms, early-exit watch 5 s polled every 500 ms, forwarded start read
  timeout 180 s, CLI start/stop read timeout 210 s, CLI dashboard read
  timeout 10 s, `--wait` default 180 s polled every 2 s, dashboard
  Starting... limit 3 minutes.
- Typed line, exactly:
  `cd -- '<dir>' && { <run>; }; echo "[whatsup <nonce>] exited with $?"`.
- Exit codes: 0 success, 1 action failed or timed out, 2 usage, unknown row
  or WhatsUp not running.
- Error copy is the spec's, word for word, except that `PortResult.error`
  never carries the leading "port N: ". `action_response` adds it when it
  joins the errors, as `stop_response` does today. Tests in Tasks 2 and 3
  assert the bare text.
- Every herdr socket exchange (connect included) is bounded. A std
  `UnixStream::connect` has no timeout and blocks forever on Linux against
  a herdr that stopped accepting. A start that never returns would keep the
  row's busy mark until WhatsUp restarts.
- A forwarded request carries only `{machine, group, row, ports}`.

## Review Focus

1. `dir` with spaces or a single quote (`~/work/it's here`): the typed line
   must still `cd` there. Test `typed_line` with both.
2. A herdr tab with the link's label that has been split into several
   panes: use the first pane of that tab in `pane.list` order, and decide
   idle or busy from that pane alone. A null `shell_pid` or
   `foreground_process_group_id` counts as busy. Test both in the fake
   herdr.
3. A stale scrollback marker from an earlier run in a reused tab must not
   read as an early exit. The nonce differs per run. Test: the pane already
   shows `[whatsup oldnonce] exited with 1` and the new start succeeds.
4. "Already up" must agree with the dashboard's green. Up means
   `probe::port_is_open(port) || link_is_up(host, port)`. A vite on `::1`
   only, behind a name that resolves to 127.0.0.1, is up, and Start must
   not launch a second one. Test with a `[::1]`-only listener and host
   `127.0.0.1`, and with a 127.0.0.1 listener and host `localhost`.
5. An older remote WhatsUp (404 on `/api/start`) and a stale instance file
   (dead pid) for the CLI: "WhatsUp on lucy cannot start stacks; update
   it" and "WhatsUp is not running; start it first" (exit 2). Tested in
   Tasks 4 and 6.

---

### Task 1: Config: link `start` and row `id`

**Files:**
- Modify: `src/config.rs` (types near lines 49-110, `RawRow` at 405, `into_row`, `parse_cell` at 532, group loop in `into_dashboard`)

**Interfaces:**
- Produces:
  - `pub struct StartCommand { pub dir: String, pub run: String }` (`dir` as written; expansion happens at start time)
  - `Link.start: Option<StartCommand>`
  - `Row.id: Option<String>`
  - `Row::herdr_prefix(&self) -> Option<&str>` (the first `Cell::Herdr` prefix)
  - `Row::name(&self, group_id: &str, index: usize) -> String` (`id`, else `group/index`)
  - `pub fn expand_dir(dir: &str, home: &Path) -> PathBuf`

- [ ] **Step 1: Write failing tests** in `config.rs` tests:
  - `link_start_and_row_id_parse`: `start: {dir:"~/workspace/huvr2", run:"tt2a && cd frontend && npm start"}` gives the `StartCommand`; `"id":"jds2"` gives `Some("jds2")`; `row.name("pipelinecore", 1) == "jds2"`, and without an id `"pipelinecore/1"`.
  - `start_needs_a_herdr_cell`: error contains `start commands need a herdr cell to name the workspace`.
  - `start_rejects_bad_fields`: `run` blank, `run` containing `\n`, `dir` relative (`workspace/x`), an unknown key inside `start`: each is an error naming the cell.
  - `two_starts_on_one_port_are_rejected`.
  - `row_ids_are_checked`: `"jds 2"`, a 33-character id, and `jds2` / `JDS2` in two groups are errors (`duplicate row id`).
  - `expand_dir_expands_home`: `~/a b` → `<home>/a b`; `/abs` unchanged.
- [ ] **Step 2: Run** `cargo test --lib config::` and expect the new tests to fail to compile.
- [ ] **Step 3: Implement.** In `parse_cell`, `known` gains `start`. Parse `start` from its object: only the keys `dir` and `run`, both strings. `id` is validated in `into_row`. Uniqueness is checked (lowercase) in `into_dashboard` across all groups. The herdr-cell rule and the one-start-per-port rule go in `into_row`. `Link` keeps deriving `PartialEq, Eq`. Fix the existing `Link { .. }` literals in tests and other modules (`start: None`).
- [ ] **Step 4: Run** `cargo test` and expect all to pass. Run clippy.
- [ ] **Step 5: Commit** `config: link start commands and row ids (#9)`.

### Task 2: Docker start from existing containers

**Files:**
- Create: `src/start.rs` (add `pub mod start;` in `src/lib.rs`)
- Modify: `src/stop.rs` (extract `SystemStopper::docker` into `pub(crate) fn run_docker(docker: &Path, args: &[&str], timeout: Duration) -> Result<String, String>`; add `Action::Herdr` with `as_str` `"herdr"`)

**Interfaces:**
- Consumes: `stop::{Action, PortResult, find_docker}`, `run_docker`.
- Produces:
  - `pub enum StartKind { Docker, Herdr { prefix: String, label: String, dir: PathBuf, run: String } }`
  - `pub struct PortStart { pub port: u16, pub kind: StartKind }` (both derive `Clone, Debug, PartialEq, Eq`; Task 4's recorder compares plans)
  - `pub trait Starter: Send + Sync { fn start(&self, plan: &[PortStart]) -> Vec<PortResult>; }` (one result per entry, same order)
  - `pub struct HerdrTimings { pub call: Duration, pub prompt_wait: Duration, pub prompt_poll: Duration, pub watch: Duration, pub watch_poll: Duration, pub link_budget: Duration }`, with `Default` set to the Global Constraints values (defined here; Task 3 uses it)
  - `pub struct SystemStarter { pub docker: Option<PathBuf>, pub list_timeout: Duration, pub command_timeout: Duration, pub herdr: Option<PathBuf>, pub herdr_timings: HerdrTimings, pub machine: String }` with `SystemStarter::new(herdr_socket: Option<PathBuf>, machine: String)`
  - `fn parse_bindings(json: &str) -> Vec<(u16, u16)>` (inclusive host-port ranges; an empty `HostPort` is skipped)
  - `fn docker_owners(ps_all: &str, inspect: &str, port: u16) -> Result<DockerTarget, String>`, with `enum DockerTarget { Compose(String), Container { id: String, name: String } }`

- [ ] **Step 1: Write failing tests** in `start.rs`:
  - `bindings_parse_single_range_and_empty`: `{"8000/tcp":[{"HostIp":"","HostPort":"8002"}],"9000/tcp":[{"HostPort":"9000-9002"}],"5432/tcp":[{"HostPort":""}]}` gives `[(8002,8002),(9000,9002)]`. `{}` and `null` give `[]`.
  - `owner_is_the_compose_project`, `owner_without_project_is_the_container`, `two_owners_fail` (the error lists both names), `no_owner_fails` (`no start command, and no container publishes it`).
  - `docker_ports_of_one_project_start_once`: a fake docker script (the pattern in `stop.rs` tests) answers `ps` and `inspect` with canned output and logs the argv of every other command. Three `Docker` ports owned by `huvr_jds2` give exactly one logged line, `compose -p huvr_jds2 start`, and three results with `Action::Compose`. A project-less container gives `start <id>` and `Action::Container`.
  - `docker_missing_fails_each_docker_port`.
  - `docker_listing_failure_is_reported`: a fake docker whose `ps` exits 1 with `Cannot connect to the Docker daemon` on stderr. Every docker port fails with that docker error, not with "no container publishes it".
- [ ] **Step 2: Run** `cargo test --lib start::` and expect failure.
- [ ] **Step 3: Implement** the docker half of `SystemStarter::start`:
  - one `ps -a --no-trunc --format {{.ID}}\t{{.Label "com.docker.compose.project"}}`;
  - one `inspect --format {{.Id}}\t{{.Name}}\t{{json .HostConfig.PortBindings}} <ids...>` (skipped when there are no containers);
  - a failed `ps -a` or `inspect` fails every docker port with docker's error (unlike Stop, which treats it as "no containers");
  - dedupe targets and run them sequentially, one after another, each with `command_timeout` (not on parallel threads, as `SystemStopper::stop` does);
  - detail is the command as shown, for example `docker compose -p huvr_jds2 start`.

  Herdr entries return a "not built yet" error until Task 3.
- [ ] **Step 4: Run** `cargo test` and expect a pass. Run clippy.
- [ ] **Step 5: Commit** `start: start docker ports from their containers (#9)`.

### Task 3: herdr launcher

**Files:**
- Modify: `src/herdr.rs` (blocking client and writes; `testing` gains a scripted fake)
- Modify: `src/start.rs` (herdr half of `SystemStarter`)

**Interfaces:**
- Produces in `herdr.rs`:
  - `pub struct HerdrClient { pub socket: PathBuf, pub timeout: Duration }`
  - `fn call(&self, method: &str, params: Value) -> Result<Value, String>`:
    - one connection per call, std `UnixStream`;
    - the whole exchange is bounded by `timeout`, connect included: connect on a helper thread and `recv_timeout`, then set read and write timeouts;
    - returns `result`, and an `error.message` becomes `Err`.
  - Typed wrappers: `workspaces() -> Vec<(String id, u64 number, String label)>`, `create_workspace(label, cwd) -> (ws, tab, pane)`, `tabs(ws) -> Vec<(tab, label)>`, `create_tab(ws, cwd, label) -> (tab, pane)`, `rename_tab(tab, label)`, `panes(ws) -> Vec<(pane, tab)>` (in `pane.list` order), `idle(pane) -> bool` (true only when `shell_pid` and `foreground_process_group_id` are both present and equal), `read(pane, lines) -> String`, `type_line(pane, text)` (`pane.send_input` with `keys:["Enter"]`)
- Produces in `start.rs`:
  - `fn typed_line(dir: &Path, run: &str, nonce: &str) -> String`
  - `fn early_exit(text: &str, nonce: &str) -> Option<(i32, Vec<String>)>`: the code, and the last 5 non-blank lines between the echoed command line (the line containing `[whatsup <nonce>] exited with $?`) and the marker
  - `fn launch(client: &HerdrClient, prefix: &str, label: &str, dir: &Path, run: &str, timings: &HerdrTimings, machine: &str) -> Result<String, String>` (Ok = detail `herdr tab <label> in <workspace label>`)
- Test fake `herdr::testing::FakeHerdr::start(path) -> FakeHerdr`:
  - holds workspaces, tabs and panes in a mutex and answers the methods above;
  - records every request (method and params);
  - `set_busy(pane)` marks a pane busy, and `on_type(fn(&str) -> String)` sets the pane output that appears after a typed line.

- [ ] **Step 1: Write failing tests:**
  - `typed_line_quotes_dir`: `/tmp/a b/it's` and `npm start` give exactly `cd -- '/tmp/a b/it'\''s' && { npm start; }; echo "[whatsup N1] exited with $?"`.
  - `early_exit_finds_only_this_nonce`: text containing `[whatsup OLD] exited with 1` gives `None` for nonce `NEW`. Text with lines `old output`, `% cd -- '/x' && { y; }; echo "[whatsup NEW] exited with $?"`, `a`, ``, `boom`, `[whatsup NEW] exited with 127` gives `Some((127, ["a","boom"]))`: the echoed command and the lines above it are excluded, and the echo itself (containing `$?`) is not the marker.
  - With the fake, and timings shortened to ms:
    - `launch_creates_workspace_when_none_matches`: requests include `workspace.create` with label `h2` and cwd = dir, then `tab.rename` to the link label, then `pane.send_input` with the typed line.
    - `launch_uses_first_matching_workspace_and_new_tab`: workspaces `h2-x` (number 3) and `h2-y` (number 1) give a `tab.create` in `h2-y`.
    - `launch_reuses_idle_tab`: no `tab.create`.
    - `launch_refuses_busy_tab`: the error contains `is busy; it may still be starting`, and nothing is typed.
    - `launch_split_tab_uses_first_pane` and `launch_unknown_shell_state_is_busy` (null `shell_pid`) (Review Focus 2).
    - `launch_reports_early_exit`: `on_type` returns `boom\n[whatsup <nonce>] exited with 1`. The error contains `exited with 1` and `boom`, and does not start with `port`.
    - `launch_ignores_stale_marker` (Review Focus 3).
    - `launch_without_herdr_fails`: a missing socket gives `herdr is not running on lucy`.
    - `launch_against_a_stuck_herdr_times_out`: a listener with a full accept queue (move `tiny_backlog_listener` from the private tests into `herdr::testing`). `launch` returns `herdr is not running on lucy` within `link_budget` plus a margin.
  - `system_starter_mixes_docker_and_herdr`: a fake docker plus a missing herdr socket, with the plan `[3002 herdr, 8002 docker, 4002 herdr]`. The results come back in plan order, 8002 is `compose` and succeeded, and 3002 and 4002 failed with the herdr error.
- [ ] **Step 2: Run** `cargo test --lib herdr:: start::` and expect failure.
- [ ] **Step 3: Implement:**
  - `launch`: wait for the prompt only in a new pane; use a nonce from time and pid in hex; stop with the "not running" error if `link_budget` runs out.
  - `SystemStarter::start`: split the plan into docker and herdr entries, run both halves on scoped threads, run the herdr entries one after another, expand `dir` with `expand_dir` and `$HOME`, and merge the results back into plan order.
- [ ] **Step 4: Run** `cargo test` and expect a pass. Run clippy.
- [ ] **Step 5: Commit** `start: launch start commands in herdr tabs (#9)`.

### Task 4: `/api/start`, shared with Stop

**Files:**
- Modify: `src/server.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `Starter`, `PortStart`, `StartKind`, `Row::herdr_prefix`, `expand_dir`.
- Produces:
  - `Actions { opener, stopper, starter: Arc<dyn Starter> }`
  - route `POST /api/start`
  - `Inner.busy: Arc<Mutex<BTreeMap<(String, usize), &'static str>>>` (replaces `stopping`; the value is `"starting"` or `"stopping"`)
  - `fn plan_start(row: &Row, up: impl Fn(&str, u16) -> bool) -> (Vec<PortStart>, Vec<PortResult>)` (start entries plus the already-decided results: foreign host, `none`/"already up"). `dir` stays unexpanded here; it becomes `PathBuf::from(dir)` and the starter expands it.
  - `fn link_is_up(host: &str, port: u16) -> bool`: TCP connect to every address of `(host, port)` with a 300 ms timeout. The server's default up check is `port_is_open(port) || link_is_up(host, port)`.
  - `AppState::with_probe(self, probe: Arc<dyn Fn(&str, u16) -> bool + Send + Sync>) -> Self` (the same pattern as `with_forward_timeout`). HTTP tests inject it, so real dev servers on 3004, 8004 or 3007 cannot change a test's plan.
  - `/api/dashboard` rows gain `"id"`; machines' `can_stop` is renamed `can_act`.
  - `with_forward_timeout` sets both forward timeouts (tests). Constants: `STOP_FORWARD_TIMEOUT` 90 s, `START_FORWARD_TIMEOUT` 180 s.

- [ ] **Step 1: Write failing tests** with a `RecordingStarter` (mirroring `RecordingStopper`) and `STOP_CONFIG` extended with `start` and `id`:
  - `plan_start_*` unit tests through the `up` closure: up ports become `none` / `already up`, links with `start` become `Herdr` (prefix from the herdr cell, label, run), the others become `Docker`, three links on one port give one entry, and a foreign host gives the refusal.
  - `start_skips_ports_that_are_up`: a real ephemeral listener on one row port, with the default probe. The result is `none` / `already up`, and the recorded plan lacks that port.
  - `start_reports_joined_errors`: a starter that fails 4002 with `boom`. The reply is 502 with `error` exactly `port 4002: boom`.
  - The other HTTP tests use `with_probe(|_, _| false)`.
  - `start_refuses_other_sites` (403 for a foreign Origin and for Host `evil.test`).
  - `start_row_checks` (400 bad body or unknown machine, 409 changed ports or missing row).
  - `start_and_stop_share_the_busy_mark`: a stopper that blocks until released. A start sent during the stop gets 409 `already stopping`, and the reverse gets `already starting`.
  - `start_refuses_foreign_hosts`.
  - `start_forwards_to_the_remote` through the SOCKS relay fake to a second server, whose recorder sees the plan.
  - `start_forward_to_old_whatsup` (the second server answers 404): `WhatsUp on lucy cannot start stacks; update it`.
  - `start_forward_errors`: nothing on the remote port gives `WhatsUp is not running on lucy`. A remote that never answers (short forward timeout) gives `lucy did not answer in time; it may still be starting`.
  - `link_is_up_probes_the_link_host` (Review Focus 4).
  - `dashboard_has_row_ids_and_can_act`.
  - Existing stop tests are updated for `can_act` and `busy`, and keep passing.
- [ ] **Step 2: Run** `cargo test --lib server::` and expect failure.
- [ ] **Step 3: Implement.** Turn `stop_row` into `act_on_row(state, headers, body, Act::Start | Act::Stop)`. The pieces differ only in:
  - the path (`/api/start` or `/api/stop`);
  - the forward timeout;
  - the copy: "cannot start stacks" or "cannot stop stacks"; "still be starting" or "still be stopping"; the 403 text "starts must come from this dashboard"; "not a start request: ..."; and "that row has nothing to start";
  - the local work: `plan_start` plus the starter in `spawn_blocking`, merged into port order, or today's stop body.

  `stop_response` becomes `action_response`. In `main.rs`, pass `starter: Arc::new(SystemStarter::new(herdr::local_socket(..), machine label))`. The machine label is resolved from the config, the same way the server does it.
- [ ] **Step 4: Run** `cargo test`, then clippy.
- [ ] **Step 5: Commit** `server: /api/start, local or forwarded, sharing Stop's checks (#9)`.

### Task 5: Dashboard Start button

**Files:**
- Modify: `assets/dashboard.html`

**Interfaces:**
- Consumes: row `index`, `ports`, `id`; machine `can_act`; `/api/start` replies.
- Produces:
  - a `rowState` Map (replacing `stopState`), keyed `machine/group/index`, with values `armed | stopping | starting-request`. It covers only arming and in-flight requests.
  - a `startWatch` Map, key → `{ ports: number[], timer }`. It alone drives Starting... after the reply, so an armed Stop and Starting... can coexist.
  - `renderActionsCell`, `paintActions`, `startClicked`, `checkStartWatches(data)`.
  - `lastData`, the last `/api/dashboard` reply.

- [ ] **Step 1: Implement:**
  - The last column holds Start (a green pill with a play glyph, the same size as Stop) and then Stop.
  - Disabled reasons: `Everything is running` (no link down), offline note, `Start WhatsUp on <machine> to start its stacks`. Stop's reasons and arming are unchanged; its `can_stop` read becomes `can_act`.
  - While `stopping` is in flight, or a start request is in flight, both buttons are disabled.
  - After the reply, a `startWatch` is made only if the reply started a port (results with action other than `none` and no error). The watch ends when all of those ports are up in the row's link cells.
  - `checkStartWatches(lastData)` runs when the reply arrives and on every refresh, before `render`'s early return for an unchanged view.
  - Each watch has its own 3-minute `setTimeout`. When it fires, it ends the watch and posts `<row name>: still waiting for <label> (<port>)...; see its herdr tab`.
  - A Stop click during `starting` clears the watch.
  - An error reply posts `Start failed: <error>` through `notice("action", ...)`.
- [ ] **Step 2: Verify by hand** against a dev server (`cargo run -- --config <scratch config> --port 47660 --no-open`; the installed app must be quit first, because of the single-instance lock):
  - Start enabled, disabled and Starting... states;
  - the tooltips;
  - the banner on a failing `run`;
  - pixel check of both buttons side by side, in a light and a dark row tone, at phone width and desktop width.
- [ ] **Step 3: Commit** `dashboard: Start button beside Stop (#9)`.

### Task 6: Command line

**Files:**
- Create: `src/http.rs` (`pub fn exchange(stream: TcpStream, method: &str, host_port: u16, path: &str, body: Option<&str>, read_timeout: Duration) -> Result<(u16, String), PostError>`; move `PostError` here and re-export it from `socks`, so that `socks::post_json` uses it)
- Create: `src/cli.rs`
- Modify: `src/main.rs` (dispatch), `src/lib.rs`

**Interfaces:**
- Produces in `cli.rs`:
  - `pub fn is_command(arg: &str) -> bool` (`list`, `start`, `stop`)
  - `pub fn run(args: Vec<String>) -> u8` (the exit code)
  - `enum Command { List { json }, Start { row, machine, wait, timeout, json }, Stop { row, machine, json } }`, plus `port: Option<u16>`, from `fn parse(args) -> Result<(Command, Option<u16>), String>`
  - `struct Target { machine, group, index, ports: Vec<u16>, name }` from `fn resolve(dashboard: &Value, row: &str, machine: Option<&str>) -> Result<Target, String>`
  - `fn format_list(dashboard: &Value) -> String`, `fn list_json(dashboard: &Value) -> Value` (the spec's shape)
- `main.rs`: if `env::args().nth(1)` satisfies `is_command`, then `return ExitCode::from(cli::run(args))` before `parse_args`. Help text gains the commands.

- [ ] **Step 1: Write failing tests:**
  - `parse_commands`: valid forms; `--port` after the command; missing row → usage error; unknown flag → usage error.
  - `resolve_rows`: by id (case ignored); by `pipelinecore/3`; unknown → error listing ids; not on that machine → `jds2 is not on lucy`; the default machine = the dashboard's `machine`.
  - `list_formats_rows_and_summary` from a canned dashboard JSON.
  - Against a std `TcpListener` fake that serves canned replies by path:
    - `start_prints_results_and_exits_0`
    - `start_error_exits_1`
    - `wait_succeeds_when_ports_come_up` (the second dashboard read shows them up; `--timeout` short)
    - `wait_times_out_exit_1`
    - `stop_acts_immediately`
    - `no_whatsup_exits_2` (`--port` pointing at a closed port gives `WhatsUp is not running; start it first`)
  - The instance-file path (dead pid) is covered by `instance::read_running` plus `process_alive`, and gets the same message.
- [ ] **Step 2: Run** `cargo test --lib cli:: http::` and expect failure.
- [ ] **Step 3: Implement** with blocking std: `TcpStream::connect_timeout` to `127.0.0.1:<port>`, then `http::exchange`. The poll interval and timeout are fields, so tests shorten them.
- [ ] **Step 4: Run** `cargo test`, then clippy.
- [ ] **Step 5: Commit** `cli: whatsup list, start and stop (#9)`.

### Task 7: README, config, deploy, end to end

**Files:**
- Modify: `README.md` (Start section, the herdr sentence, Command line section with the macOS `ln -s`)
- Modify (not committed here; the user commits it): `~/workspace/jds_private_config/whatsup/config.json`

- [ ] **Step 1:** README per the spec, then commit `docs: Start button and command line (#9)`.
- [ ] **Step 2:** Deploy both machines first (the memory note `whatsup-deploy-megan-lucy` has the commands; code reaches lucy by push to a temp ref). Then link `~/.local/bin/whatsup` on megan.
- [ ] **Step 3:** Generate the config change with a script:
  - `id` on every row;
  - `start` on the portal and tt-calc-app links of `jds`..`jds15` (`tt<N>a && cd frontend && npm start` in `~/workspace/huvr<N>`, and `ttc<N>a && just dev-fresh` in `~/workspace/tt-calc-app<N>`; the base row uses `tt1a` and `ttc1a`, in `huvr` and `tt-calc-app`).

  Check that `whatsup list` shows no config error on either machine. Ask the user to commit and push `jds_private_config`, then pull it on lucy.
- [ ] **Step 4:** Do the spec's "On the real machines" checks 1 to 6, including Stop then Start reusing the tabs. Use a throwaway row with a failing `run` for check 4, removed afterwards.
- [ ] **Step 5:** Run `/code-review` on the branch, fix what it finds, then push and open a PR when the user asks.

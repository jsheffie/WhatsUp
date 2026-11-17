# herdr Workspace Labels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Each row's Workspace cell shows the matching herdr workspace label for that machine, for example `jds/h1-herberth`.

**Architecture:**
- A new `src/herdr.rs` holds three things:
  - a blocking Unix socket client that sends herdr's `workspace.list` request;
  - the hN name-prefix matcher;
  - the default local socket path.
- `/api/dashboard` queries the local socket and each remote's socket alongside the port probes.
- A remote's herdr socket is forwarded over the existing SSH tunnel with `-L`.
- `group_view` adds the matched labels to a new kind of text cell, and `dashboard.html` renders them.

**Tech Stack:** Rust (axum, tokio, serde_json), std `UnixStream`, vanilla JS in `assets/dashboard.html`.

**Spec:** `docs/superpowers/specs/2026-11-16-herdr-workspace-labels-design.md`

## Global Constraints

- Matching rule: label == prefix, or label starts with prefix followed by a char that is not ASCII alphanumeric. ASCII case-insensitive. Order matches by herdr `number`.
- Display:
  - no match: `jds`
  - one match: `jds/h1-herberth`
  - more than one: `jds6/h6 +1`, with tooltip `herdr: h6, h6-hotfix`
- Request line: `{"id":"whatsup","method":"workspace.list","params":{}}\n`. Read only `result.workspaces[].number` and `.label`, and ignore unknown fields.
- The query budget is 1 s in total and replies are capped at 1 MiB. Any failure means "no herdr data", which shows the plain text and is never an error.
- Local socket: `$XDG_CONFIG_HOME/herdr/herdr.sock` if set and absolute, else `$HOME/.config/herdr/herdr.sock`.
- Forward: `-o StreamLocalBindUnlink=yes -L <runtime_dir>/herdr-<socks_port>.sock:<herdr_socket>`, only when `herdr_socket` is set and the local path is under 100 bytes.
- Config:
  - cell form `{ "text": "...", "herdr": "..." }`; any other key is an error;
  - remote field `herdr_socket`: absolute, with no `:` and no whitespace.
- Never use the em dash. Commits have no agent co-author. Branch: `jds-<issue>-herdr-workspace-labels`.

## Review Focus

1. Prefix boundaries: `h1` must not match `h10-e2e-testing` or `h1x`. Covered by a test in Task 2.
2. A remote herdr socket that is missing behind a live tunnel: the server accepts the connection and then sends EOF. That must give "no data" quickly, not a 1 s stall per request. Covered by a test in Task 2 (EOF server).
3. Orphan reclaim of an ssh whose `-L` value has a space. Covered by a test in Task 3.
4. lucy offline while the tunnel is restarting: herdr is not queried (the tunnel is not Running), and the cells are plain. Covered by a test in Task 4.
5. A config without herdr fields gives a byte-identical `/api/dashboard` cell JSON, with no `herdr` key. Covered by a test in Task 4.

---

### Task 0: Issue and branch

- [ ] **Step 1:** Ask the user before opening an issue on `jsheffie/WhatsUp` titled "Show the herdr workspace label in the Workspace column". The body links the spec. Once it is open, create the branch `jds-<N>-herdr-workspace-labels` from `main`.

### Task 1: Config: herdr cell and `herdr_socket`

**Files:**
- Modify: `src/config.rs` (`Cell`, `Remote`, `RawRemote`, `parse_cell`, tests)
- Modify: `src/server.rs` (`group_view` match arm, temporarily rendering as text)
- Modify: `src/tunnel.rs` (the `Remote { .. }` literals in `is_our_tunnel` and the tests)

**Interfaces:**
- Produces:
  - `Cell::Herdr(HerdrCell)` with `pub struct HerdrCell { pub text: String, pub prefix: String }`
  - `Remote.herdr_socket: Option<String>`
  - `DashboardConfig::has_herdr_cells_on(&self, machine: &str) -> bool`, which checks the rows shown on that machine

- [ ] **Step 1: Write failing tests in `config.rs` tests**
  - `herdr_cell_parses`: `{"text":"jds","herdr":"h1"}` becomes `Cell::Herdr(HerdrCell{text:"jds",prefix:"h1"})`.
  - `herdr_cell_rejects_blank_or_unknown`: each of these errors, with a message containing `group <id> row <r> column <c>`:
    - text `" "`
    - herdr `""`
    - herdr `"h 1"`
    - an extra key `"x"`
  - `remote_herdr_socket_must_be_absolute`: `"herdr_socket":".config/herdr/herdr.sock"` errors, `"/home/a:b.sock"` errors, and `"/home/jds/.config/herdr/herdr.sock"` parses to `Some(..)`. When the field is omitted, the value is `None`.
  - `has_herdr_cells_on_respects_row_machines`: a herdr cell on a row with `machines:["lucy"]` gives true for `lucy` and false for `megan`.
- [ ] **Step 2:** Run `cargo test config::` and expect the new tests to FAIL to compile or fail.
- [ ] **Step 3: Implement**
  - In `parse_cell`, an object with a `text` key is the herdr form; one with `label`/`url` keeps the link path.
  - The error for any other object stays as today.
  - Validate `herdr_socket` in `into_remote`.
  - In `group_view`, `Cell::Herdr` renders as `{"type":"text","text":..}` for now.
- [ ] **Step 4:** Run `cargo test` and expect all to PASS.
- [ ] **Step 5:** Commit: `config: herdr workspace cell and remote herdr_socket`.

### Task 2: `src/herdr.rs`: socket client and matcher

**Files:**
- Create: `src/herdr.rs` (+ `pub mod herdr;` in `src/lib.rs`)

**Interfaces:**
- Produces:
  - `pub struct HerdrWorkspace { pub number: u64, pub label: String }`
  - `pub fn label_matches(label: &str, prefix: &str) -> bool`
  - `pub fn matching_labels(workspaces: &[HerdrWorkspace], prefix: &str) -> Vec<String>`, sorted by `number`
  - `pub fn parse_reply(line: &str) -> Option<Vec<HerdrWorkspace>>`
  - `pub fn list_workspaces(socket: &Path, budget: Duration) -> Option<Vec<HerdrWorkspace>>`
  - `pub fn local_socket(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf>`
  - `pub const QUERY_BUDGET: Duration = Duration::from_secs(1)`

- [ ] **Step 1: Write failing tests**
  - `prefix_boundaries`:
    - `label_matches` is true for (`h1`,`h1`), (`h1-herberth`,`h1`), (`H1-x`,`h1`), (`h1 x`,`h1`), (`h1_x`,`h1`) and (`h1.x`,`h1`).
    - It is false for (`h10-e2e-testing`,`h1`), (`h1x`,`h1`), (`xh1`,`h1`) and (`h`,`h1`).
  - `matching_orders_by_number`: workspaces `[(5,"h6-b"),(3,"h6"),(4,"h60")]` with prefix `h6` give `["h6","h6-b"]`.
  - `parse_reply_reads_real_shape`:
    - The fixture `{"id":"whatsup","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1","number":2,"label":"h1","focused":false,"pane_count":1,"agent_status":"unknown","future_field":1}]}}` parses to `[(2,"h1")]`.
    - An `{"id":"whatsup","error":{"code":"invalid_request","message":"x"}}` reply gives `None`.
    - `"not json"` gives `None`.
    - A reply with `result.type` other than `workspace_list` gives `None`.
  - `list_workspaces_round_trip`:
    - A `UnixListener` in a temp dir (keep the path short) reads one line.
    - The test asserts that line equals the spec's request line, writes the fixture reply plus `\n`, and expects `Some([(2,"h1")])`.
  - `list_workspaces_eof_is_fast_none`: the server accepts and drops the connection. The result is `None` in under 200 ms.
  - `list_workspaces_silent_server_times_out`: the server accepts and never writes. With a 300 ms budget, the result is `None` in under 600 ms.
  - `list_workspaces_missing_socket_is_none`.
  - `local_socket_prefers_absolute_xdg`:
    - `XDG_CONFIG_HOME=/x` gives `/x/herdr/herdr.sock`.
    - A relative `XDG_CONFIG_HOME` falls back to `$HOME/.config/herdr/herdr.sock`.
    - With no `HOME`, the result is `None`.
- [ ] **Step 2:** Run `cargo test herdr::` and expect FAIL.
- [ ] **Step 3: Implement**
  - Blocking `UnixStream::connect`, then set read and write timeouts to whatever remains of the budget.
  - Read until `\n` with `take(1 MiB)`.
  - Parse with serde structs that do not deny unknown fields.
- [ ] **Step 4:** Run `cargo test` and expect PASS.
- [ ] **Step 5:** Commit: `herdr: workspace.list socket client and name matcher`.

### Task 3: Tunnel forwards the remote herdr socket

**Files:**
- Modify: `src/tunnel.rs`
- Modify: `src/main.rs` (pass `runtime_dir()`)
- Modify: `src/server.rs` (tests construct `Tunnels::new`)

**Interfaces:**
- Consumes: `Remote.herdr_socket` (Task 1).
- Produces:
  - `Tunnels::new(program: PathBuf, socket_dir: PathBuf)`
  - `pub fn herdr_forward_path(socket_dir: &Path, remote: &Remote) -> Option<PathBuf>`: `None` without `herdr_socket` or when the path is 100 bytes or longer (logged with `eprintln!` once per tunnel start)
  - `pub fn ssh_args(remote: &Remote, forward: Option<&Path>) -> Vec<String>`
  - `Tunnels::herdr_socket(&self, name: &str) -> Option<PathBuf>`: `Some` only when state is `Running` and a forward is configured

- [ ] **Step 1: Write failing tests**
  - `ssh_args_with_forward`: with a forward, the args are today's list with `-o StreamLocalBindUnlink=yes -L <local>:<remote>` inserted before the host. Without one, they are byte-identical to today's (update `ssh_args_match_the_spec` to call `ssh_args(&lucy(), None)`).
  - `forward_path_needs_socket_and_short_dir`:
    - With `herdr_socket` set: `<dir>/herdr-47655.sock`.
    - Without it: `None`.
    - With a 120-char dir: `None`.
  - `recognizes_our_leftover_tunnel_with_forward`:
    - The command line is `ssh -N -D 127.0.0.1:47655 -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=5 -o ServerAliveCountMax=2 -o StreamLocalBindUnlink=yes -L /Users/jds/Library/Application Support/WhatsUp/herdr-47655.sock:/home/jds/.config/herdr/herdr.sock lucy`.
    - With port 47655 the result is true. With 47656 it is false.
    - The existing no-forward case is still true.
    - A command with an extra `-R x` is false.
  - `stopping_a_tunnel_removes_its_socket_file`:
    - Use the existing stub ssh script, and create the socket path file by hand.
    - `reconcile(&[])` removes it.
  - `herdr_socket_only_while_running`: with the stub that exits at once, the state ends in `Waiting`, so `herdr_socket` gives `None`. With `running_stub`, it gives `Some(path)`.
- [ ] **Step 2:** Run `cargo test tunnel::` and expect FAIL.
- [ ] **Step 3: Implement**
  - `supervise` computes the forward path once per start.
  - `reconcile` and `shutdown` delete `herdr_forward_path` for removed entries.
  - `is_our_tunnel` matches against the joined command string:
    - the program;
    - the base args;
    - then either `" " + host-word` at the end, or `" -o StreamLocalBindUnlink=yes -L "`, then anything containing `/herdr-<port>.sock:/`, then `" " + host-word` at the end.
- [ ] **Step 4:** Run `cargo test` and expect PASS.
- [ ] **Step 5:** Commit: `tunnel: forward the remote herdr socket`.

### Task 4: Dashboard sends matched labels

**Files:**
- Modify: `src/server.rs` (`Inner`, `AppState::new`, `dashboard`, `MachineStatus`, `group_view`, tests)
- Modify: `src/main.rs` (pass `herdr::local_socket(&env)`)

**Interfaces:**
- Consumes: Task 1 `Cell::Herdr` and `has_herdr_cells_on`; Task 2 `list_workspaces`, `matching_labels` and `QUERY_BUDGET`; Task 3 `Tunnels::herdr_socket`.
- Produces:
  - `AppState::new(.., tunnels, local_herdr_socket: Option<PathBuf>)`
  - `MachineStatus.herdr: Option<Vec<HerdrWorkspace>>`
  - cell JSON `{"type":"text","text":..,"herdr":[..]}`, where the `herdr` key appears only when the list is non-empty

- [ ] **Step 1: Write failing tests in `server.rs`**, using the existing `serve` helper extended with an optional herdr socket
  - `local_herdr_label_in_workspace_cell`:
    - A fake herdr socket replies with `h1-herberth` (#2) and `h10-e2e-testing` (#3).
    - The config row has `{"text":"jds","herdr":"h1"}`.
    - `/api/dashboard` gives that cell as `{"type":"text","text":"jds","herdr":["h1-herberth"]}`.
  - `no_herdr_socket_gives_plain_text`: `None` socket gives `{"type":"text","text":"jds"}` with no `herdr` key.
  - `herdr_down_does_not_delay_dashboard`: with a silent fake socket, the response arrives in under `QUERY_BUDGET + 500 ms`, and the cell is plain.
  - `remote_cells_plain_while_tunnel_not_running`: with a remote that has `herdr_socket` and the `true` stub (the tunnel is never Running), the remote's herdr cell is plain.
  - `config_without_herdr_unchanged`: the existing dashboard JSON assertions still pass untouched.
- [ ] **Step 2:** Run `cargo test server::` and expect FAIL.
- [ ] **Step 3: Implement**
  - Spawn the herdr queries (`spawn_blocking`) in the same "start everything, then await" block as the probes.
  - Query a machine only if `has_herdr_cells_on` is true and a socket path is known.
  - `group_view` calls `matching_labels`.
- [ ] **Step 4:** Run `cargo test` and expect PASS.
- [ ] **Step 5:** Commit: `dashboard: show matched herdr labels`.

### Task 5: Render, docs, lint

**Files:**
- Modify: `assets/dashboard.html` (`renderCell`, CSS)
- Modify: `README.md` (a "herdr labels" subsection under Config format)
- Modify: `config/whatsup.json` (only if it has a Workspace-like column; else leave it)

- [ ] **Step 1: Rendering**
  - `renderCell`: when `cell.herdr?.length`, render the text, then a `span.herdr` holding `/` + `herdr[0]` (secondary color, the existing muted token).
  - When there is more than one match, add ` +N` and set `td.title = "herdr: " + herdr.join(", ")`.
  - Keep `white-space: nowrap`. Long labels get an ellipsis with `max-width`, and the full label goes in the tooltip.
- [ ] **Step 2: README**: document the cell form, the matching rule, `herdr_socket`, and that WhatsUp sends only `workspace.list`.
- [ ] **Step 3:** Run `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` and expect all clean.
- [ ] **Step 4:** Commit: `dashboard: render herdr labels; document`.

### Task 6: E2E on megan and lucy, then rollout

- [ ] **Step 1: Run on megan**
  - Run a dev WhatsUp (`cargo run -- --config <copy of shared config with herdr fields> --port 47660 --no-open`).
  - Its config adds `herdr_socket` to lucy and herdr cells on the ten rows.
  - Use a separate `socks_port`, so the installed WhatsUp's tunnel is untouched.
- [ ] **Step 2: Check through claude-in-chrome** in the Personal profile, per the spec's checks 1-5.
  - Megan: `jds/h1`, `jds4/h4-apitb`.
  - Lucy: `jds/h1-herberth`, `jds10/h10-e2e-testing`.
  - Hover a `+N` cell, if one exists, and check the tooltip.
  - Kill the tunnel's ssh: the lucy cells go plain, then recover.
  - Be picky about alignment and wrapping in the Workspace column.
- [ ] **Step 3: Quit the dev WhatsUp**: no ssh is left, and no `herdr-*.sock` is left in the runtime dir.
- [ ] **Step 4: PR.** Use superpowers:finishing-a-development-branch. Ask before merging.
- [ ] **Step 5: Rollout, after the merge:**
  - Install on lucy (`packaging/install-linux.sh` over SSH) and on megan.
  - Only then change `jds_private_config/whatsup/config.json` (its own branch `jds-<issue>-...` and PR), merge it, and pull it on lucy.
- [ ] **Step 6:** Check the installed megan WhatsUp once more against spec checks 6-7.

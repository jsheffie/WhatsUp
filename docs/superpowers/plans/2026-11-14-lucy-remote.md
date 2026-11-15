# Lucy Remote Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** megan's WhatsUp shows lucy's stacks with live status and opens lucy's pages in a `megan:Lucy` Chrome profile that routes through lucy over an SSH SOCKS tunnel WhatsUp owns.

**Architecture:** Config gains `remotes` and `machine_names`. For each active remote WhatsUp supervises one `ssh -N -D` process (tokio), probes each remote link with a SOCKS5 CONNECT through it (blocking std sockets, like `probe.rs`), and renders every group once per machine. Remote links open via Chrome's `--profile-directory`; a tiny MV3 extension in that profile points it at the SOCKS port.

**Tech Stack:** Rust 2021, axum 0.8, tokio 1 (add the `process` feature), serde_json; vanilla JS in `assets/dashboard.html`; Chrome MV3 extension; bash/python in `jds_config`.

**Spec:** `docs/superpowers/specs/2026-11-14-lucy-remote-design.md`

## Global Constraints

- Never use the em dash character anywhere (code, docs, commits, UI copy). Use `-`.
- Commit messages: no agent co-author trailer.
- Default SOCKS port: `47655` (dashboard default is `47654`). The extension hardcodes the same value.
- ssh command, exactly: `ssh -N -D 127.0.0.1:<socks_port> -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=5 -o ServerAliveCountMax=2 <ssh>`.
- Restart backoff: 1s, doubling, capped at 30s; reset to 1s after a run that lasted 60s or more.
- Remote probe timeout: 1500 ms per stage (connect, greeting, reply). Local probe timeout stays 300 ms.
- SOCKS outcomes: success reply = up; socket closed without a reply, or a non-zero reply code = down; refused / timeout / bad greeting = offline (whole remote).
- Configs without `remotes` must behave and render exactly as today (no tags, no machine tabs).
- Branches: WhatsUp work on `jds-3-lucy-remote` (exists). `jds_config` and `jds_private_config` work each on a new `jds-<issue>-...` branch off `main`, with an issue first.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must pass at every commit.

## Review Focus

1. lucy powered off, or ssh auth failing: megan's own status must still refresh on time. Remote probes run concurrently with local probes and are bounded by the 1500 ms timeouts (Task 5 test `dashboard_is_not_slowed_by_a_dead_remote`).
2. Editing the config while WhatsUp runs (adding, removing or changing a remote) starts, stops or restarts tunnels without restarting WhatsUp (Task 3 test `reconcile_starts_and_stops_tunnels`).
3. The same URL configured on both machines: `/api/open` with `machine: "lucy"` must use the profile opener and without `machine` the local opener (Task 5 test `open_routes_by_machine`).
4. SOCKS port already taken (orphaned ssh from a crashed WhatsUp): no tight restart loop, the error text reaches the dashboard, and probes still work if the port is a working proxy (Task 3 test `backoff_grows_and_resets`; Task 5 surfaces `note`).
5. Config without `remotes`: dashboard JSON and page unchanged in shape for existing fields (Task 5 test `no_remotes_view_is_unchanged`, Task 6 manual check).

---

### Task 1: Config: `remotes`, `machine_names`, link hosts

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Produces:
  - `pub const DEFAULT_SOCKS_PORT: u16 = 47655;`
  - `pub struct Remote { pub name: String, pub ssh: String, pub browser_profile: String, pub socks_port: u16 }` (Debug, Clone, PartialEq, Eq)
  - `DashboardConfig` gains `pub remotes: Vec<Remote>` and `pub machine_names: BTreeMap<String, String>`
  - `impl DashboardConfig { pub fn machine_label(&self, hostname: &str) -> String; pub fn active_remotes(&self, local_label: &str) -> Vec<Remote>; pub fn remote(&self, name: &str) -> Option<&Remote> }`
  - `pub fn host_from_url(url: &str) -> Result<String, String>` (lowercased host, IPv6 without brackets)
  - `Link` gains `pub host: String`, filled from `host_from_url` at parse time.

- [ ] **Step 1: Write the failing tests** in `config.rs` tests:
  - `parses_remotes_with_defaults`: config with `"remotes": [{ "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy" }]` gives `socks_port == 47655`; with `"socks_port": 40000` gives 40000.
  - `rejects_bad_remotes`: duplicate `name`; empty `name`/`ssh`/`browser_profile`; `ssh` starting with `-` or containing whitespace; `socks_port` 0 or equal to `PREFERRED_PORT` (47654); unknown remote field. Each `parse_config(..).unwrap_err()`.
  - `machine_label_maps_hostnames`: `"machine_names": { "Jeffs-MacBook-Pro-2": "megan" }`; `machine_label("Jeffs-MacBook-Pro-2") == "megan"`, `machine_label("lucy") == "lucy"`.
  - `active_remotes_skip_the_local_machine`: with remote `lucy`, `active_remotes("lucy")` is empty and `active_remotes("megan")` has one (compare case-insensitively).
  - `host_from_url_cases`: `http://jds4.local.me:3004/` -> `jds4.local.me`; `http://LOCALHOST:4004` -> `localhost`; `http://[::1]:2023/admin/` -> `::1`; `https://u@h.example/x` -> `h.example`.
  - Extend `default_config_parses_example_stacks`: `config.remotes.is_empty()`, `frontend.host == "localhost"`.

- [ ] **Step 2: Run** `cargo test config::` - Expected: compile errors / FAIL.

- [ ] **Step 3: Implement** `RawRemote` (serde, `deny_unknown_fields`, `socks_port: Option<u16>`), optional `remotes` and `machine_names` on `RawConfig` (`#[serde(default)]`), validation in `into_dashboard`, and the functions above. Share the authority parsing between `host_from_url` and `port_from_url`.

- [ ] **Step 4: Run** `cargo test` - Expected: PASS (all old tests too).

- [ ] **Step 5: Commit** `git commit -am "config: add remotes, machine_names and link hosts"`

### Task 2: SOCKS5 probe

**Files:**
- Create: `src/socks.rs`; Modify: `src/lib.rs` (`pub mod socks;`)

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Reach { Up, Down, Offline }`
  - `pub const REMOTE_TIMEOUT: Duration = Duration::from_millis(1500);`
  - `pub fn socks_reach(socks_port: u16, host: &str, port: u16, timeout: Duration) -> Reach`
  - `pub fn probe_remote(socks_port: u16, targets: impl IntoIterator<Item = (String, u16)>) -> BTreeMap<(String, u16), Reach>` (dedups, one thread per target, `REMOTE_TIMEOUT`)

- [ ] **Step 1: Write the failing tests** with a fake SOCKS server on `127.0.0.1:0` in a test thread (reads greeting `[5,1,0]`, answers `[5,0]`, reads the CONNECT request, then acts per test):
  - `success_reply_is_up`: replies `[5,0,0,1,0,0,0,0,0,0]` -> `Reach::Up`; also assert the request it read was `[5,1,0,3,len,host..,port_hi,port_lo]` for `("jds4.local.me", 3004)`.
  - `close_without_reply_is_down`: drops the socket after the request -> `Reach::Down`.
  - `failure_reply_is_down`: replies `[5,5,0,1,0,0,0,0,0,0]` -> `Reach::Down`.
  - `refused_is_offline`: bind then drop a listener, probe its port -> `Reach::Offline`.
  - `silent_server_is_offline`: accepts and never answers; timeout 200 ms -> `Reach::Offline`, and the call returns in under 1 s.
  - `bad_greeting_is_offline`: answers `[5,0xFF]` -> `Reach::Offline`.

- [ ] **Step 2: Run** `cargo test socks::` - Expected: FAIL (module missing).

- [ ] **Step 3: Implement** with `TcpStream::connect_timeout` to `127.0.0.1:socks_port`, read/write timeouts = `timeout`, `read_exact` for the 2-byte greeting reply, then read the CONNECT reply's first 2 bytes: EOF -> Down, `reply[1] == 0` -> Up, other code -> Down, timeout/other IO error -> Offline. Hosts longer than 255 bytes -> Down.

- [ ] **Step 4: Run** `cargo test` - Expected: PASS.

- [ ] **Step 5: Commit** `git commit -am "socks: probe remote ports through a SOCKS5 tunnel"`

### Task 3: Tunnel supervisor

**Files:**
- Create: `src/tunnel.rs`; Modify: `src/lib.rs`, `Cargo.toml` (tokio feature `process`)

**Interfaces:**
- Consumes: `config::Remote`
- Produces:
  - `pub fn ssh_args(remote: &Remote) -> Vec<String>` (exactly the Global Constraints command, without the leading `ssh`)
  - `pub struct Backoff` with `pub fn new() -> Self`, `pub fn next_delay(&mut self, ran_for: Duration) -> Duration`
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum TunnelState { Starting, Running, Waiting { last_error: String } }`
  - `pub struct Tunnels` with `pub fn new(program: PathBuf) -> Self` (`"ssh"` in production; tests pass a stub script), `pub fn reconcile(&self, remotes: &[Remote])`, `pub fn state(&self, name: &str) -> Option<TunnelState>`, `pub fn shutdown(&self)`. Each tunnel is a tokio task owning a `tokio::process::Child` with `kill_on_drop(true)`, `stdin`/`stdout` null, stderr captured; the last non-empty stderr line becomes `last_error`. A changed `Remote` (any field) restarts its tunnel.

- [ ] **Step 1: Write the failing tests:**
  - `ssh_args_match_the_spec`: for `Remote{ name:"lucy", ssh:"lucy", socks_port:47655, .. }` equals `["-N","-D","127.0.0.1:47655","-o","BatchMode=yes","-o","ExitOnForwardFailure=yes","-o","ServerAliveInterval=5","-o","ServerAliveCountMax=2","lucy"]`.
  - `backoff_grows_and_resets`: short runs give 1s, 2s, 4s, 8s, 16s, 30s, 30s; then a 60s run gives 1s.
  - `reconcile_starts_and_stops_tunnels` (`#[tokio::test]`): stub program = a temp shell script that writes its pid to a file named after its last arg and sleeps; `reconcile([lucy])` -> within 2 s the pid file exists and `state("lucy") == Some(Running)`; `reconcile([])` -> the process is gone (`kill -0` fails) within 2 s and `state("lucy") == None`.
  - `failing_tunnel_reports_its_error`: stub prints `bind [127.0.0.1]:47655: Address already in use` to stderr and exits 255 -> `state` becomes `Waiting { last_error }` containing `Address already in use`.

- [ ] **Step 2: Run** `cargo test tunnel::` - Expected: FAIL.

- [ ] **Step 3: Implement** as specified; `Tunnels` holds `Mutex<BTreeMap<String, (Remote, JoinHandle<()>, Arc<Mutex<TunnelState>>)>>`; `reconcile` must be callable from sync code inside the runtime (use `tokio::runtime::Handle::current().spawn`).

- [ ] **Step 4: Run** `cargo test` - Expected: PASS, no leftover stub processes (`pgrep -f whatsup-stub` empty).

- [ ] **Step 5: Commit** `git commit -am "tunnel: supervise one ssh SOCKS tunnel per remote"`

### Task 4: Open a URL in a named Chrome profile

**Files:**
- Modify: `src/browser.rs`

**Interfaces:**
- Produces:
  - `pub fn profile_dir_for_name(local_state_json: &str, name: &str) -> Option<String>` (matches `profile.info_cache.<dir>.name`, exact, case-sensitive)
  - `pub fn chrome_local_state_path() -> Option<PathBuf>` (macOS `~/Library/Application Support/Google/Chrome/Local State`, else `~/.config/google-chrome/Local State`)
  - `pub fn profile_launch(app: &str, profile_dir: &str, url: &str) -> Launch` (macOS: program `open`, args `["-na", app, "--args", "--profile-directory=<dir>", url]`; Linux: first `browser_commands(app)` hit, args `["--profile-directory=<dir>", url]`)
  - `pub fn open_in_profile(app: &str, profile_name: &str, url: &str) -> Result<(), String>` (validates all three with `validate_text`; error `Chrome profile "<name>" not found` when the lookup fails)

- [ ] **Step 1: Write the failing tests:**
  - `finds_profile_dir_by_display_name`: JSON `{"profile":{"info_cache":{"Default":{"name":"Jeff"},"Profile 7":{"name":"megan:Lucy"}}}}` -> `Some("Profile 7")`; `"megan:lucy"` -> `None`; invalid JSON -> `None`.
  - `profile_launch_on_macos` (`#[cfg(target_os = "macos")]`): args equal `["-na","Google Chrome","--args","--profile-directory=Profile 7","http://jds4.local.me:3004"]`.
  - `rejects_profile_names_that_could_become_flags`: `open_in_profile("Google Chrome", "--x", "http://a:1")` is `Err`.

- [ ] **Step 2: Run** `cargo test browser::` - Expected: FAIL.
- [ ] **Step 3: Implement**; spawn detached exactly like `spawn_detached` (make it available on macOS too).
- [ ] **Step 4: Run** `cargo test` - Expected: PASS.
- [ ] **Step 5: Commit** `git commit -am "browser: open links in a named Chrome profile"`

### Task 5: Server and main wiring

**Files:**
- Modify: `src/server.rs`, `src/main.rs`

**Interfaces:**
- Consumes: Tasks 1-4.
- Produces (dashboard JSON, additive):
  - top level `"machine": "<local label>"` and `"machines": [{ "name", "local": bool, "offline": bool, "note": string|null, "ports_up", "ports_total" }]` (local first). `ports_up`/`ports_total` at top level stay local-only.
  - each group object gains `"machine": "<name>"` and `"key": "<machine>/<group id>"`; remote groups follow all local groups, in `remotes` order. Remote link `up` comes from `Reach::Up` for its `(host, port)`; when the remote is offline every remote link is `up: false`.
  - `note`: `null` when fine; `"<name> offline"` plus `": <last_error>"` when the tunnel is `Waiting`.
  - `/api/open` body `{ "url", "machine"? }`: missing or equal to the local label -> current behavior; a remote name -> `opener.open_in_profile(browser_app, remote.browser_profile, url)` after `allows_url`; unknown machine -> 400 `{"error":"unknown machine <name>"}`.
  - `BrowserOpener` gains `fn open_in_profile(&self, app: &str, profile: &str, url: &str) -> Result<(), String>`.
  - `AppState::new` gains `local_label: String` and `tunnels: Arc<Tunnels>`; `reload()` calls `tunnels.reconcile(&config.active_remotes(&local_label))` whenever the config is (re)loaded, including at startup.
  - `main.rs`: `local_label = config.machine_label(&short_hostname())` where `short_hostname()` runs `hostname -s` (fallback `"local"`); `tunnels.shutdown()` after the server stops.

- [ ] **Step 1: Write the failing tests** (extend the existing server test harness; inject a fake `Tunnels` program and a fake SOCKS server from Task 2's test helper, moved to a shared `#[cfg(test)]` helper):
  - `no_remotes_view_is_unchanged`: existing config -> `machines` has one local entry, groups have `machine == local`, all pre-existing keys present with the same values.
  - `remote_groups_follow_local_groups`: config with remote `lucy` (SOCKS port = fake server answering Up for `localhost:3000` only) -> group keys `["megan/web-stacks", ..., "lucy/web-stacks", ...]`, the lucy `frontend` link `up == true`, lucy `admin` link `up == false`.
  - `dashboard_is_not_slowed_by_a_dead_remote`: remote whose SOCKS port is silent -> `/api/dashboard` answers in under 2 s with that machine `offline: true`.
  - `open_routes_by_machine`: same URL; no `machine` -> `open` recorded; `machine: "lucy"` -> `open_in_profile` recorded with `megan:Lucy`; `machine: "nope"` -> 400.

- [ ] **Step 2: Run** `cargo test server::` - Expected: FAIL.
- [ ] **Step 3: Implement.** Run the local `probe_ports` and every remote's `probe_remote` concurrently (`tokio::join!` over `spawn_blocking`).
- [ ] **Step 4: Run** `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` - Expected: clean, PASS.
- [ ] **Step 5: Commit** `git commit -am "server: show remote machines and open their links in a profile"`

### Task 6: Dashboard UI (layout A)

**Files:**
- Modify: `assets/dashboard.html`

**Interfaces:**
- Consumes: Task 5 JSON.

Behavior (copy the approved mockup `.superpowers/brainstorm/*/content/lucy-layout.html`, option A, in the page's existing style):
- When `machines.length > 1`: tabs are `All`, one per machine (`data-filter="machine:<name>"`), then one per group id (`data-filter="group:<id>"`, shows that group for every machine). Session-stored tab key stays `whatsup-tab`; unknown stored values fall back to `All`.
- Each section heading shows the group label plus a machine tag; the local tag is neutral gray, remote tags purple (`#d2a8ff` text, `#6e40c9` border in dark; pick light-mode equivalents with AA contrast in the existing token style).
- Offline remote: its links render disabled and gray (not red), and the heading shows the `note` text in muted color.
- Remote link click posts `{ url, machine }`.
- Summary line: `"<up> of <total> ports listening"` for local, then ` · lucy <up> of <total>` per remote (or ` · lucy offline`).
- Footer: append `" Remote machines are checked through their SSH tunnel."` when there are remotes.
- When `machines.length == 1`: page is identical to today.

- [ ] **Step 1: Manual check** with `cargo run -- --config <scratch config with a lucy remote> --no-open`, then open `http://127.0.0.1:<port>/` via claude-in-chrome (Personal profile). Screenshot: All, megan, lucy and one group tab; lucy offline (stop the tunnel with `pkill -f "ssh -N -D 127.0.0.1:47655"` and confirm it recovers); light and dark mode; window at 900 px wide. Fix anything off.
- [ ] **Step 2: Manual check** without remotes (`cargo run`): looks exactly like before.
- [ ] **Step 3: Commit** `git commit -am "dashboard: render remote machines as their own sections"`

### Task 7: Proxy extension and docs

**Files:**
- Create: `extension/remote-proxy/manifest.json`, `extension/remote-proxy/background.js`
- Modify: `README.md`

**Interfaces:**
- Produces: MV3 extension, `"name": "WhatsUp remote proxy"`, `"permissions": ["proxy"]`, service worker that on `runtime.onInstalled` and `runtime.onStartup` calls `chrome.proxy.settings.set({ value: { mode: "fixed_servers", rules: { singleProxy: { scheme: "socks5", host: "127.0.0.1", port: 47655 }, bypassList: ["<-loopback>"] } }, scope: "regular" })`.

- [ ] **Step 1: Write** the two files.
- [ ] **Step 2: README** section "Remote machines": config fields (`remotes`, `machine_names`, defaults), the self-skip rule, how status is checked, creating the profile, loading the extension unpacked (chrome://extensions, Developer mode, Load unpacked, `extension/remote-proxy`), and that the whole profile goes through the remote.
- [ ] **Step 3: Commit** `git commit -am "Add the remote proxy extension and document remote machines"`

### Task 8: Build, install on megan and lucy, PR

- [ ] **Step 1:** `packaging/build-app.sh`, then replace `~/Applications/WhatsUp.app` with `dist/WhatsUp.app` (stop the running WhatsUp first via its Quit API: `curl -X POST http://127.0.0.1:47654/api/quit`), relaunch. Expected: dashboard unchanged (no remotes in the real config yet).
- [ ] **Step 2:** `git push -u origin jds-3-lucy-remote`.
- [ ] **Step 3:** On lucy over ssh: `git clone https://github.com/jsheffie/WhatsUp.git ~/workspace/whatsup && cd ~/workspace/whatsup && git switch jds-3-lucy-remote && packaging/install-linux.sh`. Restart lucy's WhatsUp if it is running. Expected: `~/.local/bin/whatsup --help` works; with `WHATSUP_CONFIG` pointing at a scratch copy of the config containing `remotes`, `whatsup --no-open` starts, and no ssh process is spawned (self-skip).
- [ ] **Step 4:** Open the PR (`Closes #3`), but merge only after Task 12 passes.

### Task 9: User steps (hand to the user, then wait)

Ask the user to do these and say when done:
- [ ] Create Chrome profile named exactly `megan:Lucy`, not signed into Chrome.
- [ ] In that profile: chrome://extensions -> Developer mode -> Load unpacked -> `~/workspace/ac-slash-pc/whatsup/extension/remote-proxy`.
- [ ] In that profile: install the Claude extension and sign it in.
- [ ] On lucy, run the hosts fix (prepare it exactly; it must be idempotent):
  `sudo sed -i.bak -E '/^127\.0\.0\.1 (jds[0-9]*\.local\.me)$/{p;s//::1 \1/}' /etc/hosts` and show `grep local.me /etc/hosts` afterwards. Verify the sed on a copy first and give the user the tested version.

### Task 10: `jds_config` changes

**Files:** `~/workspace/jds_config/bin/claude-chrome-browser`, `doc/claude-in-chrome.md`, `skhd/skhdrc-app-launchers`, `skhd/skhdrc` (generated), and the AGENTS.md source under `dotfiles/` that lists Chrome profiles.

- [ ] **Step 1:** `gh issue create` in `jsheffie/jds_config`; branch `jds-<n>-lucy-chrome-profile` off `main` (the checkout is currently on `jds-14-claude-attribution`; stash nothing that is not ours, just switch if clean, otherwise ask).
- [ ] **Step 2:** `claude-chrome-browser`: a profile whose `info_cache` name ends with `:Lucy` (case-insensitive) and has no signed-in account gets role `lucy`; table sorts work, personal, lucy, other. Hook output adds a line when a lucy profile with the extension exists: `- The LUCY profile (megan:Lucy) routes through lucy; deviceId <id>. Use it only to test pages served by lucy.` Verify: `claude-chrome-browser` prints the lucy row with a deviceId; `echo '{"cwd":"'$PWD'"}' | claude-chrome-browser hook` includes the line.
- [ ] **Step 3:** `make install` in `jds_config/claude/`. Expected: "Linked ..." and hook already present.
- [ ] **Step 4:** skhd: append `cmd + ctrl - z : "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --profile-directory="<Profile N of megan:Lucy>" --new-window` to `skhdrc-app-launchers` next to the x/c/v lines. `skhdrc` currently matches none of the `build-*` outputs exactly: diff it against each to find the layout in use and any drift, fold drift back into its source file, then run that `make build-*` and `make reload`. Expected: `grep "ctrl - z" skhd/skhdrc` hits; pressing the key opens a `megan:Lucy` window.
- [ ] **Step 5:** Update `doc/claude-in-chrome.md` and the AGENTS.md profile list (add `megan:Lucy`: unsigned, proxied through lucy, for testing lucy pages).
- [ ] **Step 6:** Commit, push, PR, merge after the user checks, switch to `main`, pull.

### Task 11: Turn it on in the shared config

- [ ] **Step 1:** Issue + branch in `jds_private_config`; add to `whatsup/config.json`: `"machine_names": { "Jeffs-MacBook-Pro-2": "megan" }` and `"remotes": [{ "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy" }]`.
- [ ] **Step 2:** megan's WhatsUp hot-reloads it: lucy sections appear, `pgrep -fl "ssh -N -D 127.0.0.1:47655"` shows the tunnel.
- [ ] **Step 3:** Commit, PR, merge, pull on megan; then on lucy `git -C ~/workspace/jds_private_config pull` and confirm lucy's dashboard shows only lucy, with no tunnel.

### Task 12: End-to-end checks (spec "How we will know it works")

Select the `megan:Lucy` browser via `claude-chrome-browser` deviceId; the user handles login prompts.
- [ ] lucy stacks up show green in lucy sections; megan's own status unchanged.
- [ ] Stop one lucy server (ask the user, or use a stack they name): red within two refreshes; start: green.
- [ ] Click a lucy AssetCORE link, a tt-calc-app `localhost:40xx` link and a Django admin link: each opens in `megan:Lucy`, loads, and the app reaches its API (login works).
- [ ] Same `jdsN` on both machines: each machine's link opens its own app (check a page detail that differs, e.g. data or git branch banner).
- [ ] `pkill -f "ssh -N -D 127.0.0.1:47655"`: lucy shows offline, recovers by itself within 30 s. Sleep megan for a minute: recovers after wake.
- [ ] Claude extension in `megan:Lucy` still connects through the proxy. If not, add a `bypassList` entry for whatever loopback endpoint it uses and re-test.
- [ ] Quit WhatsUp: no ssh process left.
- [ ] Then merge the WhatsUp PR, switch to `main`, pull; reinstall on megan and lucy from `main`.

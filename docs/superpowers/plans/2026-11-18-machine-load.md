# Machine Load (CPU, RAM, Disk) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The dashboard header shows CPU, RAM and disk use for megan and
lucy, colored when they get high, so the user can see at a glance when to
shut down a stack. `whatsup list` shows the same numbers.

**Architecture:** Every WhatsUp samples its own machine on a background
thread every 2 s and serves the latest sample at `GET /api/load`. megan's
`/api/dashboard` adds its own sample and fetches each remote's
`/api/load` through the existing SSH SOCKS tunnel, in parallel with the
remote probes. The server attaches a level (`ok`, `warn`, `high`) to each
number, so the dashboard and the CLI agree on what "high" means.

**Tech Stack:** Rust (axum 0.8, tokio, serde, libc), vanilla JS in
`assets/dashboard.html`. No new crates.

**Spec:** none. The user skipped the design conversation; the decisions
below are the design, and the ones marked "my call" are open to change.

## Design

- **Why:** the user shuts down extra stacks when RAM gets tight or CPU runs
  high, and today has to check each machine by hand.
- **Where it shows:** a load line per machine in the header, under the
  summary, visible on every tab. Single-machine dashboards show one line
  with no machine tag.
- **CPU:** percent busy across all cores, averaged over the last 10 s
  (5 intervals of 2 s), so a short spike does not flash red. `null` until
  two samples exist.
- **RAM:** percent *in use*, meaning not available, the number that tracks
  pressure. It is not "free":
  - Linux: `100 * (MemTotal - MemAvailable) / MemTotal`. On lucy today,
    MemFree is 3.5 GB while MemAvailable is 13.6 GB.
  - macOS: `100 - kern.memorystatus_level`. This is the kernel's free
    percentage, the same number `memory_pressure` prints (50 on megan
    today). The Activity Monitor style "used" sum reads 85% on the same
    machine because of compression, and would sit amber all day.
  - Swap in use is shown in the tooltip.
- **Disk:** the filesystem holding `$HOME` (`/` if `HOME` is unset), as
  `100 * (total - available) / total` from `statvfs` (`f_bavail`).
  - On megan this is the APFS container (88% today). `statvfs` reports
    container totals, so `/` gives the same number; df's 27% for `/` is
    that volume's own "Used" only. (Corrected after review.)
  - On lucy, `/`, `/home` and `/var/lib/docker` are one filesystem.
- **Levels (my call):**

  | Metric | warn at | high at |
  |---|---|---|
  | CPU | 75 | 90 |
  | RAM | 80 | 90 |
  | Disk | 85 | 95 |

  Hard-coded constants in `src/load.rs`, not config.
- **Sizes (my call):** 1024-based, written `GB` with no decimals at 10 GB
  and over, one decimal below that (`36 GB`, `468 MB`, `1.5 GB`).
- **Remote missing load:**
  - An older WhatsUp on lucy (404 on `/api/load`) gives the note
    `update WhatsUp on lucy to see its load`.
  - lucy offline or its WhatsUp down gives no load and no extra note,
    because the existing offline and can_act messages already say so.
  - Never a banner.
- **Non-goals:**
  - History or graphs.
  - Per-stack or per-container numbers.
  - Configurable thresholds.
  - Docker Desktop's VM on megan. It is capped at 8 GB RAM and 12 CPUs
    (`docker info`) and can fill before host RAM looks tight; that would
    be a follow-up.

## Global Constraints

- Branch `jds-11-machine-load` off `origin/main` (#9 is merged there;
  the current checkout `jds-9-row-start-and-cli` is stale). Commit messages
  end with `(#11)`; no co-author lines; never the em dash in code, docs
  or copy.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
  stay clean after every task **on megan**. Tasks 1, 2 and 6 also run them
  on lucy, because `cfg(target_os = "linux")` code never compiles on megan.
  On lucy: `~/workspace/whatsup`, `PATH=$HOME/.cargo/bin:$PATH`. Push code
  with `git push ssh://lucy/home/jds/workspace/whatsup <branch>:refs/heads/tmp-load`
  then `git merge --ff-only tmp-load` there.
- No new crates. `libc::mach_host_self` is `#[deprecated]` in libc 0.2.191:
  call it once, cache the port in a `OnceLock`, and put
  `#[allow(deprecated)]` with a one-line reason on that call only.
- Timing: sample every 2 s; CPU window 5 intervals; remote `/api/load` read
  timeout 1 s (the SOCKS connect keeps its existing `REMOTE_TIMEOUT`).
- A slow or absent remote adds at most `LOAD_READ_TIMEOUT` (1 s) after the
  SOCKS connect (`REMOTE_TIMEOUT`, 1.5 s, the same bound the remote probes
  already have) to `/api/dashboard`, and never blocks the local probe.
- Percents are `f64`, so JSON shows `88.4`, not `88.4000015258789`.
- `statvfs` block fields are 32-bit on macOS and 64-bit on Linux. Widen
  with `u64::from` or `as u64`, and put any `#[allow]` that one platform's
  clippy needs on that conversion alone, with a reason.
- No `NaN` or infinity ever reaches JSON. A zero or negative delta, or a
  zero total, gives `null`.

## Review Focus

1. Disk on megan must be the data volume: the number matches `df -k ~`
   (88%), not `df -k /` (27%). Checked in Task 1 Step 4 and Task 6.
2. A remote WhatsUp that accepts the connection but never replies must not
   hold up `/api/dashboard` past about 2 s (1 s read timeout plus slack).
   Test in Task 3.
3. An older lucy WhatsUp (404) shows the update note and no banner; a dead
   one shows nothing extra. Tests in Task 3.
4. Load changing every 2 s must not rebuild the tables. `viewKey` hashes
   all of `data`, so load must be left out of it, or open Start/Stop
   watches and keyboard focus get disturbed each tick. Checked in the
   browser in Task 4.
5. The first sample, and a counter that wraps or resets (macOS ticks are
   `u32` and wrap after about 41 days at 12 cores), give CPU `null`, never
   0%, a negative number or a huge one. Tests in Task 1.

---

### Task 1: Measure this machine (`src/load.rs`)

**Files:**
- Create: `src/load.rs` (add `pub mod load;` to `src/lib.rs`)

**Interfaces:**
- Produces (all `Clone, Debug, PartialEq, Serialize, Deserialize`; fields `pub`):
  - `struct Load { cpu: Option<Cpu>, memory: Option<Memory>, disk: Option<Disk> }`, plus `Default`
  - `struct Cpu { percent: f64, cores: u32 }`
  - `struct Memory { percent: f64, total: u64, swap_used: u64 }` (bytes)
  - `struct Disk { percent: f64, total: u64, free: u64, path: String }` (bytes)
  - `struct Ticks { busy: u64, all: u64 }`
  - `fn cpu_percent(earlier: Ticks, later: Ticks) -> Option<f64>`: `checked_sub` on both fields; `None` if either underflows or `all` delta is 0
  - `fn parse_proc_stat(text: &str) -> Option<Ticks>`:
    - reads the first line that starts with `cpu `;
    - `all` is the sum of the first 8 fields (user, nice, system, idle, iowait, irq, softirq, steal);
    - `busy` is `all - idle - iowait`.
  - `fn parse_meminfo(text: &str) -> Option<Memory>`: from `MemTotal`, `MemAvailable`, `SwapTotal` and `SwapFree` in kB. `None` if any is missing or the total is 0.
  - `fn memory_from_pressure(total: u64, free_level: u32, swap_used: u64) -> Memory`: percent = `100 - min(free_level, 100)`
  - `fn disk_from_statvfs(blocks: u64, available: u64, fragment: u64, path: &str) -> Option<Disk>`: `None` when the total is 0
  - `fn read_ticks() -> Result<Ticks, String>`, `fn read_memory() -> Result<Memory, String>`, `fn read_disk(path: &Path) -> Result<Disk, String>`. These are the cfg-gated glue:
    - Linux: `/proc/stat` and `/proc/meminfo`.
    - macOS: `host_statistics(HOST_CPU_LOAD_INFO)`, using `cpu_ticks` user, system, nice as busy and idle as idle, each `u32` widened to `u64`. Also `sysctlbyname` for `hw.memsize`, `kern.memorystatus_level` and `vm.swapusage` (`xsw_usage.xsu_used`).
    - Both: `libc::statvfs`.
  - `enum Level { Ok, Warn, High }` with `as_str()` returning `ok`, `warn` or `high`; `fn level(percent: f64, warn: f64, high: f64) -> Level`; constants `CPU_WARN/CPU_HIGH = 75/90`, `MEMORY_WARN/MEMORY_HIGH = 80/90`, `DISK_WARN/DISK_HIGH = 85/95`
  - `fn view(load: &Load) -> serde_json::Value`: each part as its fields plus `"level"`; a missing part is `null`; percents rounded to one decimal

- [ ] **Step 1: Write failing tests** in `load.rs`:
  - `proc_stat_counts_iowait_as_idle`: `"cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 ..."` gives `Ticks { busy: 150, all: 1000 }`.
  - `cpu_percent_over_a_window`: `(150,1000)` to `(450,2000)` gives `Some(30.0)`.
  - `cpu_percent_is_none_without_progress_or_on_wrap`: equal ticks gives `None`. `later.all < earlier.all` gives `None`. `later.busy < earlier.busy` gives `None`.
  - `meminfo_uses_available_not_free`: MemTotal 32110128, MemFree 3543964, MemAvailable 13645448, SwapTotal 2097148, SwapFree 1048574. Expect percent ≈ 57.5 (±0.1), total `32110128*1024`, swap_used `1048574*1024`. Missing `MemAvailable` gives `None`.
  - `pressure_level_is_free_percent`: `memory_from_pressure(36<<30, 50, 0).percent == 50.0`, and a level of 150 gives 0.0.
  - `disk_uses_available_blocks`: blocks 1000, available 120, fragment 4096 gives percent 88.0, total 4_096_000, free 491_520. Blocks 0 gives `None`.
  - `levels_at_their_edges`: `level(74.9, 75, 90) == Ok`, `level(75.0, ..) == Warn`, `level(90.0, ..) == High`.
  - `view_attaches_levels_and_nulls`: a `Load` with only `disk` (88%) views as `{"cpu":null,"memory":null,"disk":{..,"level":"warn"}}`.
  - `reads_this_machine` (live, any platform): `read_ticks` twice 200 ms apart gives a `cpu_percent` of `None` or within 0..=100. `read_memory` gives a total > 1 GB and a percent within 0..=100. `read_disk(home)` gives a total > 0 and free ≤ total.
- [ ] **Step 2: Run** `cargo test --lib load::`. Expect a compile failure.
- [ ] **Step 3: Implement** to the interfaces above.
- [ ] **Step 4: Check against the OS.** On megan, `cargo test --lib load:: -- --nocapture` with a temporary `eprintln!` in `reads_this_machine`, removed before commit. Disk percent within 1 of `df -k ~` capacity; memory percent within 3 of `100 - $(sysctl -n kern.memorystatus_level)`. On lucy, the same test passes and the numbers agree with `df -k ~` and `free -b`.
- [ ] **Step 5: Run** fmt, clippy and test on megan and on lucy.
- [ ] **Step 6: Commit** `load: read cpu, memory and disk use (#11)`.

### Task 2: Sample in the background and serve `/api/load`

**Files:**
- Modify: `src/load.rs` (monitor), `src/server.rs` (route, `Inner.load`, local load in the view), `src/main.rs` (start the monitor)

**Interfaces:**
- Consumes: Task 1's `Load`, `read_*`, `cpu_percent`, `view`.
- Produces:
  - `struct CpuWindow` with `fn new(intervals: usize) -> Self` and `fn push(&mut self, ticks: Ticks) -> Option<f64>`. It keeps at most `intervals + 1` ticks and returns `cpu_percent(oldest, newest)`.
  - `pub const SAMPLE_EVERY: Duration = 2 s`, `pub const CPU_WINDOW: usize = 5`
  - `pub struct LoadMonitor` with `pub fn start(disk_path: PathBuf) -> Arc<LoadMonitor>` (a named std thread, `whatsup-load`, sampling immediately and then every `SAMPLE_EVERY`) and `pub fn latest(&self) -> Load`. A failed read leaves that part `None` for this sample.
  - `server.rs`: `pub type LoadReader = Arc<dyn Fn() -> Load + Send + Sync>`; `Inner.load: LoadReader`, defaulting to `Load::default()`; `AppState::with_load(self, reader: LoadReader) -> Self` (before sharing, like `with_probe`); route `GET /api/load` returning `Json(reader())`
  - `MachineStatus` gains `load: Option<Load>` and `load_note: Option<String>`. `view` adds `"load"` (`load::view`, or `null`) and `"load_note"` to each entry of `machines`. The local machine's `load` is always `Some(reader())`.

- [ ] **Step 1: Write failing tests:**
  - `load.rs` `cpu_window_averages_over_the_window`: the first push gives `None`; pushes of all = 0,100,…,600 with busy = all/2 give 50.0, and after 7 pushes the oldest kept is all = 100.
  - `server.rs` `local_load_is_in_the_view`: `.with_load(Arc::new(|| fixture))` (disk 88%). `/api/dashboard` `machines[0].load.disk.level == "warn"`, `machines[0].load_note == null`, and `/api/load` returns the fixture as `Load` JSON (round-trips through `serde_json::from_str::<Load>`).
  - `no_remotes_view_is_unchanged` keeps passing, updated only for the two new machine keys.
- [ ] **Step 2: Run** `cargo test` and expect failure.
- [ ] **Step 3: Implement.** In `main.rs`, `LoadMonitor::start(home_or_root)` before `AppState::new`, then `.with_load(Arc::new(move || monitor.latest()))`.
- [ ] **Step 4: Run** fmt, clippy and test on megan and on lucy.
- [ ] **Step 5: Smoke.** Run a dev instance beside the installed app (see the deploy memory: scratch `HOME`, `--port 47660`, a config without remotes). `curl -s 127.0.0.1:47660/api/load` shows CPU `null` at first and a number after 2 s.
- [ ] **Step 6: Commit** `server: sample this machine's load and serve /api/load (#11)`.

### Task 3: Fetch the remote's load through the tunnel

**Files:**
- Modify: `src/socks.rs` (`get_json`), `src/server.rs` (`dashboard`)

**Interfaces:**
- Consumes: `socks_connect`, `http::exchange`, `PostError`, Task 2's `MachineStatus.load`/`load_note`.
- Produces:
  - `pub fn get_json(socks_port: u16, port: u16, path: &str, read_timeout: Duration) -> Result<(u16, String), PostError>`, mirroring `post_json` with method `GET` and no body.
  - `const LOAD_READ_TIMEOUT: Duration = 1 s` in `server.rs`.
  - `fn remote_load(name: &str, reply: Result<(u16, String), PostError>) -> (Option<Load>, Option<String>)`:
    - 200 with a body that parses as `Load` gives `(Some, None)`;
    - 404 gives `(None, Some("update WhatsUp on <name> to see its load"))`;
    - anything else gives `(None, None)`.

- [ ] **Step 1: Write failing tests** in `server.rs`, following the existing fake-remote tests (lines ~1061, ~1646):
  - `remote_load_is_shown`: the fake remote WhatsUp answers `GET /api/load` with a fixture. `machines[1].load.memory.percent` matches, and `load_note` is null.
  - `older_remote_gets_an_update_note`: the fake answers 404. `machines[1].load == null`, `load_note == "update WhatsUp on lucy to see its load"`, and `config_error` and the machine `note` are unchanged.
  - `dead_remote_whatsup_adds_nothing`: nothing listens on `whatsup_port`. `load == null` and `load_note == null`.
  - `silent_remote_does_not_hold_up_the_dashboard`: the fake accepts and never replies. `/api/dashboard` returns in under 2 s and the local `machines[0].load` is present.
  - Unit tests of `remote_load` for 200 with a body that is not JSON (`(None, None)`) and for 500.
- [ ] **Step 2: Run** `cargo test` and expect failure.
- [ ] **Step 3: Implement.** In `dashboard()`, spawn one `spawn_blocking(get_json(...))` per remote next to `remote_probes`, before awaiting anything, and await it in the per-remote loop.
- [ ] **Step 4: Run** fmt, clippy and test.
- [ ] **Step 5: Commit** `server: show each remote's load from its own WhatsUp (#11)`.

### Task 4: Dashboard load line

**Files:**
- Modify: `assets/dashboard.html`, `README.md` (a short "Machine load" section: what each number means and the thresholds)

**Interfaces:**
- Consumes: `data.machines[i].load` and `load_note` (Task 2 and 3 shapes).

- [ ] **Step 1: Markup and style.**
  - Add `<div class="load" id="load"></div>` under `#summary`.
  - Add one `.load-line` per machine:
    - a machine `.tag` (`.tag.remote` for remotes), omitted when there is one machine;
    - then `CPU 23%`, `RAM 50%` and `Disk 88%`, each followed by a 36x4 px meter;
    - the percent text in `--muted`, `--warn` (new token, `#e3b341`) or `--down`, by level;
    - a meter with the same color as its fill, on a `--line` track.
  - Missing part: `CPU -`, with no meter.
  - Load `null`: the `load_note` in `.note`, else nothing after the tag. For an offline machine, `offline`.
  - Tooltips (`title`):
    - CPU: `10 s average across 12 cores`;
    - RAM: `50% in use of 36 GB · swap 468 MB used`, with the swap part only when nonzero;
    - Disk: `/Users/jds · 53 GB free of 460 GB`.
  - Sizes as in the Design section.
  - Lines wrap cleanly at 375 px width, with no horizontal scroll.
- [ ] **Step 2: Paint every tick.** Add `paintLoad(data)` and call it from `render()` next to `paintSummary`, before the `viewKey` early return. In `viewKey`, leave load out without touching `data` (the copy is shallow, so deleting from `copy.machines[i]` would strip `lastData`):
  `copy.machines = (data.machines || []).map(({ load, load_note, ...rest }) => rest);`
- [ ] **Step 3: Check in the browser.** Run a dev instance with a scratch config that keeps the lucy remote but sets `socks_port` to 47666. That avoids the installed app's tunnel on 47655; check that the herdr forward socket also lands under the scratch dirs. lucy still runs the old build, so this shows two machine lines, the remote tag and the `update WhatsUp on lucy...` note (Review Focus 3). Use the Personal profile.
  - The line shows and updates every 2 s.
  - Tag a table element (`document.querySelector("table").dataset.probe = 1`) and confirm it survives 3 ticks. The tables are not rebuilt.
  - Screenshots at desktop width and at 375 px. Fix anything that looks off.
- [ ] **Step 4: Commit** `dashboard: cpu, ram and disk line per machine (#11)`.

### Task 5: `whatsup list` shows load

**Files:**
- Modify: `src/cli.rs` (`format_list`)

- [ ] **Step 1: Write the failing test** `list_shows_load`. The machine header becomes `megan 3/12 up  cpu 23%  ram 50%  disk 88% (warn)`, with `(warn)` or `(high)` added only when the level is not `ok` and whole percents. A `null` part is left out. With `load == null` and a `load_note`, the note follows in parentheses, after any existing `(offline)` text.
- [ ] **Step 2: Run** `cargo test --lib cli::` and expect failure.
- [ ] **Step 3: Implement.** `--json` already passes `machines` through unchanged.
- [ ] **Step 4: Run** fmt, clippy and test.
- [ ] **Step 5: Commit** `cli: list shows each machine's load (#11)`.

### Task 6: End to end on megan and lucy

- [ ] **Step 1: Before lucy's update.** Deploy to megan only (build-app, quit, rsync, open, per the deploy memory). The lucy line reads `update WhatsUp on lucy to see its load`, with no banner. This is Review Focus 3 for real.
- [ ] **Step 2: Deploy to lucy.** Push, then fmt, clippy and test there, `packaging/install-linux.sh`, quit, `systemd-run` start. Within 2 ticks megan's header shows lucy's numbers, and they match `ssh lucy 'free -b; df -k ~'`.
- [ ] **Step 3: Load it** (a 30 s CPU burn on lucy while its stacks run). Run `ssh lucy 'for i in $(seq 24); do timeout 30 yes >/dev/null & done; wait'`, so each `yes` ends on its own even if ssh drops. lucy's CPU goes amber or red within about 10 s, and back within about 10 s after. megan's status keeps refreshing.
- [ ] **Step 4: Run** `whatsup list` on megan and see both machines' load.
- [ ] **Step 5:** Record the demo with `pr-record` for the PR, after asking the user before `attach`.

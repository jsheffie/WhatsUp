# WhatsUp: a Start button on each row, and a command line

Date: 2026-11-17
Issue: https://github.com/jsheffie/WhatsUp/issues/9

## Why

Each row is one dev stack: a huvr frontend (`npm start` in
`~/workspace/huvr<N>/frontend`, after `tt<N>a`), a tt-calc-app frontend
(`just dev-fresh` in `~/workspace/tt-calc-app<N>`, after `ttc<N>a`) and a
docker compose project (django, postgres, redis). Stop (issue #7) takes a
row down with one button. Bringing it back still means opening the right
herdr workspace on the right machine, running the alias, and remembering
which compose project to start. The user wants a Start button that starts
whatever of the row is not running.

The user also wants the same start and stop on the command line, so agents
can bring a stack up or down without driving the browser.

Success:

- On megan's dashboard, Start on lucy's `jds4` row, with everything down,
  starts lucy's compose project `huvr_jds4`, and opens herdr tabs
  "jds4 portal" and "tt-calc-app4" in lucy's `h4` workspace that run
  `tt4a && cd frontend && npm start` and `ttc4a && just dev-fresh`. The row
  turns green within a few minutes.
- `whatsup start jds4 --machine lucy --wait` does the same from a terminal
  on megan, and exits 0 when the row is up.
- `whatsup stop jds4 --machine lucy` stops it again, with no confirmation.

## What we found (2026-11-17, both machines)

- Stopped containers keep their port bindings. `docker ps -a` shows no
  ports for an exited container, but `docker inspect` does:
  `huvr_jds2-django-1` has `HostConfig.PortBindings` =
  `{"8000/tcp":[{"HostIp":"","HostPort":"8002"}]}`. The compose project is
  in the `com.docker.compose.project` label (`huvr_jds2`), and `docker
  compose -p huvr_jds2 start` needs no compose file. So docker Start needs
  no config, just as docker Stop needs none.
- Nothing records how a vite server was started once it has stopped.
  Those links need their start command in the config.
- The aliases need an interactive shell. A clean login shell finds `npm`,
  `pnpm`, `just` and `docker` on both machines, but `HUVR_WORKSPACE`,
  `tt<N>a`, `ttc<N>a` and `tt-calc-activate` exist only in an interactive
  shell (`~/.zshrc` on megan, `~/.bashrc` on lucy). `activate-workspace`
  refuses to run without `HUVR_WORKSPACE`.
- lucy's WhatsUp runs in a transient systemd unit. Anything it spawned
  directly would be killed with it on every restart.
- herdr can do it all over its socket (protocol 22 on both machines, herdr
  0.9.5 on megan and 0.9.1 on lucy). Tried on megan:
  - `workspace.create {label, cwd, focus:false}` returns the workspace, its
    first tab and that tab's `root_pane`. `tab.create {workspace_id, cwd,
    label, focus:false}` returns the tab and its `root_pane` too.
  - The pane runs the user's login shell, interactive (`-zsh`).
  - `pane.send_input {pane_id, text, keys:["Enter"]}` typed a command, and
    it ran.
  - `pane.read {pane_id, source:"recent", lines}` returned the output, and
    `pane.process_info {pane_id}` returned `shell_pid` and
    `foreground_process_group_id`. They are equal when the shell is idle at
    its prompt.
- A server started at an interactive prompt runs as its own job with its
  own process group, so Stop's group logic stops it and leaves the shell and
  the tab alone. That case was verified for #7.

## Goals

1. Every row with links gets a Start button beside Stop.
2. Start means: start whatever of this row is not running, on the row's
   machine. Ports that are already up are left alone.
3. Docker ports are started from their existing containers, with no config.
4. Other ports are started from a command in the config. The command runs
   in a herdr tab in the row's herdr workspace (decided by the user), typed
   into an interactive shell, so aliases work and logs stay visible.
5. One click (decided by the user).
6. Remote rows are started by the remote's own WhatsUp, as Stop does.
7. A `whatsup` command line with `list`, `start` and `stop`. Command-line
   `stop` acts immediately, with no confirmation (decided by the user).

## Non-goals

- Creating containers that do not exist (first `make up` of a new clone).
  Start then reports that no container publishes the port. A link can carry
  its own `start` for this case.
- Restarting something that is up but unhealthy.
- Starting one service of a row. Start is per row.
- Templated config. Each link spells out its own command.
- Logs inside WhatsUp. They are in the herdr tab.

## Anti-goals

- Running a command that is not in the config of the machine that runs it.
  A forwarded request names a row, never a command.
- A page on another site triggering a start. Start runs commands, so the
  Host/Origin guard on it matters more than on Stop.
- Starting a second copy of something that is already running or starting:
  a second tab running `npm start`, a docker start of a running project.
- A start that silently did nothing, or a command that died at once while
  the button says Starting. Every failure reaches the banner or the command
  line.
- Typing into a herdr pane that is busy with something else.

## The design

### Config

- A link gets an optional `start`:

  ```json
  { "label": "jds2 portal", "url": "http://jds2.local.me:3002", "port": 3002,
    "start": { "dir": "~/workspace/huvr2", "run": "tt2a && cd frontend && npm start" } }
  ```

  - `dir`: an absolute path, or one starting with `~/` (expanded to `$HOME`
    of the machine that runs the start). Required.
  - `run`: shell text, typed as is into an interactive shell in `dir`.
    Required, not blank, no newline or carriage return.
- A row gets an optional `id`, a name for the command line: 1 to 32
  characters of `A-Za-z0-9_-`, unique across every row of every group
  (case ignored). Example: `"id": "jds2"`.
- Load errors (the existing config error banner):
  - a row with any `start` and no herdr cell: "row <id or group/index>:
    start commands need a herdr cell to name the workspace";
  - two links of one row on the same port that both have `start`;
  - a bad or duplicate `id`, or a bad `dir` or `run`.
- Rollout order matters, as with `whatsup_port`: the parser rejects unknown
  fields, so install the new WhatsUp on both machines before adding `start`
  or `id` to the shared config.
- The shared config (`jds_private_config/whatsup/config.json`) gets `id`
  on every row (`jds`, `jds2`, ..., `jds15`, `ccds`, `inspire-me`,
  `megan-testbed`), and `start` on the portal and tt-calc-app links of the
  `pipelinecore` rows only (`jds` to `jds15`). The base row uses `tt1a` and
  `ttc1a`. The other groups have no herdr cell and no known start command;
  their docker links start from their containers and the rest report "no
  start command".

### What Start does for a row

1. Find the ports to act on. For each distinct port of the row, skip it
   when any of its links is not this machine (the same rule and message as
   Stop). Probe it with a TCP connect to the link's host. When it is up,
   the port's result is `none` / "already up".
2. Pick a starter for each down port:
   - The port's link has `start`: herdr.
   - Otherwise docker: one `docker ps -a --no-trunc --format
     '{{.ID}}\t{{.Label "com.docker.compose.project"}}'` and one `docker
     inspect --format '{{.Id}}\t{{json .HostConfig.PortBindings}}' <ids>`
     per request (5 s each). A container owns the port when any binding's
     `HostPort` is that port, or a range `a-b` containing it. Exactly one
     owner: its compose project, or the container alone when it has no
     project. Several owners: fail with "port N: several containers publish
     it: <names>". None: fail with "port N: no start command, and no
     container publishes it".
   - Deduplicate. Three links on 8002 and one compose project make one
     `docker compose -p huvr_jds2 start`.
3. Run the docker starters and the herdr starters at the same time, on two
   threads. The frontends start without the backend (they proxy to it only
   when a page asks), so there is no reason to wait. (My call: this keeps
   the worst case near 130 s instead of adding the two phases.) Each docker
   command gets 120 s (`compose start` can wait on health checks), and the
   docker commands run one after another. `docker` is found as Stop finds
   it.
4. herdr, for each port to start, in row order:
   - Workspace: the row's herdr cell gives the prefix (`h2`). Use the first
     matching workspace in sidebar order, the same match as the labels. If
     none matches, create one: label = the prefix, cwd = `dir`, not focused.
     Its first tab is then used for this link and renamed to the link's
     label. (My call: the user did not answer whether to create or refuse.)
   - Tab: the link's label is the tab label. If the workspace has a tab
     with that label (case ignored) whose first pane is idle (foreground
     process group equals the shell pid), reuse it. If that pane is busy,
     fail with "port N: herdr tab "<label>" in <workspace> is busy; it may
     still be starting". Otherwise create a tab (cwd = `dir`, label, not
     focused).
   - Wait for the shell's prompt in a new pane: `pane.read` returns
     non-blank text, polled every 200 ms for up to 10 s.
   - Type one line and Enter:
     `cd -- '<dir>' && { <run>; }; echo "[whatsup <nonce>] exited with $?"`
     where `<dir>` is single-quoted for the shell and `<nonce>` is a fresh
     random hex string. bash and zsh both accept this line.
   - Watch for 5 s: poll `pane.read` (recent, 60 lines) every 500 ms for a
     line `[whatsup <nonce>] exited with <digits>`. Found: the command
     ended. Fail with "port N: <run> exited with <code>" plus the last 5
     non-blank lines above the marker. Not found: success, with detail
     "herdr tab <label> in <workspace>".
   - herdr not running or not answering (1 s per call, 30 s per link
     overall): fail that port with "herdr is not running on <machine>".
     Docker ports are still started.
5. The reply comes when every starter has been launched and watched. The
   ports may take minutes more to listen; the dashboard and `--wait` watch
   for that. Worst case for one request: docker listing 10 s + compose
   120 s on one thread, two herdr links at up to 30 s each on the other,
   so about 130 s.

A start, once begun, runs to the end even if the client goes away, as a
stop does. A row is either starting or stopping, never both: one in-flight
guard per row covers both actions ("already starting" / "already
stopping", 409). The guard lasts as long as the request.

Accepted gap (my call): after the reply, a server may still be booting in
its tab. A Stop then finds nothing on that port and reports "not running",
and the server comes up afterwards. Stop does not reach into herdr tabs.
The README says: wait for the row to come up before stopping it, or press
Ctrl-C in the tab.

### herdr writes

WhatsUp sends `workspace.list` for the labels, as today. Start also sends
`workspace.create`, `tab.list`, `tab.create`, `tab.rename`, `pane.list`,
`pane.process_info`, `pane.read` and `pane.send_input`, only to this
machine's herdr socket, and only on a Start request. The README's "only
ever sends `workspace.list`" sentence changes to say so. The forwarded
herdr socket of a remote is still only read.

### API

`POST /api/start`, the same body as `/api/stop`:

```json
{ "machine": "lucy", "group": "pipelinecore", "row": 3, "ports": [3004, 4004, 8004] }
```

- Same checks as Stop: Host/Origin guard (403), row check against the
  reloaded config (409 "config changed"), unknown machine or bad body
  (400), the in-flight guard (409).
- Reply: `200 { "ok": true, "results": [...] }`, or `502` with `results` and
  a joined `error` when any port failed. Each result is `{ port, action,
  detail, error }`, with `action` one of `herdr`, `compose`, `container`,
  `none`.
- Remote rows are forwarded exactly as Stop forwards them, to
  `/api/start`, with a 180 s read timeout (the remote's 130 s worst case
  and room). The messages mirror Stop's:
  "WhatsUp is not running on lucy", "WhatsUp on lucy cannot start stacks;
  update it" (404 or not JSON), "lucy did not answer in time; it may still
  be starting", and the reworded config-skew 409.
- `/api/dashboard` adds `id` (string or null) to every row. The machine
  field `can_stop` becomes `can_act`; it gates both buttons.

### Dashboard

- The last column holds Start and Stop side by side. Start is a filled
  green pill with a play glyph, the same size as Stop.
- Start states:
  - **idle**: `Start`. Enabled when any link of the row is down, the
    machine is not offline, and it has `can_act`.
  - **disabled**: tooltip "Everything is running", "lucy is offline", or
    "Start WhatsUp on lucy to start its stacks".
  - **starting**: `Starting...`, disabled. It lasts until every port the
    reply says it started (results with action other than `none` and no
    error) is up, or 3 minutes after the click. After 3 minutes the banner
    says "<row>: still waiting for <label> (<port>)...; see its herdr tab".
    If no port was started, it ends when the reply arrives.
- Stop is disabled with "Nothing is running" when no link is up, as today.
  Both buttons are disabled while either request is in flight. During
  Starting... after the reply, Stop is enabled as usual; using it ends
  the Starting state.
- One click. A failed start puts its `error` in the banner, which works as
  it does for stops.
- Button state lives outside the DOM, keyed by `machine/group/index`,
  extending Stop's.

### Command line

The same binary. A first argument of `list`, `start` or `stop` selects the
command line, handled before the config is loaded, the instance lock is
taken or a browser is opened. Anything else runs the dashboard as today.

```
whatsup list  [--json] [--port <port>]
whatsup start <row> [--machine <name>] [--wait] [--timeout <secs>] [--json] [--port <port>]
whatsup stop  <row> [--machine <name>] [--json] [--port <port>]
```

- Options come after the command. Before a command, `--port` keeps its
  dashboard meaning (the port to listen on); after one, it names the port
  of the running WhatsUp to talk to.
- It talks to the running WhatsUp over HTTP: the port from the instance
  file (alive pid), or `--port`. Plain HTTP/1.1 to `127.0.0.1`, no
  `Origin`, so it passes the guard. Read timeout 210 s for start and stop
  (longer than the 180 s forward), 10 s for the dashboard reads. No
  WhatsUp running: "WhatsUp is not running; start it first", exit 2.
- `<row>`: a row `id` (case ignored), or `<group>/<index>` using the
  config index. `--machine` defaults to the local machine. The rows come
  from `GET /api/dashboard`, which also gives the `index` and `ports` the
  request needs. Unknown row: list the known ids, exit 2. A row not shown
  on that machine: "jds2 is not on lucy", exit 2.
- `list`: one line per row per machine: machine, id (or `group/index`),
  the first text cell, and each port up or down with its link labels, then
  a summary line per machine (`megan 12/33 up`). Offline machines and a
  missing `can_act` are said. `--json` prints `{ "machines": [...], "rows":
  [ { machine, id, group, index, title, can_act, ports: [ { port, up,
  labels } ] } ] }`.
- `start` and `stop`: send the request and print one line per port
  (`3002  herdr   herdr tab jds2 portal in h2-x`, `8002  compose
  docker compose -p huvr_jds2 start`, `4002  error   ...`). Exit 1 when the
  reply is an error.
- `start --wait`: then poll `/api/dashboard` every 2 s until every started
  port is up (exit 0) or `--timeout` (default 180 s) passes (exit 1, naming
  the ports still down). An error reply exits 1 at once, without waiting.
- `--json` prints the server's reply as is, and exit codes are unchanged.
- Exit codes: 0 success; 1 the action failed or timed out; 2 usage, unknown
  row, or WhatsUp not running.
- `whatsup --help` lists the commands.
- macOS: the binary is `WhatsUp.app/Contents/MacOS/whatsup`. The README
  says to link it into `PATH`:
  `ln -s ~/Applications/WhatsUp.app/Contents/MacOS/whatsup ~/.local/bin/whatsup`.
  Linux already installs `~/.local/bin/whatsup`.
- The README gets a short "Command line" section aimed at agents too.

## Left to the builder

- Module layout. Suggested: `src/start.rs` (ports to starters, docker
  start, the herdr launcher), herdr writes in `src/herdr.rs`, `src/cli.rs`
  for the command line, and a small blocking HTTP client shared with
  `socks.rs` where it fits. Start goes behind a trait, as `Stopper` does,
  so server tests record calls.
- Exact styling, glyphs and wording within the rules above.
- How the nonce is made (time and pid are enough).

## How we will know it works

Automated:

1. Config: `start` and `id` parse; every load error above.
2. Docker: `PortBindings` JSON to owners, including a range, an empty
   `HostPort`, two owners, and none. A fake `docker` proves one `compose
   start` for three ports of one project, and `docker start` for a container
   without a project.
3. herdr launcher against a fake herdr socket: creates a workspace when none
   matches, creates a tab, reuses an idle tab, refuses a busy one, types the
   exact line, reports an early exit with the output, and fails cleanly
   when the socket is missing.
4. Server: a port already up is skipped; Host/Origin guard (403) on
   `/api/start`; row checks; the shared in-flight guard (a start during a
   stop is 409); a non-loopback link refused; a start forwarded through the
   SOCKS relay fake; `id` and `can_act` in `/api/dashboard`.
5. Command line: argument parsing, row resolution (id, `group/index`,
   unknown, wrong machine), exit codes, and `--wait` against a test server
   whose ports come up.

On the real machines, with the new config:

1. megan dashboard, a megan row that is down (`jds5`): Start. The compose
   project starts, tabs "jds5 portal" and "tt-calc-app5" appear in herdr
   workspace `h5` running the commands, and the row turns green. Stop, then
   Start again: the same tabs are reused, no new ones.
2. megan dashboard, a lucy row: the same on lucy, in lucy's herdr.
3. lucy's own dashboard (the `megan:Lucy` profile): the same.
4. Start a row whose `run` fails at once (a throwaway row): the banner
   shows the exit code and the last lines.
5. `whatsup list`, `whatsup start <row> --machine lucy --wait` and
   `whatsup stop <row> --machine lucy` from a megan terminal.
6. Other rows are untouched throughout.

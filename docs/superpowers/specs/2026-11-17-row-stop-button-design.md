# WhatsUp: a Stop button on each row

Date: 2026-11-17
Issue: https://github.com/jsheffie/WhatsUp/issues/7

## Why

Each dashboard row is one dev stack: a huvr frontend (`npm start` in
`~/workspace/huvr<N>/frontend`), a tt-calc-app frontend (`just dev-fresh` in
`~/workspace/tt-calc-app<N>`) and a docker compose project (django, postgres,
redis). The dashboard shows which are up, but stopping a stack means finding
the right herdr pane or the right `docker compose` directory, on the right
machine. The user wants one big Stop button per row.

Success: on megan's dashboard, pressing Stop then Confirm on lucy's `jds4`
row stops lucy's vite on 3004, lucy's vite on 4004 and lucy's compose project
`huvr_jds4`, and the row goes red. The same button on megan's rows stops
megan's processes, and lucy's own dashboard stops lucy's.

## What we found (2026-11-17, both machines)

- The same kind of server is started many ways. On lucy right now: `just
  dev-fresh` in a herdr pane, `just dev`, `pnpm dev`, and `npm start` from
  Claude background shells, some since re-parented to `systemd --user`.
  Matching on an alias or a command line would miss some of these.
- What is the same every time: the listener on the row's port, and its
  process group. `npm`/`just`/`pnpm` and the vite they start share one
  process group, separate from the shell or agent that launched them.
  - lucy 3008: vite pid 1175308, pgid 1175247, also holding `npm start`.
  - lucy 4002: vite pid 1216460, pgid 1215499 = `just dev-fresh`, the
    foreground job of an interactive bash in a herdr pane.
  - megan 3000: vite pid 25348, pgid 25228, also holding `npm start`.
- Docker ports are held by docker, not by the service: `com.docker.backend`
  (the user's own process) on megan, root `docker-proxy` on lucy (`ss` shows
  no pid for it). `docker ps` shows the mapping in its Ports column
  (`0.0.0.0:8004->8000/tcp, [::]:8004->8000/tcp`) and the compose project in
  the label `com.docker.compose.project` (`huvr_jds4`). `docker compose -p
  huvr_jds4 stop` needs no compose file or directory.
- megan's WhatsUp reaches lucy's WhatsUp through the existing tunnel:
  `curl --socks5-hostname 127.0.0.1:47655 http://127.0.0.1:47654/api/dashboard`
  returned lucy's dashboard JSON.
- lucy has `ss` but no `lsof`. megan has `lsof` (in `/usr/sbin`).
- megan's WhatsUp.app starts with a minimal `PATH`; `docker` is
  `/usr/local/bin/docker` there, `/usr/bin/docker` on lucy.
- Both machines read the same shared config file
  (`jds_private_config/whatsup/config.json`), so a row has the same group id
  and row position on both when the checkouts are in sync.

## Goals

1. Every row with links has a Stop button, in a last column.
2. Stop means: stop whatever serves this row's ports, on the row's machine.
3. megan's dashboard can stop megan rows and lucy rows. lucy's dashboard can
   stop lucy rows. One implementation, always run by the WhatsUp on the
   machine that owns the processes.
4. Two clicks (decided by the user): Stop arms the button to Confirm for a
   few seconds; Confirm stops.
5. Docker is stopped with `stop`, never `down` (decided by the user), so
   containers and their data stay.

## Non-goals

- Starting a stack.
- Stopping one service of a row. The button is per row.
- Finding processes that are not on a port the dashboard lists (a second
  vite on another port, a container with no published port, postgres and
  redis of a compose project whose django is already down).
- New per-row config. The ports already in the config are enough.

## Anti-goals

- Stopping something that is not serving a port of that row. In particular:
  never signal Docker Desktop or `docker-proxy`, never signal WhatsUp
  itself, never signal the shell, herdr or agent that launched the server.
- Stopping the wrong row because the config changed between paint and click.
- A page on another site triggering a stop.
- A stop that silently did nothing. Every failure reaches the banner.

## The design

### What Stop does to one port

Resolve each distinct port of the row to one target, in this order:

1. **Docker.** List running containers once per stop request:
   `docker ps --no-trunc --format '{{.ID}}\t{{.Label "com.docker.compose.project"}}\t{{.Ports}}'`.
   A container owns the port when its Ports column has a host mapping to it:
   an entry `<addr>:<port>-><...>` or a range `<addr>:<a>-<b>-><...>` that
   contains the port. Do not use `--filter publish=`; it also matches
   container-side ports.
   - Container with a compose project: `docker compose -p <project> stop`.
   - Container without one: `docker stop <id>`.
2. **Process.** Otherwise find the pids listening on the port:
   - macOS: `lsof -nP -iTCP:<port> -sTCP:LISTEN -Fp` (lines `p<pid>`).
   - Linux: `ss -H -ltnp 'sport = :<port>'` (every `pid=<n>`).

   For each pid take its process group. Send SIGTERM to the group, wait up to
   5 s for the port to stop accepting connections (the existing
   `port_is_open`), then SIGKILL the group and wait up to 2 s more.
   - Signal the group only when it is one job: every process in the group
     is a listener on the port, an ancestor of one, or a descendant of one
     (from `ps -axo pid=,ppid=,pgid=`). Otherwise signal only the listener
     pids. This covers a server started without job control (a script that
     backgrounds several servers, a harness that does not `setsid`): its
     group then holds unrelated processes, and they are left alone. Killing
     only vite still ends `npm start` / `just dev-fresh`, which exit when
     their child does.
   - My call: an ancestor inside the group (`bash -c 'cd x && npm start'`)
     is treated as part of the job and goes with it.
   - If the group is WhatsUp's own group, or the group id is 0 or 1, signal
     only the pid.
   - If the pid is WhatsUp itself, refuse ("that is WhatsUp").
   - If the process name contains `docker` (case ignored), refuse: docker
     holds the port but step 1 did not find the container (docker not found
     or not answering). Say so.
3. **Nothing.** No container and no visible pid:
   - port closed: nothing to do, success ("not running");
   - port open: fail ("cannot see what is listening on port N").

Targets are deduplicated (three links on 8004 and a compose project are one
`compose stop`) and run in parallel. A docker stop command may take up to
60 s (compose waits 10 s per container before killing); after that it is
killed and reported as failed. The `docker ps` listing gets 5 s; if it fails
or times out, step 1 finds nothing (a hung Docker Desktop must not hold up
stopping vite). Worst case for one request is about 70 s.

A port is only stopped when every link of the row on that port names this
machine: its host resolves, on the machine doing the stop, to loopback
addresses only (`localhost`, `jds4.local.me`). Otherwise that port fails
with "port N: <host> is not this machine" (or "does not resolve on this
machine") and nothing is signalled for it. The row's other ports are still
stopped.

A stop, once started, runs to the end even if the HTTP client goes away
(tab reload, the forwarding WhatsUp timing out). The "already stopping"
guard is held until it ends.

`docker` is looked up on `PATH`, then `/usr/local/bin`, `/opt/homebrew/bin`,
`~/.docker/bin`, `/Applications/Docker.app/Contents/Resources/bin`. When it
is not found, step 1 finds nothing.

### API

`POST /api/stop`, JSON body:

```json
{ "machine": "lucy", "group": "pipelinecore", "row": 3, "ports": [3004, 4004, 8004] }
```

- `machine` is optional and defaults to this machine's label.
- `row` is the row's index in the group's `rows` in the config (all rows,
  before any per-machine filtering). `/api/dashboard` adds `"index"` and
  `"ports"` (distinct, ascending) to every row it sends.
- The server reloads the config, finds the row, and checks that it is shown
  on `machine` and that its distinct ports equal `ports`. Otherwise
  `409 { "error": "the dashboard config changed; reload and try again" }`.
  Unknown group, row out of range, row with no links: `400`.
- A row already being stopped by this server: `409 { "error": "already
  stopping" }`.
- Guard against other sites: the `Host` header's host must be `127.0.0.1`,
  `localhost` or `[::1]`, and an `Origin` header, when present, must be
  `http://<Host>`. Otherwise `403`.
- Reply when every target succeeded: `200`
  `{ "ok": true, "results": [ { "port": 3004, "action": "process", "detail": "pgid 879338", "error": null }, ... ] }`.
  `action` is `process`, `compose`, `container` or `none`.
  When any target failed: `502` with the same `results` and an `"error"`
  string that joins the failures (`port 8004: docker compose -p huvr_jds4
  stop: ...`).

### Remote rows

- When `machine` names an active remote, the server does the same row check
  against its own config, then forwards the same JSON body to the WhatsUp on
  that machine: SOCKS CONNECT through the remote's tunnel to
  `127.0.0.1:<whatsup_port>`, then a plain HTTP/1.1 `POST /api/stop` with
  `Host: 127.0.0.1:<whatsup_port>`, `Connection: close` and no `Origin`. It
  relays the status and body it gets back. Read timeout 90 s.
- The remote WhatsUp treats it like any local request: `machine` is its own
  label, and it checks the row against its own config.
- Failures:
  - tunnel or remote WhatsUp unreachable: `502 { "error": "WhatsUp is not
    running on lucy" }`;
  - `404` from the remote, or a reply that is not JSON (an older WhatsUp,
    or something else on that port): `502 { "error": "WhatsUp on lucy
    cannot stop stacks; update it" }`;
  - connected, but no complete reply within 90 s: `502 { "error": "lucy did
    not answer in time; it may still be stopping" }`.
- An error relayed from the remote is prefixed with its name (`lucy: port
  8004: ...`). The remote's "config changed" 409 is reworded, because this
  dashboard already checked the row against its own config: "lucy's WhatsUp
  has a different config for that row; update the config on both machines".
- The "already stopping" guard is kept by the machine that does the stop,
  not by the forwarder.
- A body that is not a valid request gets a JSON `400` like the other
  errors (the handler parses it itself).
- Machine names are compared ignoring ASCII case, as `shown_on` does.
- New optional remote field `whatsup_port` (default 47654, the port WhatsUp
  prefers), for a remote WhatsUp started with `--port`.
- `/api/dashboard` adds `can_stop` to each machine: `true` for the local
  machine; for a remote, whether `127.0.0.1:<whatsup_port>` answered through
  the tunnel, probed with the link probes in the same pass. That extra
  target does not count in `ports_up` / `ports_total`. It is a TCP check
  only: something else on that port enables the buttons, and the stop then
  fails with the "cannot stop stacks" message. Accepted.
- My call: forward to the remote WhatsUp instead of `ssh lucy whatsup stop`.
  It reuses the tunnel (no new SSH login), keeps one code path, and the
  remote checks the request against its own config. The cost: lucy's WhatsUp
  must be running; the dashboard says so when it is not.

### Dashboard

- Every table gets a last column with an empty header. A row with at least
  one link gets a Stop button there; other rows get an empty cell.
- The button is large and red: a filled pill with a square stop glyph and
  the word Stop, clearly the heaviest control in the row. Exact size and
  color are the builder's call, in the existing dark style.
- States:
  - **idle**: `Stop`. Enabled when any link in the row is up, the machine is
    not offline and the machine has `can_stop`.
  - **disabled**: muted outline. Tooltip says why: "Nothing is running",
    "lucy is offline", or "Start WhatsUp on lucy to stop its stacks".
  - **armed** (after one click): `Confirm`, visibly different, for 4 s, then
    back to idle.
  - **stopping** (after the second click): `Stopping...`, disabled, until
    the request returns.
- The table is rebuilt whenever the status changes, which happens during a
  stop. Button state therefore lives outside the DOM, keyed by
  `machine/group/index`, and is reapplied on every build.
- A failed stop puts the server's `error` in the banner, and it stays
  there across refreshes (today the 2 s refresh repaints the banner from
  the config error alone, which would wipe it). It clears when another stop
  or open succeeds or starts, or when the banner is clicked. A config error
  takes precedence while it lasts. Failed opens get the same treatment.
- The "Up only" filter and the all-up row tint keep working from link cells
  only.

### Config

- `remotes[].whatsup_port`: optional, a TCP port above 0. Nothing else.
- Rollout order matters, as before: the parser rejects unknown fields, so
  install the new WhatsUp on both machines before adding `whatsup_port` to
  the shared config. The feature needs no config change at all today.

## Left to the builder

- Module layout. A new `src/stop.rs` for finding and stopping, behind a
  trait (like `BrowserOpener`) so server tests record calls.
- How signals are sent (`libc` or the `kill` command).
- Exact styling and wording within the rules above.
- README section.

## How we will know it works

Automated:

1. Parsers: `ss` and `lsof` output to pids; `docker ps` lines to the owner
   of a port, including a range and an unrelated container-side port.
2. A real child in its own process group listening on a port is stopped,
   and its group mate dies with it. A child that ignores SIGTERM is killed
   after the (shortened) grace period.
3. A fake `docker` proves the compose and container commands, and that two
   ports of one project give one `compose stop`.
4. Server: row checks (200, 400, 409), the Host/Origin guard (403), the
   in-flight guard, a non-loopback host refused, and a stop forwarded
   through a relaying SOCKS fake to a second server.
6. The one-job rule: a group with an unrelated member gets pid-only
   signals; a group of launcher, runner and server gets a group signal.
5. `can_stop` true and false for a remote; counts unchanged by the extra
   probe.

On the real machines:

1. megan dashboard, a megan row: Stop, Confirm. The row's vite and compose
   project stop; the herdr pane that ran them is back at its prompt; Docker
   Desktop is still running.
2. megan dashboard, a lucy row: the same on lucy.
3. lucy's own dashboard (through the `megan:Lucy` profile): the same.
4. Stop, then wait: the button returns to Stop after 4 s and nothing was
   stopped.
5. Quit lucy's WhatsUp: lucy's buttons on megan's dashboard are disabled
   with the reason. Start it again: they come back.
6. megan's rows and lucy's rows on other ports are untouched after each
   stop.

# WhatsUp

A tiny local dashboard for macOS and Linux that shows which of your dev services are listening, and opens them in your browser with one click.

You describe your stacks in a JSON config (groups of rows, each cell a label or a link with a port). WhatsUp probes each port every couple of seconds and lights up the links that are up.

## Run

```sh
cargo run            # reads config/whatsup.json
cargo run -- --config path/to/config.json --port 47654 --no-open
```

## Install

macOS:

```sh
packaging/build-app.sh   # produces dist/WhatsUp.app
```

Linux:

```sh
packaging/install-linux.sh              # ~/.local/bin/whatsup + an app launcher entry
packaging/install-linux.sh --uninstall
```

An installed WhatsUp reads the user config file and seeds it from the bundled example (`config/whatsup.json`) on first launch:

| | Config | Instance file |
|---|---|---|
| macOS | `~/Library/Application Support/WhatsUp/config.json` | same folder |
| Linux | `$XDG_CONFIG_HOME/whatsup/config.json` (`~/.config/whatsup/`) | `$XDG_RUNTIME_DIR/whatsup/` |

Launching WhatsUp while it is already running just opens the running dashboard. The Quit button stops it.

## Config resolution

1. `--config PATH`
2. `WHATSUP_CONFIG` environment variable
3. `config/whatsup.json` in the repo (when run via `cargo run`)
4. The user config file above

Tip: keep your real config in a private dotfiles repo and symlink it to the user config path. The same file works on both platforms.

## Browser

`browser_app` picks the browser (default `"Google Chrome"`), and `open` is `tab` or `window`.

- macOS: `browser_app` is an application name, driven through AppleScript.
- Linux: `browser_app` is a command name or path (`chromium`, `/opt/brave/brave`). Common macOS names map to their Linux commands, so `"Google Chrome"` runs `google-chrome-stable`, and `"Chromium"`, `"Firefox"`, `"Brave Browser"`, `"Microsoft Edge"` and `"Vivaldi"` work too. `window` mode passes `--new-window`. When the browser is not installed, links open with `xdg-open` in the default browser.

## Config format

See [`config/whatsup.json`](config/whatsup.json) for a working example. Each link is `{ "label", "url", "port"? }`; when `port` is omitted it is derived from the URL. `null` leaves a cell empty. `tone` (`gray` / `black`) alternates row shading.

## Remote machines

WhatsUp can also show the stacks of another machine on the same network, and open their pages in a dedicated Chrome profile that browses as if it were on that machine. Both machines can run the same stacks on the same ports at once.

```json
"machine_names": { "Jeffs-MacBook-Pro-2": "megan" },
"remotes": [
  { "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy" }
]
```

- `remotes`: each entry mirrors every group for that machine, in its own sections with a machine tag and tab.
  - `ssh` is a host from `~/.ssh/config` that logs in without a prompt.
  - `browser_profile` is the display name of the Chrome profile that opens its links.
  - `socks_port` is optional (default `47655`).
- A row can carry `"machines": ["lucy"]` to show only on those machines (by label), for stacks that exist on one machine only. Rows without it show everywhere.
- `machine_names`: optional labels for short hostnames (`hostname -s`). A remote whose `name` matches this machine's label is ignored, so one shared config works on every machine.

How it works:

- WhatsUp runs `ssh -N -D 127.0.0.1:<socks_port> <ssh>` for each remote and restarts it with backoff (1s up to 30s) when it drops. It stops when WhatsUp quits.
- Status is checked through that tunnel: the remote machine itself connects to each link's host and port, so a link is green only when the page would load there. When the tunnel is down, the remote's links turn gray with an "offline" note.
- Links open in the remote's Chrome profile. That profile sends all of its traffic through the tunnel, so host names like `jds4.local.me` and `localhost` resolve on the remote machine.

One-time profile setup:

1. Create a Chrome profile with the `browser_profile` name. It does not need to be signed in to Chrome.
2. In that profile, open `chrome://extensions`, turn on Developer mode, choose Load unpacked, and pick `extension/remote-proxy` from this repo. If you change `socks_port`, change `SOCKS_PORT` in `extension/remote-proxy/background.js` to match.

## Stopping a stack

Every row with links has a Stop button beside its Start button. Click it, then click Confirm within 4 seconds. Stop means "stop whatever serves this row's ports":

- A port published by docker: `docker compose -p <project> stop` for the container's compose project (so its database and other services stop too), or `docker stop` for a container outside compose. Containers are stopped, never removed.
- Any other listener: SIGTERM to its process group, then SIGKILL if the port is still open after 5 seconds. The group is the server plus the `npm` / `just` / `pnpm` that started it, not the shell or terminal it was started from. When the group also holds unrelated processes, only the listener itself is signalled.
- Only ports whose links point at the machine itself (`localhost`, or a name that resolves to loopback there) are stopped.

Things the dashboard has no port for are not found: a second dev server on another port, a container without a published port, or the database of a compose project whose web container is already down.

Rows of a remote machine are stopped by the WhatsUp running on that machine: the request goes through the remote's tunnel to its WhatsUp on `127.0.0.1:47654`. So WhatsUp must be running there, in the same version or newer, with the same config. Add `"whatsup_port": <port>` to the remote's entry if its WhatsUp was started with `--port`. When the remote's WhatsUp does not answer, its Start and Stop buttons are disabled and say so.

## Starting a stack

Every row with links has a Start button beside Stop. It starts whatever of the row is not running, with one click. Ports that are already up are left alone, and only ports whose links point at the machine itself are started.

- A port published by docker: `docker compose -p <project> start` for the compose project of the container that publishes it, or `docker start` for a container outside compose. It uses the existing, stopped containers (they keep their port bindings), so it needs no config. Containers are never created.
- A link with a `start` in the config: the command runs in a herdr tab named after the link, in the row's herdr workspace (the first one matching the row's `herdr` prefix, created if none matches). A tab with that name is reused when its shell is idle, and refused ("busy; it may still be starting") when it is not; otherwise a tab is created. WhatsUp types this one line into the tab's interactive shell, so aliases like `tt2a` work and the logs stay in the tab:
  `cd -- '<dir>' && { <run>; }; echo "[whatsup <nonce>] exited with $?"`
- When the command fails (a nonzero exit code) within 5 seconds, Start reports the code and the last lines of output instead of success. Exiting 0 within those 5 seconds counts as started ("<run> finished"); the ports then say whether the row came up. If herdr stops answering after the command was typed, Start still reports the port as started, with "could not watch it start".
- A port with no `start` and no container publishing it fails with "no start command, and no container publishes it". Several containers publishing it fails with "several containers publish it: <names>".
- The button reads Starting... until every port it started is up, or for 3 minutes, after which the banner names what is still down and where to look: its herdr tab, or docker.

Rows of a remote machine are started by the WhatsUp running on that machine (same tunnel and requirements as Stop). A request names a row, never a command: each WhatsUp runs only commands from its own config.

```json
{ "id": "jds2", "tone": "gray", "cells": [
  "huvr2, tt-calc-app2",
  { "text": "jds2", "herdr": "h2" },
  { "label": "jds2 portal", "url": "http://jds2.local.me:3002", "port": 3002,
    "start": { "dir": "~/workspace/huvr2", "run": "tt2a && cd frontend && npm start" } }
] }
```

- `start.dir` is absolute or starts with `~/` (the home of the machine that runs it). `start.run` is one line of shell text, without tabs or other control characters, that does not end with `;`, `&` or `|` (typed inside `{ <run>; }`, those would run nothing).
- `id` on a row is its name for the [command line](#command-line): 1 to 32 characters of `A-Za-z0-9_-`, unique across all rows (case ignored).
- The config is rejected when a row has a `start` but no herdr cell (the workspace has no name), when two links of a row on the same port both have a `start`, or when an `id`, `dir` or `run` is invalid.
- A row is either starting or stopping, never both.

A server may still be booting in its tab after Start returns, and a Stop then finds nothing on its port. Wait for the row to come up before stopping it, or press Ctrl-C in the tab.

Rollout: older versions reject unknown fields, so install the new WhatsUp on every machine before adding `start` or `id` to a shared config.

## herdr workspace labels

When a stack runs in its own herdr workspace, the dashboard can show that workspace's name next to the stack: the Workspace cell `jds` reads `jds/h1-herberth`.

```json
{ "text": "jds", "herdr": "h1" }
```

- Use this cell form in place of a plain text cell. `herdr` is a prefix: it matches a herdr workspace whose label is `h1`, or starts with `h1` followed by a separator (`h1-herberth`, `h1 x`). `h10-...` does not match. Case is ignored.
- Each machine's section uses that machine's herdr (its default session). With several matches, the cell shows the first one in herdr's sidebar order plus `+N`, and the tooltip lists them all.
- On this machine WhatsUp reads `$XDG_CONFIG_HOME/herdr/herdr.sock`, else `~/.config/herdr/herdr.sock`.
- For a remote, add `"herdr_socket": "/home/<user>/.config/herdr/herdr.sock"` (absolute) to its entry. WhatsUp forwards that socket over the remote's tunnel to `herdr-<socks_port>.sock` in its own directory.
- For the labels WhatsUp sends only herdr's read-only `workspace.list` request. Start (see above) also creates workspaces and tabs, reads panes and types the configured command, but only on this machine's herdr socket and only on a Start request. The forwarded socket of a remote is still only read. If herdr is not running or does not answer within a second, the cell shows its plain text.

## Command line

The same binary starts and stops rows from a terminal, for people and agents. It talks to the running WhatsUp over HTTP (the port comes from the instance file, or `--port`), so rows of a remote machine are forwarded exactly as the buttons do.

```
whatsup list  [--json] [--port <port>]
whatsup start <row> [--machine <name>] [--wait] [--timeout <secs>] [--json] [--port <port>]
whatsup stop  <row> [--machine <name>] [--json] [--port <port>]
```

- `<row>` is a row `id` (case ignored) or `<group>/<index>` using the config index. `--machine` defaults to this machine.
- Options go after the command. Before a command, `--port` is the port to listen on; after it, the port of the running WhatsUp.
- `list` prints, for each machine, a summary line such as `megan 12/33 up` (marked `(offline)` or `(WhatsUp not running there)` when so), then one line per row with its ports up or down and their labels. `--json` prints `{ machines, rows }`.
- `start` and `stop` print one line per port (`3002  herdr   herdr tab jds2 portal in h2`). `stop` acts at once, with no confirmation.
- `start --wait` then polls until every port it started is up, or `--timeout` seconds (default 180) pass. `--json` prints the server's reply as is.
- Exit codes: 0 success; 1 the action failed or timed out; 2 usage, unknown row, or WhatsUp not running.

```sh
whatsup list                      # find the row id
whatsup start jds2 --wait         # exits 0 once jds2 is up
whatsup stop jds2 --machine lucy
```

Linux installs `~/.local/bin/whatsup`. On macOS the binary is inside the app; link it into your `PATH`:

```sh
ln -s ~/Applications/WhatsUp.app/Contents/MacOS/whatsup ~/.local/bin/whatsup
```

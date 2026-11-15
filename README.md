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
- `machine_names`: optional labels for short hostnames (`hostname -s`). A remote whose `name` matches this machine's label is ignored, so one shared config works on every machine.

How it works:

- WhatsUp runs `ssh -N -D 127.0.0.1:<socks_port> <ssh>` for each remote and restarts it with backoff (1s up to 30s) when it drops. It stops when WhatsUp quits.
- Status is checked through that tunnel: the remote machine itself connects to each link's host and port, so a link is green only when the page would load there. When the tunnel is down, the remote's links turn gray with an "offline" note.
- Links open in the remote's Chrome profile. That profile sends all of its traffic through the tunnel, so host names like `jds4.local.me` and `localhost` resolve on the remote machine.

One-time profile setup:

1. Create a Chrome profile with the `browser_profile` name. It does not need to be signed in to Chrome.
2. In that profile, open `chrome://extensions`, turn on Developer mode, choose Load unpacked, and pick `extension/remote-proxy` from this repo. If you change `socks_port`, change `SOCKS_PORT` in `extension/remote-proxy/background.js` to match.

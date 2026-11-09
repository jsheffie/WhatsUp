# WhatsUp

A tiny local dashboard for macOS that shows which of your dev services are listening, and opens them in Chrome with one click.

You describe your stacks in a JSON config (groups of rows, each cell a label or a link with a port). WhatsUp probes each port every couple of seconds and lights up the links that are up.

## Run

```sh
cargo run            # reads config/whatsup.json
cargo run -- --config path/to/config.json --port 47654 --no-open
```

## Build the Mac app

```sh
packaging/build-app.sh   # produces dist/WhatsUp.app
```

The app reads `~/Library/Application Support/WhatsUp/config.json`. On first launch it seeds that file from the bundled example (`config/whatsup.json`).

## Config resolution

1. `--config PATH`
2. `WHATSUP_CONFIG` environment variable
3. `config/whatsup.json` in the repo (when run via `cargo run`)
4. `~/Library/Application Support/WhatsUp/config.json`

Tip: keep your real config in a private dotfiles repo and symlink it to the Application Support path.

## Config format

See [`config/whatsup.json`](config/whatsup.json) for a working example. Each link is `{ "label", "url", "port"? }`; when `port` is omitted it is derived from the URL. `null` leaves a cell empty. `tone` (`gray` / `black`) alternates row shading.

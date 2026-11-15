# WhatsUp: see and open lucy's stacks from megan

Date: 2026-11-14

## Why

The user develops on two machines on the same home LAN: megan (macOS, where he
sits) and lucy (Linux, reached through a herdr client and SSH). Both run the
same kind of dev stacks (huvr + tt-calc-app workspaces `jdsN`, plus a few
personal projects) on the same ports. WhatsUp on megan only knows about
megan, and there is currently no way to open lucy's pages in a browser on
megan at all.

Success: on megan, one WhatsUp dashboard shows which of lucy's stacks are up
next to megan's own, and one click opens a lucy page in a Chrome window on
megan that works exactly like it does locally (same URLs, same logins, same
API calls).

## What makes this hard

- URLs must stay as they are: `http://jdsN.local.me:<port>` and
  `http://localhost:<port>` (tt-calc-app). Both machines map every
  `jdsN.local.me` to `127.0.0.1` in `/etc/hosts`, and the frontend, Django
  `ALLOWED_HOSTS`, CORS and cookies are all keyed on those names.
- The same `jdsN` can be running on both machines at once, so megan cannot
  simply point `jdsN.local.me` at lucy, or forward lucy's ports onto megan's
  ports; that would collide with megan's own stacks.
- A forwarded port always accepts a TCP connection, so "is the port open on
  megan" says nothing about whether lucy's server is up.
- Background, already documented in
  `jds_private_config/megan_to_lucy_dev.md`: reaching lucy by hostname over
  the LAN hits DNS, Vite bind address, Vite allowed hosts and ufw walls.

## Goals

1. megan's WhatsUp shows lucy's stacks with live up/down status.
2. Clicking a lucy link opens it on megan in a dedicated Chrome profile named
   `megan:Lucy`, at the same URL lucy itself would use.
3. WhatsUp on megan starts the connection to lucy and keeps it alive across
   sleep, Wi-Fi drops and lucy reboots, with no manual step.
4. When lucy is unreachable, the dashboard says so clearly instead of showing
   every lucy link as down.

## Non-goals

- lucy watching megan (the reverse direction). Design so it is mostly config
  later (sshd on megan + a remote entry on lucy), but do not build or test it
  now.
- Access from outside the home LAN. Tailscale comes later and should only
  change the SSH host name.
- Running WhatsUp on lucy for this feature. lucy needs nothing new installed.
- Opening ufw ports or changing app bind addresses on lucy for LAN access.

## Anti-goals

- Anything that breaks or slows megan's own dashboard or links, including
  when lucy is off.
- A status that lies: showing lucy's link green when lucy's server is down
  (the forwarded-port trap).
- Sending the user's normal (Work / Personal) Chrome traffic through lucy.

## The design

### Connection: one SSH SOCKS tunnel owned by WhatsUp

WhatsUp on megan runs and supervises one process per remote:

    ssh -N -D 127.0.0.1:<socks_port> -o BatchMode=yes \
        -o ExitOnForwardFailure=yes -o ServerAliveInterval=5 \
        -o ServerAliveCountMax=2 <ssh_host>

- `<ssh_host>` is an `~/.ssh/config` host (today `lucy`, which already works
  with key auth; verified in session).
- If it exits, restart with backoff (e.g. 1s doubling to 30s cap). Never
  prompt for anything (`BatchMode`).
- Stopped when WhatsUp quits. A leftover listener on the SOCKS port from a
  previous run must not wedge it (detect and report clearly, or reclaim it
  if it is ours).

### Status: probe lucy's ports through the tunnel

For each lucy link, WhatsUp opens a SOCKS5 CONNECT through the tunnel to
that link's own host and port (`jdsN.local.me:3004`, `localhost:4004`), so
lucy's sshd resolves the name exactly as the browser's request will be
resolved. This is the real answer from lucy, so it cannot be fooled by the
forwarded-port trap, and a link is never green while its page would fail.
Status is therefore per link (host + port), not per port, for remotes.

OpenSSH's `-D` behavior (checked in `channels.c` during review): the success
reply is held until lucy has connected to the target; if lucy cannot
connect, ssh closes the socket without any reply.

- SOCKS success reply: up.
- Socket closed without a reply: down.
- Connection refused on the local SOCKS port, or a timeout (for example a
  stalled tunnel after megan wakes): the remote is "offline", not "down".
- Probes run on the same refresh cycle and timeouts as local probes, and a
  slow or dead lucy must never delay megan's own status.

### Opening: the `megan:Lucy` Chrome profile

- The user creates a normal Chrome profile named `megan:Lucy` (one-time, by
  hand). It is not signed into any Google account in Chrome (no sync), so no
  Workspace policy applies to it; app logins (Google sign-in on the huvr and
  tt-calc-app pages) happen inside the page as usual. megan has no
  machine-level Chrome policies (checked: no `/Library/Managed Preferences`,
  no proxy or extension keys in `com.google.Chrome` defaults).
- A tiny private Chrome extension, kept in this repo and loaded unpacked into
  that profile only, sets the profile's proxy with `chrome.proxy`:
  `fixed_servers`, `socks5 127.0.0.1:<socks_port>`, bypass list
  `["<-loopback>"]` so `localhost` and `127.0.0.1` also go to lucy (Chrome
  otherwise never proxies loopback). With SOCKS5, Chrome sends host names to
  the proxy, so lucy resolves `jdsN.local.me`, exactly like a browser on
  lucy would.
- All traffic of that profile goes through lucy. That is fine: same LAN,
  same internet connection, and the Work / Personal profiles are untouched.
- WhatsUp opens a lucy link in that profile. On macOS, AppleScript cannot
  pick a profile, so this goes through
  `open -na "Google Chrome" --args --profile-directory=<dir> <url>`, where
  `<dir>` is looked up by display name in Chrome's `Local State` file
  (`profile.info_cache`).

### Dashboard: a section per machine (layout A, chosen by the user)

- megan's groups render exactly as today, tagged `megan`.
- Below them, every group is rendered again for lucy, tagged `lucy` (purple
  tag in the mockup). All groups are mirrored: AssetCORE/PipelineCORE,
  Personal Projects, Django Schema Evaluation.
- The tab bar gains machine filters (`megan`, `lucy`) next to `All` and the
  group tabs.
- lucy offline: lucy's links render gray with a "lucy offline" note on its
  sections.
- The `/api/open` allow-list check covers remote links too: a request names
  the machine as well as the URL, and only configured URLs for that machine
  open.

### Config

The shared config (symlinked from `jds_private_config/whatsup/config.json`
on both machines) gains a remote list, roughly:

    "remotes": [
      { "name": "lucy", "ssh": "lucy", "browser_profile": "megan:Lucy" }
    ]

- `socks_port` is optional with a fixed default; the extension uses the same
  default.
- A remote whose `name` matches the local machine's short hostname is
  ignored, so lucy's own WhatsUp, reading the same file, does not tunnel to
  itself.
- Every remote mirrors all groups; there is no per-group remote selection.
- Unchanged configs without `remotes` behave exactly as today.

### Rollout order (lucy must not break)

The config parser rejects unknown fields, and lucy runs WhatsUp on the same
shared config file. An old lucy WhatsUp that sees `remotes` shows a config
error while running and refuses to start next time. So:

1. Ship the WhatsUp change and install it on lucy first. lucy has
   `~/.local/bin/whatsup` but no WhatsUp checkout: clone it there and run
   `packaging/install-linux.sh` over SSH.
2. Then add `remotes` to `jds_private_config/whatsup/config.json`. lucy
   picks it up only when its `jds_private_config` checkout is pulled (it is
   a plain git checkout, currently behind `main`).

### One fix on lucy: Vite only listens on `::1`

Verified in session: megan's Vite listens on `127.0.0.1:3000`, but lucy's
Vite servers listen on `[::1]:30xx` / `[::1]:40xx` only, while lucy's
`/etc/hosts` maps `jdsN.local.me` to `127.0.0.1` only. So in the Lucy
profile `jdsN.local.me:30xx` would fail while `localhost:40xx` and Django
(`0.0.0.0:80xx`) work. Fix (decided by the user): a one-time edit of lucy's
`/etc/hosts` that adds a `::1 jdsN.local.me` line next to each existing
`127.0.0.1 jdsN.local.me` line (jds through jds10). The user runs it with
sudo; the builder prepares the exact command. No app code change and no
change to the activate scripts; OpenSSH tries each resolved address.
Confirm end to end; if it does not work, come back to the user before
touching the huvr / tt-calc-app repos.

## Supporting changes in `jds_config` (decided by the user)

These live in `~/workspace/jds_config` (a separate repo; branch and PR it
the usual way, `jds-<issue>-...`).

- **`claude-chrome-browser`** (source of truth: `jds_config/bin/claude-chrome-browser`,
  installed with `make install` in `jds_config/claude/`). Today it decides a
  profile's role from the Google account signed into it, and `megan:Lucy` has
  none. Teach it a `lucy` role for that profile (identified by profile
  name), print it in the table, and mention it in the hook guidance so a
  session can select it for lucy testing. Update
  `jds_config/doc/claude-in-chrome.md` and the global agent instructions'
  profile list to match.
- **Claude extension in `megan:Lucy`**: installed so the builder can drive
  the end-to-end checks there. Note that its traffic will go through lucy's
  proxy too; confirm it still connects.
- **skhd key `ctrl + cmd - z`** opens a new window in the `megan:Lucy`
  profile, like the existing `cmd + ctrl - x/c/v` launchers. Edit the source
  file `jds_config/skhd/skhdrc-app-launchers` (not the generated `skhdrc`),
  rebuild `skhdrc` with the skhd Makefile target that matches the layout in
  use, and reload skhd. Use the profile's directory name (`Profile N`) once
  the profile exists. Check `ctrl + cmd - z` is not already bound.

## Calls made in brainstorming (the user can overrule)

- No WhatsUp-to-WhatsUp client/server: megan probes lucy directly through
  the tunnel, so lucy needs nothing installed and there is one config.
- A Chrome profile plus a ~20-line private extension, not a second Chrome
  instance and not a third-party proxy extension.
- The whole Lucy profile is proxied (not just `local.me` / `localhost`),
  because Chrome will not proxy loopback from a PAC script.
- One direction only (megan watches lucy) for now.

## Left to the builder

- SOCKS5 client details, default `socks_port`, backoff numbers, how the
  tunnel state is surfaced in `/api/dashboard`.
- Exact tab bar and tag styling (follow the existing dashboard and the
  approved mockup in `.superpowers/brainstorm/`).
- Where the extension lives in the repo and the README steps for loading it.
- Linux behavior of the remote feature: it must compile and not break lucy's
  local dashboard; opening in a profile on Linux can use
  `--profile-directory` the same way, but is not required to be tested.

## How we will know it works (E2E on the real machines)

1. With lucy stacks up (several are up today), megan's dashboard shows them
   green in the lucy sections and megan's own status is unchanged.
2. Stop a lucy server: its link goes red within a refresh or two. Start it:
   green again.
0. The builder drives these through Claude in Chrome in the `megan:Lucy`
   profile (selected via `claude-chrome-browser`); the user handles any
   login prompt.
3. Click a lucy AssetCORE link: it opens in `megan:Lucy` and the app loads
   and talks to its API (login works). Same for a tt-calc-app
   `localhost:40xx` link and a Django admin link.
4. Same `jdsN` up on both machines: each link opens its own machine's app.
5. Cut the connection (kill the ssh process, or sleep megan): lucy shows
   offline, then recovers by itself without touching anything.
6. Quit WhatsUp: the ssh process is gone.

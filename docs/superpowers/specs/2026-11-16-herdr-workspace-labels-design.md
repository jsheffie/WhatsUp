# WhatsUp: show the herdr workspace next to each stack

Date: 2026-11-16

## Why

The user runs every dev stack (`jds`, `jds2`, ... `jds10`) in its own herdr
workspace, on megan and on lucy. The herdr label says what the stack is busy
with right now (`h1-herberth`, `h4-apitb-14384`), but the WhatsUp dashboard
only shows the fixed stack name (`jds`). Seeing both on one line answers
"which stack was I using for what" without switching to herdr.

Success: in lucy's section, the Workspace cell of the `jds` row reads
`jds/h1-herberth` while lucy's herdr has a workspace named `h1-herberth`.
Megan's section shows megan's herdr labels the same way.

## What we found (2026-11-16, both machines)

- herdr (megan 0.9.5, lucy 0.9.1, both API protocol 22) listens on
  `~/.config/herdr/herdr.sock` for its default session (`herdr session list
  --json` reports that path on both machines; `XDG_CONFIG_HOME` is unset on
  both).
- The socket speaks newline-delimited JSON. One request line
  `{"id":"whatsup","method":"workspace.list","params":{}}` returns one line
  `{"id":"whatsup","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1Z","number":2,"label":"h1","focused":false,...}]}}`.
  An unknown method returns `{"id":...,"error":{"code":"invalid_request",...}}`.
  Both versions answer `workspace.list` the same way.
- WhatsUp can query herdr without the `herdr` CLI (the macOS app starts with a
  minimal `PATH` that has no `/opt/homebrew/bin`).
- lucy's socket can ride WhatsUp's existing SSH tunnel:
  `-o StreamLocalBindUnlink=yes -L <local.sock>:/home/jds/.config/herdr/herdr.sock`
  gave a `0600` local socket that answered in about 0.14 s.
  - The remote path must be absolute. ssh rejects `.config/herdr/herdr.sock`.
  - On macOS the local path must stay under about 100 bytes, or ssh fails
    with "Bad local forwarding specification".
  - When the remote socket is missing, the tunnel stays up. The local
    connection gets EOF with no reply, and ssh prints
    `channel N: open failed: connect failed: ...`, which `is_probe_noise`
    already ignores.
- The lucy herdr labels are `h1-herberth`, `h2-licensing`, ... `h10-e2e-testing`.
  There are also non-stack workspaces such as `lazydocker`, `mattock` and
  `WhatsUp`. Megan's are `h1` (renamed from `huvr` today), `h2`, `h3`,
  `h4-apitb`, `h5` and `h6`.

## Goals

1. A row's Workspace cell shows `<text>/<herdr label>` when that machine's
   herdr has a matching workspace, and plain `<text>` otherwise.
2. Each machine section uses that machine's own herdr: megan's section reads
   megan's herdr, and lucy's section reads lucy's, through the tunnel.
3. Any herdr failure leaves the cell as plain `<text>`. A failure can be
   herdr not running, herdr not installed, the socket missing, a timeout, a
   bad reply, or lucy offline. It never adds an error, never delays status,
   and never takes the tunnel down.

## Non-goals

- Agent status (working / idle), click-to-focus a herdr workspace, or any
  other herdr control. WhatsUp only ever sends `workspace.list`.
- herdr sessions other than the default one. Megan's `personal` session holds
  no stack workspaces today.
- Matching by pane directories. The user chose the name rule below. A
  pane-directory rule was tested on the live data; it also worked, but it
  needed per-row directory lists and a remote home directory.
- Changes to the other groups (Personal Projects, Django Schema Evaluation).
  They have no Workspace column. The new cell form works in any group if one
  is added later.

## Anti-goals

- A stale or wrong label presented as current. If the query fails, show the
  plain text, never the last good label.
- Breaking lucy's tunnel or status because of the herdr forward.
- Showing a label for the wrong stack, for example `h10-...` on the `jds`
  (`h1`) row.

## The design

### Matching: the name rule (decided by the user)

- In the config, a Workspace cell names its herdr prefix (see Config).
- A herdr workspace label matches prefix `p` when the label equals `p`, or
  starts with `p` followed by a character that is not an ASCII letter or digit
  (`h1-herberth`, `h1 x`, `h1_x`, `h1.x`). The comparison ignores ASCII case.
  So `h1` matches `h1` and `h1-herberth`, but not `h10-e2e-testing` and not
  `h1x`.
- Matches are ordered by herdr's workspace `number` (sidebar order).

### Display: first match plus a count (decided by the user)

- No match: `jds`.
- One match: `jds/h1-herberth`.
- More than one match: `jds6/h6 +1`. The cell's tooltip lists every matched
  label, in order (for example `herdr: h6, h6-hotfix`).
- The `/h1-herberth` part is visually secondary, so the stack name still scans
  down the column. It keeps the row height of the other rows. How long labels
  behave (truncation and the tooltip) is the builder's call; follow the
  existing dashboard style.

### Querying herdr

- Use one blocking client: connect to a Unix socket, write the
  `workspace.list` line, read one reply line, and parse only
  `result.workspaces[].number` and `.label`. Unknown fields are ignored, so
  herdr versions can drift. Anything else (an `error` reply, a missing
  `result`, EOF, a parse failure, more than 1 s in total, or a reply larger
  than 1 MiB) means "no herdr data".
- Local machine: `$XDG_CONFIG_HOME/herdr/herdr.sock` when `XDG_CONFIG_HOME` is
  set and absolute, else `$HOME/.config/herdr/herdr.sock`.
- Remote machine: the local end of the tunnel forward (below), only when the
  remote has `herdr_socket` in the config and its tunnel is running.
- When to query: during each `/api/dashboard` request, alongside the port
  probes and in parallel with them, using `spawn_blocking` like the probes.
  - A slow herdr never holds up the response beyond its 1 s budget.
  - The local status and the remote status never wait on each other's
    herdr.
  - Each machine is queried at most once per request, only when that
    machine's rows have a herdr cell.

### Remote access: the herdr socket rides the existing tunnel

- When a remote has `herdr_socket`, `ssh_args` adds
  `-o StreamLocalBindUnlink=yes -L <runtime_dir>/herdr-<socks_port>.sock:<herdr_socket>`.
  - The file name uses the SOCKS port rather than the remote name, so it
    needs no escaping and is already unique per remote.
  - The runtime dir path can contain a space (macOS `Application Support`).
    That is fine because arguments go in as argv, never through a shell.
- If the local socket path is 100 bytes or longer, leave the forward out (the
  tunnel runs as today) and log the reason once. With
  `ExitOnForwardFailure=yes`, a forward that cannot bind would otherwise take
  lucy's whole tunnel down.
- `is_our_tunnel` (the orphan reclaim) must recognize both forms, with and
  without the herdr forward, so a force-quit WhatsUp's leftover ssh is still
  reclaimed after this change. It currently splits `ps` output on
  whitespace, and the `-L` value can contain a space (`Application Support`).
  So it must match the forward by its shape
  (`-o StreamLocalBindUnlink=yes -L ...herdr-<socks_port>.sock:/...`), not by
  exact tokens.
- When the tunnel stops for good (remote removed from the config, or WhatsUp
  quits), remove the local socket file.
- The forwarded socket gives this user on megan control of lucy's herdr. That
  is not new access, since the same user already has key-based SSH to lucy.
  The socket is `0600` (ssh's default `StreamLocalBindMask`) inside
  WhatsUp's own directory.
- My call: a forward on the existing tunnel rather than `ssh lucy herdr ...`
  on every refresh. The forward costs no new SSH login per refresh (that would
  be one every 2 s in lucy's auth log). Its lifetime and the offline state
  come from the existing tunnel supervisor, and it is about 0.14 s per query.
  I also ruled out `herdr --machine lucy`: it depends on herdr's own machine
  profile and the `herdr` binary being on WhatsUp's `PATH`.

### Config

- A new cell form, next to text, link and null:
  `{ "text": "jds", "herdr": "h1" }`.
  - `text` is required and not blank. `herdr` is the prefix: not blank, no
    whitespace.
  - Any other key is an error, as with links.
  - It renders as text plus the herdr match.
- A new optional remote field: `"herdr_socket": "/home/jds/.config/herdr/herdr.sock"`.
  It must be absolute and contain no `:` (the ssh `-L` separator). When it is
  absent, that remote's herdr cells show plain text.
- The shared config (`jds_private_config/whatsup/config.json`) then sets
  `{ "text": "jdsN", "herdr": "hN" }` on the ten stack rows (`jds` gets
  `h1`) and adds `herdr_socket` to the lucy remote.
- `/api/dashboard` sends these cells as
  `{ "type": "text", "text": "jds", "herdr": ["h1-herberth"] }`, with
  `herdr` omitted or empty when nothing matched. Clients that ignore
  `herdr` still render the plain text.

### Rollout order (lucy must not break)

This is the same constraint as the lucy-remote feature: the parser rejects
unknown fields, and lucy's WhatsUp reads the same shared config.

1. Merge the WhatsUp change and install it on megan and on lucy.
2. Only then change the shared config and pull it on lucy.

## Left to the builder

- Module layout. A new `src/herdr.rs` is the natural home for the socket
  client and the matching.
- Exact styling of the secondary label and the `+N`.
- Whether README gets a short "herdr labels" section. It should: document
  the cell form and `herdr_socket`.

## How we will know it works (E2E on the real machines)

1. Megan's dashboard: the `jds` row reads `jds/h1`, `jds4` reads
   `jds4/h4-apitb`, and the lucy-only rows have no megan section, as today.
2. Lucy's section: `jds/h1-herberth`, `jds10/h10-e2e-testing`; `jds` does not
   show `h10-...`.
3. Rename a lucy herdr workspace (the user does it, or with permission
   `herdr workspace rename`). The cell follows within a refresh or two.
4. Two matching workspaces on one machine show `+1` and the tooltip lists
   both.
5. Stop megan's herdr, or point the socket somewhere wrong: megan's cells go
   plain and nothing else changes. Kill the tunnel: lucy's cells go plain
   with the existing "offline" state, then come back by themselves.
6. A config without any herdr fields behaves exactly as before. Lucy's
   WhatsUp, reading the shared config, shows lucy's labels in its own local
   section.
7. Quit WhatsUp: the ssh process and the local `herdr-<port>.sock` are gone.

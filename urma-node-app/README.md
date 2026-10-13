# URMA Node app

Desktop UI for a local URMA Node, built with Tauri v2, Svelte 5 and TypeScript.
It polls `GET /v1/status` from the local node service every 2 s through
`urma-node-client`, backing off while the service is down.

## Connection

By default the app talks to the installed service on `127.0.0.1:28331` with its
token: Linux `/var/lib/urma-node/service.token` (group `urma-node`; join it with
`sudo usermod -aG urma-node $USER` and log in again), Windows
`%ProgramData%\URMA Node\service.token`.

A developer node overrides that: `$URMA_CONFIG`, else
`~/.config/urma/config.json`, key
`"archive": {"endpoint": "127.0.0.1:28331", "token_file": "/abs/path/token"}`,
with a private token file (mode 0600). See `urma-node/contrib/dev/local-node.md`.

## System dependencies

- Rust stable toolchain
- Node.js and pnpm
- Linux: WebKitGTK 4.1 (`webkit2gtk-4.1`, which pulls GTK 3 and libsoup 3)

## Develop

```sh
pnpm install
pnpm tauri dev
```

Frontend edits hot-reload in the open window; `src-tauri/` edits rebuild and
restart it.

## Install

```sh
./install.sh
```

Builds a release binary and installs `urma-node-app`, its icon and a
`URMA Node` desktop entry under `~/.local` (`PREFIX=/usr/local` for
system-wide). The daemon keeps the `urma-node` name.

## Window behaviour

Simple mode is a fixed-size window (min size = max size), which tiling window
managers such as dwm float automatically. Advanced mode resizes the same window
and allows manual resizing. Both scale with the monitor the window is on.

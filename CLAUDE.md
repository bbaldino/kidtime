# kidtime: project context

Kidtime tracks kids' computer time across a household's Linux machines, and will eventually
enforce limits too. It's meant to replace timekpr, which keeps each machine's usage separate
and so can't share a time budget across machines.

**Household specifics** (account names, machines, IPs, deployment targets, the state of the
streaming setup) are in `CLAUDE.local.md`. It's gitignored. Keep anything identifying out of
committed files: use placeholder names like `kid1`/`kid2` in code, tests and examples.

## Architecture

A Cargo workspace (edition 2024) with three crates:

- `crates/protocol`: the wire types shared by agent and server (`Report`, `Sample`, `UserSample`, `UserState`, `App`).
- `crates/agent`: the binary `kidtime-agent`. It runs as **root** on each kid PC, as a systemd system service.
- `crates/server`: the binary `kidtime-server`. It runs axum + rusqlite (bundled SQLite) and serves the dashboard.
  The dashboard's static files are compiled in with `include_bytes!`.

### Data flow

- Every `interval_secs` (15 by default), the agent takes a `Sample` for each tracked user:
  - their `state`;
  - the apps attributed to them;
  - `elapsed_secs`, measured on the monotonic clock (so time spent suspended never counts), capped at 2× the interval;
  - an increasing `seq`.
- It `POST`s `/api/report` with a `Bearer` token.
  - Unsent samples are queued (up to a day's worth) and sent in batches of up to 500.
  - `agent_id` is a random UUID per process, so the server can tell an agent restart apart from a retried batch.
- The server stores the samples in SQLite (`crates/server/src/db.rs`):
  - `usage(day, user, host, app_id, app_name, secs)` holds the per-host and per-app breakdowns. `app_id = ''` is the host total.
  - `activity(user, host, start, end)` holds one row per counted sample. **Daily totals are the *union* of
    intervals across hosts**, so streaming from one machine to another doesn't count twice.
  - `agents(host, agent_id, last_seq)` drops samples the server has already recorded.
- `GET /api/status` returns JSON for the dashboard:
  - each user's headline state and the host it's on;
  - live sessions for each host (a host is treated as offline after 3× its interval without a report);
  - today's and the last 7 days' totals;
  - apps today and hosts today.
- `GET /healthz` returns `ok`.
- **Plan for enforcement:** the server's *response* to `/api/report` will carry decisions
  ("12 minutes left", "lock now"), so agents keep making a single call. The business logic lives
  centrally, and agents stay simple.
- Agents don't write to a shared database: the server is needed for the dashboard anyway, rules would
  have to be duplicated in every agent, and enforcement needs a request/response. If Postgres is ever
  wanted, swap the *server's* storage (sqlx), not the agents.

### User states (`protocol::UserState`)

| State | Meaning | Counts |
|---|---|---|
| `active` | Graphical logind session in the foreground, not idle, not locked | yes |
| `streaming` | The user's Sunshine is encoding (a Moonlight client is connected) | yes |
| `stream_idle` | Something is open in the streaming session, but nobody is connected | no |
| `idle` | Foreground session with logind `IdleHint` set (GNOME marks idle after a few minutes) | no |
| `locked` | Foreground session with `LockedHint` set | no |
| `background` | Signed in, but another session is in the foreground | no |
| `offline` | No graphical session | no |

### Agent detection details (`crates/agent/src/`)

- `logind.rs`: zbus proxies for Manager, User and Session. Only `wayland`/`x11`/`mir` sessions with class `user` count.
  - The foreground session (`State == "active"`) decides the state.
  - Proxies are built with `CacheProperties::No` so every read is fresh.
- `apps.rs`: walks `/sys/fs/cgroup/user.slice/user-<uid>.slice` recursively.
  - **Desktop apps:** GNOME scopes like `app-gnome-<desktop-id>-<pid>.scope`, unescaped (`\x2d` → `-`),
    with launcher prefixes stripped. The name comes from the `.desktop` file; `NoDisplay`/`Hidden` entries are skipped.
  - **Steam games:** found by the `reaper SteamLaunch AppId=N -- <program>` command line. Only `reaper`
    carries it, but it stays alive as the game's parent. Names come from `steamapps/appmanifest_N.acf`
    in every library listed in `libraryfolders.vdf`. **Non-Steam shortcuts** (large app ids) are named
    from the binary `userdata/*/config/shortcuts.vdf` (`\x02appid\0<u32 LE>` … `\x01AppName\0<name>\0`).
    Fallback: the launched program's file name.
  - In streaming units it also notes the Steam client process (comm `steam`) as the app "Steam".
- `sunshine.rs`: **decides whether a stream is connected.**
  - Reads `drm-engine-enc` (amdgpu video-encoder time) from `/proc/<pid>/fdinfo/*` of the user's
    `sunshine` process(es). Needs root.
  - Sunshine opens **new DRM clients per stream** and closes them afterwards, so it tracks a counter per
    `drm-client-id`. A new client that has encoded, or growth in an existing one, means Connected.
  - Verified on real hardware: while paused, only a long-lived client with no enc counter → `stream_idle`;
    while streaming, two new clients appeared with enc time rising → `streaming`.
- `sway.rs`: a minimal Sway IPC client (`i3-ipc` magic, `GET_TREE` = 4) that finds the **focused window** in
  the streaming session's Sway.
  - Proton windows of non-Steam shortcuts all have class `steam_app_default`, so the **window title** is the name.
  - Class `steam` → Steam; `steam_app_<N>` → that Steam game.
  - Untitled focused windows are ignored (Wine's hidden `explorer.exe` desktop).
  - This names games launched from other launchers (e.g. a game launched from Battle.net shows as the game,
    not Battle.net), and launchers left running in the background with no window don't count.
- `main.rs`: config, the sampling loop, and the decision logic, including the streaming override.
  - If the logind state doesn't count, the Sunshine check decides: `streaming` or `stream_idle`.
  - Streamed apps come from Sway focus when available, otherwise from the process scan.
  - If the encoder can't be read (not root), a running game counts as streaming, so real play is never missed.
  - `--dump` takes two samples 3s apart (so the encoder check has something to compare) and prints the second.

Agent config: see `deploy/agent.toml.example`. A streaming host sets `streaming_units` and
`streaming_sway_socket`; other PCs only need `server_url`, `token` and `users`.

### Server, dashboard and container

- Config comes from an optional TOML file (`--config`, or `/etc/kidtime/server.toml` if it exists),
  overridden by the environment: `KIDTIME_AGENT_TOKEN` (required), `KIDTIME_LISTEN`
  (default `0.0.0.0:8470`), `KIDTIME_DB`. Shuts down cleanly on SIGTERM.
- Days are counted in the server's local time zone, so `TZ` for the container must match the kids' PCs.
- The dashboard (`crates/server/static/`) is vanilla JS with no build step. It polls `/api/status` every 10s.
  - Designed for phones first, with two columns on wide screens and separate light and dark colors.
  - Each kid gets a card: a status pill (icon plus label, never color alone), a big "today" number,
    "last 7 days", chips for what's running now, per-app bars (top 6 plus "N others"), a 7-day column chart
    with hover/tap tooltips, and time by computer.
  - Charts follow the dataviz skill's guidance: single series in blue `#2a78d6`/`#3987e5`, and status
    colors used only for the status pill.
  - PWA: `manifest.webmanifest`, a network-first `sw.js`, and PNG icons (drawn with ImageMagick primitives,
    since its policy on the dev machine blocks SVG input).
  - **Installing it on a phone needs HTTPS** (a reverse proxy in front of :8470).
- `Dockerfile`: multi-stage, `rust:1-bookworm` → `gcr.io/distroless/cc-debian12:nonroot` (uid 65532).
  `/data` is created owned by 65532 so a fresh named volume is writable. Also `.dockerignore`.
- `compose.yaml` runs the published image `ghcr.io/bbaldino/kidtime:<X.Y.Z>` (always an exact version).
  - Data is a bind mount, `./data`, so host backups of the compose directory include the database.
  - **One-time step on a new host:** `chown 65532:65532 data`. Docker creates a bind directory owned by
    root, and the server then fails with "unable to open database file". A one-shot init service would
    fix it too, but Komodo reports a stack with an exited container as not running.
  - The port is published (`8470:8470`): the reverse proxy forwards to the host's IP, not to a Docker network.
  - Built and run locally with Docker (2026-10-01): 28.8 MB image, about 2.4 MiB of memory when idle.
- **Releases** (`.github/workflows/`): release-please keeps one version for the whole repo in `version.txt`,
  from conventional commits (`feat:`/`fix:` cut a release). The tag `vX.Y.Z` triggers `publish-image.yml`,
  which pushes the image to GHCR. Both workflows run `test.yml` first (nightly fmt, clippy `-D warnings`, tests).
  - The crate versions in `Cargo.toml` are not the release version. Don't read `CARGO_PKG_VERSION` as it.
  - Integrate with squash or rebase, never a merge commit: release-please only reads first-parent history.
  - Needs the repo secret `RELEASE_BOT_TOKEN`. With the default token, the tag would not trigger the image build.
- `deploy/install-agent.sh` (sudo) installs or upgrades the agent on a kid PC: it asks for the token,
  checks the server and the token, writes the config, installs the binary and unit, and prints the
  agent's view. `KIDTIME_INSTALL_ROOT=<dir>` installs under a fake root and skips systemd, for testing.
- `deploy/` has systemd units for both binaries (the server unit: `DynamicUser`, `StateDirectory`,
  `LoadCredential` for the config), plus example configs.

### Tests

Run `cargo test` (all pass) and `cargo clippy --all-targets` (clean). Tests cover:
- scope-name parsing;
- desktop entries;
- VDF and binary shortcuts;
- the stream lifecycle (paused → start → ongoing → stop → resume);
- Sway focus and app naming;
- server: samples counted once (retries and restarts), idle not counted, and overlapping hosts counted once.

## Dev helpers (`dev/`)

- `agent.dev.toml.example`: copy it to `agent.dev.toml` (gitignored) and fill in real accounts.
  It uses a 5s interval and points at a local server.
- `stream-snapshot.sh <user>` (sudo): the agent's view of the user, the Sway windows with focus, and the top
  GPU users in their streaming session.
- `stream-audio-check.sh <user>` (sudo): mute and volume of sinks, streams and capture, plus the real
  signal level Sunshine records.
- `stuck-key-check.sh <user> [secs]`: logs W press/release on the physical keyboard and on the user's
  Sunshine passthrough keyboard.
- Local run:
  ```
  cargo build
  printf 'listen="127.0.0.1:8470"\ndb="/tmp/k.db"\nagent_token="dev"\n' > /tmp/s.toml
  target/debug/kidtime-server --config /tmp/s.toml &
  target/debug/kidtime-agent --config dev/agent.dev.toml
  ```
  Without root, other users' homes, Steam names and fdinfo are unreadable, so the agent falls back to its
  defaults. Use `sudo target/debug/kidtime-agent --config dev/agent.dev.toml --dump` for the real view.
- Gotcha: `pkill -f <pattern>` from the Bash tool can match its own shell. Use `pkill -x <name>`.

## Status

- Verified on a real streaming host: session states, desktop app names, Steam and shortcut names,
  stream connected vs idle, and Sway focus naming for streams.
- Not yet tried for real: cross-host de-duplication (needs a second PC reporting), the release
  workflows on GitHub, and the PWA over HTTPS.
- Deployed 2026-10-02: the server container (image `0.1.0`) and the agent on both kid PCs, all reporting.
  timekpr still enforces.

## Next steps

1. Done: the image is published, the server runs, and the agents are installed (see `CLAUDE.local.md`).
2. Install the PWA on a phone from the HTTPS address.
3. Run for a few days and check the numbers against reality.
4. Then:
   - **dashboard auth** (needed before any controls; the dashboard is read-only and unauthenticated now);
   - **enforcement**: allowed hours per weekday, daily budgets across machines (union time), blackout dates,
     "+30 min" / "lock now" from the phone, warnings via `notify-send` into the kid's session, locking via
     logind `Session.Lock()`, and offline fallback in the agent. Then retire timekpr.
   - a **GNOME Shell extension** reporting the focused window over D-Bus. Desktop app time is currently
     "running while active" rather than focus. This also fixes a known gap: **non-Steam games on a GNOME
     desktop (e.g. Battle.net games) aren't detected at all** — no app scope and no SteamLaunch.
     Install it system-wide and lock it on with dconf.
   - Possibly: idle detection for an *unattended but connected* stream (input activity on the passthrough
     devices; gamepads need care), and retention/pruning of `activity` rows.

## Design decisions to keep

- The rules live on the server; agents report and carry out decisions.
- Undercount rather than overcount, except where a missed play session is worse. For example, an unknown
  encoder state with a game running counts as streaming.
- Daily totals are the union of intervals across hosts; per-host and per-app breakdowns stay as plain sums.
- Window titles are only used to name apps in streaming sessions (mostly games). Think about privacy before
  recording titles on desktops (e.g. browser page titles).
- Tracking is per account. If kids use a parent's account, that time isn't attributed to them.

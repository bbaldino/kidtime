# kidtime

Tracks how much the kids use their computers, and which apps and games they use,
across machines. The results show up on a web dashboard that works on phones.

- **`kidtime-agent`** runs as root on each kid computer. Every 15 seconds it
  records each tracked user's state and running apps, and sends that to the server.
- **`kidtime-server`** stores usage in SQLite and serves the dashboard.

This version tracks usage and holds the rules, but it doesn't enforce anything, so timekpr keeps
handling schedules for now.

## What counts as usage

The agent asks systemd-logind for each user's graphical sessions:

| State | Meaning | Counted |
|---|---|---|
| `active` | Desktop session in the foreground, not idle, not locked | yes |
| `streaming` | A Steam game running inside a `streaming_units` unit (e.g. Sunshine), with a client connected | yes |
| `stream_idle` | That game is running, but nobody is connected to the stream | no |
| `idle` | Foreground session, but GNOME has marked it idle | no |
| `locked` | Foreground session behind the lock screen | no |
| `background` | Signed in, but someone else's session is in front | no |
| `offline` | No graphical session | no |

Elapsed time comes from the monotonic clock, so time the machine spends suspended is never counted.

## Per-app tracking

- **Desktop apps:** GNOME launches each app in its own systemd scope
  (`app-gnome-org.freecad.FreeCAD-1234.scope`). The agent turns the scope name
  into the app's name using its `.desktop` file. Apps hidden from menus
  (`NoDisplay`) are skipped.
- **Steam games:** these run inside Steam's cgroup. The agent finds them by the
  `reaper SteamLaunch AppId=N` command line and names them from the app
  manifest. Non-Steam shortcuts (e.g. Lutris games) are named from
  `shortcuts.vdf`.

**Streaming sessions** use the focused window of the session's Sway compositor
(`streaming_sway_socket`), named by its title. A game launched from Battle.net shows
up as "Heroes of the Storm", not as Battle.net, and launchers left running in the
background don't count.

For desktop sessions, app time currently means "running while the user was active". It isn't focus
time: if Chrome and a game are both open for an hour, each gets the hour.
Focus-based tracking needs a small GNOME Shell extension, which is planned.

## Build

```sh
cargo build --release
# target/release/kidtime-agent, target/release/kidtime-server
```

Check what the agent sees without contacting a server. Run it as root, or names
from other users' home directories won't resolve:

```sh
sudo ./target/release/kidtime-agent --config deploy/agent.toml.example --dump
```

## Install

**Server as a container** (e.g. on the Proxmox box):

```sh
echo "KIDTIME_AGENT_TOKEN=$(openssl rand -hex 24)" > .env
echo "TZ=America/New_York" >> .env    # your time zone
mkdir -p data && sudo chown 65532:65532 data   # the server runs as uid 65532
docker compose up -d                  # pulls ghcr.io/bbaldino/kidtime
curl http://localhost:8470/healthz    # -> ok
```

It listens on 8470, keeps its database in `./data` next to `compose.yaml`, and
counts days in the `TZ` from `.env`, which must match the kids' computers.
Configuration comes from environment variables: `KIDTIME_AGENT_TOKEN` (required),
`KIDTIME_LISTEN`, `KIDTIME_DB`, and the login check's `KIDTIME_ACCESS_TEAM` and
`KIDTIME_ACCESS_AUD` (see "Who can see and change things").

**Server as a systemd service** (alternative, without containers):

```sh
sudo install -m755 target/release/kidtime-server /usr/local/bin/
sudo install -Dm600 deploy/server.toml.example /etc/kidtime/server.toml   # set agent_token
sudo install -m644 deploy/kidtime-server.service /etc/systemd/system/
sudo systemctl enable --now kidtime-server
sudo firewall-cmd --permanent --add-port=8470/tcp && sudo firewall-cmd --reload
```

**Agent** (every kid computer):

```sh
cargo build --release -p agent
sudo deploy/install-agent.sh --server http://SERVER-HOST:8470 --users kid1,kid2
```

The script asks for the agent token, checks the server and the token, writes
`/etc/kidtime/agent.toml` (mode 600), installs the binary and the systemd unit, and
shows what the agent sees. Add `--streaming` on a machine that hosts Sunshine
streaming sessions, and `--host NAME` if the machine's hostname is unset. Run it again
with no options to upgrade the binary. See `deploy/agent.toml.example` for all settings.

If the server can't be reached, the agent queues up to a day of samples and sends them later.

## Dashboard on a phone

Open `http://SERVER-HOST:8470` in a mobile browser. It works over plain HTTP, but only while the
login check is off: with it on, the dashboard answers only through the reverse proxy that does the
login (see "Who can see and change things").

Installing it as an app (a PWA) needs **HTTPS**, because browsers only allow
service workers on secure origins. The easiest options on a home network:

- **Tailscale:** `tailscale serve --bg 8470` gives it a `https://<machine>.<tailnet>.ts.net`
  address with a valid certificate. This also works away from home.
- **Caddy** as a reverse proxy, using a real domain and a DNS challenge.

After that, use "Add to Home Screen" (iOS Safari) or "Install app" (Android Chrome).

## Rules

The dashboard has three tabs.

- **Today** shows each kid's usage, what the rules say right now, and a log of what kidtime would have done.
  Nothing is enforced on the computers yet.
- **Rules** sets, per kid and per weekday, the allowed hours and a games budget, plus one-off blackouts.
  "Copy this week to" replaces another kid's whole week with the one shown (blackouts are not copied).
- **Apps** lists every app seen and its category. Games and uncategorised apps use the games budget; move apps
  you don't care about to **Ignored** to hide them from Today and stop them counting.

## Who can see and change things

Without the login check, anyone who can reach the server can read and change everything. To turn it on,
put the server behind a reverse proxy that authenticates people and adds a signed token to each request
(Cloudflare Access does), and set `KIDTIME_ACCESS_TEAM` and `KIDTIME_ACCESS_AUD`. The server then verifies
that token itself, so reaching its port directly doesn't get around the login. The agents' endpoint and
`/healthz` stay open; the agents use their own token.

## Known gaps / next steps

- **Streaming detection** checks whether the user's `sunshine` process is using
  the GPU video encoder: the `drm-engine-enc` counter in `/proc/<pid>/fdinfo`
  goes up only while it's encoding a stream. This needs root. If the counter
  can't be read, streamed games count as usage.
- **Focus tracking:** a GNOME Shell extension that reports the focused app over D-Bus.
- **Enforcement:** the rules are stored and evaluated, but nothing acts on them yet. Next: closing apps
  and locking sessions, and "+30 min" from the dashboard, to replace timekpr.

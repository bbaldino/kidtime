# kidtime

Tracks how much the kids use their computers, and which apps and games they use,
across machines. The results show up on a web dashboard that works on phones.

- **`kidtime-agent`** runs as root on each kid computer. Every 15 seconds it
  records each tracked user's state and running apps, and sends that to the server.
- **`kidtime-server`** stores usage in SQLite and serves the dashboard.

This version only tracks usage. It doesn't enforce anything, so timekpr keeps
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
docker compose up -d                  # pulls ghcr.io/bbaldino/kidtime
curl http://localhost:8470/healthz    # -> ok
```

It listens on 8470, keeps its database in `./data` next to `compose.yaml`, and
counts days in the `TZ` from `.env`, which must match the kids' computers.
Configuration comes from environment variables: `KIDTIME_AGENT_TOKEN` (required),
`KIDTIME_LISTEN`, `KIDTIME_DB`.

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
sudo install -m755 target/release/kidtime-agent /usr/local/bin/
sudo install -Dm600 deploy/agent.toml.example /etc/kidtime/agent.toml     # set server_url, token, users
sudo install -m644 deploy/kidtime-agent.service /etc/systemd/system/
sudo systemctl enable --now kidtime-agent
```

If the server can't be reached, the agent queues up to a day of samples and sends them later.

## Dashboard on a phone

Open `http://SERVER-HOST:8470` in a mobile browser. It works over plain HTTP.

Installing it as an app (a PWA) needs **HTTPS**, because browsers only allow
service workers on secure origins. The easiest options on a home network:

- **Tailscale:** `tailscale serve --bg 8470` gives it a `https://<machine>.<tailnet>.ts.net`
  address with a valid certificate. This also works away from home.
- **Caddy** as a reverse proxy, using a real domain and a DNS challenge.

After that, use "Add to Home Screen" (iOS Safari) or "Install app" (Android Chrome).

## Known gaps / next steps

- **No dashboard login yet.** It's read-only for now. Add auth before adding controls.
- **Streaming detection** checks whether the user's `sunshine` process is using
  the GPU video encoder: the `drm-engine-enc` counter in `/proc/<pid>/fdinfo`
  goes up only while it's encoding a stream. This needs root. If the counter
  can't be read, streamed games count as usage.
- **Focus tracking:** a GNOME Shell extension that reports the focused app over D-Bus.
- **Enforcement:** schedules, budgets, locking, and "+30 min" from the dashboard, to replace timekpr.

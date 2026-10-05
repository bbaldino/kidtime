# kidtime

Tracks how much the kids use their computers, and which apps and games they use,
across machines. The results show up on a web dashboard that works on phones.

- **`kidtime-agent`** runs as root on each kid computer. Every 15 seconds it
  records each tracked user's state and running apps, and sends that to the server.
- **`kidtime-server`** stores usage in SQLite and serves the dashboard.

It also enforces the rules, per kid, once you switch Enforce on for that kid (see "Enforcement").
Until then it only reports, so timekpr can keep handling schedules.

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
with no options to upgrade the binary. `--stop-timekpr` and `--uninstall` are described under "Enforcement".
See `deploy/agent.toml.example` for all settings.

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

- **Today** shows each kid's usage, what the rules say right now, and a log of what kidtime did (or would have
  done, while Enforce is off).
  It also has **Send a message**: a note to one kid or everyone, shown as a notification on the computer
  they're using within about 15 seconds (or in their stream), and dropped if they aren't at a computer within 10 minutes.
  Each kid's card has a **timer**: 15, 30 or 60 minutes (or any length up to 4 hours), then either the computer
  locks or games close, until you press Allow again, start another timer, or midnight. Timers only ever end things
  sooner; allowed hours and budgets still apply. The usual 10/5/1-minute warnings count down to it. Like all rules, a timer is enforced from the PCs' last copy
  of the rules if the server can't be reached, so Cancel or Allow again only takes effect once a PC hears from it.
- **Rules** has an **Enforce** switch per kid, and sets, per kid and per weekday, the allowed hours and a games
  budget, plus one-off blackouts.
  "Copy this week to" replaces another kid's whole week with the one shown (blackouts are not copied).
- **Apps** lists every app seen and its category. Games and uncategorised apps use the games budget; move apps
  you don't care about to **Ignored** to hide them from Today and stop them counting.

## Enforcement

Each kid has an **Enforce** switch on the Rules tab. It is off by default, and while it is off the agents only
report. Turning it on is picked up within one report cycle.

With it on, each agent decides locally every 5 seconds, from the latest rules the server sent. So it
keeps enforcing if the server is down or the PC is offline (the rules are saved on the PC). What happens:

- **Warnings** at 10, 5 and 1 minute before allowed hours end or a blackout starts, and before the games
  budget runs out. They show on the desktop and in a Moonlight stream.
- **At the end of allowed time or a blackout:** the session is locked and the account's login is disabled
  (`usermod -L`), so the lock can't be unlocked with a password. It is re-enabled when allowed time
  returns. Blocked sessions are checked every second and locked again if the kid unlocks them from inside
  (which logind allows); the dashboard shows that it happened. An account that has no password can't have
  its login disabled; it gets the session lock only, and an error on the dashboard. An account that was
  already locked is never unlocked, and that is shown on the dashboard too. If a parent runs `usermod -L` on
  a kid while kidtime is blocking them, kidtime unlocks the account when the block ends.
- **Streams are cut:** traffic to the kid's Sunshine ports is dropped (an nftables table, `inet kidtime`),
  so Moonlight disconnects within seconds and can't reconnect. Afterwards the stream can be resumed. The
  ports come from `sunshine_ports` in the agent config if set (recommended: the kid can edit their own
  `sunshine.conf`), otherwise from the kid's `sunshine.conf`, otherwise Sunshine's default. The rules are
  checked and repaired every 5 seconds while the kid is blocked.
- **Games budget used up:** apps in the Games category are closed (SIGTERM, then SIGKILL after 10 seconds)
  and kept closed. Apps that aren't categorised count toward the budget but are never closed; they show on
  the dashboard instead.
- **Login screen:** while anyone is blocked, a banner shows one line per blocked kid.

The agent keeps what it changed in `/var/lib/kidtime/agent-state.json` (mode 600, `state_file` in the agent
config overrides it), so blocks survive a reboot or an agent restart and are undone later.
The dashboard shows what enforcement did, and any action that failed (it is retried every loop).

**Retiring timekpr:** once you trust kidtime, run `sudo deploy/install-agent.sh --stop-timekpr` on each PC.

**Recovery.** If the agent is broken, or you need a kid unlocked right now. If the server is reachable, the
quickest way is to switch Enforce off for that kid on the Rules tab and wait 15 seconds: everything is released
and the agent keeps running. Otherwise stop the agent first. While it runs, it undoes a manual `usermod -U`
within seconds, and after a reboot it starts again and re-blocks from its saved rules.

```sh
sudo systemctl disable --now kidtime-agent
sudo /usr/local/bin/kidtime-agent --config /etc/kidtime/agent.toml --release-all   # undo everything it recorded
sudo usermod -U <kid>                  # re-enable a login by hand
sudo nft delete table inet kidtime     # remove the stream block by hand
sudo rm -f /etc/dconf/db/gdm.d/90-kidtime && sudo dconf update   # clear the login-screen banner
```

`--release-all` exits non-zero if anything remains. It works from the state file alone if the config can't be
read. `sudo deploy/install-agent.sh --uninstall` stops the agent, runs `--release-all`, and only if that
succeeds removes the unit, binary, config, state and the login-screen banner settings (the GDM profile
override only if the install script wrote it). It removes nothing if the agent won't stop, or if the state file
exists but the binary is gone.

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
- **Enforcement:** next are "+30 min" and "lock now" from the dashboard, and phone notifications.

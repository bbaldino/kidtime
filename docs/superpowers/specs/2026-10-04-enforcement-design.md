# Enforcement on the PCs: design

Date: 2026-10-04. Status: agreed in conversation, awaiting review of this document.

## Purpose

Kidtime decides, on the server, whether each kid may use the computer and how much games time is left
(see `2026-10-02-rules-and-management-design.md`). Nothing acts on that yet: timekpr still enforces,
separately on each PC, with its own rules. This piece makes the kidtime agent carry out the rules on
every PC, so timekpr can be removed.

Success: with Enforce on for a kid, outside allowed hours or during a blackout their sessions are locked
and their account can't log in or unlock; their streams are cut; when the games budget runs out, games
close and stay closed; they are warned 10, 5 and 1 minute before each of those; all of it keeps working
while the server is unreachable; and a parent can switch it off per kid from the phone.

## Decisions made

| Question | Decision |
|---|---|
| What "locked" means | Lock the screen and keep it locked. The session and its programs keep running. |
| Unlocking while blocked | Not possible: password login for the account is disabled while blocked (`usermod -L`), restored when allowed (`usermod -U`). |
| Streams | The PC the kid streams *to* locks their session there like any other. The gaming PC additionally cuts the stream by dropping network traffic to that kid's Sunshine ports while they are blocked. Sunshine keeps running and the session stays resumable. |
| Warnings | 10, 5 and 1 minute before a lock or before games close, on the computer the kid is using, including inside a stream. |
| Games budget used up | Close apps in the Games category and keep them closed. Uncategorised apps keep counting but are not closed; they are flagged on the dashboard. |
| Games behind a lock | Keep running indefinitely. (A "close games after N minutes of lock" option can be added later.) |
| Server unreachable | Keep enforcing from the last rules received. A PC that has never heard from the server enforces nothing. |
| Midnight | The snapshot carries the account's rule for all seven weekdays, so a block in force at midnight holds, and a lock at midnight is warned about, even if the server can't be reached then. |
| Switching it on | A per-kid Enforce switch on the Rules tab, off by default. |
| Where the decision is made | The agent always decides locally, with the same decision function as the server, from the latest snapshot the server sent. |
| timekpr | Runs alongside until the parent is satisfied; the install script gains an option to stop it. |
| Message at the login screen | A login-screen banner on each PC, one line per enforced kid who is blocked, seen by everyone. |

## Spike findings (2026-10-04, on the streaming host)

| Check | Result |
|---|---|
| A bar inside the streaming Sway session: `swaynag --layer overlay --edge top` | Shows in Moonlight over Steam. Over a fullscreen game: still to confirm (real-machine checklist). |
| Who receives notifications on a kid's session bus | GNOME Shell (`gjs`). The desktop and the streaming session share one bus, so plain notifications never reach a stream. |
| `notify-send` | Not installed. Notifications are sent over D-Bus directly (`org.freedesktop.Notifications.Notify`), as the kid. Urgent notifications show on the desktop. |
| Text of a notification on the GNOME lock screen | Only the icon shows, even with a hidden desktop entry and the per-app "details on lock screen" setting. Left as icon only. |
| Login screen banner (GDM 50) | GDM 50 runs each login screen as a temporary user (`gdm-greeter`, dynamic uid), not `gdm`. Writing `org.gnome.login-screen banner-message-text`/`-enable` through that user's session bus (found via the `gnome-shell --mode=gdm` process) shows the banner on the running login screen, below the password prompt after a user is picked. Plain text only (markup shows literally), small, line breaks allowed. **Used** for status lines. |
| Sunshine's web API (version 2025.924) | No per-client disconnect. `POST /api/apps/close` (needs `Content-Type: application/json`) ends the app session for every client, removes Resume, and runs the app's undo commands; for the Steam Big Picture app that includes `stop-steam.sh`, which closes Steam. Not used. |
| Dropping traffic to a kid's Sunshine ports (nftables, not from loopback) | Moonlight shows an error after about 5 seconds and can't reconnect. After the rule is removed, reconnecting offers Resume and the session is intact. **Used.** |
| `loginctl lock-session` on a kid's background session | Locks it; switching to the kid shows the lock screen. The kid can unlock with their password, hence the login disabling. |

## Protocol

The report response, today an empty 204, becomes 200 with a JSON body:

```
ReportResponse {
  server_time: NaiveDateTime,           // server's local wall-clock time when the response was built
  accounts: Vec<AccountSnapshot>,       // one per account named in the report's last sample
}
AccountSnapshot {
  user: String,
  enforce: bool,
  week: Vec<DayRule>,                   // the account's rule for each weekday, Monday first
  blackouts: Vec<BlackoutSpan>,         // blackouts that apply to this account and haven't ended
  used_secs: BTreeMap<CategoryId, i64>, // today's merged, cross-PC category time
  games: Vec<String>,                   // app ids in the Games category (the ones the agent may close)
  ignored: Vec<String>,                 // app ids in the Ignored category (never counted)
  for_day: NaiveDate,                   // the day `used_secs` belongs to
}
```

The rule types (`DayRule`, `Stretch`, `BlackoutSpan`, `CategoryId`) and `decide` move from the server
crate into `protocol`, unchanged, so both sides run the same function. The server keeps storage, the
event log and validation.

Compatibility: an old agent ignores the response body. A new agent receiving the old empty 204 treats it as
"no snapshot" and enforces nothing. Either side can be upgraded first.

On a successful (2xx) response the agent:
- empty body: drops every snapshot, so the next tick releases everything (a rollback to an old server stops
  enforcement instead of freezing the last rules forever);
- a body that parses: replaces the snapshots, and drops the snapshot of any account the response leaves out;
- a non-empty body that doesn't parse: keeps everything, with a warning in the log.

## Agent

### State

- The latest snapshot per account, held in memory and written to `/var/lib/kidtime/agent-state.json`
  (mode 600) whenever it changes, so a reboot during an outage still enforces.
- Per account: seconds of counted (non-Ignored) app time on this PC since the snapshot arrived.
- The offset between the server's clock and the local clock, from `server_time`.
- The set of accounts whose login the agent itself disabled, and the set of accounts whose Sunshine ports
  it blocked, so it can undo exactly what it did.
- Which warnings have been shown for which upcoming event, so each fires once.

Local counting uses the snapshot's two app lists: an app in `ignored` never counts; any other app counts toward
the games budget, as on the server. Only apps in `games` are ever closed.

### Loop

A second tokio task alongside the existing sampling loop, every 5 seconds:

1. For each account with a snapshot and `enforce = true`, compute
   `decide(day, blackouts, used + local, now + offset)`. The rule for the current weekday comes from
   `week`; `used_secs` counts only when `for_day` is today. The whole week travels so that a block in force
   at midnight holds, and a lock at midnight is warned about, even when the server can't be reached then. Blackouts always apply, since they are
   absolute times.
2. Compare with what the agent did last time and act (below).
3. Accounts with `enforce = false` or no snapshot: undo anything the agent did to them (unlock login,
   unblock ports) and do nothing else.

### Actions

Each action is behind a trait so the loop can be tested with fakes:

```
trait Session  { fn lock(&self, user) ; fn disable_login(&self, user) ; fn enable_login(&self, user) ; }
trait Notifier { fn desktop(&self, user, text) ; fn stream(&self, user, text) ; }
trait Apps     { fn running(&self, user) -> Vec<RunningApp> ; fn close(&self, app) ; }
trait Streams  { fn block(&self, user) ; fn unblock(&self, user) ; }
trait Banner   { fn set(&self, lines: &[String]) ; }  // empty = off
```

- **Warnings.** When a lock (computer blocked at `next_change`) or a budget running out (at the current rate:
  `left_secs` with at least one counted app running) is 10, 5 or 1 minute away, send once:
  - desktop: `Notify` on the kid's session bus, urgency critical, app name "Kidtime", sent as the kid
    (`runuser -u <kid> -- gdbus call …`, or an equivalent that drops to the kid's uid);
  - stream, on a host with `streaming_sway_socket`: `swaymsg -s <socket> exec "swaynag --layer overlay
    --edge top -t warning -m '<text>'"`, as the kid.
  Texts: "10 minutes left today: the computer locks at 9:00pm", "5 minutes of games left today",
  "1 minute left: save your game". A warning never delays the action it announces. 00:00 reads "midnight"
  ("locks at midnight", "until midnight"); a midnight more than a day away keeps the day ("Thu 12:00am").
- **Lock.** When the decision becomes blocked: disable login (`usermod -L <kid>`, recorded), `loginctl
  lock-session` each graphical session of the kid on this PC, then send the notification "Computer time is
  over until <time>" (after the locks, so nothing delays them). The record is written to the state file
  (atomically, fsynced, mode 600) *before* `usermod -L` runs; if that write fails, login is not disabled (the
  session lock still applies), the dashboard shows "couldn't save state; not disabling login", and it is retried
  next tick. So a crash or power loss can never leave a disabled login that the agent doesn't know it disabled.
  When allowed again: `usermod -U <kid>` (only if the agent recorded disabling it).
  An account that was already password-locked before the agent acted is never unlocked by the agent; that, and
  an account with no password (session lock only), are shown on the dashboard once per agent run. While an
  enforced account is allowed, a disabled login the agent didn't record is reported once ("login for kid1 is
  disabled but kidtime didn't do it; if it should be enabled run `sudo usermod -U kid1`"), never undone.
- **Re-lock.** logind lets a session's owner unlock it, and GNOME clears the lock when asked, so a kid can
  unlock their own session from inside it (`loginctl unlock-session` in a loop). Every second, for each blocked
  account, the agent reads its graphical sessions and locks any whose `LockedHint` is false (the 5-second tick
  does the same). A session that was locked and is found unlocked counts as an unlock by the kid; the dashboard
  shows "kid1 unlocked the session while blocked", at most once a minute per account. **Open question:**
  escalating after repeated unlocks (terminating the session) is not done.
- **Login-screen banner.** Whenever the set of blocked, enforced accounts on this PC changes, set the login
  screen's banner to one line per blocked kid ("kid1: computer time is over until 6:15am"; "kid2: blackout until
  Sat 9:00am: grounded"), or turn it off when nobody is blocked. Written live to the running login screen: find
  the `gnome-shell --mode=gdm` process, and as its uid run
  `gsettings set org.gnome.login-screen banner-message-text/-enable` over its session bus. A login screen that
  starts later reads the same values from the GDM system settings file (`/etc/dconf/db/gdm.d/90-kidtime`,
  rewritten with `dconf update`), which the install script enables with a profile override
  (`/etc/dconf/profile/gdm`: `user-db:user`, `system-db:gdm`, then the stock `file-db` line).
- **Cut the stream.** On a host with `streaming_units`, while the kid is blocked: add nftables rules (table
  `inet kidtime`, input hook, priority -10) dropping TCP and UDP to the kid's Sunshine ports
  (`base-5`…`base+21`) from any interface except loopback. The base comes from the agent config's
  `sunshine_ports` table (`{ kid1 = 48189 }`) first, since the kid can edit their own sunshine.conf; then
  `~kid/.config/sunshine/sunshine.conf` (`port = N`); then 47989. The source is logged once per kid. Every
  blocked tick the agent checks that both the TCP and the UDP rule exist with the right range, deletes the
  kid's rules with a wrong range, and adds what is missing, so the block survives a reboot, a manual rule
  removal or a port change. Remove the rules when allowed. The agent owns the whole `inet kidtime` table.
- **Close games.** While the Games budget is used up: for each running app of the kid whose id is in `games`,
  close it: a stream window through Sway (`[pid=…] kill`), otherwise SIGTERM to its processes (a Steam game's
  process tree under `reaper`, a desktop app's GNOME scope), then SIGKILL after 10 seconds. Repeat every 5
  seconds. Uncategorised apps still running are listed in the next report (`Sample` gains
  `overrun: Vec<String>` per user) so the dashboard can flag them.

The app scanner already finds these processes to name them; it returns their pids (and the Sway window, for
streams) alongside the `App`.

### Start-up and shutdown

- At start: load the state file (ignore it, with an error logged, if unreadable), recreate the nftables
  table from the recorded blocks, and re-enable login for any recorded account whose snapshot now allows it
  or has `enforce = false`.
- An account taken out of the agent config is released on the next tick (login re-enabled, stream unblocked)
  and its snapshot dropped.
- On SIGTERM: leave locks and blocks in place (the agent is being restarted or the PC shut down; the next
  start reconciles). `kidtime-agent --release-all` (used by uninstall) unlocks every recorded account and
  removes the nftables table. It works from the state file alone (at its default path) when the config can't
  be read, and exits non-zero if anything remains.

### Clock

If the server's clock and the local clock differ by more than 5 minutes, the agent decides with the server's
time (local time plus the recorded offset) and logs a warning once per hour.

### Systemd unit

`ProtectSystem=strict` stays, with `ReadWritePaths=/etc /var/lib/kidtime` (shadow files for `usermod`, the
GDM settings file, the state file). The unit gains `StateDirectory=kidtime`. nftables needs `CAP_NET_ADMIN`, which root has.

## Server

- `account_settings(user, enforce)` table; `PUT /api/accounts/{user}/enforce {enforce: bool}`.
- The report handler builds the response from the same data the decision uses.
- Events record whether the account was enforced and which host reported: kinds `locked`, `closed`,
  `allowed` keep their meaning; the dashboard words them "Locked…" when enforced, "Would have…" when not.
- Status gains per user: `enforce`, and `overrun` (uncategorised apps reported running after the budget ran
  out today).

## Dashboard

- Rules tab: an Enforce switch at the top of each kid's week, with "Off: kidtime only shows what it would do.
  On: the PCs lock, close games and cut streams."
- Today card: the decision line without "Would be" when enforced; a line for overrun apps, linking to Apps.
- Log: wording by the event's `enforced` flag.

## Install script

- Installs the new unit (state directory, write paths).
- `--stop-timekpr`: stops and disables timekpr's service on that PC.
- Writes the GDM profile override so later login screens read kidtime's banner settings.
- `--uninstall`: stops the agent first, runs `kidtime-agent --release-all`, then removes the unit, binary, config
  and state, plus `/etc/dconf/db/gdm.d/90-kidtime` and the GDM profile override (only if its first line is
  `# written by kidtime`, which the script writes), then `dconf update`; it removes nothing if the release fails
  or the agent won't stop.

## Error handling

- Hangs: every command the agent runs is killed (with its process group) and reaped after 5 seconds, and
  reported as timed out; `gdbus call` also gets `--timeout 5`. The streaming Sway's IPC gets 2 seconds for the
  whole exchange and replies over 16 MiB are refused. Files the kid controls (sunshine.conf, `.desktop` files,
  Steam manifests, `libraryfolders.vdf`, `shortcuts.vdf`) are opened non-blocking, must be regular files, and
  are read up to 1 MiB (16 MiB for `shortcuts.vdf`). A process owned by a tracked account is never taken for
  a login screen.
- Closing games signals a pid only if `/proc/<pid>/status` shows the kid's uid as its real uid: a stream
  window's pid comes from the kid's own Sway (for Xwayland, from a property the client sets).

- Server unreachable: enforce from the saved snapshot. The dashboard shows the host offline as now. Time counted
  locally since the last snapshot is lost if the agent restarts while the server is unreachable (an accepted undercount).
- A lock, login change, firewall change or close fails: log it, retry on the next loop, and include it in the
  next report (`Sample` gains `errors: Vec<String>`), shown on the dashboard.
- A notification fails: logged; enforcement proceeds on time.
- Recovery if the agent stops for good while a kid is blocked: `sudo usermod -U <kid>` and
  `sudo nft delete table inet kidtime`, in the README.

## Testing

- `decide` keeps its tests in `protocol`.
- The enforcement loop, with fake actions and a fake clock: warnings once each at 10/5/1; lock, login
  disable, re-lock while blocked, re-enable when allowed; an account already locked by someone else is never
  unlocked; stream block on and off; games closed and re-closed, uncategorised reported not closed; offline
  counting adds to the snapshot's figure; a snapshot from yesterday; clock offset; `enforce` off undoes
  everything; start-up reconciliation from a state file; banner lines for zero, one and two blocked kids.
- Server: the response snapshot (contents, only accounts in the report, Ignored excluded), the Enforce switch,
  event wording, `overrun` and `errors` in status.
- Real machines, by hand with the parent and one kid account: each decision above end to end, the stream cut
  and Resume afterwards, the warning bar over a fullscreen game, the server stopped mid-session, a reboot
  while blocked, `--uninstall` while blocked.

## Rollout

1. Server first (old agents ignore the response).
2. New agent on both PCs, Enforce still off everywhere.
3. Enforce on for one kid, timekpr still running; then the other.
4. `--stop-timekpr` on both PCs.

## Out of scope

- "+30 min", "lock now", and push notifications to the parent.
- Games-only time windows (a schedule per category).
- Closing games after a period behind the lock.
- Text of the notification on the lock screen.
- Formatting or size of the login-screen banner.
- The GNOME focus extension.

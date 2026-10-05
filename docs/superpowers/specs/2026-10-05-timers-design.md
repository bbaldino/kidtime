# Timers: design

Date: 2026-10-05. Status: agreed in conversation, awaiting review of this document.

## Purpose

A parent often allows "30 minutes of game time" right now, separate from the day's rules. A timer stops a kid
after a set number of minutes from when the parent starts it, using the enforcement that already exists.

## Decisions made

| Question | Decision |
|---|---|
| What stops | The parent chooses when starting: lock the computer (default) or close games. |
| What it counts | Clock time from Start, not time spent playing. |
| Relation to other rules | A timer only ever ends things sooner. Allowed hours, blackouts and the budget still apply; whichever stops first wins. |
| How long the stop lasts | Until the parent lifts it ("Allow again"), starts a new timer, or midnight after the end time — the first of these. |
| "+30 min" (granting time) | Out of scope; a separate quick action later. |

## The rule

Each account has at most one timer: `Timer { ends: NaiveDateTime, mode: Lock | Games }`, in the server's local time.

`decide` gains the timer as an input (`Option<&Timer>`). With `now` in local time and `stop_until` the midnight
after `ends`:

- **No timer, or `now >= stop_until`:** no effect.
- **Mode Lock, `now < ends`:** `ends` is a candidate for `next_change`, so the existing lock warnings count down to it.
- **Mode Lock, `ends <= now < stop_until`:** the computer is blocked with a new reason
  `Computer::TimerEnded { at: ends }`, unless a blackout applies (blackout keeps precedence, as it already does over
  the schedule). `next_change` includes `stop_until`.
- **Mode Games, `now < ends`:** the Games category gets a status even without a budget; its `left_secs` is the
  smaller of the budget's remaining seconds (if any) and `ends - now`, so the existing games warnings count down to
  whichever is sooner.
- **Mode Games, `ends <= now < stop_until`:** the Games category is used up, as with an exhausted budget.

Precedence for `computer`: blackout, then timer ended, then outside schedule.

## Protocol

`AccountSnapshot` gains `timer: Option<Timer>` (serde default, so old servers' snapshots still parse). `Timer` and
`TimerMode` live in `protocol::rules`. The agent passes the snapshot's timer to `decide`; nothing else in the agent
changes: locking, login disabling, the stream cut, closing games, warnings and the banner already follow the
decision. The banner and lock notice for `TimerEnded` read "timer ended at 7:42pm".

## Server

- Table `timer(user TEXT PRIMARY KEY, started INTEGER, ends TEXT, mode TEXT)` (`ends` as local text like blackouts).
- `POST /api/timers/{user}` `{ "minutes": 1..=240, "mode": "lock" | "games" }` → 201; replaces any existing timer.
  422 for minutes out of range or an unknown mode; 404 for an unknown account.
- `DELETE /api/timers/{user}` → 204 ("Cancel" before the end, "Allow again" after); 404 if none.
- The server's own `decision()` and `snapshot()` include the timer; `GET /api/status` gives each user
  `timer: { ends, mode, ended } | null`.
- A timer row whose stop has passed (`now >= stop_until`) is ignored and deleted by the daily prune.
- All endpoints behind the login check.

## Dashboard

On each kid's card (Today tab):
- No timer: "Start timer" with 15 / 30 / 60 minutes buttons and a custom minutes box, and a choice "Lock computer"
  (default) / "Close games".
- Running: "Timer ends at 7:42pm: locks the computer" (or "closes games") with Cancel.
- Ended: "Timer ended at 7:42pm" with "Allow again".
- The log words a timer stop "Locked: timer ended" / "Closed games: timer ended" (or "Would have …" with Enforce off).

## Testing

- `decide`: each mode before and after `ends`; precedence with blackout and schedule; Games with and without a
  budget (sooner of the two); `next_change` at `ends` and at `stop_until`; midnight expiry; a timer crossing midnight.
- Server: start (validation), replace, cancel, snapshot and status contents, prune.
- Agent: an ended Lock timer in a snapshot blocks (disable, lock, banner "timer ended"); Games mode closes games.
- Dashboard: start, cancel, ended → Allow again, by hand in a browser.

## Rollout

Release, server bump, agent upgrade on both PCs. An old agent ignores the timer (no enforcement of it) — the
dashboard still shows it; a new agent against an old server sees no timer.

## Out of scope

"+30 min" and "lock now"; timers that pause while away; per-category timers other than Games.

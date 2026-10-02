# Rules and management UI: design

Date: 2026-10-02. Status: approved in conversation, awaiting review of this document.

## Purpose

Kidtime measures the kids' computer time but can't limit it; timekpr still does that, per machine.
This piece lets a parent define the limits in kidtime and see what they would do, before anything
acts on the PCs. It is the first of four pieces:

1. **Rules and management UI** (this document), including who may change rules.
2. Enforcement on the PCs: warnings, closing games, locking. This retires timekpr.
3. Quick actions ("+30 min", "lock now") and one-off allowances.
4. Push notifications to the parent's phone.

Success for this piece: a parent can set a weekly schedule, blackouts and a Games budget per kid from
their phone; the dashboard shows each kid's time left and current decision; a log shows when kidtime
would have locked or closed something; and nobody can read or change any of it without the parent's login.

## Decisions made

| Question | Decision |
|---|---|
| Order | Rules and UI first. Enforcement is a separate, later piece; timekpr keeps enforcing meanwhile. |
| Rule granularity | Per kid, per weekday. |
| Allowed hours | Several from–to stretches per weekday, or "no restriction". |
| Blackout | A span of time when the computer isn't allowed. Recurring ones are the weekly schedule. One-off ones have a start and an end date-time, for one kid or all. |
| Budget | Counts one category of apps, not all time. Only Games has a budget in this piece. The model allows a budget per category later. |
| Categorisation | Automatic where obvious, with a screen to correct it. Uncategorised apps use no budget. |
| Category time | Stored per app as time stretches; category time is worked out when asked, so recategorising applies to the whole day. |
| Who may edit | Anyone with a login verified from the reverse proxy's signed token. Kidtime stores no passwords and no user list. |
| Without a login | Only `/api/report` and `/healthz` answer. No kid view in this piece. |

## Rules

Three kinds of rule, all per account. An account with no rules is unrestricted.

- **Weekly schedule.** For each weekday: either unrestricted, or a list of allowed stretches
  (`start`, `end` in minutes after local midnight, `start < end`, `end ≤ 1440`). Stretches on one day
  must not overlap. A restricted day with no stretches means the computer isn't allowed that day.
  Applies to the whole computer.
- **One-off blackout.** `start` and `end` as local date-times, `start < end`; an account, or all
  restricted accounts; an optional note. Applies to the whole computer.
- **Budget.** For each weekday and category: a number of minutes, or no limit. Applies only to apps
  in that category.

"All restricted accounts" means every account that has at least one schedule or budget rule. This keeps a
parent's own tracked account out of an "everyone" blackout.

Days and weekdays use the server's local time zone, as usage already does. The budget for a day covers
local midnight to local midnight.

## Categories

- A category has an id and a name. One is created on first start: Games. Creating more is out of scope,
  but nothing in the model assumes there is only one.
- The server keeps a catalogue of every app id it has seen: latest name, first seen, last seen, category
  (or none), whether the category was set by a person, and whether a person has reviewed it.
- Automatic rules, applied when an app is first seen and only while no person has set its category:
  - an id starting with `steam:` is Games;
  - an app reported in a sample whose state is `streaming` is Games, unless its id is `steam`
    (the Steam client);
  - everything else is uncategorised.
- A category set by a person is never changed automatically.
- An app is "new" until a person sets its category or marks it reviewed.

Known limits, to state in the UI's help text rather than hide:

- At the desktop, an app counts while it is running and the user is active, whether or not it has focus.
  A game left open in the background uses budget.
- Whether a non-Steam game launched at the desktop is detected at all has not been tested.

## Counting category time

A new table records when each app was in use: `(user, host, app_id, start, end)`. For every counted
sample (state `active` or `streaming`) and every app in it, the server either extends that app's latest
stretch for the same user and host, if the sample starts where the stretch ends (within one second), or
inserts a new stretch.

Category time for a user on a day is the length of the union of all stretches, across hosts, of apps
currently in that category, clipped to the day. So a game streamed from one PC to another counts once,
and recategorising an app changes the whole day's figure at once.

The existing `usage` and `activity` tables and the totals built on them don't change.

## The decision

A pure function, with no database or clock access of its own:

```
decide(rules for one account, category seconds used today, now) -> Decision
```

`Decision` holds:

- `computer`: allowed, or blocked with a reason: `outside_schedule` or `blackout` (with its end and note).
  A blackout takes precedence when both apply.
- for each budgeted category: seconds used, seconds left (none if unlimited), and whether it is used up;
- `next_change`: the earliest time at which `computer` or any "used up" flag could change because of the
  clock alone (a stretch or blackout starting or ending, or midnight). Budget running out is not
  predicted here, because it depends on what the kid does.

The status endpoint calls it for each account. The enforcement piece will call the same function when
answering `/api/report`.

Planned behaviour for enforcement, recorded here so this piece's wording matches it. It follows how
timekpr treats its own games budget: when a category's budget is used up, warn, then close that
category's apps and keep them closed, leaving the session usable. When the computer is blocked, lock the
session.

## The would-have log

Whenever a report is recorded, the server computes the decision for each account that is in use in that
report (state `active` or `streaming` in its latest sample) and compares it with the last one logged for
that account. If `computer` or any "used up" flag differs, it inserts an
event: account, time, what changed, and the reason. Events are shown newest first and kept for 30 days.

Wording describes what would happen, because nothing is enforced yet: "Would have locked: outside
schedule", "Would have closed games: Games budget used up", "Allowed again".

Events are only produced while reports arrive. A blackout starting while the kid's PCs are off produces no
event, which is correct: there was nothing to lock.

## Login check

Two new settings, from the config file or the environment:

- `KIDTIME_ACCESS_TEAM`: the identity provider's base URL (the token's issuer);
- `KIDTIME_ACCESS_AUD`: the application's audience tag.

With both set:

- Every request except `POST /api/report` and `GET /healthz` must carry a valid token in the
  `Cf-Access-Jwt-Assertion` header. Valid means: an RS256 signature by one of the keys published at
  `<team>/cdn-cgi/access/certs`, `iss` equal to the team URL, `aud` containing the audience tag, and
  not expired.
- Keys are fetched at start-up and cached. An unknown key id triggers one refetch, at most once a minute.
  If the keys can't be fetched, requests fail closed with 503.
- A request without a valid token gets 401 with no body. This includes the static files.
- No other header is trusted for identity.

With either unset, there is no login check, and the server logs a warning at start-up saying so. Setting
only one of the two is a start-up error.

`/api/report` keeps its bearer token. Reports are treated as advisory data, not tamper-proof: the token
is a shared secret stored on the monitored machines.

The dashboard's script treats a 401 from the API as an expired session and reloads the page, which sends
the browser back through the login.

## API

All under the login check. JSON in and out. Invalid input gets 422 with a message naming the field.

| Method and path | Purpose |
|---|---|
| `GET /api/status` | As today, plus per account: `decision`, and `restricted` (whether it has any rule). |
| `GET /api/rules/{user}` | The account's seven weekday rules: restricted flag, stretches, budgets by category. |
| `PUT /api/rules/{user}/{weekday}` | Replace one weekday's rule. |
| `POST /api/rules/{user}/copy` | Copy one weekday's rule to a list of weekdays. |
| `GET /api/blackouts` | Blackouts that haven't ended yet. |
| `POST /api/blackouts`, `DELETE /api/blackouts/{id}` | Add, remove. |
| `GET /api/apps` | The catalogue, new ones first. |
| `PUT /api/apps/{id}` | Set category (or none) and mark reviewed. The id is URL-encoded; ids can contain any character. |
| `GET /api/categories` | Categories, for the UI's pickers. |
| `GET /api/events?user=` | The would-have log. |

Accounts are the ones the agents report. Rules can be set for any account name that has reported at
least once.

## Storage

New SQLite tables, created with `CREATE TABLE IF NOT EXISTS` like the existing ones:

- `category(id, name)`
- `app(app_id PRIMARY KEY, name, first_seen, last_seen, category_id NULL, set_by_person, reviewed)`
- `app_activity(user, host, app_id, start, end)`, indexed on `(user, end)`
- `day_rule(user, weekday, restricted, PRIMARY KEY (user, weekday))`
- `stretch(user, weekday, start_min, end_min)`
- `budget(user, weekday, category_id, minutes, PRIMARY KEY (user, weekday, category_id))`
- `blackout(id, user NULL for all restricted accounts, start, end, note)`
- `event(id, user, at, kind, detail)`

`app_activity` rows older than 30 days and `event` rows older than 30 days are deleted once a day.
History before this release has no per-app stretches, so category time starts from the upgrade.

## Code layout

The server is two files today, and this roughly triples it. Split by responsibility:

- `rules.rs`: rule types, validation, and `decide`. No I/O. Most of the tests live here.
- `auth.rs`: token verification and the key cache, as an axum middleware.
- `db.rs`: storage, as now, with the new tables. Interval merging stays here.
- `api.rs`: the new handlers.
- `main.rs`: config, routing, the existing handlers.

New dependencies, added with `cargo add`: a JWT verification crate and an HTTP client for fetching keys.

## Screens

Additions to the existing dashboard: same vanilla JS, no build step, phone-first, same visual style.
Navigation is a small tab bar: Today, Rules, Apps.

- **Today** (the current dashboard). Each restricted kid's card gains a line with the decision
  ("Allowed until 8:00pm", "Would be locked: blackout until Sat 9:00am") and, where there is a budget,
  Games time left. Below the cards, the would-have log.
- **Rules.** Pick a kid. Seven weekday rows, each summarising its stretches and Games budget. Tapping a row
  opens an editor: a restricted/unrestricted switch, from–to rows with add and remove, the budget in
  minutes or "no limit", and "copy to weekdays / weekend / all days". Below the week: blackouts, current
  and upcoming, with add (start, end, who, note) and delete.
- **Apps.** The catalogue, new ones first with a marker. Each row shows the name, when it was last seen,
  and a category picker. A short note explains the two known limits above.

Status is never shown by colour alone, as on the existing cards.

## Error handling

- Rule edits are validated on the server; the UI shows the server's message next to the field.
- A failed save leaves the editor open with what was typed.
- If computing a decision fails for one account, the status response still returns the others, and that
  account's `decision` is null.
- The login check fails closed: key fetch errors mean 503, not open access.

## Testing

- `decide`: schedule edges (exactly at start and end), midnight, a restricted day with no stretches,
  blackouts spanning days and overlapping the schedule, precedence of blackout over schedule,
  `next_change` in each case, budget used up and unlimited.
- Category time: union across two hosts, recategorising mid-day, stretch merging for back-to-back samples
  and a gap, clipping at midnight.
- Automatic categorisation: each rule, and a person's choice surviving later reports.
- Token verification with a key pair generated in the test: valid, expired, wrong audience, wrong issuer,
  bad signature, missing header, unknown key id.
- API: each endpoint's happy path, each validation rule, and 401 on every route except the two open ones
  when the login check is on.
- Would-have log: one event per change, none when nothing changes.
- Screens: checked by hand in a browser against a local server with the login check off.

## Deployment notes

- The two new settings go in the stack's environment. Until they are set, the new version behaves as
  open, with the start-up warning.
- The server needs outbound HTTPS to fetch the keys.
- The release is a `feat:`, so it publishes a new image; the stack's pinned tag then has to be bumped.

## Out of scope

- Anything on the PCs: warnings, closing apps, locking, and behaviour when the server is unreachable.
- "+30 min", "lock now", and one-off allowances that give more time.
- Push notifications.
- Budgets for categories other than Games, and creating categories.
- A view for the kids.
- Focus tracking at the desktop.

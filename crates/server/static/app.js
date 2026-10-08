"use strict";

const REFRESH_MS = 10_000;
const MAX_APPS = 6;

const STATUS = {
  active:     { tone: "good",    label: (h) => `Active on ${h}`,        icon: "dot" },
  streaming:  { tone: "good",    label: (h) => `Streaming from ${h}`,   icon: "play" },
  idle:       { tone: "warning", label: (h) => `Idle on ${h}`,          icon: "half" },
  stream_idle:{ tone: "warning", label: (h) => `Game open, no stream on ${h}`, icon: "half" },
  locked:     { tone: "neutral", label: (h) => `Locked on ${h}`,        icon: "lock" },
  background: { tone: "neutral", label: (h) => `Signed in on ${h}`,     icon: "ring" },
  offline:    { tone: "neutral", label: () => "Offline",                icon: "ring" },
};

const ICONS = {
  dot:  '<circle cx="6" cy="6" r="5" fill="currentColor"/>',
  play: '<path d="M2.5 1.5 L10.5 6 L2.5 10.5 Z" fill="currentColor"/>',
  half: '<circle cx="6" cy="6" r="4.5" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M6 1.5 A4.5 4.5 0 0 1 6 10.5 Z" fill="currentColor"/>',
  lock: '<rect x="2" y="5.5" width="8" height="5.5" rx="1" fill="currentColor"/><path d="M4 5.5 V4 a2 2 0 0 1 4 0 V5.5" fill="none" stroke="currentColor" stroke-width="1.4"/>',
  ring: '<circle cx="6" cy="6" r="4.5" fill="none" stroke="currentColor" stroke-width="1.5"/>',
};

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

// Every API call goes through here. Behind the login proxy an expired session is a redirect to another
// origin, which fetch can't follow usefully; treat it like a 401 and reload into the login.
async function api(path, { method = "GET", body } = {}) {
  const res = await fetch(path, {
    method,
    cache: "no-store",
    redirect: "manual",
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 401 || res.type === "opaqueredirect") {
    location.reload();
    throw new Error("signed out");
  }
  if (res.status === 422 || res.status === 400) {
    // Our own validation answers with JSON; the framework's rejection of a malformed request is plain text
    const text = await res.text();
    let problem = null;
    try { problem = JSON.parse(text); } catch {}
    throw Object.assign(new Error(problem?.error || text || `HTTP ${res.status}`), { field: problem?.field });
  }
  if (!res.ok) throw new Error(res.statusText || `HTTP ${res.status}`);
  return res.status === 204 ? null : res.json();
}

let categories = [];
let appNames = new Map(); // app id -> name, only fetched while some kid has an overrun
const categoryName = (id) => categories.find((c) => c.id === id)?.name ?? "Uncategorised";

// With `withDate`, another day reads "Sat Oct 10, 5:00pm" rather than "Sat 5:00pm"
function clock(text, withDate = false) {
  // "2026-10-05T20:00:00" is the server's local time; show it as written
  const [date, time] = text.split("T");
  const [h, m] = time.split(":").map(Number);
  const label = `${h % 12 || 12}:${String(m).padStart(2, "0")}${h < 12 ? "am" : "pm"}`;
  const today = new Date();
  const todayText = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, "0")}-${String(today.getDate()).padStart(2, "0")}`;
  if (date === todayText) return label;
  if (withDate) return `${parseDay(date).toLocaleDateString(undefined, { weekday: "short" })} ${parseDay(date).toLocaleDateString(undefined, { month: "short", day: "numeric" })}, ${label}`;
  return `${parseDay(date).toLocaleDateString(undefined, { weekday: "short" })} ${label}`;
}

function decisionHtml(u) {
  const errors = u.errors?.length ? `<p class="form-error">${u.errors.map(esc).join("; ")}</p>` : "";
  const names = (u.overrun ?? []).map((id) => appNames.get(id) ?? id);
  const overrun = names.length
    ? `<p class="decision-note">${names.map(esc).join(", ")} ${names.length === 1 ? "is" : "are"} uncategorised and kept running after the games budget ran out. <a href="#" data-goto="apps">Sort in Apps</a></p>`
    : "";
  if (!u.decision) return overrun + errors;
  const d = u.decision;
  // An account with no rules of its own can still be named in a blackout
  if (!u.restricted && d.computer.state === "allowed" && !d.categories.length) return overrun + errors;
  const locked = u.enforce ? "Locked" : "Would be locked";
  let line;
  if (d.computer.state === "allowed") {
    // next_change is not always the moment the state flips, so only name a time when it isn't just midnight
    const midnight = d.next_change?.split("T")[1] === "00:00:00";
    // A games timer's end closes games but doesn't stop the computer: its own line says so
    const gamesTimerEnd = u.timer?.mode === "games" && d.next_change === u.timer.ends;
    line = !d.next_change || midnight || gamesTimerEnd ? "Allowed" : `Allowed until ${esc(clock(d.next_change))}`;
  } else if (d.computer.state === "blackout") line = `${locked}: blackout until ${esc(clock(d.computer.until))}${d.computer.note ? ` (${esc(d.computer.note)})` : ""}`;
  else if (d.computer.state === "timer_ended") line = `${locked}: timer ended at ${esc(clock(d.computer.at))}`;
  else line = `${locked}: outside allowed hours`;
  const gamesTimerEnded = u.timer?.mode === "games" && u.timer.ended;
  const budgets = d.categories.map((c) => c.used_up
    ? `<li><strong>${esc(categoryName(c.category))}</strong> ${c.category === GAMES /* from manage.js */ && gamesTimerEnded ? "timer ended" : "budget used up"}${c.category === GAMES && u.enforce ? ": games closed" : ""}</li>`
    : `<li><strong>${esc(categoryName(c.category))}</strong> ${duration(c.left_secs)} left</li>`).join("");
  return `<div class="decision"><p>${line}</p>${budgets ? `<ul>${budgets}</ul>` : ""}${overrun}</div>${errors}`;
}

const EVENT_TEXT = {
  locked: (e) => `${e.enforced ? "Locked" : "Would have locked"}: ${e.detail}`,
  closed: (e) => e.detail === "timer ended"
    ? `${e.enforced ? "Closed" : "Would have closed"} games: timer ended`
    : e.enforced
      ? `Closed ${e.detail.toLowerCase()}: ${e.detail} budget used up`
      : `Would have closed ${e.detail.toLowerCase()}: ${e.detail} budget used up`,
  allowed: () => "Allowed again",
};

function eventsHtml(events) {
  if (!events.length) return '<li class="empty-note">Nothing yet.</li>';
  return events.map((e) => {
    const when = new Date(e.at * 1000).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" });
    return `<li><span class="event-when">${esc(when)}</span> <span class="event-who">${esc(e.user)}</span> ${esc((EVENT_TEXT[e.kind] ?? (() => e.kind))(e))}${e.host ? ` on ${esc(e.host)}` : ""}</li>`;
  }).join("");
}

function duration(secs) {
  const m = Math.floor(secs / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
}

function parseDay(day) {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(y, m - 1, d);
}

function statusHtml(u) {
  const s = STATUS[u.state] ?? STATUS.offline;
  return `<span class="status ${s.tone}"><svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">${ICONS[s.icon]}</svg>${esc(s.label(u.host ?? ""))}</span>`;
}

// An app's colour is its place among the day's top apps, shared by the bars and the timeline
const COLOURED_APPS = 5;
const seriesColour = (rank) => (rank >= 0 && rank < COLOURED_APPS ? `var(--series-${rank + 1})` : "var(--series-other)");

function appsHtml(u, past) {
  if (!u.apps_today.length) return `<p class="empty-note">No app time ${past ? "that day" : "today"}.</p>`;
  let apps = u.apps_today;
  if (apps.length > MAX_APPS) {
    const rest = apps.slice(MAX_APPS - 1);
    apps = [...apps.slice(0, MAX_APPS - 1), { name: `${rest.length} others`, secs: rest.reduce((a, b) => a + b.secs, 0) }];
  }
  // Apps run side by side, so each bar is its share of today's total
  const scale = Math.max(u.today_secs, ...apps.map((a) => a.secs), 1);
  const rows = apps.map((a, i) => `
    <li class="bar-row" title="${esc(a.name)}: ${duration(a.secs)}">
      <span class="bar-name">${esc(a.name)}</span>
      <span class="bar-track"><span class="bar-fill" style="display:block;width:${(100 * a.secs / scale).toFixed(1)}%;background:${seriesColour(i)}"></span></span>
      <span class="bar-value">${duration(a.secs)}</span>
    </li>`).join("");
  return `<ul class="bars">${rows}</ul>`;
}

const HOUR = 3600;
const timeOfDay = (t) => new Date(t * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
const hourLabel = (t) => new Date(t * 1000).toLocaleTimeString([], { hour: "numeric" }).replace(/\s/g, "").toLowerCase();

// Which app was in use when: a row per top app plus "Other", from the first to the last activity of the day
function timelineHtml(u) {
  const t = u.timeline;
  if (!t) return '<p class="empty-note">Couldn\'t load the timeline.</p>';
  if (!t.blocks.length) {
    const kept = Date.now() / 1000 - t.end < 30 * 86400;
    return `<p class="empty-note">${kept ? "Nothing to show." : "Timelines are kept for 30 days."}</p>`;
  }
  // Whole hours around the activity, inside the day
  const from = Math.max(t.start, Math.floor(Math.min(...t.blocks.map((b) => b.start)) / HOUR) * HOUR);
  const to = Math.min(t.end, Math.ceil(Math.max(...t.blocks.map((b) => b.end)) / HOUR) * HOUR);
  const span = Math.max(to - from, 1);
  const pct = (secs) => (100 * secs / span).toFixed(2);

  const top = u.apps_today.slice(0, COLOURED_APPS).map((a) => a.name);
  const rows = top.map((name, i) => ({ name, colour: seriesColour(i), blocks: t.blocks.filter((b) => b.name === name) }));
  rows.push({ name: "Other", colour: seriesColour(-1), blocks: t.blocks.filter((b) => !top.includes(b.name)) });

  const rowsHtml = rows.filter((r) => r.blocks.length).map((r) => {
    const blocks = r.blocks.map((b) => {
      const tip = `${b.name} · ${timeOfDay(b.start)}–${timeOfDay(b.end)} · ${b.hosts.join(", ")}`;
      return `<span class="tl-block" tabindex="0" style="left:${pct(b.start - from)}%;width:${pct(b.end - b.start)}%;background:${r.colour}" data-tip="${esc(tip)}" data-value="${duration(b.end - b.start)}" aria-label="${esc(tip)}: ${duration(b.end - b.start)}"></span>`;
    }).join("");
    return `<div class="tl-row"><span class="tl-name">${esc(r.name)}</span><span class="tl-track">${blocks}</span></div>`;
  }).join("");

  // At most about six hour labels
  const hours = Math.round(span / HOUR);
  const step = [1, 2, 3, 4, 6, 12, 24].find((s) => hours / s <= 6) ?? 24;
  let ticks = "";
  for (let at = from; at <= to; at += step * HOUR) ticks += `<span${at === to ? ' class="tl-end"' : ""} style="left:${pct(at - from)}%">${esc(hourLabel(at))}</span>`;
  return `<div class="timeline">${rowsHtml}<div class="tl-axis" aria-hidden="true">${ticks}</div></div>`;
}

function weekHtml(u) {
  const max = Math.max(...u.days.map((d) => d.secs), 60 * 60);
  const cols = u.days.map((d, i) => {
    const date = parseDay(d.name);
    const label = date.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" });
    const pct = (100 * d.secs / max).toFixed(1);
    // Every column but the last (the day on show) jumps to its day
    const jump = i === u.days.length - 1 ? "" : ` data-day="${esc(d.name)}"`;
    return `<div class="day${d.secs ? "" : " empty"}"${jump} data-tip="${esc(label)}" data-value="${duration(d.secs)}" tabindex="0" aria-label="${esc(label)}: ${duration(d.secs)}">
      <div class="day-fill" style="height:${d.secs ? pct : 0}%"></div></div>`;
  }).join("");
  const labels = u.days.map((d, i) => {
    const wd = parseDay(d.name).toLocaleDateString(undefined, { weekday: "narrow" });
    return `<span class="${i === u.days.length - 1 ? "today" : ""}">${esc(wd)}</span>`;
  }).join("");
  return `<div class="week" role="img" aria-label="Daily screen time, last 7 days">${cols}</div><div class="week-labels" aria-hidden="true">${labels}</div>`;
}

// What's typed in each kid's timer controls, so the 10-second refresh doesn't wipe it
const timerDrafts = new Map();

function timerHtml(u) {
  const t = u.timer;
  const user = esc(u.user);
  const draft = timerDrafts.get(u.user) ?? {};
  const busy = draft.saving ? "disabled" : "";
  const error = draft.error ? `<p class="form-error" role="alert">${esc(draft.error)}</p>` : "";
  if (t && t.ended) {
    return `<p class="timer-line">Timer ended at ${esc(clock(t.ends))} (until midnight) <button type="button" data-cancel-timer="${user}" ${busy}>Allow again</button></p>${error}`;
  }
  if (t) {
    const what = t.mode === "games" ? "closes games" : "locks the computer";
    return `<p class="timer-line">Timer ends at ${esc(clock(t.ends))}: ${what} <button type="button" data-cancel-timer="${user}" ${busy}>Cancel</button></p>${error}`;
  }
  const mode = draft.mode ?? "lock";
  return `
      <form class="timer-form" data-user="${user}">
        <span class="timer-label">Timer</span>
        <button type="button" data-minutes="15" ${busy}>15m</button>
        <button type="button" data-minutes="30" ${busy}>30m</button>
        <button type="button" data-minutes="60" ${busy}>60m</button>
        <input type="number" name="minutes" min="1" max="240" placeholder="min" aria-label="Minutes" value="${esc(draft.minutes ?? "")}">
        <select name="mode" aria-label="When it ends">
          <option value="lock" ${mode === "lock" ? "selected" : ""}>Lock computer</option>
          <option value="games" ${mode === "games" ? "selected" : ""}>Close games</option>
        </select>
        <button type="submit" ${busy}>Start</button>
        ${error}
      </form>`;
}

// "Mon, Oct 5" for a day like "2026-10-05"
const dayLabel = (day) => parseDay(day).toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" });

// A past day's card: that day's usage only, with nothing live and nothing to change
function pastCardHtml(u, day) {
  const hosts = u.hosts_today.map((h) => `${esc(h.name)} ${duration(h.secs)}`).join(" · ");
  return `
    <article class="card">
      <div class="card-head"><h2>${esc(u.user)}</h2></div>
      <div class="hero">
        <div><span class="hero-value">${duration(u.today_secs)}</span><span class="hero-label">${esc(dayLabel(day))}</span></div>
        <div><span class="hero-secondary">${duration(u.week_secs)}</span><span class="hero-label">7 days to then</span></div>
      </div>
      <h3 class="section-title">Apps that day</h3>
      ${appsHtml(u, true)}
      <h3 class="section-title">Timeline</h3>
      ${timelineHtml(u)}
      <h3 class="section-title">7 days to ${esc(dayLabel(day))}</h3>
      ${weekHtml(u)}
      ${hosts ? `<p class="hosts">By computer: ${hosts}</p>` : ""}
    </article>`;
}

function cardHtml(u) {
  const running = u.sessions.filter((s) => s.state === "active" || s.state === "streaming").flatMap((s) => s.apps);
  const hosts = u.hosts_today.map((h) => `${esc(h.name)} ${duration(h.secs)}`).join(" · ");
  return `
    <article class="card">
      <div class="card-head"><h2>${esc(u.user)}</h2>${statusHtml(u)}</div>
      <div class="hero">
        <div><span class="hero-value">${duration(u.today_secs)}</span><span class="hero-label">today</span></div>
        <div><span class="hero-secondary">${duration(u.week_secs)}</span><span class="hero-label">last 7 days</span></div>
      </div>
      ${decisionHtml(u)}
      ${timerHtml(u)}
      ${running.length ? `<ul class="running" aria-label="Running now">${running.map((a) => `<li>${esc(a)}</li>`).join("")}</ul>` : ""}
      <h3 class="section-title">Apps today</h3>
      ${appsHtml(u)}
      <h3 class="section-title">Timeline</h3>
      ${timelineHtml(u)}
      <h3 class="section-title">Last 7 days</h3>
      ${weekHtml(u)}
      ${hosts ? `<p class="hosts">Today by computer: ${hosts}</p>` : ""}
    </article>`;
}

// Tooltip for the week columns: hover on desktop, tap on touch screens
const tooltip = document.getElementById("tooltip");
let tipTarget = null;
function showTip(el) {
  tipTarget?.classList.remove("active");
  tipTarget = el;
  el.classList.add("active");
  tooltip.innerHTML = `${esc(el.dataset.tip)} · <strong>${esc(el.dataset.value)}</strong>`;
  const r = el.getBoundingClientRect();
  tooltip.hidden = false;
  const half = tooltip.offsetWidth / 2 + 8;
  tooltip.style.left = `${Math.min(Math.max(r.left + r.width / 2, half), window.innerWidth - half)}px`;
  // Above a chart column's filled part, or above a timeline block
  tooltip.style.top = `${el.firstElementChild ? r.top + r.height - el.firstElementChild.offsetHeight : r.top}px`;
}
function hideTip() {
  tipTarget?.classList.remove("active");
  tipTarget = null;
  tooltip.hidden = true;
}
const TIPPED = ".day, .tl-block";
document.addEventListener("pointerover", (e) => { const d = e.target.closest?.(TIPPED); if (d && e.pointerType === "mouse") showTip(d); });
document.addEventListener("pointerout", (e) => { if (e.pointerType === "mouse" && e.target.closest?.(TIPPED)) hideTip(); });
document.addEventListener("click", (e) => {
  const d = e.target.closest?.(TIPPED);
  if (d?.dataset.day) return showDay(d.dataset.day);
  d && d !== tipTarget ? showTip(d) : hideTip();
});
document.addEventListener("keydown", (e) => {
  if ((e.key === "Enter" || e.key === " ") && e.target.classList?.contains("day") && e.target.dataset.day) {
    e.preventDefault();
    showDay(e.target.dataset.day);
  }
});
document.addEventListener("focusin", (e) => { if (e.target.matches?.(TIPPED)) showTip(e.target); });
document.addEventListener("focusout", hideTip);
window.addEventListener("scroll", hideTip, { passive: true });

const usersEl = document.getElementById("users");
const updatedEl = document.getElementById("updated");

// "shown on host-a at 7:42pm", "waiting", or "not shown (expired)"
function messageStatus(m, now) {
  const at = (t) => new Date(t * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (m.shown_host) return `shown on ${esc(m.shown_host)} at ${at(m.shown_at)}`;
  return m.expires * 1000 > now ? "waiting" : "not shown (expired)";
}

function messagesHtml(messages) {
  if (!messages.length) return '<li class="empty-note">No messages yet.</li>';
  const now = Date.now();
  return messages.map((m) => `<li><span class="event-when">${esc(new Date(m.created * 1000).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" }))}</span> <span class="event-who">${esc(m.user)}</span> “${esc(m.text)}”: ${messageStatus(m, now)}</li>`).join("");
}

// The recipient list follows the accounts on the dashboard, keeping the current choice
function updateRecipients(users) {
  const select = document.getElementById("message-to");
  const current = select.value;
  const options = [`<option value="*">Everyone</option>`].concat(users.map((u) => `<option value="${esc(u)}">${esc(u)}</option>`));
  const html = options.join("");
  if (select.dataset.html !== html) {
    select.innerHTML = html;
    select.dataset.html = html;
    if ([...select.options].some((o) => o.value === current)) select.value = current;
  }
}

document.getElementById("message-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const form = e.target;
  const error = document.getElementById("message-error");
  const button = form.querySelector("button");
  error.hidden = true;
  const to = form.elements.to.value;
  const users = to === "*" ? (window.kidtimeUsers ?? []) : [to];
  button.disabled = true;
  try {
    await api("/api/messages", { method: "POST", body: { users, text: form.elements.text.value } });
    form.elements.text.value = "";
    document.getElementById("messages").innerHTML = messagesHtml(await api("/api/messages"));
  } catch (err) {
    error.textContent = err.message || "Couldn't send. Try again.";
    error.hidden = false;
  } finally {
    button.disabled = false;
  }
});

// Saving and error state live in timerDrafts, so the 10-second refresh keeps them on screen
async function timerRequest(user, path, options, failure) {
  const draft = timerDrafts.get(user) ?? {};
  timerDrafts.set(user, { ...draft, saving: true, error: null });
  await refresh();
  try {
    await api(path, options);
    timerDrafts.delete(user);
  } catch (err) {
    timerDrafts.set(user, { ...(timerDrafts.get(user) ?? {}), saving: false, error: err.message || failure });
  }
  await refresh();
}

function startTimer(form, minutes) {
  const user = form.dataset.user;
  return timerRequest(
    user,
    `/api/timers/${encodeURIComponent(user)}`,
    { method: "POST", body: { minutes: Number(minutes), mode: form.elements.mode.value } },
    "Couldn't start the timer. Try again.",
  );
}

usersEl.addEventListener("click", async (e) => {
  const preset = e.target.closest("[data-minutes]");
  if (preset) return startTimer(preset.closest("form"), preset.dataset.minutes);
  const cancel = e.target.closest("[data-cancel-timer]");
  if (cancel) {
    const user = cancel.dataset.cancelTimer;
    // A 404 means it's already gone (for example cancelled from another phone): nothing to report
    await timerRequest(user, `/api/timers/${encodeURIComponent(user)}`, { method: "DELETE" }, "Couldn't change the timer. Try again.")
      .then(() => { if (/Not Found|HTTP 404/.test(timerDrafts.get(user)?.error ?? "")) timerDrafts.delete(user); })
      .then(refresh);
  }
});

usersEl.addEventListener("submit", (e) => {
  const form = e.target.closest(".timer-form");
  if (!form) return;
  e.preventDefault();
  if (!form.reportValidity()) return;
  startTimer(form, form.elements.minutes.value);
});

usersEl.addEventListener("input", (e) => {
  const form = e.target.closest(".timer-form");
  if (form) timerDrafts.set(form.dataset.user, { ...timerDrafts.get(form.dataset.user), minutes: form.elements.minutes.value, mode: form.elements.mode.value });
});

// Each account's timeline for the day; a failed fetch leaves that card's timeline saying so
async function withTimelines(status, day) {
  const query = day ? `?day=${encodeURIComponent(day)}` : "";
  await Promise.all(status.users.map(async (u) => {
    u.timeline = await api(`/api/timeline/${encodeURIComponent(u.user)}${query}`).catch(() => null);
  }));
  return status;
}

// The day on show: null is today (and stays today over midnight), otherwise "2026-10-05"
let viewDay = null;
let serverToday = null;
let refreshes = 0;
const dayPick = document.getElementById("day-pick");
const dayText = (date) => `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;

function showDay(day) {
  viewDay = day && day !== serverToday ? day : null;
  return refresh();
}

function stepDay(by) {
  const date = parseDay(viewDay ?? serverToday ?? dayText(new Date()));
  date.setDate(date.getDate() + by);
  const day = dayText(date);
  // Never past the server's today
  if (!serverToday || day <= serverToday) showDay(day);
}

document.getElementById("day-prev").addEventListener("click", () => stepDay(-1));
document.getElementById("day-next").addEventListener("click", () => stepDay(1));
document.getElementById("day-today").addEventListener("click", () => showDay(null));
dayPick.addEventListener("change", () => {
  // Cleared, or typed past the limit: back to today
  const day = dayPick.value;
  showDay(day && (!serverToday || day <= serverToday) ? day : null);
});

function renderDayBar(status) {
  serverToday = status.today;
  const past = status.day !== status.today;
  dayPick.max = status.today;
  if (document.activeElement !== dayPick) dayPick.value = status.day;
  document.getElementById("day-next").disabled = !past;
  document.getElementById("day-today").hidden = !past;
  for (const el of document.querySelectorAll("[data-today-only]")) el.hidden = past;
}

async function refreshPast(day, turn) {
  const status = await withTimelines(await api(`/api/status?day=${encodeURIComponent(day)}`), day);
  // A slower answer for a day no longer on show is dropped
  if (turn !== refreshes) return;
  hideTip();
  renderDayBar(status);
  usersEl.innerHTML = status.users.length
    ? status.users.map((u) => pastCardHtml(u, status.day)).join("")
    : '<article class="card"><p class="empty-note">Nothing was recorded on this day or the six before it.</p></article>';
  updatedEl.classList.remove("error");
  updatedEl.textContent = `Showing ${dayLabel(status.day)}`;
}

async function refresh() {
  const turn = ++refreshes;
  try {
    if (viewDay) return await refreshPast(viewDay, turn);
    const [status, events, messages] = await Promise.all([api("/api/status"), api("/api/events"), api("/api/messages").catch(() => null)]);
    await withTimelines(status, null);
    if (turn !== refreshes) return;
    renderDayBar(status);
    // Without the names the cards still render; the next refresh asks again
    if (!categories.length) categories = await api("/api/categories").catch(() => []);
    // Overrun apps come as ids; names are nice to have, so a failed fetch just shows ids
    if (status.users.some((u) => u.overrun?.length)) {
      appNames = new Map((await api("/api/apps").catch(() => [])).map((a) => [a.app_id, a.name]));
    }
    hideTip();
    // Keep focus in a timer box across the redraw
    const focused = document.activeElement?.closest?.(".timer-form") ? [document.activeElement.closest(".timer-form").dataset.user, document.activeElement.name] : null;
    usersEl.innerHTML = status.users.length
      ? status.users.map(cardHtml).join("")
      : '<article class="card"><p class="empty-note">No activity reported yet. Is an agent running?</p></article>';
    if (focused) usersEl.querySelector(`.timer-form[data-user="${CSS.escape(focused[0])}"] [name="${focused[1]}"]`)?.focus();
    document.getElementById("events").innerHTML = eventsHtml(events);
    window.kidtimeUsers = status.users.map((u) => u.user);
    updateRecipients(window.kidtimeUsers);
    if (messages) document.getElementById("messages").innerHTML = messagesHtml(messages);
    updatedEl.classList.remove("error");
    updatedEl.textContent = `Updated ${new Date(status.generated_at * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`;
  } catch {
    if (turn !== refreshes) return;
    updatedEl.classList.add("error");
    updatedEl.textContent = "Can't reach server";
  }
}

const tabs = [...document.querySelectorAll(".tab")];
function showTab(name) {
  for (const t of tabs) {
    const on = t.dataset.tab === name;
    if (on) t.setAttribute("aria-current", "page"); else t.removeAttribute("aria-current");
    document.getElementById(`tab-${t.dataset.tab}`).hidden = !on;
  }
  window.dispatchEvent(new CustomEvent("kidtime:tab", { detail: name }));
}
for (const t of tabs) t.addEventListener("click", () => showTab(t.dataset.tab));
document.addEventListener("click", (e) => {
  const link = e.target.closest?.("[data-goto]");
  if (!link) return;
  e.preventDefault();
  showTab(link.dataset.goto);
});

refresh();
setInterval(() => { if (!document.hidden) refresh(); }, REFRESH_MS);
document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });

if ("serviceWorker" in navigator) navigator.serviceWorker.register("/sw.js").catch(() => {});

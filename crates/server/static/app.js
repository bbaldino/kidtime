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
const categoryName = (id) => categories.find((c) => c.id === id)?.name ?? "Uncategorised";

function clock(text) {
  // "2026-10-05T20:00:00" is the server's local time; show it as written
  const [date, time] = text.split("T");
  const [h, m] = time.split(":").map(Number);
  const label = `${h % 12 || 12}:${String(m).padStart(2, "0")}${h < 12 ? "am" : "pm"}`;
  const today = new Date();
  const todayText = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, "0")}-${String(today.getDate()).padStart(2, "0")}`;
  if (date === todayText) return label;
  return `${parseDay(date).toLocaleDateString(undefined, { weekday: "short" })} ${label}`;
}

function decisionHtml(u) {
  if (!u.restricted || !u.decision) return "";
  const d = u.decision;
  let line;
  if (d.computer.state === "allowed") {
    // next_change is not always the moment the state flips, so only name a time when it isn't just midnight
    const midnight = d.next_change?.split("T")[1] === "00:00:00";
    line = !d.next_change || midnight ? "Allowed" : `Allowed until ${esc(clock(d.next_change))}`;
  } else if (d.computer.state === "blackout") line = `Would be locked: blackout until ${esc(clock(d.computer.until))}${d.computer.note ? ` (${esc(d.computer.note)})` : ""}`;
  else line = "Would be locked: outside allowed hours";
  const budgets = d.categories.map((c) => c.used_up
    ? `<li><strong>${esc(categoryName(c.category))}</strong> budget used up</li>`
    : `<li><strong>${esc(categoryName(c.category))}</strong> ${duration(c.left_secs)} left</li>`).join("");
  return `<div class="decision"><p>${line}</p>${budgets ? `<ul>${budgets}</ul>` : ""}</div>`;
}

const EVENT_TEXT = {
  locked: (e) => `Would have locked: ${e.detail}`,
  closed: (e) => `Would have closed ${e.detail.toLowerCase()}: ${e.detail} budget used up`,
  allowed: () => "Allowed again",
};

function eventsHtml(events) {
  if (!events.length) return '<li class="empty-note">Nothing yet.</li>';
  return events.map((e) => {
    const when = new Date(e.at * 1000).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" });
    return `<li><span class="event-when">${esc(when)}</span> <span class="event-who">${esc(e.user)}</span> ${esc((EVENT_TEXT[e.kind] ?? (() => e.kind))(e))}</li>`;
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

function appsHtml(u) {
  if (!u.apps_today.length) return '<p class="empty-note">No app time today.</p>';
  let apps = u.apps_today;
  if (apps.length > MAX_APPS) {
    const rest = apps.slice(MAX_APPS - 1);
    apps = [...apps.slice(0, MAX_APPS - 1), { name: `${rest.length} others`, secs: rest.reduce((a, b) => a + b.secs, 0) }];
  }
  // Apps run side by side, so each bar is its share of today's total
  const scale = Math.max(u.today_secs, ...apps.map((a) => a.secs), 1);
  const rows = apps.map((a) => `
    <li class="bar-row" title="${esc(a.name)}: ${duration(a.secs)}">
      <span class="bar-name">${esc(a.name)}</span>
      <span class="bar-track"><span class="bar-fill" style="display:block;width:${(100 * a.secs / scale).toFixed(1)}%"></span></span>
      <span class="bar-value">${duration(a.secs)}</span>
    </li>`).join("");
  return `<ul class="bars">${rows}</ul>`;
}

function weekHtml(u) {
  const max = Math.max(...u.days.map((d) => d.secs), 60 * 60);
  const cols = u.days.map((d, i) => {
    const date = parseDay(d.name);
    const label = date.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" });
    const pct = (100 * d.secs / max).toFixed(1);
    return `<div class="day${d.secs ? "" : " empty"}" data-tip="${esc(label)}" data-value="${duration(d.secs)}" tabindex="0" aria-label="${esc(label)}: ${duration(d.secs)}">
      <div class="day-fill" style="height:${d.secs ? pct : 0}%"></div></div>`;
  }).join("");
  const labels = u.days.map((d, i) => {
    const wd = parseDay(d.name).toLocaleDateString(undefined, { weekday: "narrow" });
    return `<span class="${i === u.days.length - 1 ? "today" : ""}">${esc(wd)}</span>`;
  }).join("");
  return `<div class="week" role="img" aria-label="Daily screen time, last 7 days">${cols}</div><div class="week-labels" aria-hidden="true">${labels}</div>`;
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
      ${running.length ? `<ul class="running" aria-label="Running now">${running.map((a) => `<li>${esc(a)}</li>`).join("")}</ul>` : ""}
      <h3 class="section-title">Apps today</h3>
      ${appsHtml(u)}
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
  tooltip.innerHTML = `${el.dataset.tip} · <strong>${el.dataset.value}</strong>`;
  const r = el.getBoundingClientRect();
  tooltip.hidden = false;
  const half = tooltip.offsetWidth / 2 + 8;
  tooltip.style.left = `${Math.min(Math.max(r.left + r.width / 2, half), window.innerWidth - half)}px`;
  tooltip.style.top = `${r.top + r.height - (el.firstElementChild?.offsetHeight ?? 0)}px`;
}
function hideTip() {
  tipTarget?.classList.remove("active");
  tipTarget = null;
  tooltip.hidden = true;
}
document.addEventListener("pointerover", (e) => { const d = e.target.closest?.(".day"); if (d && e.pointerType === "mouse") showTip(d); });
document.addEventListener("pointerout", (e) => { if (e.pointerType === "mouse" && e.target.closest?.(".day")) hideTip(); });
document.addEventListener("click", (e) => { const d = e.target.closest?.(".day"); d && d !== tipTarget ? showTip(d) : hideTip(); });
document.addEventListener("focusin", (e) => { if (e.target.classList?.contains("day")) showTip(e.target); });
document.addEventListener("focusout", hideTip);
window.addEventListener("scroll", hideTip, { passive: true });

const usersEl = document.getElementById("users");
const updatedEl = document.getElementById("updated");

async function refresh() {
  try {
    const [status, events] = await Promise.all([api("/api/status"), api("/api/events")]);
    if (!categories.length) categories = await api("/api/categories");
    hideTip();
    usersEl.innerHTML = status.users.length
      ? status.users.map(cardHtml).join("")
      : '<article class="card"><p class="empty-note">No activity reported yet. Is an agent running?</p></article>';
    document.getElementById("events").innerHTML = eventsHtml(events);
    window.kidtimeUsers = status.users.map((u) => u.user);
    updatedEl.classList.remove("error");
    updatedEl.textContent = `Updated ${new Date(status.generated_at * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`;
  } catch {
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

refresh();
setInterval(() => { if (!document.hidden) refresh(); }, REFRESH_MS);
document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });

if ("serviceWorker" in navigator) navigator.serviceWorker.register("/sw.js").catch(() => {});

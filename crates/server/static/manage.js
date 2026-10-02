"use strict";

// Rules, blackouts and the app catalogue. Shares api(), esc(), duration() and categories with app.js.

const DAYS = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const GAMES = 1;
const rulesEl = document.getElementById("tab-rules");
const appsEl = document.getElementById("tab-apps");
let rulesUser = null;
let week = [];
let editing = null; // weekday being edited, or null

const toClock = (min) => `${String(Math.floor(min / 60)).padStart(2, "0")}:${String(min % 60).padStart(2, "0")}`;
// <input type="time"> can't express 24:00; the editor shows end-of-day as 23:59 and saves it as 1440
const fromClock = (text, isEnd) => {
  const [h, m] = text.split(":").map(Number);
  const min = h * 60 + m;
  return isEnd && min === 1439 ? 1440 : min;
};
const clockLabel = (min) => {
  if (min === 1440) return "midnight";
  const h = Math.floor(min / 60), m = min % 60;
  return `${h % 12 || 12}${m ? `:${String(m).padStart(2, "0")}` : ""}${h < 12 ? "am" : "pm"}`;
};

function daySummary(rule) {
  const hours = !rule.restricted ? "Any time"
    : rule.stretches.length ? rule.stretches.map((s) => `${clockLabel(s.start_min)}–${clockLabel(s.end_min)}`).join(", ")
    : "Not allowed";
  const games = rule.budgets[GAMES];
  const budget = games === undefined ? "no games limit" : games === 0 ? "no games" : `games ${duration(games * 60)}`;
  return `${hours} · ${budget}`;
}

function editorHtml(weekday) {
  const rule = week[weekday];
  const rows = rule.stretches.map((s, i) => `
    <li class="stretch-row">
      <label>From <input type="time" name="start" value="${toClock(s.start_min)}" required></label>
      <label>to <input type="time" name="end" value="${toClock(Math.min(s.end_min, 1439))}" required></label>
      <button type="button" data-remove="${i}" aria-label="Remove this stretch">Remove</button>
    </li>`).join("");
  const games = rule.budgets[GAMES];
  return `
    <form class="editor" data-weekday="${weekday}">
      <label class="switch"><input type="checkbox" name="restricted" ${rule.restricted ? "checked" : ""}> Limit the hours on ${DAYS[weekday]}</label>
      <ul class="stretches" ${rule.restricted ? "" : "hidden"}>${rows}</ul>
      <button type="button" data-add ${rule.restricted ? "" : "hidden"}>Add allowed hours</button>
      <label class="switch"><input type="checkbox" name="limited" ${games === undefined ? "" : "checked"}> Limit games</label>
      <label ${games === undefined ? "hidden" : ""}>Minutes of games <input type="number" name="minutes" min="0" max="1440" step="5" value="${games ?? 60}" ${games === undefined ? "" : "required"}></label>
      <p class="form-error" role="alert" hidden></p>
      <div class="editor-actions">
        <button type="submit">Save</button>
        <button type="button" data-cancel>Cancel</button>
      </div>
      <div class="editor-actions">
        <span>Save and copy to:</span>
        <button type="button" data-copy="0,1,2,3,4">Weekdays</button>
        <button type="button" data-copy="5,6">Weekend</button>
        <button type="button" data-copy="0,1,2,3,4,5,6">All days</button>
      </div>
    </form>`;
}

function readEditor(form) {
  const restricted = form.elements.restricted.checked;
  const stretches = restricted ? [...form.querySelectorAll(".stretch-row")].map((row) => ({
    start_min: fromClock(row.querySelector('[name="start"]').value, false),
    end_min: fromClock(row.querySelector('[name="end"]').value, true),
  })) : [];
  const budgets = form.elements.limited.checked ? { [GAMES]: Number(form.elements.minutes.value) } : {};
  return { restricted, stretches, budgets };
}

function blackoutsHtml(blackouts) {
  const rows = blackouts.map((b) => `
    <li>
      <span>${esc(b.user ?? "All kids")}: ${esc(clock(b.start, true))} to ${esc(clock(b.end, true))}${b.note ? ` (${esc(b.note)})` : ""}</span>
      <button type="button" data-delete-blackout="${b.id}">Delete</button>
    </li>`).join("");
  const now = new Date(Date.now() - new Date().getTimezoneOffset() * 60000).toISOString().slice(0, 16);
  const who = (window.kidtimeUsers ?? []).map((u) => `<option value="${esc(u)}" ${u === rulesUser ? "selected" : ""}>${esc(u)}</option>`).join("");
  return `
    <h2>Blackouts</h2>
    <ul class="blackouts">${rows || '<li class="empty-note">None.</li>'}</ul>
    <p class="form-error" id="blackout-error" role="alert" hidden></p>
    <form class="editor" id="blackout-form">
      <label>Who <select name="user"><option value="">All kids</option>${who}</select></label>
      <p class="footnote">"All kids" applies to kids that have a schedule or a budget.</p>
      <label>From <input type="datetime-local" name="start" value="${now}" required></label>
      <label>To <input type="datetime-local" name="end" required></label>
      <label>Note <input type="text" name="note" maxlength="80"></label>
      <p class="form-error" role="alert" hidden></p>
      <button type="submit">Add blackout</button>
    </form>`;
}

// Re-rendering replaces the buttons, which would drop keyboard focus; put it back on `focusSel`
function refocus(root, focusSel) {
  if (focusSel) root.querySelector(focusSel)?.focus();
}

// What a tab shows when its data can't be fetched
function loadFailed(root) {
  root.innerHTML = `<article class="card"><p class="form-error" role="alert">Couldn't load. Try again.</p><div class="card-actions"><button type="button" data-retry>Retry</button></div></article>`;
}

// `message` is shown next to the blackout list, for a change that didn't go through
async function renderRules(focusSel, message) {
  let users, rules, blackouts;
  try {
    // The Today refresh normally supplies the accounts; this tab can be opened before it has finished
    window.kidtimeUsers ??= (await api("/api/status")).users.map((u) => u.user);
    users = window.kidtimeUsers;
    if (!users.length) { rulesEl.innerHTML = '<article class="card"><p class="empty-note">No accounts have reported yet.</p></article>'; return; }
    rulesUser ??= users[0];
    [rules, blackouts] = await Promise.all([api(`/api/rules/${encodeURIComponent(rulesUser)}`), api("/api/blackouts")]);
  } catch {
    // After a failed change the page as it stands is still right: keep it, and say what went wrong
    if (message && document.getElementById("blackout-error")) showBlackoutError(message);
    else loadFailed(rulesEl);
    return;
  }
  week = rules.days;
  const picker = users.map((u) => `<button type="button" class="chip" data-user="${esc(u)}" ${u === rulesUser ? 'aria-pressed="true"' : 'aria-pressed="false"'}>${esc(u)}</button>`).join("");
  const days = week.map((rule, i) => editing === i ? `<li>${editorHtml(i)}</li>` : `
    <li><button type="button" class="day-row" data-edit="${i}"><span class="day-name">${DAYS[i]}</span><span class="day-summary">${esc(daySummary(rule))}</span></button></li>`).join("");
  rulesEl.innerHTML = `
    <article class="card">
      <div class="chips" role="group" aria-label="Account">${picker}</div>
      <ul class="week-rules">${days}</ul>
    </article>
    <article class="card">${blackoutsHtml(blackouts)}</article>`;
  if (message) showBlackoutError(message);
  refocus(rulesEl, focusSel);
}

function showBlackoutError(message) {
  const el = document.getElementById("blackout-error");
  el.textContent = message;
  el.hidden = false;
}

function showError(form, error) {
  const el = form.querySelector(".form-error");
  el.textContent = error.message;
  el.hidden = false;
}

rulesEl.addEventListener("click", async (e) => {
  const t = e.target.closest("button");
  if (!t) return;
  const form = t.closest("form");
  if (t.dataset.retry !== undefined) return renderRules();
  if (t.dataset.user) { rulesUser = t.dataset.user; editing = null; return renderRules(`[data-user="${CSS.escape(t.dataset.user)}"]`); }
  if (t.dataset.edit !== undefined) { editing = Number(t.dataset.edit); return renderRules("form[data-weekday] [name=restricted]"); }
  if (t.dataset.cancel !== undefined) { const day = editing; editing = null; return renderRules(`[data-edit="${day}"]`); }
  if (t.dataset.add !== undefined) {
    // Keep what was typed: update the model from the form, then add a row
    week[editing] = readEditor(form);
    week[editing].stretches.push({ start_min: 960, end_min: 1200 });
    return renderRulesKeepingEdits(".stretch-row:last-child [name=start]");
  }
  if (t.dataset.remove !== undefined) {
    week[editing] = readEditor(form);
    week[editing].stretches.splice(Number(t.dataset.remove), 1);
    return renderRulesKeepingEdits("[data-add]");
  }
  if (t.dataset.copy) {
    if (!form.reportValidity()) return;
    return saveDay(form, t.dataset.copy.split(",").map(Number));
  }
  if (t.dataset.deleteBlackout) {
    let message;
    try {
      await api(`/api/blackouts/${t.dataset.deleteBlackout}`, { method: "DELETE" });
    } catch {
      message = "Couldn't delete the blackout. Try again.";
    }
    // Either way the list is fetched again, so it shows what the server has
    return renderRules("#blackout-form [name=user]", message);
  }
});

// Re-render the open editor from `week` without refetching, so unsaved edits survive
function renderRulesKeepingEdits(focusSel) {
  const li = rulesEl.querySelector("form[data-weekday]").parentElement;
  li.innerHTML = editorHtml(editing);
  refocus(li, focusSel);
}

rulesEl.addEventListener("change", (e) => {
  const form = e.target.closest("form[data-weekday]");
  if (!form || (e.target.name !== "restricted" && e.target.name !== "limited")) return;
  week[editing] = readEditor(form);
  if (e.target.name === "limited" && !e.target.checked) week[editing].budgets = {};
  if (e.target.name === "limited" && e.target.checked) week[editing].budgets = { [GAMES]: 60 };
  renderRulesKeepingEdits(`[name=${e.target.name}]`);
});

async function saveDay(form, copyTo) {
  const weekday = Number(form.dataset.weekday);
  try {
    await api(`/api/rules/${encodeURIComponent(rulesUser)}/${weekday}`, { method: "PUT", body: readEditor(form) });
    if (copyTo) await api(`/api/rules/${encodeURIComponent(rulesUser)}/copy`, { method: "POST", body: { from: weekday, to: copyTo } });
    const day = editing;
    editing = null;
    await renderRules(`[data-edit="${day}"]`);
  } catch (error) {
    // The editor stays open with what was typed
    showError(form, error);
  }
}

rulesEl.addEventListener("submit", async (e) => {
  e.preventDefault();
  const form = e.target;
  if (form.dataset.weekday !== undefined) return saveDay(form, null);
  if (form.id === "blackout-form") {
    const f = form.elements;
    try {
      await api("/api/blackouts", { method: "POST", body: { user: f.user.value || null, start: f.start.value, end: f.end.value, note: f.note.value } });
      await renderRules("#blackout-form [name=user]");
    } catch (error) {
      showError(form, error);
    }
  }
});

let appList = []; // the catalogue as last fetched

// `message` is shown in the card, for a change that didn't go through
async function renderApps(focusApp, message) {
  try {
    // The Today refresh normally supplies the categories; this tab can be opened before it has finished
    if (!categories.length) categories = await api("/api/categories");
    appList = await api("/api/apps");
  } catch {
    // After a failed change, fall through and draw the last list fetched: it has the stored values
    if (!message || !categories.length) { loadFailed(appsEl); return; }
  }
  const options = (current) => [`<option value="" ${current === null ? "selected" : ""}>Uncategorised</option>`]
    .concat(categories.map((c) => `<option value="${c.id}" ${current === c.id ? "selected" : ""}>${esc(c.name)}</option>`)).join("");
  const rows = appList.map((a) => `
    <li class="app-row">
      <span class="app-name">${a.reviewed ? "" : '<span class="badge">New</span> '}${esc(a.name)}</span>
      <span class="app-seen">last used ${esc(new Date(a.last_seen * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" }))}</span>
      <span class="app-controls">
        <select data-app="${esc(a.app_id)}" aria-label="Category for ${esc(a.name)}">${options(a.category_id)}</select>
        ${a.reviewed ? "" : `<button type="button" data-review="${esc(a.app_id)}" aria-label="${esc(a.name)} looks right">Looks right</button>`}
      </span>
    </li>`).join("");
  const anyNew = appList.some((a) => !a.reviewed);
  appsEl.innerHTML = `
    <article class="card">
      <p class="footnote">Only apps in a category with a budget use that budget. Steam games and anything played in a stream are set to Games automatically; change any of them here.</p>
      <p class="footnote">At the desktop an app counts while it is open and the kid is active, even in the background. Games launched outside Steam at the desktop may not be detected.</p>
      ${anyNew ? '<div class="card-actions"><button type="button" data-review-all>Mark all as reviewed</button></div>' : ""}
      <p class="form-error" id="apps-error" role="alert" hidden></p>
      <ul class="apps">${rows || '<li class="empty-note">No apps seen yet.</li>'}</ul>
    </article>`;
  if (message) {
    const el = document.getElementById("apps-error");
    el.textContent = message;
    el.hidden = false;
  }
  updateNewBadge(appList);
  if (focusApp) appsEl.querySelector(`select[data-app="${CSS.escape(focusApp)}"]`)?.focus();
}

function updateNewBadge(apps) {
  const badge = document.getElementById("apps-new");
  const count = apps.filter((a) => !a.reviewed).length;
  badge.textContent = count;
  badge.hidden = count === 0;
  badge.setAttribute("aria-label", `${count} new`);
}

// Setting an app's category, even to the one it has, marks it reviewed
const putApp = (appId, categoryId) => api(`/api/apps/${encodeURIComponent(appId)}`, { method: "PUT", body: { category_id: categoryId } });

// Runs a change to the catalogue, then draws the list again from the server either way, so a select
// never keeps showing a value that wasn't saved
async function changeApps(change, focusApp) {
  let message;
  try {
    await change();
  } catch {
    message = "Couldn't save. Try again.";
  }
  await renderApps(focusApp, message);
}

appsEl.addEventListener("change", (e) => {
  const select = e.target.closest("select[data-app]");
  if (!select) return;
  const appId = select.dataset.app;
  return changeApps(() => putApp(appId, select.value === "" ? null : Number(select.value)), appId);
});

appsEl.addEventListener("click", (e) => {
  const t = e.target.closest("button");
  if (!t) return;
  if (t.dataset.retry !== undefined) return renderApps();
  if (t.dataset.review !== undefined) {
    const app = appList.find((a) => a.app_id === t.dataset.review);
    if (app) return changeApps(() => putApp(app.app_id, app.category_id), app.app_id);
  }
  if (t.dataset.reviewAll !== undefined) {
    return changeApps(async () => {
      for (const app of appList.filter((a) => !a.reviewed)) await putApp(app.app_id, app.category_id);
    });
  }
});

window.addEventListener("kidtime:tab", (e) => {
  if (e.detail === "rules") renderRules();
  if (e.detail === "apps") renderApps();
});

// Keep the "new apps" badge current without opening the tab
api("/api/apps").then(updateNewBadge).catch(() => {});

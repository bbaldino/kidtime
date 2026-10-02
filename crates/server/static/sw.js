// Network first; fall back to the last cached copy so the app still opens when the server can't be
// reached. API answers are never cached: the page must be able to tell that it has no fresh data.
// The server fills in the stamp below from the scripts' and stylesheet's content, so every release
// gets its own cache and its own script addresses.
const CACHE = "kidtime-__ASSETS__";
const SHELL = [
  "/", "/app.js?v=__ASSETS__", "/manage.js?v=__ASSETS__", "/style.css?v=__ASSETS__",
  "/manifest.webmanifest", "/icon.svg", "/icon-192.png",
];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (e) => {
  if (e.request.method !== "GET" || new URL(e.request.url).pathname.startsWith("/api/")) return;
  e.respondWith(
    fetch(e.request)
      .then((res) => {
        if (res.ok) {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put(e.request, copy));
        }
        return res;
      })
      .catch(() => caches.match(e.request).then((r) => r ?? Response.error())),
  );
});

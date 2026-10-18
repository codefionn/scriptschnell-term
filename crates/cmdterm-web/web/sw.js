// Service worker: keeps the app's files so the page opens offline, and the last answers
// of GET /api/machines and GET /api/me so the app can show them read-only.
//
// Bump VERSION whenever a file in SHELL changes; the new worker then fetches them again
// and deletes the old cache.

const VERSION = '1';
const SHELL_CACHE = `cmdterm-web-shell-v${VERSION}`;
// Not versioned: an app update keeps the last machine list. Logging out deletes it.
const API_CACHE = 'cmdterm-web-api';

const SHELL = [
  './',
  'app.js',
  'api.js',
  'dom.js',
  'shell.js',
  'store.js',
  'machines.js',
  'terminal.js',
  'app.css',
  'manifest.webmanifest',
  'icon.svg',
  'vendor/xterm.js',
  'vendor/xterm.css',
  'vendor/addon-fit.js',
];

const BASE = new URL('./', self.location).pathname;
const SHELL_PATHS = new Set(SHELL.map((p) => new URL(p, self.location).pathname));
// Answers taken from the network when it works, from the cache when it does not.
const API_PATHS = new Set([`${BASE}api/machines`, `${BASE}api/me`]);

self.addEventListener('install', (event) => {
  event.waitUntil((async () => {
    const cache = await caches.open(SHELL_CACHE);
    // Skip the HTTP cache so a new version never stores old files.
    await cache.addAll(SHELL.map((p) => new Request(p, { cache: 'reload' })));
    await self.skipWaiting();
  })());
});

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    for (const name of await caches.keys()) {
      if (name.startsWith('cmdterm-web-') && name !== SHELL_CACHE && name !== API_CACHE) {
        await caches.delete(name);
      }
    }
    await self.clients.claim();
  })());
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  const path = url.pathname;

  // Logins, OIDC redirects and dev tokens always go to the server.
  if (path.startsWith(`${BASE}auth/`)) return;

  if (API_PATHS.has(path) && !url.search) {
    event.respondWith(networkFirst(req));
    return;
  }
  // No other API answer is kept: they change, or they carry a terminal.
  if (path.startsWith(`${BASE}api/`) || path.startsWith(`${BASE}v1/`)) return;

  if (req.mode === 'navigate') {
    if (path === BASE || path === `${BASE}index.html`) {
      event.respondWith(cacheFirst(new Request('./'), req));
    }
    return;
  }

  if (SHELL_PATHS.has(path)) {
    event.respondWith(cacheFirst(req, req));
  }
});

async function cacheFirst(key, req) {
  const cache = await caches.open(SHELL_CACHE);
  const hit = await cache.match(key, { ignoreSearch: true });
  if (hit) return hit;
  return fetch(req);
}

async function networkFirst(req) {
  const cache = await caches.open(API_CACHE);
  let res;
  try {
    res = await fetch(req);
  } catch {
    const hit = await cache.match(req);
    if (!hit) return Response.error();
    // Tell the page this answer is old.
    const headers = new Headers(hit.headers);
    headers.set('X-Cmdterm-Offline', '1');
    return new Response(await hit.blob(), { status: hit.status, statusText: hit.statusText, headers });
  }
  if (res.ok) {
    await cache.put(req, res.clone());
  } else if (res.status === 401 || res.status === 403) {
    await cache.delete(req);
  }
  return res;
}

// State and page chrome shared by every view: the offline banner, the title, the
// current user and the hash routes.

import { h } from './dom.js';
import { NetworkError } from './api.js';

export const state = {
  // `GET /api/me`, or the cached copy.
  user: null,
  // `GET /api/auth/options`, once fetched.
  options: null,
  // True while the last request failed to reach the server or came from the cache.
  offline: false,
};

const listeners = new Set();

export function onOfflineChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

export function setOffline(offline) {
  offline = !!offline;
  if (state.offline === offline) return;
  state.offline = offline;
  const banner = document.getElementById('offline-banner');
  if (banner) banner.hidden = !offline;
  document.documentElement.classList.toggle('is-offline', offline);
  for (const fn of listeners) fn(offline);
}

export function setTitle(text) {
  document.title = text ? `${text} · scriptschnell-term` : 'scriptschnell-term';
}

// '#machine/abc?x=1' -> { name: 'machine', parts: ['abc'], query: URLSearchParams }
export function parseHash(hash = location.hash) {
  let s = hash.replace(/^#/, '');
  let query = '';
  const q = s.indexOf('?');
  if (q >= 0) {
    query = s.slice(q + 1);
    s = s.slice(0, q);
  }
  const parts = s.split('/').map((p) => {
    try {
      return decodeURIComponent(p);
    } catch {
      return p;
    }
  });
  const name = parts.shift() || '';
  return { name, parts, query: new URLSearchParams(query) };
}

export function href(name, ...parts) {
  return '#' + [name, ...parts.map((p) => encodeURIComponent(p))].join('/');
}

export function terminalHref(machineId, session, cwd) {
  const base = href('terminal', machineId, session);
  return cwd ? `${base}?${new URLSearchParams({ cwd })}` : base;
}

export function go(hash) {
  if (location.hash === hash) window.dispatchEvent(new HashChangeEvent('hashchange'));
  else location.hash = hash;
}

// A status line inside a view: errors, notices.
export function notice(text, kind = 'error') {
  return h('p', { class: `notice notice-${kind}`, role: kind === 'error' ? 'alert' : 'status' }, text);
}

export function errorText(e) {
  if (!e) return 'Something went wrong.';
  if (e instanceof NetworkError) {
    return 'Cannot reach the server.';
  }
  return e.message || String(e);
}

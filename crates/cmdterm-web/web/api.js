// Calls to the web server's HTTP API (crates/cmdterm-web/README.md, "HTTP API").
//
// Every request sends the session cookie. Requests that change something send a JSON
// body, because the server refuses anything else. A 401 answer sends the page to the
// login view, except where the caller handles it itself.

export class ApiError extends Error {
  constructor(status, message, body) {
    super(message);
    this.status = status;
    this.body = body || {};
  }
}

// Thrown when the request never reached the server.
export class NetworkError extends Error {}

let onUnauthorized = () => {};

export function setUnauthorizedHandler(fn) {
  onUnauthorized = fn;
}

async function request(method, path, body, { allow401 = false } = {}) {
  const init = { method, credentials: 'same-origin', headers: { Accept: 'application/json' } };
  if (method !== 'GET') {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body === undefined ? {} : body);
  }
  let res;
  try {
    res = await fetch(path, init);
  } catch (e) {
    throw new NetworkError(e && e.message ? e.message : 'Network error');
  }
  // The service worker marks answers it took from its cache.
  const stale = res.headers.get('X-Cmdterm-Offline') === '1';
  let data = null;
  if (res.status !== 204) {
    const text = await res.text().catch(() => '');
    if (text) {
      try {
        data = JSON.parse(text);
      } catch {
        data = null;
      }
    }
  }
  if (res.status === 401 && !allow401) {
    onUnauthorized();
  }
  if (!res.ok) {
    const msg = (data && data.error) || `${res.status} ${res.statusText || 'error'}`;
    throw new ApiError(res.status, msg, data);
  }
  return { data, stale };
}

const enc = encodeURIComponent;

export const api = {
  authOptions: () => request('GET', '/api/auth/options'),
  login: (username, password) =>
    request('POST', '/api/auth/login', { username, password }, { allow401: true }),
  logout: () => request('POST', '/api/auth/logout', {}, { allow401: true }),

  me: () => request('GET', '/api/me'),
  setPassword: (current, next) => request('POST', '/api/me/password', { current, new: next }),
  removeIdentity: (provider) => request('DELETE', `/api/me/identities/${enc(provider)}`),

  machines: () => request('GET', '/api/machines'),
  pair: () => request('POST', '/api/machines/pair', {}),
  renameMachine: (id, name) => request('PATCH', `/api/machines/${enc(id)}`, { name }),
  removeMachine: (id) => request('DELETE', `/api/machines/${enc(id)}`),
  projects: (id) => request('GET', `/api/machines/${enc(id)}/projects`),
  sessions: (id) => request('GET', `/api/machines/${enc(id)}/sessions`),
  endSession: (id, name) => request('DELETE', `/api/machines/${enc(id)}/sessions/${enc(name)}`),
};

export function oidcStartUrl(id) {
  return `/auth/oidc/${enc(id)}/start`;
}

export function terminalUrl(machineId, { session, cols, rows, cwd, resume }) {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const q = new URLSearchParams({ session, cols: String(cols), rows: String(rows) });
  if (cwd) q.set('cwd', cwd);
  if (resume > 0) q.set('resume', String(resume));
  return `${proto}//${location.host}/api/machines/${enc(machineId)}/terminal?${q}`;
}

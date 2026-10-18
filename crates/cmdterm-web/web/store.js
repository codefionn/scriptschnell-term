// What the app keeps in the browser between visits.
//
// localStorage holds the last machine list and the user name. IndexedDB holds up to
// 256 KiB of output per session with the byte offset it ends at. Storage can be missing
// or full (private windows, quota, blocked site data), so every access is wrapped and a
// failure only means nothing is cached.

const MACHINES_KEY = 'cmdterm-web.machines';
const USER_KEY = 'cmdterm-web.user';

const DB_NAME = 'cmdterm-web';
const DB_VERSION = 1;
const STORE = 'scrollback';

export const SCROLLBACK_LIMIT = 256 * 1024;

function readJson(key) {
  try {
    const raw = localStorage.getItem(key);
    return raw ? JSON.parse(raw) : null;
  } catch {
    return null;
  }
}

function writeJson(key, value) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Quota or blocked storage: keep going without the cache.
  }
}

// The machine list as `GET /api/machines` answered it, with the time it was saved.
export function loadMachines() {
  const v = readJson(MACHINES_KEY);
  return v && Array.isArray(v.machines) ? v : null;
}

export function saveMachines(machines) {
  writeJson(MACHINES_KEY, { saved: Date.now(), machines });
}

export function cachedMachine(id) {
  const v = loadMachines();
  return v ? v.machines.find((m) => m.id === id) || null : null;
}

export function loadUser() {
  return readJson(USER_KEY);
}

export function saveUser(user) {
  writeJson(USER_KEY, user);
}

let dbPromise = null;

function openDb() {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise((resolve) => {
    try {
      const req = indexedDB.open(DB_NAME, DB_VERSION);
      req.onupgradeneeded = () => {
        const db = req.result;
        if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => resolve(null);
      req.onblocked = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
  return dbPromise;
}

async function idb(mode, fn) {
  const db = await openDb();
  if (!db) return null;
  return new Promise((resolve) => {
    try {
      const tx = db.transaction(STORE, mode);
      const req = fn(tx.objectStore(STORE));
      tx.oncomplete = () => resolve(req ? req.result : null);
      tx.onerror = () => resolve(null);
      tx.onabort = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
}

function sessionKey(machineId, session) {
  return `${machineId}\n${session}`;
}

// Output of one session. `start` and `end` are byte offsets of the session's output;
// `data` holds the bytes between them, at most SCROLLBACK_LIMIT.
export class Scrollback {
  constructor(machineId, session) {
    this.key = sessionKey(machineId, session);
    this.start = 0;
    this.end = 0;
    this.chunks = [];
    this.size = 0;
    this.timer = 0;
    this.dirty = false;
  }

  async load() {
    const rec = await idb('readonly', (s) => s.get(this.key));
    if (rec && rec.data instanceof Uint8Array && typeof rec.end === 'number') {
      this.chunks = [rec.data];
      this.size = rec.data.length;
      this.end = rec.end;
      this.start = rec.end - rec.data.length;
    }
    return this;
  }

  bytes() {
    if (this.chunks.length !== 1) {
      const out = new Uint8Array(this.size);
      let at = 0;
      for (const c of this.chunks) {
        out.set(c, at);
        at += c.length;
      }
      this.chunks = [out];
    }
    return this.chunks[0] || new Uint8Array(0);
  }

  // Drops everything and starts again at `offset`.
  reset(offset) {
    this.chunks = [];
    this.size = 0;
    this.start = offset;
    this.end = offset;
    this.schedule();
  }

  append(bytes) {
    this.chunks.push(bytes);
    this.size += bytes.length;
    this.end += bytes.length;
    if (this.size > SCROLLBACK_LIMIT * 2) this.trim();
    this.schedule();
  }

  trim() {
    if (this.size <= SCROLLBACK_LIMIT) return;
    const all = this.bytes();
    const cut = all.length - SCROLLBACK_LIMIT;
    this.chunks = [all.slice(cut)];
    this.size = SCROLLBACK_LIMIT;
    this.start = this.end - SCROLLBACK_LIMIT;
  }

  schedule() {
    this.dirty = true;
    if (!this.timer) this.timer = setTimeout(() => this.flush(), 1500);
  }

  async flush() {
    clearTimeout(this.timer);
    this.timer = 0;
    if (!this.dirty) return;
    this.dirty = false;
    this.trim();
    const rec = { end: this.end, data: this.bytes(), saved: Date.now() };
    await idb('readwrite', (s) => s.put(rec, this.key));
  }
}

export async function forgetScrollback(machineId, session) {
  await idb('readwrite', (s) => s.delete(sessionKey(machineId, session)));
}

// Removes everything this app stored, for logging out.
export async function clearAll() {
  writeJson(MACHINES_KEY, null);
  writeJson(USER_KEY, null);
  await idb('readwrite', (s) => s.clear());
  try {
    if (self.caches) {
      for (const name of await caches.keys()) {
        if (name.startsWith('cmdterm-web-api')) await caches.delete(name);
      }
    }
  } catch {
    // Nothing cached, or no access to the cache API.
  }
}

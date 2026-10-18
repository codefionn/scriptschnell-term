// The terminal view (#terminal/{machineId}/{session}).
//
// One xterm.js terminal on the terminal WebSocket (README, "Terminals"). The view keeps
// the byte offset of the session's output it shows, saves the output to IndexedDB, and
// resumes from that offset when it reconnects. Without a connection it shows the saved
// output read-only.

import { api, terminalUrl, ApiError } from './api.js';
import { h } from './dom.js';
import { state, setOffline, setTitle, href } from './shell.js';
import { Scrollback } from './store.js';

const BACKOFF_START = 1000;
const BACKOFF_MAX = 30000;
// A tab hidden this long may hold a socket the phone has silently dropped.
const STALE_AFTER_HIDDEN = 10000;

const encoder = new TextEncoder();

function cssVar(name, fallback) {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

function termTheme() {
  return {
    background: cssVar('--term-bg', '#111'),
    foreground: cssVar('--term-fg', '#ddd'),
    cursor: cssVar('--term-cursor', '#ddd'),
    cursorAccent: cssVar('--term-bg', '#111'),
    selectionBackground: cssVar('--term-selection', 'rgba(128,128,128,0.4)'),
  };
}

// Ctrl plus a key, as a terminal sends it.
function ctrlChar(c) {
  if (c.length !== 1) return c;
  const code = c.toUpperCase().charCodeAt(0);
  if (code >= 64 && code <= 95) return String.fromCharCode(code - 64); // @ A-Z [ \ ] ^ _
  if (c === ' ' || c === '2') return '\x00';
  if (c === '?') return '\x7f';
  if (c === '/') return '\x1f';
  if (c === '-') return '\x1f';
  if (c === '~') return '\x1e';
  return c;
}

const ARROWS = [
  ['ArrowLeft', 'D', 'Left', '←'],
  ['ArrowUp', 'A', 'Up', '↑'],
  ['ArrowDown', 'B', 'Down', '↓'],
  ['ArrowRight', 'C', 'Right', '→'],
];

export function renderTerminal(root, route) {
  const machineId = route.parts[0] || '';
  const session = route.parts[1] || '';
  let cwd = route.query.get('cwd') || '';

  document.body.classList.add('terminal-mode');
  setTitle(session);

  // Connection state.
  let ws = null;
  let phase = 'idle'; // idle | connecting | pre | live | waiting | ended
  let backoff = BACKOFF_START;
  let retryTimer = 0;
  let countdownTimer = 0;
  let retryAt = 0;
  let disposed = false;
  let hiddenSince = 0;
  let lastError = '';
  // Byte offset of the session's output up to which the terminal shows it.
  let offset = 0;
  let pre = [];

  // Sticky modifiers from the extra-keys bar, used up by the next key.
  const mods = { ctrl: false, alt: false };

  const statusEl = h('span', { class: 'term-status', role: 'status', 'aria-live': 'polite' });
  const retryBtn = h('button', { type: 'button', class: 'quiet small-btn', hidden: true, onclick: () => connectNow() }, 'Retry now');
  const back = h('a', { class: 'button quiet term-back', href: href('machine', machineId), 'aria-label': 'Back to machine' }, '‹ Back');
  const titleEl = h('span', { class: 'term-title mono', title: cwd || null }, session);
  const bar = h('div', { class: 'term-bar' }, back, titleEl, h('span', { class: 'term-state' }, statusEl, retryBtn));
  const host = h('div', { class: 'term-host' });
  const keys = h('div', { class: 'extra-keys', role: 'toolbar', 'aria-label': 'Extra keys' });
  const view = h('div', { class: 'term-view' }, bar, host, keys);
  root.append(view);

  const Terminal = window.Terminal;
  const FitAddon = window.FitAddon && window.FitAddon.FitAddon;
  if (!Terminal || !FitAddon) {
    statusEl.textContent = 'The terminal library did not load.';
    return () => document.body.classList.remove('terminal-mode');
  }

  const narrow = Math.min(window.innerWidth, window.innerHeight) < 500;
  const term = new Terminal({
    fontFamily: cssVar('--mono', 'monospace'),
    fontSize: narrow ? 12 : 14,
    cursorBlink: true,
    scrollback: 5000,
    theme: termTheme(),
    disableStdin: true,
    macOptionIsMeta: true,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(host);

  const scrollback = new Scrollback(machineId, session);
  let haveSaved = false;
  // True while the screen shows a note of ours rather than session output.
  let placeholder = false;

  function setStatus(text, kind) {
    statusEl.textContent = text;
    view.dataset.state = kind;
    retryBtn.hidden = kind !== 'waiting' && kind !== 'offline';
  }

  function setLive(live) {
    term.options.disableStdin = !live;
    view.classList.toggle('read-only', !live);
  }

  // Fitting.
  let fitFrame = 0;
  function scheduleFit() {
    if (fitFrame) return;
    fitFrame = requestAnimationFrame(() => {
      fitFrame = 0;
      if (disposed) return;
      try {
        fit.fit();
      } catch {
        // The host has no size yet.
      }
    });
  }

  term.onResize(({ cols, rows }) => {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: 'resize', cols, rows }));
    }
  });

  // Keep the view inside the visual viewport so the on-screen keyboard does not cover
  // the cursor.
  const vv = window.visualViewport;
  function placeView() {
    if (vv) {
      view.style.top = `${vv.offsetTop}px`;
      view.style.left = `${vv.offsetLeft}px`;
      view.style.height = `${vv.height}px`;
      view.style.width = `${vv.width}px`;
    }
    scheduleFit();
  }
  if (vv) {
    vv.addEventListener('resize', placeView);
    vv.addEventListener('scroll', placeView);
  }
  window.addEventListener('resize', placeView);
  const ro = typeof ResizeObserver !== 'undefined' ? new ResizeObserver(scheduleFit) : null;
  if (ro) ro.observe(host);

  // Input.
  function send(text) {
    if (phase !== 'live' || !ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(encoder.encode(text));
  }

  function clearMods() {
    mods.ctrl = mods.alt = false;
    for (const b of keys.querySelectorAll('[aria-pressed]')) b.setAttribute('aria-pressed', 'false');
  }

  function withMods(data) {
    if (!mods.ctrl && !mods.alt) return data;
    let out = data;
    if (mods.ctrl && out.length >= 1) out = ctrlChar(out[0]) + out.slice(1);
    if (mods.alt) out = '\x1b' + out;
    clearMods();
    return out;
  }

  term.onData((data) => send(withMods(data)));
  term.onBinary((data) => {
    if (phase !== 'live' || !ws || ws.readyState !== WebSocket.OPEN) return;
    const bytes = new Uint8Array(data.length);
    for (let i = 0; i < data.length; i++) bytes[i] = data.charCodeAt(i) & 0xff;
    ws.send(bytes);
  });

  function arrow(letter) {
    const m = (mods.ctrl ? 4 : 0) + (mods.alt ? 2 : 0);
    clearMods();
    if (m) return `\x1b[1;${m + 1}${letter}`;
    return term.modes.applicationCursorKeysMode ? `\x1bO${letter}` : `\x1b[${letter}`;
  }

  function key(label, aria, action, extra) {
    const b = h('button', { type: 'button', class: 'key', 'aria-label': aria, ...extra }, label);
    // Keep focus, and with it the on-screen keyboard, on the terminal.
    b.addEventListener('pointerdown', (e) => e.preventDefault());
    b.addEventListener('mousedown', (e) => e.preventDefault());
    b.addEventListener('click', () => {
      action(b);
      term.focus();
    });
    keys.append(b);
  }

  function toggle(name) {
    return (b) => {
      mods[name] = !mods[name];
      b.setAttribute('aria-pressed', String(mods[name]));
    };
  }

  key('Esc', 'Escape', () => send(withMods('\x1b')));
  key('Tab', 'Tab', () => send(withMods('\t')));
  key('Ctrl', 'Control, applies to the next key', toggle('ctrl'), { 'aria-pressed': 'false' });
  key('Alt', 'Alt, applies to the next key', toggle('alt'), { 'aria-pressed': 'false' });
  for (const [, letter, name, glyph] of ARROWS) key(glyph, name, () => send(arrow(letter)));
  key('|', 'Pipe', () => send(withMods('|')));
  key('~', 'Tilde', () => send(withMods('~')));
  key('/', 'Slash', () => send(withMods('/')));
  key('-', 'Dash', () => send(withMods('-')));

  // Connection.
  function clearTimers() {
    clearTimeout(retryTimer);
    clearInterval(countdownTimer);
    retryTimer = 0;
    countdownTimer = 0;
  }

  function connectNow() {
    if (disposed || phase === 'ended') return;
    clearTimers();
    backoff = BACKOFF_START;
    if (ws && (phase === 'connecting' || phase === 'pre' || phase === 'live')) {
      // Drop a socket that may be dead; onclose will not schedule another try.
      const old = ws;
      ws = null;
      old.onclose = null;
      old.close();
    }
    connect();
  }

  function connect() {
    if (disposed || phase === 'ended') return;
    phase = 'connecting';
    pre = [];
    setStatus(lastError ? 'Reconnecting' : 'Connecting', 'connecting');
    let url;
    try {
      fit.fit();
    } catch {
      // No size yet; the server gets the defaults below.
    }
    url = terminalUrl(machineId, {
      session,
      cols: term.cols || 80,
      rows: term.rows || 24,
      cwd,
      resume: offset,
    });
    let opened = false;
    const sock = new WebSocket(url);
    sock.binaryType = 'arraybuffer';
    ws = sock;
    sock.onopen = () => {
      opened = true;
      phase = 'pre';
    };
    sock.onmessage = (ev) => {
      if (ws !== sock) return;
      if (typeof ev.data === 'string') return onControl(ev.data);
      const bytes = new Uint8Array(ev.data);
      if (phase === 'pre') {
        // Restores terminal modes; counts toward no offset.
        pre.push(bytes);
        return;
      }
      if (phase !== 'live') return;
      term.write(bytes);
      scrollback.append(bytes);
      offset += bytes.length;
    };
    sock.onclose = () => {
      if (ws !== sock) return;
      ws = null;
      if (disposed || phase === 'ended') return;
      setLive(false);
      phase = 'waiting';
      if (!opened) probe();
      scheduleRetry();
    };
    sock.onerror = () => {
      // onclose follows and handles it.
    };
  }

  function onControl(text) {
    let msg;
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }
    if (msg.type === 'attached') {
      const n = Number(msg.offset) || 0;
      if (n !== offset || placeholder) {
        // The saved output does not end where the server resumes: start over from N.
        term.reset();
        scrollback.reset(n);
        offset = n;
        placeholder = false;
      }
      for (const b of pre) term.write(b);
      pre = [];
      phase = 'live';
      backoff = BACKOFF_START;
      lastError = '';
      setOffline(false);
      setLive(true);
      setStatus('Connected', 'live');
      if (cwd) {
        // The session exists now; a reload must not pass cwd again.
        cwd = '';
        try {
          history.replaceState(null, '', `#terminal/${encodeURIComponent(machineId)}/${encodeURIComponent(session)}`);
        } catch {
          // Leave the address as it is.
        }
      }
      if (document.activeElement === document.body || !document.activeElement) term.focus();
    } else if (msg.type === 'exit') {
      phase = 'ended';
      setLive(false);
      clearTimers();
      const code = msg.code === undefined || msg.code === null ? '' : ` with code ${msg.code}`;
      setStatus(`Session ended${code}`, 'ended');
      scrollback.flush();
      if (ws) {
        const old = ws;
        ws = null;
        old.close();
      }
    } else if (msg.type === 'error') {
      lastError = msg.message || 'Error';
      setStatus(lastError, 'error');
    }
  }

  // The socket never opened. Ask the API why: a 401 sends the page to the login view,
  // a network error or a cached answer means the server cannot be reached.
  async function probe() {
    try {
      const { stale } = await api.me();
      setOffline(stale);
    } catch (e) {
      if (!(e instanceof ApiError)) setOffline(true);
    }
  }

  function scheduleRetry() {
    clearTimers();
    const delay = backoff;
    backoff = Math.min(backoff * 2, BACKOFF_MAX);
    retryAt = Date.now() + delay;
    const tick = () => {
      const left = Math.max(0, Math.ceil((retryAt - Date.now()) / 1000));
      const what = state.offline || !navigator.onLine
        ? (haveSaved ? 'Offline, saved output' : 'Offline')
        : (lastError || 'Disconnected');
      setStatus(`${what}. Reconnecting in ${left} s`, state.offline ? 'offline' : 'waiting');
    };
    tick();
    countdownTimer = setInterval(tick, 1000);
    retryTimer = setTimeout(() => {
      clearInterval(countdownTimer);
      countdownTimer = 0;
      connect();
    }, delay);
  }

  function onOnline() {
    if (phase === 'waiting' || phase === 'idle') connectNow();
  }

  function onVisibility() {
    if (document.visibilityState === 'hidden') {
      hiddenSince = Date.now();
      scrollback.flush();
      return;
    }
    const wasLong = hiddenSince && Date.now() - hiddenSince > STALE_AFTER_HIDDEN;
    hiddenSince = 0;
    if (phase === 'waiting') connectNow();
    else if (phase === 'live' && wasLong) connectNow();
  }

  function onPageHide() {
    scrollback.flush();
  }

  const scheme = window.matchMedia('(prefers-color-scheme: dark)');
  function onScheme() {
    term.options.theme = termTheme();
  }

  window.addEventListener('online', onOnline);
  document.addEventListener('visibilitychange', onVisibility);
  window.addEventListener('pagehide', onPageHide);
  if (scheme.addEventListener) scheme.addEventListener('change', onScheme);

  placeView();
  setLive(false);
  setStatus('Loading saved output', 'connecting');
  scrollback.load().then(() => {
    if (disposed) return;
    if (scrollback.size) {
      haveSaved = true;
      term.write(scrollback.bytes());
    }
    offset = scrollback.end;
    if (!scrollback.size && (state.offline || !navigator.onLine)) {
      term.write('\x1b[2mNothing saved for this session on this device.\x1b[0m\r\n');
      placeholder = true;
    }
    connect();
  });

  return () => {
    disposed = true;
    clearTimers();
    if (fitFrame) cancelAnimationFrame(fitFrame);
    if (ws) {
      const old = ws;
      ws = null;
      old.onclose = null;
      old.close();
    }
    scrollback.flush();
    window.removeEventListener('online', onOnline);
    document.removeEventListener('visibilitychange', onVisibility);
    window.removeEventListener('pagehide', onPageHide);
    window.removeEventListener('resize', placeView);
    if (scheme.removeEventListener) scheme.removeEventListener('change', onScheme);
    if (vv) {
      vv.removeEventListener('resize', placeView);
      vv.removeEventListener('scroll', placeView);
    }
    if (ro) ro.disconnect();
    term.dispose();
    document.body.classList.remove('terminal-mode');
  };
}

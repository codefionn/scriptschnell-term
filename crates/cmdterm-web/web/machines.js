// The machine list (#machines) and one machine's projects and sessions (#machine/{id}).

import { api, ApiError, NetworkError } from './api.js';
import { h, clear, relativeTime, fullTime, confirmInline, busy, copyText } from './dom.js';
import { state, setOffline, setTitle, href, terminalHref, notice, errorText, onOfflineChange } from './shell.js';
import { loadMachines, saveMachines, cachedMachine, forgetScrollback } from './store.js';

const REFRESH_MS = 20000;

// Fetches the machine list. Falls back to the copy in localStorage when the server
// cannot be reached. Returns { machines, stale } or throws ApiError.
async function fetchMachines() {
  try {
    const { data, stale } = await api.machines();
    const machines = Array.isArray(data) ? data : [];
    if (!stale) saveMachines(machines);
    setOffline(stale);
    return { machines, stale };
  } catch (e) {
    if (!(e instanceof NetworkError)) throw e;
    setOffline(true);
    const cached = loadMachines();
    return { machines: cached ? cached.machines : [], stale: true, missing: !cached };
  }
}

function statusDot(online) {
  return h('span', {
    class: online ? 'dot dot-on' : 'dot',
    role: 'img',
    'aria-label': online ? 'Online' : 'Offline',
  });
}

function seenText(m) {
  if (m.online) return 'Online';
  return m.last_seen ? `Last seen ${relativeTime(m.last_seen)}` : 'Never connected';
}

// Repeats `fn` while the tab is visible. Returns a stop function.
function every(ms, fn) {
  const id = setInterval(() => {
    if (document.visibilityState === 'visible') fn();
  }, ms);
  return () => clearInterval(id);
}

// Wraps an async function so a call while one runs is dropped.
function serial(fn) {
  let running = false;
  return async () => {
    if (running) return;
    running = true;
    try {
      await fn();
    } finally {
      running = false;
    }
  };
}

export function renderMachines(root) {
  setTitle('Machines');
  let machines = (loadMachines() || { machines: [] }).machines;
  let stale = state.offline;
  let pairPanel = null;
  let pairTimer = 0;
  // Ids with a rename or remove step open, so a refresh does not throw away the input.
  const editing = new Set();

  const linkBtn = h('button', { type: 'button', class: 'primary', onclick: () => startPairing() }, 'Link a machine');
  const head = h('div', { class: 'view-head' }, h('h1', null, 'Machines'), linkBtn);
  const pairSlot = h('div');
  const msg = h('div');
  const list = h('ul', { class: 'list', 'aria-label': 'Machines' });
  root.append(head, pairSlot, msg, list);

  function draw() {
    linkBtn.hidden = stale;
    clear(list);
    if (!machines.length) {
      list.append(
        h('li', { class: 'empty' }, stale
          ? 'No machines saved on this device.'
          : 'No machines yet. Link one to open its terminals here.'),
      );
      return;
    }
    for (const m of machines) list.append(machineRow(m));
  }

  function machineRow(m) {
    const li = h('li', { class: 'row' });
    const main = h('a', { class: 'row-main', href: href('machine', m.id) },
      h('span', { class: 'row-title' }, statusDot(m.online), h('span', { class: 'name' }, m.name || m.id)),
      h('span', { class: 'row-meta', title: m.last_seen ? fullTime(m.last_seen) : null }, seenText(m)),
      m.fingerprint ? h('span', { class: 'mono small fingerprint', title: 'Host key fingerprint' }, m.fingerprint) : null,
    );
    li.append(main);
    if (!stale) {
      const rename = h('button', { type: 'button', 'aria-label': `Rename ${m.name}` }, 'Rename');
      const remove = h('button', { type: 'button', class: 'danger-quiet', 'aria-label': `Remove ${m.name}` }, 'Remove');
      rename.addEventListener('click', () => startRename(li, main, m));
      remove.addEventListener('click', async () => {
        editing.add(m.id);
        const ok = await confirmInline(remove, {
          message: `Remove ${m.name}? Its key is refused from then on.`,
          confirm: 'Remove',
        });
        editing.delete(m.id);
        if (!ok) return;
        try {
          await api.removeMachine(m.id);
          machines = machines.filter((x) => x.id !== m.id);
          saveMachines(machines);
          draw();
        } catch (e) {
          showError(e);
        }
      });
      li.append(h('span', { class: 'row-actions' }, rename, remove));
    }
    return li;
  }

  function startRename(li, main, m) {
    editing.add(m.id);
    const input = h('input', { type: 'text', value: m.name || '', 'aria-label': 'Machine name', required: true, maxlength: '100', autocomplete: 'off' });
    const save = h('button', { type: 'submit', class: 'primary' }, 'Save');
    const cancel = h('button', { type: 'button' }, 'Cancel');
    const form = h('form', { class: 'inline-form' }, input, h('span', { class: 'row-actions' }, cancel, save));
    const actions = li.querySelector('.row-actions');
    const finish = () => {
      editing.delete(m.id);
      draw();
    };
    cancel.addEventListener('click', finish);
    form.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') finish();
    });
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      const name = input.value.trim();
      if (!name) return;
      await busy(save, async () => {
        try {
          await api.renameMachine(m.id, name);
          m.name = name;
          saveMachines(machines);
          finish();
        } catch (err) {
          showError(err);
        }
      });
    });
    main.replaceWith(form);
    if (actions) actions.remove();
    input.focus();
    input.select();
  }

  function showError(e) {
    clear(msg);
    msg.append(notice(errorText(e)));
  }

  async function startPairing() {
    clear(msg);
    await busy(linkBtn, async () => {
      try {
        const { data } = await api.pair();
        showPairing(data);
      } catch (e) {
        showError(e);
      }
    });
  }

  function closePairing() {
    clearInterval(pairTimer);
    pairTimer = 0;
    if (pairPanel) pairPanel.remove();
    pairPanel = null;
    linkBtn.focus();
  }

  function showPairing(p) {
    closePairing();
    // `expires` is a Unix time in seconds. A small number is taken as seconds from now.
    const expires = p.expires > 1e9 ? p.expires * 1000 : Date.now() + p.expires * 1000;
    const countdown = h('span', { class: 'countdown', 'aria-live': 'polite' });
    const copyBtn = h('button', { type: 'button' }, 'Copy');
    const command = h('code', { class: 'mono command' }, p.command);
    copyBtn.addEventListener('click', async () => {
      const ok = await copyText(p.command);
      copyBtn.textContent = ok ? 'Copied' : 'Copy failed';
      setTimeout(() => (copyBtn.textContent = 'Copy'), 2000);
    });
    const again = h('button', { type: 'button', hidden: true, onclick: () => startPairing() }, 'New code');
    const close = h('button', { type: 'button', class: 'quiet', onclick: closePairing }, 'Close');
    pairPanel = h('section', { class: 'panel pair', 'aria-label': 'Link a machine' },
      h('h2', null, 'Link a machine'),
      h('p', null, 'Run this on the machine. The code works once.'),
      h('div', { class: 'pair-code mono', 'aria-label': `Code ${p.code}` }, p.code),
      h('div', { class: 'command-box' }, command, copyBtn),
      h('p', { class: 'row-meta' }, countdown),
      h('div', { class: 'actions' }, again, close),
    );
    const tick = () => {
      const left = Math.round((expires - Date.now()) / 1000);
      if (left <= 0) {
        countdown.textContent = 'This code has expired.';
        pairPanel.classList.add('expired');
        again.hidden = false;
        clearInterval(pairTimer);
        pairTimer = 0;
        return;
      }
      const mm = Math.floor(left / 60);
      const ss = String(left % 60).padStart(2, '0');
      countdown.textContent = `Expires in ${mm}:${ss}`;
    };
    tick();
    pairTimer = setInterval(tick, 1000);
    pairSlot.append(pairPanel);
    copyBtn.focus();
  }

  const refresh = serial(async () => {
    try {
      const r = await fetchMachines();
      // Keep an open rename or remove step; the next refresh picks up the change.
      if (editing.size) return;
      machines = r.machines;
      stale = r.stale;
      clear(msg);
      if (r.missing) msg.append(notice('Offline, and nothing is saved on this device yet.', 'info'));
      draw();
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 401)) showError(e);
    }
  });

  draw();
  refresh();
  const stopTimer = every(REFRESH_MS, refresh);
  const stopOffline = onOfflineChange((off) => {
    if (!off) refresh();
  });
  return () => {
    stopTimer();
    stopOffline();
    clearInterval(pairTimer);
  };
}

function randomSuffix() {
  const a = new Uint8Array(4);
  crypto.getRandomValues(a);
  return Array.from(a, (b) => (b % 36).toString(36)).join('');
}

// Session names go into URLs and a remote file name, so keep them to a safe set.
function sessionName(base) {
  const clean = String(base || 'shell')
    .replace(/[^A-Za-z0-9._-]+/g, '-')
    .replace(/^[-.]+|-+$/g, '')
    .slice(0, 40);
  return `${clean || 'shell'}-${randomSuffix()}`;
}

// The machine list carries `projects` and `sessions` as the machine reported them.
// Entries are taken as the objects the live endpoints return; plain strings are
// accepted as names.
function listOf(v, alsoKey) {
  if (!Array.isArray(v)) return null;
  return v.map((x) => {
    if (typeof x !== 'string') return x;
    const o = { name: x };
    if (alsoKey) o[alsoKey] = '';
    return o;
  });
}

function sessionStatus(s) {
  if (s.exited === true || typeof s.exited === 'number') {
    return typeof s.exited === 'number' ? `Exited with code ${s.exited}` : 'Exited';
  }
  if (typeof s.attached === 'number') {
    if (s.attached === 0) return 'Detached';
    return s.attached === 1 ? 'Attached on 1 device' : `Attached on ${s.attached} devices`;
  }
  return s.attached ? 'Attached' : 'Detached';
}

export function renderMachine(root, route) {
  const id = route.parts[0] || '';
  let machine = cachedMachine(id);
  let projects = (machine && listOf(machine.projects, 'root')) || [];
  let sessions = (machine && listOf(machine.sessions)) || [];
  let projectsLive = false;
  let sessionsLive = false;
  let busySessions = new Set();

  const title = h('h1', null, machine ? machine.name : 'Machine');
  const status = h('p', { class: 'row-meta machine-status' });
  const newBtn = h('a', { class: 'button primary' }, 'New terminal');
  const head = h('div', { class: 'view-head' },
    h('div', null, h('a', { class: 'back', href: '#machines' }, 'Machines'), title, status),
    newBtn,
  );
  const msg = h('div');
  const projList = h('ul', { class: 'list', 'aria-label': 'Projects' });
  const sessList = h('ul', { class: 'list', 'aria-label': 'Sessions' });
  const projNote = h('p', { class: 'row-meta section-note' });
  const sessNote = h('p', { class: 'row-meta section-note' });
  root.append(
    head,
    msg,
    h('section', { class: 'section' }, h('h2', null, 'Sessions'), sessNote, sessList),
    h('section', { class: 'section' }, h('h2', null, 'Projects'), projNote, projList),
  );

  // New terminals need the server and the machine.
  const canStart = () => !state.offline && machine && machine.online;

  function draw() {
    setTitle(machine ? machine.name : 'Machine');
    title.textContent = machine ? machine.name || machine.id : 'Unknown machine';
    clear(status);
    if (machine) {
      status.append(statusDot(machine.online), ' ', seenText(machine));
    }
    if (canStart()) {
      newBtn.hidden = false;
      newBtn.href = terminalHref(id, sessionName('shell'));
    } else {
      newBtn.hidden = true;
    }

    projNote.textContent = projectsLive ? '' : 'What the machine reported when it was last online.';
    projNote.hidden = projectsLive || !projects.length;
    clear(projList);
    if (!projects.length) projList.append(h('li', { class: 'empty' }, 'No projects.'));
    for (const p of projects) {
      const li = h('li', { class: 'row' },
        h('div', { class: 'row-main' },
          h('span', { class: 'row-title' }, h('span', { class: 'name' }, p.name), p.vcs ? h('span', { class: 'badge' }, p.vcs) : null),
          h('span', { class: 'mono small' }, p.root),
        ),
      );
      if (canStart()) {
        li.append(h('span', { class: 'row-actions' },
          h('a', {
            class: 'button',
            href: terminalHref(id, sessionName(p.name), p.root),
            'aria-label': `New terminal in ${p.name}`,
          }, 'New terminal'),
        ));
      }
      projList.append(li);
    }

    sessNote.textContent = sessionsLive ? '' : 'What the machine reported when it was last online.';
    sessNote.hidden = sessionsLive || !sessions.length;
    clear(sessList);
    if (!sessions.length) sessList.append(h('li', { class: 'empty' }, 'No sessions.'));
    for (const s of sessions) sessList.append(sessionRow(s));
  }

  function sessionRow(s) {
    const live = !state.offline && machine && machine.online;
    const open = h('a', {
      class: 'row-main',
      href: terminalHref(id, s.name),
    },
    h('span', { class: 'row-title' }, h('span', { class: 'name mono' }, s.name)),
    h('span', { class: 'row-meta' }, sessionStatus(s)),
    );
    const li = h('li', { class: 'row' }, open);
    const actions = h('span', { class: 'row-actions' });
    actions.append(h('a', {
      class: 'button',
      href: terminalHref(id, s.name),
      'aria-label': live ? `Attach to ${s.name}` : `View saved output of ${s.name}`,
    }, live ? 'Attach' : 'Saved output'));
    if (live) {
      const end = h('button', { type: 'button', class: 'danger-quiet', 'aria-label': `End ${s.name}` }, 'End');
      end.addEventListener('click', async () => {
        busySessions.add(s.name);
        const ok = await confirmInline(end, { message: `End ${s.name}? Its shell is stopped.`, confirm: 'End session' });
        busySessions.delete(s.name);
        if (!ok) return;
        try {
          await api.endSession(id, s.name);
          sessions = sessions.filter((x) => x.name !== s.name);
          forgetScrollback(id, s.name);
          draw();
        } catch (e) {
          clear(msg);
          msg.append(notice(errorText(e)));
        }
      });
      actions.append(end);
    }
    li.append(actions);
    return li;
  }

  const refresh = serial(async () => {
    clear(msg);
    try {
      const r = await fetchMachines();
      const m = r.machines.find((x) => x.id === id);
      if (m) machine = m;
      else if (!r.stale) {
        machine = null;
        msg.append(notice('This machine is not linked to your account.'));
      }
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 401)) msg.append(notice(errorText(e)));
      draw();
      return;
    }
    if (machine && !projectsLive) projects = listOf(machine.projects, 'root') || projects;
    if (machine && !sessionsLive) sessions = listOf(machine.sessions) || sessions;
    if (state.offline || !machine) {
      projectsLive = sessionsLive = false;
      draw();
      return;
    }
    const [p, s] = await Promise.allSettled([api.projects(id), api.sessions(id)]);
    projectsLive = p.status === 'fulfilled' && Array.isArray(p.value.data) && !p.value.stale;
    if (projectsLive) projects = p.value.data;
    // Keep an open "End" step: replacing the rows would drop it.
    if (!busySessions.size) {
      sessionsLive = s.status === 'fulfilled' && Array.isArray(s.value.data) && !s.value.stale;
      if (sessionsLive) sessions = s.value.data;
    } else {
      return;
    }
    if ([p, s].some((r) => r.status === 'rejected' && r.reason instanceof NetworkError)) {
      // The server went away between the requests; the banner says so.
      setOffline(true);
      draw();
      return;
    }
    for (const r of [p, s]) {
      if (r.status === 'rejected' && !(r.reason instanceof ApiError && (r.reason.status === 503 || r.reason.status === 401))) {
        clear(msg);
        msg.append(notice(errorText(r.reason)));
      }
    }
    draw();
  });

  draw();
  refresh();
  const stopTimer = every(REFRESH_MS, () => {
    if (!busySessions.size) refresh();
  });
  const stopOffline = onOfflineChange((off) => {
    if (!off) refresh();
    else draw();
  });
  return () => {
    stopTimer();
    stopOffline();
  };
}

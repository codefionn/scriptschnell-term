// Entry point: hash routes, the login and account views, and the service worker.
//
// Routes: #login, #machines, #machine/{id}, #terminal/{machineId}/{session}[?cwd=...],
// #account. Every part after the route name is URI-encoded.

import { api, ApiError, NetworkError, oidcStartUrl, setUnauthorizedHandler } from './api.js';
import { h, clear, confirmInline, busy } from './dom.js';
import { state, setOffline, setTitle, parseHash, go, notice, errorText, onOfflineChange } from './shell.js';
import { loadUser, saveUser, clearAll } from './store.js';
import { renderMachines, renderMachine } from './machines.js';
import { renderTerminal } from './terminal.js';

const viewEl = document.getElementById('view');
const navEl = document.getElementById('nav');
let cleanup = null;

setUnauthorizedHandler(() => {
  state.user = null;
  if (parseHash().name !== 'login') go('#login');
});

async function authOptions() {
  if (state.options) return state.options;
  const { data } = await api.authOptions();
  state.options = {
    password: data && data.password !== false,
    providers: (data && Array.isArray(data.providers)) ? data.providers : [],
    dev: !!(data && data.dev),
  };
  return state.options;
}

function providerName(id) {
  const p = state.options && state.options.providers.find((x) => x.id === id);
  return p ? p.name : id;
}

// Login view.

function renderLogin(root, route) {
  setTitle('Log in');
  const error = route.query.get('error');
  const msg = h('div');
  const body = h('div', { class: 'login-body' }, h('p', { class: 'row-meta' }, 'Loading'));
  root.append(
    h('section', { class: 'panel login' },
      h('div', { class: 'brand-mark', 'aria-hidden': 'true' }, h('img', { src: 'icon.svg', alt: '', width: '40', height: '40' })),
      h('h1', null, 'Log in to scriptschnell-term'),
      msg,
      body,
    ),
  );
  if (error) msg.append(notice(error));

  let retryTimer = 0;
  let lockedUntil = 0;

  authOptions().then((opts) => {
    clear(body);
    if (opts.password) body.append(passwordForm());
    if (opts.providers.length) {
      if (opts.password) body.append(h('p', { class: 'divider' }, h('span', null, 'or')));
      const list = h('div', { class: 'sso' });
      for (const p of opts.providers) {
        list.append(h('a', { class: 'button', href: oidcStartUrl(p.id) }, `Continue with ${p.name}`));
      }
      body.append(list);
    }
    if (!opts.password && !opts.providers.length) {
      body.append(notice('This server offers no way to log in.', 'info'));
    }
    if (opts.dev) {
      body.append(h('p', { class: 'row-meta' }, 'Development server: open the login link it printed to log in as dev.'));
    }
  }).catch((e) => {
    clear(body);
    body.append(notice(e instanceof NetworkError ? 'Cannot reach the server. Logging in needs a connection.' : errorText(e)));
    const again = h('button', { type: 'button', onclick: () => go('#login') }, 'Try again');
    body.append(again);
  });

  function passwordForm() {
    const user = h('input', { id: 'login-user', name: 'username', type: 'text', autocomplete: 'username', autocapitalize: 'none', spellcheck: 'false', required: true });
    const pass = h('input', { id: 'login-pass', name: 'password', type: 'password', autocomplete: 'current-password', required: true });
    const submit = h('button', { type: 'submit', class: 'primary wide' }, 'Log in');
    const saved = loadUser();
    if (saved && saved.name) user.value = saved.name;
    const form = h('form', { class: 'form' },
      h('label', { for: 'login-user' }, 'Username'), user,
      h('label', { for: 'login-pass' }, 'Password'), pass,
      submit,
    );
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clear(msg);
      await busy(submit, async () => {
        try {
          const { data } = await api.login(user.value.trim(), pass.value);
          await loggedIn(data && data.name);
        } catch (err) {
          pass.value = '';
          if (err instanceof ApiError && err.status === 401) {
            msg.append(notice('Invalid username or password.'));
          } else if (err instanceof ApiError && err.status === 429) {
            lockOut(submit, Number(err.body.retry_after) || 60);
          } else {
            msg.append(notice(errorText(err)));
          }
        }
      });
      if (Date.now() < lockedUntil) submit.disabled = true;
      else pass.focus();
    });
    queueMicrotask(() => (user.value ? pass : user).focus());
    return form;
  }

  function lockOut(submit, seconds) {
    clearInterval(retryTimer);
    const until = Date.now() + seconds * 1000;
    lockedUntil = until;
    const line = notice('', 'info');
    msg.append(line);
    const tick = () => {
      const left = Math.ceil((until - Date.now()) / 1000);
      if (left <= 0) {
        clearInterval(retryTimer);
        line.remove();
        submit.disabled = false;
        return;
      }
      line.textContent = `Too many failed logins. Try again in ${left} s.`;
      submit.disabled = true;
    };
    tick();
    retryTimer = setInterval(tick, 1000);
  }

  return () => clearInterval(retryTimer);
}

async function loggedIn(name) {
  const prev = loadUser();
  // Another account on this device: drop what the last one left behind.
  if (prev && name && prev.name !== name) await clearAll();
  saveUser({ name });
  state.user = null;
  go('#machines');
}

// Account view.

function renderAccount(root) {
  setTitle('Account');
  const msg = h('div');
  const content = h('div');
  root.append(h('div', { class: 'view-head' }, h('h1', null, 'Account')), msg, content);

  async function load() {
    clear(msg);
    let me;
    let stale = false;
    try {
      const r = await api.me();
      me = r.data;
      stale = r.stale;
      setOffline(stale);
      saveUser({ name: me.name });
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return;
      if (e instanceof NetworkError) setOffline(true);
      const saved = loadUser();
      if (!saved) {
        msg.append(notice(errorText(e)));
        return;
      }
      me = { name: saved.name, has_password: null, identities: null };
      stale = true;
    }
    state.user = me;
    try {
      await authOptions();
    } catch {
      // Provider names fall back to their ids.
    }
    draw(me, stale);
  }

  function draw(me, stale) {
    clear(content);
    content.append(
      h('section', { class: 'section' },
        h('h2', null, 'Name'),
        h('p', { class: 'account-name' }, me.name),
      ),
    );
    if (stale) {
      content.append(notice('Offline. Account changes need a connection.', 'info'));
    } else {
      content.append(passwordSection(me), identitiesSection(me));
    }
    const logout = h('button', { type: 'button', class: 'danger-quiet' }, 'Log out');
    logout.addEventListener('click', () => busy(logout, async () => {
      try {
        await api.logout();
      } catch {
        // Offline: still forget everything on this device.
      }
      await clearAll();
      state.user = null;
      go('#login');
    }));
    content.append(h('section', { class: 'section' }, h('h2', null, 'Session'),
      h('p', { class: 'row-meta' }, 'Logging out also removes the machine list and saved terminal output from this device.'),
      logout));
  }

  function passwordSection(me) {
    const sectionMsg = h('div');
    const fields = [];
    let current = null;
    if (me.has_password) {
      current = h('input', { id: 'pw-current', type: 'password', autocomplete: 'current-password', required: true });
      fields.push(h('label', { for: 'pw-current' }, 'Current password'), current);
    }
    const next = h('input', { id: 'pw-new', type: 'password', autocomplete: 'new-password', required: true, minlength: '8' });
    const again = h('input', { id: 'pw-again', type: 'password', autocomplete: 'new-password', required: true });
    const submit = h('button', { type: 'submit', class: 'primary' }, me.has_password ? 'Change password' : 'Set password');
    fields.push(h('label', { for: 'pw-new' }, 'New password'), next, h('label', { for: 'pw-again' }, 'Repeat new password'), again);
    const form = h('form', { class: 'form' }, ...fields, submit);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      clear(sectionMsg);
      if (next.value !== again.value) {
        sectionMsg.append(notice('The new passwords do not match.'));
        again.focus();
        return;
      }
      await busy(submit, async () => {
        try {
          await api.setPassword(current ? current.value : '', next.value);
          form.reset();
          sectionMsg.append(notice('Password saved.', 'ok'));
          if (!me.has_password) {
            me.has_password = true;
            load();
          }
        } catch (err) {
          sectionMsg.append(notice(errorText(err)));
        }
      });
    });
    return h('section', { class: 'section' },
      h('h2', null, 'Password'),
      me.has_password ? null : h('p', { class: 'row-meta' }, 'This account has no password yet. Set one to log in without single sign-on.'),
      sectionMsg,
      form,
    );
  }

  function identitiesSection(me) {
    const sectionMsg = h('div');
    const identities = Array.isArray(me.identities) ? me.identities : [];
    const list = h('ul', { class: 'list', 'aria-label': 'Connected identities' });
    if (!identities.length) list.append(h('li', { class: 'empty' }, 'No single sign-on identities connected.'));
    for (const idn of identities) {
      const name = providerName(idn.provider);
      const remove = h('button', { type: 'button', class: 'danger-quiet', 'aria-label': `Remove ${name} identity` }, 'Remove');
      remove.addEventListener('click', async () => {
        const ok = await confirmInline(remove, { message: `Remove ${name}? You can no longer log in with it.`, confirm: 'Remove' });
        if (!ok) return;
        try {
          await api.removeIdentity(idn.provider);
          load();
        } catch (err) {
          clear(sectionMsg);
          sectionMsg.append(notice(errorText(err)));
        }
      });
      list.append(h('li', { class: 'row' },
        h('div', { class: 'row-main' },
          h('span', { class: 'row-title' }, h('span', { class: 'name' }, name)),
          h('span', { class: 'row-meta' }, idn.email || ''),
        ),
        h('span', { class: 'row-actions' }, remove),
      ));
    }
    const connected = new Set(identities.map((i) => i.provider));
    const more = (state.options ? state.options.providers : []).filter((p) => !connected.has(p.id));
    const connect = more.length
      ? h('div', { class: 'sso' }, more.map((p) => h('a', { class: 'button', href: oidcStartUrl(p.id) }, `Connect ${p.name}`)))
      : null;
    return h('section', { class: 'section' }, h('h2', null, 'Single sign-on'), sectionMsg, list, connect);
  }

  load();
  const stop = onOfflineChange((off) => {
    if (!off) load();
  });
  return stop;
}

// Routing.

const routes = {
  login: renderLogin,
  machines: renderMachines,
  machine: renderMachine,
  terminal: renderTerminal,
  account: renderAccount,
};

function render() {
  const route = parseHash();
  if (!routes[route.name]) {
    location.replace('#machines');
    return;
  }
  if (cleanup) {
    try {
      cleanup();
    } catch {
      // A view that fails to clean up must not block the next one.
    }
    cleanup = null;
  }
  clear(viewEl);
  window.scrollTo(0, 0);
  const isLogin = route.name === 'login';
  navEl.hidden = isLogin;
  for (const a of navEl.querySelectorAll('a')) {
    const target = a.getAttribute('href').slice(1);
    const active = target === route.name || (target === 'machines' && route.name === 'machine');
    if (active) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }
  cleanup = routes[route.name](viewEl, route) || null;
  // Move focus to the new view for screen readers, except in the terminal, which takes
  // focus itself.
  if (route.name !== 'terminal') {
    const heading = viewEl.querySelector('h1');
    if (heading) {
      heading.tabIndex = -1;
      heading.focus({ preventScroll: true });
    }
  }
}

window.addEventListener('hashchange', render);
window.addEventListener('online', () => setOffline(false));
window.addEventListener('offline', () => setOffline(true));

if (!navigator.onLine) setOffline(true);
render();

if ('serviceWorker' in navigator) {
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('sw.js', { scope: './' }).catch(() => {
      // Without the worker the app still runs, only not offline.
    });
  });
}

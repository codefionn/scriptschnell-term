// Small helpers for building views without a framework.

// h('button', { class: 'x', onclick: fn }, 'Label', child, ...)
export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v === null || v === undefined || v === false) continue;
      if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
      else if (k === 'class') el.className = v;
      else if (k === 'dataset') Object.assign(el.dataset, v);
      else if (k in el && typeof v !== 'string') el[k] = v;
      else el.setAttribute(k, v === true ? '' : v);
    }
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    if (Array.isArray(c)) append(el, c);
    else el.append(c instanceof Node ? c : String(c));
  }
}

export function clear(el) {
  while (el.firstChild) el.firstChild.remove();
}

const rtf = typeof Intl !== 'undefined' && Intl.RelativeTimeFormat
  ? new Intl.RelativeTimeFormat('en', { numeric: 'auto' })
  : null;

// "2 minutes ago" for a Unix time in seconds.
export function relativeTime(unix) {
  if (!unix) return 'never';
  const diff = Math.round(unix - Date.now() / 1000);
  const abs = Math.abs(diff);
  const steps = [
    [60, 1, 'second'],
    [3600, 60, 'minute'],
    [86400, 3600, 'hour'],
    [86400 * 30, 86400, 'day'],
    [86400 * 365, 86400 * 30, 'month'],
    [Infinity, 86400 * 365, 'year'],
  ];
  if (abs < 10) return 'just now';
  for (const [limit, div, unit] of steps) {
    if (abs < limit) {
      const n = Math.round(diff / div);
      return rtf ? rtf.format(n, unit) : `${Math.abs(n)} ${unit}s ago`;
    }
  }
  return '';
}

export function fullTime(unix) {
  return unix ? new Date(unix * 1000).toLocaleString() : '';
}

// A confirm step shown in place of `anchor` until the user answers. Resolves true or
// false. Used instead of window.confirm, which installed web apps on phones often block.
export function confirmInline(anchor, { message, confirm = 'Confirm', danger = true }) {
  return new Promise((resolve) => {
    const box = h('div', { class: 'confirm', role: 'group', 'aria-label': message });
    const done = (ok) => {
      box.replaceWith(anchor);
      resolve(ok);
      if (!ok) anchor.focus();
    };
    const yes = h('button', { type: 'button', class: danger ? 'danger' : 'primary', onclick: () => done(true) }, confirm);
    const no = h('button', { type: 'button', onclick: () => done(false) }, 'Cancel');
    box.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') done(false);
    });
    box.append(h('span', { class: 'confirm-text' }, message), h('span', { class: 'confirm-actions' }, no, yes));
    anchor.replaceWith(box);
    no.focus();
  });
}

// Disables `btn` while `fn` runs.
export async function busy(btn, fn) {
  btn.disabled = true;
  btn.setAttribute('aria-busy', 'true');
  try {
    return await fn();
  } finally {
    btn.disabled = false;
    btn.removeAttribute('aria-busy');
  }
}

export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Clipboard API refused (plain HTTP, no permission): select a hidden field instead.
    const ta = h('textarea', { class: 'offscreen', readonly: true });
    ta.value = text;
    document.body.append(ta);
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand('copy');
    } catch {
      ok = false;
    }
    ta.remove();
    return ok;
  }
}

// Bootstraps the page: loads metadata and settings, builds the dashboard,
// backfills live charts and follows the server-sent event stream.

import { Dashboard, el } from './cards.js';
import { store } from './charts.js';
import { prefs, duration } from './format.js';
import { setSmoothing } from './tween.js';
import { SettingsPanel } from './settings.js';

const FPS = { off: 1, low: 15, medium: 30, high: 60 };

const state = {
  meta: null,
  settings: null,
  instance: null,
  signature: '',
  range: 'live',
  connected: false,
  overall: 'ok',
  historyTimer: 0,
};

async function api(path, opts = {}) {
  const r = await fetch(path, { headers: { 'Content-Type': 'application/json' }, ...opts });
  if (!r.ok) {
    const t = await r.text().catch(() => '');
    throw new Error(t || `${r.status} ${r.statusText}`);
  }
  return r.json();
}

async function fetchHistory(keys, from, to, points) {
  const q = new URLSearchParams({ keys: keys.join(','), from, to, points });
  return api(`/api/history?${q}`);
}

async function saveSettings(patch) {
  const s = await api('/api/settings', { method: 'PUT', body: JSON.stringify(patch) });
  applySettings(s);
  return s;
}

const dash = new Dashboard(document.getElementById('grid'), {
  settings: () => state.settings,
  fetchHistory,
  setTempUnit: (u) => saveSettings({ display: { temp_unit: u } }).catch(() => {}),
});

const settingsPanel = new SettingsPanel(document.getElementById('settings'), {
  settings: () => state.settings,
  meta: () => state.meta,
  status: () => api('/api/status'),
  backups: () => api('/api/backups'),
  backupNow: () => api('/api/backups', { method: 'POST' }),
  save: saveSettings,
});

// ---------- banners ----------

const banners = document.getElementById('banners');
const bannerEls = new Map();

function banner(id, message, kind = 'warn', dismissible = false) {
  let b = bannerEls.get(id);
  if (!message) {
    if (b) b.remove();
    bannerEls.delete(id);
    return;
  }
  if (!b) {
    b = el('div', { class: `banner ${kind}`, role: kind === 'bad' ? 'alert' : 'status' });
    bannerEls.set(id, b);
    banners.append(b);
  }
  b.replaceChildren(el('span', {}, message));
  if (dismissible) {
    b.append(el('button', { type: 'button', class: 'banner-x', 'aria-label': 'Dismiss', onclick: () => banner(id, null) }, '×'));
  }
}

// ---------- header ----------

function renderHeader() {
  const h = state.meta.host || {};
  document.getElementById('hostname').textContent = h.hostname || 'hwstats';
  document.title = `${h.hostname || 'hwstats'} · hwstats`;
  const facts = document.getElementById('facts');
  const items = [
    ['Machine', [h.vendor, h.product].filter(Boolean).join(' ')],
    ['System', h.os],
    ['Kernel', h.kernel],
    ['Memory', h.mem_total ? `${Math.round(h.mem_total / 1073741824)} GiB` : null],
  ];
  facts.replaceChildren(...items.filter(([, v]) => v).map(([k, v]) => el('div', {}, el('dt', {}, k), el('dd', {}, v))));
  const up = el('dd', { id: 'uptime' }, '–');
  facts.append(el('div', {}, el('dt', {}, 'Up'), up));
}

function setConn(kind, label) {
  const c = document.getElementById('conn');
  c.dataset.state = kind;
  document.getElementById('conn-label').textContent = label;
}

function refreshConn() {
  if (!state.connected) return setConn('bad', 'Offline');
  if (state.overall !== 'ok' || !state.meta?.history_available) return setConn('warn', 'Degraded');
  return setConn('ok', 'Live');
}

// ---------- settings ----------

function applyTheme(theme) {
  const root = document.documentElement;
  if (theme === 'light' || theme === 'dark') root.dataset.theme = theme;
  else delete root.dataset.theme;
}

function applySettings(s) {
  const prev = state.settings;
  state.settings = s;
  const d = s.display;
  setSmoothing(d.smoothing);
  store.fps = FPS[d.smoothing] ?? 30;
  store.windowSec = d.live_window_s;
  const themeChanged = !prev || prev.display.theme !== d.theme;
  const unitChanged = prefs.tempUnit !== d.temp_unit;
  prefs.tempUnit = d.temp_unit;
  if (themeChanged) {
    applyTheme(d.theme);
    if (prev) requestAnimationFrame(() => dash.restyle());
  } else if (unitChanged) {
    dash.restyle();
  }
  if (unitChanged && prev) dash.refreshValues();
}

matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
  if (state.settings?.display.theme === 'auto') dash.restyle();
});

// ---------- data loading ----------

async function loadMeta() {
  state.meta = await api('/api/meta');
  store.offsetMs = state.meta.now_ms - Date.now();
  state.instance = state.meta.instance;
}

async function backfill() {
  const keys = dash.charts.flatMap((c) => c.keys());
  if (!keys.length) return;
  const since = Math.round(store.now() * 1000 - 905 * 1000);
  const uniq = Array.from(new Set(keys));
  for (let i = 0; i < uniq.length; i += 150) {
    const part = uniq.slice(i, i + 150);
    const q = new URLSearchParams({ keys: part.join(','), since_ms: since });
    const r = await api(`/api/live?${q}`);
    for (const [k, s] of Object.entries(r.series)) {
      for (let j = 0; j < s.t.length; j++) dash.apply(k, s.t[j] / 1000, s.v[j]);
    }
  }
}

async function rebuild() {
  await loadMeta();
  const snapshot = await api('/api/snapshot');
  state.signature = Dashboard.signature(state.meta.series);
  const scrollY = window.scrollY;
  store.clear();
  dash.build(state.meta, snapshot);
  window.scrollTo(0, scrollY);
  renderHeader();
  bindUptime();
  await backfill().catch(() => {});
  if (state.range !== 'live') dash.setRange(state.range);
  banner('history', state.meta.history_available ? null : 'History is off: the database could not be opened. Live values still work. See Settings, Status.', 'warn');
  refreshConn();
}

function bindUptime() {
  const up = document.getElementById('uptime');
  dash.bind('sys.uptime', (v) => { up.textContent = duration(v)[0]; });
}

let catalogTimer = 0;
async function onCatalog() {
  clearTimeout(catalogTimer);
  catalogTimer = setTimeout(async () => {
    try {
      const meta = await api('/api/meta');
      if (Dashboard.signature(meta.series) !== state.signature) await rebuild();
    } catch { /* retried on next catalog event */ }
  }, 1500);
}

// ---------- live stream ----------

let es = null;
let offlineTimer = 0;

function openStream() {
  if (es) es.close();
  es = new EventSource('/api/stream');
  es.addEventListener('hello', async (e) => {
    const h = JSON.parse(e.data);
    clearTimeout(offlineTimer);
    state.connected = true;
    banner('offline', null);
    if (state.instance && h.instance !== state.instance) {
      // The service restarted: reload everything.
      await rebuild();
    }
    refreshConn();
  });
  es.addEventListener('sample', (e) => dash.onSample(JSON.parse(e.data)));
  es.addEventListener('info', (e) => {
    const d = JSON.parse(e.data);
    dash.onInfo(d.c, d.info);
  });
  es.addEventListener('processes', (e) => dash.onProcesses(JSON.parse(e.data)));
  es.addEventListener('extra', (e) => {
    const d = JSON.parse(e.data);
    dash.onExtra(d.c, d.data);
  });
  es.addEventListener('catalog', () => onCatalog());
  es.addEventListener('settings', (e) => applySettings(JSON.parse(e.data)));
  es.addEventListener('status', () => {
    pollStatus();
    settingsPanel.refreshIfOpen();
  });
  es.addEventListener('resync', async () => {
    try {
      const snap = await api('/api/snapshot');
      for (const [k, [ms, v]] of Object.entries(snap.series)) dash.apply(k, ms / 1000, v);
    } catch { /* next event will catch up */ }
  });
  es.onerror = () => {
    clearTimeout(offlineTimer);
    offlineTimer = setTimeout(() => {
      state.connected = false;
      banner('offline', 'Lost connection to the hwstats service. Reconnecting automatically…', 'bad');
      refreshConn();
    }, 2500);
  };
}

async function pollStatus() {
  try {
    const s = await api('/api/status');
    state.overall = s.overall;
    const notes = s.notes || [];
    banner('notes', notes.length ? notes.join(' ') : null, 'warn', true);
    refreshConn();
  } catch { /* offline banner covers this */ }
}

// ---------- range ----------

function setRange(r) {
  state.range = r;
  document.querySelectorAll('.range button').forEach((b) => b.setAttribute('aria-checked', String(b.dataset.range === r)));
  document.body.classList.toggle('history-mode', r !== 'live');
  dash.setRange(r);
  clearInterval(state.historyTimer);
  if (r !== 'live') state.historyTimer = setInterval(() => dash.setRange(r), 60000);
}

document.querySelectorAll('.range button').forEach((b) => b.addEventListener('click', () => setRange(b.dataset.range)));
document.getElementById('open-settings').addEventListener('click', () => settingsPanel.open());

// ---------- start ----------

async function start() {
  try {
    const s = await api('/api/settings');
    applySettings(s);
    await rebuild();
    if (document.fonts && document.fonts.ready) {
      document.fonts.ready.then(() => {
        dash.restyle();
        dash.layout();
      });
    }
    openStream();
    pollStatus();
  } catch (e) {
    banner('boot', `Cannot reach the hwstats service (${e.message}). Retrying…`, 'bad');
    setConn('bad', 'Offline');
    setTimeout(start, 3000);
    return;
  }
  banner('boot', null);
}

start();

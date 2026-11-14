// Settings overlay: collector intervals, history retention, backups,
// display preferences, the hardware inventory (with rescan) and a status
// view. Every change is saved right away.

import { el } from './cards.js';
import { text, clock } from './format.js';

const INTERVALS = [250, 500, 1000, 2000, 3000, 5000, 10000, 15000, 30000, 60000];
const RESOLUTIONS = [1, 2, 5, 10, 15, 30, 60, 120, 300];

function secLabel(ms) {
  return ms < 1000 ? `${ms} ms` : `${ms / 1000} s`;
}

const HW_GROUPS = [
  ['machine', 'Computer'],
  ['cpu', 'Processor'],
  ['memory', 'Memory'],
  ['gpu', 'Graphics'],
  ['disk', 'Storage'],
  ['network', 'Network adapters'],
  ['power', 'Battery and power supplies'],
  ['sensor', 'Sensor chips'],
];

const FACT_LABELS = {
  cuda: 'CUDA',
  cuda_cores: 'CUDA cores',
  vbios: 'VBIOS',
  pcie_max: 'PCIe',
  memory_bus: 'memory bus',
  max_clock: 'max clock',
  compute_capability: 'compute capability',
  kind: 'type',
};

// Error text of a failed API call (JSON `{"error": ...}` bodies included).
function errText(e) {
  try {
    return JSON.parse(e.message).error || e.message;
  } catch {
    return e.message;
  }
}

function patchFor(path, value) {
  const parts = path.split('.');
  const root = {};
  let cur = root;
  parts.forEach((p, i) => {
    if (i === parts.length - 1) cur[p] = value;
    else cur = cur[p] = {};
  });
  return root;
}

export class SettingsPanel {
  constructor(dialog, api) {
    this.d = dialog;
    this.api = api;
    this.tab = 'collectors';
    this.statusData = null;
    this.body = dialog.querySelector('.dlg-body');
    this.note = dialog.querySelector('.dlg-note');
    this.tabs = dialog.querySelectorAll('[role="tab"]');
    this.tabs.forEach((t) => t.addEventListener('click', () => this.show(t.dataset.tab)));
    dialog.querySelector('.dlg-close').addEventListener('click', () => dialog.close());
    dialog.addEventListener('click', (e) => {
      if (e.target === dialog) dialog.close();
    });
    dialog.addEventListener('keydown', (e) => {
      if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
      if (!e.target.matches('[role="tab"]')) return;
      const list = Array.from(this.tabs);
      const i = list.indexOf(e.target) + (e.key === 'ArrowRight' ? 1 : -1);
      const next = list[(i + list.length) % list.length];
      next.focus();
      this.show(next.dataset.tab);
    });
  }

  async open(tab) {
    if (tab) this.tab = tab;
    if (!this.d.open) this.d.showModal();
    await this.show(this.tab);
  }

  async show(tab) {
    this.tab = tab;
    this.tabs.forEach((t) => {
      const on = t.dataset.tab === tab;
      t.setAttribute('aria-selected', String(on));
      t.tabIndex = on ? 0 : -1;
    });
    try {
      this.statusData = await this.api.status();
    } catch {
      this.statusData = null;
    }
    const fn = {
      collectors: () => this.renderCollectors(),
      history: () => this.renderHistory(),
      backups: () => this.renderBackups(),
      display: () => this.renderDisplay(),
      hardware: () => this.renderHardware(),
      status: () => this.renderStatus(),
    }[tab];
    this.body.replaceChildren();
    await fn();
  }

  refreshIfOpen() {
    if (this.d.open && this.tab === 'status') this.show('status');
  }

  // A `hardware` event: redraw the Hardware tab when it is visible.
  onHardware(data) {
    if (!this.d.open || this.tab !== 'hardware') return;
    const scroll = this.body.scrollTop;
    this.body.replaceChildren();
    this.renderHardware(data);
    this.body.scrollTop = scroll;
  }

  flash(msg, bad) {
    this.note.textContent = msg;
    this.note.classList.toggle('bad', !!bad);
    clearTimeout(this.noteTimer);
    if (!bad) this.noteTimer = setTimeout(() => { this.note.textContent = ''; }, 1800);
  }

  async save(path, value) {
    try {
      await this.api.save(patchFor(path, value));
      this.flash('Saved');
    } catch (e) {
      this.flash(`Not saved: ${e.message}`, true);
    }
  }

  field(label, control, help) {
    const id = `f-${Math.random().toString(36).slice(2, 8)}`;
    control.id = id;
    const row = el('div', { class: 'field' }, el('label', { for: id }, label), control);
    if (help) row.append(el('p', { class: 'help' }, help));
    return row;
  }

  select(options, value, onChange) {
    const s = el('select');
    for (const [v, l] of options) {
      const o = el('option', { value: String(v) }, l);
      if (String(v) === String(value)) o.selected = true;
      s.append(o);
    }
    s.addEventListener('change', () => onChange(s.value));
    return s;
  }

  number(value, min, max, onChange) {
    const i = el('input', { type: 'number', min, max, step: 1, value, inputmode: 'numeric' });
    i.addEventListener('change', () => {
      const v = Math.round(Number(i.value));
      if (Number.isFinite(v)) onChange(Math.min(max, Math.max(min, v)));
    });
    return i;
  }

  radios(name, options, value, onChange) {
    const g = el('div', { class: 'segmented', role: 'radiogroup' });
    for (const [v, l] of options) {
      const b = el('button', { type: 'button', class: 'seg', role: 'radio', 'aria-checked': String(v === value) }, l);
      b.addEventListener('click', () => {
        g.querySelectorAll('.seg').forEach((x) => x.setAttribute('aria-checked', 'false'));
        b.setAttribute('aria-checked', 'true');
        onChange(v);
      });
      g.append(b);
    }
    g.setAttribute('aria-label', name);
    return g;
  }

  stateChip(state) {
    return el('span', { class: 'chip', 'data-state': state || 'starting' }, state || 'starting');
  }

  renderCollectors() {
    const s = this.api.settings();
    const statuses = new Map((this.statusData?.collectors || []).map((c) => [c.name, c]));
    const rows = this.api.meta().collectors.map((c) => {
      const cfg = s.collectors[c.name] || { enabled: true, interval_ms: c.default_interval_ms };
      const on = el('input', { type: 'checkbox', 'aria-label': `Collect ${c.label}` });
      on.checked = cfg.enabled;
      on.addEventListener('change', () => this.save(`collectors.${c.name}.enabled`, on.checked));
      const opts = INTERVALS.includes(cfg.interval_ms) ? INTERVALS : [...INTERVALS, cfg.interval_ms].sort((a, b) => a - b);
      const sel = this.select(opts.map((ms) => [ms, `every ${secLabel(ms)}`]), cfg.interval_ms,
        (v) => this.save(`collectors.${c.name}.interval_ms`, Number(v)));
      sel.setAttribute('aria-label', `${c.label} update interval`);
      const st = statuses.get(c.name);
      return el('tr', {},
        el('td', {}, on),
        el('th', { scope: 'row' }, c.label),
        el('td', {}, sel),
        el('td', {}, this.stateChip(st?.state)));
    });
    this.body.append(
      el('p', { class: 'help' }, 'How often each kind of statistic is read. Faster updates use a little more CPU; history is stored as averages either way.'),
      el('table', { class: 'settings-table' },
        el('thead', {}, el('tr', {}, el('th', {}, 'On'), el('th', {}, 'Statistic'), el('th', {}, 'Update'), el('th', {}, 'State'))),
        el('tbody', {}, rows)));
    const ex = el('input', { type: 'text', value: s.network.exclude.join(', '), spellcheck: 'false' });
    ex.addEventListener('change', () => this.save('network.exclude', ex.value.split(',').map((x) => x.trim()).filter(Boolean)));
    this.body.append(this.field('Ignored network interfaces', ex, 'Comma separated, * matches anything (for example veth*).'));
  }

  renderHistory() {
    const h = this.api.settings().history;
    const st = this.statusData?.storage;
    if (this.statusData?.memory_only) {
      this.body.append(el('p', { class: 'warn-box' }, `History is off: ${this.statusData.storage_error || 'database unavailable'}.`));
    } else if (st) {
      this.body.append(el('p', { class: 'help' }, `Database size: ${text('B', st.db_bytes)}. File: ${st.path}`));
    }
    this.body.append(
      this.field('Average samples over', this.select(RESOLUTIONS.map((r) => [r, `${r} s`]), h.resolution_s,
        (v) => this.save('history.resolution_s', Number(v))), 'Width of each stored point in the detailed history.'),
      this.field('Keep detailed history (hours)', this.number(h.raw_retention_hours, 1, 720, (v) => this.save('history.raw_retention_hours', v))),
      this.field('Keep 1-minute averages (days)', this.number(h.minute_retention_days, 1, 365, (v) => this.save('history.minute_retention_days', v))),
      this.field('Keep 1-hour averages (days)', this.number(h.hour_retention_days, 1, 3650, (v) => this.save('history.hour_retention_days', v))),
      this.field('Database size limit (MB)', this.number(h.max_db_mb, 64, 1048576, (v) => this.save('history.max_db_mb', v)),
        'When the limit is reached, the oldest history is removed first.'),
    );
  }

  async renderBackups() {
    const b = this.api.settings().backups;
    const on = el('input', { type: 'checkbox' });
    on.checked = b.enabled;
    on.addEventListener('change', () => this.save('backups.enabled', on.checked));
    const onRow = el('div', { class: 'field inline' }, on, el('label', {}, 'Back up the database every day'));
    on.id = 'bk-on';
    onRow.querySelector('label').setAttribute('for', 'bk-on');
    this.body.append(
      onRow,
      this.field('Daily backups to keep', this.number(b.keep_daily, 1, 365, (v) => this.save('backups.keep_daily', v))),
      this.field('Weekly backups to keep', this.number(b.keep_weekly, 0, 520, (v) => this.save('backups.keep_weekly', v))),
      this.field('Total size limit (MB)', this.number(b.max_total_mb, 16, 1048576, (v) => this.save('backups.max_total_mb', v)),
        'The oldest backups are deleted first; the newest is always kept.'),
    );
    const btn = el('button', { type: 'button', class: 'btn' }, 'Back up now');
    const msg = el('span', { class: 'help', 'aria-live': 'polite' });
    btn.addEventListener('click', async () => {
      btn.disabled = true;
      msg.textContent = 'Backing up…';
      try {
        const r = await this.api.backupNow();
        msg.textContent = `Backed up: ${r.file}`;
        await this.show('backups');
      } catch (e) {
        msg.textContent = `Backup failed: ${e.message}`;
      } finally {
        btn.disabled = false;
      }
    });
    this.body.append(el('div', { class: 'field inline' }, btn, msg));
    let data;
    try {
      data = await this.api.backups();
    } catch (e) {
      this.body.append(el('p', { class: 'warn-box' }, `Cannot list backups: ${e.message}`));
      return;
    }
    if (data.last_error) this.body.append(el('p', { class: 'warn-box' }, `Last backup failed: ${data.last_error}`));
    this.body.append(el('p', { class: 'help' }, `Folder: ${data.dir}. Total: ${text('B', data.total_bytes)}.`));
    if (!data.backups.length) {
      this.body.append(el('p', { class: 'muted' }, 'No backups yet. The first one is made a few minutes after the service starts.'));
      return;
    }
    this.body.append(el('table', { class: 'settings-table' },
      el('thead', {}, el('tr', {}, el('th', {}, 'Created'), el('th', {}, 'File'), el('th', { class: 'num' }, 'Size'))),
      el('tbody', {}, data.backups.map((x) => el('tr', {},
        el('td', {}, clock(x.created, true)),
        el('td', { class: 'mono-ish' }, x.file),
        el('td', { class: 'num' }, text('B', x.size)))))));
  }

  renderDisplay() {
    const d = this.api.settings().display;
    this.body.append(
      this.field('Live chart window', this.select([[60, '1 minute'], [300, '5 minutes'], [900, '15 minutes']], d.live_window_s,
        (v) => this.save('display.live_window_s', Number(v)))),
      this.field('Smoothing', this.radios('Smoothing', [['off', 'Off'], ['low', 'Low'], ['medium', 'Medium'], ['high', 'High']], d.smoothing,
        (v) => this.save('display.smoothing', v)), 'How gently numbers and charts move to new values. Off redraws only when data arrives, which uses the least power.'),
      this.field('Theme', this.radios('Theme', [['auto', 'System'], ['light', 'Light'], ['dark', 'Dark']], d.theme,
        (v) => this.save('display.theme', v))),
      this.field('Temperature', this.radios('Temperature unit', [['C', '°C'], ['F', '°F']], d.temp_unit,
        (v) => this.save('display.temp_unit', v))),
    );
  }

  async renderHardware(given) {
    let data = given;
    if (!data) {
      try {
        data = await this.api.hardware();
      } catch (e) {
        this.body.append(el('p', { class: 'warn-box' }, `Cannot load the hardware list: ${errText(e)}`));
        return;
      }
    }
    this.body.append(el('p', { class: 'help' },
      'Hardware found on this computer, stored in the database. New devices are added automatically. A rescan looks for all devices and their names again, for example after installing a driver, loading a kernel module or replacing a part.'));
    const btn = el('button', { type: 'button', class: 'btn' }, 'Rescan hardware');
    const msg = el('span', { class: 'help', 'aria-live': 'polite' });
    if (data.scanning) {
      btn.disabled = true;
      msg.textContent = 'Rescanning…';
    } else if (data.reason === 'rescanned') {
      msg.textContent = 'Rescan finished.';
    } else if (data.scanned_ms) {
      msg.textContent = `Last full scan: ${clock(data.scanned_ms / 1000, true)}`;
    }
    btn.addEventListener('click', async () => {
      btn.disabled = true;
      msg.textContent = 'Starting the rescan…';
      try {
        const r = await this.api.rescan();
        msg.textContent = `Rescanning, up to ${Math.ceil(r.expected_ms / 1000)} s…`;
      } catch (e) {
        msg.textContent = `Rescan not started: ${errText(e)}`;
        btn.disabled = false;
      }
    });
    this.body.append(el('div', { class: 'field inline' }, btn, msg));
    const items = data.items || [];
    if (!items.length) {
      this.body.append(el('p', { class: 'muted' }, 'Nothing stored yet. The list is filled about 15 seconds after the service starts.'));
      return;
    }
    for (const [kind, title] of HW_GROUPS) {
      const list = items.filter((i) => i.kind === kind);
      if (!list.length) continue;
      this.body.append(el('h3', { class: 'sub-head' }, title));
      this.body.append(el('ul', { class: 'hw-list' }, list.map((i) => {
        const facts = Object.entries(i.details || {}).map(([k, v]) => `${FACT_LABELS[k] || k.replace(/_/g, ' ')} ${v}`);
        return el('li', { class: i.present ? 'hw-item' : 'hw-item absent' },
          el('div', { class: 'hw-name' }, i.name,
            i.present ? null : el('span', { class: 'chip', 'data-state': 'unavailable' }, `not detected since ${clock(i.last_seen / 1000, true)}`)),
          facts.length ? el('div', { class: 'help' }, facts.join(', ')) : null,
          el('div', { class: 'help hw-seen' }, `First seen ${clock(i.first_seen / 1000, true)}`));
      })));
    }
  }

  renderStatus() {
    const s = this.statusData;
    if (!s) {
      this.body.append(el('p', { class: 'warn-box' }, 'The service is not reachable.'));
      return;
    }
    for (const n of s.notes || []) this.body.append(el('p', { class: 'warn-box' }, n));
    if (s.memory_only) this.body.append(el('p', { class: 'warn-box' }, `History is off: ${s.storage_error}`));
    const rows = s.collectors.map((c) => el('tr', {},
      el('th', { scope: 'row' }, c.name),
      el('td', {}, this.stateChip(c.state)),
      el('td', {},
        el('div', {}, c.source || ''),
        c.reason ? el('div', { class: 'help' }, c.reason) : null,
        c.hint ? el('div', { class: 'help hint' }, `Tip: ${c.hint}`) : null)));
    this.body.append(el('table', { class: 'settings-table status' },
      el('thead', {}, el('tr', {}, el('th', {}, 'Collector'), el('th', {}, 'State'), el('th', {}, 'Source and details'))),
      el('tbody', {}, rows)));
    const st = s.storage;
    const facts = [
      ['Running for', text('s', s.uptime_s)],
      ['Live viewers', String(s.sse_clients)],
    ];
    if (st) {
      facts.push(['Database', `${text('B', st.db_bytes)}, ${st.rows_written.toLocaleString()} values written since start`]);
      facts.push(['Last write', st.last_write_ms ? clock(st.last_write_ms / 1000) : 'not yet']);
      if (st.error) facts.push(['Database error', st.error]);
      if (st.dropped_batches) facts.push(['Dropped batches', String(st.dropped_batches)]);
    }
    facts.push(['Backups', `${s.backups.count} (${text('B', s.backups.total_bytes)})`]);
    if (s.dropped_series) facts.push(['Series over the limit', String(s.dropped_series)]);
    const dl = el('dl', { class: 'facts' });
    for (const [k, v] of facts) dl.append(el('div', {}, el('dt', {}, k), el('dd', {}, v)));
    this.body.append(dl);
  }
}

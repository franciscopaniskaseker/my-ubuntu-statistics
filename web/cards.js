// Builds the dashboard panels from the series catalog and routes live
// samples to readouts, meters and charts. Panels only appear when their
// data exists, so the page adapts to any machine.

import { Chart, store } from './charts.js';
import { byUnit, text, bytes, duration, prefs } from './format.js';
import { Tween, numberTween } from './tween.js';
import { makeBarGroup } from './vbar.js';
import { AppDetail } from './procdetail.js';

// ---------- DOM helpers (all text goes through textContent) ----------

export function el(tag, attrs = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === 'class') e.className = v;
    else if (k === 'style') e.style.cssText = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? '' : v);
  }
  for (const c of kids.flat()) {
    if (c == null || c === false) continue;
    e.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return e;
}

export const CORE_WARN = 70;
export const CORE_CRIT = 90;

// Per-thread bar color: blue ramp for normal load, yellow and red for high load.
function coreColor(v) {
  if (v >= CORE_CRIT) return '--crit';
  if (v >= CORE_WARN) return '--warn';
  return '--s1';
}

const TEMP_COLOR = { ok: '--s1', warn: '--warn', serious: '--serious', crit: '--crit' };

function tempStatus(v, crit) {
  const c = crit && crit > 40 ? crit : 100;
  if (v >= c - 3) return 'crit';
  if (v >= c - 8) return 'serious';
  if (v >= c - 20) return 'warn';
  return 'ok';
}

const STATUS_WORD = { warn: 'warm', serious: 'hot', crit: 'critical' };

function usageStatus(p) {
  if (p >= 90) return 'crit';
  if (p >= 80) return 'warn';
  return 'ok';
}

// ---------- Dashboard ----------

export class Dashboard {
  constructor(grid, opts) {
    this.grid = grid;
    this.opts = opts; // { settings: () => settings, fetchHistory }
    this.bindings = new Map();
    this.chartsByKey = new Map();
    this.charts = [];
    this.infoHandlers = [];
    this.procHandlers = [];
    this.extraHandlers = [];
    this.byKey = new Map();
    this.range = 'live';
    this.coreTip = el('div', { class: 'core-tip', hidden: true, role: 'tooltip' });
    document.body.append(this.coreTip);
    this.flow = [];
    this.gpuProcs = new Map();
    const getJson = async (url) => {
      const r = await fetch(url);
      if (!r.ok) {
        let msg = `${r.status} ${r.statusText}`;
        try {
          msg = (await r.json()).error || msg;
        } catch {
          // keep the status text
        }
        const e = new Error(msg);
        e.status = r.status;
        throw e;
      }
      return r.json();
    };
    this.appDetail = new AppDetail({
      fetchApp: (app) => getJson(`/api/processes?app=${encodeURIComponent(app)}`),
      fetchProc: (pid) => getJson(`/api/process?pid=${encodeURIComponent(pid)}`),
      gpu: () => this.gpuProcs,
    });
    let t = 0;
    window.addEventListener('resize', () => {
      clearTimeout(t);
      t = setTimeout(() => this.layout(), 200);
    });
  }

  // Cards other than the full-width ones are packed into balanced columns:
  // each card goes to the currently shortest column, so no card is
  // stretched to the height of a taller neighbour.
  layout() {
    if (!this.masonry) return;
    const w = this.grid.clientWidth;
    const n = w >= 1240 ? 3 : w >= 760 ? 2 : 1;
    const cols = Array.from({ length: n }, () => el('div', { class: 'mcol' }));
    this.masonry.replaceChildren(...cols);
    for (const p of this.flow) {
      let best = cols[0];
      for (const c of cols) if (c.offsetHeight < best.offsetHeight) best = c;
      best.append(p);
    }
  }

  // Panels are rebuilt whenever the set of series keys changes, so series
  // that appear later (a GPU waking up, battery time while discharging)
  // get their readouts and charts.
  static signature(series) {
    return series.map((d) => d.key).sort().join(',');
  }

  destroy() {
    for (const c of this.charts) c.destroy();
    this.charts = [];
    this.bindings.clear();
    this.chartsByKey.clear();
    this.infoHandlers = [];
    this.procHandlers = [];
    this.extraHandlers = [];
    this.flow = [];
    this.grid.replaceChildren();
  }

  intervalSec(collector) {
    const s = this.opts.settings();
    const c = s && s.collectors ? s.collectors[collector] : null;
    return c ? c.interval_ms / 1000 : 2;
  }

  // Expected seconds between samples of a series.
  periodSec(d) {
    return Math.max(this.intervalSec(d.collector), (d.period_ms || 0) / 1000);
  }

  bind(key, fn) {
    if (!this.bindings.has(key)) this.bindings.set(key, []);
    this.bindings.get(key).push(fn);
  }

  has(key) {
    return this.byKey.has(key);
  }

  // A [number, unit] readout with a label underneath.
  readout(parent, key, label, opts = {}) {
    if (!key || !this.has(key)) return null;
    const d = this.byKey.get(key);
    const num = el('span', { class: 'ro-num' }, '–');
    const unit = el('span', { class: 'ro-unit' });
    const box = el('div', { class: `ro${opts.hero ? ' ro-hero' : ''}` },
      el('div', { class: 'ro-val' }, num, unit),
      el('div', { class: 'ro-label' }, label));
    parent.append(box);
    const tw = numberTween(num, unit, opts.fmt || byUnit(d.unit));
    this.bind(key, (v) => tw.set(v));
    return box;
  }

  // A labelled horizontal meter (value against a maximum).
  meter(parent, { name, key, max, maxKey, unit, status, sub, statusText }) {
    if (!this.has(key)) return null;
    const d = this.byKey.get(key);
    const num = el('span', { class: 'ro-num' }, '–');
    const un = el('span', { class: 'ro-unit' });
    const tag = el('span', { class: 'meter-tag' });
    const fill = el('span', { class: 'meter-fill' });
    const bar = el('div', { class: 'meter', 'data-status': 'ok' }, fill);
    const subEl = sub ? el('div', { class: 'meter-sub' }) : null;
    const row = el('div', { class: 'meter-row' },
      el('div', { class: 'meter-top' },
        el('span', { class: 'meter-name' }, name),
        el('span', { class: 'meter-val' }, tag, num, un)),
      bar, subEl);
    parent.append(row);
    const fmt = byUnit(unit || d.unit);
    let mx = max ?? 100;
    if (maxKey) this.bind(maxKey, (v) => { mx = v; });
    const numTw = numberTween(num, un, fmt);
    const barTw = new Tween((v) => {
      const p = mx > 0 ? Math.max(0, Math.min(100, (v / mx) * 100)) : 0;
      fill.style.width = `${p}%`;
    });
    this.bind(key, (v) => {
      numTw.set(v);
      barTw.set(v);
      const st = status ? status(v, mx) : 'ok';
      if (bar.dataset.status !== st) bar.dataset.status = st;
      const word = statusText ? statusText(st) : '';
      if (tag.textContent !== word) tag.textContent = word;
      if (subEl) subEl.textContent = sub(v, mx);
    });
    return row;
  }

  chart(parent, cfg) {
    const series = cfg.series.filter((s) => s.keys.length && s.keys.every((k) => this.has(k)));
    if (!series.length) return null;
    const host = el('div');
    parent.append(host);
    const descs = series.flatMap((s) => s.keys.map((k) => this.byKey.get(k)));
    const lagSec = () => Math.max(...descs.map((d) => this.periodSec(d)));
    const c = new Chart(host, { ...cfg, series, lagSec });
    this.charts.push(c);
    for (const k of c.keys()) {
      if (!this.chartsByKey.has(k)) this.chartsByKey.set(k, new Set());
      this.chartsByKey.get(k).add(c);
    }
    return c;
  }

  panel(id, title, sub, span = 4) {
    const head = el('header', { class: 'panel-head' }, el('h2', {}, title));
    if (sub) head.append(el('p', { class: 'panel-sub' }, sub));
    const p = el('section', { class: `panel${span === 12 ? ' panel-wide' : ''}`, id: `p-${id}`, 'aria-label': title }, head);
    if (span === 12) {
      (id === 'processes' ? this.bottom : this.top).append(p);
    } else {
      this.flow.push(p);
      this.masonry.firstChild.append(p);
    }
    return p;
  }

  facts(parent, rows) {
    const dl = el('dl', { class: 'facts' });
    for (const [label, node] of rows) dl.append(el('div', {}, el('dt', {}, label), el('dd', {}, node)));
    parent.append(dl);
    return dl;
  }

  build(meta, snapshot) {
    this.destroy();
    this.top = el('div', { class: 'full-row' });
    this.masonry = el('div', { class: 'masonry' }, el('div', { class: 'mcol' }));
    this.bottom = el('div', { class: 'full-row' });
    this.grid.append(this.top, this.masonry, this.bottom);
    this.meta = meta;
    this.byKey = new Map(meta.series.map((d) => [d.key, d]));
    const all = meta.series;
    const groupsOf = (collector) => {
      const m = new Map();
      for (const d of all) {
        if (d.collector !== collector || !d.group) continue;
        if (!m.has(d.group)) m.set(d.group, { id: d.group, label: d.group_label, descs: [] });
        m.get(d.group).descs.push(d);
      }
      return Array.from(m.values());
    };
    const snap = snapshot?.series || {};
    const latest = (k) => (snap[k] ? snap[k][1] : null);

    this.buildCpu(meta, all);
    this.buildLoad();
    this.buildMemory();
    for (const g of [...groupsOf('gpu_nvidia'), ...groupsOf('gpu_drm')]) this.buildGpu(g);
    this.buildTemps(all, groupsOf);
    this.buildStorage(groupsOf('disk'), groupsOf('filesystem'));
    this.buildNetwork(groupsOf('network'), latest);
    this.buildPower(all, groupsOf('battery'));
    this.buildFans(all);
    this.buildSystem();
    this.buildProcesses();

    // Seed readouts and the store with the snapshot.
    for (const [k, [ms, v]] of Object.entries(snap)) {
      this.apply(k, ms / 1000, v);
    }
    if (snapshot?.info) for (const [c, info] of Object.entries(snapshot.info)) this.onInfo(c, info);
    if (snapshot?.processes) this.onProcesses(snapshot.processes);
    if (snapshot?.extra) for (const [c, data] of Object.entries(snapshot.extra)) this.onExtra(c, data);
    this.layout();
  }

  apply(key, tSec, v) {
    const d = this.byKey.get(key);
    const gap = d ? this.periodSec(d) * 3 + 1.5 : 6;
    store.push(key, tSec, v, gap);
    const b = this.bindings.get(key);
    if (b) for (const fn of b) fn(v);
    const cs = this.chartsByKey.get(key);
    if (cs) for (const c of cs) c.markDirty();
  }

  onSample(evt) {
    const t = evt.t / 1000;
    for (const [k, v] of Object.entries(evt.v)) this.apply(k, t, v);
  }

  onInfo(collector, info) {
    for (const h of this.infoHandlers) h(collector, info || {});
  }

  onProcesses(p) {
    for (const h of this.procHandlers) h(p);
  }

  // Re-apply the latest values (after a unit change).
  refreshValues() {
    for (const [key, fns] of this.bindings) {
      const v = store.latest(key);
      if (v != null) for (const fn of fns) fn(v);
    }
    for (const b of this.grid.querySelectorAll('.unit-toggle .seg')) {
      b.setAttribute('aria-checked', String(b.dataset.unit === prefs.tempUnit));
    }
  }

  onExtra(collector, data) {
    if (collector === 'gpu_nvidia') {
      const m = new Map();
      for (const [gpuId, list] of Object.entries(data || {})) {
        for (const p of list) m.set(p.pid, { mem: p.mem, sm: p.sm, gpu: gpuId });
      }
      this.gpuProcs = m;
    }
    for (const h of this.extraHandlers) h(collector, data || {});
  }

  restyle() {
    for (const c of this.charts) c.restyle();
  }

  setRange(range) {
    this.range = range;
    const now = store.now();
    const spans = { '1h': 3600, '6h': 6 * 3600, '24h': 86400, '7d': 7 * 86400, '30d': 30 * 86400 };
    for (const c of this.charts) {
      if (range === 'live') c.setLive();
      else c.loadHistory(Math.floor(now - spans[range]), Math.ceil(now), this.opts.fetchHistory);
    }
  }

  // ---------- panels ----------

  buildCpu(meta, all) {
    if (!this.has('cpu.usage')) return;
    const host = meta.host || {};
    const sub = [host.cpu, host.threads ? `${host.threads} threads` : null].filter(Boolean).join(', ');
    const p = this.panel('cpu', 'Processor', sub, 12);
    const main = el('div', { class: 'cpu-main' });
    const side = el('div', { class: 'cpu-cores' });
    p.append(el('div', { class: 'cpu-body' }, main, side));
    const ros = el('div', { class: 'readouts' });
    main.append(ros);
    this.readout(ros, 'cpu.usage', 'busy', { hero: true });
    this.readout(ros, 'cpu.mhz_avg', 'average clock');
    this.readout(ros, 'cpu.mhz_max', 'fastest core');
    const tkey = cpuTempKey(all);
    if (tkey) this.readout(ros, tkey, 'package temperature');
    const pkey = ['power.rapl.package-0', ...all.filter((d) => d.collector === 'power').map((d) => d.key)].find((k) => this.has(k));
    if (pkey) this.readout(ros, pkey, 'package power');
    this.readout(ros, 'cpu.iowait', 'waiting on I/O');

    this.chart(main, {
      unit: '%',
      max: 100,
      stacked: true,
      height: 170,
      series: [
        { keys: ['cpu.user'], label: 'User', color: '--s1' },
        { keys: ['cpu.system'], label: 'System', color: '--s2' },
        { keys: ['cpu.iowait'], label: 'I/O wait', color: '--s3' },
        { keys: ['cpu.nice', 'cpu.irq', 'cpu.softirq', 'cpu.steal'], label: 'Other', color: '--s4' },
      ],
    });

    const threads = all
      .filter((d) => d.collector === 'cpu' && d.metric === 'core_usage')
      .map((d) => ({ idx: +d.key.split('.')[2], key: d.key, group: d.group, glabel: d.group_label }));
    if (threads.length) {
      const byCore = new Map();
      for (const t of threads) {
        if (!byCore.has(t.group)) byCore.set(t.group, { label: t.glabel, threads: [] });
        byCore.get(t.group).threads.push(t);
      }
      const num = (x) => {
        const m = /(\d+)(?!.*\d)/.exec(x);
        return m ? +m[1] : 0;
      };
      const groups = Array.from(byCore.values());
      for (const g of groups) g.threads.sort((x, y) => x.idx - y.idx);
      groups.sort((x, y) => x.threads[0].idx - y.threads[0].idx || num(x.label) - num(y.label));
      const legend = el('ul', { class: 'legend core-legend', 'aria-label': 'Bar colors' },
        el('li', {}, el('span', { class: 'swatch', style: '--c: var(--s1)' }), `under ${CORE_WARN}%`),
        el('li', {}, el('span', { class: 'swatch', style: '--c: var(--warn)' }), `${CORE_WARN}–${CORE_CRIT}%`),
        el('li', {}, el('span', { class: 'swatch', style: '--c: var(--crit)' }), `${CORE_CRIT}% and up`));
      const physical = groups.length !== threads.length;
      const title = physical ? `Load per core (${groups.length} cores, ${threads.length} threads)` : `Load per thread (${threads.length})`;
      const grid = el('div', { class: 'bgrid cores', role: 'group', 'aria-label': title });
      // Eight core groups per row, at most three rows.
      grid.style.setProperty('--gcols', groups.length <= 24 ? Math.min(8, groups.length) : Math.ceil(groups.length / 3));
      side.append(el('div', { class: 'sub-row cores-head' }, el('h3', { class: 'sub-head plain' }, title), legend), grid);
      for (const g of groups) {
        const bg = makeBarGroup(g.label, g.threads.map((t) => ({
          label: `cpu${t.idx}`,
          aria: `CPU ${t.idx} (${g.label})`,
          fmt: (v) => [String(Math.round(v)), '%'],
          color: coreColor,
        })), { min: 0, max: 100 });
        grid.append(bg.el);
        g.threads.forEach((t, i) => {
          let mhz = null;
          this.bind(t.key, (v) => bg.bars[i].set(v));
          this.bind(`cpu.core.${t.idx}.mhz`, (v) => {
            mhz = v;
            bg.bars[i].col.title = `CPU ${t.idx} (${g.label}): ${text('MHz', mhz)}`;
          });
        });
      }
    }
  }

  buildLoad() {
    if (!this.has('load.1')) return;
    const p = this.panel('load', 'Load and pressure', 'Pressure is the share of time tasks were stalled waiting for a resource.', 4);
    const ros = el('div', { class: 'readouts compact' });
    p.append(ros);
    this.readout(ros, 'load.1', '1 min load');
    this.readout(ros, 'load.5', '5 min');
    this.readout(ros, 'load.15', '15 min');
    this.readout(ros, 'cpu.procs_running', 'runnable');
    this.readout(ros, 'cpu.procs_blocked', 'blocked on I/O');
    const psi = this.chart(p, {
      unit: '%',
      minTop: 5,
      height: 120,
      series: [
        { keys: ['psi.cpu.some'], label: 'CPU', color: '--s1' },
        { keys: ['psi.io.some'], label: 'I/O', color: '--s2' },
        { keys: ['psi.memory.some'], label: 'Memory', color: '--s3' },
      ],
    });
    if (!psi) {
      this.chart(p, { unit: 'count', minTop: 1, height: 120, series: [{ keys: ['load.1'], label: 'Load 1 min', color: '--s1' }] });
    }
  }

  buildMemory() {
    if (!this.has('mem.used')) return;
    const p = this.panel('memory', 'Memory', null, 4);
    const ros = el('div', { class: 'readouts compact' });
    p.append(ros);
    this.readout(ros, 'mem.used', 'in use');
    this.readout(ros, 'mem.used_pct', 'of total');
    this.readout(ros, 'mem.available', 'available');
    this.readout(ros, 'mem.cached', 'cache');
    this.chart(p, {
      unit: 'B',
      stacked: true,
      max: () => store.latest('mem.total'),
      height: 120,
      series: [
        { keys: ['mem.used'], label: 'In use', color: '--s1' },
        { keys: ['mem.cached'], label: 'Cache', color: '--s3' },
      ],
    });
    const m = el('div', { class: 'meters' });
    p.append(m);
    if (this.has('swap.total') && (store.latest('swap.total') ?? 1) > 0) {
      this.meter(m, {
        name: 'Swap',
        key: 'swap.used',
        maxKey: 'swap.total',
        max: 0,
        status: (v, mx) => usageStatus(mx ? (v / mx) * 100 : 0),
        sub: (v, mx) => `${text('B', v)} of ${text('B', mx)}`,
      });
    }
    const ros2 = el('div', { class: 'readouts mini' });
    p.append(ros2);
    this.readout(ros2, 'swap.in', 'swap in');
    this.readout(ros2, 'swap.out', 'swap out');
    this.readout(ros2, 'mem.majfault', 'major faults');
    this.readout(ros2, 'mem.dirty', 'dirty');
  }

  buildGpu(g) {
    const k = (m) => `gpu.${g.id}.${m}`;
    const nvidia = g.descs.some((d) => d.collector === 'gpu_nvidia');
    const p = this.panel(`gpu-${g.id}`, g.label, null, 4);
    const badge = el('span', { class: 'badge', hidden: true }, 'Sleeping, not polled');
    p.querySelector('.panel-head').append(badge);
    const ros = el('div', { class: 'readouts compact' });
    p.append(ros);
    this.readout(ros, k('util'), 'busy');
    this.readout(ros, k('temp'), 'temperature');
    this.readout(ros, k('power'), 'power');
    this.readout(ros, k('ppt'), 'SoC package (CPU + GPU)');
    this.readout(ros, k('clock_gr'), 'core clock');
    this.readout(ros, k('clock_mem'), 'memory clock');
    this.readout(ros, k('mem_util'), 'memory controller');
    this.readout(ros, k('video'), 'video engine');
    this.readout(ros, k('voltage'), 'core voltage');
    if (nvidia) {
      const media = el('div', { class: 'readouts mini' });
      p.append(media);
      this.readout(media, k('enc'), 'encoder');
      this.readout(media, k('enc_sessions'), 'encoder sessions');
      this.readout(media, k('enc_fps'), 'encoder FPS');
      this.readout(media, k('dec'), 'decoder');
      this.readout(media, k('clock_video'), 'video clock');
      this.readout(media, k('pcie_rx'), 'PCIe to GPU');
      this.readout(media, k('pcie_tx'), 'PCIe from GPU');
    }
    const m = el('div', { class: 'meters' });
    p.append(m);
    const memMeter = (name, used, total) => {
      if (!this.has(total)) return;
      this.meter(m, {
        name,
        key: used,
        maxKey: total,
        max: 0,
        unit: 'B',
        status: (v, mx) => usageStatus(mx ? (v / mx) * 100 : 0),
        sub: (v, mx) => `${text('B', v)} of ${text('B', mx)}`,
      });
    };
    memMeter('Video memory', k('vram_used'), k('vram_total'));
    memMeter('Shared system memory', k('gtt_used'), k('gtt_total'));
    if (this.has(k('power_limit'))) {
      this.meter(m, {
        name: 'Power against limit',
        key: k('power'),
        maxKey: k('power_limit'),
        max: 0,
        unit: 'W',
        status: (v, mx) => usageStatus(mx ? (v / mx) * 100 : 0),
        sub: (v, mx) => `${text('W', v)} of ${text('W', mx)}`,
      });
    }
    const extra = [];
    if (this.has(k('enc'))) extra.push({ keys: [k('enc')], label: 'Encoder', color: '--s2' });
    if (this.has(k('dec'))) extra.push({ keys: [k('dec')], label: 'Decoder', color: '--s3' });
    if (this.has(k('video'))) extra.push({ keys: [k('video')], label: 'Video engine', color: '--s2' });
    this.chart(p, {
      unit: '%',
      max: 100,
      height: 110,
      series: [{ keys: [k('util')], label: 'Busy', color: '--s1' }, ...extra],
    });

    if (nvidia) {
      const facts = [
        ['cuda_cores', 'CUDA cores'],
        ['architecture', 'Architecture'],
        ['compute_capability', 'Compute capability'],
        ['max_clock', 'Maximum core clock'],
        ['memory_bus', 'Memory bus'],
        ['pcie_link', 'PCIe link now'],
        ['pcie_max', 'PCIe link maximum'],
        ['pstate', 'Performance state'],
        ['throttle', 'Clock limited by'],
        ['driver', 'Driver'],
        ['cuda', 'CUDA version'],
        ['vbios', 'VBIOS'],
      ];
      const vals = {};
      const dl = this.facts(p, facts.map(([id, label]) => {
        vals[id] = el('span', {}, '–');
        return [label, vals[id]];
      }));
      dl.classList.add('facts-2');
      this.infoHandlers.push((c, info) => {
        if (c !== 'gpu_nvidia') return;
        for (const [id] of facts) {
          const v = info[`${g.id}.${id}`];
          if (v !== undefined) vals[id].textContent = id === 'cuda_cores' ? Number(v).toLocaleString('en-US') : v;
        }
      });

      p.append(el('h3', { class: 'sub-head' }, 'Processes using the GPU'));
      const tbody = el('tbody');
      const empty = el('p', { class: 'muted small' }, 'No processes are using the GPU.');
      const table = el('table', { class: 'procs gpu-procs' },
        el('thead', {}, el('tr', {},
          el('th', { scope: 'col' }, 'Name'),
          el('th', { scope: 'col' }, 'Type'),
          el('th', { scope: 'col', class: 'num' }, 'GPU memory'),
          el('th', { scope: 'col', class: 'num', title: 'Streaming multiprocessors (CUDA/3D)' }, 'SM'),
          el('th', { scope: 'col', class: 'num' }, 'Enc'),
          el('th', { scope: 'col', class: 'num' }, 'Dec'),
          el('th', { scope: 'col', class: 'num' }, 'PID'))),
        tbody);
      p.append(el('div', { class: 'table-wrap' }, table), empty);
      const openRow = (e) => {
        const tr = e.target.closest('tr[data-pid]');
        if (tr) this.appDetail.openProcess(Number(tr.dataset.pid), tr.dataset.name);
      };
      tbody.addEventListener('click', openRow);
      tbody.addEventListener('keydown', (e) => {
        if (e.key !== 'Enter' && e.key !== ' ') return;
        e.preventDefault();
        openRow(e);
      });
      this.extraHandlers.push((c, data) => {
        if (c !== 'gpu_nvidia') return;
        const rows = data[g.id] || [];
        empty.hidden = rows.length > 0;
        table.hidden = rows.length === 0;
        tbody.replaceChildren(...rows.slice(0, 10).map((r) => el('tr', {
          class: 'clickable', tabindex: '0', 'data-pid': r.pid, 'data-name': r.name, 'aria-label': `Show details of ${r.name} (PID ${r.pid})`,
        },
          el('td', { class: 'pname' }, r.name),
          el('td', { class: 'muted' }, r.kind),
          el('td', { class: 'num' }, r.mem == null ? '–' : bytes(r.mem).join(' ')),
          el('td', { class: 'num' }, `${r.sm}%`),
          el('td', { class: 'num' }, `${r.enc}%`),
          el('td', { class: 'num' }, `${r.dec}%`),
          el('td', { class: 'num muted' }, r.pid))));
      });
    }

    this.infoHandlers.push((c, info) => {
      if (c !== 'gpu_nvidia' && c !== 'gpu_drm') return;
      const st = info[`${g.id}.state`];
      if (st === undefined) return;
      const asleep = st === 'sleeping';
      badge.hidden = !asleep;
      p.classList.toggle('is-asleep', asleep);
    });
  }

  buildTemps(all, groupsOf) {
    const temps = all.filter((d) => d.unit === 'C' && (d.collector === 'sensors' || d.collector === 'thermal' || d.metric === 'temp'));
    if (!temps.length) return;
    const p = this.panel('temps', 'Temperatures', null, 4);
    const lines = [];
    const cpu = cpuTempKey(all);
    if (cpu) lines.push({ keys: [cpu], label: 'CPU' });
    const gpus = all.filter((d) => (d.collector === 'gpu_nvidia' || d.collector === 'gpu_drm') && d.metric === 'temp');
    for (const g of gpus) lines.push({ keys: [g.key], label: g.collector === 'gpu_nvidia' ? 'GPU' : 'iGPU' });
    const nvme = all.find((d) => d.collector === 'sensors' && d.group.startsWith('nvme.') && d.metric === 'temp');
    if (nvme) lines.push({ keys: [nvme.key], label: 'SSD' });
    const colors = ['--s1', '--s2', '--s3', '--s4'];
    this.chart(p, {
      unit: 'C',
      minTop: 60,
      height: 120,
      series: lines.slice(0, 4).map((l, i) => ({ ...l, color: colors[i] })),
    });

    const sensorName = (d) => {
      const m = /^temp(\d+)$/.exec(d.label);
      return m ? `Sensor ${m[1]}` : d.label;
    };
    const groups = [...groupsOf('sensors'), ...groupsOf('thermal')]
      .map((g) => ({ ...g, temps: g.descs.filter((d) => d.metric === 'temp') }))
      .filter((g) => g.temps.length);
    // Chips with a single generic sensor (DIMMs, zones) share one group.
    const singles = groups.filter((g) => g.temps.length === 1 && /^temp\d+$/.test(g.temps[0].label));
    const multi = groups.filter((g) => !singles.includes(g));
    const sections = multi.map((g) => ({ title: g.label, items: g.temps.map((d) => ({ d, name: sensorName(d) })) }));
    if (singles.length) {
      sections.push({
        title: 'Other sensors',
        items: singles.map((g) => ({ d: g.temps[0], name: g.label.replace(/^Memory /, '') })),
      });
    }
    const toggle = el('div', { class: 'segmented unit-toggle', role: 'radiogroup', 'aria-label': 'Temperature unit' });
    for (const [u, label] of [['C', '°C'], ['F', '°F']]) {
      const btn = el('button', { type: 'button', class: 'seg', role: 'radio', 'data-unit': u, 'aria-checked': String(prefs.tempUnit === u) }, label);
      btn.addEventListener('click', () => this.opts.setTempUnit && this.opts.setTempUnit(u));
      toggle.append(btn);
    }
    p.querySelector('.panel-head').append(toggle);

    const wrap = el('div', { class: 'tgroups', role: 'group', 'aria-label': 'Temperature sensors' });
    p.append(wrap);
    for (const sec of sections) {
      const vals = el('div', { class: 'tvals' });
      const title = sec.title.replace(/^NVMe /, '');
      wrap.append(el('section', { class: 'tgroup' }, el('h3', { class: 'tgroup-name', title: sec.title }, title), vals));
      for (const { d, name } of sec.items) {
        const num = el('span', { class: 'ro-num' }, '–');
        const unit = el('span', { class: 'ro-unit' });
        const word = el('span', { class: 'tval-word' });
        const item = el('div', {
          class: 'tval',
          'data-status': 'ok',
          title: `${sec.title}: ${d.label}${d.crit ? ` (critical at ${text('C', d.crit)})` : ''}`,
        }, el('div', { class: 'ro-val' }, num, unit), el('div', { class: 'tval-label' }, el('span', {}, name), word));
        vals.append(item);
        const tw = numberTween(num, unit, byUnit('C'));
        this.bind(d.key, (v) => {
          tw.set(v);
          const st = tempStatus(v, d.crit);
          if (item.dataset.status !== st) item.dataset.status = st;
          const w = STATUS_WORD[st] || '';
          if (word.textContent !== w) word.textContent = w;
        });
      }
    }
  }

  buildStorage(disks, filesystems) {
    if (!disks.length && !filesystems.length) return;
    const p = this.panel('storage', 'Storage', null, 4);
    for (const g of disks) {
      const k = (m) => `disk.${g.id}.${m}`;
      p.append(el('h3', { class: 'sub-head' }, g.label));
      const ros = el('div', { class: 'readouts mini' });
      p.append(ros);
      this.readout(ros, k('read_bps'), 'read');
      this.readout(ros, k('write_bps'), 'write');
      this.readout(ros, k('util'), 'busy');
      this.readout(ros, k('await'), 'latency');
      this.chart(p, {
        unit: 'B/s',
        minTop: 1024 * 1024,
        height: 90,
        series: [
          { keys: [k('read_bps')], label: 'Read', color: '--s1' },
          { keys: [k('write_bps')], label: 'Write', color: '--s2' },
        ],
      });
    }
    if (filesystems.length) {
      p.append(el('h3', { class: 'sub-head' }, 'Filesystems'));
      const m = el('div', { class: 'meters' });
      p.append(m);
      for (const g of filesystems) {
        const id = g.id;
        const name = g.label.split(' (')[0];
        let used = 0;
        let total = 0;
        this.bind(`fs.${id}.used`, (v) => { used = v; });
        this.bind(`fs.${id}.total`, (v) => { total = v; });
        this.meter(m, {
          name,
          key: `fs.${id}.used_pct`,
          max: 100,
          status: (v) => usageStatus(v),
          statusText: (st) => (st === 'crit' ? 'almost full' : st === 'warn' ? 'filling up' : ''),
          sub: () => (total ? `${text('B', used)} of ${text('B', total)}` : ''),
        });
      }
    }
  }

  buildNetwork(ifaces, latest) {
    if (!ifaces.length) return;
    const p = this.panel('network', 'Network', null, 4);
    const inactive = [];
    const colorsIn = '--s1';
    const colorsOut = '--s2';
    for (const g of ifaces) {
      const k = (m) => `net.${g.id}.${m}`;
      const up = latest(k('up'));
      const traffic = (latest(k('rx_bps')) || 0) + (latest(k('tx_bps')) || 0);
      if (up === 0 && traffic === 0) {
        inactive.push(g.id);
        continue;
      }
      p.append(el('h3', { class: 'sub-head' }, g.label));
      const ros = el('div', { class: 'readouts mini' });
      p.append(ros);
      this.readout(ros, k('rx_bps'), 'download');
      this.readout(ros, k('tx_bps'), 'upload');
      this.readout(ros, k('wifi_dbm'), 'signal');
      this.readout(ros, k('wifi_quality'), 'link quality');
      this.chart(p, {
        unit: 'B/s',
        minTop: 16 * 1024,
        height: 80,
        series: [
          { keys: [k('rx_bps')], label: 'Download', color: colorsIn },
          { keys: [k('tx_bps')], label: 'Upload', color: colorsOut },
        ],
      });
    }
    const ros = el('div', { class: 'readouts mini' });
    p.append(ros);
    this.readout(ros, 'net.tcp.inuse', 'TCP sockets');
    this.readout(ros, 'net.udp.inuse', 'UDP sockets');
    this.readout(ros, 'net.tcp.tw', 'TIME_WAIT');
    if (inactive.length) p.append(el('p', { class: 'muted small' }, `No link: ${inactive.join(', ')}`));
  }

  buildPower(all, batteries) {
    const bats = batteries.filter((g) => g.id.startsWith('bat.'));
    const psus = batteries.filter((g) => g.id.startsWith('psu.'));
    const hasTotal = this.has('power.total');
    if (!bats.length && !psus.length && !hasTotal && !all.some((d) => d.collector === 'power')) return;
    const p = this.panel('power', 'Power and battery', null, 4);
    const ros = el('div', { class: 'readouts compact' });
    p.append(ros);
    this.readout(ros, 'power.total', 'measured total');
    this.readout(ros, 'power.cpu_total', 'CPU package');
    this.readout(ros, 'power.rapl.package-0.core', 'CPU cores');
    this.readout(ros, 'power.gpu_total', 'GPUs');
    this.readout(ros, 'power.dram_total', 'memory');
    const parts = el('p', { class: 'muted small power-parts' });
    p.append(parts);
    this.infoHandlers.push((c, info) => {
      if (c === 'power_summary' && info.parts) {
        parts.textContent = `Total of ${info.parts}. Screen, SSDs and other parts are not measured separately.`;
      }
    });
    this.chart(p, {
      unit: 'W',
      stacked: true,
      minTop: 10,
      height: 120,
      series: [
        { keys: ['power.cpu_total'], label: 'CPU package', color: '--s1' },
        { keys: ['power.gpu_total'], label: 'GPUs', color: '--s2' },
        { keys: ['power.dram_total'], label: 'Memory', color: '--s3' },
      ],
    });
    for (const g of bats) {
      const k = (m) => `${g.id}.${m}`;
      const statusEl = el('span', { class: 'meter-tag' });
      const m = el('div', { class: 'meters' });
      p.append(m);
      let state = '';
      const row = this.meter(m, {
        name: g.label,
        key: k('capacity'),
        max: 100,
        status: (v) => (state === 'Discharging' ? (v < 15 ? 'crit' : v < 30 ? 'warn' : 'ok') : 'ok'),
      });
      if (row) row.querySelector('.meter-top').append(statusEl);
      this.infoHandlers.push((c, info) => {
        if (c !== 'battery') return;
        const st = info[`${g.id}.status`];
        if (st !== undefined) {
          state = st;
          statusEl.textContent = st === 'Not charging' ? 'Plugged in, not charging' : st;
        }
      });
      const bros = el('div', { class: 'readouts mini' });
      p.append(bros);
      this.readout(bros, k('power'), 'battery draw (whole system)');
      this.readout(bros, k('time_left'), 'remaining');
      this.readout(bros, k('health'), 'health');
      this.readout(bros, k('cycles'), 'cycles');
      this.readout(bros, k('energy'), 'stored');
    }
    if (psus.length) {
      const pros = el('div', { class: 'readouts mini' });
      p.append(pros);
      const plug = (v) => [v >= 0.5 ? 'connected' : 'unplugged', ''];
      for (const g of psus) {
        const label = g.label.startsWith('AC adapter') ? 'AC adapter' : 'USB-C power';
        this.readout(pros, `${g.id}.online`, label, { fmt: plug });
        this.readout(pros, `${g.id}.contract_w`, 'USB-C negotiated');
      }
    }
  }

  buildFans(all) {
    const fans = all.filter((d) => d.unit === 'rpm');
    if (!fans.length) return;
    const p = this.panel('fans', 'Fans', null, 4);
    const ros = el('div', { class: 'readouts compact' });
    p.append(ros);
    const perGroup = new Map();
    for (const f of fans) perGroup.set(f.group, (perGroup.get(f.group) || 0) + 1);
    const name = (f) => {
      const g = f.group_label.replace(/ \(.*\)$/, '').replace('embedded controller', 'EC');
      const n = /^fan(\d+)$/.exec(f.label);
      if (perGroup.get(f.group) === 1) return n ? g : `${g} ${f.label}`;
      return n ? `${g} fan ${n[1]}` : `${g} ${f.label}`;
    };
    for (const f of fans) this.readout(ros, f.key, name(f));
    const colors = ['--s1', '--s2', '--s3', '--s4'];
    this.chart(p, {
      unit: 'rpm',
      minTop: 1000,
      height: 110,
      series: fans.slice(0, 4).map((f, i) => ({ keys: [f.key], label: name(f), color: colors[i] })),
    });
  }

  buildSystem() {
    const p = this.panel('system', 'System', null, 4);
    const ros = el('div', { class: 'readouts compact' });
    p.append(ros);
    this.readout(ros, 'sys.uptime', 'uptime', { fmt: duration });
    this.readout(ros, 'proc.count', 'processes');
    this.readout(ros, 'proc.threads', 'threads');
    this.readout(ros, 'sys.open_files', 'open files');
    const vals = {};
    const rows = [
      ['platform_profile', 'Power profile'],
      ['governor', 'CPU governor'],
      ['epp', 'Energy preference'],
      ['cpufreq_driver', 'Frequency driver'],
      ['boost', 'Boost'],
    ];
    const dl = this.facts(p, rows.map(([id, label]) => {
      vals[id] = el('span', {}, '–');
      return [label, vals[id]];
    }));
    this.infoHandlers.push((c, info) => {
      if (c !== 'system') return;
      for (const [id] of rows) if (info[id] !== undefined) vals[id].textContent = info[id];
    });
    const host = this.meta.host || {};
    if (host.bios) dl.append(el('div', {}, el('dt', {}, 'Firmware'), el('dd', {}, host.bios)));
  }

  buildProcesses() {
    if (!this.has('proc.count')) return;
    const p = this.panel('processes', 'Processes', 'Grouped by application; select one to see its processes and their details. CPU is per thread: 100% is one fully used thread.', 12);
    const ros = el('div', { class: 'readouts compact' });
    p.append(ros);
    this.readout(ros, 'proc.count', 'processes');
    this.readout(ros, 'proc.running', 'running');
    this.readout(ros, 'proc.blocked', 'uninterruptible (D)');
    this.readout(ros, 'proc.zombie', 'zombies');
    let mode = 'cpu';
    const bCpu = el('button', { type: 'button', class: 'seg', 'aria-pressed': 'true' }, 'By CPU');
    const bMem = el('button', { type: 'button', class: 'seg', 'aria-pressed': 'false' }, 'By memory');
    p.querySelector('.panel-head').append(el('div', { class: 'segmented', role: 'group', 'aria-label': 'Sort applications' }, bCpu, bMem));
    const tbody = el('tbody');
    const table = el('table', { class: 'procs apps' },
      el('thead', {}, el('tr', {},
        el('th', { scope: 'col' }, 'Application'),
        el('th', { scope: 'col', class: 'num' }, 'Processes'),
        el('th', { scope: 'col' }, 'User'),
        el('th', { scope: 'col', class: 'num' }, 'CPU'),
        el('th', { scope: 'col', class: 'num' }, 'Memory'),
        el('th', { scope: 'col', class: 'num' }, 'GPU memory'),
        el('th', { scope: 'col', class: 'num' }, 'Threads'))),
      tbody);
    p.append(el('div', { class: 'table-wrap' }, table));
    let lastData = null;
    const render = () => {
      if (!lastData) return;
      const rows = (mode === 'cpu' ? lastData.apps_cpu : lastData.apps_mem) || [];
      const focusApp = document.activeElement?.closest?.('tr[data-app]')?.dataset.app;
      tbody.replaceChildren(...rows.map((r) => {
        // A single-process application opens the process details directly.
        const single = r.count === 1 && r.pids?.length === 1;
        const open = () => (single ? this.appDetail.openProcess(r.pids[0], r.name) : this.appDetail.open(r.name));
        let gpu = 0;
        for (const pid of r.pids || []) gpu += this.gpuProcs.get(pid)?.mem || 0;
        const tr = el('tr', {
          class: 'clickable',
          tabindex: '0',
          role: 'button',
          'data-app': r.name,
          'aria-label': single ? `Show details of ${r.name}` : `Show the processes of ${r.name}`,
        },
          el('td', { class: 'pname' }, r.name, r.multi ? el('span', { class: 'tag' }, 'multi-process') : null),
          el('td', { class: 'num' }, r.count),
          el('td', { class: 'muted' }, (r.users || []).join(', ')),
          el('td', { class: 'num' }, `${r.cpu.toFixed(1)}%`),
          el('td', { class: 'num' }, bytes(r.rss).join(' ')),
          el('td', { class: 'num muted' }, gpu ? bytes(gpu).join(' ') : '–'),
          el('td', { class: 'num' }, r.threads));
        tr.addEventListener('click', open);
        tr.addEventListener('keydown', (e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            open();
          }
        });
        return tr;
      }));
      if (focusApp) {
        for (const tr of tbody.children) {
          if (tr.dataset.app === focusApp) tr.focus({ preventScroll: true });
        }
      }
    };
    const setMode = (m) => {
      mode = m;
      bCpu.setAttribute('aria-pressed', String(m === 'cpu'));
      bMem.setAttribute('aria-pressed', String(m === 'mem'));
      render();
    };
    bCpu.addEventListener('click', () => setMode('cpu'));
    bMem.addEventListener('click', () => setMode('mem'));
    this.procHandlers.push((d) => {
      lastData = d;
      render();
    });
  }
}

export function cpuTempKey(all) {
  const sens = all.filter((d) => d.collector === 'sensors' && d.metric === 'temp');
  const prefs = [
    (d) => /^(k10temp|zenpower)\./.test(d.group) && /^(tctl|tdie)$/i.test(d.label),
    (d) => d.group.startsWith('coretemp.') && /^package/i.test(d.label),
    (d) => /^(k10temp|coretemp|zenpower|cpu_thermal)\./.test(d.group),
  ];
  for (const p of prefs) {
    const f = sens.find(p);
    if (f) return f.key;
  }
  const th = all.find((d) => d.collector === 'thermal' && /x86_pkg|cpu/i.test(d.label));
  return th ? th.key : null;
}

// Live store and uPlot chart wrapper.
//
// Live mode scrolls continuously: the x window trails "now" by one sampling
// interval and is redrawn every animation frame (frame rate depends on the
// smoothing setting), so new points slide in from the right instead of
// jumping. The y scale eases toward its target. History mode shows bucketed
// averages with a min/max band for single-series charts.

import { axis, byUnit, clock } from './format.js';

const RING_SEC = 15 * 60 + 30;

export const store = {
  series: new Map(),
  offsetMs: 0, // server clock minus browser clock
  windowSec: 300,
  fps: 30,

  now() {
    return (Date.now() + this.offsetMs) / 1000;
  },

  ensure(key) {
    let s = this.series.get(key);
    if (!s) {
      s = { t: [], v: [], gap: 5 };
      this.series.set(key, s);
    }
    return s;
  },

  push(key, tSec, v, gapSec) {
    const s = this.ensure(key);
    s.gap = gapSec;
    const n = s.t.length;
    if (n) {
      const lt = s.t[n - 1];
      if (tSec <= lt) return;
      if (tSec - lt > gapSec) {
        s.t.push(lt + 0.001);
        s.v.push(null);
      }
    }
    s.t.push(tSec);
    s.v.push(v);
    if (s.t.length > 64 && s.t[0] < tSec - RING_SEC) {
      let i = 0;
      while (i < s.t.length && s.t[i] < tSec - RING_SEC) i++;
      s.t.splice(0, i);
      s.v.splice(0, i);
    }
  },

  latest(key) {
    const s = this.series.get(key);
    if (!s) return null;
    for (let i = s.v.length - 1; i >= 0; i--) if (s.v[i] != null) return s.v[i];
    return null;
  },

  clear() {
    this.series.clear();
  },
};

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

function alpha(hex, a) {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex);
  if (!m) return hex;
  const n = parseInt(m[1], 16);
  return `rgba(${(n >> 16) & 255},${(n >> 8) & 255},${n & 255},${a})`;
}

// Mantissas with the number of axis segments that keep tick labels round.
const NICE = [[1, 4], [1.2, 4], [1.5, 3], [1.6, 4], [2, 4], [2.5, 5], [3, 3], [3.2, 4], [4, 4], [5, 5], [6, 3], [6.4, 4], [8, 4], [10, 5]];

// Round maximum for an axis, in binary steps for byte units, plus the
// number of segments to split it into.
function niceScale(v, unit) {
  if (!(v > 0)) return { max: 1, segs: 4 };
  let k = 1;
  if (unit === 'B' || unit === 'B/s') {
    while (v / k >= 1024) k *= 1024;
  }
  const x = v / k;
  const p = Math.pow(10, Math.floor(Math.log10(x)));
  for (const [m, segs] of NICE) {
    if (m * p >= x - 1e-9) return { max: m * p * k, segs };
  }
  return { max: 10 * p * k, segs: 5 };
}

const IEC = ['B', 'K', 'M', 'G', 'T'];

// Tick labels; byte axes use one unit for every tick (taken from the top).
function yValues(unit, vals, fmt) {
  if (unit !== 'B' && unit !== 'B/s') return vals.map(fmt);
  const top = Math.max(...vals.map((v) => Math.abs(v || 0)));
  let i = 0;
  let k = 1;
  while (top / k >= 1000 && i < IEC.length - 1) { k *= 1024; i++; }
  return vals.map((v) => {
    if (v == null) return '';
    if (v === 0) return '0';
    const x = v / k;
    return `${+x.toFixed(x !== 0 && Math.abs(x) < 10 ? 1 : 0)} ${IEC[i]}`;
  });
}

const timeFmt = {
  sec: new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' }),
  min: new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }),
  day: new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' }),
  dayTime: new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }),
};

// Time tick labels that always say hours and minutes (and seconds only
// when ticks fall between minutes).
function timeTicks(u, splits) {
  const span = u.scales.x.max - u.scales.x.min;
  const step = splits.length > 1 ? splits[1] - splits[0] : span;
  return splits.map((t) => {
    const d = new Date(t * 1000);
    if (span > 3 * 86400) return step >= 86400 ? timeFmt.day.format(d) : timeFmt.dayTime.format(d);
    if (step < 60 && Math.round(t) % 60 !== 0) return timeFmt.sec.format(d);
    return timeFmt.min.format(d);
  });
}

// Merge several (t, v) series onto one time axis. Missing points hold the
// previous value for up to `hold` seconds (series from different collectors
// tick at different moments), otherwise become null (a gap).
function align(raws, hold) {
  if (raws.length === 1) return [raws[0].t, [raws[0].v]];
  const same = raws.every((r) => r.t.length === raws[0].t.length
    && r.t[0] === raws[0].t[0] && r.t[r.t.length - 1] === raws[0].t[raws[0].t.length - 1]);
  if (same) return [raws[0].t, raws.map((r) => r.v)];
  const set = new Set();
  for (const r of raws) for (const t of r.t) set.add(t);
  const xs = Array.from(set).sort((a, b) => a - b);
  const ys = raws.map((r) => {
    const out = new Array(xs.length);
    let j = 0;
    let prevT = -Infinity;
    let prevV = null;
    for (let i = 0; i < xs.length; i++) {
      const x = xs[i];
      while (j < r.t.length && r.t[j] <= x) {
        prevT = r.t[j];
        prevV = r.v[j];
        j++;
      }
      out[i] = prevT === x || x - prevT <= hold ? prevV : null;
    }
    return out;
  });
  return [xs, ys];
}

// Sum several keys that share timestamps (same collector).
function combine(keys, get) {
  const first = get(keys[0]);
  if (!first) return null;
  if (keys.length === 1) return first;
  const maps = keys.slice(1).map((k) => {
    const s = get(k);
    const m = new Map();
    if (s) for (let i = 0; i < s.t.length; i++) m.set(s.t[i], s.v[i]);
    return m;
  });
  const v = first.v.map((x, i) => {
    if (x == null) return null;
    let sum = x;
    for (const m of maps) {
      const y = m.get(first.t[i]);
      if (y != null) sum += y;
    }
    return sum;
  });
  return { t: first.t, v };
}

const charts = new Set();
let loopStarted = false;
let lastFrame = 0;

function loop(ts) {
  requestAnimationFrame(loop);
  const minDt = 1000 / Math.max(store.fps, 1);
  if (ts - lastFrame < minDt - 2) return;
  lastFrame = ts;
  const now = store.now();
  for (const c of charts) c.tick(now);
}

export class Chart {
  // cfg: { series: [{keys, label, color}], unit, min, max (number|fn|null),
  //        stacked, height, lagSec: fn, title }
  constructor(host, cfg) {
    this.cfg = { min: 0, max: null, stacked: false, height: 140, ...cfg };
    this.host = host;
    this.mode = 'live';
    this.visible = true;
    this.dirty = true;
    this.yCur = null;
    this.yTarget = 1;
    this.lastYCalc = 0;
    this.raw = null;
    this.history = null;

    host.classList.add('chart');
    if (this.cfg.series.length > 1) {
      const ul = document.createElement('ul');
      ul.className = 'legend';
      for (const s of this.cfg.series) {
        const li = document.createElement('li');
        const k = document.createElement('span');
        k.className = 'key';
        k.style.setProperty('--c', `var(${s.color})`);
        li.append(k, document.createTextNode(s.label));
        ul.append(li);
      }
      host.append(ul);
    }
    this.plot = document.createElement('div');
    this.plot.className = 'plot';
    this.plot.style.height = `${this.cfg.height}px`;
    host.append(this.plot);
    this.tip = document.createElement('div');
    this.tip.className = 'tip';
    this.tip.hidden = true;
    this.plot.append(this.tip);

    if (!window.uPlot) {
      const p = document.createElement('p');
      p.className = 'muted small';
      p.textContent = 'Charts could not be loaded; live values are still shown.';
      this.plot.append(p);
      return;
    }
    this.io = new IntersectionObserver((es) => {
      for (const e of es) this.visible = e.isIntersecting;
    });
    this.io.observe(this.plot);
    this.ro = new ResizeObserver(() => this.resize());
    this.ro.observe(this.plot);
    this.create();
    charts.add(this);
    if (!loopStarted) {
      loopStarted = true;
      requestAnimationFrame(loop);
    }
  }

  keys() {
    return this.cfg.series.flatMap((s) => s.keys);
  }

  create() {
    if (this.u) this.u.destroy();
    const ink2 = cssVar('--muted');
    const hair = cssVar('--hair');
    const surface = cssVar('--surface');
    const stacked = this.cfg.stacked;
    const single = this.cfg.series.length === 1;
    const history = this.mode === 'history';
    const spline = window.uPlot.paths && window.uPlot.paths.spline ? window.uPlot.paths.spline() : undefined;
    const series = [{}];
    const colors = this.cfg.series.map((s) => cssVar(s.color) || '#2a78d6');
    this.cfg.series.forEach((s, i) => {
      series.push({
        label: s.label,
        stroke: colors[i],
        width: 2,
        fill: stacked ? alpha(colors[i], 0.28) : single ? alpha(colors[i], 0.1) : undefined,
        paths: history ? undefined : spline,
        points: { show: false },
        spanGaps: false,
      });
    });
    const bands = [];
    if (stacked) {
      for (let i = 2; i <= this.cfg.series.length; i++) {
        bands.push({ series: [i, i - 1], fill: alpha(colors[i - 1], 0.28) });
      }
    }
    if (history && single) {
      // hidden min/max series for the range band
      series.push({ label: 'min', stroke: 'transparent', width: 0, points: { show: false }, spanGaps: false });
      series.push({ label: 'max', stroke: 'transparent', width: 0, points: { show: false }, spanGaps: false });
      bands.push({ series: [3, 2], fill: alpha(colors[0], 0.14) });
      series[1].fill = undefined;
    }
    const font = '500 11px Barlow, system-ui, sans-serif';
    const yfmt = axis(this.cfg.unit);
    const rect = this.plot.getBoundingClientRect();
    const opts = {
      width: Math.max(120, Math.floor(rect.width)),
      height: this.cfg.height,
      padding: [6, 6, 0, 0],
      legend: { show: false },
      cursor: {
        drag: { x: false, y: false, setScale: false },
        y: false,
        points: { size: 8, width: 2, stroke: surface },
      },
      select: { show: false },
      scales: {
        x: { time: true, auto: false },
        y: { auto: false, range: (u, mn, mx) => [mn, mx] },
      },
      axes: [
        {
          stroke: ink2,
          font,
          size: 24,
          space: 80,
          values: timeTicks,
          grid: { show: false },
          ticks: { show: true, stroke: cssVar('--axis'), width: 1, size: 3 },
        },
        {
          stroke: ink2,
          font,
          size: 46,
          space: 28,
          gap: 4,
          grid: { stroke: hair, width: 1 },
          ticks: { show: false },
          splits: (u, i, mn, mx) => {
            let n = this.ySegs || 4;
            if (this.cfg.height < 100) n = n === 4 ? 2 : n === 3 ? 3 : 1;
            const out = [];
            for (let j = 0; j <= n; j++) out.push(mn + ((mx - mn) * j) / n);
            return out;
          },
          values: (u, vals) => yValues(this.cfg.unit, vals, yfmt),
        },
      ],
      series,
      bands,
      hooks: {
        setCursor: [(u) => this.showTip(u)],
      },
    };
    this.u = new window.uPlot(opts, this.emptyData(), this.plot);
    this.plot.append(this.tip);
    this.dirty = true;
    this.yCur = null;
    if (history && this.history) this.applyHistory();
  }

  emptyData() {
    const n = this.u ? this.u.series.length : this.cfg.series.length + 1 + (this.mode === 'history' && this.cfg.series.length === 1 ? 2 : 0);
    return Array.from({ length: n }, () => []);
  }

  resize() {
    if (!this.u) return;
    const w = Math.floor(this.plot.getBoundingClientRect().width);
    if (w > 0 && Math.abs(w - this.u.width) > 1) {
      this.u.setSize({ width: w, height: this.cfg.height });
      if (this.mode === 'history') this.u.redraw();
    }
  }

  restyle() {
    if (window.uPlot && this.u) this.create();
  }

  markDirty() {
    this.dirty = true;
  }

  maxValue() {
    const m = this.cfg.max;
    return typeof m === 'function' ? m() : m;
  }

  buildLive() {
    const raws = [];
    for (const s of this.cfg.series) {
      const r = combine(s.keys, (k) => store.series.get(k));
      raws.push(r || { t: [], v: [] });
    }
    const hold = (this.cfg.lagSec ? this.cfg.lagSec() : 2) * 3 + 1;
    const [xs, ys] = align(raws, hold);
    this.raw = [xs, ...ys];
    let plotted = ys;
    if (this.cfg.stacked) {
      const acc = new Array(xs.length).fill(0);
      plotted = ys.map((y) => y.map((v, i) => {
        if (v == null) return null;
        acc[i] += v;
        return acc[i];
      }));
    }
    this.u.setData([xs, ...plotted], false);
    this.plottedYs = plotted;
  }

  computeYTarget(xmin) {
    const fixed = this.maxValue();
    if (fixed != null && Number.isFinite(fixed)) {
      if (fixed === 100) {
        this.yTarget = 100;
        this.ySegs = 4;
      } else {
        const ns = niceScale(fixed, this.cfg.unit);
        this.yTarget = ns.max;
        this.ySegs = ns.segs;
      }
      return;
    }
    let mx = 0;
    const xs = this.raw ? this.raw[0] : [];
    let i0 = 0;
    while (i0 < xs.length && xs[i0] < xmin) i0++;
    for (const y of this.plottedYs || []) {
      for (let i = Math.max(0, i0 - 1); i < y.length; i++) if (y[i] != null && y[i] > mx) mx = y[i];
    }
    const floor = this.cfg.minTop ?? 1;
    const ns = niceScale(Math.max(mx * 1.12, floor), this.cfg.unit);
    this.yTarget = ns.max;
    this.ySegs = ns.segs;
  }

  tick(now) {
    if (!this.u || this.mode !== 'live' || !this.visible) return;
    if (this.dirty) {
      this.buildLive();
      this.dirty = false;
    }
    const lag = (this.cfg.lagSec ? this.cfg.lagSec() : 1) + 0.25;
    const xmax = now - lag;
    const xmin = xmax - store.windowSec;
    const t = performance.now();
    if (t - this.lastYCalc > 250 || this.yCur == null) {
      this.computeYTarget(xmin);
      this.lastYCalc = t;
    }
    if (this.yCur == null || store.fps <= 1) this.yCur = this.yTarget;
    else this.yCur += (this.yTarget - this.yCur) * 0.12;
    if (Math.abs(this.yCur - this.yTarget) < this.yTarget * 0.002) this.yCur = this.yTarget;
    const u = this.u;
    u.batch(() => {
      u.setScale('x', { min: xmin, max: xmax });
      u.setScale('y', { min: this.cfg.min ?? 0, max: this.yCur });
    });
  }

  setLive() {
    this.want = 'live';
    if (this.mode === 'live' && this.u) return;
    this.mode = 'live';
    this.history = null;
    this.host.classList.remove('loading');
    if (window.uPlot) this.create();
  }

  async loadHistory(from, to, fetcher) {
    if (!window.uPlot) return;
    this.want = 'history';
    this.host.classList.add('loading');
    const points = Math.max(60, Math.min(2000, Math.floor(this.plot.getBoundingClientRect().width)));
    try {
      const res = await fetcher(this.keys(), from, to, points);
      if (this.want !== 'history') return;
      this.history = { res, from, to };
      if (this.mode !== 'history') {
        this.mode = 'history';
        this.create();
      } else {
        this.applyHistory();
      }
    } catch (e) {
      if (this.want !== 'history') return;
      this.history = { res: { series: {} }, from, to, error: String(e) };
      if (this.mode !== 'history') {
        this.mode = 'history';
        this.create();
      } else {
        this.applyHistory();
      }
    } finally {
      this.host.classList.remove('loading');
    }
  }

  applyHistory() {
    const { res, from, to } = this.history;
    const get = (k) => {
      const s = res.series ? res.series[k] : null;
      return s ? { t: s.t, v: s.avg, min: s.min, max: s.max } : null;
    };
    const raws = this.cfg.series.map((s) => combine(s.keys, get) || { t: [], v: [] });
    const [xs, ys] = align(raws, 0);
    this.raw = [xs, ...ys];
    let plotted = ys;
    if (this.cfg.stacked) {
      const acc = new Array(xs.length).fill(0);
      plotted = ys.map((y) => y.map((v, i) => {
        if (v == null) return null;
        acc[i] += v;
        return acc[i];
      }));
    }
    const data = [xs, ...plotted];
    let mx = 0;
    for (const y of plotted) for (const v of y) if (v != null && v > mx) mx = v;
    if (this.cfg.series.length === 1 && this.cfg.series[0].keys.length === 1) {
      const s = get(this.cfg.series[0].keys[0]);
      const mins = s ? s.min : [];
      const maxs = s ? s.max : [];
      data.push(xs.map((_, i) => (mins[i] ?? null)));
      data.push(xs.map((_, i) => (maxs[i] ?? null)));
      for (const v of maxs) if (v != null && v > mx) mx = v;
    } else if (this.u.series.length > data.length) {
      while (data.length < this.u.series.length) data.push(xs.map(() => null));
    }
    const fixed = this.maxValue();
    let ymax;
    if (fixed === 100) {
      ymax = 100;
      this.ySegs = 4;
    } else if (fixed != null && Number.isFinite(fixed)) {
      const ns = niceScale(fixed, this.cfg.unit);
      ymax = ns.max;
      this.ySegs = ns.segs;
    } else {
      const ns = niceScale(Math.max(mx * 1.08, this.cfg.minTop ?? 1), this.cfg.unit);
      ymax = ns.max;
      this.ySegs = ns.segs;
    }
    this.u.batch(() => {
      this.u.setData(data, false);
      this.u.setScale('x', { min: from, max: to });
      this.u.setScale('y', { min: this.cfg.min ?? 0, max: ymax });
    });
  }

  showTip(u) {
    const i = u.cursor.idx;
    if (i == null || !this.raw || u.cursor.left < 0) {
      this.tip.hidden = true;
      return;
    }
    const xs = this.raw[0];
    if (i >= xs.length) {
      this.tip.hidden = true;
      return;
    }
    const fmt = byUnit(this.cfg.unit);
    this.tip.replaceChildren();
    const time = document.createElement('div');
    time.className = 'tip-time';
    time.textContent = clock(xs[i], this.mode === 'history' && (this.history?.to - this.history?.from) > 86400);
    this.tip.append(time);
    this.cfg.series.forEach((s, k) => {
      const v = this.raw[k + 1] ? this.raw[k + 1][i] : null;
      const row = document.createElement('div');
      row.className = 'tip-row';
      const key = document.createElement('span');
      key.className = 'key';
      key.style.setProperty('--c', `var(${s.color})`);
      const val = document.createElement('strong');
      const [n, un] = v == null ? ['–', ''] : fmt(v);
      val.textContent = un ? `${n} ${un}` : n;
      const lab = document.createElement('span');
      lab.textContent = s.label;
      row.append(key, val, lab);
      this.tip.append(row);
    });
    this.tip.hidden = false;
    const left = u.cursor.left + u.over.offsetLeft;
    const w = this.tip.offsetWidth;
    const pw = this.plot.clientWidth;
    this.tip.style.left = `${left + 12 + w > pw ? left - w - 12 : left + 12}px`;
    this.tip.style.top = `${u.over.offsetTop + 4}px`;
  }

  destroy() {
    charts.delete(this);
    if (this.io) this.io.disconnect();
    if (this.ro) this.ro.disconnect();
    if (this.u) this.u.destroy();
    this.u = null;
  }
}

export function allCharts() {
  return charts;
}

// Process overlay with two views in one dialog:
// - application: every process of one application, a breakdown by process
//   type for multi-process programs (browsers, Electron apps) and GPU
//   memory per process when NVIDIA data is present (refresh 3 s);
// - process: details of one process from /proc/<pid> (refresh 2 s), with
//   CPU and memory trends since the view was opened.
// Rows, the parent and child processes open the process view; a back
// button returns to the previous view.

import { el } from './cards.js';
import { bytes, duration } from './format.js';

const BROWSER_HINT = 'Each tab, extension and service runs in its own process. Linux does not record which tab a process belongs to; for per-tab usage open the browser task manager (Chrome, Edge, Brave: Shift+Esc; Firefox: about:processes).';
const MEMORY_HINT = 'Memory is resident memory (RSS). Memory shared between processes is counted in each of them, so the total can be higher than the real use.';
const RESTRICTED_HINT = 'The kernel shows the executable, working directory, open files and disk I/O only to the process owner and root. The service runs as the hwstats user.';
const TREND_S = 300;
const THREADS_SHOWN = 40;

const STATE = {
  R: 'running',
  S: 'sleeping',
  D: 'waiting (I/O)',
  Z: 'zombie',
  T: 'stopped',
  t: 'traced',
  I: 'idle',
  X: 'dead',
};

function fmtBytes(v) {
  return v == null ? '–' : bytes(v).join(' ');
}

function fmtCpuTime(s) {
  if (s == null) return '–';
  if (s < 60) return `${s.toFixed(s < 10 ? 2 : 1)} s`;
  return duration(s)[0];
}

function num(v) {
  return v == null ? '–' : Number(v).toLocaleString('en-US');
}

function pidButton(pid, label) {
  return el('button', { type: 'button', class: 'linkish', 'data-pid': pid }, label);
}

function procLabel(p) {
  return p.kind ? `${p.app} (${p.kind})` : p.app;
}

// Small trend line: 0-based for CPU, min-max for memory. Hovering shows the
// value at that time in the caption.
function trend(label, fmt, zeroBased) {
  const NS = 'http://www.w3.org/2000/svg';
  const W = 300;
  const H = 56;
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', `0 0 ${W} ${H}`);
  svg.setAttribute('preserveAspectRatio', 'none');
  svg.setAttribute('class', 'trend-svg');
  svg.setAttribute('aria-hidden', 'true');
  const area = document.createElementNS(NS, 'path');
  area.setAttribute('class', 'trend-area');
  const line = document.createElementNS(NS, 'path');
  line.setAttribute('class', 'trend-line');
  const cross = document.createElementNS(NS, 'line');
  cross.setAttribute('class', 'trend-cross');
  cross.setAttribute('y1', '0');
  cross.setAttribute('y2', String(H));
  cross.style.display = 'none';
  svg.append(area, line, cross);
  const value = el('span', { class: 'trend-val' }, '–');
  const range = el('span', { class: 'trend-range' });
  const fig = el('figure', { class: 'trend' },
    el('figcaption', {}, el('span', { class: 'trend-label' }, label), value, range),
    svg);
  let pts = [];
  let scale = null;
  let hover = null;

  const draw = () => {
    if (!pts.length) return;
    // The window grows from one minute up to TREND_S as points arrive.
    const t1 = pts[pts.length - 1].t;
    const span = Math.min(TREND_S * 1000, Math.max(60_000, t1 - pts[0].t));
    const t0 = t1 - span;
    const vs = pts.map((p) => p.v);
    let lo = zeroBased ? 0 : Math.min(...vs);
    let hi = Math.max(...vs);
    if (zeroBased) hi = Math.max(hi * 1.15, 5);
    if (hi - lo < 1e-9) {
      hi += zeroBased ? 1 : Math.max(hi * 0.01, 1);
      if (!zeroBased) lo = Math.max(0, lo - Math.max(lo * 0.01, 1));
    } else if (!zeroBased) {
      const pad = (hi - lo) * 0.15;
      hi += pad;
      lo = Math.max(0, lo - pad);
    }
    scale = { t0, t1, lo, hi };
    const x = (t) => ((t - t0) / (t1 - t0)) * W;
    const y = (v) => H - 2 - ((v - lo) / (hi - lo)) * (H - 4);
    const d = pts.map((p, i) => `${i ? 'L' : 'M'}${x(p.t).toFixed(1)},${y(p.v).toFixed(1)}`).join('');
    line.setAttribute('d', d);
    area.setAttribute('d', `${d}L${x(t1).toFixed(1)},${H}L${x(pts[0].t).toFixed(1)},${H}Z`);
    const peak = Math.max(...vs);
    range.textContent = zeroBased ? `peak ${fmt(peak)}` : `${fmt(Math.min(...vs))} – ${fmt(peak)}`;
    showValue();
  };
  const showValue = () => {
    if (hover != null && scale) {
      let best = pts[0];
      for (const p of pts) if (Math.abs(p.t - hover) < Math.abs(best.t - hover)) best = p;
      const x = ((best.t - scale.t0) / (scale.t1 - scale.t0)) * W;
      cross.setAttribute('x1', x.toFixed(1));
      cross.setAttribute('x2', x.toFixed(1));
      cross.style.display = '';
      value.textContent = `${fmt(best.v)} at ${new Date(best.t).toLocaleTimeString()}`;
    } else {
      cross.style.display = 'none';
      value.textContent = pts.length ? fmt(pts[pts.length - 1].v) : '–';
    }
  };
  svg.addEventListener('pointermove', (e) => {
    if (!scale) return;
    const r = svg.getBoundingClientRect();
    hover = scale.t0 + ((e.clientX - r.left) / r.width) * (scale.t1 - scale.t0);
    showValue();
  });
  svg.addEventListener('pointerleave', () => {
    hover = null;
    showValue();
  });
  return {
    el: fig,
    push(t, v) {
      if (v == null || !Number.isFinite(v)) return;
      pts.push({ t, v });
      const cut = t - TREND_S * 1000;
      while (pts.length > 2 && pts[0].t < cut) pts.shift();
      draw();
    },
    reset() {
      pts = [];
    },
  };
}

export class AppDetail {
  // opts: { fetchApp(app), fetchProc(pid), gpu() -> Map(pid -> {mem, sm, gpu}) }
  // fetch functions reject with an Error that carries `status`.
  constructor(opts) {
    this.opts = opts;
    this.stack = [];
    this.gen = 0;
    this.timer = 0;
    this.d = el('dialog', { class: 'proc-dialog', 'aria-labelledby': 'pd-title' });
    this.backBtn = el('button', { type: 'button', class: 'back-btn', hidden: true });
    this.backBtn.addEventListener('click', () => this.back());
    this.title = el('h2', { id: 'pd-title' });
    this.sub = el('p', { class: 'panel-sub' });
    const close = el('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Close details' }, '×');
    close.addEventListener('click', () => this.d.close());
    this.body = el('div', { class: 'dlg-body' });
    this.d.append(el('div', { class: 'dlg' },
      el('div', { class: 'dlg-head' }, el('div', { class: 'dlg-titles' }, this.backBtn, this.title, this.sub), close),
      this.body));
    this.d.addEventListener('click', (e) => {
      if (e.target === this.d) {
        this.d.close();
        return;
      }
      const t = e.target.closest('[data-pid]');
      if (t && this.body.contains(t)) this.push({ type: 'proc', pid: Number(t.dataset.pid) });
    });
    this.body.addEventListener('keydown', (e) => {
      if (e.key !== 'Enter' && e.key !== ' ') return;
      const t = e.target.closest('tr[data-pid]');
      if (!t) return;
      e.preventDefault();
      this.push({ type: 'proc', pid: Number(t.dataset.pid) });
    });
    this.d.addEventListener('close', () => {
      // The event is asynchronous: ignore it when the dialog was reopened.
      if (this.d.open) return;
      clearTimeout(this.timer);
      this.gen += 1;
      this.stack = [];
      this.pv = null;
    });
    document.body.append(this.d);
  }

  get view() {
    return this.stack[this.stack.length - 1];
  }

  open(app) {
    this.stack = [{ type: 'app', app }];
    this.show();
  }

  openProcess(pid, app) {
    this.stack = [{ type: 'proc', pid, name: app }];
    this.show();
  }

  push(view) {
    const cur = this.view;
    if (cur && cur.type === 'proc' && view.type === 'proc' && cur.pid === view.pid) return;
    this.stack.push(view);
    if (this.stack.length > 20) this.stack.shift();
    this.show();
  }

  back() {
    if (this.stack.length < 2) return;
    this.stack.pop();
    this.show();
  }

  titleOf(v) {
    if (v.type === 'app') return v.app;
    return v.name ? `${v.name} (PID ${v.pid})` : `PID ${v.pid}`;
  }

  show() {
    this.gen += 1;
    clearTimeout(this.timer);
    this.pv = null;
    const v = this.view;
    this.title.textContent = this.titleOf(v);
    const prev = this.stack[this.stack.length - 2];
    this.backBtn.hidden = !prev;
    if (prev) this.backBtn.textContent = `‹ ${this.titleOf(prev)}`;
    this.sub.textContent = 'Loading…';
    this.body.replaceChildren();
    this.body.scrollTop = 0;
    if (!this.d.open) this.d.showModal();
    this.refresh();
  }

  async refresh() {
    const gen = this.gen;
    const v = this.view;
    if (!v) return;
    let again = true;
    try {
      if (v.type === 'app') {
        const data = await this.opts.fetchApp(v.app);
        if (gen !== this.gen) return;
        this.renderApp(v, data.processes || []);
      } else {
        const data = await this.opts.fetchProc(v.pid);
        if (gen !== this.gen) return;
        if (v.start != null && data.start_ticks !== v.start) {
          this.exited(v, this.pv
            ? 'This process has exited and its PID now belongs to another process. Showing the last values read.'
            : 'This process is not running any more (its PID now belongs to another process).');
          again = false;
        } else {
          this.renderProc(v, data);
        }
      }
    } catch (e) {
      if (gen !== this.gen) return;
      if (v.type === 'proc' && e.status === 404) {
        this.exited(v, this.pv ? 'This process has exited. Showing the last values read.' : 'This process is not running any more.');
        again = false;
      } else {
        this.sub.textContent = `Cannot load the details: ${e.message}`;
      }
    }
    if (again && gen === this.gen && this.d.open) {
      this.timer = setTimeout(() => this.refresh(), v.type === 'app' ? 3000 : 2000);
    }
  }

  exited(v, msg) {
    this.sub.textContent = msg;
    this.body.classList.add('is-stale');
    if (!this.pv) this.body.replaceChildren();
  }

  // ---------- application view ----------

  renderApp(v, procs) {
    const gpu = this.opts.gpu();
    const scroll = this.body.scrollTop;
    const focusPid = document.activeElement?.closest?.('tr[data-pid]')?.dataset.pid;
    this.body.classList.remove('is-stale');
    if (!procs.length) {
      this.sub.textContent = 'No running processes (the application may have exited).';
      this.body.replaceChildren();
      return;
    }
    procs.sort((a, b) => b.cpu - a.cpu || b.rss - a.rss);
    const users = Array.from(new Set(procs.map((p) => p.user).filter(Boolean)));
    const multi = procs.some((p) => p.kind);
    const sum = (f) => procs.reduce((s, p) => s + (f(p) || 0), 0);
    const gpuMem = sum((p) => gpu.get(p.pid)?.mem);
    this.sub.textContent = `${procs.length} process${procs.length === 1 ? '' : 'es'}${users.length ? `, user ${users.join(', ')}` : ''}. Select a process for its details.`;

    const ro = (value, label) => el('div', { class: 'ro' },
      el('div', { class: 'ro-val' }, el('span', { class: 'ro-num' }, value)),
      el('div', { class: 'ro-label' }, label));
    const kids = [el('div', { class: 'readouts compact' },
      ro(`${sum((p) => p.cpu).toFixed(1)}%`, 'CPU (100% = one thread)'),
      ro(fmtBytes(sum((p) => p.rss)), 'memory (RSS)'),
      ro(String(sum((p) => p.threads)), 'threads'),
      gpuMem > 0 ? ro(fmtBytes(gpuMem), 'GPU memory') : null)];

    if (multi) {
      const kinds = new Map();
      for (const p of procs) {
        const k = p.kind || 'main process';
        const e = kinds.get(k) || { n: 0, cpu: 0, rss: 0, gpu: 0 };
        e.n += 1;
        e.cpu += p.cpu;
        e.rss += p.rss;
        e.gpu += gpu.get(p.pid)?.mem || 0;
        kinds.set(k, e);
      }
      const rows = Array.from(kinds.entries()).sort((a, b) => b[1].cpu - a[1].cpu || b[1].rss - a[1].rss);
      kids.push(el('h3', { class: 'sub-head' }, 'By process type'));
      kids.push(el('div', { class: 'table-wrap' }, el('table', { class: 'procs' },
        el('thead', {}, el('tr', {},
          el('th', { scope: 'col' }, 'Type'),
          el('th', { scope: 'col', class: 'num' }, 'Processes'),
          el('th', { scope: 'col', class: 'num' }, 'CPU'),
          el('th', { scope: 'col', class: 'num' }, 'Memory'),
          gpuMem > 0 ? el('th', { scope: 'col', class: 'num' }, 'GPU memory') : null)),
        el('tbody', {}, rows.map(([k, e]) => el('tr', {},
          el('td', { class: 'pname' }, k),
          el('td', { class: 'num' }, e.n),
          el('td', { class: 'num' }, `${e.cpu.toFixed(1)}%`),
          el('td', { class: 'num' }, fmtBytes(e.rss)),
          gpuMem > 0 ? el('td', { class: 'num' }, e.gpu ? fmtBytes(e.gpu) : '–') : null))))));
      kids.push(el('p', { class: 'help' }, BROWSER_HINT));
    }

    kids.push(el('h3', { class: 'sub-head' }, 'Processes'));
    kids.push(el('div', { class: 'table-wrap' }, el('table', { class: 'procs proc-list' },
      el('thead', {}, el('tr', {},
        el('th', { scope: 'col', class: 'num' }, 'PID'),
        multi ? el('th', { scope: 'col' }, 'Type') : null,
        el('th', { scope: 'col' }, 'User'),
        el('th', { scope: 'col', class: 'num' }, 'CPU'),
        el('th', { scope: 'col', class: 'num' }, 'Memory'),
        gpuMem > 0 ? el('th', { scope: 'col', class: 'num' }, 'GPU') : null,
        el('th', { scope: 'col', class: 'num' }, 'Threads'),
        el('th', { scope: 'col' }, 'State'),
        el('th', { scope: 'col', class: 'num' }, 'Running for'),
        el('th', { scope: 'col' }, 'Command'))),
      el('tbody', {}, procs.map((p) => el('tr', {
        class: 'clickable',
        tabindex: '0',
        'data-pid': p.pid,
        'aria-label': `Show details of process ${p.pid}`,
      },
      el('td', { class: 'num muted' }, p.pid),
      multi ? el('td', {}, p.kind || 'main process') : null,
      el('td', { class: 'muted' }, p.user),
      el('td', { class: 'num' }, `${p.cpu.toFixed(1)}%`),
      el('td', { class: 'num' }, fmtBytes(p.rss)),
      gpuMem > 0 ? el('td', { class: 'num' }, gpu.get(p.pid) ? fmtBytes(gpu.get(p.pid).mem) : '–') : null,
      el('td', { class: 'num' }, p.threads),
      el('td', { class: 'muted', title: STATE[p.state] || '' }, STATE[p.state] || p.state),
      el('td', { class: 'num muted' }, duration(p.age_s)[0]),
      el('td', { class: 'cmd', title: p.cmd }, p.cmd)))))));
    kids.push(el('p', { class: 'help' }, MEMORY_HINT));
    this.body.replaceChildren(...kids);
    this.body.scrollTop = scroll;
    if (focusPid) this.body.querySelector(`tr[data-pid="${focusPid}"]`)?.focus({ preventScroll: true });
  }

  // ---------- process view ----------

  renderProc(v, d) {
    this.body.classList.remove('is-stale');
    if (v.start == null) v.start = d.start_ticks;
    if (!v.name) {
      v.name = d.kernel_thread ? d.comm : d.app || d.comm;
      this.title.textContent = this.titleOf(v);
    }
    const sub = [d.kind || (d.kernel_thread ? 'kernel thread' : null), STATE[d.state] || d.state, d.user ? `user ${d.euser ? `${d.user} (effective ${d.euser})` : d.user}` : null];
    this.sub.textContent = sub.filter(Boolean).join(', ');
    if (!this.pv) this.pv = this.buildProc(d);
    this.pv.update(d);
  }

  buildProc(d0) {
    const vals = {};
    const val = (id) => (vals[id] = el('span', {}, '–'));
    const ro = (id, label) => el('div', { class: 'ro', 'data-ro': id },
      el('div', { class: 'ro-val' }, el('span', { class: 'ro-num' }, val(id))),
      el('div', { class: 'ro-label' }, label));
    const dl = (rows) => el('dl', { class: 'facts' }, rows.map(([label, node]) =>
      el('div', {}, el('dt', {}, label), el('dd', {}, node))));
    const gpuMap = () => this.opts.gpu();
    const isKernel = d0.kernel_thread;

    const readouts = el('div', { class: 'readouts compact' },
      ro('cpu', 'CPU (100% = one thread)'),
      ro('rss', 'memory (RSS)'),
      isKernel ? null : ro('swap', 'swap'),
      ro('threads', 'threads'),
      isKernel ? null : ro('fds', 'open files'),
      isKernel ? null : ro('io_r', 'disk read'),
      isKernel ? null : ro('io_w', 'disk write'));
    const gpuRo = ro('gpu', 'GPU memory');
    gpuRo.hidden = true;
    readouts.append(gpuRo);

    const cpuTrend = trend('CPU', (x) => `${x.toFixed(1)}%`, true);
    const memTrend = trend('Memory', fmtBytes, false);

    const parentCell = el('span');
    const identity = dl([
      ['Process name', val('comm')],
      ['Parent', parentCell],
      ['Started', val('started')],
      ['Systemd unit', val('unit')],
      ['Executable', val('exe')],
      ['Working directory', val('cwd')],
      ['Session, group', val('sess')],
      ['Sandbox', val('sandbox')],
    ]);
    const sched = dl([
      ['CPU time', val('cputime')],
      ['Children CPU time', val('ccpu')],
      ['Priority', val('prio')],
      ['Scheduling', val('policy')],
      ['Last ran on', val('processor')],
      ['Allowed CPUs', val('cpus')],
      ['Context switches', val('ctx')],
      ['OOM score', val('oom')],
    ]);
    const memRows = dl([
      ['Resident (RSS)', val('m_rss')],
      ['Anonymous', val('m_anon')],
      ['File-backed', val('m_file')],
      ['Shared memory', val('m_shmem')],
      ['Peak resident', val('m_peak_rss')],
      ['Swapped out', val('m_swap')],
      ['Virtual size', val('m_virt')],
      ['Data and heap', val('m_data')],
      ['Stack', val('m_stack')],
      ['Libraries', val('m_lib')],
      ['Page tables', val('m_pte')],
      ['Locked', val('m_locked')],
    ]);
    memRows.classList.add('facts-2');
    const ioRows = dl([
      ['Read from disk', val('io_rb')],
      ['Written to disk', val('io_wb')],
      ['Read (all I/O)', val('io_rc')],
      ['Written (all I/O)', val('io_wc')],
      ['Read calls', val('io_sr')],
      ['Write calls', val('io_sw')],
    ]);
    ioRows.classList.add('facts-2');
    const restricted = el('p', { class: 'help' }, RESTRICTED_HINT);
    restricted.hidden = true;
    const ioHead = el('h3', { class: 'sub-head' }, 'Disk I/O');

    const cmd = el('pre', { class: 'cmdline' });
    const childHead = el('h3', { class: 'sub-head' }, 'Child processes');
    const childBody = el('tbody');
    const childTable = el('div', { class: 'table-wrap' }, el('table', { class: 'procs proc-list' },
      el('thead', {}, el('tr', {},
        el('th', { scope: 'col', class: 'num' }, 'PID'),
        el('th', { scope: 'col' }, 'Process'),
        el('th', { scope: 'col', class: 'num' }, 'CPU'),
        el('th', { scope: 'col', class: 'num' }, 'Memory'),
        el('th', { scope: 'col', class: 'num' }, 'Threads'))),
      childBody));
    const childEmpty = el('p', { class: 'muted small' }, 'No child processes.');
    const threadHead = el('h3', { class: 'sub-head' }, 'Threads');
    const threadBody = el('tbody');
    const threadNote = el('p', { class: 'help' });
    const threadTable = el('div', { class: 'table-wrap' }, el('table', { class: 'procs proc-list' },
      el('thead', {}, el('tr', {},
        el('th', { scope: 'col' }, 'Thread'),
        el('th', { scope: 'col', class: 'num' }, 'TID'),
        el('th', { scope: 'col' }, 'State'),
        el('th', { scope: 'col', class: 'num' }, 'CPU'),
        el('th', { scope: 'col', class: 'num' }, 'CPU time'),
        el('th', { scope: 'col', class: 'num' }, 'Last CPU'))),
      threadBody));

    this.body.replaceChildren(...[
      readouts,
      el('div', { class: 'trends' }, cpuTrend.el, memTrend.el),
      el('div', { class: 'pd-cols' }, identity, sched),
      restricted,
      el('h3', { class: 'sub-head' }, 'Memory'), memRows,
      isKernel ? null : ioHead,
      isKernel ? null : ioRows,
      isKernel ? null : el('h3', { class: 'sub-head' }, 'Command line'),
      isKernel ? null : cmd,
      childHead, childTable, childEmpty,
      threadHead, threadTable, threadNote].filter(Boolean));

    let prev = null;
    const prevThreads = new Map();
    let argsKey = '';
    const set = (id, text) => {
      if (vals[id] && vals[id].textContent !== text) vals[id].textContent = text;
    };
    const rateText = (now, before, dt) => (before == null || now == null || dt <= 0 ? '–' : `${fmtBytes(Math.max(0, now - before) / dt)}/s`);

    const update = (d) => {
      const t = d.now_ms || Date.now();
      const dt = prev ? (t - prev.t) / 1000 : 0;
      let cpu = d.cpu;
      if (prev && dt > 0.2) cpu = Math.max(0, ((d.cpu_s - prev.cpu_s) / dt) * 100);
      set('cpu', cpu == null ? '–' : `${cpu.toFixed(1)}%`);
      set('rss', fmtBytes(d.mem.rss));
      set('swap', fmtBytes(d.mem.swap));
      set('threads', num(d.threads));
      // Readouts of values the kernel hides are dropped (the note explains).
      const io = d.io;
      for (const r of readouts.querySelectorAll('[data-ro]')) {
        const id = r.dataset.ro;
        if (id === 'fds') r.hidden = d.fds == null;
        else if (id === 'io_r' || id === 'io_w') r.hidden = !io;
      }
      set('fds', d.fds == null ? '–' : `${num(d.fds)}${d.fd_limit ? ` of ${num(d.fd_limit)}` : ''}`);
      set('io_r', io ? rateText(io.read_bytes, prev?.io?.read_bytes, dt) : '–');
      set('io_w', io ? rateText(io.write_bytes, prev?.io?.write_bytes, dt) : '–');
      const g = gpuMap().get(d.pid);
      gpuRo.hidden = !g;
      if (g) set('gpu', fmtBytes(g.mem));
      if (cpu != null) cpuTrend.push(t, cpu);
      memTrend.push(t, d.mem.rss);

      set('comm', d.comm);
      const parent = d.parent ? `${d.ppid} ${procLabel(d.parent)}` : (d.ppid ? String(d.ppid) : 'none (started by the kernel)');
      if (parentCell.dataset.text !== parent) {
        parentCell.dataset.text = parent;
        parentCell.replaceChildren(d.ppid ? pidButton(d.ppid, parent) : parent);
      }
      set('started', d.started_ms ? `${new Date(d.started_ms).toLocaleString()} (${duration(d.age_s)[0]} ago)` : '–');
      set('unit', d.unit || d.cgroup || '–');
      const hidden = (k) => (d.restricted.includes(k) ? 'hidden' : '–');
      set('exe', d.exe || hidden('exe'));
      set('cwd', d.cwd || hidden('cwd'));
      set('sess', `session ${d.session}, process group ${d.pgrp}`);
      const sandbox = [];
      if (d.seccomp === 2) sandbox.push('seccomp filter');
      else if (d.seccomp === 1) sandbox.push('strict seccomp');
      if (d.no_new_privs) sandbox.push('no new privileges');
      set('sandbox', sandbox.length ? sandbox.join(', ') : 'none');

      set('cputime', `${fmtCpuTime(d.cpu_s)} (user ${fmtCpuTime(d.user_s)}, system ${fmtCpuTime(d.system_s)})`);
      set('ccpu', fmtCpuTime(d.children_cpu_s));
      set('prio', d.policy === 'normal' || d.policy === 'batch' || d.policy === 'idle'
        ? `nice ${d.nice}${d.nice < 0 ? ' (higher priority)' : d.nice > 0 ? ' (lower priority)' : ''}`
        : `real-time priority ${d.rt_priority}`);
      set('policy', d.policy);
      set('processor', d.processor == null ? '–' : `CPU ${d.processor}`);
      set('cpus', d.cpus_allowed || '–');
      set('ctx', d.ctx_voluntary == null ? '–' : `${num(d.ctx_voluntary)} voluntary, ${num(d.ctx_involuntary)} forced`);
      set('oom', d.oom_score == null ? '–' : `${d.oom_score}${d.oom_score_adj ? ` (adjustment ${d.oom_score_adj > 0 ? '+' : ''}${d.oom_score_adj})` : ''}`);

      const m = d.mem;
      for (const k of ['rss', 'anon', 'file', 'shmem', 'peak_rss', 'swap', 'virt', 'data', 'stack', 'lib', 'pte', 'locked']) set(`m_${k}`, fmtBytes(m[k]));
      if (io) {
        set('io_rb', fmtBytes(io.read_bytes));
        set('io_wb', fmtBytes(io.write_bytes));
        set('io_rc', fmtBytes(io.rchar));
        set('io_wc', fmtBytes(io.wchar));
        set('io_sr', num(io.syscr));
        set('io_sw', num(io.syscw));
      }
      ioRows.hidden = !io;
      ioHead.hidden = !io;
      restricted.hidden = !d.restricted.length;

      const key = (d.args || []).join('\0');
      if (key !== argsKey) {
        argsKey = key;
        cmd.textContent = (d.args || []).join('\n') + (d.args_truncated ? '\n…' : '');
      }

      const kids = d.children || [];
      childHead.textContent = kids.length ? `Child processes (${kids.length})` : 'Child processes';
      childTable.hidden = !kids.length;
      childEmpty.hidden = kids.length > 0;
      const focusPid = document.activeElement?.closest?.('tr[data-pid]')?.dataset.pid;
      childBody.replaceChildren(...kids.map((c) => el('tr', {
        class: 'clickable', tabindex: '0', 'data-pid': c.pid, 'aria-label': `Show details of process ${c.pid}`,
      },
      el('td', { class: 'num muted' }, c.pid),
      el('td', { class: 'pname' }, procLabel(c)),
      el('td', { class: 'num' }, c.cpu == null ? '–' : `${c.cpu.toFixed(1)}%`),
      el('td', { class: 'num' }, fmtBytes(c.rss)),
      el('td', { class: 'num' }, c.threads))));
      if (focusPid) childBody.querySelector(`tr[data-pid="${focusPid}"]`)?.focus({ preventScroll: true });

      const threads = (d.thread_list || []).map((th) => {
        const before = prevThreads.get(th.tid);
        const pct = before != null && dt > 0.2 ? Math.max(0, ((th.cpu_s - before) / dt) * 100) : null;
        return { ...th, pct };
      });
      prevThreads.clear();
      for (const th of d.thread_list || []) prevThreads.set(th.tid, th.cpu_s);
      threads.sort((a, b) => (b.pct ?? -1) - (a.pct ?? -1) || b.cpu_s - a.cpu_s);
      threadHead.textContent = `Threads (${num(d.threads_total)})`;
      threadBody.replaceChildren(...threads.slice(0, THREADS_SHOWN).map((th) => el('tr', {},
        el('td', { class: 'pname' }, th.name),
        el('td', { class: 'num muted' }, th.tid),
        el('td', { class: 'muted' }, STATE[th.state] || th.state),
        el('td', { class: 'num' }, th.pct == null ? '–' : `${th.pct.toFixed(1)}%`),
        el('td', { class: 'num' }, fmtCpuTime(th.cpu_s)),
        el('td', { class: 'num muted' }, th.processor ?? '–'))));
      threadNote.textContent = d.threads_total > THREADS_SHOWN
        ? `Showing the ${THREADS_SHOWN} busiest of ${num(d.threads_total)} threads.`
        : '';
      threadNote.hidden = d.threads_total <= THREADS_SHOWN;

      prev = { t, cpu_s: d.cpu_s, io: d.io };
    };
    return { update };
  }
}

// Smooth number transitions. Each Tween approaches its target with an
// exponential ease whose time constant comes from the smoothing setting, so
// values glide instead of jumping. Respects prefers-reduced-motion.

const TAU = { off: 0, low: 160, medium: 420, high: 900 };
const active = new Set();
let tau = TAU.medium;
let raf = 0;
let last = 0;
const reduced = window.matchMedia('(prefers-reduced-motion: reduce)');

export function setSmoothing(level) {
  tau = reduced.matches ? 0 : (TAU[level] ?? TAU.medium);
}

export function smoothingLevel() {
  return tau;
}

function frame(ts) {
  const dt = last ? Math.min(ts - last, 1000) : 16;
  last = ts;
  const k = tau > 0 ? 1 - Math.exp(-dt / tau) : 1;
  for (const t of active) {
    const diff = t.target - t.cur;
    if (Math.abs(diff) <= Math.max(Math.abs(t.target) * 1e-4, 1e-6) || k >= 1) {
      t.cur = t.target;
      active.delete(t);
    } else {
      t.cur += diff * k;
    }
    t.apply(t.cur);
  }
  raf = active.size ? requestAnimationFrame(frame) : 0;
  if (!raf) last = 0;
}

export class Tween {
  constructor(apply) {
    this.apply = apply;
    this.cur = null;
    this.target = null;
  }

  set(v) {
    if (!Number.isFinite(v)) return;
    this.target = v;
    if (this.cur === null || tau === 0 || document.hidden) {
      this.cur = v;
      active.delete(this);
      this.apply(v);
      return;
    }
    active.add(this);
    if (!raf) raf = requestAnimationFrame(frame);
  }
}

// Bind a tween to an element that shows [number, unit].
export function numberTween(numEl, unitEl, fmt) {
  return new Tween((v) => {
    const [n, u] = fmt(v);
    if (numEl.textContent !== n) numEl.textContent = n;
    if (unitEl && unitEl.textContent !== u) unitEl.textContent = u;
  });
}

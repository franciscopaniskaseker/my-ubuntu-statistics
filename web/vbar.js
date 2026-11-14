// Grouped vertical bar charts (CPU per core, temperatures per chip).
//
// A group is a small chart: faint gridlines at every quarter, one bar per
// item with a small gap, the value above the bar (inside it when the bar is
// nearly full), item labels under the bars and the group name below. Bar
// heights and values glide with the shared smoothing setting.

import { Tween } from './tween.js';

// Text color for a label drawn inside a filled bar.
const INSIDE_DARK = new Set(['--warn']);

function makeBar(item, scale) {
  const col = document.createElement('div');
  col.className = 'bg-col';
  col.setAttribute('role', 'meter');
  col.setAttribute('aria-label', item.aria || item.label);
  col.setAttribute('aria-valuemin', String(scale.min));
  col.setAttribute('aria-valuemax', String(scale.max));

  const fill = document.createElement('div');
  fill.className = 'bg-fill';
  const val = document.createElement('div');
  val.className = 'bg-val';
  const num = document.createElement('span');
  num.textContent = '–';
  const unit = document.createElement('small');
  val.append(num, unit);
  col.append(fill, val);

  let color = '--s1';
  const geom = new Tween((v) => {
    const p = scale.max > scale.min ? Math.max(0, Math.min(100, ((v - scale.min) / (scale.max - scale.min)) * 100)) : 0;
    fill.style.height = `${p.toFixed(1)}%`;
    const inside = p > 78;
    val.classList.toggle('inside', inside);
    val.classList.toggle('on-light', inside && INSIDE_DARK.has(color));
    val.style.bottom = inside ? `calc(${p.toFixed(1)}% - 1.45em)` : `calc(${p.toFixed(1)}% + 2px)`;
  });
  const fig = new Tween((v) => {
    const [n, u] = item.fmt(v);
    if (num.textContent !== n) num.textContent = n;
    if (unit.textContent !== u) unit.textContent = u;
  });

  return {
    col,
    set(v) {
      color = item.color ? item.color(v) : '--s1';
      col.style.setProperty('--bc', `var(${color})`);
      geom.set(v);
      fig.set(v);
      col.setAttribute('aria-valuenow', String(Math.round(v * 10) / 10));
      if (item.note) {
        const t = item.note(v);
        col.title = t ? `${item.aria || item.label}: ${t}` : (item.aria || item.label);
      }
    },
  };
}

// items: [{ label, aria, fmt: v => [num, unit], color: v => css var, note }]
// scale: { min, max } shared by the bars of the group (for gridlines).
export function makeBarGroup(title, items, scale) {
  const root = document.createElement('section');
  root.className = 'bgroup';
  root.style.setProperty('--n', items.length);
  const plot = document.createElement('div');
  plot.className = 'bg-plot';
  const labels = document.createElement('div');
  labels.className = 'bg-labels';
  const bars = items.map((it) => {
    const b = makeBar(it, scale);
    plot.append(b.col);
    const l = document.createElement('span');
    l.textContent = it.label;
    l.title = it.aria || it.label;
    labels.append(l);
    return b;
  });
  const name = document.createElement('div');
  name.className = 'bg-name';
  name.textContent = title;
  root.append(plot, labels, name);
  return { el: root, bars };
}

// Number formatting. Every formatter returns [number, unit] so the unit can
// be typeset smaller than the figure.

export const prefs = { tempUnit: 'C' };

const IEC = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];

function fixed(v, d) {
  if (!Number.isFinite(v)) return '–';
  return v.toFixed(d);
}

export function bytes(v) {
  if (!Number.isFinite(v)) return ['–', 'B'];
  let i = 0;
  let x = Math.abs(v);
  while (x >= 1024 && i < IEC.length - 1) { x /= 1024; i++; }
  const s = Math.sign(v) * x;
  return [i === 0 ? fixed(s, 0) : fixed(s, x >= 100 ? 0 : x >= 10 ? 1 : 2), IEC[i]];
}

export function rate(v) {
  const [n, u] = bytes(v);
  return [n, u + '/s'];
}

export function pct(v) {
  return [fixed(v, Math.abs(v) >= 99.95 ? 0 : 1), '%'];
}

export function temp(v) {
  if (prefs.tempUnit === 'F') return [fixed(v * 9 / 5 + 32, 1), '°F'];
  return [fixed(v, 1), '°C'];
}

export function mhz(v) {
  if (v >= 1000) return [fixed(v / 1000, 2), 'GHz'];
  return [fixed(v, 0), 'MHz'];
}

export function watts(v) {
  return [fixed(v, v < 10 ? 2 : 1), 'W'];
}

export function count(v) {
  if (!Number.isFinite(v)) return ['–', ''];
  return [Math.round(v).toLocaleString('en-US'), ''];
}

export function perSec(v) {
  if (!Number.isFinite(v)) return ['–', '/s'];
  if (v >= 1e6) return [fixed(v / 1e6, 2) + 'M', '/s'];
  if (v >= 1e4) return [fixed(v / 1e3, 1) + 'k', '/s'];
  return [v < 10 ? fixed(v, 1) : Math.round(v).toLocaleString('en-US'), '/s'];
}

export function duration(sec) {
  if (!Number.isFinite(sec)) return ['–', ''];
  sec = Math.max(0, Math.round(sec));
  const d = Math.floor(sec / 86400);
  const h = Math.floor((sec % 86400) / 3600);
  const m = Math.floor((sec % 3600) / 60);
  if (d > 0) return [`${d} d ${h} h`, ''];
  if (h > 0) return [`${h} h ${m} min`, ''];
  if (m > 0) return [`${m} min`, ''];
  return [`${sec} s`, ''];
}

const BY_UNIT = {
  '%': pct,
  C: temp,
  rpm: (v) => [fixed(v, 0), 'rpm'],
  W: watts,
  V: (v) => [fixed(v, 3), 'V'],
  A: (v) => [fixed(v, 2), 'A'],
  MHz: mhz,
  B: bytes,
  'B/s': rate,
  '/s': perSec,
  ms: (v) => [fixed(v, v < 10 ? 2 : 1), 'ms'],
  s: duration,
  count,
  Wh: (v) => [fixed(v, 1), 'Wh'],
  bool: (v) => [v >= 0.5 ? 'yes' : 'no', ''],
  dBm: (v) => [fixed(v, 0), 'dBm'],
};

export function byUnit(unit) {
  return BY_UNIT[unit] || ((v) => [fixed(v, 2), unit || '']);
}

export function text(unit, v) {
  const [n, u] = byUnit(unit)(v);
  return u ? `${n} ${u}` : n;
}

// Axis tick formatter: compact, no decimals where avoidable.
export function axis(unit) {
  return (v) => {
    if (v == null) return '';
    switch (unit) {
      case '%': return `${Math.round(v)}%`;
      case 'C': return prefs.tempUnit === 'F' ? `${Math.round(v * 9 / 5 + 32)}°` : `${Math.round(v)}°`;
      case 'B':
      case 'B/s': {
        const [n, u] = bytes(v);
        return `${parseFloat(n)} ${u.replace('iB', '')}`;
      }
      case 'W': return `${+v.toFixed(1)} W`;
      case 'rpm': return v >= 1000 ? `${+(v / 1000).toFixed(1)}k` : `${Math.round(v)}`;
      case 'MHz': return v >= 1000 ? `${+(v / 1000).toFixed(1)}G` : `${Math.round(v)}`;
      default: return Math.abs(v) >= 1000 ? `${+(v / 1000).toFixed(1)}k` : `${+v.toFixed(2)}`;
    }
  };
}

export function clock(sec, withDate) {
  const d = new Date(sec * 1000);
  const t = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: withDate ? undefined : '2-digit' });
  if (!withDate) return t;
  return `${d.toLocaleDateString([], { month: 'short', day: 'numeric' })} ${t}`;
}

// Convert a °C value for display scales.
export function tempValue(v) {
  return prefs.tempUnit === 'F' ? v * 9 / 5 + 32 : v;
}

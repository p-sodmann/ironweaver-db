/* Ironweaver DB operator console: what both pages share (step 16a). Formatting, the core's values as text,
 * label colours and shapes, the theme, the page links in the rail, the "Go to anything" palette and a graph
 * layout. Classic script: defines globalThis.IW.ui (and module.exports for node --test). */
(function (root) {
  'use strict';
  const IW = (root.IW = root.IW || {});

  /* ------------------------------------------------------------------ numbers, sizes, times */
  const group = (n) => String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ' '); // the design system's grouping: 1 204 331
  function num(n, digits = 0) {
    if (n == null || Number.isNaN(n)) return '—';
    const [i, f] = Math.abs(n).toFixed(digits).split('.');
    return (n < 0 ? '−' : '') + group(i) + (f ? '.' + f : '');
  }
  function bytes(b) {
    if (b == null) return '—';
    const u = ['B', 'KB', 'MB', 'GB', 'TB']; let i = 0; let v = b;
    while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
    return (i === 0 ? String(v) : v.toFixed(v < 10 ? 1 : 0)) + ' ' + u[i];
  }
  const ms = (x) => (x == null ? '—' : x < 10 ? x.toFixed(1) + ' ms' : x < 1000 ? Math.round(x) + ' ms' : (x / 1000).toFixed(1) + ' s');
  function span(sec) {
    sec = Math.max(0, Math.round(sec));
    const d = Math.floor(sec / 86400), h = Math.floor((sec % 86400) / 3600), m = Math.floor((sec % 3600) / 60), s = sec % 60;
    return d ? `${d} d ${h} h` : h ? `${h} h ${m} min` : m ? `${m} min ${s} s` : `${s} s`;
  }
  const date = (micros) => (micros ? new Date(micros / 1000).toISOString().slice(0, 16).replace('T', ' ') : '—');
  const pct = (f) => (f == null ? '—' : (f * 100 >= 10 ? Math.round(f * 100) : (f * 100).toFixed(1)) + ' %');

  /* ------------------------------------------------------------------ the core's values (REST JSON form) */
  const kindOf = (v) => (v === 'None' || v == null ? 'None' : Object.keys(v)[0]);
  const TYPE = { String: 'STRING', Int: 'INT', Float: 'FLOAT', Bool: 'BOOL', None: 'NONE', List: 'LIST', Dict: 'DICT', Bytes: 'BYTES', Date: 'DATE', DateTime: 'DATETIME' };
  function text(v) {
    const k = kindOf(v);
    if (k === 'None') return 'None';
    if (k === 'List') return '[' + v.List.map(text).join(', ') + ']';
    if (k === 'Dict') return '{' + Object.entries(v.Dict).map(([a, b]) => a + ': ' + text(b)).join(', ') + '}';
    if (k === 'Float') return String(v.Float);
    return String(v[k]);
  }
  /** What an edited value means: the kind it had if the text still fits it, else JSON, else a string. */
  function parseValue(s, kind) {
    const t = s.trim();
    if ((kind === 'Int' || !kind) && /^-?\d+$/.test(t)) return { Int: parseInt(t, 10) };
    if ((kind === 'Float' || kind === 'Int' || !kind) && /^-?\d+(\.\d+)?([eE][-+]?\d+)?$/.test(t)) return { Float: parseFloat(t) };
    if ((kind === 'Bool' || !kind) && /^(true|false)$/.test(t)) return { Bool: t === 'true' };
    if (kind === 'Date' && /^\d{4}-\d{2}-\d{2}$/.test(t)) return { Date: t };
    if (kind === 'DateTime' && /^\d{4}-\d{2}-\d{2}T/.test(t)) return { DateTime: t };
    if (t === 'None') return 'None';
    if (/^[[{]/.test(t)) { try { return plainToValue(JSON.parse(t)); } catch (_) { /* not JSON: a string */ } }
    return { String: s };
  }
  function plainToValue(x) {
    if (x === null) return 'None';
    if (typeof x === 'boolean') return { Bool: x };
    if (typeof x === 'number') return Number.isInteger(x) ? { Int: x } : { Float: x };
    if (Array.isArray(x)) return { List: x.map(plainToValue) };
    if (typeof x === 'object') return { Dict: Object.fromEntries(Object.keys(x).sort().map((k) => [k, plainToValue(x[k])])) };
    return { String: String(x) };
  }
  const CAPTION_KEYS = ['name', 'title', 'sku', 'code', 'kind'];
  function caption(n) {
    for (const k of CAPTION_KEYS) { const v = n.attr && n.attr[k]; if (v && kindOf(v) === 'String') return v.String.length > 22 ? v.String.slice(0, 21) + '…' : v.String; }
    return n.id;
  }

  /* ------------------------------------------------------------------ labels: one colour and shape each, everywhere */
  // Shapes carry meaning (design system: circle an entity, diamond an event, square a record). Labels the
  // console doesn't know are entities. The colours lb-1..lb-5 go to labels in name order; the rest share lb-6.
  const SHAPES = { Order: 'diamond', Event: 'diamond', Encounter: 'diamond', Procedure: 'diamond', Part: 'square', Document: 'square', Record: 'square' };
  function labelStyles(names) {
    const sorted = [...new Set(names)].sort();
    const out = {}; sorted.forEach((l, i) => { out[l] = { shape: SHAPES[l] || 'circle', color: i < 5 ? 'lb-' + (i + 1) : 'lb-6' }; });
    return out;
  }
  /** The label a node is drawn with: the one with the most nodes (so ann, an Admin and a Person, is a Person). */
  function primaryLabel(n, counts) {
    if (!n.labels || !n.labels.length) return '(none)';
    return n.labels.slice().sort((a, b) => (counts[b] || 0) - (counts[a] || 0) || (a < b ? -1 : 1))[0];
  }

  /* ------------------------------------------------------------------ theme (per viewer: the OS unless pinned) */
  const store = {
    get: (k) => { try { return root.localStorage && root.localStorage.getItem(k); } catch (_) { return null; } },
    set: (k, v) => { try { root.localStorage && root.localStorage.setItem(k, v); } catch (_) { /* private mode: not remembered */ } },
  };
  function applyTheme(t) { if (!root.document) return; const el = root.document.documentElement; if (t === 'light' || t === 'dark') el.setAttribute('data-theme', t); else el.removeAttribute('data-theme'); }
  function cycleTheme() {
    const order = ['system', 'light', 'dark']; const cur = store.get('iwdb.theme') || 'system';
    const next = order[(order.indexOf(cur) + 1) % order.length]; store.set('iwdb.theme', next); applyTheme(next); return next;
  }
  applyTheme(store.get('iwdb.theme'));

  /* ------------------------------------------------------------------ a graph layout for query results */
  /** Places nodes ({id}) for the canvas: a few hundred rounds of springs and repulsion, the same every time
   *  for the same input. Returns a Map id -> {x, y} around (0, 0). */
  function layout(nodes, edges) {
    const n = nodes.length; const P = nodes.map((_, i) => { const a = i * 2.39996; const r = 34 * Math.sqrt(i + 0.5); return { x: Math.cos(a) * r, y: Math.sin(a) * r }; });
    const idx = new Map(nodes.map((m, i) => [m.id, i]));
    const E = edges.map((e) => [idx.get(e.s), idx.get(e.t)]).filter(([a, b]) => a != null && b != null && a !== b);
    const L = 116, rounds = n > 150 ? 120 : 260;
    for (let it = 0; it < rounds; it++) {
      const cool = 1 - it / rounds; const F = P.map(() => ({ x: 0, y: 0 }));
      for (let i = 0; i < n; i++) for (let j = i + 1; j < n; j++) {
        let dx = P[j].x - P[i].x, dy = P[j].y - P[i].y; const d2 = dx * dx + dy * dy + 0.01; const d = Math.sqrt(d2);
        if (d > 460) continue; const f = 9000 / d2; dx /= d; dy /= d;
        F[i].x -= dx * f; F[i].y -= dy * f; F[j].x += dx * f; F[j].y += dy * f;
      }
      for (const [a, b] of E) {
        let dx = P[b].x - P[a].x, dy = P[b].y - P[a].y; const d = Math.sqrt(dx * dx + dy * dy) || 0.01; const f = (d - L) * 0.06; dx /= d; dy /= d;
        F[a].x += dx * f; F[a].y += dy * f; F[b].x -= dx * f; F[b].y -= dy * f;
      }
      for (let i = 0; i < n; i++) { F[i].x -= P[i].x * 0.004; F[i].y -= P[i].y * 0.004; const m = Math.hypot(F[i].x, F[i].y); const cap = 18 * cool + 1; const k = m > cap ? cap / m : 1; P[i].x += F[i].x * k; P[i].y += F[i].y * k; }
    }
    return new Map(nodes.map((m, i) => [m.id, { x: Math.round(P[i].x), y: Math.round(P[i].y) }]));
  }

  /* ------------------------------------------------------------------ the console's source */
  function params() { try { return new URLSearchParams(root.location.search); } catch (_) { return new URLSearchParams(); } }
  /** The Source the pages read (contract: source.js). Only the mock exists so far; `?scenario=degraded` shows
   *  how problems look. */
  function source() {
    if (IW._source) return IW._source;
    const p = params();
    IW._source = IW.mock.create({ scenario: p.get('scenario') || 'calm' });
    return IW._source;
  }
  /** Links between the pages keep the source's parameters. */
  function href(page, extra) {
    const p = params(); const keep = new URLSearchParams();
    if (p.get('scenario')) keep.set('scenario', p.get('scenario'));
    Object.entries(extra || {}).forEach(([k, v]) => v != null && keep.set(k, v));
    const q = keep.toString(); return page + (q ? '?' + q : '');
  }

  /* ------------------------------------------------------------------ React pieces (only in a browser) */
  if (root.React && root.IronWeaver) {
    const R = root.React; const h = R.createElement; const { useState, useEffect, useRef, useMemo } = R;
    const I = root.IronWeaver;

    /** The page links and the mock marker, for the rail's children. */
    function RailLinks({ page }) {
      return h('span', { className: 'cs-links' },
        h('span', { className: 'cs-mockword', title: 'This console runs on generated data. Nothing here is read from or written to a server yet.' }, 'MOCK DATA'),
        h('nav', { className: 'cs-pages', 'aria-label': 'Pages' },
          [['explore', 'EXPLORE', 'index.html'], ['status', 'STATUS', 'status.html']].map(([id, label, file]) =>
            h('a', { key: id, href: href(file), className: 'cs-page' + (page === id ? ' is-on' : ''), 'aria-current': page === id ? 'page' : undefined }, label))));
    }

    /** "Go to anything" (⌘K): one list of what the page can jump to. Items: {group, title, detail?, label?, rel?,
     *  mono?, run}; `dynamic(text)` adds items made from what was typed (a node id to look up). */
    function Palette({ items, dynamic, onClose, placeholder, initial = '' }) {
      const [q, setQ] = useState(initial); const [cur, setCur] = useState(0);
      const list = useRef(null);
      const shown = useMemo(() => {
        const ql = q.trim().toLowerCase();
        const scored = items.map((it) => {
          const t = (it.title + ' ' + (it.detail || '') + ' ' + it.group).toLowerCase();
          if (!ql) return [it, 0];
          const at = it.title.toLowerCase().indexOf(ql);
          return [it, at === 0 ? 3 : at > 0 ? 2 : ql.split(/\s+/).every((w) => t.includes(w)) ? 1 : -1];
        }).filter(([, s]) => s >= 0);
        const typed = ql && dynamic ? dynamic(q.trim()) : [];
        return typed.concat(scored.sort((a, b) => b[1] - a[1]).slice(0, 40).map(([it]) => it));
      }, [items, q, dynamic]);
      useEffect(() => { setCur(0); }, [q]);
      useEffect(() => { const el = list.current && list.current.querySelector('.is-cur'); el && el.scrollIntoView({ block: 'nearest' }); }, [cur]);
      const go = (it) => { onClose(); it && it.run(); };
      return h('div', { className: 'cs-palette', role: 'dialog', 'aria-label': 'Go to anything', onClick: (e) => { if (e.target === e.currentTarget) onClose(); } },
        h('div', { className: 'cs-palette__card' },
          h('label', { className: 'cs-palette__in' },
            h(I.Glyph, { action: 'focus', size: 14 }),
            h('input', { autoFocus: true, value: q, placeholder: placeholder || 'A label, a node id, a namespace, a page or a command', 'aria-label': 'Go to', onChange: (e) => setQ(e.target.value),
              onKeyDown: (e) => {
                if (e.key === 'ArrowDown') { e.preventDefault(); setCur((c) => Math.min(shown.length - 1, c + 1)); }
                else if (e.key === 'ArrowUp') { e.preventDefault(); setCur((c) => Math.max(0, c - 1)); }
                else if (e.key === 'Enter') { e.preventDefault(); go(shown[cur]); }
                else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); onClose(); }
              } }),
            h(I.Key, { dim: true }, 'Esc')),
          h('div', { className: 'cs-palette__list', ref: list, role: 'listbox' },
            shown.length === 0 && h('div', { className: 'cs-palette__none iw-small iw-muted' }, 'Nothing called that. Check the spelling, then the schema.'),
            shown.map((it, i) => h('button', { key: it.group + ':' + it.title, type: 'button', role: 'option', 'aria-selected': i === cur, className: 'cs-palette__row' + (i === cur ? ' is-cur' : ''), onMouseEnter: () => setCur(i), onClick: () => go(it) },
              h('span', { className: 'cs-palette__glyph' }, it.label ? h(I.ShapeGlyph, { label: it.label }) : it.rel ? h('span', { className: 'iw-accent iw-mono-s' }, '→') : null),
              h('span', { className: it.mono ? 'iw-mono' : '' }, it.title),
              it.detail && h('span', { className: 'iw-mono-s iw-muted cs-palette__detail' }, it.detail),
              h('span', { className: 'iw-cap cs-palette__group' }, it.group))))));
    }

    /** Pages and theme, for every palette. */
    function commonItems(page) {
      return [
        { group: 'Page', title: 'Explore the graph', detail: 'index.html', run: () => { if (page !== 'explore') root.location.href = href('index.html'); } },
        { group: 'Page', title: 'Server status', detail: 'status.html', run: () => { if (page !== 'status') root.location.href = href('status.html'); } },
        { group: 'Command', title: 'Switch theme', detail: 'system → light → dark', run: cycleTheme },
        { group: 'Command', title: 'Show problems', detail: '?scenario=degraded', run: () => { root.location.href = href(page === 'status' ? 'status.html' : 'index.html', { scenario: 'degraded' }); } },
        { group: 'Command', title: 'Calm server', detail: '?scenario=calm', run: () => { root.location.href = href(page === 'status' ? 'status.html' : 'index.html', { scenario: 'calm' }); } },
      ];
    }

    IW.ui = Object.assign(IW.ui || {}, { RailLinks, Palette, commonItems });
  }

  const ui = { group, num, bytes, ms, span, date, pct, kindOf, TYPE, text, parseValue, plainToValue, caption, labelStyles, primaryLabel, layout, source, href, params, cycleTheme, store };
  IW.ui = Object.assign(IW.ui || {}, ui);
  if (typeof module !== 'undefined' && module.exports) module.exports = ui;
})(typeof globalThis !== 'undefined' ? globalThis : this);

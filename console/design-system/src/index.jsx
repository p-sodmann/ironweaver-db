/* IronWeaver::DB operator interface — component source.
   Vendored from the IronWeaver::DB design system into the operator console (step 16a); the console's
   changes are marked "console:" and listed in ../README.md. Each is an optional prop: left out, the
   component behaves as the design system's.
   Built with: esbuild index.jsx --bundle --format=iife --jsx-factory=h --jsx-fragment=Frag
   React is read from window.React; no imports, no network. */
const R = window.React;
const h = R.createElement;
const Frag = R.Fragment;
const { useState, useEffect, useRef, useMemo, useCallback, useLayoutEffect } = R;

/* ------------------------------------------------------------------ utils */
const cx = (...a) => a.filter(Boolean).join(' ');
const reduced = () => typeof matchMedia !== 'undefined' && matchMedia('(prefers-reduced-motion: reduce)').matches;
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
const fmt = (n) => String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ' ');
const easeDamped = (p) => 1 - Math.pow(1 - p, 3);
function tween(ms, onFrame, onDone) {
  if (reduced() || ms <= 0) { onFrame(1); onDone && onDone(); return () => {}; }
  let raf; const t0 = performance.now();
  const step = (t) => { const p = Math.min(1, (t - t0) / ms); onFrame(easeDamped(p)); if (p < 1) raf = requestAnimationFrame(step); else onDone && onDone(); };
  raf = requestAnimationFrame(step);
  return () => cancelAnimationFrame(raf);
}
function useSize(ref) {
  const [s, set] = useState({ w: 0, h: 0 });
  useLayoutEffect(() => {
    const el = ref.current; if (!el) return;
    const ro = new ResizeObserver(() => set({ w: el.clientWidth, h: el.clientHeight }));
    ro.observe(el); set({ w: el.clientWidth, h: el.clientHeight });
    return () => ro.disconnect();
  }, []);
  return s;
}

/* Hold ⌥ (Alt) anywhere: every shortcut prints itself next to its control. Release: they vanish. */
if (typeof window !== 'undefined' && !window.__iwKeys) {
  window.__iwKeys = 1;
  const set = (on) => document.documentElement.classList.toggle('iw-keys', on);
  window.addEventListener('keydown', (e) => { if (e.key === 'Alt') set(true); });
  window.addEventListener('keyup', (e) => { if (e.key === 'Alt') set(false); });
  window.addEventListener('blur', () => set(false));
}

/* ------------------------------------------------------------------ demo data */
const LABELS = {
  Patient:   { shape: 'circle',  color: 'lb-3' },
  Encounter: { shape: 'diamond', color: 'lb-1' },
  Diagnosis: { shape: 'circle',  color: 'lb-4' },
  Procedure: { shape: 'diamond', color: 'lb-2' },
  Document:  { shape: 'square',  color: 'lb-6' },
  Clinician: { shape: 'circle',  color: 'lb-5' },
};
const SPAWN = {
  Patient:   [['Encounter', 'HAD'], ['Diagnosis', 'HAS_DX'], ['Document', 'DESCRIBED_IN']],
  Encounter: [['Clinician', 'ATTENDED_BY'], ['Procedure', 'INCLUDED'], ['Document', 'PRODUCED']],
  Diagnosis: [['Document', 'MENTIONED_IN'], ['Procedure', 'INDICATES']],
  Procedure: [['Clinician', 'PERFORMED_BY'], ['Document', 'REPORTED_IN'], ['Diagnosis', 'FOUND']],
  Document:  [['Clinician', 'AUTHORED_BY'], ['Diagnosis', 'MENTIONS']],
  Clinician: [['Encounter', 'ATTENDED'], ['Procedure', 'PERFORMED']],
};
const CAPS = {
  Encounter: ['ENC 26-09-12', 'ENC 26-09-28', 'ENC 26-10-01', 'ENC 26-08-30'],
  Diagnosis: ['K50.1', 'K51.0', 'D12.6', 'K92.2', 'E11.9'],
  Procedure: ['Koloskopie', 'ÖGD', 'ERCP', 'EUS', 'Biopsie'],
  Document:  ['Epikrise', 'Befund', 'Arztbrief', 'Histologie', 'Konsil'],
  Clinician: ['CLN-07', 'CLN-12', 'CLN-31', 'CLN-44'],
  Patient:   ['P-1182', 'P-2093', 'P-0417'],
};
let SEQ = 6100;
const demoGraph = () => ({
  nodes: [
    { id: '4:1182', label: 'Patient',   caption: 'P-1182',       x: 0,    y: 0 },
    { id: '4:2210', label: 'Encounter', caption: 'ENC 26-09-12', x: 118,  y: -46 },
    { id: '4:2211', label: 'Encounter', caption: 'ENC 26-09-28', x: 104,  y: 78 },
    { id: '4:3051', label: 'Diagnosis', caption: 'K50.1',        x: -112, y: -62 },
    { id: '4:5120', label: 'Document',  caption: 'Epikrise',     x: -96,  y: 84 },
    { id: '4:7002', label: 'Procedure', caption: 'Koloskopie',   x: 236,  y: -8 },
  ],
  edges: [
    { id: 'r:1', s: '4:1182', t: '4:2210', type: 'HAD' },
    { id: 'r:2', s: '4:1182', t: '4:2211', type: 'HAD' },
    { id: 'r:3', s: '4:1182', t: '4:3051', type: 'HAS_DX' },
    { id: 'r:4', s: '4:1182', t: '4:5120', type: 'DESCRIBED_IN' },
    { id: 'r:5', s: '4:2210', t: '4:7002', type: 'INCLUDED' },
  ],
});
function propsFor(n) {
  const num = parseInt(String(n.id).split(':')[1], 10) || 0;
  const base = {
    Patient:   [['mrn', 'STRING', 'UKW-00' + (90000 + num)], ['born', 'DATE', '1961-04-17'], ['sex', 'STRING', 'f'], ['insured', 'BOOLEAN', 'true']],
    Encounter: [['start', 'DATETIME', '2026-09-12T08:14'], ['ward', 'STRING', 'M2 Gastro'], ['los_days', 'INTEGER', '4']],
    Diagnosis: [['icd10', 'STRING', n.caption], ['primary', 'BOOLEAN', 'true'], ['certainty', 'STRING', 'G']],
    Procedure: [['ops', 'STRING', '1-650.2'], ['name', 'STRING', n.caption], ['duration_min', 'INTEGER', '38']],
    Document:  [['kind', 'STRING', n.caption], ['pages', 'INTEGER', '3'], ['sha256', 'STRING', '9f2c…e01a']],
    Clinician: [['code', 'STRING', n.caption], ['role', 'STRING', 'Assistenzarzt'], ['dept', 'STRING', 'Gastroenterologie']],
  }[n.label] || [];
  return base.map(([key, type, value]) => ({ key, type, value }));
}
const demoSchema = () => ([
  { id: 'labels', title: 'Labels', items: Object.keys(LABELS).map((k, i) => ({ id: 'lb:' + k, name: k, label: k, count: [182044, 911230, 64018, 120554, 1802113, 4211][i] })) },
  { id: 'rels', title: 'Relationship types', items: [
    ['HAD', 911230], ['HAS_DX', 402118], ['DESCRIBED_IN', 1204557], ['INCLUDED', 140221], ['ATTENDED_BY', 920014], ['MENTIONS', 3011290],
  ].map(([n, c]) => ({ id: 'rt:' + n, name: n, rel: true, count: c })) },
  { id: 'idx', title: 'Indexes', collapsed: true, items: [
    { id: 'ix:1', name: 'patient_mrn', meta: 'RANGE · :Patient(mrn)', state: 'ONLINE' },
    { id: 'ix:2', name: 'dx_icd10', meta: 'RANGE · :Diagnosis(icd10)', state: 'ONLINE' },
    { id: 'ix:3', name: 'doc_fulltext', meta: 'FULLTEXT · :Document(body)', state: 'POPULATING', pct: 63 },
    { id: 'ix:4', name: 'enc_start', meta: 'RANGE · :Encounter(start)', state: 'FAILED' },
  ] },
  { id: 'con', title: 'Constraints', collapsed: true, items: [
    { id: 'c:1', name: 'patient_mrn_unique', meta: 'UNIQUE · :Patient(mrn)' },
    { id: 'c:2', name: 'doc_sha_exists', meta: 'EXISTS · :Document(sha256)' },
  ] },
  { id: 'proc', title: 'Procedures', collapsed: true, items: [
    { id: 'p:1', name: 'iw.graph.expand', meta: 'READ' },
    { id: 'p:2', name: 'iw.storage.compact', meta: 'DBMS' },
  ] },
]);
const DEMO_QUERY = "// patients with a Crohn's diagnosis and their latest encounter\nMATCH (p:Patient)-[:HAS_DX]->(d:Diagnosis {icd10: 'K50.1'})\nMATCH (p)-[:HAD]->(e:Encounter)\nWHERE e.start >= date('2026-01-01')\nRETURN p, d, e ORDER BY e.start DESC LIMIT 50";
const demoPlan = () => ([
  { id: 1, depth: 0, op: 'ProduceResults', detail: 'p, d, e', est: 48, rows: 48, hits: 0, ms: 0.1 },
  { id: 2, depth: 1, op: 'Top', detail: 'e.start DESC LIMIT 50', est: 48, rows: 48, hits: 0, ms: 0.3 },
  { id: 3, depth: 2, op: 'Filter', detail: "e.start >= date('2026-01-01')", est: 610, rows: 212, hits: 424, ms: 0.6 },
  { id: 4, depth: 3, op: 'Expand(All)', detail: '(p)-[:HAD]->(e)', est: 610, rows: 1840, hits: 3682, ms: 1.1 },
  { id: 5, depth: 4, op: 'Expand(Into)', detail: '(p)-[:HAS_DX]->(d)', est: 212, rows: 233, hits: 9120, ms: 2.9 },
  { id: 6, depth: 5, op: 'NodeIndexSeek', detail: 'd:Diagnosis(icd10 = $autostring_0)', est: 230, rows: 233, hits: 234, ms: 0.2 },
  { id: 7, depth: 5, op: 'NodeByLabelScan', detail: 'p:Patient', est: 182044, rows: 182044, hits: 182045, ms: 8.4, hot: true, warn: 'Label scan — consider an index on :Patient' },
]);
const demoLog = () => ([
  { t: '09:41:02.118', level: 'INFO', msg: 'Bolt connection accepted · 10.4.2.17 · phil' },
  { t: '09:41:02.402', level: 'INFO', msg: 'tx 18 203 committed · 3 writes · 1.8 ms' },
  { t: '09:41:07.990', level: 'WARN', msg: 'Index doc_fulltext POPULATING · 63 % · ETA 4 min' },
  { t: '09:41:11.006', level: 'ERROR', msg: 'Index enc_start FAILED · value out of range on 4:2291' },
  { t: '09:41:15.551', level: 'INFO', msg: 'Checkpoint · 412 pages flushed · 22 ms' },
]);
function rowsFromGraph(g) {
  const enc = g.nodes.filter((n) => n.label === 'Encounter');
  const pts = [['4:1182', 'P-1182'], ['4:2093', 'P-2093'], ['4:0417', 'P-0417'], ['4:3310', 'P-3310'], ['4:5521', 'P-5521']];
  const out = enc.map((e, i) => ({ id: e.id, cells: [
    { node: { id: '4:1182', label: 'Patient', caption: 'P-1182' } }, { node: { id: '4:3051', label: 'Diagnosis', caption: 'K50.1' } },
    { node: e }, { v: '2026-09-' + (12 + i * 16) }, { v: String(4 - i) }] }));
  for (let i = 0; i < 12; i++) {
    const [pid, pc] = pts[(i + 1) % pts.length]; const day = String(28 - i * 2).padStart(2, '0'); const m = i < 9 ? '08' : '07';
    out.push({ id: '4:' + (2300 + i), cells: [
      { node: { id: pid, label: 'Patient', caption: pc } }, { node: { id: '4:' + (3060 + (i % 3)), label: 'Diagnosis', caption: 'K50.1' } },
      { node: { id: '4:' + (2300 + i), label: 'Encounter', caption: 'ENC 26-' + m + '-' + day } }, { v: '2026-' + m + '-' + day }, { v: String((i * 7) % 11 + 1) }] });
  }
  return out;
}
const DEMO_COLUMNS = [
  { key: 'p', name: 'p', type: 'NODE' }, { key: 'd', name: 'd', type: 'NODE' }, { key: 'e', name: 'e', type: 'NODE' },
  { key: 'start', name: 'e.start', type: 'DATE' }, { key: 'los', name: 'e.los_days', type: 'INTEGER', num: true },
];
/* console: register the labels a real graph uses ({Label: {shape, color}}), replacing the demo's. */
function setLabels(map) { Object.keys(LABELS).forEach((k) => delete LABELS[k]); Object.assign(LABELS, map || {}); }
const demo = { LABELS, graph: demoGraph, schema: demoSchema, query: DEMO_QUERY, plan: demoPlan, log: demoLog, rowsFromGraph, columns: DEMO_COLUMNS, propsFor };

/* ------------------------------------------------------------------ primitives */
function NodeShape({ label, shape, color, r = 13, className, ...rest }) {
  const meta = LABELS[label] || {};
  const sh = shape || meta.shape || 'circle';
  const fill = 'var(--' + (color || meta.color || 'lb-6') + ')';
  const p = { className: cx('iw-shape', className), style: { fill }, ...rest };
  if (sh === 'square') { const s = r * 0.88; return h('rect', { ...p, x: -s, y: -s, width: s * 2, height: s * 2 }); }
  if (sh === 'diamond') { const s = r * 1.15; return h('polygon', { ...p, points: `0,${-s} ${s},0 0,${s} ${-s},0` }); }
  return h('circle', { ...p, r });
}
function ShapeGlyph({ label, shape, color, size = 12 }) {
  return h('svg', { className: 'iw-shapeglyph', width: size, height: size, viewBox: '-8 -8 16 16', 'aria-hidden': true },
    h(NodeShape, { label, shape, color, r: 6 }));
}

/* Action glyphs: each performs its function on hover/focus of the owning control,
   or when `play` is true. Transitions only; reduced-motion jumps to the end state. */
function Glyph({ action, state = 'idle', play, size = 16, title }) {
  const common = { className: cx('iw-g', 'iw-g-' + action), width: size, height: size, viewBox: '0 0 16 16', 'data-state': state, 'data-play': play ? '1' : undefined, 'aria-hidden': title ? undefined : true, role: title ? 'img' : undefined };
  const kids = [];
  if (title) kids.push(h('title', { key: 't' }, title));
  switch (action) {
    case 'expand': {
      const A = [-90, 30, 150].map((a) => (a * Math.PI) / 180);
      A.forEach((a, i) => {
        kids.push(h('line', { key: 's' + i, className: 'spoke', x1: 8 + Math.cos(a) * 2.6, y1: 8 + Math.sin(a) * 2.6, x2: 8 + Math.cos(a) * 5, y2: 8 + Math.sin(a) * 5, pathLength: 1 }));
        kids.push(h('circle', { key: 'n' + i, className: 'sat', cx: 8 + Math.cos(a) * 6.2, cy: 8 + Math.sin(a) * 6.2, r: 1.5, style: { transitionDelay: i * 25 + 'ms' } }));
      });
      kids.push(h('circle', { key: 'c', className: 'core', cx: 8, cy: 8, r: 2.4 }));
      break;
    }
    case 'connect':
      kids.push(h('line', { key: 'l', className: 'wire', x1: 4, y1: 12, x2: 12, y2: 4, pathLength: 1 }));
      kids.push(h('circle', { key: 'a', className: 'end a', cx: 3.5, cy: 12.5, r: 1.9 }));
      kids.push(h('circle', { key: 'b', className: 'end b', cx: 12.5, cy: 3.5, r: 1.9 }));
      break;
    case 'detach':
      kids.push(h('line', { key: 'l1', className: 'half h1', x1: 4, y1: 12, x2: 8, y2: 8 }));
      kids.push(h('line', { key: 'l2', className: 'half h2', x1: 8, y1: 8, x2: 12, y2: 4 }));
      kids.push(h('circle', { key: 'a', className: 'end', cx: 3.5, cy: 12.5, r: 1.9 }));
      kids.push(h('circle', { key: 'b', className: 'end', cx: 12.5, cy: 3.5, r: 1.9 }));
      break;
    case 'filter':
      [4, 8, 12].forEach((y, i) => kids.push(h('line', { key: i, className: 'bar b' + i, x1: 2, y1: y, x2: 14, y2: y })));
      break;
    case 'focus':
      kids.push(h('path', { key: 'tl', className: 'brk tl', d: 'M2 5.5V2h3.5' }));
      kids.push(h('path', { key: 'tr', className: 'brk tr', d: 'M10.5 2H14v3.5' }));
      kids.push(h('path', { key: 'br', className: 'brk br', d: 'M14 10.5V14h-3.5' }));
      kids.push(h('path', { key: 'bl', className: 'brk bl', d: 'M5.5 14H2v-3.5' }));
      kids.push(h('circle', { key: 'c', className: 'core', cx: 8, cy: 8, r: 1.8 }));
      break;
    case 'run':
      kids.push(h('path', { key: 'tri', className: 'tri', d: 'M5 3.5L5 12.5L12.5 8Z' }));
      kids.push(h('path', { key: 'tr', className: 'trace', d: 'M5 3.5L5 12.5L12.5 8Z', pathLength: 1 }));
      kids.push(h('path', { key: 'ck', className: 'check', d: 'M3.5 8.5L6.6 11.5L12.5 4.5', pathLength: 1 }));
      break;
    case 'wander':
      kids.push(h('path', { key: 'p', className: 'path', d: 'M3 12.5C6.5 12.5 4.5 5 8.5 5.5S11 10 12.5 4', pathLength: 1 }));
      kids.push(h('circle', { key: 'a', className: 'from', cx: 3, cy: 12.5, r: 1.6 }));
      kids.push(h('circle', { key: 'b', className: 'to', cx: 12.5, cy: 4, r: 1.9 }));
      break;
    case 'hide':
      kids.push(h('circle', { key: 'c', className: 'ring', cx: 8, cy: 8, r: 5 }));
      kids.push(h('line', { key: 'x', className: 'slash', x1: 3, y1: 13, x2: 13, y2: 3, pathLength: 1 }));
      break;
    case 'save':
      kids.push(h('path', { key: 'b', className: 'box', d: 'M3 2.5h8l2 2v9H3z' }));
      kids.push(h('path', { key: 'k', className: 'check', d: 'M5.5 8.5l2 2 3.2-4', pathLength: 1 }));
      break;
    default:
      kids.push(h('circle', { key: 'c', cx: 8, cy: 8, r: 2 }));
  }
  return h('svg', common, kids);
}

function Key({ children, dim, hint }) { return h('kbd', { className: cx('iw-key', dim && 'is-dim', hint && 'is-hint') }, children); }

function Button({ variant = 'default', glyph, glyphState, play, kbd, showKey, children, active, className, iconOnly, title, ...rest }) {
  const tip = title || (typeof children === 'string' ? children.charAt(0) + children.slice(1).toLowerCase() : undefined);
  return h('button', { type: 'button', title: tip && kbd ? tip + '  ' + kbd : tip, className: cx('iw-btn', 'iw-btn--' + variant, active && 'is-active', iconOnly && 'is-icon', className), 'aria-pressed': active === undefined ? undefined : !!active, 'aria-keyshortcuts': kbd, ...rest },
    glyph && h(Glyph, { action: glyph, state: glyphState, play }),
    children != null && h('span', { className: 'iw-btn__cap' }, children),
    kbd && h(Key, { dim: variant !== 'primary', hint: !showKey }, kbd));
}

function Tag({ label, rel, count, children }) {
  return h('span', { className: cx('iw-tag', rel && 'is-rel') },
    label && h(ShapeGlyph, { label }),
    rel && h('span', { className: 'iw-tag__arrow', 'aria-hidden': true }, '→'),
    h('span', null, children || label),
    count != null && h('span', { className: 'iw-tag__n' }, fmt(count)));
}

function Led({ state = 'ok', activity, children }) {
  return h('span', { className: cx('iw-led', 'is-' + state, activity && 'is-busy') },
    h('span', { className: 'iw-led__dot', 'aria-hidden': true }),
    children && h('span', { className: 'iw-led__cap' }, children));
}

function Meter({ value, max = 1, warnAt = 0.85, caption, readout, width = 56 }) {
  const f = clamp(value / max, 0, 1);
  return h('span', { className: cx('iw-meter', f >= warnAt && 'is-warn'), role: 'meter', 'aria-valuenow': value, 'aria-valuemax': max, 'aria-label': caption },
    caption && h('span', { className: 'iw-cap' }, caption),
    h('svg', { width, height: 8, viewBox: `0 0 ${width} 8`, 'aria-hidden': true },
      [0, 0.25, 0.5, 0.75, 1].map((t) => h('line', { key: t, className: 'tick', x1: Math.round(t * (width - 1)) + 0.5, x2: Math.round(t * (width - 1)) + 0.5, y1: 0, y2: 2 })),
      h('rect', { className: 'track', x: 0, y: 3.5, width, height: 3 }),
      h('rect', { className: 'fill', x: 0, y: 3.5, width: f * width, height: 3 })),
    readout && h('span', { className: 'iw-mono-s' }, readout));
}

/* ------------------------------------------------------------------ SaveAck: mechanical counter */
function SaveAck({ tx, label = 'COMMITTED' }) {
  const prev = useRef(tx); const [stamp, setStamp] = useState(0);
  const old = String(prev.current).padStart(5, '0'); const now = String(tx).padStart(5, '0');
  useEffect(() => { if (prev.current !== tx) { setStamp((s) => s + 1); prev.current = tx; } }, [tx]);
  return h('span', { className: 'iw-ack', 'aria-live': 'polite' },
    h('span', { className: 'iw-cap' }, 'TX'),
    h('span', { className: 'iw-ack__drum' }, now.split('').map((d, i) => h('span', { key: i + ':' + d + ':' + (d !== old[i] ? stamp : 0), className: cx('iw-ack__digit', d !== old[i] && stamp && 'is-roll') }, d))),
    stamp > 0 && h('span', { key: stamp, className: 'iw-ack__stamp' }, h(Glyph, { action: 'save', play: true, size: 12 }), label));
}

/* ------------------------------------------------------------------ StatusRail */
/* console: `drawer` replaces the drawer's instruments, `footer` its right-hand note, `children` sit before the
   health word (page links), `onDb` opens the database (namespace) switcher, `word` overrides the health word. */
function StatusRail({ db = 'clinical', metrics, tx, activity, txOpen = 0, health = 'ok', open: openProp, onToggle, onCommand, onHelp, drawer, footer, children, onDb, word: wordProp }) {
  const m = metrics || { nodes: 2_884_174, rels: 6_589_430, heap: 2.1, heapMax: 8, cache: 98.2, p50: 2.4 };
  const [openS, setOpenS] = useState(false); const open = openProp !== undefined ? openProp : openS;
  const toggle = () => { onToggle ? onToggle(!open) : setOpenS(!open); };
  const [recent, setRecent] = useState(false); const first = useRef(true);
  useEffect(() => { if (first.current) { first.current = false; return; } setRecent(true); const t = setTimeout(() => setRecent(false), 3200); return () => clearTimeout(t); }, [tx]);
  const word = wordProp || (activity ? 'Writing' : health === 'ok' ? 'Healthy' : health === 'warn' ? 'Degraded' : 'Down');
  return h('header', { className: cx('iw-rail', open && 'is-open') },
    h('div', { className: 'iw-rail__row' },
      h('span', { className: 'iw-mark' }, h('b', null, 'IRONWEAVER'), h('span', null, '::DB')),
      h('button', { className: 'iw-rail__db', type: 'button', title: 'Switch database', onClick: onDb }, h('span', { className: 'iw-mono' }, db), h('span', { 'aria-hidden': true, className: 'iw-chev' }, '▾')),
      h('span', { className: 'iw-rail__fill' }),
      h('button', { type: 'button', className: 'iw-rail__go', onClick: onCommand, title: 'Go to a label, node, index or command' }, h('span', null, 'Go to anything'), h(Key, { dim: true }, '⌘K')),
      h('span', { className: 'iw-rail__fill' }),
      children,
      txOpen > 0 && h('span', { className: 'iw-rail__tx' }, h('span', { className: 'iw-cap' }, 'TX OPEN'), h('span', { className: 'iw-mono-s' }, txOpen)),
      tx != null && h('span', { className: cx('iw-rail__ack', recent && 'is-on'), 'aria-hidden': !recent }, h(SaveAck, { tx })),
      h('button', { type: 'button', className: 'iw-rail__health', 'aria-expanded': open, onClick: toggle, title: open ? 'Hide instruments' : 'Show instruments' }, h(Led, { state: health, activity }, word), h('span', { className: cx('iw-disc', open && 'is-open'), 'aria-hidden': true })),
      h('button', { type: 'button', className: 'iw-rail__help', onClick: onHelp, title: 'Keyboard shortcuts  ?', 'aria-label': 'Keyboard shortcuts' }, '?')),
    open && h('div', { className: 'iw-rail__drawer' }, drawer || [
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'NODES'), h('span', { className: 'iw-mono-s' }, fmt(m.nodes))),
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'RELS'), h('span', { className: 'iw-mono-s' }, fmt(m.rels))),
      h(Meter, { caption: 'HEAP', value: m.heap, max: m.heapMax, readout: m.heap.toFixed(1) + '/' + m.heapMax + ' GB' }),
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'CACHE HIT'), h('span', { className: 'iw-mono-s' }, m.cache.toFixed(1) + ' %')),
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'P50'), h('span', { className: 'iw-mono-s' }, m.p50.toFixed(1) + ' ms')),
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'LAST TX'), h('span', { className: 'iw-mono-s' }, tx != null ? fmt(tx) : '—'))].map((el, i) => h(Frag, { key: i }, el)),
      h('span', { className: 'iw-rail__fill' }),
      h('span', { className: 'iw-small iw-muted' }, footer != null ? footer : 'Bolt 10.4.2.17 · v0.9.3')));
}

/* ------------------------------------------------------------------ SchemaNavigator */
/* console: an item's `stateLabel` replaces the word its `state` shows (READY for ONLINE, say). */
function SchemaNavigator({ sections, selected, onSelect, onActivate }) {
  const data = useMemo(() => sections || demoSchema(), [sections]);
  const [open, setOpen] = useState(() => Object.fromEntries(data.map((s) => [s.id, !s.collapsed])));
  const [q, setQ] = useState('');
  const [cur, setCur] = useState(selected || null);
  const listRef = useRef(null); const inputRef = useRef(null);
  useEffect(() => { if (selected) setCur(selected); }, [selected]);
  const flat = useMemo(() => {
    const out = []; const ql = q.trim().toLowerCase();
    data.forEach((s) => {
      const items = s.items.filter((it) => !ql || it.name.toLowerCase().includes(ql));
      if (ql && !items.length) return;
      out.push({ kind: 'sec', id: s.id, s, n: s.items.length, shown: items.length });
      if (open[s.id] || ql) items.forEach((it) => out.push({ kind: 'item', id: it.id, it, sec: s.id }));
    });
    return out;
  }, [data, open, q]);
  const pick = (row) => { setCur(row.id); if (row.kind === 'item') onSelect && onSelect(row.it); };
  const onKey = (e) => {
    const i = flat.findIndex((r) => r.id === cur);
    if (e.key === 'ArrowDown') { e.preventDefault(); const r = flat[Math.min(flat.length - 1, i + 1)]; r && pick(r); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); const r = flat[Math.max(0, i - 1)]; r && pick(r); }
    else if (e.key === 'ArrowRight' && flat[i]?.kind === 'sec') setOpen((o) => ({ ...o, [cur]: true }));
    else if (e.key === 'ArrowLeft') { const r = flat[i]; if (!r) return; const sid = r.kind === 'sec' ? r.id : r.sec; setOpen((o) => ({ ...o, [sid]: false })); setCur(sid); }
    else if (e.key === 'Enter' && flat[i]?.kind === 'item') onActivate && onActivate(flat[i].it);
    else if (e.key === '/') { e.preventDefault(); inputRef.current && inputRef.current.focus(); }
  };
  return h('nav', { className: 'iw-nav', 'aria-label': 'Schema' },
    h('label', { className: 'iw-nav__filter' },
      h(Glyph, { action: 'filter', play: !!q, size: 14 }),
      h('input', { ref: inputRef, value: q, placeholder: 'Filter schema', onChange: (e) => setQ(e.target.value), onKeyDown: (e) => { if (e.key === 'Escape') { setQ(''); listRef.current && listRef.current.focus(); } if (e.key === 'ArrowDown') { e.preventDefault(); listRef.current && listRef.current.focus(); flat[0] && pick(flat[0]); } } }),
      h(Key, { dim: true, hint: true }, '/')),
    h('div', { className: 'iw-nav__list', role: 'tree', tabIndex: 0, ref: listRef, onKeyDown: onKey },
      flat.length === 0 && h('div', { className: 'iw-nav__none' }, 'Nothing called that. Check the spelling, then the schema.'),
      flat.map((r) => r.kind === 'sec'
        ? h('div', { key: r.id, role: 'treeitem', 'aria-expanded': !!open[r.id], className: cx('iw-nav__sec', cur === r.id && 'is-cur'), onClick: () => { setCur(r.id); setOpen((o) => ({ ...o, [r.id]: !o[r.id] })); } },
            h('span', { className: cx('iw-disc', open[r.id] && 'is-open'), 'aria-hidden': true }),
            h('span', { className: 'iw-cap' }, r.s.title.toUpperCase()),
            (() => { const f = r.s.items.filter((i) => i.state === 'FAILED').length, pp = r.s.items.filter((i) => i.state === 'POPULATING').length;
              return h('span', { className: 'iw-nav__issues' }, f > 0 && h('span', { className: 'iw-state is-failed', title: f + ' failed' }, '✕ ' + f), pp > 0 && h('span', { className: 'iw-state is-populating', title: pp + ' populating' }, '\u25B2 ' + pp)); })(),
            h('span', { className: 'iw-mono-s iw-nav__n' }, q ? r.shown + '/' + r.n : r.n))
        : h('div', { key: r.id, role: 'treeitem', 'aria-selected': cur === r.id, className: cx('iw-nav__row', cur === r.id && 'is-cur', r.it.active && 'is-active'), onClick: () => pick(r), onDoubleClick: () => onActivate && onActivate(r.it), draggable: !!r.it.label },
            r.it.label ? h(ShapeGlyph, { label: r.it.label })
              : r.it.rel ? h('span', { className: 'iw-nav__rel', 'aria-hidden': true }, '→')
              : r.it.glyph === 'db' ? h(Led, { state: r.it.active ? 'ok' : 'off' })
              : h('span', { className: 'iw-nav__dot', 'aria-hidden': true }),
            h('span', { className: cx('iw-nav__name', (r.it.rel || r.it.meta) && 'iw-mono') }, r.it.name),
            r.it.state ? h('span', { className: cx('iw-state', 'is-' + r.it.state.toLowerCase()) }, r.it.stateLabel || (r.it.state === 'POPULATING' ? r.it.pct + ' %' : r.it.state === 'FAILED' ? '✕ FAILED' : 'ONLINE'))
              : r.it.count != null ? h('span', { className: 'iw-mono-s iw-nav__n' }, fmt(r.it.count))
              : r.it.meta && !r.it.meta.includes('·') ? h('span', { className: 'iw-mono-s iw-nav__n' }, r.it.meta) : null))),
    h('div', { className: 'iw-nav__foot iw-hintrow' }, h(Key, { dim: true }, '↑↓'), ' move ', h(Key, { dim: true }, '↵'), ' open ', h(Key, { dim: true }, '←'), ' fold ', h(Key, { dim: true }, '/'), ' filter'));
}

/* ------------------------------------------------------------------ RadialMenu (Colani #3) */
const RADIAL_ITEMS = [
  { id: 'expand', label: 'Expand', key: 'E' },
  { id: 'focus', label: 'Focus', key: 'F' },
  { id: 'connect', label: 'Connect', key: 'C' },
  { id: 'filter', label: 'Same label', key: 'L' },
  { id: 'hide', label: 'Hide', key: 'H' },
];
/* A segment between boundary angles a0..a1 whose side edges are offset ±g from the boundary ray,
   so neighbouring segments face each other with exactly parallel edges and a constant 2g gap. */
function crescentPath(a0, a1, rIn, rOut, g) {
  const pt = (a, r, s) => [Math.cos(a) * r - Math.sin(a) * s, Math.sin(a) * r + Math.cos(a) * s];
  const along = (R, s) => Math.sqrt(Math.max(R * R - s * s, 0));
  const i0 = pt(a0, along(rIn, g), g), o0 = pt(a0, along(rOut(a0), g), g);
  const i1 = pt(a1, along(rIn, g), -g), o1 = pt(a1, along(rOut(a1), g), -g);
  const b0 = Math.atan2(o0[1], o0[0]), b1 = Math.atan2(o1[1], o1[0]);
  const pts = [o0]; const N = 10;
  for (let i = 1; i < N; i++) { const a = b0 + ((b1 - b0) * i) / N; pts.push([Math.cos(a) * rOut(a), Math.sin(a) * rOut(a)]); }
  pts.push(o1);
  const f = (p) => p[0].toFixed(2) + ' ' + p[1].toFixed(2);
  return 'M' + f(i0) + 'L' + pts.map(f).join('L') + 'L' + f(i1) + `A${rIn} ${rIn} 0 0 0 ${f(i0)}Z`;
}
function RadialMenu({ x = 0, y = 0, items = RADIAL_ITEMS, onPick, onClose, nodeRadius = 13, standalone }) {
  const [hot, setHot] = useState(0);
  const ref = useRef(null);
  const A0 = (-112 * Math.PI) / 180, A1 = (76 * Math.PI) / 180;            // asymmetric sweep
  const rIn = nodeRadius + 9;
  const rOut = (a) => { const t = (a - A0) / (A1 - A0); return rIn + 22 + 22 * Math.sin(Math.PI * Math.pow(t, 0.7)); }; // swells past the middle
  const half = 1.25; const step = (A1 - A0) / items.length;
  useEffect(() => { ref.current && ref.current.focus(); }, []);
  const onKey = (e) => {
    if (e.key === 'Escape') { e.stopPropagation(); onClose && onClose(); }
    else if (e.key === 'ArrowDown' || e.key === 'ArrowRight') { e.preventDefault(); setHot((i) => (i + 1) % items.length); }
    else if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') { e.preventDefault(); setHot((i) => (i + items.length - 1) % items.length); }
    else if (e.key === 'Enter') { e.preventDefault(); onPick && onPick(items[hot].id); }
    else { const it = items.find((i) => i.key.toLowerCase() === e.key.toLowerCase()); if (it) { e.preventDefault(); e.stopPropagation(); onPick && onPick(it.id); } }
  };
  const segs = items.map((it, i) => {
    const a0 = A0 + i * step, a1 = A0 + (i + 1) * step, am = (a0 + a1) / 2;
    const rm = (rIn + rOut(am)) / 2;
    return { it, i, d: crescentPath(a0, a1, rIn, rOut, half), gx: Math.cos(am) * rm, gy: Math.sin(am) * rm, am };
  });
  const hs = segs[hot]; const capR = rOut(hs.am) + 10;
  const body = h('g', { className: 'iw-radial', transform: `translate(${x},${y})`, tabIndex: 0, ref, onKeyDown: onKey, role: 'menu', 'aria-label': 'Node actions', onBlur: (e) => { if (!standalone && !e.currentTarget.contains(e.relatedTarget)) onClose && onClose(); } },
    h('circle', { className: 'iw-radial__hub', r: rIn - 3 }),
    segs.map((s) => h('g', { key: s.it.id, className: cx('iw-radial__seg', hot === s.i && 'is-hot'), style: { animationDelay: s.i * 18 + 'ms' }, role: 'menuitem', 'aria-label': s.it.label, onPointerEnter: () => setHot(s.i), onPointerDown: (e) => e.stopPropagation(), onClick: (e) => { e.stopPropagation(); onPick && onPick(s.it.id); } },
      h('path', { d: s.d }),
      h('g', { transform: `translate(${(s.gx - 8).toFixed(1)},${(s.gy - 8).toFixed(1)})` }, h(Glyph, { action: s.it.id === 'filter' ? 'filter' : s.it.id, play: hot === s.i })))),
    h('g', { className: 'iw-radial__cap', transform: `translate(${(Math.cos(hs.am) * capR).toFixed(1)},${(Math.sin(hs.am) * capR).toFixed(1)})` },
      h('text', { className: 'iw-radial__label', x: 0, y: 4, textAnchor: Math.cos(hs.am) < -0.2 ? 'end' : 'start' }, hs.it.label.toUpperCase() + '  ' + hs.it.key)));
  if (!standalone) return body;
  return h('svg', { className: 'iw-radial-demo', width: 220, height: 200, viewBox: '-80 -96 220 200' }, h(NodeShape, { label: 'Patient', className: 'is-sel' }), body);
}

/* ------------------------------------------------------------------ NavPuck (Colani #1: asymmetric housing + aerodynamic minimap) */
const PUCK_PATH = 'M50 4C20 4 4 24 4 48C4 73 21 92 51 92C95 92 140 84 164 68C178 59 178 39 164 30C140 13 95 4 50 4Z';
function NavPuck({ nodes = [], view = { x: 0, y: 0, k: 1 }, size = { w: 800, h: 500 }, onPan, onZoom, onFit }) {
  const MM = { x: 12, y: 12, w: 80, h: 72 };
  const b = useMemo(() => {
    if (!nodes.length) return { x0: -100, y0: -100, x1: 100, y1: 100 };
    const xs = nodes.map((n) => n.x), ys = nodes.map((n) => n.y);
    return { x0: Math.min(...xs) - 80, y0: Math.min(...ys) - 80, x1: Math.max(...xs) + 80, y1: Math.max(...ys) + 80 };
  }, [nodes]);
  const vp = { x: -view.x / view.k, y: -view.y / view.k, w: size.w / view.k, h: size.h / view.k };
  const ux0 = Math.min(b.x0, vp.x), uy0 = Math.min(b.y0, vp.y), ux1 = Math.max(b.x1, vp.x + vp.w), uy1 = Math.max(b.y1, vp.y + vp.h);
  const s = Math.min(MM.w / (ux1 - ux0), MM.h / (uy1 - uy0));
  const ox = MM.x + (MM.w - (ux1 - ux0) * s) / 2 - ux0 * s, oy = MM.y + (MM.h - (uy1 - uy0) * s) / 2 - uy0 * s;
  const drag = useRef(null);
  const toWorld = (e) => { const r = e.currentTarget.ownerSVGElement.getBoundingClientRect(); const px = ((e.clientX - r.left) / r.width) * 176, py = ((e.clientY - r.top) / r.height) * 96; return { x: (px - ox) / s, y: (py - oy) / s }; };
  const center = (e) => { const w = toWorld(e); onPan && onPan({ x: size.w / 2 - w.x * view.k, y: size.h / 2 - w.y * view.k }); };
  return h('div', { className: 'iw-puck' },
    h('svg', { width: 176, height: 96, viewBox: '0 0 176 96', role: 'group', 'aria-label': 'Navigation and minimap' },
      h('defs', null, h('clipPath', { id: 'iw-puck-clip' }, h('path', { d: PUCK_PATH }))),
      h('path', { className: 'iw-puck__body', d: PUCK_PATH }),
      h('g', { clipPath: 'url(#iw-puck-clip)' },
        h('rect', { className: 'iw-puck__map', x: 0, y: 0, width: 100, height: 96, onPointerDown: (e) => { drag.current = 1; e.currentTarget.setPointerCapture(e.pointerId); center(e); }, onPointerMove: (e) => drag.current && center(e), onPointerUp: () => (drag.current = null) }),
        nodes.map((n) => h('circle', { key: n.id, className: 'iw-puck__dot', cx: n.x * s + ox, cy: n.y * s + oy, r: 1.6 })),
        h('rect', { className: 'iw-puck__vp', x: vp.x * s + ox, y: vp.y * s + oy, width: vp.w * s, height: vp.h * s })),
      h('path', { className: 'iw-puck__seam', d: 'M100 9C104 30 104 66 100 88' }),
      h('text', { className: 'iw-puck__zoom', x: 138, y: 34, textAnchor: 'middle' }, Math.round(view.k * 100) + '%'),
      [['−', 112, () => onZoom && onZoom(1 / 1.25), 'Zoom out'], ['+', 136, () => onZoom && onZoom(1.25), 'Zoom in'], ['fit', 160, () => onFit && onFit(), 'Fit graph']].map(([t, x, fn, lab]) =>
        h('g', { key: t, className: 'iw-puck__btn', transform: `translate(${x},56)`, onClick: fn, role: 'button', 'aria-label': lab, tabIndex: 0, onKeyDown: (e) => (e.key === 'Enter' || e.key === ' ') && fn() },
          h('circle', { r: t === 'fit' ? 8 : 9 }),
          t === 'fit' ? h('path', { className: 'iw-puck__fit', d: 'M-4.5 -1.5V-4.5H-1.5M1.5 -4.5H4.5V-1.5M4.5 1.5V4.5H1.5M-1.5 4.5H-4.5V1.5' }) : h('text', { y: 4, textAnchor: 'middle' }, t)))));
}

/* ------------------------------------------------------------------ GraphCanvas */
function radiusOf(label) { const s = (LABELS[label] || {}).shape; return s === 'diamond' ? 15 : s === 'square' ? 12 : 13; }
function settle(nodes, fresh, anchor) {
  const P = new Map(nodes.map((n) => [n.id, { x: n.x, y: n.y }]));
  const O = new Map(nodes.map((n) => [n.id, { x: n.x, y: n.y }]));
  const ids = nodes.map((n) => n.id); const MIN = 70;
  for (let it = 0; it < 36; it++) {
    for (let i = 0; i < ids.length; i++) for (let j = i + 1; j < ids.length; j++) {
      const a = P.get(ids[i]), b = P.get(ids[j]); let dx = b.x - a.x, dy = b.y - a.y; let d = Math.hypot(dx, dy) || 0.01;
      if (d >= MIN) continue; const push = (MIN - d) / 2; dx /= d; dy /= d;
      const wa = ids[i] === anchor ? 0 : fresh.has(ids[i]) ? 1 : 0.2, wb = ids[j] === anchor ? 0 : fresh.has(ids[j]) ? 1 : 0.2;
      a.x -= dx * push * wa; a.y -= dy * push * wa; b.x += dx * push * wb; b.y += dy * push * wb;
    }
  }
  O.forEach((o, id) => { if (fresh.has(id)) return; const p = P.get(id); const dx = p.x - o.x, dy = p.y - o.y, d = Math.hypot(dx, dy); if (d > 12) { p.x = o.x + (dx / d) * 12; p.y = o.y + (dy / d) * 12; } });
  return P;
}
/* console: `neighbours(id) -> Promise<{nodes:[{id,label,caption}], edges:[{id,s,t,type}], truncated?}>` makes
   peek and expand show the real neighbourhood instead of the demo's; `onConnect(edge)` and `onDetach(edge)`
   report the staged change, `connectType` names a connected edge's type. */
function GraphCanvas({ graph, selected, onSelect, onLog, hiddenLabels, onExpandRef, empty, className, neighbours, onConnect, onDetach, connectType = 'RELATES_TO' }) {
  const wrap = useRef(null); const size = useSize(wrap);
  const init = useMemo(() => graph || demoGraph(), []);
  const [nodes, setNodes] = useState(init.nodes);
  const [edges, setEdges] = useState(init.edges);
  const [sel, setSel] = useState(selected !== undefined ? selected : init.nodes[0] && init.nodes[0].id);
  const [selEdge, setSelEdge] = useState(null);
  const [view, setView] = useState({ x: 0, y: 0, k: 1 });
  const [menu, setMenu] = useState(null);
  const [mode, setMode] = useState(null);       // 'connect'
  const [pointer, setPointer] = useState(null);
  const [pulse, setPulse] = useState(null);
  const [focusKey, setFocusKey] = useState(0);
  const [breaking, setBreaking] = useState(null);
  const [fresh, setFresh] = useState({ n: new Set(), e: new Set() });
  const [filterOpen, setFilterOpen] = useState(false);
  const [dim, setDim] = useState(new Set(hiddenLabels || []));
  const [hint, setHint] = useState(null);
  const [moving, setMoving] = useState(false); const movingT = useRef(null);
  const nudge = () => { setMoving(true); clearTimeout(movingT.current); movingT.current = setTimeout(() => setMoving(false), 450); };
  const placed = useRef(false); const drag = useRef(null); const lastTap = useRef({ id: null, t: 0 });
  const hist = useRef([]); const [histN, setHistN] = useState(0);
  const [peek, setPeek] = useState(null); const peekT = useRef(null);
  const [trail, setTrail] = useState([]); const [intro, setIntro] = useState(true);
  const edgesRef = useRef(edges); edgesRef.current = edges; const stopTween = useRef(null);
  const nodesRef = useRef(nodes); nodesRef.current = nodes;
  /* console: neighbourhoods by node id, fetched once per node ({p: promise, r: result}) */
  const nb = useRef(new Map()); const [, setNbTick] = useState(0); const expandLatest = useRef(null);
  useEffect(() => { nb.current.clear(); }, [neighbours]);
  const fetchNb = (id) => {
    let c = nb.current.get(id);
    if (!c) {
      c = { r: null };
      c.p = Promise.resolve().then(() => neighbours(id)).then(
        (r) => { c.r = r || { nodes: [], edges: [] }; setNbTick((t) => t + 1); },
        (err) => { nb.current.delete(id); log('ERROR', 'neighbours of ' + id + ' · ' + ((err && err.message) || err)); });
      nb.current.set(id, c);
    }
    return c.p;
  };
  const nbOf = (id) => { const c = nb.current.get(id); return c ? c.r : null; };
  useEffect(() => { if (selected !== undefined) setSel(selected); }, [selected]);
  useEffect(() => { if (!placed.current && size.w) { placed.current = true; setView({ x: size.w / 2 - 40, y: size.h / 2, k: 1 }); } }, [size.w]);
  const byId = useMemo(() => new Map(nodes.map((n) => [n.id, n])), [nodes]);
  const select = (id) => { setSel(id); setSelEdge(null); setFocusKey((k) => k + 1); if (id) { setIntro(false); setTrail((t) => t.filter((x) => x !== id).concat(id).slice(-6)); } onSelect && onSelect(id ? byId.get(id) : null); };
  const say = (t) => { setHint(t); clearTimeout(say.t); say.t = setTimeout(() => setHint(null), 2200); };
  const log = (level, msg) => onLog && onLog(level, msg);

  const animateTo = (target, ms = 250) => {
    stopTween.current && stopTween.current();
    const from = new Map(nodesRef.current.map((n) => [n.id, { x: n.x, y: n.y }]));
    stopTween.current = tween(ms, (p) => setNodes((ns) => ns.map((n) => { const a = from.get(n.id), b = target.get(n.id); return a && b ? { ...n, x: a.x + (b.x - a.x) * p, y: a.y + (b.y - a.y) * p } : n; })));
  };
  /* Where would expansion put new neighbours? Inside the widest free angular gap, so the mental map stays intact. */
  const planExpand = (id) => {
    const n = byId.get(id); if (!n) return null;
    const inc = edges.filter((e) => e.s === id || e.t === id);
    const linked = inc.map((e) => byId.get(e.s === id ? e.t : e.s)).filter(Boolean);
    let todo, extra = null;
    if (neighbours) {
      /* console: the neighbours not on the canvas yet, and the edges it lacks */
      const r = nbOf(id); if (!r) return null;
      const have = new Set(nodes.map((m) => m.id)); const haveE = new Set(edges.map((e) => e.id));
      const typeOf = (m) => { const e = r.edges.find((x) => (x.s === id && x.t === m.id) || (x.t === id && x.s === m.id)); return e ? e.type : ''; };
      todo = r.nodes.filter((m) => !have.has(m.id)).map((m) => ({ lab: m.label, type: typeOf(m), node: m }));
      extra = r.edges.filter((e) => !haveE.has(e.id));
    } else {
      const spawn = SPAWN[n.label] || [];
      const existing = new Set(inc.map((e) => e.type + ':' + (byId.get(e.s === id ? e.t : e.s) || {}).label));
      todo = (n.expanded ? [] : spawn.filter(([lab, type]) => !existing.has(type + ':' + lab) || n.label === 'Patient')).map(([lab, type]) => ({ lab, type }));
    }
    if (!todo.length) return { n, items: [], edges: extra };
    const angs = linked.map((o) => Math.atan2(o.y - n.y, o.x - n.x)).sort((a, b) => a - b);
    let g0 = -Math.PI / 2 - Math.PI, gs = Math.PI * 2;
    if (angs.length) { gs = -1; angs.forEach((a, i) => { const b = i + 1 < angs.length ? angs[i + 1] : angs[0] + Math.PI * 2; if (b - a > gs) { gs = b - a; g0 = a; } }); }
    const k = todo.length; const stepA = gs / (k + 1); const rad = clamp(64 / stepA, 96, 200);
    return { n, edges: extra, items: todo.map((t, i) => { const a = angs.length ? g0 + stepA * (i + 1) : g0 + (gs * (i + 0.5)) / k; return { ...t, i, x: n.x + Math.cos(a) * rad, y: n.y + Math.sin(a) * rad }; }) };
  };
  const snap = () => { hist.current.push({ nodes: nodesRef.current, edges: edgesRef.current }); if (hist.current.length > 40) hist.current.shift(); setHistN(hist.current.length); };
  const undo = () => {
    const last = hist.current.pop(); setHistN(hist.current.length); if (!last) { say('Nothing to undo. You are at the beginning.'); return; }
    stopTween.current && stopTween.current(); setNodes(last.nodes); setEdges(last.edges); setPeek(null);
    if (sel && !last.nodes.some((n) => n.id === sel)) select(null);
    log('INFO', 'undo · view restored');
  };
  const expand = (id) => {
    if (neighbours && byId.has(id) && !nbOf(id)) { fetchNb(id).then(() => nbOf(id) && expandLatest.current(id)); return; }
    const plan = planExpand(id); if (!plan) return;
    const n = plan.n; setPeek(null); setIntro(false);
    setPulse({ id, k: Date.now() });
    if (!plan.items.length && !(plan.edges && plan.edges.some((e) => byId.has(e.s) && byId.has(e.t)))) {
      setNodes((ns) => ns.map((x) => (x.id === id ? { ...x, expanded: true } : x)));
      say('Nothing further. ' + n.caption + ' is a leaf, or very private.'); return;
    }
    snap();
    let newN = [], newE = [];
    if (plan.edges) {
      newN = plan.items.map((it) => ({ ...it.node, x: it.x, y: it.y, i: it.i }));
      const ids = new Set(nodes.map((m) => m.id).concat(newN.map((m) => m.id)));
      newE = plan.edges.filter((e) => ids.has(e.s) && ids.has(e.t));
      const r = nbOf(id); if (r && r.truncated) log('WARN', `expand ${id} · neighbourhood truncated at ${r.nodes.length} nodes`);
    } else plan.items.forEach((it) => {
      const nid = '4:' + (++SEQ); const pool = CAPS[it.lab] || ['—'];
      newN.push({ id: nid, label: it.lab, caption: pool[SEQ % pool.length], x: it.x, y: it.y, i: it.i });
      newE.push({ id: 'r:' + SEQ, s: id, t: nid, type: it.type });
    });
    setTimeout(() => {
      setFresh({ n: new Set(newN.map((x) => x.id)), e: new Set(newE.map((x) => x.id)) });
      setEdges((es) => es.concat(newE));
      setNodes((ns) => ns.map((x) => (x.id === id ? { ...x, expanded: true } : x)).concat(newN));
      log('INFO', `expand ${id} · +${newN.length} nodes · ${[...new Set(newE.map((e) => e.type))].join(', ')}`);
      setTimeout(() => { const all = nodesRef.current; animateTo(settle(all, new Set(newN.map((x) => x.id)), id), 250); }, reduced() ? 0 : 340);
    }, reduced() ? 0 : 60);
  };
  expandLatest.current = expand;
  /* Wander: from the selection, step somewhere unexplored. Expands a closed node; otherwise hops to an unexpanded neighbour. */
  const wander = () => {
    const pickRand = (a) => a[Math.floor(Math.random() * a.length)];
    const cur = sel && byId.get(sel);
    if (cur && !cur.expanded && (neighbours || (planExpand(cur.id) || { items: [] }).items.length)) { expand(cur.id); return; }
    const near = cur ? edges.filter((e) => e.s === cur.id || e.t === cur.id).map((e) => byId.get(e.s === cur.id ? e.t : e.s)).filter((m) => m && !m.expanded) : [];
    const pool = near.length ? near : nodes.filter((m) => !m.expanded && m.id !== sel);
    if (!pool.length) { say('Everything in view is explored. Run a query to go further.'); return; }
    const m = pickRand(pool); focusOn(m.id); setPulse({ id: m.id, k: Date.now() });
  };
  if (onExpandRef) onExpandRef.current = expand;
  const focusOn = (id) => {
    const n = byId.get(id); if (!n || !size.w) return; select(id);
    const from = { ...view }; const to = { k: Math.max(view.k, 1), x: 0, y: 0 }; to.x = size.w / 2 - n.x * to.k; to.y = size.h / 2 - n.y * to.k;
    tween(220, (p) => setView({ x: from.x + (to.x - from.x) * p, y: from.y + (to.y - from.y) * p, k: from.k + (to.k - from.k) * p }));
  };
  const fit = () => {
    if (!nodes.length || !size.w) return;
    const xs = nodes.map((n) => n.x), ys = nodes.map((n) => n.y);
    const x0 = Math.min(...xs) - 60, x1 = Math.max(...xs) + 60, y0 = Math.min(...ys) - 60, y1 = Math.max(...ys) + 60;
    const k = clamp(Math.min(size.w / (x1 - x0), size.h / (y1 - y0)), 0.3, 1.6); const from = { ...view };
    const to = { k, x: size.w / 2 - ((x0 + x1) / 2) * k, y: size.h / 2 - ((y0 + y1) / 2) * k };
    tween(220, (p) => setView({ x: from.x + (to.x - from.x) * p, y: from.y + (to.y - from.y) * p, k: from.k + (to.k - from.k) * p }));
  };
  const zoomBy = (f, cx0 = size.w / 2, cy0 = size.h / 2) => setView((v) => { const k = clamp(v.k * f, 0.3, 3); return { k, x: cx0 - ((cx0 - v.x) * k) / v.k, y: cy0 - ((cy0 - v.y) * k) / v.k }; });
  const hide = (id) => { snap(); setNodes((ns) => ns.filter((n) => n.id !== id)); setEdges((es) => es.filter((e) => e.s !== id && e.t !== id)); if (sel === id) select(null); log('INFO', `hide ${id} · view only, nothing deleted`); };
  const detach = (eid) => {
    const e = edges.find((x) => x.id === eid); if (!e) return;
    snap(); setBreaking(eid); setSelEdge(null);
    setTimeout(() => { setEdges((es) => es.filter((x) => x.id !== eid)); setBreaking(null); log('WARN', `detach ${e.s} -[:${e.type}]-> ${e.t} · staged in tx`); onDetach && onDetach(e); }, reduced() ? 0 : 180);
  };
  const connect = (a, b) => {
    if (a === b) return; snap(); const id = 'r:' + (++SEQ); const edge = { id, s: a, t: b, type: connectType };
    setFresh({ n: new Set(), e: new Set([id]) });
    setEdges((es) => es.concat(edge)); setMode(null);
    log('INFO', `connect ${a} -[:${connectType}]-> ${b} · staged in tx`); onConnect && onConnect(edge);
  };
  const pick = (act) => {
    const id = menu && menu.id; setMenu(null); if (!id) return;
    if (act === 'expand') expand(id); else if (act === 'focus') focusOn(id); else if (act === 'hide') hide(id);
    else if (act === 'connect') { select(id); setMode('connect'); say('Connect: pick the target node. Esc cancels.'); }
    else if (act === 'filter') { const lab = byId.get(id).label; setDim(new Set(Object.keys(LABELS).filter((l) => l !== lab))); setFilterOpen(true); }
  };
  const toWorld = (e) => { const r = wrap.current.getBoundingClientRect(); return { x: (e.clientX - r.left - view.x) / view.k, y: (e.clientY - r.top - view.y) / view.k }; };
  const onBgDown = (e) => { if (e.button !== 0) return; setMenu(null); drag.current = { kind: 'pan', sx: e.clientX, sy: e.clientY, v: { ...view }, moved: false }; e.currentTarget.setPointerCapture(e.pointerId); };
  const onNodeDown = (e, n) => { if (e.button !== 0) return; e.stopPropagation(); setMenu(null); drag.current = { kind: 'node', id: n.id, sx: e.clientX, sy: e.clientY, ox: n.x, oy: n.y, moved: false }; wrap.current.querySelector('svg').setPointerCapture(e.pointerId); };
  const onMove = (e) => {
    if (mode === 'connect') setPointer(toWorld(e));
    const d = drag.current; if (!d) return; const dx = e.clientX - d.sx, dy = e.clientY - d.sy; if (Math.abs(dx) + Math.abs(dy) > 3) d.moved = true;
    if (d.moved) nudge();
    if (d.kind === 'pan') setView({ ...d.v, x: d.v.x + dx, y: d.v.y + dy });
    else if (d.moved) setNodes((ns) => ns.map((n) => (n.id === d.id ? { ...n, x: d.ox + dx / view.k, y: d.oy + dy / view.k } : n)));
  };
  const onUp = () => {
    const d = drag.current; drag.current = null; if (!d) return;
    if (d.kind === 'node' && !d.moved) { const t = performance.now(); const dbl = lastTap.current.id === d.id && t - lastTap.current.t < 320; lastTap.current = { id: d.id, t: dbl ? 0 : t }; if (mode === 'connect' && sel) connect(sel, d.id); else if (dbl) expand(d.id); else select(d.id); }
    if (d.kind === 'pan' && !d.moved) { if (mode) { setMode(null); } else select(null); }
  };
  const onWheel = (e) => { e.preventDefault(); nudge(); const r = wrap.current.getBoundingClientRect(); zoomBy(Math.exp(-e.deltaY * 0.0015), e.clientX - r.left, e.clientY - r.top); };
  useEffect(() => { const el = wrap.current; el.addEventListener('wheel', onWheel, { passive: false }); return () => el.removeEventListener('wheel', onWheel); });
  const onKey = (e) => {
    if (menu) return;
    const k = e.key;
    if (k === 'Escape') { if (mode) setMode(null); else if (filterOpen) setFilterOpen(false); else select(null); }
    else if ((e.metaKey || e.ctrlKey) && k.toLowerCase() === 'z') undo();
    else if (k === 'w') wander();
    else if (k === 'e' && sel) expand(sel);
    else if (k === 'f' && sel) focusOn(sel);
    else if (k === '0') fit();
    else if (k === 'h' && sel) hide(sel);
    else if (k === 'c' && sel) { setMode('connect'); say('Connect: pick the target node. Esc cancels.'); }
    else if ((k === 'Delete' || k === 'Backspace') && selEdge) detach(selEdge);
    else if ((k === '.' || (k === 'F10' && e.shiftKey)) && sel) { const n = byId.get(sel); setMenu({ id: sel, x: n.x, y: n.y }); }
    else if (k === '+' || k === '=') zoomBy(1.25); else if (k === '-') zoomBy(0.8);
    else if (k.startsWith('Arrow') && sel) {
      e.preventDefault(); const n = byId.get(sel); const dir = { ArrowRight: 0, ArrowDown: 90, ArrowLeft: 180, ArrowUp: -90 }[k] * Math.PI / 180;
      let best = null, bd = Infinity;
      nodes.forEach((m) => { if (m.id === sel) return; const dx = m.x - n.x, dy = m.y - n.y, d = Math.hypot(dx, dy); let da = Math.abs(Math.atan2(dy, dx) - dir); da = Math.min(da, 2 * Math.PI - da); if (da < Math.PI / 3 && d * (1 + da) < bd) { bd = d * (1 + da); best = m.id; } });
      best && select(best);
    } else return;
    e.preventDefault();
  };
  const selNode = byId.get(sel);
  const showEmpty = empty || nodes.length === 0;
  return h('div', { className: cx('iw-canvas', mode && 'is-' + mode, moving && 'is-moving', className), ref: wrap, tabIndex: 0, onKeyDown: onKey, 'aria-label': 'Graph canvas', role: 'application' },
    h('svg', { className: 'iw-canvas__svg', width: '100%', height: '100%', onPointerDown: onBgDown, onPointerMove: onMove, onPointerUp: onUp, onContextMenu: (e) => e.preventDefault() },
      h('defs', null, h('pattern', { id: 'iw-grid', width: 24 * view.k, height: 24 * view.k, patternUnits: 'userSpaceOnUse', x: view.x, y: view.y }, h('circle', { className: 'iw-grid-dot', cx: 1, cy: 1, r: 1 }))),
      h('rect', { className: 'iw-grid', width: '100%', height: '100%', fill: 'url(#iw-grid)' }),
      !showEmpty && h('g', { transform: `translate(${view.x},${view.y}) scale(${view.k})` },
        edges.map((e) => {
          const a = byId.get(e.s), b = byId.get(e.t); if (!a || !b) return null;
          const dx = b.x - a.x, dy = b.y - a.y, d = Math.hypot(dx, dy) || 1, ux = dx / d, uy = dy / d;
          const ra = radiusOf(a.label) + 2, rb = radiusOf(b.label) + 3;
          const x1 = a.x + ux * ra, y1 = a.y + uy * ra, x2 = b.x - ux * rb, y2 = b.y - uy * rb;
          const incident = sel && (e.s === sel || e.t === sel); const isSel = selEdge === e.id; const isFresh = fresh.e.has(e.id);
          const dimmed = dim.size && (dim.has(a.label) || dim.has(b.label));
          const mxp = (x1 + x2) / 2, myp = (y1 + y2) / 2; const ang = (Math.atan2(dy, dx) * 180) / Math.PI; const flip = ang > 90 || ang < -90;
          if (breaking === e.id) return h('g', { key: e.id, className: 'iw-edge is-breaking' },
            h('line', { className: 'iw-edge__half h1', x1, y1, x2: mxp, y2: myp, style: { '--bx': (-ux * 5).toFixed(1) + 'px', '--by': (-uy * 5).toFixed(1) + 'px' } }),
            h('line', { className: 'iw-edge__half h2', x1: mxp, y1: myp, x2, y2, style: { '--bx': (ux * 5).toFixed(1) + 'px', '--by': (uy * 5).toFixed(1) + 'px' } }));
          return h('g', { key: e.id, className: cx('iw-edge', incident && 'is-inc', isSel && 'is-sel', isFresh && 'is-fresh', dimmed && 'is-dim'), onPointerDown: (ev) => { ev.stopPropagation(); setSelEdge(e.id); setSel(null); } },
            h('line', { className: 'iw-edge__hit', x1, y1, x2, y2 }),
            h('line', { className: 'iw-edge__line', x1, y1, x2, y2, pathLength: 1 }),
            h('path', { className: 'iw-edge__arrow', d: `M0 0L-6 -3L-6 3Z`, transform: `translate(${x2},${y2}) rotate(${ang})` }),
            ((incident && d > 88) || isSel) && h('text', { className: 'iw-edge__type', transform: `translate(${mxp},${myp}) rotate(${flip ? ang + 180 : ang})`, y: -4, textAnchor: 'middle' }, e.type));
        }),
        peek && !drag.current && (() => { const p = planExpand(peek); if (!p || !p.items.length) return null;
          return h('g', { key: 'ghost:' + peek, className: 'iw-ghosts', onPointerEnter: () => clearTimeout(peekT.current), onPointerLeave: () => { peekT.current = setTimeout(() => setPeek(null), 160); }, onPointerDown: (e) => { e.stopPropagation(); }, onClick: (e) => { e.stopPropagation(); expand(peek); } },
            p.items.map((it) => h('g', { key: it.i, style: { '--i': it.i } },
              h('line', { className: 'iw-ghost__hit', x1: p.n.x, y1: p.n.y, x2: it.x, y2: it.y }),
              h('circle', { className: 'iw-ghost__hit', cx: it.x, cy: it.y, r: 26 }),
              h('line', { className: 'iw-ghost__edge', x1: p.n.x, y1: p.n.y, x2: it.x, y2: it.y }),
              h('g', { className: 'iw-ghost', transform: `translate(${it.x},${it.y})` }, h(NodeShape, { label: it.lab, r: radiusOf(it.lab) * 0.8 }), h('text', { className: 'iw-ghost__cap', y: radiusOf(it.lab) + 13, textAnchor: 'middle' }, it.lab)))),
            h('text', { className: 'iw-ghost__hint', x: p.n.x, y: p.n.y - radiusOf(p.n.label) - 10, textAnchor: 'middle' }, '+' + p.items.length + ' · click to bring them in')); })(),
        mode === 'connect' && selNode && pointer && h('line', { className: 'iw-rubber', x1: selNode.x, y1: selNode.y, x2: pointer.x, y2: pointer.y }),
        nodes.map((n) => {
          const r = radiusOf(n.label); const isSel = sel === n.id; const isFresh = fresh.n.has(n.id); const dimmed = dim.size && dim.has(n.label);
          return h('g', { key: n.id, className: cx('iw-node', isSel && 'is-sel', isFresh && 'is-fresh', dimmed && 'is-dim', n.expanded && 'is-expanded'), transform: `translate(${n.x},${n.y})`, style: isFresh ? { '--d': (200 + (n.i || 0) * 30) + 'ms' } : undefined,
            onPointerEnter: () => { clearTimeout(peekT.current); if (!n.expanded && !drag.current) peekT.current = setTimeout(() => { if (neighbours) fetchNb(n.id); setPeek(n.id); }, 280); },
            onPointerLeave: () => { clearTimeout(peekT.current); peekT.current = setTimeout(() => setPeek(null), 160); },
            onPointerDown: (e) => onNodeDown(e, n), onContextMenu: (e) => { e.preventDefault(); e.stopPropagation(); select(n.id); setMenu({ id: n.id, x: n.x, y: n.y }); } },
            pulse && pulse.id === n.id && h('circle', { key: pulse.k, className: 'iw-pulse', r: r + 2 }),
            h('g', { className: 'iw-node__body' }, h(NodeShape, { label: n.label, r })),
            !n.expanded && !isFresh && h('circle', { className: 'iw-node__more', cx: r * 0.8, cy: -r * 0.8, r: 2.2 }),
            isSel && h('g', { key: 'f' + focusKey, className: 'iw-brackets' }, [[-1, -1], [1, -1], [1, 1], [-1, 1]].map(([sx, sy], i) => { const o = r + 7, l = 5; return h('path', { key: i, style: { '--fx': sx * 6 + 'px', '--fy': sy * 6 + 'px' }, d: `M${sx * o} ${sy * (o - l)}V${sy * o}H${sx * (o - l)}` }); })),
            h('text', { className: 'iw-node__cap', y: r + 15, textAnchor: 'middle' }, n.caption));
        }),
        menu && h(RadialMenu, { x: menu.x, y: menu.y, nodeRadius: radiusOf((byId.get(menu.id) || {}).label), onPick: pick, onClose: () => { setMenu(null); wrap.current && wrap.current.focus(); } }))),
    showEmpty && h(EmptyState, { kind: 'canvas' }),
    selNode && !menu && !drag.current && peek !== sel && h('div', { key: 'sb:' + sel, className: 'iw-selbar', role: 'toolbar', 'aria-label': 'Node actions', style: { left: selNode.x * view.k + view.x, top: selNode.y * view.k + view.y - radiusOf(selNode.label) * view.k - 22 } },
      h(Button, { variant: 'ghost', iconOnly: true, glyph: 'expand', kbd: 'E', title: 'Expand', onClick: () => expand(sel) }),
      h(Button, { variant: 'ghost', iconOnly: true, glyph: 'focus', kbd: 'F', title: 'Focus', onClick: () => focusOn(sel) }),
      h(Button, { variant: 'ghost', iconOnly: true, glyph: 'connect', kbd: 'C', title: 'Connect', active: mode === 'connect', onClick: () => { setMode(mode ? null : 'connect'); if (!mode) say('Connect: pick the target node. Esc cancels.'); } }),
      h(Button, { variant: 'ghost', iconOnly: true, glyph: 'hide', kbd: 'H', title: 'Hide from view', onClick: () => hide(sel) }),
      h('span', { className: 'iw-selbar__sep' }),
      h(Button, { variant: 'ghost', iconOnly: true, kbd: '.', title: 'More actions', 'aria-label': 'More actions', onClick: () => setMenu({ id: sel, x: selNode.x, y: selNode.y }) }, '…')),
    selEdge && (() => { const e = edges.find((x) => x.id === selEdge); const a = e && byId.get(e.s), b = e && byId.get(e.t); if (!a || !b) return null;
      return h('div', { key: 'eb:' + selEdge, className: 'iw-selbar', role: 'toolbar', 'aria-label': 'Relationship actions', style: { left: ((a.x + b.x) / 2) * view.k + view.x, top: ((a.y + b.y) / 2) * view.k + view.y - 14 } },
        h('span', { className: 'iw-selbar__type iw-mono-s' }, e.type),
        h(Button, { variant: 'ghost', iconOnly: true, glyph: 'detach', kbd: '⌫', title: 'Detach', onClick: () => detach(selEdge) })); })(),
    h('div', { className: 'iw-canvas__corner' },
      h(Button, { variant: 'ghost', iconOnly: true, glyph: 'wander', kbd: 'W', title: 'Wander somewhere unexplored', onClick: wander }),
      h(Button, { variant: 'ghost', iconOnly: !dim.size, glyph: 'filter', active: filterOpen, play: dim.size > 0, title: 'Filter labels', onClick: () => setFilterOpen((o) => !o) }, dim.size ? (Object.keys(LABELS).length - dim.size) + '/' + Object.keys(LABELS).length : null)),
    filterOpen && h('div', { className: 'iw-canvas__filter' },
      Object.keys(LABELS).map((l) => h('button', { key: l, type: 'button', className: cx('iw-chip', !dim.has(l) && 'is-on'), 'aria-pressed': !dim.has(l), onClick: () => setDim((d) => { const n = new Set(d); n.has(l) ? n.delete(l) : n.add(l); return n; }) }, h(ShapeGlyph, { label: l }), l)),
      h('span', { className: 'iw-small iw-muted iw-canvas__filternote' }, dim.size ? h('button', { type: 'button', className: 'iw-link', onClick: () => setDim(new Set()) }, 'Show all') : 'Hidden labels dim in place.')),
    (hint || mode) && h('div', { className: 'iw-canvas__hint', role: 'status' }, hint || 'Connect: pick the target node. Esc cancels.'),
    h('div', { className: 'iw-canvas__trail' },
      histN > 0 && h(Button, { variant: 'ghost', kbd: '⌘Z', title: 'Undo the last step', onClick: undo }, '↶ UNDO'),
      trail.filter((id) => byId.has(id)).map((id, i, a) => h(Frag, { key: id }, i > 0 && h('span', { className: 'iw-trail__sep', 'aria-hidden': true }, '›'),
        h('button', { type: 'button', className: cx('iw-trail__step', i === a.length - 1 && 'is-here'), onClick: () => focusOn(id), title: 'Go back to ' + byId.get(id).caption }, h(ShapeGlyph, { label: byId.get(id).label, size: 10 }), byId.get(id).caption))),
      h('span', { className: 'iw-mono-s iw-muted iw-trail__count' }, nodes.length + ' nodes · ' + edges.length + ' rels')),
    intro && !showEmpty && h('div', { className: 'iw-canvas__intro' }, 'Hover a node to peek at its neighbours. Double-click to bring them in. Nothing here changes the database until you commit.'),
    h(NavPuck, { nodes, view, size, onPan: (p) => setView((v) => ({ ...v, ...p })), onZoom: (f) => zoomBy(f), onFit: fit }));
}

/* ------------------------------------------------------------------ Inspector */
function Disclosure({ title, meta, open, onToggle, children }) {
  return h('section', { className: cx('iw-disclose', open && 'is-open') },
    h('button', { type: 'button', className: 'iw-disclose__head', 'aria-expanded': open, onClick: onToggle },
      h('span', { className: cx('iw-disc', open && 'is-open'), 'aria-hidden': true }), h('span', { className: 'iw-cap' }, title), meta != null && h('span', { className: 'iw-mono-s iw-muted iw-disclose__meta' }, meta)),
    open && h('div', { className: 'iw-disclose__body' }, children));
}
/* console: `properties` [{key,type,value,dirty?}], `relationships` [{type,dir,label,count}] and `storage` [[k,v]]
   replace the demo's; `readOnly` turns editing off; with `onSave`, "+ Add property" opens a key/value row. */
function Inspector({ node, onSave, onExpand, onClose, properties, relationships, storage, readOnly }) {
  const n = node || demoGraph().nodes[0];
  const [openRels, setOpenRels] = useState(false); const [openStore, setOpenStore] = useState(false);
  const base = () => properties || propsFor(n);
  const [rows, setRows] = useState(base);
  const [edit, setEdit] = useState(null); const [draft, setDraft] = useState('');
  const [acked, setAcked] = useState(null); const [adding, setAdding] = useState(null);
  useEffect(() => { setRows(base()); setEdit(null); setAdding(null); }, [n.id, properties]);
  const addKey = useRef(null);
  const commitAdd = () => {
    const k = adding && adding.key.trim(); if (!k) { setAdding(null); return; }
    setRows((rs) => rs.filter((r) => r.key !== k).concat({ key: k, type: 'NEW', value: adding.value, dirty: true }));
    setAdding(null); setAcked(k + ':' + Date.now()); onSave && onSave(n, k, adding.value);
  };
  const commit = (i) => {
    if (draft === rows[i].value) { setEdit(null); return; }
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, value: draft, dirty: true } : r))); setEdit(null);
    setAcked(rows[i].key + ':' + Date.now()); onSave && onSave(n, rows[i].key, draft);
  };
  const rels = relationships ? relationships.map((r) => ({ ...r, lab: r.label })) : (SPAWN[n.label] || []).map(([lab, type], i) => ({ type, lab, dir: i % 3 === 2 ? 'in' : 'out', count: [2, 1, 4, 7, 3][i] || 1 }));
  const degree = rels.reduce((a, r) => a + r.count, 0);
  return h('aside', { className: 'iw-insp', 'aria-label': 'Inspector' },
    h('div', { className: 'iw-insp__head' },
      h('svg', { width: 28, height: 28, viewBox: '-14 -14 28 28', 'aria-hidden': true }, h(NodeShape, { label: n.label, r: 11 })),
      h('div', { className: 'iw-insp__title' }, h('div', { className: 'iw-title' }, n.caption), h('div', { className: 'iw-mono-s iw-muted' }, ':' + n.label + ' · ' + n.id)),
      onClose && h(Button, { variant: 'ghost', iconOnly: true, kbd: 'Esc', title: 'Close', 'aria-label': 'Close inspector', onClick: onClose }, '✕')),
    h('div', { className: 'iw-props', role: 'grid', 'aria-label': 'Properties' },
      rows.map((r, i) => h('div', { key: r.key, role: 'row', className: cx('iw-props__row', edit === i && 'is-edit', acked && acked.startsWith(r.key + ':') && 'is-acked', r.dirty && 'is-dirty') },
        h('span', { className: 'iw-props__k', title: r.dirty ? r.type + ' · uncommitted' : r.type }, r.key),
        edit === i
          ? h('input', { className: 'iw-props__in', autoFocus: true, value: draft, onChange: (e) => setDraft(e.target.value), onBlur: () => setEdit(null), onKeyDown: (e) => { if (e.key === 'Enter') commit(i); if (e.key === 'Escape') { e.stopPropagation(); setEdit(null); } } })
          : readOnly ? h('span', { className: 'iw-props__v', title: r.type }, r.value)
          : h('button', { type: 'button', className: 'iw-props__v', onClick: () => { setEdit(i); setDraft(r.value); }, title: r.type + ' · click to edit' }, r.value),
        h('span', { className: 'iw-props__t' }, edit === i ? r.type : ''),
        acked && acked.startsWith(r.key + ':') && h('span', { key: acked, className: 'iw-props__ack', 'aria-label': 'saved' }, h(Glyph, { action: 'save', play: true, size: 12 })))),
      adding && h('div', { role: 'row', className: 'iw-props__row is-edit' },
        h('input', { ref: addKey, className: 'iw-props__in iw-props__kin', autoFocus: true, placeholder: 'key', value: adding.key, onChange: (e) => setAdding({ ...adding, key: e.target.value }), onKeyDown: (e) => { if (e.key === 'Escape') { e.stopPropagation(); setAdding(null); } if (e.key === 'Enter') e.target.nextSibling && e.target.nextSibling.focus(); } }),
        h('input', { className: 'iw-props__in', placeholder: 'value (JSON or text)', value: adding.value, onChange: (e) => setAdding({ ...adding, value: e.target.value }), onKeyDown: (e) => { if (e.key === 'Enter') commitAdd(); if (e.key === 'Escape') { e.stopPropagation(); setAdding(null); } } })),
      !readOnly && !adding && h('button', { type: 'button', className: 'iw-props__add', onClick: () => setAdding({ key: '', value: '' }) }, '+ Add property')),
    h(Disclosure, { title: 'Relationships', meta: degree, open: openRels, onToggle: () => setOpenRels((o) => !o) },
      rels.map((r) => h('div', { key: r.type + r.dir + r.lab, className: 'iw-rels__row' },
        h('span', { className: 'iw-mono-s iw-muted' }, r.dir === 'out' ? '→' : '←'),
        h('span', { className: 'iw-mono-s iw-accent' }, r.type),
        h(ShapeGlyph, { label: r.lab }),
        h('span', { className: 'iw-mono-s iw-muted iw-rels__n' }, r.count),
        h(Button, { variant: 'ghost', iconOnly: true, glyph: 'expand', title: 'Expand ' + r.type, onClick: () => onExpand && onExpand(n.id) })))),
    h(Disclosure, { title: 'Storage', open: openStore, onToggle: () => setOpenStore((o) => !o) },
      h('dl', { className: 'iw-store' },
        (storage || [['created', 'tx 17 902'], ['record', 'nodestore · p 4 118'], ['size', '15 B + 212 B'], ['degree', String(degree)]]).map(([k, v]) =>
          h(Frag, { key: k }, h('dt', { className: 'iw-small iw-muted' }, k), h('dd', { className: 'iw-mono-s' }, v))))));
}

/* ------------------------------------------------------------------ RunCap (Colani #2) + QueryConsole */
/* Geometry, committed: flat left edge, straight top and bottom, right end an exact semicircle of radius h/2. */
function RunCap({ state = 'idle', onRun, ms, height = 64 }) {
  const H = height, r = H / 2, compact = H < 56, b = compact ? 72 : 88, W = b + r, K = 0.5523;
  const round = state !== 'idle';
  // Both outlines use the same command list (two cubics for the nose), so one can be tweened into the other.
  const tri = [b + r / 3, r / 3, b + (2 * r) / 3, (2 * r) / 3, b + r, r, b + (2 * r) / 3, r + r / 3, b + r / 3, H - r / 3, b, H];
  const arc = [b + K * r, 0, b + r, r - K * r, b + r, r, b + r, r + K * r, b + K * r, H, b, H];
  const [m, setM] = useState(round ? 1 : 0); const mRef = useRef(m); mRef.current = m;
  useEffect(() => { const from = mRef.current, to = round ? 1 : 0; if (from === to) return; return tween(200, (p) => setM(from + (to - from) * p)); }, [round]);
  const c = tri.map((v, i) => (v + (arc[i] - v) * m).toFixed(2));
  const shell = `M0 0H${b}C${c[0]} ${c[1]} ${c[2]} ${c[3]} ${c[4]} ${c[5]}C${c[6]} ${c[7]} ${c[8]} ${c[9]} ${c[10]} ${c[11]}H0Z`;
  const word = state === 'done' ? (ms != null && !compact ? ms + ' ms' : 'DONE') : state === 'running' ? 'RUNNING' : 'RUN';
  /* Running: each letter travels right into the nose and back. Out, the last letter leaves first; back, the first
     letter returns first, so the word opens like an accordion and closes again, and letters never collide. */
  const capRef = useRef(null);
  useEffect(() => {
    if (state !== 'running' || reduced() || !capRef.current) return;
    const ch = [...capRef.current.querySelectorAll('.iw-runcap__ch')]; const n = ch.length; const dx = compact ? 10 : 18;
    const st = 0.22 / Math.max(n - 1, 1), mv = 0.24, e = 'cubic-bezier(.45,0,.55,1)';
    const anims = ch.map((el, i) => {
      const o1 = (n - 1 - i) * st, o2 = 0.5 + i * st;
      return el.animate([
        { transform: 'translateX(0)', offset: 0 }, { transform: 'translateX(0)', offset: o1, easing: e },
        { transform: `translateX(${dx}px)`, offset: o1 + mv }, { transform: `translateX(${dx}px)`, offset: o2, easing: e },
        { transform: 'translateX(0)', offset: Math.min(o2 + mv, 1) }, { transform: 'translateX(0)', offset: 1 },
      ], { duration: 1400, iterations: Infinity });
    });
    return () => anims.forEach((a) => a.cancel());
  }, [state, compact]);
  return h('button', { type: 'button', className: cx('iw-runcap', 'is-' + state, compact && 'is-compact'), style: { width: W, height: H, }, onClick: onRun, disabled: state === 'running', 'aria-label': state === 'running' ? 'Running' : 'Run query', 'aria-keyshortcuts': 'Meta+Enter', title: state === 'done' && ms != null ? 'Finished in ' + ms + ' ms' : 'Run  ⌘↵' },
    h('svg', { className: 'iw-runcap__body', width: W, height: H, viewBox: `0 0 ${W} ${H}`, 'aria-hidden': true }, h('path', { className: 'iw-runcap__shell', d: shell })),
    h('span', { className: 'iw-runcap__in', 'aria-hidden': true },
      h('span', { key: word, ref: capRef, className: 'iw-runcap__cap' }, word.split('').map((ch, i) => h('span', { key: i, className: 'iw-runcap__ch', style: { '--i': i } }, ch === ' ' ? ' ' : ch)))));
}
const KW = /\b(MATCH|OPTIONAL|WHERE|RETURN|ORDER|BY|DESC|ASC|LIMIT|WITH|CREATE|MERGE|SET|DELETE|DETACH|UNWIND|AS|AND|OR|NOT|CALL|YIELD|SKIP|DISTINCT)\b/;
/* console: `comment` is the line-comment prefix (the design system's is //). */
const TOKENS = /('(?:[^'\\]|\\.)*')|(:[A-Za-z_][A-Za-z0-9_]*)|(\b\d+(?:\.\d+)?\b)|(\$[A-Za-z_]\w*)|([A-Za-z_]+)|(\s+|.)/.source;
function highlight(src, kw = KW, comment = '//') {
  const cm = comment.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&');
  const out = []; const re = new RegExp('(' + cm + '[^\\n]*)|' + TOKENS, 'g'); let m; let i = 0;
  while ((m = re.exec(src))) {
    const t = m[0]; let c = null;
    if (m[1]) c = 'cm'; else if (m[2]) c = 'st'; else if (m[3]) c = 'lb'; else if (m[4]) c = 'nu'; else if (m[5]) c = 'pa'; else if (m[6] && kw.test(t) && (kw !== KW || t === t.toUpperCase())) c = 'kw';
    out.push(c ? h('span', { key: i++, className: 'q-' + c }, t) : t);
  }
  return out;
}
/* console: `params` replaces the editor's options line, `keywords` (a RegExp) the highlighted words, `minRunMs`
   the shortest RUNNING (so the cap can be read), `comment` the line-comment prefix; a new `query` replaces the
   text; a rejected `onRun` returns the cap to rest. */
function QueryConsole({ query = DEMO_QUERY, onRun, open: openProp, onOpenChange, params, keywords, minRunMs, comment = '//' }) {
  const [src, setSrc] = useState(query);
  useEffect(() => { setSrc(query); }, [query]);
  const [state, setState] = useState('idle'); const [ms, setMs] = useState(null);
  const [ghost, setGhost] = useState(0);
  const [openS, setOpenS] = useState(openProp == null ? true : openProp); const open = openProp != null ? openProp : openS;
  const setOpen = (v) => { onOpenChange ? onOpenChange(v) : setOpenS(v); };
  const preRef = useRef(null); const taRef = useRef(null);
  useEffect(() => { if (open && taRef.current && openProp != null) taRef.current.focus(); }, [open]);
  const run = () => {
    if (state === 'running') return;
    setGhost((g) => g + 1); setState('running');
    const t0 = performance.now(); const dur = minRunMs != null ? minRunMs : 900 + Math.random() * 500;
    Promise.resolve().then(() => (onRun ? onRun(src) : null)).then((v) => new Promise((r) => setTimeout(() => r(v), dur))).then((v) => {
      setMs(typeof v === 'number' ? v.toFixed(1) : (performance.now() - t0 - dur + 12.4).toFixed(1)); setState('done');
      setTimeout(() => setState('idle'), 1600);
    }, () => setState('idle'));
  };
  const code = src.split('\n').filter((l) => l.trim() && !l.trim().startsWith(comment));
  const first = code[0] || src;
  const lines = src.split('\n').length;
  if (!open) return h('div', { className: 'iw-qc is-compact' },
    h('button', { type: 'button', className: 'iw-qc__line', onClick: () => setOpen(true), title: 'Edit query  ⌘L' },
      h('span', { className: 'iw-qc__prompt iw-mono', 'aria-hidden': true }, '›'),
      h('span', { className: 'iw-mono iw-qc__first' }, highlight(first.trim(), keywords, comment)),
      code.length > 1 && h('span', { className: 'iw-mono-s iw-muted' }, '+' + (code.length - 1) + ' lines'),
      ghost > 0 && h('span', { key: ghost, className: 'iw-qc__ghost iw-mono' }, first.trim().slice(0, 36))),
    h(RunCap, { state, onRun: run, ms, height: 40 }));
  return h('div', { className: 'iw-qc' },
    h('div', { className: 'iw-qc__gutter iw-mono', 'aria-hidden': true }, Array.from({ length: lines }, (_, i) => h('div', { key: i }, i + 1))),
    h('div', { className: 'iw-qc__ed' },
      h('pre', { className: 'iw-qc__hl iw-mono', ref: preRef, 'aria-hidden': true }, highlight(src, keywords, comment), '\n'),
      h('textarea', { ref: taRef, className: 'iw-qc__ta iw-mono', spellCheck: false, value: src, 'aria-label': 'Query', onChange: (e) => setSrc(e.target.value), onScroll: (e) => { if (preRef.current) { preRef.current.scrollTop = e.target.scrollTop; preRef.current.scrollLeft = e.target.scrollLeft; } }, onKeyDown: (e) => { if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') { e.preventDefault(); run(); } if (e.key === 'Escape' && onOpenChange) { e.preventDefault(); setOpen(false); } } }),
      ghost > 0 && h('span', { key: ghost, className: 'iw-qc__ghost iw-mono' }, first.trim().slice(0, 36)),
      h('div', { className: 'iw-qc__params iw-mono-s iw-muted' }, params != null ? params : 'read · auto-commit · 30 s')),
    h('div', { className: 'iw-qc__side' },
      onOpenChange && h(Button, { variant: 'ghost', iconOnly: true, kbd: 'Esc', title: 'Collapse', 'aria-label': 'Collapse editor', onClick: () => setOpen(false) }, '⌄'),
      h(RunCap, { state, onRun: run, ms })));
}

/* ------------------------------------------------------------------ ResultTable */
/* console: rows keep their order until a header is clicked (the demo's sorts by its 4th column); numeric columns
   sort as numbers; `pager` {page, pages, onPrev, onNext} drives the pager and `onExport('csv'|'json')` the export
   buttons; `emptyTitle`/`emptyBody` word the empty state. */
function ResultTable({ columns = DEMO_COLUMNS, rows, selected, onSelect, morph, footer, pager, onExport, emptyTitle, emptyBody }) {
  const data = useMemo(() => rows || rowsFromGraph(demoGraph()), [rows]);
  const [sort, setSort] = useState(() => (rows ? { i: -1, dir: 1 } : { i: 3, dir: -1 }));
  useEffect(() => { if (rows) setSort({ i: -1, dir: 1 }); }, [columns]);
  const sorted = useMemo(() => {
    if (sort.i < 0) return data;
    const num = columns[sort.i] && columns[sort.i].num;
    const v = (r) => { const c = r.cells[sort.i] || {}; const x = c.node ? c.node.caption : c.v; return num ? Number(x) : x == null ? '' : String(x); };
    return [...data].sort((a, b) => (v(a) > v(b) ? 1 : v(a) < v(b) ? -1 : 0) * sort.dir);
  }, [data, sort, columns]);
  if (!data.length) return h(EmptyState, { kind: 'results', title: emptyTitle, body: emptyBody });
  return h('div', { className: cx('iw-table', morph && 'is-morph') },
    h('div', { className: 'iw-table__scroll' }, h('table', null,
      h('thead', null, h('tr', null,
        h('th', { className: 'iw-table__num' }, '#'),
        columns.map((c, i) => h('th', { key: c.key, className: c.num ? 'is-num' : undefined, 'aria-sort': sort.i === i ? (sort.dir > 0 ? 'ascending' : 'descending') : 'none' },
          h('button', { type: 'button', onClick: () => setSort((s) => ({ i, dir: s.i === i ? -s.dir : 1 })) },
            h('span', { className: 'iw-table__name iw-mono' }, c.name),
            h('span', { className: 'iw-table__type' }, c.type),
            h('span', { className: 'iw-table__sort', 'aria-hidden': true }, sort.i === i ? (sort.dir > 0 ? '▲' : '▼') : ''))))) ),
      h('tbody', null, sorted.map((r, ri) => h('tr', { key: r.id, className: cx(selected === r.id && 'is-sel'), style: { '--i': ri }, onClick: () => onSelect && onSelect(r.id), tabIndex: 0, onKeyDown: (e) => e.key === 'Enter' && onSelect && onSelect(r.id) },
        h('td', { className: 'iw-table__num iw-mono-s' }, ri + 1),
        r.cells.map((c, i) => h('td', { key: i, className: cx(c.node && 'is-node', columns[i] && columns[i].num && 'is-num') },
          c.node ? h('span', { className: 'iw-table__nodecell' }, h('span', { className: 'iw-table__dot' }, h(ShapeGlyph, { label: c.node.label })), h('span', { className: 'iw-mono' }, c.node.caption), h('span', { className: 'iw-mono-s iw-muted' }, c.node.id)) : h('span', { className: 'iw-mono' }, c.v)))))))),
    h('div', { className: 'iw-table__foot' },
      h('span', { className: 'iw-mono-s' }, footer || data.length + ' rows · started streaming after 2.1 ms · consumed after 14.3 ms'),
      h('span', { className: 'iw-table__pager' },
        h(Button, { variant: 'ghost', disabled: !pager || pager.page <= 1, title: 'Previous page', onClick: pager && pager.onPrev }, '‹'),
        h('span', { className: 'iw-mono-s' }, pager ? pager.page + ' / ' + pager.pages : '1 / 1'),
        h(Button, { variant: 'ghost', disabled: !pager || pager.page >= pager.pages, title: 'Next page', onClick: pager && pager.onNext }, '›'),
        h(Button, { variant: 'ghost', onClick: onExport && (() => onExport('csv')) }, 'CSV'), h(Button, { variant: 'ghost', onClick: onExport && (() => onExport('json')) }, 'JSON'))));
}

/* ------------------------------------------------------------------ PlanView */
/* console: `summary` [[caption, value]] replaces the demo's figures, `hint` {label, onClick} its link; the
   current operator starts on the first hot or warned one. */
function PlanView({ plan, summary, hint }) {
  const ops = useMemo(() => plan || demoPlan(), [plan]);
  const firstCur = () => (ops.find((o) => o.hot || o.warn) || ops[0] || {}).id;
  const [cur, setCur] = useState(firstCur);
  useEffect(() => { setCur(firstCur()); }, [ops]);
  const maxHits = Math.max(1, ...ops.map((o) => o.hits));
  const total = ops.reduce((a, o) => a + o.hits, 0), time = ops.reduce((a, o) => a + o.ms, 0);
  return h('div', { className: 'iw-plan' },
    h('div', { className: 'iw-plan__sum' },
      (summary || [['DB HITS', fmt(total)], ['TIME', time.toFixed(1) + ' ms'], ['ROWS', '48'], ['RUNTIME', 'pipelined'], ['PLANNER', 'cost']]).map(([k, v]) => h('div', { key: k }, h('div', { className: 'iw-cap' }, k), h('div', { className: 'iw-metric' }, v)))),
    h('div', { className: 'iw-plan__head iw-cap' }, h('span', null, 'OPERATOR'), h('span', { className: 'is-num' }, 'EST.'), h('span', { className: 'is-num' }, 'ROWS'), h('span', { className: 'is-num' }, 'DB HITS'), h('span', null, ''), h('span', { className: 'is-num' }, 'MS')),
    ops.map((o) => h('div', { key: o.id, className: cx('iw-plan__row', cur === o.id && 'is-cur', o.hot && 'is-hot'), onClick: () => setCur(o.id), tabIndex: 0 },
      h('span', { className: 'iw-plan__op', style: { paddingLeft: o.depth * 14 } },
        o.depth > 0 && h('span', { className: 'iw-plan__elbow', 'aria-hidden': true }),
        h('span', { className: 'iw-body-strong' }, o.op), h('span', { className: 'iw-mono-s iw-muted' }, o.detail),
        o.warn && h('span', { className: 'iw-state is-populating', title: o.warn }, 'SCAN')),
      h('span', { className: 'iw-mono-s is-num iw-muted' }, fmt(o.est)),
      h('span', { className: cx('iw-mono-s is-num', o.est > 0 && o.rows / o.est > 4 && 'iw-warn') }, fmt(o.rows)),
      h('span', { className: 'iw-mono-s is-num' }, fmt(o.hits)),
      h('span', { className: 'iw-plan__bar' }, h('span', { style: { width: Math.max(1, (o.hits / maxHits) * 100) + '%' } })),
      h('span', { className: 'iw-mono-s is-num' }, o.ms.toFixed(1)))),
    (() => { const o = ops.find((x) => x.id === cur); return o && o.warn ? h('div', { className: 'iw-plan__note' }, h('span', { className: 'iw-state is-populating' }, 'HINT'), ' ', o.warn, '. ', (!summary || hint) && h('button', { type: 'button', className: 'iw-link', onClick: hint && hint.onClick }, hint ? hint.label : 'Create index…')) : null; })());
}

/* ------------------------------------------------------------------ LogStream */
function LogStream({ entries, follow = true }) {
  const data = entries || demoLog();
  const [lv, setLv] = useState({ INFO: true, WARN: true, ERROR: true });
  const ref = useRef(null);
  useEffect(() => { if (follow && ref.current) ref.current.scrollTop = ref.current.scrollHeight; }, [data.length]);
  const shown = data.filter((e) => lv[e.level]);
  return h('div', { className: 'iw-log' },
    h('div', { className: 'iw-log__bar' }, ['INFO', 'WARN', 'ERROR'].map((l) => h('button', { key: l, type: 'button', className: cx('iw-chip', lv[l] && 'is-on', 'lv-' + l.toLowerCase()), 'aria-pressed': lv[l], onClick: () => setLv((s) => ({ ...s, [l]: !s[l] })) }, l, h('span', { className: 'iw-mono-s' }, data.filter((e) => e.level === l).length))), h('span', { className: 'iw-log__fill' }), h('span', { className: 'iw-small iw-muted' }, follow ? 'following' : 'paused')),
    h('div', { className: 'iw-log__list', ref, role: 'log' },
      shown.length === 0 ? h('div', { className: 'iw-log__none iw-small iw-muted' }, 'Nothing logged. Suspiciously calm.')
        : shown.map((e, i) => h('div', { key: i + e.t, className: cx('iw-log__row', 'lv-' + e.level.toLowerCase(), e.fresh && 'is-fresh') },
          h('span', { className: 'iw-mono-s iw-muted' }, e.t.slice(0, 8)), h('span', { className: 'iw-log__lv' }, e.level === 'ERROR' ? '✕ ERROR' : e.level), h('span', { className: 'iw-mono-s' }, e.msg)))));
}

/* One line: the newest event, and a count of problems you have not looked at. Click opens the full log. */
function LogTicker({ entries, onOpen, open }) {
  const data = entries || demoLog();
  const last = data[data.length - 1];
  const errs = data.filter((e) => e.level === 'ERROR').length, warns = data.filter((e) => e.level === 'WARN').length;
  return h('button', { type: 'button', className: cx('iw-ticker', open && 'is-open'), onClick: onOpen, 'aria-expanded': !!open, title: (open ? 'Hide log' : 'Show log') + '  ⌘J' },
    errs > 0 && h('span', { className: 'iw-state is-failed' }, '✕ ' + errs),
    warns > 0 && h('span', { className: 'iw-state is-populating' }, '\u25B2 ' + warns),
    last && h('span', { key: last.t + last.msg, className: 'iw-ticker__msg iw-mono-s' }, h('span', { className: 'iw-muted' }, last.t.slice(0, 8)), ' ', last.msg),
    h('span', { className: cx('iw-disc', 'iw-ticker__disc', open && 'is-open'), 'aria-hidden': true }));
}

/* ------------------------------------------------------------------ ShortcutSheet */
const SHEET = [
  ['Anywhere', [['⌘K', 'Go to anything'], ['?', 'This sheet'], ['\u2325', 'Hold to show shortcuts in place'], ['1 2 3', 'Graph, table, plan'], ['⌘L', 'Edit query'], ['⌘↵', 'Run query'], ['⌘J', 'Log'], ['⌘I', 'Instruments']]],
  ['Graph', [['hover', 'Peek at neighbours'], ['W', 'Wander'], ['\u2318Z', 'Undo'], ['E', 'Expand'], ['F', 'Focus'], ['C', 'Connect'], ['H', 'Hide'], ['⌫', 'Detach relationship'], ['.', 'All actions'], ['←↑→↓', 'Walk to neighbour'], ['0', 'Fit'], ['Esc', 'Deselect']]],
  ['Schema', [['/', 'Filter'], ['↑↓', 'Move'], ['→ ←', 'Unfold, fold'], ['↵', 'Open']]],
  ['Inspector', [['↵', 'Commit value'], ['Esc', 'Revert']]],
];
/* console: `extra` adds groups ([[group, [[key, what]...]]...]) after the design system's. */
function ShortcutSheet({ onClose, extra }) {
  const ref = useRef(null);
  useEffect(() => { ref.current && ref.current.focus(); }, []);
  return h('div', { className: 'iw-sheet', role: 'dialog', 'aria-label': 'Keyboard shortcuts', tabIndex: -1, ref, onKeyDown: (e) => { if (e.key === 'Escape' || e.key === '?') { e.preventDefault(); e.stopPropagation(); onClose && onClose(); } }, onClick: (e) => { if (e.target === e.currentTarget) onClose && onClose(); } },
    h('div', { className: 'iw-sheet__card' },
      h('div', { className: 'iw-sheet__head' }, h('span', { className: 'iw-title' }, 'Shortcuts'), h('span', { className: 'iw-small iw-muted' }, 'Hold ⌥ anywhere to see them in place.')),
      h('div', { className: 'iw-sheet__cols' }, SHEET.concat(extra || []).map(([g, rows]) => h('div', { key: g },
        h('div', { className: 'iw-cap' }, g.toUpperCase()),
        rows.map(([k, l]) => h('div', { key: k + l, className: 'iw-sheet__row' }, h(Key, null, k), h('span', null, l))))))));
}

/* ------------------------------------------------------------------ ViewSwitch */
function ViewSwitch({ items, value, onChange }) {
  const its = items || [{ id: 'graph', label: 'GRAPH', key: '1' }, { id: 'table', label: 'TABLE', key: '2' }, { id: 'plan', label: 'PLAN', key: '3' }];
  const ref = useRef(null); const [ind, setInd] = useState({ x: 0, w: 0 });
  useLayoutEffect(() => { const el = ref.current && ref.current.querySelector('[aria-selected="true"]'); if (el) setInd({ x: el.offsetLeft, w: el.offsetWidth }); }, [value]);
  return h('div', { className: 'iw-switch', role: 'tablist', ref },
    h('span', { className: 'iw-switch__ind', style: { transform: `translateX(${ind.x}px)`, width: ind.w } }),
    its.map((it) => h('button', { key: it.id, role: 'tab', type: 'button', 'aria-selected': value === it.id, className: cx('iw-switch__it', value === it.id && 'is-on'), onClick: () => onChange && onChange(it.id) }, it.label, h('span', { className: 'iw-switch__k iw-hint' }, it.key))));
}

/* ------------------------------------------------------------------ EmptyState */
const EMPTY = {
  canvas: ['An empty graph.', 'Technically valid. Drag a label from the schema, or run a query.', [['/', 'filter schema'], ['⌘↵', 'run query']]],
  results: ['0 rows.', 'The database did exactly what you asked. That is the problem.', [['⌘↵', 'run again']]],
  log: ['Nothing logged.', 'Suspiciously calm.', []],
  selection: ['Nothing selected.', 'Click a node, a row or a schema entry. The inspector is patient.', [['↑↓', 'navigate']]],
};
function EmptyState({ kind = 'canvas', title, body }) {
  const [t, b, keys] = EMPTY[kind] || EMPTY.canvas;
  return h('div', { className: 'iw-empty' },
    h('svg', { width: 64, height: 40, viewBox: '0 0 64 40', 'aria-hidden': true, className: 'iw-empty__art' },
      h('circle', { className: 'ghost', cx: 14, cy: 20, r: 8 }), h('line', { className: 'ghost-line', x1: 23, y1: 20, x2: 41, y2: 20 }), h('circle', { className: 'ghost', cx: 50, cy: 20, r: 8 })),
    h('div', { className: 'iw-empty__t' }, title || t),
    h('div', { className: 'iw-empty__b' }, body || b),
    keys.length > 0 && h('div', { className: 'iw-empty__k' }, keys.map(([k, l]) => h('span', { key: k }, h(Key, null, k), ' ', l))));
}

/* ------------------------------------------------------------------ Workbench (page) */
function Workbench() {
  const g = useMemo(() => demoGraph(), []);
  const [view, setView] = useState('graph'); const [morph, setMorph] = useState(false);
  const [sel, setSel] = useState(null);
  const [tx, setTx] = useState(18204); const [busy, setBusy] = useState(false);
  const [log, setLog] = useState(demoLog());
  const [qOpen, setQOpen] = useState(false); const [logOpen, setLogOpen] = useState(false);
  const [instr, setInstr] = useState(false); const [sheet, setSheet] = useState(false);
  const expandRef = useRef(null);
  const now = () => { const d = new Date(); return d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0'); };
  const addLog = (level, msg) => setLog((l) => l.concat({ t: now(), level, msg, fresh: true }).slice(-80));
  const go = (v) => {
    if (v === view) return;
    if (view === 'table' && v === 'graph' && !reduced()) { setMorph(true); setTimeout(() => { setMorph(false); setView('graph'); }, 220); }
    else setView(v);
  };
  useEffect(() => {
    const k = (e) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key.toLowerCase() === 'l') { e.preventDefault(); setQOpen(true); return; }
      if (mod && e.key.toLowerCase() === 'j') { e.preventDefault(); setLogOpen((o) => !o); return; }
      if (mod && e.key.toLowerCase() === 'i') { e.preventDefault(); setInstr((o) => !o); return; }
      if (e.target.closest('input,textarea')) return;
      if (e.key === '?') { e.preventDefault(); setSheet((s) => !s); }
      if (e.key === '1') go('graph'); if (e.key === '2') go('table'); if (e.key === '3') go('plan');
    };
    window.addEventListener('keydown', k); return () => window.removeEventListener('keydown', k);
  });
  const rows = useMemo(() => rowsFromGraph(g), [g]);
  const bottomOpen = qOpen || logOpen;
  return h('div', { className: cx('iw-wb', sel && 'has-insp', bottomOpen && 'has-bottom') },
    h(StatusRail, { tx, activity: busy, txOpen: busy ? 1 : 0, open: instr, onToggle: setInstr, onHelp: () => setSheet(true) }),
    h('div', { className: 'iw-wb__nav' }, h(SchemaNavigator, { onSelect: (it) => it.label && addLog('INFO', `schema · :${it.label} · ${fmt(it.count)} nodes`) })),
    h('main', { className: 'iw-wb__main' },
      h('div', { className: 'iw-wb__bar' }, h(ViewSwitch, { value: morph ? 'graph' : view, onChange: go })),
      h('div', { className: 'iw-wb__view' },
        h('div', { className: 'iw-wb__pane', hidden: view !== 'graph' }, h(GraphCanvas, { graph: g, selected: sel ? sel.id : null, onSelect: (n) => setSel(n || null), onLog: addLog, onExpandRef: expandRef })),
        (view === 'table' || morph) && h(ResultTable, { rows, morph, selected: sel && sel.id, onSelect: (id) => { const n = g.nodes.find((x) => x.id === id); n && setSel(n); } }),
        view === 'plan' && h(PlanView, null))),
    h('div', { className: 'iw-wb__insp' }, sel && h(Inspector, { node: sel, onClose: () => setSel(null), onExpand: (id) => { go('graph'); expandRef.current && expandRef.current(id); }, onSave: (n, k, v) => { setBusy(true); setTimeout(() => { setBusy(false); setTx((t) => t + 1); addLog('INFO', `tx ${fmt(tx + 1)} committed · SET ${n.id}.${k} = '${v}'`); }, 260); } })),
    h('div', { className: cx('iw-wb__bottom', logOpen && 'has-log') },
      h('div', { className: 'iw-wb__q' }, h(QueryConsole, { open: qOpen, onOpenChange: setQOpen, onRun: (q) => { setBusy(true); addLog('INFO', 'query submitted · ' + q.split('\n').filter((l) => l.trim() && !l.startsWith('//'))[0].slice(0, 48) + '…'); setTimeout(() => { setBusy(false); addLog('INFO', '48 rows · 195 505 db hits · 13.6 ms'); }, 1200); return 13.6; } })),
      h('div', { className: 'iw-wb__log' }, h(LogTicker, { entries: log, open: logOpen, onOpen: () => setLogOpen((o) => !o) }), logOpen && h(LogStream, { entries: log }))),
    sheet && h(ShortcutSheet, { onClose: () => setSheet(false) }));
}

const IronWeaver = { LogTicker, ShortcutSheet, Disclosure, Glyph, Button, Key, Tag, Led, Meter, NodeShape, ShapeGlyph, SaveAck, StatusRail, SchemaNavigator, GraphCanvas, RadialMenu, NavPuck, Inspector, RunCap, QueryConsole, ResultTable, PlanView, LogStream, ViewSwitch, EmptyState, Workbench, demo, setLabels };
window.IronWeaver = Object.assign(window.IronWeaver || {}, IronWeaver);

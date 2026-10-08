/* Ironweaver DB operator console: the explorer (step 16a). The graph to walk through, like Neo4j's browser,
 * and the namespace to browse and edit, like phpMyAdmin: schema on the left, graph / table / plan / structure
 * in the middle, the inspector on the right, the query line and the log at the bottom. Edits are staged and
 * committed together, as one commit of the Database trait. Reads and writes go to IW.ui.source() only. */
(function () {
  'use strict';
  const R = window.React; const h = R.createElement; const Frag = R.Fragment;
  const { useState, useEffect, useRef, useMemo, useCallback } = R;
  const I = window.IronWeaver; const U = window.IW.ui; const Q = window.IW.query;
  const src = U.source();

  const PAGE = 50;          // rows per page of find
  const MATCH_LIMIT = 200;  // matches per match, unless \limit says otherwise
  const GRAPH_MAX = 150;    // nodes a result puts on the canvas
  const NEIGHBOURS = 24;    // nodes one expansion brings in
  const DEFAULT_QUERY = '-- a first look: any 40 edges\nmatch (a)-[e]->(b)\n\\limit 40';

  const stamp = () => { const d = new Date(); return d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0'); };
  const errText = (e) => (e && e.code ? `error (${e.code}): ${e.message}` : String((e && e.message) || e));
  const reduced = () => window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  function download(name, type, text) {
    const a = document.createElement('a'); a.href = URL.createObjectURL(new Blob([text], { type })); a.download = name;
    document.body.appendChild(a); a.click(); setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 0);
  }

  /* ------------------------------------------------------------------ structure: phpMyAdmin's "Structure" tab */
  function StructureView({ ns, schema, status, onBrowse, onCreateIndex }) {
    const [path, setPath] = useState('');
    if (!schema || !status) return h(I.EmptyState, { kind: 'selection', title: 'Reading the namespace.', body: 'Labels, keys and indexes appear here.' });
    const ix = new Map(status.indexes.map((x) => [x.path.join('.'), x]));
    const idxState = (x) => (!x ? null : x.building ? h('span', { className: 'iw-state is-populating' }, 'BUILDING ' + U.pct(x.building.scanned / Math.max(1, x.building.total))) : h('span', { className: 'iw-state is-online' }, x.unique ? 'UNIQUE' : 'READY'));
    const T = (cols, rows, empty) => h('div', { className: 'iw-table cs-struct__table' }, h('table', null,
      h('thead', null, h('tr', null, cols.map(([name, numeric]) => h('th', { key: name, className: numeric ? 'is-num' : undefined }, h('span', { className: 'cs-th iw-cap' }, name))))),
      h('tbody', null, rows.length ? rows : h('tr', null, h('td', { colSpan: cols.length, className: 'iw-small iw-muted' }, empty)))));
    const create = (e) => { e.preventDefault(); const p = path.trim().split('.').filter(Boolean); if (p.length) onCreateIndex(p).then(() => setPath(''), () => {}); };
    return h('div', { className: 'cs-struct' },
      h('section', { className: 'cs-struct__head' },
        h('div', null, h('div', { className: 'iw-cap' }, 'NAMESPACE'), h('div', { className: 'iw-title' }, ns), h('div', { className: 'iw-mono-s iw-muted' }, 'id ' + status.id + ' · created ' + U.date(status.createdMicros))),
        [['NODES', U.num(status.nodes)], ['EDGES', U.num(status.edges)], ['MEMORY', U.bytes(status.memoryBytes)], ['SEQ', U.num(status.seq)], ['SYNCED', status.syncedSeq == null ? 'off' : U.num(status.syncedSeq)], ['CHECKPOINT', status.checkpoint == null ? '—' : U.num(status.checkpoint)]]
          .map(([k, v]) => h('div', { key: k }, h('div', { className: 'iw-cap' }, k), h('div', { className: 'iw-metric' }, v))),
        status.readOnly && h('div', { className: 'cs-struct__alert' }, h('span', { className: 'iw-state is-failed' }, '✕ READ-ONLY'), h('span', { className: 'iw-small' }, ' ' + status.readOnly)),
        status.checkpointFailure && h('div', { className: 'cs-struct__alert' }, h('span', { className: 'iw-state is-failed' }, '✕ CHECKPOINT FAILED'), h('span', { className: 'iw-small' }, ' ' + status.checkpointFailure))),
      h('section', null, h('h2', { className: 'iw-cap' }, 'LABELS AND THEIR KEYS'),
        schema.sampledNodes < schema.nodes && h('p', { className: 'iw-small iw-muted cs-struct__note' }, `Node counts are exact; keys are from the first ${U.num(schema.sampledNodes)} of ${U.num(schema.nodes)} nodes.`),
        T([['LABEL'], ['KEY'], ['VALUES'], ['PRESENT', true], ['INDEX'], ['']],
          schema.labels.flatMap((l) => {
            const keys = Object.entries(l.keys).sort(([a], [b]) => (a < b ? -1 : 1));
            const head = h('tr', { key: l.name, className: 'cs-struct__group' },
              h('td', null, h('span', { className: 'cs-struct__label' }, h(I.ShapeGlyph, { label: l.name }), h('b', null, l.name))),
              h('td', { colSpan: 4, className: 'iw-mono-s iw-muted' }, U.num(l.count) + ' nodes · ' + keys.length + (l.moreKeys ? '+' : '') + ' keys' + (l.sampled < l.count ? ' in ' + U.num(l.sampled) + ' sampled' : '')),
              h('td', { className: 'is-num' }, h(I.Button, { variant: 'ghost', onClick: () => onBrowse(l.name) }, 'BROWSE')));
            return [head].concat(keys.map(([k, kinds]) => {
              const n = Object.values(kinds).reduce((a, b) => a + b, 0);
              return h('tr', { key: l.name + '.' + k },
                h('td', null), h('td', { className: 'iw-mono' }, k),
                h('td', { className: 'iw-mono-s' }, Object.entries(kinds).sort((a, b) => b[1] - a[1]).map(([kind, c]) => U.TYPE[kind] + (Object.keys(kinds).length > 1 ? ' ' + c : '')).join(' · ')),
                h('td', { className: 'is-num iw-mono-s' }, U.pct(n / Math.max(1, l.sampled))),
                h('td', null, idxState(ix.get(k))), h('td', null));
            }));
          }), 'No labels yet.')),
      h('section', null, h('h2', { className: 'iw-cap' }, 'EDGE TYPES'),
        T([['TYPE'], ['EDGES', true]], schema.types.map((t) => h('tr', { key: t.name || '' }, h('td', { className: 'iw-mono iw-accent' }, '→ ' + (t.name || '(untyped)')), h('td', { className: 'is-num iw-mono-s' }, U.num(t.count)))), 'No edges yet.')),
      h('section', null, h('h2', { className: 'iw-cap' }, 'INDEXES'),
        T([['PATH'], ['STATE'], ['WHY'], ['ENTRIES', true], ['DISTINCT', true], ['MEMORY', true]],
          status.indexes.map((x) => h('tr', { key: x.path.join('.') },
            h('td', { className: 'iw-mono' }, '[' + x.path.join(', ') + ']'), h('td', null, idxState(x)),
            h('td', { className: 'iw-small iw-muted' }, [x.declared && 'declared', x.unique && 'a unique constraint'].filter(Boolean).join(', ')),
            h('td', { className: 'is-num iw-mono-s' }, x.size ? U.num(x.size.entries) : x.building ? U.num(x.building.scanned) + ' / ' + U.num(x.building.total) : '—'),
            h('td', { className: 'is-num iw-mono-s' }, x.size ? U.num(x.size.distinctKeys) : '—'),
            h('td', { className: 'is-num iw-mono-s' }, x.size ? U.bytes(x.size.memoryBytes) : '—'))), 'No indexes.'),
        h('form', { className: 'cs-struct__form', onSubmit: create },
          h('label', { className: 'iw-small iw-muted', htmlFor: 'cs-ixpath' }, 'Create an index on'),
          h('input', { id: 'cs-ixpath', className: 'cs-input iw-mono', placeholder: 'attribute path, like address.city', value: path, onChange: (e) => setPath(e.target.value), disabled: !!status.readOnly }),
          h(I.Button, { type: 'submit', disabled: !path.trim() || !!status.readOnly }, 'CREATE INDEX'),
          h('span', { className: 'iw-small iw-muted' }, 'Built online; reads use it once it is ready.'))),
      h('section', null, h('h2', { className: 'iw-cap' }, 'CONSTRAINTS'),
        T([['KIND'], ['LABEL'], ['PATH']], schema.constraints.map((c, i) => h('tr', { key: i }, h('td', { className: 'iw-cap' }, c.kind.toUpperCase()), h('td', null, h('span', { className: 'cs-struct__label' }, h(I.ShapeGlyph, { label: c.label }), c.label)), h('td', { className: 'iw-mono' }, '[' + c.path.join(', ') + ']'))), 'No constraints.')),
      status.marks.length > 0 && h('section', null, h('h2', { className: 'iw-cap' }, 'MARKS (PROJECTION)'),
        T([['MARK'], ['POSITION', true], ['SET BY SEQ', true]], status.marks.map((m) => h('tr', { key: m.name }, h('td', { className: 'iw-mono' }, m.name), h('td', { className: 'is-num iw-mono-s' }, U.num(m.position)), h('td', { className: 'is-num iw-mono-s' }, U.num(m.seq)))), '')),
      h('section', null, h('h2', { className: 'iw-cap' }, 'RECOVERY AT OPEN'),
        h('p', { className: 'iw-small iw-muted cs-struct__note' }, status.recovery.checkpoint == null ? 'Started from an empty namespace.' : `Loaded checkpoint ${U.num(status.recovery.checkpoint)}, replayed ${U.num(status.recovery.replayed)} WAL records, recovered to seq ${U.num(status.recovery.seq)}.`)));
  }

  /* ------------------------------------------------------------------ the page */
  function Explorer() {
    const [nss, setNss] = useState([]); const [ns, setNs] = useState(null);
    const [schema, setSchema] = useState(null); const [status, setStatus] = useState(null); const [server, setServer] = useState(null);
    const [view, setView] = useState('graph'); const [morph, setMorph] = useState(false);
    const [result, setResult] = useState(null); const [error, setError] = useState(null);
    const [canvasKey, setCanvasKey] = useState(0);
    const [selId, setSelId] = useState(null); const [, setCacheTick] = useState(0);
    const cache = useRef(new Map());
    const [pending, setPending] = useState([]); const [busy, setBusy] = useState(false);
    const [log, setLog] = useState([]);
    const [qOpen, setQOpen] = useState(false); const [logOpen, setLogOpen] = useState(false);
    const [instr, setInstr] = useState(false); const [sheet, setSheet] = useState(false); const [palette, setPalette] = useState(null);
    const [query, setQuery] = useState(DEFAULT_QUERY); const [rels, setRels] = useState(null);
    const expandRef = useRef(null); const navT = useRef(null);

    const addLog = useCallback((level, msg) => setLog((l) => l.concat({ t: stamp(), level, msg, fresh: true }).slice(-200)), []);
    const remember = (nodes) => { nodes.forEach((n) => n && cache.current.set(n.id, n)); setCacheTick((t) => t + 1); };
    // Label counts decide a node's primary label; a ref, so a command run right after `refresh` sees the new ones
    const counts = useRef({});
    const toCanvas = useCallback((n) => ({ id: n.id, label: U.primaryLabel(n, counts.current), caption: U.caption(n) }), []);

    /* The server: a mock that keeps running (background commits, checkpoints) while the page is open */
    useEffect(() => {
      src.log().forEach((e) => setLog((l) => l.concat(e)));
      const off = src.onLog((e) => setLog((l) => l.concat({ ...e, fresh: true }).slice(-200)));
      const poll = () => src.server().then(setServer, (e) => setServer((s) => (s ? { ...s, ready: false, error: e } : null)));
      poll(); const t1 = setInterval(() => src.tick(), 1000); const t2 = setInterval(poll, 5000);
      src.namespaces().then((list) => { setNss(list); const want = U.params().get('ns'); setNs((list.find((x) => x.name === want) || list[0]).name); });
      return () => { off(); clearInterval(t1); clearInterval(t2); };
    }, []);

    const refresh = useCallback((name) => Promise.all([src.schema(name), src.namespaceStatus(name)]).then(([sc, st]) => {
      counts.current = Object.fromEntries(sc.labels.map((l) => [l.name, l.count]));
      I.setLabels(U.labelStyles(sc.labels.map((l) => l.name))); setSchema(sc); setStatus(st); return sc;
    }), []);

    /* ---------------------------------------------------------------- running a command */
    const graphOf = (nodes, edges) => {
      const shown = nodes.slice(0, GRAPH_MAX); const ids = new Set(shown.map((n) => n.id));
      const es = edges.filter((e) => ids.has(e.from) && ids.has(e.to)).map((e) => ({ id: e.id, s: e.from, t: e.to, type: e.type || '—' }));
      const pos = U.layout(shown, es);
      return { nodes: shown.map((n) => ({ ...toCanvas(n), ...pos.get(n.id) })), edges: es, clipped: nodes.length - shown.length };
    };
    const nodeCell = (n) => ({ node: n ? toCanvas(n) : { id: '?', label: '(none)', caption: 'missing' } });
    const tableOfNodes = (nodes) => {
      const freq = new Map();
      nodes.forEach((n) => Object.entries(n.attr).forEach(([k, v]) => { const f = freq.get(k) || freq.set(k, { n: 0, kinds: {} }).get(k); f.n++; const kk = U.kindOf(v); f.kinds[kk] = (f.kinds[kk] || 0) + 1; }));
      const keys = [...freq.entries()].sort((a, b) => b[1].n - a[1].n || (a[0] < b[0] ? -1 : 1)).map(([k, f]) => {
        const kind = Object.entries(f.kinds).sort((a, b) => b[1] - a[1])[0][0];
        return { key: 'a.' + k, name: k, type: U.TYPE[kind], num: kind === 'Int' || kind === 'Float', attr: k };
      });
      return {
        columns: [{ key: 'id', name: 'node', type: 'NODE' }, { key: 'labels', name: 'labels', type: 'LIST' }].concat(keys),
        rows: nodes.map((n) => ({ id: n.id, node: n.id, cells: [nodeCell(n), { v: n.labels.map((l) => ':' + l).join('') }].concat(keys.map((c) => ({ v: n.attr[c.attr] === undefined ? '' : U.text(n.attr[c.attr]) }))) })),
      };
    };

    const execute = async (c, name, cursor) => {
      const t0 = performance.now();
      if (c.cmd === 'match') {
        const limit = c.opts.limit === undefined ? MATCH_LIMIT : c.opts.limit || 10000;
        const res = await src.matchPattern(name, c.pattern, { limit });
        const ids = [...new Set(res.rows.flatMap((r) => r.nodes))]; const eids = [...new Set(res.rows.flatMap((r) => r.edges.flat()))];
        const [nr, er] = await Promise.all([src.getNodes(name, ids), src.getEdges(name, eids)]);
        const nodes = nr.nodes.filter(Boolean); remember(nodes); const byId = new Map(nodes.map((n) => [n.id, n]));
        const nodeCols = res.columns.filter((x) => x.kind === 'node');
        const w = res.meta.work;
        return {
          kind: 'match', graph: graphOf(nodes, er.edges.filter(Boolean)),
          columns: res.columns.map((x) => ({ key: x.name, name: x.name, type: x.kind === 'node' ? 'NODE' : 'EDGE' })),
          rows: res.rows.map((r, k) => ({ id: 'm' + k, node: r.nodes[0], cells: r.nodes.map((id) => nodeCell(byId.get(id))).concat(r.edges.map((p) => ({ v: p.join(' → ') }))) })),
          footer: `${U.num(res.rows.length)} match${res.rows.length === 1 ? '' : 'es'}${res.meta.truncated ? ' (stopped at the limit)' : ''} · ${nodeCols.length} node and ${res.columns.length - nodeCols.length} edge columns · visited ${U.num(w.visited)} nodes, ${U.num(w.edges)} edges · seq ${U.num(res.meta.seq)} · ${U.ms(performance.now() - t0)}`,
          plan: null,
        };
      }
      if (c.cmd === 'find' || c.cmd === 'explain') {
        const limit = c.opts.limit || PAGE;
        const [res, ex] = await Promise.all([src.find(name, c.filter, { limit, cursor }), src.explain(name, c.filter, { analyze: true })]);
        remember(res.nodes);
        const sub = await src.subgraph(name, res.nodes.map((n) => n.id));
        const elapsed = performance.now() - t0; const t = tableOfNodes(res.nodes);
        // No read counts the matches (an unbounded count, design rule 5): on a first page with no more, the page is it
        const e = { ...ex.explain, matched: !cursor && !res.meta.next ? res.nodes.length : undefined };
        const counted = `${U.num(res.nodes.length)} node${res.nodes.length === 1 ? '' : 's'} on this page${res.meta.next ? ', more on the next' : ''}`;
        return {
          kind: 'find', filter: c.filter, limit, graph: graphOf(res.nodes, sub.edges), ...t, next: res.meta.next,
          footer: `${counted} · ${U.num(res.meta.work.visited)} candidates checked · seq ${U.num(res.meta.seq)} · ${U.ms(elapsed)}`,
          plan: Q.planRows(e, c.filter, elapsed),
          summary: [['EST. CANDIDATES', U.num(e.estimatedCandidates)], ['CANDIDATES', U.num(e.candidates)], ['MATCH', U.num(e.matched)], ['NODES', U.num(e.nodes)], ['PLAN', Object.keys(e.plan)[0].toUpperCase()]],
          scanPath: e.plan.scan ? Q.firstPath(c.filter) : null,
        };
      }
      if (c.cmd === 'node') {
        const res = await src.getNodes(name, c.ids); const nodes = res.nodes.filter(Boolean); remember(nodes);
        const missing = c.ids.filter((id, k) => !res.nodes[k]);
        const sub = await src.subgraph(name, nodes.map((n) => n.id));
        return { kind: 'node', graph: graphOf(nodes, sub.edges), ...tableOfNodes(nodes), plan: null, footer: `${nodes.length} found${missing.length ? ' · not found: ' + missing.join(', ') : ''} · seq ${U.num(res.meta.seq)}` };
      }
      const res = await src.neighbours(name, c.id, { limit: c.opts.limit || NEIGHBOURS * 4 });
      const self = (await src.getNodes(name, [c.id])).nodes[0]; const nodes = [self].concat(res.nodes); remember(nodes);
      return { kind: 'neighbours', graph: graphOf(nodes, res.edges), ...tableOfNodes(nodes), plan: null, footer: `${c.id} and ${res.nodes.length} neighbours${res.truncated ? ' (more beyond the limit)' : ''} · seq ${U.num(res.meta.seq)}` };
    };

    const go = (v) => {
      if (v === view) return;
      if (view === 'table' && v === 'graph' && !reduced()) { setMorph(true); setTimeout(() => { setMorph(false); setView('graph'); }, 220); } else setView(v);
    };

    const run = useCallback(async (text, o = {}) => {
      const name = o.ns || ns; if (!name) return 0;
      setQuery(text);
      let c;
      try { c = Q.parse(text); } catch (e) { setError(e); addLog('ERROR', errText(e)); throw e; }
      const t0 = performance.now();
      try {
        const r = await execute(c, name, o.cursor);
        if (o.page) Object.assign(r, { page: o.page, cursors: o.cursors }); else Object.assign(r, { page: 1, cursors: [''] });
        setResult(r); setError(null); setCanvasKey((k) => k + 1);
        if (o.view) setView(o.view); else if (c.cmd === 'explain') setView('plan'); else if (view === 'plan' && !r.plan) setView('table'); else if (view === 'structure') setView('graph');
        if (r.graph.clipped > 0) addLog('WARN', `the canvas shows the first ${GRAPH_MAX} of ${r.graph.nodes.length + r.graph.clipped} nodes; the table has them all`);
        return performance.now() - t0;
      } catch (e) { setError(e); addLog('ERROR', errText(e)); throw e; }
    }, [ns, view]);

    /* A namespace: its schema, then the first look */
    useEffect(() => {
      if (!ns) return;
      setSelId(null); setResult(null); setPending([]); cache.current.clear(); setRels(null);
      try { const u = new URL(window.location.href); u.searchParams.set('ns', ns); window.history.replaceState(null, '', u); } catch (_) { /* file:// in some browsers */ }
      refresh(ns).then(() => run(DEFAULT_QUERY, { ns, view: 'graph' }).catch(() => {}));
    }, [ns]);

    const switchNs = (name) => {
      if (name === ns) return;
      if (pending.length && !window.confirm(`Discard the ${pending.length} staged change${pending.length > 1 ? 's' : ''} in ${ns}?`)) return;
      setNs(name);
    };

    /* ---------------------------------------------------------------- selection and the inspector */
    useEffect(() => {
      if (!selId || !ns) { setRels(null); return; }
      if (!cache.current.has(selId)) src.getNodes(ns, [selId]).then((r) => remember(r.nodes.filter(Boolean)), () => {});
      let live = true;
      src.neighbours(ns, selId, { limit: 1000 }).then((r) => {
        if (!live) return; const by = new Map(r.nodes.map((n) => [n.id, n])); const g = new Map();
        r.edges.forEach((e) => { const out = e.from === selId; const other = by.get(out ? e.to : e.from); const lab = other ? U.primaryLabel(other, counts.current) : '?'; const k = e.type + '|' + (out ? 'out' : 'in') + '|' + lab; const x = g.get(k) || g.set(k, { type: e.type, dir: out ? 'out' : 'in', label: lab, count: 0 }).get(k); x.count++; });
        setRels({ id: selId, list: [...g.values()].sort((a, b) => b.count - a.count), degree: r.edges.length, more: r.truncated });
      }, () => {});
      return () => { live = false; };
    }, [selId, ns, status && status.seq]);
    const selNode = selId ? cache.current.get(selId) : null;
    const inspNode = useMemo(() => (selNode ? toCanvas(selNode) : null), [selNode, toCanvas]);
    const staged = useMemo(() => {
      const m = new Map(); pending.forEach((p) => { const b = p.m.setAttr; if (b && b.target.node != null) m.set(b.target.node + '\u0000' + b.key, b.value); }); return m;
    }, [pending]);
    const properties = useMemo(() => {
      if (!selNode) return null;
      const attr = { ...selNode.attr }; const dirty = new Set();
      staged.forEach((v, k) => { const [id, key] = k.split('\u0000'); if (id === selNode.id) { attr[key] = v; dirty.add(key); } });
      return Object.keys(attr).sort().map((k) => ({ key: k, type: U.TYPE[U.kindOf(attr[k])], value: U.text(attr[k]), dirty: dirty.has(k) }));
    }, [selNode, staged]);
    const storage = useMemo(() => selNode && [['labels', selNode.labels.map((l) => ':' + l).join('') || '—'], ['version', String(selNode.version)], ['degree', rels && rels.id === selNode.id ? U.num(rels.degree) + (rels.more ? '+' : '') : '…'], ['read at', 'seq ' + U.num(status ? status.seq : 0)]], [selNode, rels, status]);

    /* ---------------------------------------------------------------- staging and committing */
    const stage = (p) => setPending((ps) => {
      const b = p.m.setAttr;
      const rest = b ? ps.filter((x) => !(x.m.setAttr && x.m.setAttr.target.node === b.target.node && x.m.setAttr.key === b.key)) : ps;
      return rest.concat(p);
    });
    const onSave = (n, key, value) => {
      const node = cache.current.get(n.id); const old = node && node.attr[key];
      const v = U.parseValue(value, old === undefined ? null : U.kindOf(old));
      stage({ m: { setAttr: { target: { node: n.id }, key, value: v } }, desc: `set ${n.id}.${key} = ${U.text(v)}` });
      addLog('INFO', `staged · set ${n.id}.${key} = ${U.text(v)} (${U.TYPE[U.kindOf(v)]})`);
    };
    const typed = schema ? schema.types.filter((t) => t.name) : []; // untyped edges have no name to navigate by
    const connectType = typed.length ? typed.slice().sort((a, b) => b.count - a.count)[0].name : 'RELATES_TO';
    const onConnect = (e) => stage({ m: { addEdge: { from: e.s, to: e.t, type: e.type } }, tmp: e.id, desc: `add ${e.s} -[:${e.type}]-> ${e.t}` });
    const onDetach = (e) => setPending((ps) => (ps.some((p) => p.tmp === e.id) ? ps.filter((p) => p.tmp !== e.id) : ps.concat({ m: { deleteEdge: { id: e.id } }, desc: `delete edge ${e.id}` })));
    const discard = () => { if (!pending.length) return; setPending([]); setCanvasKey((k) => k + 1); addLog('INFO', `discarded ${pending.length} staged change${pending.length > 1 ? 's' : ''}`); };
    const commit = () => {
      if (!pending.length || busy) return;
      const list = pending; setBusy(true);
      src.commit(ns, list.map((p) => p.m)).then((res) => {
        setPending([]);
        const touched = [...new Set(list.flatMap((p) => { const b = p.m.setAttr || p.m.addEdge || {}; return [b.target && b.target.node, b.from, b.to].filter((x) => x != null); }))];
        return Promise.all([refresh(ns), touched.length ? src.getNodes(ns, touched).then((r) => remember(r.nodes.filter(Boolean))) : null]).then(() => addLog('INFO', `committed · seq ${U.num(res.seq)} · ${list.length} mutation${list.length > 1 ? 's' : ''}`));
      }, (e) => { setError(e); addLog('ERROR', 'commit refused · ' + errText(e)); }).finally(() => setBusy(false));
    };
    const createIndex = (path) => src.createIndex(ns, path).then(() => refresh(ns), (e) => { setError(e); addLog('ERROR', errText(e)); throw e; });

    /* ---------------------------------------------------------------- the canvas's neighbourhoods */
    const neighbours = useCallback((id) => src.neighbours(ns, id, { limit: NEIGHBOURS }).then((r) => {
      remember(r.nodes);
      return { nodes: r.nodes.map(toCanvas), edges: r.edges.map((e) => ({ id: e.id, s: e.from, t: e.to, type: e.type || '—' })), truncated: r.truncated };
    }), [ns, toCanvas]);

    /* ---------------------------------------------------------------- the schema navigator */
    const sections = useMemo(() => {
      const st = new Map(((server && server.namespaces) || []).map((s) => [s.name, s]));
      return [
        { id: 'ns', title: 'Namespaces', items: nss.map((x) => { const s = st.get(x.name); const bad = s && (s.readOnly || s.checkpointFailure); return { id: 'ns:' + x.name, name: x.name, glyph: 'db', active: x.name === ns, state: bad ? 'FAILED' : undefined, stateLabel: bad ? (s.readOnly ? '✕ READ-ONLY' : '✕ CHECKPOINT') : undefined, count: bad ? undefined : s && s.nodes }; }) },
        { id: 'labels', title: 'Labels', items: (schema ? schema.labels : []).map((l) => ({ id: 'lb:' + l.name, name: l.name, label: l.name, count: l.count })) },
        { id: 'types', title: 'Edge types', items: typed.map((t) => ({ id: 'rt:' + t.name, name: t.name, rel: true, count: t.count })) },
        { id: 'idx', title: 'Indexes', collapsed: true, items: (status ? status.indexes : []).map((x) => ({ id: 'ix:' + x.path.join('.'), name: '[' + x.path.join('.') + ']', meta: x.unique ? 'unique' : 'index', state: x.building ? 'POPULATING' : 'ONLINE', stateLabel: x.building ? U.pct(x.building.scanned / Math.max(1, x.building.total)) : x.unique ? 'UNIQUE' : 'READY' })) },
        { id: 'con', title: 'Constraints', collapsed: true, items: (schema ? schema.constraints : []).map((c, i) => ({ id: 'c:' + i, name: c.label + '.' + c.path.join('.'), meta: c.kind })) },
      ];
    }, [nss, ns, schema, status, server]);
    const browse = (label) => run(`find {"Label": "${label}"}`, { view: 'table' }).catch(() => {});
    const onNav = (it, now) => {
      clearTimeout(navT.current);
      const act = () => {
        const [k, ...rest] = it.id.split(':'); const v = rest.join(':');
        if (k === 'ns') switchNs(v);
        else if (k === 'lb') browse(v);
        else if (k === 'rt') run(`match (a)-[e:${v}]->(b)\n\\limit 60`, { view: 'graph' }).catch(() => {});
        else go('structure');
      };
      if (now) act(); else navT.current = setTimeout(act, 260); // arrows pass over items; a pause opens one
    };

    /* ---------------------------------------------------------------- keys */
    useEffect(() => {
      const k = (e) => {
        const mod = e.metaKey || e.ctrlKey; const key = e.key.toLowerCase();
        if (mod && key === 'k') { e.preventDefault(); setPalette({}); return; }
        if (mod && key === 'l') { e.preventDefault(); setQOpen(true); return; }
        if (mod && key === 'j') { e.preventDefault(); setLogOpen((o) => !o); return; }
        if (mod && key === 'i') { e.preventDefault(); setInstr((o) => !o); return; }
        if (mod && key === 's') { e.preventDefault(); commit(); return; }
        if (e.target.closest('input,textarea') || palette) return;
        if (e.key === '?') { e.preventDefault(); setSheet((s) => !s); }
        if (e.key === 'Escape' && !e.defaultPrevented && document.activeElement === document.body) setSelId(null);
        if (e.key === '1') go('graph'); if (e.key === '2') go('table'); if (e.key === '3') go('plan'); if (e.key === '4') go('structure');
      };
      window.addEventListener('keydown', k); return () => window.removeEventListener('keydown', k);
    });

    /* ---------------------------------------------------------------- the palette */
    const paletteItems = useMemo(() => {
      if (!palette) return [];
      const items = [];
      nss.forEach((x) => items.push({ group: 'Namespace', title: x.name, detail: x.name === ns ? 'current' : 'switch', mono: true, run: () => switchNs(x.name) }));
      (schema ? schema.labels : []).forEach((l) => items.push({ group: 'Label', title: l.name, detail: U.num(l.count) + ' nodes · browse', label: l.name, run: () => browse(l.name) }));
      typed.forEach((t) => items.push({ group: 'Edge type', title: t.name, detail: U.num(t.count) + ' edges', rel: true, mono: true, run: () => run(`match (a)-[e:${t.name}]->(b)\n\\limit 60`, { view: 'graph' }).catch(() => {}) }));
      cache.current.forEach((n) => items.push({ group: 'Node', title: U.caption(n), detail: n.id + ' · :' + n.labels.join(':'), label: U.primaryLabel(n, counts.current), run: () => { setSelId(n.id); } }));
      [['Graph', 'graph', '1'], ['Table', 'table', '2'], ['Plan', 'plan', '3'], ['Structure', 'structure', '4']].forEach(([t, v, key]) => items.push({ group: 'View', title: t, detail: key, run: () => go(v) }));
      if (pending.length) { items.push({ group: 'Command', title: 'Commit staged changes', detail: pending.length + ' · ⌘S', run: commit }); items.push({ group: 'Command', title: 'Discard staged changes', detail: String(pending.length), run: discard }); }
      return items.concat(U.commonItems('explore'));
    }, [palette, nss, ns, schema, pending]);
    const dynamic = useCallback((q) => (/^\S+$/.test(q) && !q.startsWith('{')
      ? [{ group: 'Look up', title: 'node ' + q, detail: 'run', mono: true, run: () => run('node ' + q, { view: 'graph' }).catch(() => {}) }, { group: 'Look up', title: 'neighbours ' + q, detail: 'run', mono: true, run: () => run('neighbours ' + q, { view: 'graph' }).catch(() => {}) }]
      : []), [run]);

    /* ---------------------------------------------------------------- render */
    const page = result && result.kind === 'find' ? result : null;
    const pager = page && {
      page: page.page, pages: page.page + (page.next ? 1 : 0),
      onPrev: () => { const cs = page.cursors; run(query, { cursor: cs[page.page - 2], page: page.page - 1, cursors: cs, view: 'table' }).catch(() => {}); },
      onNext: () => { if (!page.next) return; const cs = page.cursors.slice(0, page.page).concat(page.next); run(query, { cursor: page.next, page: page.page + 1, cursors: cs, view: 'table' }).catch(() => {}); },
    };
    const onExport = (kind) => {
      if (!result) return;
      const rows = result.rows.map((r) => Object.fromEntries(result.columns.map((c, i) => [c.name, r.cells[i] && (r.cells[i].node ? r.cells[i].node.id : r.cells[i].v)])));
      if (kind === 'json') download(`${ns}-${result.kind}.json`, 'application/json', JSON.stringify(rows, null, 2));
      else { const esc = (x) => (/[",\n]/.test(String(x ?? '')) ? '"' + String(x).replace(/"/g, '""') + '"' : String(x ?? '')); download(`${ns}-${result.kind}.csv`, 'text/csv', [result.columns.map((c) => esc(c.name)).join(',')].concat(rows.map((r) => result.columns.map((c) => esc(r[c.name])).join(','))).join('\n')); }
    };
    const rowSel = result && selId ? (result.rows.find((r) => r.id === selId) || result.rows.find((r) => r.node === selId) || {}).id : null;
    const mem = server && server.memory;
    const drawer = status && [
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'NODES'), h('span', { className: 'iw-mono-s' }, U.num(status.nodes))),
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'EDGES'), h('span', { className: 'iw-mono-s' }, U.num(status.edges))),
      mem && (U.memory(mem)
        ? h(I.Meter, { caption: 'MEMORY', value: mem.usedBytes, max: mem.limitBytes, warnAt: U.memory(mem).meterWarnAt, readout: U.bytes(mem.usedBytes) + ' / ' + U.bytes(mem.limitBytes) })
        : h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'MEMORY'), h('span', { className: 'iw-mono-s' }, U.bytes(mem.usedBytes)))),
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'SEQ'), h('span', { className: 'iw-mono-s' }, U.num(status.seq))),
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'UNSYNCED'), h('span', { className: 'iw-mono-s' }, status.unsynced == null ? 'fsync off' : U.num(status.unsynced))),
      h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'SINCE CHECKPOINT'), h('span', { className: 'iw-mono-s' }, U.num(status.sinceCheckpoint) + ' commits')),
      server && server.series.commitP99.length > 0 && h('span', { className: 'iw-rail__m' }, h('span', { className: 'iw-cap' }, 'COMMIT P99'), h('span', { className: 'iw-mono-s' }, U.ms(server.series.commitP99.slice(-1)[0]))),
    ].filter(Boolean);
    const serverOk = server && !server.error && U.problems(server).length === 0;
    const health = server ? (server.ready ? (serverOk ? 'ok' : 'warn') : 'danger') : 'off';
    const nsBad = status && (status.readOnly || status.checkpointFailure);
    const plan = result && result.plan;

    return h('div', { className: 'iw-wb cs-wb' + (selNode ? ' has-insp' : '') + (qOpen || logOpen ? ' has-bottom' : '') },
      h(I.StatusRail, {
        key: ns, db: ns || '…', tx: status ? status.seq : undefined, activity: busy, txOpen: pending.length, health: nsBad ? 'warn' : health,
        word: server ? (busy ? 'Writing' : !server.ready ? 'Not ready' : nsBad ? (status.readOnly ? 'Read-only' : 'Degraded') : serverOk ? 'Healthy' : 'Degraded') : 'Connecting',
        open: instr, onToggle: setInstr, onCommand: () => setPalette({}), onHelp: () => setSheet(true), onDb: () => setPalette({ initial: '' }),
        drawer, footer: server ? (src.kind === 'mock' ? `mock · v${server.version}` : `server ${src.endpoint}${server.version ? ' · v' + server.version : ''}`) : '',
      }, h(U.RailLinks, { page: 'explore' })),
      h('div', { className: 'iw-wb__nav' }, h(I.SchemaNavigator, { key: ns, sections, onSelect: (it) => onNav(it, false), onActivate: (it) => onNav(it, true) })),
      h('main', { className: 'iw-wb__main' },
        h('div', { className: 'iw-wb__bar cs-bar' },
          h(I.ViewSwitch, { value: morph ? 'graph' : view, onChange: go, items: [{ id: 'graph', label: 'GRAPH', key: '1' }, { id: 'table', label: 'TABLE', key: '2' }, { id: 'plan', label: 'PLAN', key: '3' }, { id: 'structure', label: 'STRUCTURE', key: '4' }] }),
          pending.length > 0 && h('div', { className: 'cs-staged', role: 'status' },
            h('span', { className: 'cs-staged__n', title: pending.map((p) => p.desc).join('\n') }, h('span', { className: 'iw-cap' }, 'STAGED'), h('span', { className: 'iw-mono-s' }, pending.length)),
            h(I.Button, { variant: 'ghost', onClick: discard, title: 'Discard the staged changes' }, 'DISCARD'),
            h(I.Button, { variant: 'primary', glyph: 'save', kbd: '⌘S', onClick: commit, disabled: busy, title: 'Commit' }, 'COMMIT'))),
        error && h('div', { className: 'cs-error', role: 'alert' },
          h('span', { className: 'iw-state is-failed' }, '✕ ' + String(error.code || 'error').toUpperCase().replace(/_/g, ' ')),
          h('span', { className: 'iw-mono-s cs-error__msg' }, error.message || String(error)),
          h(I.Button, { variant: 'ghost', iconOnly: true, title: 'Dismiss', 'aria-label': 'Dismiss', onClick: () => setError(null) }, '✕')),
        h('div', { className: 'iw-wb__view' },
          h('div', { className: 'iw-wb__pane', hidden: view !== 'graph' && !morph },
            result ? h(I.GraphCanvas, { key: ns + ':' + canvasKey, graph: result.graph, selected: selId, onSelect: (n) => setSelId(n ? n.id : null), onLog: addLog, onExpandRef: expandRef, neighbours, onConnect, onDetach, connectType, empty: result.graph.nodes.length === 0 })
              : h(I.EmptyState, { kind: 'canvas', title: 'Reading the namespace.', body: 'The first look is any 40 edges.' })),
          (view === 'table' || morph) && result && h(I.ResultTable, { key: 't' + canvasKey, columns: result.columns, rows: result.rows, morph, selected: rowSel, onSelect: (id) => { const r = result.rows.find((x) => x.id === id); r && r.node && setSelId(r.node); }, footer: result.footer, pager, onExport }),
          view === 'plan' && (plan
            ? h(I.PlanView, { plan, summary: result.summary, hint: result.scanPath && { label: `Create an index on [${result.scanPath.join('.')}]…`, onClick: () => { createIndex(result.scanPath).then(() => { addLog('INFO', `index [${result.scanPath.join('.')}] building; run the find again once it is ready`); go('structure'); }, () => {}); } } })
            : h(I.EmptyState, { kind: 'results', title: 'No plan for this one.', body: 'Plans are for filters: run find or explain. Patterns and lookups have none to show.' })),
          view === 'structure' && h(StructureView, { ns, schema, status, onBrowse: browse, onCreateIndex: createIndex }))),
      h('div', { className: 'iw-wb__insp' }, selNode && h(I.Inspector, {
        node: inspNode, properties, relationships: rels && rels.id === selNode.id ? rels.list : [], storage, readOnly: !!(status && status.readOnly),
        onClose: () => setSelId(null), onSave, onExpand: (id) => { go('graph'); expandRef.current && expandRef.current(id); },
      })),
      h('div', { className: 'iw-wb__bottom' + (logOpen ? ' has-log' : '') },
        h('div', { className: 'iw-wb__q' }, h(I.QueryConsole, {
          query, open: qOpen, onOpenChange: setQOpen, onRun: (text) => run(text), keywords: Q.KEYWORDS, comment: '--', minRunMs: 320,
          params: `${ns || ''} · read · limit ${PAGE} per page, ${MATCH_LIMIT} matches · match, find, explain, node, neighbours · \\limit n`,
        })),
        h('div', { className: 'iw-wb__log' }, h(I.LogTicker, { entries: log, open: logOpen, onOpen: () => setLogOpen((o) => !o) }), logOpen && h(I.LogStream, { entries: log }))),
      sheet && h(I.ShortcutSheet, { onClose: () => setSheet(false), extra: [['Console', [['4', 'Structure'], ['⌘S', 'Commit staged changes'], ['--', 'Comment in a query'], ['\\limit', 'Limit of the command']]]] }),
      palette && h(U.Palette, { items: paletteItems, dynamic, initial: palette.initial || '', onClose: () => setPalette(null) }));
  }

  ReactDOM.createRoot(document.getElementById('root')).render(h(IW.ui.AuthGate, null, h(Explorer)));
})();

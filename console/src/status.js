/* Ironweaver DB operator console: the status page (step 16a; on the server's status views since step 16c). The
 * server at one glance: health, the write and read figures of the last minute and a half, every namespace's
 * state, memory and disk, latency per operation, running requests (with cancel), change-stream readers, index
 * builds, and the log. Problems sit at the top of the rail's drawer and on their rows; nothing is hidden behind a
 * click. Reads IW.ui.source() only. */
(function () {
  'use strict';
  const R = window.React; const h = R.createElement;
  const { useState, useEffect, useRef, useMemo } = R;
  const I = window.IronWeaver; const U = window.IW.ui;
  const src = U.source();

  /* ------------------------------------------------------------------ a sparkline with a hover readout */
  function Spark({ values, format, tickMs, onHover }) {
    const ref = useRef(null); const [hi, setHi] = useState(null);
    const W = 160, H = 32, n = values.length;
    if (n < 2) return h('svg', { className: 'cs-spark', viewBox: `0 0 ${W} ${H}`, 'aria-hidden': true });
    const lo = Math.min(...values), top = Math.max(...values); const span = top - lo || 1;
    const x = (i) => (i / (n - 1)) * W; const y = (v) => H - 3 - ((v - lo) / span) * (H - 6);
    const d = values.map((v, i) => (i ? 'L' : 'M') + x(i).toFixed(1) + ' ' + y(v).toFixed(1)).join('');
    const move = (e) => {
      const r = ref.current.getBoundingClientRect(); const i = Math.max(0, Math.min(n - 1, Math.round(((e.clientX - r.left) / r.width) * (n - 1))));
      setHi(i); onHover && onHover(format(values[i]) + ' · ' + U.span(((n - 1 - i) * tickMs) / 1000) + ' ago');
    };
    const leave = () => { setHi(null); onHover && onHover(null); };
    return h('svg', { ref, className: 'cs-spark', viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none', onPointerMove: move, onPointerLeave: leave, role: 'img', 'aria-label': `last ${n} seconds, from ${format(lo)} to ${format(top)}` },
      h('rect', { className: 'cs-spark__hit', x: 0, y: 0, width: W, height: H }),
      h('path', { className: 'cs-spark__line', d, vectorEffect: 'non-scaling-stroke' }),
      hi == null ? h('circle', { className: 'cs-spark__now', cx: x(n - 1), cy: y(values[n - 1]), r: 2 })
        : h('line', { className: 'cs-spark__x', x1: x(hi), x2: x(hi), y1: 0, y2: H, vectorEffect: 'non-scaling-stroke' }));
  }

  /** A stat tile: an engraved caption, the current figure, the last minute and a half. */
  function Tile({ caption, values, format, tickMs, note, state }) {
    const [readout, setReadout] = useState(null);
    const last = values.length ? values[values.length - 1] : null;
    return h('section', { className: 'cs-tile' + (state ? ' is-' + state : '') },
      h('div', { className: 'cs-tile__head' }, h('span', { className: 'iw-cap' }, caption), state === 'warn' && h('span', { className: 'iw-state is-populating' }, '▲ HIGH')),
      h('div', { className: 'iw-metric' }, last == null ? '—' : format(last)),
      h(Spark, { values, format, tickMs, onHover: setReadout }),
      h('div', { className: 'iw-mono-s iw-muted cs-tile__note' }, readout || note));
  }

  const Section = ({ title, meta, className, children }) => h('section', { className: 'cs-sec ' + (className || '') },
    h('header', { className: 'cs-sec__head' }, h('h2', { className: 'iw-cap' }, title), meta != null && h('span', { className: 'iw-mono-s iw-muted' }, meta)),
    children);
  const Table = ({ cols, rows, empty }) => h('div', { className: 'iw-table cs-dense' }, h('div', { className: 'iw-table__scroll' }, h('table', null,
    h('thead', null, h('tr', null, cols.map(([name, numeric], i) => h('th', { key: i, className: numeric ? 'is-num' : undefined }, h('span', { className: 'cs-th iw-cap' }, name))))),
    h('tbody', null, rows.length ? rows : h('tr', null, h('td', { colSpan: cols.length, className: 'iw-small iw-muted' }, empty))))));
  const Num = (v) => h('td', { className: 'is-num iw-mono-s' }, v);
  const ago = (micros, now) => (micros ? U.span((now * 1000 - micros) / 1e6) + ' ago' : '—');

  function nsState(s) {
    if (s.readOnly) return ['failed', '✕ READ-ONLY', s.readOnly];
    if (s.checkpointFailure) return ['failed', '✕ CHECKPOINT FAILED', s.checkpointFailure];
    const b = s.indexes.find((x) => x.building);
    if (b) return ['populating', '▲ BUILDING [' + b.path.join('.') + ']', null];
    return ['online', 'ONLINE', null];
  }

  /* ------------------------------------------------------------------ the page */
  function Status() {
    const [s, setS] = useState(null); const [log, setLog] = useState([]);
    const [live, setLive] = useState(true); const [drawer, setDrawer] = useState(false); const [palette, setPalette] = useState(false); const [sheet, setSheet] = useState(false);
    const [now, setNow] = useState(Date.now());

    useEffect(() => {
      setLog(src.log());
      const off = src.onLog((e) => setLog((l) => l.concat({ ...e, fresh: true }).slice(-200)));
      return off;
    }, []);
    const logKind = useRef(src.logKind());
    useEffect(() => {
      const read = () => src.server().then((x) => {
        setS(x); setNow(Date.now());
        // The first poll that could read the server's log replaces this page's requests with it
        if (src.logKind() !== logKind.current) { logKind.current = src.logKind(); setLog(src.log()); }
      }, () => {});
      read(); if (!live) return undefined;
      const t = setInterval(() => { src.tick(); read(); }, src.kind === 'mock' ? 1000 : 3000);
      return () => clearInterval(t);
    }, [live]);
    useEffect(() => {
      const k = (e) => {
        const mod = e.metaKey || e.ctrlKey;
        if (mod && e.key.toLowerCase() === 'k') { e.preventDefault(); setPalette(true); return; }
        if (mod && e.key.toLowerCase() === 'i') { e.preventDefault(); setDrawer((o) => !o); return; }
        if (e.target.closest('input,textarea') || palette) return;
        if (e.key === '?') { e.preventDefault(); setSheet((x) => !x); }
        if (e.key === ' ' && document.activeElement === document.body) { e.preventDefault(); setLive((x) => !x); }
      };
      window.addEventListener('keydown', k); return () => window.removeEventListener('keydown', k);
    });

    const problems = useMemo(() => U.problems(s), [s]);
    const cancel = (id) => src.cancel(id).then(() => src.server().then(setS), () => src.server().then(setS, () => {}));

    if (!s) return h('div', { className: 'cs-status' }, h(I.EmptyState, { kind: 'log', title: 'Asking the server.', body: 'The status appears in a moment.' }));
    const sr = s.series; const tick = s.tickMs;
    const avg = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : 0);
    const maxOf = (a) => (a.length ? Math.max(...a) : 0);
    const limit = s.memory.limitBytes; const memF = limit ? s.memory.graphBytes / limit : null;
    const uptime = s.startedMicros ? (now * 1000 - s.startedMicros) / 1e6 : null;
    const word = !s.ready ? 'Shutting down' : problems.length ? 'Degraded' : 'Healthy';
    const p99max = Math.max(1e-9, ...s.operations.map((o) => o.p99Ms));
    const serverLog = src.logKind() === 'server';
    const items = [
      ...s.namespaces.map((n) => ({ group: 'Namespace', title: n.name, detail: 'explore', mono: true, run: () => { window.location.href = U.href('index.html', { ns: n.name }); } })),
      { group: 'Command', title: live ? 'Pause live updates' : 'Resume live updates', detail: 'Space', run: () => setLive((x) => !x) },
      ...U.commonItems('status'),
    ];

    return h('div', { className: 'cs-statuspage' },
      h(I.StatusRail, {
        db: 'server', health: !s.ready ? 'danger' : problems.length ? 'warn' : 'ok', word, open: drawer, onToggle: setDrawer, onCommand: () => setPalette(true), onHelp: () => setSheet(true),
        onDb: () => setPalette(true),
        drawer: problems.length ? [h('span', { className: 'iw-state is-failed' }, '✕ ' + problems.length + ' PROBLEM' + (problems.length > 1 ? 'S' : '')), h('span', { className: 'iw-small cs-problems' }, problems.join(' · '))] : [h('span', { className: 'iw-state is-online' }, 'NO PROBLEMS'), h('span', { className: 'iw-small iw-muted' }, 'Every namespace is writable and checkpointing.')],
        footer: src.kind === 'mock' ? `mock · v${s.version}` : `server ${src.endpoint}`,
      }, h(U.RailLinks, { page: 'status' })),
      h('main', { className: 'cs-status' },
        /* The server */
        h('section', { className: 'cs-sec cs-server' },
          h('div', { className: 'cs-server__id' },
            h(I.Led, { state: !s.ready ? 'danger' : problems.length ? 'warn' : 'ok' }, s.ready ? 'READY' : 'DRAINING'),
            h('span', { className: 'iw-title' }, word),
            h('span', { className: 'iw-mono-s iw-muted' }, [uptime != null && 'up ' + U.span(uptime), s.version && 'v' + s.version, s.fsync && 'fsync ' + s.fsync].filter(Boolean).join(' · '))),
          h('div', { className: 'cs-server__ends iw-mono-s' }, h('span', null, h('span', { className: 'iw-cap' }, 'SERVER'), ' ', src.endpoint)),
          h('div', { className: 'cs-server__live' },
            h(I.Led, { state: live ? 'ok' : 'off' }, live ? (src.kind === 'mock' ? 'LIVE · 1 s' : 'LIVE · 3 s') : 'PAUSED'),
            h(I.Button, { variant: 'ghost', kbd: 'Space', onClick: () => setLive((x) => !x), title: live ? 'Pause' : 'Resume' }, live ? 'PAUSE' : 'RESUME')),
          problems.length > 0 && h('ul', { className: 'cs-server__problems' }, problems.map((p) => h('li', { key: p }, h('span', { className: 'iw-state is-failed' }, '✕'), ' ', h('span', { className: 'iw-small' }, p))))),
        /* The last minute and a half */
        h('div', { className: 'cs-kpis' },
          h(Tile, { caption: 'COMMITS / S', values: sr.commitsPerSec, format: (v) => U.num(v), tickMs: tick, note: 'avg ' + U.num(avg(sr.commitsPerSec), 1) + ' · max ' + U.num(maxOf(sr.commitsPerSec)) }),
          h(Tile, { caption: 'COMMIT P99', values: sr.commitP99, format: U.ms, tickMs: tick, note: 'p50 ' + U.ms(sr.commitP50.slice(-1)[0]) + ' · max ' + U.ms(maxOf(sr.commitP99)), state: sr.commitP99.slice(-1)[0] > 20 ? 'warn' : null }),
          h(Tile, { caption: 'FSYNC P99', values: sr.fsyncP99, format: U.ms, tickMs: tick, note: 'max ' + U.ms(maxOf(sr.fsyncP99)) + ' · policy ' + s.fsync }),
          h(Tile, { caption: 'QUERY P99', values: sr.queryP99, format: U.ms, tickMs: tick, note: 'p50 ' + U.ms(sr.queryP50.slice(-1)[0]) }),
          h(Tile, { caption: 'ACTIVE REQUESTS', values: sr.active, format: (v) => U.num(v), tickMs: tick, note: U.num(s.requests.rejected) + ' rejected · ' + U.num(s.requests.timedOut) + ' timed out' }),
          h(Tile, { caption: 'GRAPH MEMORY', values: sr.graphBytes, format: U.bytes, tickMs: tick, note: limit ? U.pct(memF) + ' of ' + U.bytes(limit) : 'graphs and indexes · no limit', state: memF >= U.MEMORY_WARN ? 'warn' : null })),
        /* Namespaces */
        h(Section, { title: 'NAMESPACES', meta: s.namespaces.length + ' open', className: 'cs-ns' },
          h(Table, {
            cols: [['NAMESPACE'], ['STATE'], ['NODES', 1], ['EDGES', 1], ['MEMORY', 1], ['SEQ', 1], ['UNSYNCED', 1], ['SINCE CHECKPOINT', 1], ['LAST CHECKPOINT', 1], ['INDEXES', 1], ['MARK']],
            rows: s.namespaces.flatMap((n) => {
              const [cls, label, why] = nsState(n);
              const building = n.indexes.filter((x) => x.building).length;
              const row = h('tr', { key: n.name, className: cls === 'failed' ? 'is-problem' : undefined },
                h('td', null, h('a', { className: 'cs-link iw-mono', href: U.href('index.html', { ns: n.name }) }, n.name)),
                h('td', null, h('span', { className: 'iw-state is-' + cls }, label)),
                Num(U.num(n.nodes)), Num(U.num(n.edges)), Num(U.bytes(n.memoryBytes)), Num(U.num(n.seq)),
                Num(n.unsynced == null ? 'fsync off' : U.num(n.unsynced)),
                Num(U.num(n.sinceCheckpoint)),
                Num(ago(n.lastCheckpointMicros, now)),
                Num(U.num(n.indexes.length - building) + (building ? ' + ' + building + ' building' : '')),
                h('td', { className: 'iw-mono-s' }, n.marks.length ? n.marks.map((m) => m.name + ' @ ' + U.num(m.position)).join(', ') : '—'));
              return why ? [row, h('tr', { key: n.name + ':why', className: 'cs-why' }, h('td', null), h('td', { colSpan: 10, className: 'iw-small' }, why))] : [row];
            }),
            empty: 'No namespaces. An empty server, technically valid.',
          })),
        /* Operations */
        h(Section, { title: 'LATENCY BY OPERATION', meta: 'since start · from the histograms', className: 'cs-ops' },
          h(Table, {
            cols: [['OPERATION'], ['CALLS', 1], ['ERRORS', 1], ['P50', 1], ['P99', 1], ['']],
            rows: s.operations.slice().sort((a, b) => b.calls - a.calls).map((o) => h('tr', { key: o.operation },
              h('td', { className: 'iw-mono' }, o.operation), Num(U.num(o.calls)), Num(o.errors ? U.num(o.errors) : '—'), Num(U.ms(o.p50Ms)), Num(U.ms(o.p99Ms)),
              h('td', { className: 'cs-hbar' }, h('span', { style: { width: Math.max(1, (o.p99Ms / p99max) * 100) + '%' }, title: 'p99 ' + U.ms(o.p99Ms) })))),
            empty: 'No requests yet.',
          })),
        /* Resources */
        h(Section, { title: 'MEMORY AND DISK', className: 'cs-res' },
          h('div', { className: 'cs-res__body' },
            h('div', { className: 'cs-res__mem' },
              limit
                ? h(I.Meter, { caption: 'GRAPH MEMORY', value: s.memory.graphBytes, max: limit, warnAt: U.MEMORY_WARN, width: 220, readout: U.bytes(s.memory.graphBytes) + ' / ' + U.bytes(limit) })
                : h('div', null, h('div', { className: 'iw-cap' }, 'GRAPH MEMORY'), h('div', { className: 'iw-metric' }, U.bytes(s.memory.graphBytes))),
              h('div', { className: 'iw-small iw-muted' }, limit ? `The namespaces' graphs and indexes; the console warns at ${U.pct(U.MEMORY_WARN)} of the limit.` : 'The namespaces\' graphs and indexes. The server has no memory limit set.')),
            h('dl', { className: 'cs-dl' },
              [['WAL', U.bytes(s.disk.walBytes)], ['CHECKPOINTS', U.bytes(s.disk.checkpointBytes)], ['DISK FREE', U.bytes(s.disk.freeBytes)], ['REQUESTS', U.num(s.requests.total)], ['REJECTED', U.num(s.requests.rejected)], ['TIMED OUT', U.num(s.requests.timedOut)], ['CANCELLED', U.num(s.requests.cancelled)], ['DENIED', U.num(s.requests.denied)]]
                .map(([k, v]) => h('div', { key: k }, h('dt', { className: 'iw-cap' }, k), h('dd', { className: 'iw-mono' }, v)))),
            h('div', { className: 'cs-res__wal' }, h('span', { className: 'iw-cap' }, 'WAL SIZE'), h(Spark, { values: sr.walBytes, format: U.bytes, tickMs: tick }), h('span', { className: 'iw-small iw-muted' }, 'Drops at each checkpoint.')))),
        /* Active requests */
        h(Section, { title: 'ACTIVE REQUESTS', meta: String(s.active.length), className: 'cs-act' },
          h(Table, {
            cols: [['REQUEST'], ['OPERATION'], ['NAMESPACE'], ['USER'], ['CLIENT'], ['RUNNING', 1], ['']],
            rows: s.active.map((q) => h('tr', { key: q.id },
              h('td', { className: 'iw-mono-s' }, q.id), h('td', { className: 'iw-mono' }, q.operation), h('td', { className: 'iw-mono-s' }, q.namespace || '—'),
              h('td', { className: 'iw-mono-s' }, q.user), h('td', { className: 'iw-mono-s iw-muted' }, q.client || '—'),
              Num(U.ms(q.elapsedMicros / 1000)),
              h('td', { className: 'is-num' }, q.cancellable
                ? h(I.Button, { variant: 'ghost', onClick: () => cancel(q.id), title: 'Cancel request ' + q.id + ': its caller gets cancelled' }, 'CANCEL')
                : h('span', { className: 'iw-small iw-muted', title: 'A commit runs to its end: cancelling it would only make its outcome unknown' }, 'commit')))),
            empty: 'Nothing running. Suspiciously calm.',
          })),
        /* Change streams */
        h(Section, { title: 'CHANGE STREAM READERS', meta: s.consumers.length + ' · polled in the last minute', className: 'cs-con' },
          h(Table, {
            cols: [['USER'], ['NAMESPACE'], ['CLIENT'], ['NEXT SEQ', 1], ['BEHIND', 1], ['LAST POLL', 1], ['POLLS', 1]],
            rows: s.consumers.map((c) => h('tr', { key: c.namespace + '/' + c.user + '/' + c.client },
              h('td', { className: 'iw-mono' }, c.user), h('td', { className: 'iw-mono-s' }, c.namespace), h('td', { className: 'iw-mono-s iw-muted' }, c.client || '—'), Num(U.num(c.nextSeq)),
              h('td', { className: 'is-num iw-mono-s' + (c.lag > 500 ? ' iw-warn' : '') }, U.num(c.lag) + ' commits'),
              Num(ago(c.lastPollMicros, now)), Num(U.num(c.polls)))),
            empty: 'No one is following a change stream.',
          })),
        /* The log */
        h(Section, { title: serverLog ? 'LOG' : 'THIS PAGE\'S REQUESTS', meta: serverLog ? null : 'the server\'s log needs a server-wide admin', className: 'cs-logsec' }, h(I.LogStream, { entries: log, follow: live })),
        /* Index builds (managed jobs come with step 16f) */
        h(Section, { title: 'INDEX BUILDS', className: 'cs-jobs' }, h(Table, {
          cols: [['INDEX'], ['NAMESPACE'], ['PROGRESS']],
          rows: s.namespaces.flatMap((n) => n.indexes.filter((x) => x.building).map((x) => h('tr', { key: n.name + x.path.join('.') }, h('td', { className: 'iw-mono' }, '[' + x.path.join('.') + ']'), h('td', { className: 'iw-mono-s' }, n.name), h('td', null, h(I.Meter, { value: x.building.scanned, max: Math.max(1, x.building.total), warnAt: 2, width: 96, readout: U.num(x.building.scanned) + ' / ' + U.num(x.building.total) }))))),
          empty: 'No index is being built.',
        }))),
      sheet && h(I.ShortcutSheet, { onClose: () => setSheet(false), extra: [['Status', [['Space', 'Pause or resume'], ['⌘I', 'Problems in the rail']]]] }),
      palette && h(U.Palette, { items, onClose: () => setPalette(false) }));
  }

  ReactDOM.createRoot(document.getElementById('root')).render(h(IW.ui.AuthGate, null, h(Status)));
})();

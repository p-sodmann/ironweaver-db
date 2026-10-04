/* Ironweaver DB operator console: the status page (step 16a). The server at one glance: health, the write
 * and read figures of the last minute and a half, every namespace's state, memory and disk, latency per
 * operation, active requests, change-stream consumers, jobs and index builds, and the log. Problems sit at the
 * top of the rail's drawer and on their rows; nothing is hidden behind a click. Reads IW.ui.source() only. */
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
  /** A section the server can't fill yet: what it will show, and which step brings it. */
  const Later = ({ title, className, what }) => h(Section, { title, className },
    h('p', { className: 'iw-small iw-muted cs-later' }, what + ' The server reports this once step 16 adds its status views and metrics.'));

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
    useEffect(() => {
      const read = () => src.server().then((x) => { setS(x); setNow(Date.now()); }, () => {});
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

    const problems = useMemo(() => {
      if (!s) return [];
      const out = [];
      if (!s.ready) out.push('recovery is running: the server is not ready');
      s.namespaces.forEach((n) => { if (n.readOnly) out.push(`${n.name} is read-only: ${n.readOnly}`); if (n.checkpointFailure) out.push(`${n.name}: the last checkpoint failed: ${n.checkpointFailure}`); });
      const f = s.memory ? s.memory.usedBytes / s.memory.limitBytes : 0;
      if (s.memory && f >= s.memory.warnAt) out.push(`memory at ${U.pct(f)} of the limit: writes are refused at ${U.pct(s.memory.refuseWritesAt)}`);
      return out;
    }, [s]);

    if (!s) return h('div', { className: 'cs-status' }, h(I.EmptyState, { kind: 'log', title: 'Asking the server.', body: 'The status appears in a moment.' }));
    const sr = s.series; const tick = s.tickMs;
    const avg = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : 0);
    const maxOf = (a) => (a.length ? Math.max(...a) : 0);
    const memF = s.memory ? s.memory.usedBytes / s.memory.limitBytes : 0;
    const uptime = s.startedMicros ? (now * 1000 - s.startedMicros) / 1e6 : null;
    const word = !s.ready ? 'Recovering' : problems.length ? 'Degraded' : 'Healthy';
    const p99max = s.operations ? Math.max(...s.operations.map((o) => o.p99)) : 1;
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
        footer: src.kind === 'mock' ? `mock · v${s.version}` : `server ${s.endpoints.rest}`,
      }, h(U.RailLinks, { page: 'status' })),
      h('main', { className: 'cs-status' },
        /* The server */
        h('section', { className: 'cs-sec cs-server' },
          h('div', { className: 'cs-server__id' },
            h(I.Led, { state: !s.ready ? 'danger' : problems.length ? 'warn' : 'ok' }, s.ready ? 'READY' : 'RECOVERING'),
            h('span', { className: 'iw-title' }, word),
            h('span', { className: 'iw-mono-s iw-muted' }, [uptime != null && 'up ' + U.span(uptime), s.version && (src.kind === 'mock' ? 'v' : 'API ') + s.version, s.fsync && 'fsync ' + s.fsync, s.dataDir].filter(Boolean).join(' · ') || 'reachable')),
          h('div', { className: 'cs-server__ends iw-mono-s' }, Object.entries(s.endpoints).map(([k, v]) => h('span', { key: k }, h('span', { className: 'iw-cap' }, k.toUpperCase()), ' ', v))),
          h('div', { className: 'cs-server__live' },
            h(I.Led, { state: live ? 'ok' : 'off' }, live ? (src.kind === 'mock' ? 'LIVE · 1 s' : 'LIVE · 3 s') : 'PAUSED'),
            h(I.Button, { variant: 'ghost', kbd: 'Space', onClick: () => setLive((x) => !x), title: live ? 'Pause' : 'Resume' }, live ? 'PAUSE' : 'RESUME')),
          problems.length > 0 && h('ul', { className: 'cs-server__problems' }, problems.map((p) => h('li', { key: p }, h('span', { className: 'iw-state is-failed' }, '✕'), ' ', h('span', { className: 'iw-small' }, p))))),
        /* The last minute and a half */
        !s.series ? h(Later, { title: 'THE LAST MINUTE AND A HALF', className: 'cs-kpisec', what: 'Commits per second, commit and fsync p99, query p99, active requests and memory.' }) : h('div', { className: 'cs-kpis' },
          h(Tile, { caption: 'COMMITS / S', values: sr.commitsPerSec, format: (v) => U.num(v), tickMs: tick, note: 'avg ' + U.num(avg(sr.commitsPerSec), 1) + ' · max ' + U.num(maxOf(sr.commitsPerSec)) }),
          h(Tile, { caption: 'COMMIT P99', values: sr.commitP99, format: U.ms, tickMs: tick, note: 'p50 ' + U.ms(sr.commitP50.slice(-1)[0]) + ' · max ' + U.ms(maxOf(sr.commitP99)), state: sr.commitP99.slice(-1)[0] > 20 ? 'warn' : null }),
          h(Tile, { caption: 'FSYNC P99', values: sr.fsyncP99, format: U.ms, tickMs: tick, note: 'max ' + U.ms(maxOf(sr.fsyncP99)) + ' · policy ' + s.fsync }),
          h(Tile, { caption: 'QUERY P99', values: sr.queryP99, format: U.ms, tickMs: tick, note: 'p50 ' + U.ms(sr.queryP50.slice(-1)[0]) }),
          h(Tile, { caption: 'ACTIVE REQUESTS', values: sr.active, format: (v) => U.num(v), tickMs: tick, note: U.num(s.requests.rejected) + ' rejected · ' + U.num(s.requests.timedOut) + ' timed out' }),
          h(Tile, { caption: 'MEMORY', values: sr.memory, format: U.bytes, tickMs: tick, note: U.pct(memF) + ' of ' + U.bytes(s.memory.limitBytes), state: memF >= s.memory.warnAt ? 'warn' : null })),
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
                Num(n.syncedSeq == null ? 'fsync off' : U.num(n.seq - n.syncedSeq)),
                Num(n.checkpoint == null ? '—' : U.num(n.seq - n.checkpoint)),
                Num(n.lastCheckpointMs ? U.ms(n.lastCheckpointMs) : '—'),
                Num(U.num(n.indexes.length - building) + (building ? ' + ' + building + ' building' : '')),
                h('td', { className: 'iw-mono-s' }, n.marks.length ? n.marks.map((m) => m.name + ' @ ' + U.num(m.position)).join(', ') : '—'));
              return why ? [row, h('tr', { key: n.name + ':why', className: 'cs-why' }, h('td', null), h('td', { colSpan: 10, className: 'iw-small' }, why))] : [row];
            }),
            empty: 'No namespaces. An empty server, technically valid.',
          })),
        /* Operations */
        !s.operations ? h(Later, { title: 'LATENCY BY OPERATION', className: 'cs-ops', what: 'Calls, errors, p50 and p99 per operation.' }) : h(Section, { title: 'LATENCY BY OPERATION', meta: 'since start', className: 'cs-ops' },
          h(Table, {
            cols: [['OPERATION'], ['CALLS', 1], ['ERRORS', 1], ['P50', 1], ['P99', 1], ['']],
            rows: s.operations.slice().sort((a, b) => b.calls - a.calls).map((o) => h('tr', { key: o.op },
              h('td', { className: 'iw-mono' }, o.op), Num(U.num(o.calls)), Num(o.errors ? U.num(o.errors) : '—'), Num(U.ms(o.p50)), Num(U.ms(o.p99)),
              h('td', { className: 'cs-hbar' }, h('span', { style: { width: Math.max(1, (o.p99 / p99max) * 100) + '%' }, title: 'p99 ' + U.ms(o.p99) })))),
            empty: '',
          })),
        /* Resources */
        !s.memory ? h(Later, { title: 'MEMORY AND DISK', className: 'cs-res', what: 'Memory against the limit, WAL, checkpoints and free disk, rejected and timed-out requests.' }) : h(Section, { title: 'MEMORY AND DISK', className: 'cs-res' },
          h('div', { className: 'cs-res__body' },
            h('div', { className: 'cs-res__mem' },
              h(I.Meter, { caption: 'MEMORY', value: s.memory.usedBytes, max: s.memory.limitBytes, warnAt: s.memory.warnAt, width: 220, readout: U.bytes(s.memory.usedBytes) + ' / ' + U.bytes(s.memory.limitBytes) }),
              h('div', { className: 'iw-small iw-muted' }, `Alerts at ${U.pct(s.memory.warnAt)}; writes are refused at ${U.pct(s.memory.refuseWritesAt)}, before the OS would end the process.`)),
            h('dl', { className: 'cs-dl' },
              [['WAL', U.bytes(s.disk.walBytes)], ['CHECKPOINTS', U.bytes(s.disk.checkpointBytes)], ['DISK FREE', U.bytes(s.disk.freeBytes)], ['REJECTED', U.num(s.requests.rejected)], ['TIMED OUT', U.num(s.requests.timedOut)], ['CANCELLED', U.num(s.requests.cancelled)]]
                .map(([k, v]) => h('div', { key: k }, h('dt', { className: 'iw-cap' }, k), h('dd', { className: 'iw-mono' }, v)))),
            h('div', { className: 'cs-res__wal' }, h('span', { className: 'iw-cap' }, 'WAL SIZE'), h(Spark, { values: sr.walBytes, format: U.bytes, tickMs: tick }), h('span', { className: 'iw-small iw-muted' }, 'Drops at each checkpoint.')))),
        /* Active requests */
        !s.active ? h(Later, { title: 'ACTIVE REQUESTS', className: 'cs-act', what: 'The requests running now, with cancel.' }) : h(Section, { title: 'ACTIVE REQUESTS', meta: String(s.active.length), className: 'cs-act' },
          h(Table, {
            cols: [['REQUEST'], ['OPERATION'], ['NAMESPACE'], ['CLIENT'], ['RUNNING', 1], ['VISITED', 1], ['']],
            rows: s.active.map((q) => h('tr', { key: q.id },
              h('td', { className: 'iw-mono-s' }, q.id), h('td', { className: 'iw-mono' }, q.op), h('td', { className: 'iw-mono-s' }, q.namespace), h('td', { className: 'iw-mono-s iw-muted' }, q.client),
              Num(U.ms(q.elapsedMs)), Num(U.num(q.visited)),
              h('td', { className: 'is-num' }, h(I.Button, { variant: 'ghost', onClick: () => src.cancel(q.id).then(() => src.server().then(setS)), title: 'Cancel ' + q.id }, 'CANCEL')))),
            empty: 'Nothing running. Suspiciously calm.',
          })),
        /* Change streams */
        !s.consumers ? h(Later, { title: 'CHANGE STREAM CONSUMERS', className: 'cs-con', what: 'Who follows a change stream, and how far behind.' }) : h(Section, { title: 'CHANGE STREAM CONSUMERS', meta: String(s.consumers.length), className: 'cs-con' },
          h(Table, {
            cols: [['CONSUMER'], ['NAMESPACE'], ['CLIENT'], ['AT SEQ', 1], ['BEHIND', 1]],
            rows: s.consumers.map((c) => h('tr', { key: c.name },
              h('td', { className: 'iw-mono' }, c.name), h('td', { className: 'iw-mono-s' }, c.namespace), h('td', { className: 'iw-mono-s iw-muted' }, c.client), Num(U.num(c.atSeq)),
              h('td', { className: 'is-num iw-mono-s' + (c.lag > 500 ? ' iw-warn' : '') }, U.num(c.lag) + ' commits'))),
            empty: 'No one is following a change stream.',
          })),
        /* The log */
        h(Section, { title: src.kind === 'mock' ? 'LOG' : 'THIS PAGE\'S REQUESTS', meta: src.kind === 'mock' ? null : 'the server log comes with step 16', className: 'cs-logsec' }, h(I.LogStream, { entries: log, follow: live })),
        /* Jobs and builds */
        !s.jobs ? h(Section, { title: 'INDEX BUILDS', className: 'cs-jobs' }, h(Table, {
          cols: [['INDEX'], ['NAMESPACE'], ['PROGRESS']],
          rows: s.namespaces.flatMap((n) => n.indexes.filter((x) => x.building).map((x) => h('tr', { key: n.name + x.path.join('.') }, h('td', { className: 'iw-mono' }, '[' + x.path.join('.') + ']'), h('td', { className: 'iw-mono-s' }, n.name), h('td', null, h(I.Meter, { value: x.building.scanned, max: Math.max(1, x.building.total), warnAt: 2, width: 96, readout: U.num(x.building.scanned) + ' / ' + U.num(x.building.total) }))))),
          empty: 'No index is being built. Analytics jobs are listed once step 16 manages them.',
        })) : h(Section, { title: 'JOBS AND INDEX BUILDS', className: 'cs-jobs' },
          h(Table, {
            cols: [['WHAT'], ['NAMESPACE'], ['STATE'], ['PROGRESS']],
            rows: s.jobs.map((j) => h('tr', { key: j.id },
              h('td', { className: 'iw-mono' }, j.kind + ' ', h('span', { className: 'iw-muted iw-mono-s' }, j.id)), h('td', { className: 'iw-mono-s' }, j.namespace),
              h('td', null, h('span', { className: 'iw-state ' + (j.state === 'running' ? 'is-populating' : 'is-online') }, j.state.toUpperCase())),
              h('td', null, h(I.Meter, { value: j.progress, max: 1, warnAt: 2, width: 96, readout: U.pct(j.progress) }))))
              .concat(s.namespaces.flatMap((n) => n.indexes.filter((x) => x.building).map((x) => h('tr', { key: n.name + x.path.join('.') },
                h('td', { className: 'iw-mono' }, 'index [' + x.path.join('.') + ']'), h('td', { className: 'iw-mono-s' }, n.name),
                h('td', null, h('span', { className: 'iw-state is-populating' }, 'BUILDING')),
                h('td', null, h(I.Meter, { value: x.building.scanned, max: Math.max(1, x.building.total), warnAt: 2, width: 96, readout: U.num(x.building.scanned) + ' / ' + U.num(x.building.total) })))))),
            empty: 'No jobs, no builds.',
          }))),
      sheet && h(I.ShortcutSheet, { onClose: () => setSheet(false), extra: [['Status', [['Space', 'Pause or resume'], ['⌘I', 'Problems in the rail']]]] }),
      palette && h(U.Palette, { items, onClose: () => setPalette(false) }));
  }

  ReactDOM.createRoot(document.getElementById('root')).render(h(IW.ui.AuthGate, null, h(Status)));
})();

/* Ironweaver DB operator console: the query line (step 16a). The console runs a subset of `iwctl shell`'s
 * commands (documentation/iwctl.md, ADR 0036), so what works here works there:
 *
 *   match <pattern>        the core's pattern text: (a:Person {age: 30})-[k:KNOWS*1..2]->(b)
 *   find <filter>          the core's filter JSON: {"Label": "Person"}
 *   explain <filter>       how find reads the filter
 *   node <id>...           lookups
 *   neighbours <id>        a node and its neighbours (the console's own; the shell has no such command)
 *   \limit <n>|off         on a line of its own: the limit of the command
 *   -- a comment
 *
 * A command may span lines (a long filter); the lines are joined with spaces.
 * Classic script: defines globalThis.IW.query (and module.exports for node --test). */
(function (root) {
  'use strict';

  class QueryError extends Error { constructor(code, message) { super(message); this.code = code; } }
  const bad = (m) => new QueryError('invalid_argument', m);
  const COMMANDS = ['match', 'find', 'explain', 'node', 'neighbours'];
  const KEYWORDS = /^(match|find|explain|node|neighbours|neighbors)$/;

  function parse(text) {
    const opts = {}; const lines = [];
    for (const raw of String(text).split('\n')) {
      const l = raw.trim();
      if (!l || l.startsWith('--')) continue;
      if (l.startsWith('\\')) {
        const m = /^\\limit\s+(\d+|off)$/.exec(l);
        if (!m) throw bad(`unknown option ${l.split(/\s/)[0]}; the console knows \\limit <n>|off`);
        opts.limit = m[1] === 'off' ? null : parseInt(m[1], 10);
        if (opts.limit === 0) throw bad('\\limit takes a number above 0, or off');
        continue;
      }
      lines.push(l);
    }
    if (!lines.length) throw bad(`nothing to run: write a command (${COMMANDS.join(', ')})`);
    const src = lines.join(' ');
    const m = /^(\S+)\s*([\s\S]*)$/.exec(src);
    const cmd = m[1].toLowerCase(); const arg = m[2].trim();
    switch (cmd) {
      case 'match':
        if (!arg) throw bad('match takes a pattern, like (a:Person)-[k:KNOWS]->(b)');
        return { cmd, pattern: arg, opts };
      case 'find': case 'explain': {
        if (!arg) throw bad(`${cmd} takes a filter in the core's JSON form, like {"Label": "Person"}`);
        let filter;
        try { filter = JSON.parse(arg); } catch (e) { throw bad(`${cmd} takes a filter in the core's JSON form, like {"Label": "Person"} (${e.message})`); }
        return { cmd, filter, opts };
      }
      case 'node': {
        const ids = arg.split(/\s+/).filter(Boolean);
        if (!ids.length) throw bad('node takes one or more ids');
        return { cmd, ids, opts };
      }
      case 'neighbours': case 'neighbors': {
        const ids = arg.split(/\s+/).filter(Boolean);
        if (ids.length !== 1) throw bad('neighbours takes one node id');
        return { cmd: 'neighbours', id: ids[0], opts };
      }
      default:
        throw bad(`unknown command ${JSON.stringify(cmd)}; the console runs ${COMMANDS.join(', ')}`);
    }
  }

  /** A short text of a filter, for plan rows and the log. */
  function filterText(f) {
    const k = Object.keys(f)[0], b = f[k];
    const v = (x) => { const kk = Object.keys(x)[0]; return kk === 'String' ? JSON.stringify(x[kk]) : x === 'None' ? 'None' : String(x[kk]); };
    switch (k) {
      case 'Label': return ':' + b;
      case 'Type': return 'type ' + b;
      case 'Const': return String(b);
      case 'Exists': return 'exists ' + b.path.join('.');
      case 'In': return b.path.join('.') + ' in [' + b.values.map(v).join(', ') + ']';
      case 'Compare': return b.path.join('.') + ' ' + { Eq: '=', Ne: '≠', Lt: '<', Le: '≤', Gt: '>', Ge: '≥' }[b.op] + ' ' + v(b.value);
      case 'And': return b.map(filterText).join(' and ');
      case 'Or': return '(' + b.map(filterText).join(' or ') + ')';
      case 'Not': return 'not ' + filterText(b);
    }
    return JSON.stringify(f);
  }

  /** The first attribute path a filter compares (where an index would help). */
  function firstPath(f) {
    const k = Object.keys(f)[0], b = f[k];
    if (b && Array.isArray(b.path)) return b.path;
    if (Array.isArray(b)) { for (const x of b) { const p = firstPath(x); if (p) return p; } }
    if (k === 'Not') return firstPath(b);
    return null;
  }

  /** An explain answer as the plan view's rows: Find, then the filter check, then how candidates are read. */
  function planRows(explain, filter, elapsedMs) {
    const rows = []; let id = 0;
    const est = explain.estimatedCandidates; const cands = explain.candidates; const matched = explain.matched;
    rows.push({ id: ++id, depth: 0, op: 'Find', detail: filterText(filter), est, rows: matched ?? 0, hits: 0, ms: elapsedMs || 0 });
    rows.push({ id: ++id, depth: 1, op: 'Filter', detail: 'every candidate is checked', est, rows: matched ?? 0, hits: cands ?? est, ms: 0 });
    const walk = (p, depth) => {
      const k = Object.keys(p)[0], b = p[k];
      if (k === 'empty') rows.push({ id: ++id, depth, op: 'Empty', detail: 'the filter is false: nothing to read', est: 0, rows: 0, hits: 0, ms: 0 });
      else if (k === 'label') rows.push({ id: ++id, depth, op: 'LabelIndex', detail: ':' + b.label, est, rows: cands ?? est, hits: cands ?? est, ms: 0 });
      else if (k === 'index') rows.push({ id: ++id, depth, op: 'PropertyIndex', detail: '[' + b.path.join('.') + '] ' + b.op, est, rows: cands ?? est, hits: cands ?? est, ms: 0 });
      else if (k === 'union') { rows.push({ id: ++id, depth, op: 'Union', detail: b.plans.length + ' plans', est, rows: cands ?? est, hits: 0, ms: 0 }); b.plans.forEach((x) => walk(x, depth + 1)); }
      else if (k === 'scan') {
        const path = firstPath(filter);
        rows.push({ id: ++id, depth, op: 'Scan', detail: 'every node', est: explain.nodes, rows: explain.nodes, hits: explain.nodes, ms: 0, hot: true, warn: path ? `Every node is read: no index serves [${path.join('.')}]` : 'Every node is read: no index serves this filter' });
      } else rows.push({ id: ++id, depth, op: 'Other', detail: String(b), est, rows: cands ?? est, hits: 0, ms: 0 });
    };
    walk(explain.plan, 2);
    (explain.building || []).forEach((p) => { rows[rows.length - 1].warn = `An index on [${p.join('.')}] is being built: reads don't use it yet`; rows[rows.length - 1].hot = true; });
    return rows;
  }

  const api = { parse, filterText, firstPath, planRows, KEYWORDS, QueryError };
  root.IW = root.IW || {}; root.IW.query = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof globalThis !== 'undefined' ? globalThis : this);

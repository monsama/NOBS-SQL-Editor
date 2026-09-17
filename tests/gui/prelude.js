// Helpers every GUI scenario runs with, evaluated in the app's page before the first scenario.
// window.GUI_ENV is set by run.mjs: { edition: 'ps' | 'desktop', tmp: <folder>, dbPort }.
(() => {
  const G = {};
  G.env = window.GUI_ENV;
  G.desktop = G.env.edition === 'desktop';
  G.wait = ms => new Promise(r => setTimeout(r, ms));
  G.until = async (f, ms = 20000) => { for (let i = 0; i < ms / 100; i++) { if (f()) return true; await G.wait(100); } return false; };
  G.A = window._guiApi || api; window._guiApi = G.A;

  // What the app told the user, collected rather than shown.
  G.toasts = []; G.logs = [];
  if (!window._guiToast) window._guiToast = toast;
  toast = (m, e) => { G.toasts.push(((e === true || e === 'err') ? 'ERR ' : '') + String(m)); return window._guiToast(m, e); };
  if (!window._guiLog) window._guiLog = log;
  log = m => { G.logs.push(String(m)); return window._guiLog(m); };
  ask = async () => true;
  G.take = () => { const t = G.toasts.splice(0), l = G.logs.splice(0); return { t, l }; };
  G.errs = () => G.toasts.filter(t => t.startsWith('ERR '));

  // Queries straight to the server, bypassing the UI.
  G.q = async (sql, db) => { const r = await G.A('/api/query', { sql, db }); if (!r.ok) throw new Error(r.error); return r.rows; };
  G.one = async (sql, db) => { const r = await G.q(sql, db); return r[0] ? r[0][0] : null; };
  G.run = async (sql, db) => { const r = await G.A('/api/script', { sql, db }); if (!r.ok) throw new Error(r.error); return r; };

  // Checks. A scenario records them and ends with `return G.report()`.
  G.checks = [];
  G.check = (name, ok, detail) => { G.checks.push({ name, ok: !!ok, detail: ok ? undefined : (typeof detail === 'string' ? detail : JSON.stringify(detail)) }); return !!ok; };
  G.eq = (name, got, want) => G.check(name, JSON.stringify(got) === JSON.stringify(want), { got, want });
  G.skip = (name, why) => G.checks.push({ name, ok: true, skipped: why });
  G.report = () => G.checks.splice(0);

  // Tables open in tabs; waits until the grid has loaded and the run has finished.
  G.openTable = async (db, table) => {
    objOpen(db, 'table', table);
    const t = tabs[tabs.length - 1];
    await G.until(() => t.rows && t.pending !== undefined && !t.runningReqId);
    await G.wait(300);
    return t;
  };
  G.runIn = async (sql, db) => {
    curSchema = db;
    const id = openTab('gui', sql, db, false, null);
    await runSql(id, $('ed_' + id).value);
    return T(id);
  };
  // The download a function would start, instead of starting it.
  G.captureDownload = async fn => {
    let text = null; const real = dl; dl = async t => { text = t; };
    try { await fn(); } finally { dl = real; }
    return text;
  };
  // Hex is shown in upper case by one edition and lower case by the other.
  G.hex = v => typeof v === 'string' && /^0x[0-9a-f]*$/i.test(v) ? '0x' + v.slice(2).toUpperCase() : v;
  G.rowsOf = t => (t.rows || []).map(r => r.map(v => v === null ? '<NULL>' : G.hex(v)).join('|'));
  window.G = G;
  return 'ready';
})()

// Two runs on one tab: the newest one owns the grid, whatever order they finish in.
//
// runSql used to replace the tab's abort controller for a new run without aborting the old one, and
// on the way back it asked only whether the tab still existed - never whether a newer run had
// started since. So both finished and the slower one wrote the rows, the columns, the table binding
// and the paging cursor. The tab then showed the result of a query nobody was looking at, and an
// edit saved from it was aimed by that stale binding (issue #97).
//
// It was not reachable through the Run button, which turns into Cancel while a query runs - it was
// reachable through everything else that calls runSql: opening a table from the tree, a refresh
// after Apply, the character-set switch. That last one is how it was found, in this suite, as an
// intermittent failure that looked like a grid refusing to come back.
//
// The fixture is the shape of the bug rather than a story: a query that takes three seconds, then
// immediately one that takes none. Before the fix the slow one landed last and won.
(async () => {
  const DB = 'nobs_gui_race';
  try {
    await G.run(`DROP DATABASE IF EXISTS ${DB}; CREATE DATABASE ${DB};
CREATE TABLE ${DB}.t (id INT PRIMARY KEY, v VARCHAR(10));
INSERT INTO ${DB}.t VALUES (1,'one');`);

    const tab = await G.openTable(DB, 't');
    const id = tab.id;

    // Started without awaiting, which is what every caller in the app does.
    runSql(id, `SELECT SLEEP(3) AS slept, 'slow' AS which`);
    await G.wait(300);
    runSql(id, `SELECT 'fast' AS which`);

    // Long enough that the slow one has certainly come back too.
    await G.until(() => { const t = T(id); return t && !t.runningReqId && t.cols && t.cols.length === 1; }, 20000);
    await G.wait(4000);

    const t = T(id);
    G.eq('the grid holds the newest run, not the one that finished last',
      [t.cols, G.rowsOf(t)], [['which'], ['fast']]);
    G.check('and the older run left no state behind it', !t.runningReqId && !t.cursorId,
      { running: t.runningReqId, cursor: t.cursorId });

    // The same in the other order: the slow one arriving second must not be discarded, since then
    // it is the newest. A guard that simply ignored late answers would pass the check above and
    // fail this one.
    G.take();
    runSql(id, `SELECT 'first' AS which`);
    await G.wait(300);
    runSql(id, `SELECT SLEEP(2) AS slept, 'second' AS which`);
    await G.until(() => { const x = T(id); return x && !x.runningReqId && x.cols && x.cols.length === 2; }, 20000);
    const y = T(id), told = G.take();
    G.check('a newer run that takes longer still wins',
      JSON.stringify(y.cols) === JSON.stringify(['slept', 'which']),
      { cols: y.cols, rows: y.rows, curRun: y.curRun, running: !!y.runningReqId, seq: y.runSeq,
        status: ($('st_' + id) || {}).textContent, log: told.l.slice(-6), toasts: told.t.slice(-4) });
  } finally {
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}` });
  }
  return G.report();
})()

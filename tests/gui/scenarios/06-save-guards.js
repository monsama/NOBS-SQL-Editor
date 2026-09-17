// Saving grid edits changes exactly the row it means or nothing: a FLOAT key, a row deleted in the
// meantime, a TIMESTAMP key in the hour the clocks go back. Duplicate table, the INSERT export and
// renaming keep every value and close what they should.
(async () => {
  const DB = 'nobs_gui_audit';
  try {
    await G.run(`DROP DATABASE IF EXISTS ${DB}; CREATE DATABASE ${DB};
CREATE TABLE ${DB}.f (k FLOAT PRIMARY KEY, v VARCHAR(10));
INSERT INTO ${DB}.f VALUES (1.1,'a'),(2.5,'b');
CREATE TABLE ${DB}.ts (k TIMESTAMP PRIMARY KEY, v VARCHAR(10));
SET time_zone='+00:00';
INSERT INTO ${DB}.ts VALUES ('2026-10-25 00:30:00','summer'),('2026-10-25 01:30:00','winter'),('2026-07-01 10:00:00','july');
SET time_zone=DEFAULT;
CREATE TABLE ${DB}.d (id INT PRIMARY KEY, v VARCHAR(10));
INSERT INTO ${DB}.d VALUES (1,'one'),(2,'two');
CREATE TABLE ${DB}.x (id INT PRIMARY KEY, a INT, secret VARCHAR(10) INVISIBLE, g INT GENERATED ALWAYS AS (a*2) STORED);
INSERT INTO ${DB}.x (id,a,secret) VALUES (1,5,'hidden');`);
    const refused = m => m.some(x => x.startsWith('ERR Nothing was saved. A row you changed or deleted no longer matches exactly one row'));

    let t = await G.openTable(DB, 'f');
    t.pending.upd[t.rows.findIndex(r => r[0] === '1.1') + ':1'] = 'edited';
    G.take(); await applyChanges(t.id); await G.wait(1500);
    G.eq('a row with a FLOAT key is saved', [await G.q(`SELECT v FROM ${DB}.f ORDER BY k`), G.take().t], [[['edited'], ['b']], ['Applied 1 change(s).']]);

    t = await G.openTable(DB, 'ts');
    const same = t.rows.filter(r => r[1] !== 'july').map(r => r[0]);
    if (same[0] === same[1]) {
      t.pending.upd[t.rows.findIndex(r => r[1] === 'summer') + ':1'] = 'changed';
      G.take(); await applyChanges(t.id); await G.wait(1500);
      const told = G.take().t;
      G.check('an ambiguous TIMESTAMP key is refused, nothing changes', refused(told) && JSON.stringify(await G.q(`SELECT v FROM ${DB}.ts ORDER BY v`)) === '[["july"],["summer"],["winter"]]', told);
    } else {
      G.skip('an ambiguous TIMESTAMP key is refused', "the server's time zone has no hour that happens twice");
    }
    t = await G.openTable(DB, 'ts');
    t.pending.upd[t.rows.findIndex(r => r[1] === 'july') + ':1'] = 'july2';
    G.take(); await applyChanges(t.id); await G.wait(1500);
    G.eq('an ordinary TIMESTAMP key is saved', G.take().t, ['Applied 1 change(s).']);

    t = await G.openTable(DB, 'd');
    t.pending.upd[t.rows.findIndex(r => r[0] === '1') + ':1'] = 'uno';
    t.pending.upd[t.rows.findIndex(r => r[0] === '2') + ':1'] = 'dos';
    await G.run(`DELETE FROM ${DB}.d WHERE id=2`);
    G.take(); await applyChanges(t.id); await G.wait(1500);
    const told = G.take().t;
    G.check('a row deleted in the meantime stops the save, and nothing is changed',
      refused(told) && JSON.stringify(await G.q(`SELECT id, v FROM ${DB}.d`)) === '[["1","one"]]', told);

    const realInput = inputBox; inputBox = async () => ({ name: 'x_copy', data: true });
    try { await duplicateTable(DB, 'x'); } finally { inputBox = realInput; }
    await G.wait(1000);
    G.eq('Duplicate table keeps the invisible column and computes the generated one', await G.q(`SELECT CONCAT_WS('|',id,a,secret,g) FROM ${DB}.x_copy`), [['1|5|hidden|10']]);

    if (G.desktop) {
      const file = G.env.tmp + '/x_inserts.sql';
      const r = await G.A('/api/export-table', { db: DB, table: 'x', file, format: 'inserts' });
      G.check('the INSERT export of a table succeeds', r.ok, r);
    } else {
      const text = await G.captureDownload(() => exportFull(DB, 'x', 'inserts'));
      G.eq('the INSERT export names the columns it can write', text, `INSERT INTO ${DB}.x (id,a,secret) VALUES ('1','5','hidden') ON DUPLICATE KEY UPDATE id=id;`);
    }

    t = await G.openTable(DB, 'd');
    const tid = t.id;
    const realInput2 = inputBox; inputBox = async () => ({ name: 'd_renamed' });
    try { await renameTable(DB, 'd'); } finally { inputBox = realInput2; }
    await G.wait(800);
    G.check("renaming a table closes its tabs", !T(tid), 'the tab is still open');
  } finally {
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}` });
  }
  return G.report();
})()

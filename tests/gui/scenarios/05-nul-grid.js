// A key holding a NUL is edited and deleted as itself, not as the row whose key has a space there;
// later pages of a table are exact too; a USE decides which database a grid saves to.
(async () => {
  const DB = 'nobs_gui_nul', DB2 = 'nobs_gui_nul2';
  const N = String.fromCharCode(0);
  try {
    const rows = []; for (let i = 1; i <= 1500; i++) rows.push("('r" + String(i).padStart(4, '0') + "','v" + i + "'," + (i === 1200 ? "CONCAT('late',CHAR(0),'nul')" : "'n" + i + "'") + ")");
    await G.run(`DROP DATABASE IF EXISTS ${DB}; DROP DATABASE IF EXISTS ${DB2}; CREATE DATABASE ${DB}; CREATE DATABASE ${DB2};
CREATE TABLE ${DB}.t (k VARCHAR(10) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin PRIMARY KEY, v VARCHAR(10), note TEXT);
INSERT INTO ${DB}.t VALUES (CONCAT('a',CHAR(0),'b'), 'nul', CONCAT('x',CHAR(0),'y')), ('a b', 'space', 'plain');
INSERT INTO ${DB}.t VALUES ${rows.join(',')};
CREATE TABLE ${DB2}.t (k VARCHAR(10) PRIMARY KEY, v VARCHAR(10));
INSERT INTO ${DB2}.t VALUES ('only2', 'two');`);
    let t = await G.openTable(DB, 't');
    const ki = t.cols.indexOf('k'), vi = t.cols.indexOf('v'), ni = t.cols.indexOf('note');
    const nulRow = t.rows.findIndex(r => r[ki] === 'a' + N + 'b');
    G.check('the key and the text with a NUL are read exactly', nulRow >= 0 && t.rows[nulRow][ni] === 'x' + N + 'y', G.rowsOf(t).slice(0, 3));
    G.eq('no extra columns are shown', t.cols, ['k', 'v', 'note']);
    G.check('the NUL is shown as a badge', />NUL</.test($('res_' + t.id).innerHTML), 'no badge');
    await fetchNextBatch(t.id); await G.until(() => !t.fetchingMore);
    const late = t.rows.find(r => r[ki] === 'r1200');
    G.check('a later page is exact too', late && late[ni] === 'late' + N + 'nul', late);

    t.pending.upd[nulRow + ':' + vi] = 'edited';
    await applyChanges(t.id); await G.wait(2000);
    G.eq('an edit changes that row and not the one with a space', await G.q(`SELECT HEX(k), v FROM ${DB}.t WHERE k IN (CONCAT('a',CHAR(0),'b'), 'a b') ORDER BY k`), [['610062', 'edited'], ['612062', 'space']]);
    t = tabs[tabs.length - 1]; await G.until(() => t.pending && !t.runningReqId);
    t.pending.del.add(t.rows.findIndex(r => r[ki] === 'a' + N + 'b'));
    await applyChanges(t.id); await G.wait(2000);
    G.eq('a delete too', await G.q(`SELECT HEX(k), v FROM ${DB}.t WHERE k IN (CONCAT('a',CHAR(0),'b'), 'a b') ORDER BY k`), [['612062', 'space']]);

    t = await G.runIn(`SELECT k, UPPER(v) AS v FROM ${DB}.t WHERE v = 'space' OR k = 'r0001'`, DB);
    G.eq('an expression named like a column keeps its value', G.rowsOf(t), ['a b|SPACE', 'r0001|V1']);
    t = await G.runIn(`USE ${DB2};\nSELECT * FROM t`, DB);
    G.eq('after USE, the grid saves to that database', [t.db, t.table, G.rowsOf(t)], [DB2, 't', ['only2|two']]);

    if (!G.desktop) {
      G.take();
      const text = await G.captureDownload(() => exportFull(DB, 't', 'csv'));
      G.check('exporting a table with a NUL in text from the tree is refused here', text === null && G.take().t.some(m => /NUL byte/.test(m)), text && text.slice(0, 80));
    }
  } finally {
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}; DROP DATABASE IF EXISTS ${DB2}` });
  }
  return G.report();
})()

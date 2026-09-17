// Compare finds missing, extra and changed rows and copies them exactly, into a latin1 database,
// with a binary key that includes the empty value. Uses nobs_gui from 01.
(async () => {
  const PROF = 'nobs-gui-' + G.env.dbPort;
  const S = 'nobs_gui', T = 'nobs_gui_t';
  const sv = await G.A('/api/conn-save', { name: PROF, conn: getConn(), accent: '#3b82f6', env: 'test', readonly: false, savepw: true });
  if (!G.check('a connection profile for Compare is saved', sv.ok, sv.error)) return G.report();
  try {
    const def = '(id VARBINARY(4) PRIMARY KEY, t TEXT NULL, b VARBINARY(8) NULL, x MEDIUMTEXT NULL)';
    await G.run(`CREATE DATABASE IF NOT EXISTS ${S}; DROP DATABASE IF EXISTS ${T}; CREATE DATABASE ${T} CHARACTER SET latin1;
DROP TABLE IF EXISTS ${S}.cg; CREATE TABLE ${S}.cg ${def}; CREATE TABLE ${T}.cg ${def};
INSERT INTO ${S}.cg VALUES (0x01,'NULL',X'',CONCAT('a',CHAR(13),CHAR(10),'b')),(0x02,NULL,0x00,'x'),(0x03,'0x41',0xFF,CONVERT(x'610062' USING utf8mb4)),(X'',CONVERT(x'C3A9' USING utf8mb4),NULL,'');
INSERT INTO ${T}.cg VALUES (0x01,'null',X'',CONCAT('a',CHAR(13),CHAR(10),'b')),(0x02,'',0x00,'x'),(0x09,'extra',NULL,NULL);`);
    const sum = db => G.one(`SELECT GROUP_CONCAT(CONCAT_WS('|',HEX(id),IFNULL(HEX(CONVERT(t USING utf8mb4)),'N'),IFNULL(HEX(b),'N'),IFNULL(HEX(x),'N')) ORDER BY id SEPARATOR ';') FROM ${db}.cg WHERE id <> 0x09`);

    await openCompare(); await G.wait(800);
    $('cmpSrcConn').value = PROF; await cmpLoadDbs('src'); await G.wait(400);
    $('cmpTgtConn').value = PROF; await cmpLoadDbs('tgt'); await G.wait(400);
    $('cmpSrcDb').value = S; $('cmpTgtDb').value = T;
    await runCompare(); await G.wait(1500);
    const ti = _cmpFindTableIndex('cg');
    // The target database is latin1, so its text columns differ from the source's.
    G.check('the table is found, its text columns differing in character set', ti >= 0 && _cmpTables[ti].status === 'diff' && _cmpTables[ti].sql.every(s => /CHARACTER SET utf8mb4/.test(s.stmt)), ti >= 0 ? _cmpTables[ti] : 'not found');
    const rows = async () => {
      _cmprState = null; _cmprDiffState = null; cmpCompareRows(ti);
      await G.until(() => _cmprState && _cmprDiffState && !_cmprRequestId, 30000); await G.wait(300);
      return [_cmprState.missingTotal, _cmprState.extraTotal, _cmprDiffState.rows.length];
    };
    G.eq('rows: 2 missing, 1 extra, 2 changed (NULL vs empty, NULL vs null)', await rows(), [2, 1, 2]);
    G.take();
    await cmprApply(); await G.wait(1500);
    await cmprDiffApply(); await G.wait(2500);
    const applied = G.take().t;
    G.check('copying and updating is reported', applied.some(m => /Inserted 2 row/.test(m)) && applied.some(m => /Updated 2 row/.test(m)), applied);
    G.eq('the target now holds exactly the source values', await sum(T), await sum(S));
    G.eq('comparing again leaves only the extra row', await rows(), [0, 1, 0]);
    hide('mCompareRows'); hide('mCompare');
  } finally {
    await G.A('/api/conn-delete', { name: PROF });
  }
  return G.report();
})()

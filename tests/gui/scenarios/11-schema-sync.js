// Compare's schema sync, through the dialog: the changes it selects by itself bring the target's
// columns to exactly the source's definitions (character set, collation, comment, default,
// ON UPDATE, invisible, generated) and create a missing table, while dropping a column or a table
// is offered but left unselected. The target's rows are kept.
(async () => {
  const PROF = 'nobs-gui-sync-' + G.env.dbPort;
  const S = 'nobs_gui_ss', T = 'nobs_gui_st';
  const sv = await G.A('/api/conn-save', { name: PROF, conn: getConn(), accent: '#3b82f6', env: 'test', readonly: false, savepw: true });
  if (!G.check('a connection profile for Compare is saved', sv.ok, sv.error)) return G.report();
  try {
    await G.run(`DROP DATABASE IF EXISTS ${S}; DROP DATABASE IF EXISTS ${T};
CREATE DATABASE ${S} DEFAULT CHARACTER SET utf8mb4; CREATE DATABASE ${T} DEFAULT CHARACTER SET utf8mb4;
CREATE TABLE ${S}.t (id INT PRIMARY KEY,
  name VARCHAR(10) CHARACTER SET latin1 COLLATE latin1_bin NOT NULL COMMENT 'customer name',
  a INT NULL DEFAULT 7,
  secret VARCHAR(10) NULL INVISIBLE,
  st ENUM('a','b') NOT NULL DEFAULT 'b',
  upd TIMESTAMP NULL DEFAULT NULL ON UPDATE CURRENT_TIMESTAMP,
  g INT GENERATED ALWAYS AS (a * 2) VIRTUAL) DEFAULT CHARSET=utf8mb4;
CREATE TABLE ${S}.newt (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, v DECIMAL(10,3) NOT NULL DEFAULT 1.5) DEFAULT CHARSET=utf8mb4;
CREATE TABLE ${T}.t (id INT PRIMARY KEY, name VARCHAR(10) CHARACTER SET latin1 COLLATE latin1_bin NULL, a INT NULL, old INT NULL) DEFAULT CHARSET=utf8mb4;
INSERT INTO ${T}.t VALUES (1, 'x', 5, 99);
CREATE TABLE ${T}.oldt (id INT PRIMARY KEY);`);
    const cols = (db, table) => G.one(`SELECT GROUP_CONCAT(CONCAT_WS('|',COLUMN_NAME,COLUMN_TYPE,IS_NULLABLE,IFNULL(COLUMN_DEFAULT,'N'),EXTRA,IFNULL(COLLATION_NAME,'N'),COLUMN_COMMENT,IFNULL(GENERATION_EXPRESSION,'')) ORDER BY COLUMN_NAME SEPARATOR ';')
      FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='${db}' AND TABLE_NAME='${table}' AND COLUMN_NAME <> 'old'`);
    const tables = () => Object.fromEntries(_cmpTables.map(t => [t.name, t.status + ':' + t.sql.map(s => s.kind + (s.checked ? '+' : '-')).sort().join(',')]));

    await openCompare(); await G.wait(800);
    $('cmpSrcConn').value = PROF; await cmpLoadDbs('src'); await G.wait(400);
    $('cmpTgtConn').value = PROF; await cmpLoadDbs('tgt'); await G.wait(400);
    $('cmpSrcDb').value = S; $('cmpTgtDb').value = T;
    await runCompare(); await G.wait(500);
    G.eq('the differences are found, drops left unselected', tables(), {
      newt: 'missing_target:create_table+',
      oldt: 'missing_source:drop_table-',
      t: 'diff:add_column+,add_column+,add_column+,add_column+,drop_column-,modify_column+,modify_column+',
    });

    G.take();
    await applyCompare(); await G.wait(500);
    G.check('applying reports no failure', !/FAILED/i.test($('cmpLog').textContent) && !G.errs().length, $('cmpLog').textContent);
    G.eq('the table has the source columns exactly', await cols(T, 't'), await cols(S, 't'));
    G.eq('the missing table is created exactly', await cols(T, 'newt'), await cols(S, 'newt'));
    G.eq('the target row is kept, new columns filled in', await G.one(`SELECT CONCAT_WS('|',id,name,a,old,IFNULL(secret,'N'),st,IFNULL(upd,'N'),g) FROM ${T}.t`), '1|x|5|99|N|b|N|10');
    G.eq('the unselected table is still there', await G.one(`SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='${T}' AND TABLE_NAME='oldt'`), '1');
    // applyCompare compares again when it is done.
    G.eq('comparing again leaves only the drops', tables(), {
      newt: 'same:',
      oldt: 'missing_source:drop_table-',
      t: 'diff:drop_column-',
    });
    hide('mCompare');
  } finally {
    await G.A('/api/conn-delete', { name: PROF });
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${S}; DROP DATABASE IF EXISTS ${T}` });
  }
  return G.report();
})()

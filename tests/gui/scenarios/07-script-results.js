// A procedure call, and a script with several SELECTs, show every result in a tab of its own.
(async () => {
  const DB = 'nobs_gui_results';
  try {
    await G.run(`DROP DATABASE IF EXISTS ${DB}; CREATE DATABASE ${DB}`);
    await G.run(`CREATE TABLE ${DB}.t (id INT PRIMARY KEY, v VARCHAR(10), b VARBINARY(4));
INSERT INTO ${DB}.t VALUES (1,'one',0x00FF),(2,NULL,NULL),(3,'NULL',X'');
DELIMITER $$
CREATE PROCEDURE ${DB}.two_results(IN n INT)
BEGIN
  SELECT id, v FROM t WHERE id <= n ORDER BY id;
  UPDATE t SET v = 'touched' WHERE id = 3;
  SELECT COUNT(*) AS c, MAX(b) AS mb FROM t;
END$$
DELIMITER ;`, DB);
    const strip = id => $('rsets_' + id).style.display === 'none' ? '' : $('rsets_' + id).textContent;

    let t = await G.runIn('CALL two_results(2);', DB);
    G.eq("a procedure's first result", [strip(t.id), t.cols, G.rowsOf(t), !!t.pk], ['Result 1 (2)Result 2 (1)', ['id', 'v'], ['1|one', '2|<NULL>'], false]);
    showResultSet(t.id, 1);
    G.eq('and its second', [t.cols, G.rowsOf(t)], [['c', 'mb'], ['3|0x00FF']]);
    G.eq('the statement between them ran', await G.one(`SELECT v FROM ${DB}.t WHERE id=3`), 'touched');

    t = await G.runIn("SELECT 1 AS a;\nSELECT 'x' AS b, NULL AS c, 0x41 AS h;", DB);
    G.eq('two SELECTs, the first no longer thrown away', G.rowsOf(t), ['1']);
    showResultSet(t.id, 1);
    G.eq('and the second', G.rowsOf(t), ['x|<NULL>|0x41']);

    t = await G.runIn('SELECT id FROM t WHERE 0;\nSELECT id FROM nobs_test.bulk_rows ORDER BY id;', DB);
    G.eq('an empty result has no rows', t.rows, []);
    showResultSet(t.id, 1);
    G.check('a big result keeps its first 1000 rows and says so', t.rows.length === 1000 && /Showing the first 1.000 of 100.000 rows/.test(strip(t.id)), strip(t.id));

    t = await G.runIn('SELECT 1 AS a;\nSELECT * FROM no_such_table;\nSELECT 2 AS b;', DB);
    const st = $('st_' + t.id).textContent;
    G.check('an error part way shows the result before it', /no_such_table/.test(st) && /showing the 1 result/.test(st) && G.rowsOf(t).join() === '1', st);

    const id = t.id;
    $('ed_' + id).value = 'SELECT * FROM t';
    await runSql(id, 'SELECT * FROM t');
    G.eq('a single SELECT still gets the editable grid', [!!T(id).pk, T(id).table, $('rsets_' + id).style.display], [true, 't', 'none']);

    const ro = window._activeReadOnly;
    window._activeReadOnly = true; window.readOnly = true;
    try {
      t = await G.runIn('CALL two_results(1);', DB);
      G.eq('read-only mode refuses a CALL', $('st_' + t.id).textContent, 'Read-only mode: statement blocked.');
    } finally { window._activeReadOnly = ro; window.readOnly = false; G.take(); }
  } finally {
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}` });
  }
  return G.report();
})()

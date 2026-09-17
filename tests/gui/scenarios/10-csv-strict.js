// The CSV import matches the header to the table ignoring case, leaves a generated column to the
// server, and refuses a whole file - storing none of its rows - when a column is unknown, a row has
// too few or too many fields, or a row breaks a foreign key or a unique key. Replace mode keeps the
// table as it was when the new rows are refused.
(async () => {
  const DB = 'nobs_gui_csv';
  try {
    await G.run(`DROP DATABASE IF EXISTS ${DB}; CREATE DATABASE ${DB};
CREATE TABLE ${DB}.parent (id INT PRIMARY KEY);
INSERT INTO ${DB}.parent VALUES (1);
CREATE TABLE ${DB}.child (id INT PRIMARY KEY, Name VARCHAR(20) NULL, pid INT NULL, g INT GENERATED ALWAYS AS (id * 2) STORED,
  u INT NULL, UNIQUE KEY (u), FOREIGN KEY (pid) REFERENCES ${DB}.parent (id));`);
    const rows = () => G.one(`SELECT IFNULL(GROUP_CONCAT(CONCAT_WS('|',id,IFNULL(Name,'N'),IFNULL(pid,'N'),g,IFNULL(u,'N')) ORDER BY id SEPARATOR ';'),'') FROM ${DB}.child`);
    const imp = async (file, replace) => {
      importCsv(DB, 'child'); await G.wait(200);
      $('csvFile').value = G.env.tmp + '/' + file; $('csvHeader').checked = true;
      if ($('csvNullVal')) $('csvNullVal').value = '\\N';
      $('csvReplace').checked = !!replace;
      G.logs.length = 0; await runCsvImport();
      // The editions show the outcome differently in the dialog, and log it the same way.
      const logged = G.logs.splice(0).join('\n');
      return { ok: /^CSV import: /m.test(logged), err: /^CSV import error: /m.test(logged), text: $('csvLog').textContent };
    };
    const refused = async (name, file, re, replace) => {
      const before = await rows();
      const r = await imp(file, replace);
      G.check(name, r.err && re.test(r.text), r.text);
      G.eq('  and the table is unchanged', await rows(), before);
    };

    let r = await imp('csv-case.csv');
    G.check('a header in another case imports, the generated column left to the server',
      r.ok && /Imported 2 row/.test(r.text) && /columns: id, Name, pid, u; generated, so computed by the server: g/.test(r.text), r.text);
    G.eq('  with every value as in the file', await rows(), '1|a|1|2|7;2|N|N|4|N');

    await refused('an unknown column is refused', 'csv-unknown.csv', /no column named nmae/);
    await refused('a row with too few fields is refused', 'csv-short.csv', /has 1 field\(s\), but the header has 2/);
    await refused('a row with too many fields is refused', 'csv-long.csv', /has (3|more than 2) field\(s\), but the header has 2/);
    await refused('a row whose parent does not exist is refused, the valid row before it too', 'csv-fk.csv', /foreign key constraint fails/i);
    await refused('a duplicate in a unique key is refused', 'csv-unique.csv', /Duplicate entry '7'/);
    await refused('in Replace mode, a refused file leaves the old rows in place', 'csv-fk.csv', /foreign key constraint fails/i, true);

    r = await imp('csv-replace.csv', true);
    G.check('Replace mode with a valid file imports it', r.ok && /Imported 1 row/.test(r.text), r.text);
    G.eq('  in place of the old rows', await rows(), '5|z|N|10|N');
    hide('mCsv');
  } finally {
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}` });
  }
  return G.report();
})()

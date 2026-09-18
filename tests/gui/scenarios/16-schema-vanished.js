// A schema that disappears while the app is pointed at it.
//
// Drop a database from anywhere else - another client, a script in another tab - and every query
// from the tab still pointed at it comes back "Unknown database", while the tree goes on listing it
// and nothing says what happened. Correct, and it reads as the app being broken rather than the
// database being gone. Found while writing another test, where a scenario's own leftovers made two
// runs fail exactly this way.
//
// What it must not do is reach for the tree on any such error: a database named in the user's own
// SQL is theirs to fix, and reloading under them for a typo would be noise.
(async () => {
  const DB = 'nobs_gui_vanish', OTHER = 'nobs_gui_vanish_other';
  try {
    await G.run(`DROP DATABASE IF EXISTS ${DB}; DROP DATABASE IF EXISTS ${OTHER};
CREATE DATABASE ${DB}; CREATE DATABASE ${OTHER};
CREATE TABLE ${DB}.t (id INT PRIMARY KEY);
INSERT INTO ${DB}.t VALUES (1);`);

    // A tab running in that schema, the way the app runs one: the schema is selected, the query
    // names no database of its own.
    const t = await G.runIn('SELECT id FROM t', DB);
    G.eq('the tab reads its rows from the selected schema', G.rowsOf(t), ['1']);
    await loadSchemas();
    G.check('and the schema is listed', (window.allSchemas || []).includes(DB), window.allSchemas);

    // Gone from underneath it.
    await G.A('/api/script', { sql: `DROP DATABASE ${DB}` });
    G.take();
    await runSql(t.id, 'SELECT id FROM t');
    await G.wait(1500);
    const heard = G.take();
    const said = heard.t.concat(heard.l).join(' | ');

    G.check('the app says the database is gone rather than only that the query failed',
      /no longer exists/i.test(said), said.slice(0, 200));
    G.check('the schema list no longer offers it', !(window.allSchemas || []).includes(DB),
      window.allSchemas);
    G.check('and nothing is left selected', curSchema === null, curSchema);

    // The other half: a database named in the user's own SQL is not the app's business. It needs a
    // tab that is NOT pointed at the missing one - the query above bound this one to a table in it,
    // so dbOf() still answers with it and the app would be right to chase that. A fresh tab, and a
    // schema that exists.
    const other = await G.runIn('SELECT 1 AS ok', OTHER);
    curSchema = OTHER;
    G.take();
    await runSql(other.id, `SELECT 1 FROM ${DB}.t`);
    await G.wait(1500);
    const told = G.take();
    const all = told.t.concat(told.l).join(' | ');
    // The wording is the server's and differs - MySQL says the table does not exist for a
    // qualified name, the client says the database does not for one it was handed - so the check is
    // that an error was reported and that the app did not go reloading the tree behind it.
    G.check('a database named in the query is reported, not chased',
      /ERROR \d{3,4}/.test(all) && !/no longer exists/i.test(all), all.slice(0, 200));
    G.check('and the selected schema is left alone', curSchema === OTHER, curSchema);
  } finally {
    curSchema = null;
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}; DROP DATABASE IF EXISTS ${OTHER}` });
  }
  return G.report();
})()

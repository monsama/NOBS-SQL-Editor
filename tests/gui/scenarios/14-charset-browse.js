// Reading a table in another character set, which is how you tell a storage problem from a display
// one.
//
// The fixture is the mistake this answers: the bytes of "café" in UTF-8, stored in a latin1 column.
// The server transcodes text into the session's character set before sending it, so the same row
// reads as "cafÃ©" in utf8mb4 and as "café" in latin1 - and data that is genuinely damaged reads
// badly in both. Nothing in a grid can tell those apart, which is why this exists (issue #96).
//
// The other half is that nothing may be written while this is on: a value typed into the grid would
// be interpreted in that session's charset and stored as different bytes than the ones on screen.
// Three things enforce it and all three are checked here - the UI stops offering, the endpoint
// refuses the statement, and the server is told the session cannot write.
(async () => {
  const DB = 'nobs_gui_charset';
  try {
    await G.run(`DROP DATABASE IF EXISTS ${DB}; CREATE DATABASE ${DB};
CREATE TABLE ${DB}.mojibake (id INT PRIMARY KEY, t VARCHAR(40) CHARACTER SET latin1);
INSERT INTO ${DB}.mojibake VALUES (1, 0x636166C3A9);`);

    const t = await G.openTable(DB, 'mojibake');
    const ti = t.cols.indexOf('t');
    const readsAs = () => { const x = T(t.id); return x && x.rows && x.rows[0] ? x.rows[0][ti] : null; };
    G.eq('by default the bytes arrive transcoded into utf8mb4', readsAs(), 'cafÃ©');

    const sel = $('browseCs');
    G.check('the grid offers to read in another character set',
      !!sel && [...sel.options].some(o => o.value === 'latin1') && [...sel.options].some(o => o.value === 'binary'),
      sel ? [...sel.options].map(o => o.value).join(',') : 'no control');

    // The diagnostic itself.
    setBrowseCharset('latin1');
    await G.until(() => readsAs() === 'café', 20000);
    G.eq('read in latin1, the same row is the text it was meant to be', readsAs(), 'café');
    G.check('and the connection says it is read-only', window.readOnly === true, window.readOnly);

    // Every way in is closed, not only the buttons.
    G.take();
    const wrote = await G.A('/api/query', { sql: `UPDATE ${DB}.mojibake SET t='x' WHERE id=1`, db: DB });
    G.check('a write through the app is refused while reading in another charset',
      wrote && wrote.ok === false && /read-only/i.test(String(wrote.error)), wrote);

    // The bytes themselves, which is what "binary" is for.
    setBrowseCharset('binary');
    await G.until(() => readsAs() !== 'café', 20000);
    const raw = readsAs();
    G.check('read in binary, the row is the bytes that are stored', /^0x636166C3A9$/i.test(String(raw)), raw);

    // And back, with nothing left behind: the value reads as it did, and writing is offered again.
    setBrowseCharset('');
    await G.until(() => readsAs() === 'cafÃ©', 20000);
    G.eq('back on the server default, the row reads as it did', readsAs(), 'cafÃ©');
    G.check('and the connection is writable again', window.readOnly === false, window.readOnly);

    // Nothing of this touched the data - the point of a diagnostic.
    G.eq('the row is exactly as it was stored',
      await G.q(`SELECT HEX(t) FROM ${DB}.mojibake WHERE id=1`), [['636166C3A9']]);
  } finally {
    setBrowseCharset('');
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}` });
  }
  return G.report();
})()

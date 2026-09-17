// Bytes in a column that is NOT declared binary: what the app shows for them, and whether editing
// one through the cell editor stores the bytes it showed you.
//
// The app hex-encodes any value whose bytes do not decode as UTF-8 (val_to_opt), whatever the
// column is, and the cell editor offers its Text/Hex tabs for anything that merely LOOKS like hex
// - not only for columns the server declares binary. The writer disagrees: litAs() quotes for a
// column that is not declared binary, so a hex value from such a cell is stored as the characters
// "0", "x", ... rather than the bytes they denote. Whether that is reachable at all is the first
// thing this checks: a latin1 column is converted to the connection's charset by the server before
// the app ever sees it, so the premise may simply not hold - which is a fine answer, and the
// reason the checks below are conditional rather than assumed.
//
// The property that matters either way: opening a value and saving it must not change what is
// stored, and a value edited as bytes must be stored as those bytes.
(async () => {
  const DB = 'nobs_gui_bytes';
  try {
    await G.run(`DROP DATABASE IF EXISTS ${DB}; CREATE DATABASE ${DB};
CREATE TABLE ${DB}.t (id INT PRIMARY KEY, c VARCHAR(10) CHARACTER SET latin1, b VARBINARY(10));
INSERT INTO ${DB}.t VALUES (1, 0xFF, 0xFF), (2, 'plain', 0x00FF);`);

    const stored = async (col, id) => await G.one(`SELECT HEX(${col}) FROM ${DB}.t WHERE id = ${id}`, DB);
    G.eq('the fixture holds the byte FF in both columns', [await stored('c', 1), await stored('b', 1)], ['FF', 'FF']);

    const t = await G.openTable(DB, 't');
    const ci = t.cols.indexOf('c'), bi = t.cols.indexOf('b');
    const shown = t.rows[0][ci];
    const declaredBinary = !!(t.binCols && t.binCols[ci]);
    G.check('a latin1 column is not declared binary', !declaredBinary, { binCols: t.binCols });

    // Saving a value nobody edited must be a no-op, whatever the app decided to show.
    await editCell(gridCellEl(t.id, 0, ci), t.id, 0, ci);
    const box = $('vText').value, hexTab = $('vHexTabs').style.display !== 'none';
    hide('mView');
    G.check('opening and closing a value leaves the database alone', (await stored('c', 1)) === 'FF',
      { shown, box: JSON.stringify(box), storedNow: await stored('c', 1) });

    if (!/^0x[0-9A-Fa-f]+$/.test(String(shown))) {
      // The server converted latin1 to the connection charset, so the app got text, not bytes, and
      // the editor is a plain text box. Nothing here to disagree about.
      G.skip('a text column shown as hex', `the server delivered it as text (${JSON.stringify(shown)}), so the hex editor never opens for it`);
    } else {
      G.check('a value shown as hex is edited as hex', hexTab, { shown, hexTab });
      // Exactly what the editor's Save does: getVal() for a hex cell is hexCellValueForSave(mode,
      // box), and onSave hands that to setUpd.
      setUpd(t.id, 0, ci, hexCellValueForSave('hex', '0xFE'));
      await applyChanges(t.id);
      await G.wait(1500);
      G.eq('a byte edited as hex is stored as that byte, not as the characters of its hex',
        await stored('c', 1), 'FE');
    }

    // The declared-binary column is the case the writer and the editor agree on - included so a
    // failure above can be read as "this column kind", not "hex editing is broken".
    const t2 = tabs[tabs.length - 1];
    await editCell(gridCellEl(t2.id, 0, bi), t2.id, 0, bi);
    hide('mView');
    setUpd(t2.id, 0, bi, hexCellValueForSave('hex', '0xFE'));
    await applyChanges(t2.id);
    await G.wait(1500);
    G.eq('a declared binary column stores the bytes it was given', await stored('b', 1), 'FE');
  } finally {
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}` });
  }
  return G.report();
})()

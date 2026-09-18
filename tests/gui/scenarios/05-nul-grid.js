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

    // What the grid and the cell editor do with values that have no visible form of their own. The
    // functions behind this are unit-tested; what is only reachable here is the wiring - that the
    // note exists in the page, is filled when the editor opens, and goes away in Hex mode.
    await G.run(`CREATE TABLE ${DB}.viewer (id INT PRIMARY KEY, txt TEXT, bin VARBINARY(10), nothing VARBINARY(10));
INSERT INTO ${DB}.viewer VALUES (1, CONCAT('x',CHAR(0),'y'), CONCAT('a',CHAR(0)), X'');
INSERT INTO ${DB}.viewer VALUES (2, CONCAT('a',CHAR(9),'b'), NULL, NULL);`);
    const tv = await G.openTable(DB, 'viewer');
    const col = n => tv.cols.indexOf(n);
    const cellOf = n => gridCellEl(tv.id, 0, col(n));
    // A zero-byte binary value and a text column holding the two characters "0x" arrive
    // identically, so only the declared type tells them apart. Both editions read it now - the
    // Editor's backend off the result set, the PowerShell one from information_schema when the
    // table loads (see colTypesBinCols) - so this is asked of both rather than skipped for the
    // one that could not answer.
    G.check('a binary column with no bytes reads (0 bytes), not the 0x it arrives as',
      /\(0 bytes\)/.test(cellOf('nothing').innerHTML), cellOf('nothing').innerHTML.slice(0, 80));

    // A TEXT column: there is no Hex tab on this path, so the note is the only mention anywhere of
    // the NUL sitting in the value.
    await editCell(cellOf('txt'), tv.id, 0, col('txt'));
    G.check('the editor says a control character is in a text value',
      $('vNote').style.display !== 'none' && /1 control character \(NUL\)/.test($('vNote').textContent), $('vNote').textContent);
    G.check('and does not point at a Hex tab this cell has not got', !/Hex/.test($('vNote').textContent), $('vNote').textContent);
    G.check('while the box still holds the value exactly', $('vText').value === 'x' + N + 'y', JSON.stringify($('vText').value));
    hide('mView');

    // A binary column: the same note, pointing at Hex - where the bytes are in plain view and the
    // note has nothing left to say.
    await editCell(cellOf('bin'), tv.id, 0, col('bin'));
    G.check('a binary value says it too, and points at Hex',
      $('vNote').style.display !== 'none' && /switch to Hex/.test($('vNote').textContent), $('vNote').textContent);
    switchHexTab('hex');
    G.check('nothing to say in Hex mode, where the bytes are shown',
      $('vNote').style.display === 'none' && /^0x6100$/i.test($('vText').value), { note: $('vNote').textContent, box: $('vText').value });
    switchHexTab('text');
    G.check('and it is said again on the way back to Text',
      $('vNote').style.display !== 'none' && $('vText').value === 'a' + N, { note: $('vNote').textContent, box: JSON.stringify($('vText').value) });
    hide('mView');

    // An ordinary value has nothing to report, and the note from the cell before must not linger.
    await editCell(cellOf('id'), tv.id, 0, col('id'));
    G.check('an ordinary value is opened without a note', $('vNote').style.display === 'none', $('vNote').textContent);
    hide('mView');

    // What the clipboard is told, and what it is told about. None of this can be checked by
    // reading the clipboard - the app is not allowed to - so what is checked is what the app says,
    // which is the part that was missing in the first place.
    G.take();
    const gridCell = cellOf('txt');
    const range = document.createRange(); range.selectNodeContents(gridCell);
    const sel = getSelection(); sel.removeAllRanges(); sel.addRange(range);
    gridCell.dispatchEvent(new ClipboardEvent('copy', { bubbles: true, cancelable: true }));
    await G.wait(200);
    sel.removeAllRanges();
    G.check('copying a drawn cell says it is the drawing, not the value',
      G.take().t.some(m => /how the grid shows the value/.test(m)), 'no warning');

    // The same selection made inside the cell editor, where the value really is in the box and
    // the clipboard is what cuts it short.
    G.take();
    await editCell(cellOf('txt'), tv.id, 0, col('txt'));
    const box = $('vText'); box.focus(); box.setSelectionRange(0, box.value.length);
    box.dispatchEvent(new ClipboardEvent('copy', { bubbles: true, cancelable: true }));
    await G.wait(200);
    G.check('copying a value with a NUL out of the editor says what was left behind',
      G.take().t.some(m => /cannot carry a NUL/.test(m)), 'no warning');
    hide('mView');

    // And a whole row, where the format rather than the clipboard is what cannot carry it: row 2
    // holds a tab inside a value and two absent ones, and no NUL, so this is the only thing that
    // should be reported about it.
    //
    // The write itself is stubbed out for the length of this check. Writing to the clipboard needs
    // a window the user is working in, which this one is not - headless, driven over a debugging
    // port - so the real call neither succeeds nor fails here, and the report that follows a
    // successful copy would never run. Everything else is the real path: copyRow, copyText and
    // clipWrite, up to the hint at the end of it.
    G.take();
    const realWrite = navigator.clipboard.writeText.bind(navigator.clipboard);
    navigator.clipboard.writeText = async () => {};
    try {
      copyRow(tv.id, tv.rows.findIndex(r => String(r[col('id')]) === '2'));
      await G.wait(400);
    } finally { navigator.clipboard.writeText = realWrite; }
    const rowSaid = G.take().t;
    G.check('copying a row says what tab-separated text cannot carry',
      rowSaid.some(m => /holds a tab or a line break/.test(m)) && rowSaid.some(m => /empty value/.test(m)), rowSaid);

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

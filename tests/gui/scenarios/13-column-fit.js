// Fitting a column measures the column, not the screenful.
//
// Double-clicking a column's edge sizes it to its widest value. It used to size it to the widest
// cell the DOM held, and above 300 rows the grid only builds the rows around where you are
// scrolled - so the value that should have decided the width was usually not there to be measured,
// and the same column fitted differently depending on where you had scrolled to. The picking of
// what to measure is unit-tested (tests/ui/column-fit.test.mjs); what only this can show is that
// the fit reaches a row the grid never drew, and that it stops at the edge of the pane.
(async () => {
  const DB = 'nobs_gui_fit';
  const LONG = 'this value sits nine hundred rows down where nothing has ever scrolled to it';
  try {
    const rows = [];
    for (let i = 1; i <= 900; i++) rows.push(`(${i},'short','${i === 880 ? LONG : 'short'}','x')`);
    await G.run(`DROP DATABASE IF EXISTS ${DB}; CREATE DATABASE ${DB};
CREATE TABLE ${DB}.fit (id INT PRIMARY KEY, brief VARCHAR(255), deep VARCHAR(255), huge VARCHAR(600));
INSERT INTO ${DB}.fit VALUES ${rows.join(',')};
UPDATE ${DB}.fit SET huge = REPEAT('W', 600) WHERE id = 5;`);

    const t = await G.openTable(DB, 'fit');
    const wrap = $('res_' + t.id);
    const off = t.pk ? 2 : 1;
    const widthOf = name => {
      const cg = wrap.querySelector('table.grid colgroup');
      return parseFloat(cg.children[t.cols.indexOf(name) + off].style.width) || 0;
    };
    const fit = name => { autofitCol(t.id, t.cols.indexOf(name)); return widthOf(name); };

    // Everything below rests on this: if the grid had drawn all 900 rows, measuring the DOM would
    // have found the long value and there would be nothing to test.
    const drawn = wrap.querySelectorAll('tbody tr').length;
    G.check('the grid draws a window rather than every row', drawn > 0 && drawn < 900, `${drawn} of 900 rows drawn`);
    G.check('the row that decides the width is not one of them',
      !/nine hundred rows down/.test(wrap.innerHTML), 'the long value was already on screen');

    const brief = fit('brief'), deep = fit('deep');
    G.check('a column of short values fits to something narrow', brief > 40 && brief < 200, brief);
    G.check('a column whose widest value was never drawn fits to that value', deep > brief + 200, `brief ${brief}, deep ${deep}`);

    // The cap. 600 characters cannot fit in any window, and a column wider than the pane trades
    // reading the value for finding it.
    const huge = fit('huge');
    G.check('a value too long for the window stops at the pane', huge > deep && huge <= wrap.clientWidth,
      `huge ${huge}, pane ${wrap.clientWidth}`);

    // Fitting is a question about the column, so it has one answer. Two ways to get a different
    // one, both of which happened: measure whatever was on screen, or read the header's width back
    // out of the page, where scrollWidth never reports less than the width the column already has -
    // so each fit added its slack to the last one and the column crept wider every time.
    const twice = fit('deep'), thrice = fit('deep');
    G.check('fitting the same column again does not widen it', twice === deep && thrice === deep, `${deep}, ${twice}, ${thrice}`);
    wrap.scrollTop = wrap.scrollHeight;
    await G.wait(300);
    const again = fit('deep');
    G.check('the same column fits the same from another scroll position', Math.abs(again - deep) < 2, `${deep} then ${again}`);

    // The handle you grab has to be on the line you mean. It is positioned from the cell's padding
    // box, which with collapsed borders stops half a border short of the line, so every offset
    // lands left of it - measured at 5.5px of the band left of the line and 3.5px right, which is
    // visible when it highlights.
    const th = wrap.querySelectorAll('thead tr:first-child th')[t.cols.indexOf('deep') + off];
    const rz = th.querySelector('.rz');
    const line = th.getBoundingClientRect().right, band = rz.getBoundingClientRect();
    const centre = (band.left + band.right) / 2;
    G.check('the resize handle is centred on the line it grabs', Math.abs(centre - line) <= 0.5,
      `band ${band.left}-${band.right}, line at ${line}`);
    G.check('and is wide enough to hit on either side', band.width >= 8 && line - band.left >= 3 && band.right - line >= 3,
      `${line - band.left} left, ${band.right - line} right`);
  } finally {
    await G.A('/api/script', { sql: `DROP DATABASE IF EXISTS ${DB}` });
  }
  return G.report();
})()

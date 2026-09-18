// Which table a result grid edits, and how text with control characters is shown.
//
// Apply writes to the table the grid is bound to. A query naming its table without a database
// was bound to the tab's own database, even when the query had run somewhere else - after a
// leading "USE other;", or in an edited table tab while another schema was selected - so saving
// an edit wrote to the same-named table in the wrong database.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

// NOBS_UI_SOURCE lets the PowerShell edition run this same file against NOBSSQL.ps1, which carries
// the identical UI inline.
const html = readFileSync(process.env.NOBS_UI_SOURCE ||
  join(dirname(fileURLToPath(import.meta.url)), '../../ui/index.html'), 'utf8').replace(/\r\n/g, '\n');

function extractFunction(src, name) {
  let start = src.indexOf(`async function ${name}(`);
  if (start === -1) start = src.indexOf(`function ${name}(`);
  assert.notEqual(start, -1, `function ${name} not found - was it renamed?`);
  let depth = 0;
  for (let j = src.indexOf('{', start); j < src.length; j++) {
    if (src[j] === '{') depth++;
    else if (src[j] === '}' && --depth === 0) return src.slice(start, j + 1);
  }
  throw new Error(`unbalanced braces while extracting ${name}`);
}
function extractConst(src, name) {
  const start = src.indexOf(`const ${name}=`);
  assert.notEqual(start, -1, `const ${name} not found - was it renamed?`);
  return src.slice(start, src.indexOf(';\n', start) + 1)
}

const NAMES = ['sqlHead', 'useTarget', 'scriptShowsResults', 'parseSingleEditableTable', 'refreshRunTableBinding',
  'esc', 'clip', 'ctrlBadge', 'textCellHtml', 'decodeCtrlCharCell', 'hexToBitNumber', 'cellHtml', 'ctrlCharNote', 'binaryEditMode'];
const bundle = [extractConst(html, 'CTRL_NAMES'), extractConst(html, 'CTRL_RE'),
  ...NAMES.map(n => extractFunction(html, n))].join('\n');

function load(tab, schema) {
  const tabs = { t1: tab };
  const env = { T: id => tabs[id], $: () => null, selBtnHtml: () => '', curSchema: schema };
  const keys = Object.keys(env);
  return new Function(...keys, `${bundle}\nreturn {${NAMES.join(',')}};`)(...keys.map(k => env[k]));
}

test('the last leading USE is where a bare table name points', () => {
  const f = load({}, 'a');
  assert.equal(f.useTarget(['USE b;', 'SELECT * FROM t']), 'b');
  assert.equal(f.useTarget(['use `we``ird`', 'USE c', 'SELECT 1']), 'c');
  assert.equal(f.useTarget(['USE `we``ird`;', 'SELECT 1']), 'we`ird');
  assert.equal(f.useTarget(['-- note\nUSE d;', 'SELECT 1']), 'd');
  assert.equal(f.useTarget(['SELECT * FROM t']), null);
});

test('a grid is bound to the database its query ran in', () => {
  // A tab opened on a.t, edited to a bare "SELECT * FROM t" and run while b is selected: the query
  // read b.t, so edits must go to b.t.
  const tab = { db: 'a', table: 't' };
  const f = load(tab, 'b');
  f.refreshRunTableBinding('t1', 'SELECT * FROM t', 'b');
  assert.deepEqual([tab.db, tab.table], ['b', 't']);
});

test('a database named in the query still wins', () => {
  const tab = { db: 'a', table: null };
  const f = load(tab, 'a');
  f.refreshRunTableBinding('t1', 'SELECT * FROM `c`.`t` WHERE id = 1', 'b');
  assert.deepEqual([tab.db, tab.table], ['c', 't']);
});

test('without a database from the run, the tab keeps its own', () => {
  const tab = { db: 'a', table: null };
  const f = load(tab, 'z');
  f.refreshRunTableBinding('t1', 'SELECT * FROM t', null);
  assert.deepEqual([tab.db, tab.table], ['a', 't']);
});

test('a result that is not one table is not editable', () => {
  const tab = { db: 'a', table: 't' };
  const f = load(tab, 'a');
  f.refreshRunTableBinding('t1', 'SELECT * FROM t JOIN u USING (id)', 'a');
  assert.equal(tab.table, null);
});

test('a NUL inside text is shown, not swallowed', () => {
  const f = load({}, 'a');
  const h = f.textCellHtml('a' + String.fromCharCode(0) + 'b', 300);
  assert.match(h, /^a<span[^>]*>NUL<\/span>b$/);
  assert.equal(f.textCellHtml('tab\there\nand <b>', 300), 'tab\there\nand &lt;b&gt;', 'tab and line break are ordinary text');
  assert.match(f.textCellHtml('x' + String.fromCharCode(27) + 'y', 300), />ESC</);
});

// A zero-byte binary value is "0x" - the prefix and nothing else - which misses the hex branch's
// one-or-more-digits test and used to be printed as those two characters, the wire format leaking
// into the grid. The column's declared type decides, because a VARCHAR really can hold "0x".
test('a binary column with no bytes says so rather than printing 0x', () => {
  const f = load({}, 'a');
  assert.match(f.cellHtml('0x', false, true), /\(0 bytes\)/);
  assert.equal(f.cellHtml('0x', false, false), '0x', 'a text column holding those two characters shows them');
  assert.equal(f.cellHtml('0x', false, undefined), '0x', 'and so does a grid with no column types at all');
  assert.match(f.cellHtml('', false, true), /\(empty\)/, 'an empty string stays (empty) - not the same thing');
  assert.match(f.cellHtml(null, false, true), /\(NULL\)/);
  assert.match(f.cellHtml('0x6100', false, true), />NUL</, 'a value with bytes still decodes, badges and all');
});

// The grid badges a control character; the cell editor is a textarea and cannot, so it says what is
// in there instead. Deliberately not rendered into the box itself - Text mode saves the box's
// contents byte for byte, so a visible stand-in would be saved as its own characters.
test('the cell editor is told about control characters it cannot show', () => {
  const f = load({}, 'a');
  const N = String.fromCharCode(0);
  assert.equal(f.ctrlCharNote('plain text', true), '', 'nothing to say about ordinary text');
  assert.equal(f.ctrlCharNote('tab\there\nand a break', true), '', 'tab and line break are ordinary text, and visible');
  const one = f.ctrlCharNote('a' + N, true);
  assert.match(one, /1 control character \(NUL\)/);
  assert.match(one, /takes no space/, 'singular reads as singular');
  assert.match(one, /switch to Hex/, 'a binary cell can be edited as bytes instead');
  const many = f.ctrlCharNote('a' + N + 'b' + N + String.fromCharCode(27), true);
  assert.match(many, /3 control characters \(NUL ×2, ESC\), which take no space/);
  assert.doesNotMatch(f.ctrlCharNote('a' + N, false), /Hex/, 'an ordinary text column has no Hex tab to point at');
  assert.equal(f.ctrlCharNote(null, false), '', 'a NULL cell has no text to describe');
});

// Which editor a value gets, and the rule it has to agree with: litAs() writes a hex literal only
// for a column the server calls binary, and quotes everything else. So the byte editor - whose Save
// writes a hex literal - may only be offered for those same columns. A value that merely arrives as
// hex, because its bytes would not decode as text, is a text column's value still: it gets a text
// box, which is what a save will store.
test('the byte editor is offered for binary columns and nothing else', () => {
  const f = load({}, 'a');
  assert.equal(f.binaryEditMode(true, '0x6100'), 'hex', 'a declared binary column edits as bytes');
  assert.equal(f.binaryEditMode(true, ''), 'hex', 'whatever it happens to hold');
  assert.equal(f.binaryEditMode(false, '0xdeadbeef'), 'hexShownAsText',
    'a value shown as hex from a column that is not binary is text, and says so');
  assert.equal(f.binaryEditMode(false, 'plain text'), null);
  assert.equal(f.binaryEditMode(false, '0x'), null, 'the bare marker is not a hex value');
  assert.equal(f.binaryEditMode(false, '0xzz'), null, 'nor is something that only starts like one');
  assert.equal(f.binaryEditMode(false, null), null, 'nor is NULL');
  assert.equal(f.binaryEditMode(undefined, '0x41'), 'hexShownAsText',
    'a grid with no column types at all must not offer to write bytes');
});

// A procedure's results, and every SELECT but the last in a script, were run and thrown away. Such a
// script now shows each result; a single query keeps the editable grid.
test('a script shows every result when it calls a procedure or has several SELECTs', () => {
  const f = load({}, 'a');
  assert.equal(f.scriptShowsResults(['CALL p(1)']), true);
  assert.equal(f.scriptShowsResults(['-- first' + String.fromCharCode(10) + 'call p()']), true);
  assert.equal(f.scriptShowsResults(['SELECT 1', 'SHOW TABLES']), true);
  assert.equal(f.scriptShowsResults(['SELECT * FROM t']), false, 'one query: the editable grid');
  assert.equal(f.scriptShowsResults(['USE b', 'SELECT * FROM t']), false);
  assert.equal(f.scriptShowsResults(['UPDATE t SET a = 1', 'SELECT * FROM t']), false);
  assert.equal(f.scriptShowsResults(['UPDATE t SET a = 1']), false);
});

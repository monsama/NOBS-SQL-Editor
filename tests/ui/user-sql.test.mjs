// Regression tests for the SQL the Users dialog builds (ui/index.html).
//
// These drive the real newUser / dropUser / grantUser / revokeUser / lockUser functions, lifted
// out of the HTML file, with the dialogs and the exec() call stubbed so the generated SQL can be
// captured. A copy of the SQL-building logic here would keep passing while the real one drifted.
//
// The Users dialog is the only place in the app that writes GRANT, CREATE USER and DROP USER, and
// it builds every one of them client-side as text, so the quoting done here is all there is.
//
// Run: node --test tests/ui/     (or npm test)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const html = readFileSync(join(root, 'ui', 'index.html'), 'utf8');

// Lifts a function out by matching braces. Keeps a leading `async` if there is one, since these
// are async and would not parse without it.
function extractFunction(src, name) {
  let start = src.indexOf(`async function ${name}(`);
  if (start === -1) start = src.indexOf(`function ${name}(`);
  assert.notEqual(start, -1, `function ${name} not found in ui/index.html - was it renamed?`);
  let depth = 0;
  for (let j = src.indexOf('{', start); j < src.length; j++) {
    if (src[j] === '{') depth++;
    else if (src[j] === '}' && --depth === 0) return src.slice(start, j + 1);
  }
  throw new Error(`unbalanced braces while extracting ${name}`);
}

const NAMES = ['strLit', 'lit', 'newUser', 'dropUser', 'grantUser', 'revokeUser', 'lockUser'];
const bundle = NAMES.map(n => extractFunction(html, n)).join('\n');

// Builds the dialog functions with everything they touch stubbed out, and returns both the
// captured SQL and the harness so a test can set the selected user.
function harness({ dialog = {}, selected = null } = {}) {
  const sql = [];
  const env = {
    inputBox: async () => dialog,
    grantRevokeDialog: async () => dialog,
    ask: async () => true,
    toast: () => {},
    openUsers: () => {},
    showGrants: () => {},
    exec: async (s) => { sql.push(s); return true; },
    window: { _selUser: selected },
  };
  const keys = Object.keys(env);
  const fns = new Function(...keys, `${bundle}\nreturn {newUser,dropUser,grantUser,revokeUser,lockUser};`)(
    ...keys.map(k => env[k]));
  return { sql, fns };
}

const SEP = '\x01'; // how the Users list packs user+host into _selUser

test('a user name containing a quote is escaped, not broken', async () => {
  const h = harness({ dialog: { user: "o'brien", host: 'localhost', pw: 'pw' } });
  await h.fns.newUser();
  assert.equal(h.sql[0], "CREATE USER 'o''brien'@'localhost' IDENTIFIED BY 'pw'");
});

test('a user name that looks like a hex literal is still quoted as a name', async () => {
  // lit() passes 0x.. through UNQUOTED, which is correct for a BIT/BINARY column value and wrong
  // for a name: "CREATE USER 0xAB@'%'" is a syntax error, so an account called 0xAB simply could
  // not be created. Names go through strLit(), which has no hex case. The Rust side already made
  // this distinction deliberately - see sql_str_lit's comment in main.rs.
  const h = harness({ dialog: { user: '0xAB', host: '%', pw: 'pw' } });
  await h.fns.newUser();
  assert.equal(h.sql[0], "CREATE USER '0xAB'@'%' IDENTIFIED BY 'pw'");
});

test('a host that looks like a hex literal is quoted too', async () => {
  const h = harness({ dialog: { user: 'alice', host: '0xff', pw: 'pw' } });
  await h.fns.newUser();
  assert.equal(h.sql[0], "CREATE USER 'alice'@'0xff' IDENTIFIED BY 'pw'");
});

test('an empty host defaults to %', async () => {
  const h = harness({ dialog: { user: 'alice', host: '   ', pw: 'pw' } });
  await h.fns.newUser();
  assert.match(h.sql[0], /@'%'/);
});

test('DROP USER targets exactly the selected account', async () => {
  const h = harness({ selected: `0xAB${SEP}%` });
  await h.fns.dropUser();
  assert.equal(h.sql[0], "DROP USER '0xAB'@'%'");
});

test('DROP USER escapes a quoted name rather than truncating at it', async () => {
  const h = harness({ selected: `o'brien${SEP}localhost` });
  await h.fns.dropUser();
  assert.equal(h.sql[0], "DROP USER 'o''brien'@'localhost'");
});

test('GRANT names the account correctly and honours WITH GRANT OPTION', async () => {
  const h = harness({ selected: `0xAB${SEP}%`, dialog: { g: 'SELECT ON d.*', wgo: true } });
  await h.fns.grantUser();
  assert.equal(h.sql[0], "GRANT SELECT ON d.* TO '0xAB'@'%' WITH GRANT OPTION");
  assert.equal(h.sql[1], 'FLUSH PRIVILEGES');
});

test('REVOKE names the account correctly', async () => {
  const h = harness({ selected: `o'brien${SEP}%`, dialog: { g: 'SELECT ON d.*' } });
  await h.fns.revokeUser();
  assert.equal(h.sql[0], "REVOKE SELECT ON d.* FROM 'o''brien'@'%'");
});

test('ALTER USER ... ACCOUNT LOCK/UNLOCK names the account correctly', async () => {
  const lock = harness({ selected: `0xAB${SEP}%` });
  await lock.fns.lockUser(true);
  assert.equal(lock.sql[0], "ALTER USER '0xAB'@'%' ACCOUNT LOCK");

  const unlock = harness({ selected: `0xAB${SEP}%` });
  await unlock.fns.lockUser(false);
  assert.equal(unlock.sql[0], "ALTER USER '0xAB'@'%' ACCOUNT UNLOCK");
});

test('nothing is sent when no user is selected', async () => {
  const h = harness({ selected: null });
  await h.fns.dropUser();
  await h.fns.grantUser();
  await h.fns.revokeUser();
  await h.fns.lockUser(true);
  assert.deepEqual(h.sql, []);
});

// --- DDL apply: what the user is told when a multi-statement script fails partway ------------
// MySQL implicitly commits every DDL statement, so a script that fails on statement 2 has already
// applied statement 1 and nothing can undo it. Verified against a live server: a two-ALTER batch
// whose second statement failed left the first one's column in place. The grid's data edits ARE
// all-or-nothing and say so, which is exactly why this difference needs stating.
test('a multi-statement DDL failure warns that earlier statements already applied', () => {
  const note = new Function(extractFunction(html, 'ddlFailureNote') + '\nreturn ddlFailureNote;')();
  const err = "ERROR 1060 (42S21) at line 3: Duplicate column name 'c'";
  const multi = note(err, 'ALTER TABLE t ADD COLUMN c INT;\nALTER TABLE t ADD COLUMN c INT;');
  assert.ok(multi.startsWith(err), 'the server error must still come first');
  assert.match(multi, /cannot be rolled back/);
});

test('a single-statement DDL failure is not given that warning', () => {
  // One statement that failed applied nothing, so the caveat would be both wrong and alarming.
  const note = new Function(extractFunction(html, 'ddlFailureNote') + '\nreturn ddlFailureNote;')();
  const err = 'ERROR 1146: no such table';
  assert.equal(note(err, 'ALTER TABLE t ADD COLUMN c INT;'), err);
  assert.equal(note(err, 'ALTER TABLE t ADD COLUMN c INT'), err);
});

// --- binary cell editing: hex in, hex out ----------------------------------------------------
// A blob in a real database was found holding 307 bytes of hex-dump TEXT where a 102-byte hash
// belonged. The value editor opens a binary cell in Text mode when the bytes decode as UTF-8, and
// Text mode runs textToHex() over the box - so a hex value pasted there is stored as the
// characters "0x24.." rather than the bytes they denote. These pin both directions of the guard.
const hexFns = () => new Function(
  extractFunction(html, 'normalizeHexInput') + '\n' +
  extractFunction(html, 'looksLikePastedHex') + '\nreturn {normalizeHexInput,looksLikePastedHex};')();

test('hex copied from this app is accepted as-is', () => {
  const { normalizeHexInput } = hexFns();
  assert.equal(normalizeHexInput('0x00ff10'), '0x00ff10');
  assert.equal(normalizeHexInput('0x00FF10'), '0x00ff10');
});

test('hex copied from Workbench survives its formatting', () => {
  const { normalizeHexInput } = hexFns();
  // Workbench's hex view separates bytes, and a long value wraps across lines. Neither should
  // matter, and the 0x prefix it omits should not either.
  assert.equal(normalizeHexInput('24 37 24 43'), '0x24372443');
  assert.equal(normalizeHexInput('2437\n2443'), '0x24372443');
  assert.equal(normalizeHexInput('  0x24 37\t24 43  '), '0x24372443');
  assert.equal(normalizeHexInput('24372443'), '0x24372443');
});

test('input that is not usable hex is rejected rather than silently mangled', () => {
  const { normalizeHexInput } = hexFns();
  // hexToBytes() parseInts each pair, so "zz" used to become byte 0 - a hole in the data.
  assert.equal(normalizeHexInput('0xzz'), null);
  assert.equal(normalizeHexInput('$7$C6..../....'), null);
  // Half a byte is not a value.
  assert.equal(normalizeHexInput('0x123'), null);
  assert.equal(normalizeHexInput('24 37 2'), null);
  // An empty box means an empty value, not an error.
  assert.equal(normalizeHexInput(''), '0x');
  assert.equal(normalizeHexInput('0x'), '0x');
});

// The shape that actually corrupted two blobs in a real database: a copied cell pasted WITHOUT
// first selecting what was in the box, so the hex ends up alongside the old value rather than
// replacing it. The first version of this guard was anchored ^...$ and stayed silent for exactly
// this - it only noticed a box containing nothing but hex.
test('hex pasted alongside existing text is recognised, not just a clean paste', () => {
  const { looksLikePastedHex } = hexFns();
  const hex = '0x24372443362e2e2e2e2f2e2e2e2e65306b307751397a566d78426c66416c67353867';
  const text = '$7$C6..../....RYngpNxfC6t.r9JyBynUxwywkD8T/MbQx7QQl.Acjv.';
  assert.equal(looksLikePastedHex(hex + text), true, 'hex pasted in front of the old value');
  assert.equal(looksLikePastedHex(text + hex), true, 'hex pasted after the old value');
  // ...while ordinary text mentioning a short hex number is left alone.
  assert.equal(looksLikePastedHex('the 0xAB flag is set'), false);
  assert.equal(looksLikePastedHex('value: 0x1234 and 0x5678'), false);
  assert.equal(looksLikePastedHex(text), false, 'the decoded value itself must never warn');
});

test('a hex value pasted into the Text tab is recognised', () => {
  const { looksLikePastedHex } = hexFns();
  assert.equal(looksLikePastedHex('0x24372443362e2e2e'), true);
  assert.equal(looksLikePastedHex('  0x2437 2443 362e 2e2e  '), true);
  // The actual decoded value must NOT trip it - that is the normal thing to save from Text mode.
  assert.equal(looksLikePastedHex('$7$C6..../....RYngpNxf'), false);
  assert.equal(looksLikePastedHex('hello world'), false);
  // A bare run of hex digits is very often a genuine value (an MD5 written as text), so only the
  // 0x-prefixed form is flagged.
  assert.equal(looksLikePastedHex('d41d8cd98f00b204e9800998ecf8427e'), false);
  // Too short to be worth second-guessing.
  assert.equal(looksLikePastedHex('0x24'), false);
});

// An empty binary cell must store nothing, not the two characters "0x".
// textToHex('') and normalizeHexInput('') both yield "0x" - zero digits. That is not valid SQL,
// and lit()'s hex passthrough requires at least one digit, so it used to fall through to being
// quoted: clearing a BLOB stored the literal characters 0 and x. Found by round-tripping every
// kind of input through a live server and comparing HEX(col) to the bytes that went in.
test('an empty binary value is stored as nothing, not as the characters 0x', () => {
  // Drives the real hexCellValueForSave - the function getVal() actually calls - so removing the
  // empty-value rule from the app breaks this. An earlier version restated the rule here instead,
  // and would have gone on passing without it.
  const src = ['strLit','lit','bytesToHex','textToHex','hexToBytes','normalizeHexInput','hexCellValueForSave']
    .map(n => extractFunction(html, n)).join('\n');
  const F = new Function(src + '\nreturn {lit,textToHex,normalizeHexInput,hexCellValueForSave};')();

  // Both tabs produce a digit-less "0x" for an empty box...
  assert.equal(F.textToHex(''), '0x');
  assert.equal(F.normalizeHexInput(''), '0x');
  // ...which lit() would quote, storing the characters 0 and x.
  assert.equal(F.lit('0x'), "'0x'");
  // So the save path must never hand it over:
  assert.equal(F.hexCellValueForSave('text', ''), '', 'an empty Text box saves an empty value');
  assert.equal(F.hexCellValueForSave('hex', ''), '', 'an empty Hex box saves an empty value');
  assert.equal(F.lit(F.hexCellValueForSave('text', '')), "''");
  // A real value passes through untouched.
  assert.equal(F.hexCellValueForSave('hex', '0x00'), '0x00');
  assert.equal(F.hexCellValueForSave('text', 'hi'), '0x6869');
});

// --- copy/paste safety: no route may corrupt ---------------------------------------------------
// Two blobs in a real database were lost to the same move - copy a cell, paste it into another
// cell - twice, because each fix only covered one shape of it. So this walks every combination of
// what "Copy value" produces and where it can be pasted, and asserts each one is either exactly
// right or refused. Nothing in between.
//
// The rule being enforced: a cell's bytes must survive copy -> paste unchanged, or the save must
// not happen.
test('every copy-then-paste route either round-trips exactly or is refused', () => {
  const src = ['strLit', 'lit', 'bytesToHex', 'textToHex', 'hexToBytes', 'hexToStrictText',
               'normalizeHexInput', 'looksLikePastedHex', 'hexCellValueForSave', 'cellCopyValue']
    .map(n => extractFunction(html, n)).join('\n');
  const F = new Function('MAX_HEXTEXT_BYTES', src +
    '\nreturn {cellCopyValue,looksLikePastedHex,normalizeHexInput,hexCellValueForSave,textToHex};')(1 << 20);

  // What the grid holds for a cell: binary is always the 0x.. display form.
  const textLike = '0x' + [...new TextEncoder().encode('$7$C6..../....RYngpNxf')]
    .map(b => b.toString(16).padStart(2, '0')).join('');
  const realBinary = '0x00ff10fe';           // not valid UTF-8 - hex is its only representation
  const plainText = 'ordinary text value';   // a non-binary column

  // Simulates a save: returns the bytes that would land, or null when the app refuses.
  const save = (mode, box) => {
    if (F.looksLikePastedHex(box)) {
      if (mode === 'text') {
        // Whole box is hex -> offered as bytes (the user confirms, and it becomes a hex save).
        const whole = F.normalizeHexInput(box);
        if (whole === null) return null;      // mixed: refused outright
        mode = 'hex'; box = whole;
      }
    }
    if (mode === 'hex' && F.normalizeHexInput(box) === null) return null;
    return F.hexCellValueForSave(mode, box);
  };
  const bytesOf = (stored) => stored === '' ? '' : (/^0x/.test(stored) ? stored.toLowerCase() : F.textToHex(stored).toLowerCase());

  for (const [label, cell] of [['text-like binary', textLike], ['non-UTF-8 binary', realBinary]]) {
    const copied = F.cellCopyValue(cell);
    for (const mode of ['text', 'hex']) {
      const stored = save(mode, copied);
      if (stored === null) continue;                       // refused: safe by definition
      assert.equal(bytesOf(stored), cell.toLowerCase(),
        `${label}: copy -> paste into the ${mode} tab changed the bytes`);
    }
    // "Copy value as hex" always yields the raw display form, and it must round-trip too.
    const stored = save('hex', cell);
    assert.equal(bytesOf(stored), cell.toLowerCase(), `${label}: copy-as-hex -> Hex tab changed the bytes`);
  }

  // A hex value pasted ALONGSIDE an existing value - the move that actually caused the loss -
  // must be refused from the Text tab rather than stored as characters.
  assert.equal(save('text', textLike + '$7$C6..../....RYngpNxf'), null,
    'hex pasted in front of the old value must be refused');
  assert.equal(save('text', '$7$C6..../....RYngpNxf' + textLike), null,
    'hex pasted after the old value must be refused');

  // And an ordinary text value still saves untouched.
  assert.equal(save('text', plainText), F.textToHex(plainText));
});

test('copying a cell yields something that pastes back as the same bytes', () => {
  const src = ['bytesToHex', 'hexToBytes', 'hexToStrictText', 'cellCopyValue']
    .map(n => extractFunction(html, n)).join('\n');
  const F = new Function('MAX_HEXTEXT_BYTES', src + '\nreturn {cellCopyValue};')(1 << 20);
  // Text-like bytes copy as their text, so a paste into the Text tab reproduces them.
  assert.equal(F.cellCopyValue('0x6869'), 'hi');
  // Bytes that are not text keep the hex, which only the Hex tab will accept.
  assert.equal(F.cellCopyValue('0x00ff10fe'), '0x00ff10fe');
  // Non-binary cells are untouched, and NULL copies as empty rather than the word null.
  assert.equal(F.cellCopyValue('plain'), 'plain');
  assert.equal(F.cellCopyValue(null), '');
});

// --- the OTHER way into a binary column --------------------------------------------------------
// The value editor is not the only route: you can type or paste straight into a grid cell, and
// that never opens the editor, so the guard there never ran. A blob in a real database was
// destroyed through this path after the editor had already been fixed - it ended up holding 919
// bytes of nested hex text where a 102-byte hash belonged.
//
// applyChanges() screens staged edits before building any SQL (it already did so for BIT columns),
// which is the one choke point both inline edits and new rows pass through. These drive that
// screening function directly.
test('a grid edit with hex mixed into a binary cell is refused before anything is written', () => {
  // Drives the real pastedHexColumns - the function applyChanges calls - with a stand-in tab.
  const src = ['bytesToHex','hexToBytes','normalizeHexInput','looksLikePastedHex','pastedHexColumns']
    .map(n => extractFunction(html, n)).join('\n');
  const F = new Function(src + '\nreturn pastedHexColumns;')();
  const tab = (val) => ({ cols: ['id','data'], binCols: [false, true],
                          pending: { upd: { '0:1': val }, ins: [] } });

  const hex  = '0x24372443362e2e2e2e2f2e2e2e2e65306b307751397a566d78426c66416c67353867';
  const hash = '$7$C6..../....hMYEng9e5.w8dP2TZwBhx.NwI9';

  // The pastes that caused the loss, in both orders, and the doubly-encoded form that followed.
  assert.deepEqual(F(tab(hex + hash)), ['data'], 'hex pasted in front of the cell contents');
  assert.deepEqual(F(tab(hash + hex)), ['data'], 'hex pasted after the cell contents');
  assert.deepEqual(F(tab(hex + hex + hash)), ['data'], 'hex pasted twice, then the old value');

  // Clean hex is how you legitimately set bytes from the grid: lit() passes it through unquoted.
  assert.deepEqual(F(tab('0x00ff10')), [], 'a clean hex value must still be allowed');
  assert.deepEqual(F(tab(hex)), [], 'a clean copied cell must still be allowed');
  assert.deepEqual(F(tab(hash)), [], 'the decoded value itself');
  assert.deepEqual(F(tab('')), [], 'an emptied cell');
  assert.deepEqual(F(tab(null)), [], 'a cell set to NULL');

  // A NEW row goes through the same screen.
  const insTab = { cols: ['id','data'], binCols: [false, true],
                   pending: { upd: {}, ins: [{ data: hex + hash }] } };
  assert.deepEqual(F(insTab), ['data'], 'a new row with a bad paste is screened too');

  // A non-binary column is left alone - 0x.. is not this app's encoding there.
  const textTab = { cols: ['id','note'], binCols: [false, false],
                    pending: { upd: { '0:1': hex + hash }, ins: [] } };
  assert.deepEqual(F(textTab), [], 'a text column is not screened');
});

test('applyChanges actually calls that screen', () => {
  // The logic above can be perfect and still never run. This pins the wiring: an earlier version
  // of this test checked a restatement of the rule and kept passing with the guard deleted.
  const body = extractFunction(html, 'applyChanges');
  assert.match(body, /pastedHexColumns\(/, 'applyChanges must screen staged edits before building SQL');
  const callAt = body.indexOf('pastedHexColumns(');
  const sqlAt = body.indexOf('UPDATE ');
  assert.ok(callAt < sqlAt, 'the screen must run before any SQL is built');
});

// --- date and time cells: the picker must not quietly reshape a value -------------------------
// The native date/time inputs cannot represent everything MySQL stores. A DATETIME(6) of
// 2024-01-01 12:34:56.123456 converts cleanly to 2024-01-01T12:34:56, and saving that back
// dropped the fractional seconds silently - the same shape as every other bug found here, a
// conversion between two representations that loses part of the value on the way.
//
// The rule now: use the picker only for a value it hands back unchanged. Anything else gets the
// plain text editor, where it is edited exactly as stored. These check the rule and the values
// that drove it, all verified against what MariaDB actually returns for those column types.
test('the date picker is offered only when it round-trips the stored value', () => {
  const F = new Function(extractFunction(html, 'mysqlToNativeDate') + '\n' +
                         extractFunction(html, 'nativeDateToMysql') +
                         '\nreturn {mysqlToNativeDate,nativeDateToMysql};')();
  // The test editWidgetFor applies.
  const usesPicker = (v, t) => {
    if (v == null || v === '') return true;
    const n = F.mysqlToNativeDate(v, t);
    return !!(n && F.nativeDateToMysql(n, t) === String(v));
  };

  // Values the picker represents exactly: offered, and lossless.
  for (const [t, v] of [['datetime-local', '2024-01-01 12:34:56'],
                        ['date', '2024-02-29'],
                        ['time', '12:34:56']]) {
    assert.equal(usesPicker(v, t), true, `${v} should use the picker`);
    assert.equal(F.nativeDateToMysql(F.mysqlToNativeDate(v, t), t), v, `${v} must survive the round trip`);
  }

  // Fractional seconds: the picker drops them, so it must not be offered.
  assert.equal(usesPicker('2024-01-01 12:34:56.123456', 'datetime-local'), false, 'DATETIME(6)');
  assert.equal(usesPicker('2024-01-01 12:34:56.500', 'datetime-local'), false, 'DATETIME(3)');
  assert.equal(usesPicker('12:34:56.789', 'time'), false, 'TIME(3)');

  // MySQL TIME spans -838:59:59 to 838:59:59 - well outside a clock - and allows a zero date.
  assert.equal(usesPicker('838:59:59', 'time'), false, 'a TIME beyond 24 hours');
  assert.equal(usesPicker('-01:30:00', 'time'), false, 'a negative TIME');
  assert.equal(usesPicker('0000-00-00', 'date'), false, 'the zero date');
  assert.equal(usesPicker('0000-00-00 00:00:00', 'datetime-local'), false, 'the zero datetime');

  // An empty or absent value has nothing to lose, so the picker is fine.
  assert.equal(usesPicker(null, 'date'), true);
  assert.equal(usesPicker('', 'date'), true);
});

// Row values are written for their column's type once it is known. lit() goes by the value's shape,
// so a text cell holding 0x41 became the byte A, and an empty binary value - shown as the bare 0x -
// became the two characters 0x. And a CR is escaped: mysql.exe reading a script turns CR LF into
// LF, which silently dropped the CR from any value that had one before a line feed.
test('row values are written for their column type, and CR survives a script', () => {
  const L = new Function(['strLit', 'lit', 'litAs'].map(n => extractFunction(html, n)).join('\n') +
    '\nreturn {strLit, lit, litAs};')();
  assert.equal(L.strLit('a\r\nb'), "'a\\r\nb'", 'a CR is written as \\r');
  assert.equal(L.strLit('a\0b'), "'a\\0b'", 'a NUL is written as \\0');
  assert.equal(L.litAs('0x41', false), "'0x41'", 'text that looks like hex stays text');
  assert.equal(L.litAs('0x41', true), '0x41', 'hex in a binary column is a hex literal');
  assert.equal(L.litAs('0x', true), "X''", 'an empty binary value is empty');
  assert.equal(L.litAs('0x', false), "'0x'", 'in a text column 0x is the two characters');
  assert.equal(L.litAs(null, true), 'NULL');
  assert.equal(L.litAs('NULL', false), "'NULL'");
  assert.equal(L.litAs('0x41', null), '0x41', 'with the type unknown it is lit(), as before');
  const apply = extractFunction(html, 'applyChanges');
  assert.match(apply, /keyWhere\(t,ri,bc,kt\)/, 'applyChanges finds rows through keyWhere');
  assert.match(extractFunction(html, 'keyWhere'), /litAs\(v,bc\?bc\[ci\]:null\)/, 'which writes row keys by column type');
  assert.match(apply, /return litAs\(v,bc\?bc\[ci\]:null\)/, 'changed cells go through litAs');
  for (const f of ['insGrid', 'insSel', 'exportFull']) {
    assert.ok(extractFunction(html, f).includes('litAs('), f + ' writes rows by column type');
  }
});

// "Go to referenced row" opened the whole referenced table: openRun() rebuilds the query from the
// table and its filters, and dropped the WHERE written into the tab. Both it and the quick filter
// also wrote the value by its shape, so an empty binary key (0x) and hex-looking text found nothing.
test('following a foreign key, and the quick filter, find the right rows', async () => {
  const SRC = html;
  const CHECK = (c, l, d) => assert.ok(c, l + ' -> ' + d);
  const names = ['goToFkRow', 'qfSub', 'litAs', 'lit', 'strLit'];
  const body = names.map(n => extractFunction(SRC, n)).join('\n');
  const make = (bin) => {
    const tab = { cols: ['id', 'v'], filterClauses: null };
    const seen = { opened: null, run: null, clauses: [] };
    const env = {
      qid: s => '`' + s + '`', T: () => tab,
      tableBinCols: async () => [bin], gridBinCols: async () => [bin, false],
      openTab: (title, sql) => { seen.opened = sql; return 't1'; },
      openRun: async () => { seen.run = [...(tab.filterClauses || [])]; },
      addFilterClause: async (id, c) => { seen.clauses.push(c); },
    };
    const keys = Object.keys(env);
    const f = new Function(...keys, body + '\nreturn {goToFkRow, qfSub};')(...keys.map(k => env[k]));
    return { f, seen };
  };
  const b = make(true);
  await b.f.goToFkRow('d', 'p', 'id', '0x');
  CHECK(b.seen.run && b.seen.run.join() === "`id`=X''", 'the referenced row is found by its filter, and an empty binary key is X\'\'', JSON.stringify(b.seen));
  const t = make(false);
  await t.f.goToFkRow('d', 'tp', 'code', '0x41');
  CHECK(t.seen.run && t.seen.run.join() === "`code`='0x41'", 'a text key that looks like hex stays text', JSON.stringify(t.seen));
  const q = make(true);
  const sub = q.f.qfSub('t1', 'id', '0x');
  await sub.find(x => Array.isArray(x) && / = /.test(x[0]))[1]();
  await sub.find(x => Array.isArray(x) && / != /.test(x[0]))[1]();
  CHECK(q.seen.clauses.join(' ; ') === "`id` = X'' ; `id` <> X''", 'the quick filter writes the value for its column type', q.seen.clauses.join(' ; '));
});

test('editWidgetFor actually applies that round-trip test', () => {
  // The rule above can be right and never run. This pins the wiring.
  const body = extractFunction(html, 'editWidgetFor');
  assert.match(body, /nativeDateToMysql\(/,
    'editWidgetFor must convert back and compare, not just check the conversion produced something');
});

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
  const { normalizeHexInput } = hexFns();
  const lit = new Function(extractFunction(html, 'strLit') + '\n' + extractFunction(html, 'lit') + '\nreturn lit;')();
  const textToHex = new Function(extractFunction(html, 'bytesToHex') + '\n' + extractFunction(html, 'textToHex') + '\nreturn textToHex;')();

  // Both tabs produce "0x" for an empty box...
  assert.equal(textToHex(''), '0x');
  assert.equal(normalizeHexInput(''), '0x');
  // ...and "0x" must never reach lit(), because lit() would quote it.
  assert.equal(lit('0x'), "'0x'", 'lit() quotes a digit-less 0x - which is why getVal maps it to empty first');
  // The mapping getVal applies:
  const forEmpty = (h) => (h === null || h === '0x') ? '' : h;
  assert.equal(lit(forEmpty(textToHex(''))), "''", 'an empty Text box stores an empty value');
  assert.equal(lit(forEmpty(normalizeHexInput(''))), "''", 'an empty Hex box stores an empty value');
  // A real one-byte value is untouched by that mapping.
  assert.equal(lit(forEmpty(normalizeHexInput('0x00'))), '0x00');
});

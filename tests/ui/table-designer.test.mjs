// The table designer's column handling, run against real information_schema rows.
//
// The designer used to rebuild a column from DATA_TYPE plus one length number, so any edit to an
// existing column - even just its comment - rewrote whatever the form does not show. Measured on
// MySQL 8.0.46 from a comment-only edit: DECIMAL(10,2) 12.34 became DECIMAL(10,0) 12, INT UNSIGNED
// became signed, DATETIME(6) lost its microseconds, a latin1 column was re-encoded to utf8mb4, and
// an ENUM became the syntax error ENUM(5). Renaming a column was DROP + ADD, which throws its data
// away. On MariaDB, whose information_schema quotes string defaults, a touched default would have
// gained a second layer of quotes.
//
// FIXTURES below are verbatim information_schema.COLUMNS rows for one deliberately awkward table,
// captured from MySQL 8.0.46 and MariaDB 12.2.2. The same table was then edited through the real
// GUI on both servers, and SHOW CREATE TABLE and the data were identical afterwards.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const html = readFileSync(process.env.NOBS_UI_SOURCE ||
  join(dirname(fileURLToPath(import.meta.url)), '../../ui/index.html'), 'utf8');

function extract(src, name) {
  let start = src.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `${name}() not found`);
  let depth = 0;
  for (let j = src.indexOf('{', start); j < src.length; j++) {
    if (src[j] === '{') depth++;
    else if (src[j] === '}' && --depth === 0) return src.slice(start, j + 1);
  }
  throw new Error('unbalanced braces in ' + name);
}
const consts = html.split(/\r?\n/).filter(l => /^const D_(TEXTY|NUMERIC|TEMPORAL)=/.test(l)).join('\n');
const D = new Function(
  'const RESERVED=new Set(["key","order","select"]);\n' + consts + '\n' +
  ['qid', 'strLit', 'lit', 'dColFromInfo', 'colDef', 'dAlterSql'].map(n => extract(html, n)).join('\n') +
  '\nreturn {dColFromInfo,colDef,dAlterSql};')();

const FIXTURES = [
  {version:"8.0.46", mariadb:false, tableColl:"utf8mb4_0900_ai_ci", rows:[
    ["id","int","int","NO",null,"auto_increment","PRI","",null,null,""],
    ["amount","decimal","decimal(10,2)","NO","0.00","","","",null,null,""],
    ["big","int","int unsigned","NO",null,"","","",null,null,""],
    ["zf","smallint","smallint(5) unsigned zerofill","YES",null,"","","",null,null,""],
    ["ts","datetime","datetime(6)","YES","CURRENT_TIMESTAMP(6)","DEFAULT_GENERATED on update CURRENT_TIMESTAMP(6)","","",null,null,""],
    ["kind","enum","enum('al)pha','beta')","NO","beta","","","","utf8mb4","utf8mb4_0900_ai_ci",""],
    ["tags","set","set('x','y')","YES",null,"","","","utf8mb4","utf8mb4_0900_ai_ci",""],
    ["name","varchar","varchar(20)","YES","it's ok","","","","latin1","latin1_german2_ci",""],
    ["empty_def","varchar","varchar(5)","NO","","","","","utf8mb4","utf8mb4_0900_ai_ci",""],
    ["hidden","int","int","YES",null,"INVISIBLE","","",null,null,""],
    ["gen","bigint","bigint","YES",null,"VIRTUAL GENERATED","","",null,null,"(`big` + 1)"],
    ["uid","char","char(36)","YES","uuid()","DEFAULT_GENERATED","","","utf8mb4","utf8mb4_0900_ai_ci",""],
    ["tt","tinytext","tinytext","YES",null,"","","","utf8mb4","utf8mb4_0900_ai_ci",""],
    ["bl","mediumblob","mediumblob","YES",null,"","","",null,null,""],
    ["bits","bit","bit(3)","YES","b'101'","","","",null,null,""],
    ["f","double","double unsigned","YES",null,"","","",null,null,""]]},
  {version:"12.2.2-MariaDB", mariadb:true, tableColl:"utf8mb4_uca1400_ai_ci", rows:[
    ["id","int","int(11)","NO",null,"auto_increment","PRI","",null,null,null],
    ["amount","decimal","decimal(10,2)","NO","0.00","","","",null,null,null],
    ["big","int","int(10) unsigned","NO",null,"","","",null,null,null],
    ["zf","smallint","smallint(5) unsigned zerofill","YES","NULL","","","",null,null,null],
    ["ts","datetime","datetime(6)","YES","current_timestamp(6)","on update current_timestamp(6)","","",null,null,null],
    ["kind","enum","enum('al)pha','beta')","NO","'beta'","","","","utf8mb4","utf8mb4_uca1400_ai_ci",null],
    ["tags","set","set('x','y')","YES","NULL","","","","utf8mb4","utf8mb4_uca1400_ai_ci",null],
    ["name","varchar","varchar(20)","YES","'it''s ok'","","","","latin1","latin1_german2_ci",null],
    ["empty_def","varchar","varchar(5)","NO","''","","","","utf8mb4","utf8mb4_uca1400_ai_ci",null],
    ["hidden","int","int(11)","YES","NULL","INVISIBLE","","",null,null,null],
    ["gen","bigint","bigint(20)","YES","NULL","VIRTUAL GENERATED","","",null,null,"`big` + 1"],
    ["uid","char","char(36)","YES","uuid()","","","","utf8mb4","utf8mb4_uca1400_ai_ci",null],
    ["tt","tinytext","tinytext","YES","NULL","","","","utf8mb4","utf8mb4_uca1400_ai_ci",null],
    ["bl","mediumblob","mediumblob","YES","NULL","","","",null,null,null],
    ["bits","bit","bit(3)","YES","b'101'","","","",null,null,null],
    ["f","double","double unsigned","YES","NULL","","","",null,null,null]]}
];

const read = (f) => f.rows.map(r => D.dColFromInfo(r, f.mariadb, f.tableColl));
const byName = (cols, n) => cols.find(c => c.name === n);
const clone = (cols) => cols.map(c => ({ ...c, keep: c.keep }));
const TBL = 'nobs_gui.awk';

for (const f of FIXTURES) {
  const label = f.mariadb ? `MariaDB ${f.version}` : `MySQL ${f.version}`;

  test(`${label}: every column is written back with what the form does not show`, () => {
    const cols = read(f);
    const def = n => D.colDef(byName(cols, n));
    assert.match(def('amount'), /^amount DECIMAL\(10,2\) NOT NULL DEFAULT /, 'the DECIMAL scale was lost');
    assert.match(def('big'), /^big INT(\(10\))? UNSIGNED NOT NULL$/, 'UNSIGNED was lost');
    assert.match(def('zf'), /^zf SMALLINT\(5\) UNSIGNED ZEROFILL/, 'ZEROFILL was lost');
    assert.match(def('ts'), /^ts DATETIME\(6\) DEFAULT current_timestamp\(6\) ON UPDATE current_timestamp\(6\)$/i,
      'fractional seconds, the expression default or ON UPDATE was lost');
    assert.match(def('kind'), /^kind ENUM\('al\)pha','beta'\) NOT NULL DEFAULT 'beta'$/,
      'the ENUM value list was lost or cut at the ) inside a value');
    assert.match(def('tags'), /^tags SET\('x','y'\)/);
    assert.match(def('name'), /^name VARCHAR\(20\) CHARACTER SET latin1 COLLATE latin1_german2_ci DEFAULT 'it''s ok'$/,
      'a non-default character set, or the quoted default, was not kept exactly');
    assert.match(def('empty_def'), /^empty_def VARCHAR\(5\) NOT NULL DEFAULT ''$/,
      'an empty-string default is a default, not "no default"');
    assert.match(def('hidden'), /INVISIBLE$/);
    assert.match(def('uid'), /^uid CHAR\(36\) DEFAULT \(?uuid\(\)\)?$/, 'the expression default was quoted into a string');
    assert.match(def('bits'), /^bits BIT\(3\) DEFAULT b'101'$/, 'a bit default was quoted into a string');
    assert.match(def('f'), /^f DOUBLE UNSIGNED/);
    // Columns that merely inherit the table's collation must not grow a clause of their own, or
    // SHOW CREATE TABLE - and Compare DB - would read them as changed.
    for (const n of ['kind', 'tags', 'empty_def', 'uid', 'tt'])
      assert.doesNotMatch(def(n), /CHARACTER SET/, `${n} inherits the table collation and must not get a clause`);
  });

  test(`${label}: the form shows defaults as a person would type them`, () => {
    const cols = read(f);
    assert.equal(byName(cols, 'name').def, "it's ok", 'a quoted default should be shown unquoted');
    assert.equal(byName(cols, 'kind').def, 'beta');
    assert.equal(byName(cols, 'empty_def').def, '');
  });

  test(`${label}: an untouched table produces nothing`, () => {
    const cols = read(f);
    assert.equal(D.dAlterSql(cols, clone(cols), TBL), '-- no changes detected');
  });

  test(`${label}: a comment-only edit changes comments and nothing else`, () => {
    const orig = read(f);
    const edited = clone(orig).map(c => ({ ...c, comment: 'c-' + c.name }));
    const sql = D.dAlterSql(orig, edited, TBL);
    for (const c of orig) {
      if (c.keep.generated) continue;
      const line = sql.split('\n').find(l => l.includes('MODIFY COLUMN ' + c.name + ' '));
      assert.ok(line, `${c.name} was not modified`);
      assert.equal(line.replace(/^\s*MODIFY COLUMN /, '').replace(/,$|;$/, ''),
        D.colDef({ ...c, comment: 'c-' + c.name }), `${c.name}: the MODIFY is not the column as it was, plus the comment`);
    }
    // A generated column is left alone, and the SQL says so.
    assert.match(sql, /^-- gen is a generated column/m);
    assert.doesNotMatch(sql, /COLUMN gen /);
  });

  test(`${label}: renaming a column is CHANGE COLUMN, never DROP + ADD`, () => {
    const orig = read(f);
    const edited = clone(orig).map(c => c.name === 'name' ? { ...c, name: 'full_name' } : c);
    const sql = D.dAlterSql(orig, edited, TBL);
    assert.match(sql, /CHANGE COLUMN name full_name VARCHAR\(20\) CHARACTER SET latin1/);
    assert.doesNotMatch(sql, /DROP COLUMN/, 'a rename must not drop the column and its data');
    assert.doesNotMatch(sql, /ADD COLUMN/);
    // The primary key is not disturbed by renaming it either.
    const renPk = clone(orig).map(c => c.name === 'id' ? { ...c, name: 'row_id' } : c);
    const pkSql = D.dAlterSql(orig, renPk, TBL);
    assert.match(pkSql, /CHANGE COLUMN id row_id INT/);
    assert.doesNotMatch(pkSql, /PRIMARY KEY/, 'renaming the key column must not drop and re-add the key');
  });

  test(`${label}: removed and added columns are still dropped and added`, () => {
    const orig = read(f);
    const edited = clone(orig).filter(c => c.name !== 'tt');
    edited.push({ name: 'extra', type: 'VARCHAR', len: '10', nn: false, ai: false, pk: false, def: '', comment: '' });
    const sql = D.dAlterSql(orig, edited, TBL);
    assert.match(sql, /DROP COLUMN tt/);
    assert.match(sql, /ADD COLUMN extra VARCHAR\(10\)/);
  });

  test(`${label}: changing a type drops only what no longer applies`, () => {
    const orig = read(f);
    const f2 = { ...byName(orig, 'f'), type: 'VARCHAR', len: '20' };
    assert.doesNotMatch(D.colDef(f2), /UNSIGNED/, 'UNSIGNED on a VARCHAR is a syntax error');
    const n2 = { ...byName(orig, 'name'), type: 'INT', len: '' };
    assert.doesNotMatch(D.colDef(n2), /CHARACTER SET/, 'a character set on an INT is a syntax error');
    const ts2 = { ...byName(orig, 'ts'), type: 'DATE', len: '' };
    assert.doesNotMatch(D.colDef(ts2), /ON UPDATE/, 'ON UPDATE only applies to DATETIME and TIMESTAMP');
    const big2 = { ...byName(orig, 'big'), type: 'BIGINT' };
    assert.match(D.colDef(big2), /BIGINT(\(10\))? UNSIGNED/, 'widening an unsigned integer should keep it unsigned');
  });

  test(`${label}: a default the user typed is still written the usual way`, () => {
    const orig = read(f);
    const c = { ...byName(orig, 'empty_def'), def: "o'k" };
    assert.match(D.colDef(c), /DEFAULT 'o''k'$/);
    const n = { ...byName(orig, 'amount'), def: '5' };
    assert.match(D.colDef(n), /DEFAULT 5$/);
  });
}

test('the fixtures cover both servers', () => {
  assert.deepEqual(FIXTURES.map(f => f.mariadb).sort(), [false, true]);
});

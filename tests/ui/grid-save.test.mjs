// How Apply finds the rows it changes or deletes.
//
// Apply matched a row by its key as the grid shows it and then reported success whatever that
// matched. A FLOAT key is shown rounded, so its edits matched nothing and were reported as applied.
// A TIMESTAMP key is shown in the session time zone, where the autumn hour happens twice, so
// editing one of two such rows changed the other. Each change is now preceded by a guard that stops
// the whole batch unless its row is matched exactly once.

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
  return src.slice(start, src.indexOf(';\n', start) + 1);
}

const NAMES = ['applyChanges', 'keyWhere', 'oneRowGuard', 'litAs', 'lit', 'strLit',
  'pastedHexColumns', 'looksLikePastedHex', 'normalizeHexInput'];
const bundle = extractConst(html, 'ONE_ROW_REFUSED') + '\n' + NAMES.map(n => extractFunction(html, n)).join('\n');

async function apply({ cols, pk, rows, upd = {}, del = [], types = {}, bin, reply = { ok: true } }) {
  const sent = [], toasts = [];
  const t = { db: 'd', table: 't', cols, pk, rows, binCols: [], exact: true,
              pending: { upd, del: new Set(del), ins: [] } };
  const env = {
    roBlock: () => false, T: () => t, qid: s => '`' + s + '`', log: () => {}, invalidateTableCache: () => {},
    openRun: async () => {}, refreshTabDirty: () => {},
    gridBinCols: async () => bin || cols.map(() => false),
    tableColTypes: async () => types, tableNulTextCount: async () => 0, fmtCount: String,
    toast: (m, k) => toasts.push((k === true ? 'ERR ' : '') + m),
    api: async (p, d) => { sent.push(d.sql); return typeof reply === 'function' ? reply(d.sql) : reply; },
  };
  const keys = Object.keys(env);
  await new Function(...keys, bundle + '\nreturn applyChanges;')(...keys.map(k => env[k]))('x');
  return { sql: sent.join('\n'), stmts: sent.length ? sent[0].split('\n') : [], toasts };
}
const guardOf = where => 'SELECT 1 FROM (SELECT 1 AS x UNION ALL SELECT 2) nobs_guard WHERE (SELECT COUNT(*) FROM `d`.`t` WHERE ' + where + ') <> 1 INTO @nobs_one_row;';

test('every update and delete is preceded by a guard on the same WHERE', async () => {
  const r = await apply({ cols: ['id', 'v'], pk: ['id'], rows: [['1', 'a'], ['2', 'b']],
                          upd: { '0:1': 'x' }, del: [1], types: { id: 'int', v: 'varchar' } });
  assert.deepEqual(r.stmts, [
    guardOf("`id`='1'"), "UPDATE `d`.`t` SET `v`='x' WHERE `id`='1' LIMIT 1;",
    guardOf("`id`='2'"), "DELETE FROM `d`.`t` WHERE `id`='2' LIMIT 1;",
  ]);
  assert.ok(r.toasts.some(m => m === 'Applied 2 change(s).'), 'the guards are not counted as changes: ' + r.toasts);
});

test('a FLOAT key is matched by its text', async () => {
  const r = await apply({ cols: ['k', 'v'], pk: ['k'], rows: [['1.1', 'a']], upd: { '0:1': 'x' }, types: { k: 'float' } });
  assert.ok(r.stmts.includes("UPDATE `d`.`t` SET `v`='x' WHERE CAST(`k` AS CHAR)='1.1' LIMIT 1;"), r.sql);
});

test('a TIMESTAMP key is matched by its text, near its time', async () => {
  const r = await apply({ cols: ['ts', 'v'], pk: ['ts'], rows: [['2026-10-25 02:30:00', 'a']], del: [0], types: { ts: 'timestamp' } });
  const where = "(`ts` BETWEEN '2026-10-25 02:30:00' - INTERVAL 3 HOUR AND '2026-10-25 02:30:00' + INTERVAL 3 HOUR AND CAST(`ts` AS CHAR)='2026-10-25 02:30:00')";
  assert.deepEqual(r.stmts, [guardOf(where), 'DELETE FROM `d`.`t` WHERE ' + where + ' LIMIT 1;']);
});

test('binary and composite keys keep their typed form', async () => {
  const r = await apply({ cols: ['a', 'b', 'v'], pk: ['a', 'b'], rows: [['0x01', '0x41', 'v']], upd: { '0:2': 'w' },
                          types: { a: 'varbinary', b: 'varchar' }, bin: [true, false, false] });
  assert.ok(r.stmts.includes("UPDATE `d`.`t` SET `v`='w' WHERE `a`=0x01 AND `b`='0x41' LIMIT 1;"), r.sql);
});

test('a result without the key column saves nothing', async () => {
  const r = await apply({ cols: ['v'], pk: ['id'], rows: [['a']], upd: { '0:0': 'x' }, types: {} });
  assert.equal(r.sql, '');
  assert.ok(r.toasts.some(m => /does not include every key column \(id\)/.test(m)), r.toasts.join());
});

test('a refused guard is explained', async () => {
  const r = await apply({ cols: ['id', 'v'], pk: ['id'], rows: [['1', 'a']], upd: { '0:1': 'x' }, types: {},
                          reply: { ok: false, error: 'ERROR 1172 (42000) at line 3: Result consisted of more than one row' } });
  assert.ok(r.toasts.some(m => m.startsWith('ERR Nothing was saved. A row you changed or deleted no longer matches exactly one row')), r.toasts.join());
});

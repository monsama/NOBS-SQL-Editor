// Recreating a procedure, function or trigger from the editor.
//
// MySQL has no CREATE OR REPLACE for these, so the editor sends DROP then CREATE - and a CREATE
// that fails leaves the object gone. Measured on MySQL 8.0.46: a syntax error in an edited
// procedure deleted the procedure, with its code surviving only in the unsaved editor tab.
// applyDdl() now checks after a failure and puts the previous definition back. These drive the
// real recovery functions with the server stubbed; the GUI was also run against MySQL 8.0.46 and
// MariaDB 12.2 end to end.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const html = readFileSync(process.env.NOBS_UI_SOURCE ||
  join(dirname(fileURLToPath(import.meta.url)), '../../ui/index.html'), 'utf8');

function extract(src, name) {
  let start = src.indexOf(`async function ${name}(`);
  if (start < 0) start = src.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `${name}() not found`);
  let depth = 0;
  for (let j = src.indexOf('{', start); j < src.length; j++) {
    if (src[j] === '{') depth++;
    else if (src[j] === '}' && --depth === 0) return src.slice(start, j + 1);
  }
  throw new Error('unbalanced braces in ' + name);
}

// A fake server: `present` says whether the object exists, `restoreWorks` whether re-running the
// old definition succeeds.
function harness({ present, restoreWorks = true }) {
  const calls = [], opened = [];
  let exists = present;
  const api = async (path, p) => {
    calls.push({ path, p });
    if (path === '/api/query') return { ok: true, rows: [[exists ? '1' : '0']] };
    if (path === '/api/script') { if (restoreWorks) { exists = true; return { ok: true }; } return { ok: false, error: 'ERROR 1227: Access denied' }; }
    if (path === '/api/ddl') return { ok: true, ddl: 'CREATE PROCEDURE p() SELECT 2' };
    return { ok: true };
  };
  const src = ['lit', 'strLit', 'ddlExists', 'ddlRememberCurrent', 'ddlRestoreIfDropped', 'ddlConfirmNew']
    .map(n => extract(html, n)).join('\n');
  const F = new Function('api', 'openTab', 'loadObjects', 'ask',
    src + '\nreturn {ddlExists, ddlRememberCurrent, ddlRestoreIfDropped, ddlConfirmNew};')(
    api, (...a) => opened.push(a), () => {}, async () => false);
  return { F, calls, opened };
}
const D = () => ({ type: 'procedure', db: 'd', name: 'p', orig: 'CREATE PROCEDURE p() SELECT 1' });

test('a failure that left the object in place changes nothing', async () => {
  const h = harness({ present: true });
  assert.equal(await h.F.ddlRestoreIfDropped(D()), '');
  assert.ok(!h.calls.some(c => c.path === '/api/script'), 'nothing should be re-run when the object is still there');
});

test('a failure after the DROP puts the previous definition back and says so', async () => {
  const h = harness({ present: false });
  const note = await h.F.ddlRestoreIfDropped(D());
  const run = h.calls.find(c => c.path === '/api/script');
  assert.ok(run, 'the previous definition was not re-run');
  assert.match(run.p.sql, /DELIMITER \$\$\nCREATE PROCEDURE p\(\) SELECT 1\$\$\nDELIMITER ;/);
  assert.equal(run.p.db, 'd');
  assert.match(note, /previous version has been put back/);
  assert.equal(h.opened.length, 0);
});

test('if putting it back fails too, the definition is opened and the message says the object is gone', async () => {
  const h = harness({ present: false, restoreWorks: false });
  const note = await h.F.ddlRestoreIfDropped(D());
  assert.match(note, /IS GONE/);
  assert.equal(h.opened.length, 1, 'the previous definition must be handed back in a tab');
  assert.match(h.opened[0][1], /CREATE PROCEDURE p\(\) SELECT 1/);
  assert.match(h.opened[0][1], /Access denied/, 'the tab should say why the restore failed');
});

test('without a known previous definition there is nothing to restore', async () => {
  const h = harness({ present: false });
  assert.equal(await h.F.ddlRestoreIfDropped({ type: 'procedure', db: 'd', name: 'p' }), '');
  assert.equal(await h.F.ddlRestoreIfDropped({ type: 'view', db: 'd', name: 'v', orig: 'x' }), '',
    'a view is recreated with CREATE OR REPLACE on both servers and never dropped first');
});

test('a successful apply becomes the new fallback', async () => {
  const h = harness({ present: true });
  const d = D();
  await h.F.ddlRememberCurrent(d);
  assert.equal(d.orig, 'CREATE PROCEDURE p() SELECT 2');
});

test('New with an existing name asks before replacing it', async () => {
  const taken = harness({ present: true });
  assert.equal(await taken.F.ddlConfirmNew('d', 'procedure', 'p'), false, 'declining must stop the replacement');
  const free = harness({ present: false });
  assert.equal(await free.F.ddlConfirmNew('d', 'procedure', 'p'), true, 'a fresh name should not prompt at all');
});

test('applyDdl is wired to all of this', () => {
  const body = extract(html, 'applyDdl');
  assert.match(body, /ddlRestoreIfDropped\(/, 'a failed apply must check whether the object was dropped');
  assert.match(body, /ddlRememberCurrent\(/, 'a successful apply must refresh the fallback');
  assert.match(extract(html, 'openDdl'), /orig:r\.ddl/, 'opening an object must remember its definition');
  for (const fn of ['newProcedure', 'newFunction', 'newTrigger'])
    assert.match(extract(html, fn), /ddlConfirmNew\(/, `${fn} must ask before replacing an existing object`);
});

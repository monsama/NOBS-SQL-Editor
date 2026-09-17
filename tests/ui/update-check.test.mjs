// The "new version available" notice.
//
// It must say so when a newer release exists, stay quiet about a version the user hid, never ask
// when switched off, and say nothing at startup when the check fails - a machine without internet
// is not an error worth a toast. Opening the page goes through the backend in the desktop build,
// which only opens this app's own release pages.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

// NOBS_UI_SOURCE lets the PowerShell edition run this same file against NOBSSQL.ps1, which carries
// the identical UI inline.
const html = readFileSync(process.env.NOBS_UI_SOURCE ||
  join(dirname(fileURLToPath(import.meta.url)), '../../ui/index.html'), 'utf8');

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

const NAMES = ['updateCheckOn', 'setUpdateCheck', 'checkForUpdate', 'openUpdatePage', 'dismissUpdate'];
const bundle = 'let _update=null;\n' + NAMES.map(n => extractFunction(html, n)).join('\n');

function harness({ reply = { ok: true, current: '1.2.0', latest: '1.3.0', newer: true, url: 'https://github.com/x/releases/tag/v1.3.0' },
                   store = {}, tauri = false } = {}) {
  const calls = [], toasts = [], opened = [];
  const els = { updNote: { style: { display: 'none' } }, updLink: { textContent: '', title: '' } };
  const env = {
    $: id => els[id],
    api: async (path, body) => { calls.push([path, body]); return typeof reply === 'function' ? reply(path) : reply; },
    toast: (m, k) => toasts.push([m, k]),
    localStorage: { getItem: k => (k in store ? store[k] : null), setItem: (k, v) => { store[k] = String(v); } },
    window: { __TAURI__: tauri ? {} : undefined, open: (...a) => opened.push(a) },
  };
  const keys = Object.keys(env);
  const f = new Function(...keys, `${bundle}\nreturn {${NAMES.join(',')}};`)(...keys.map(k => env[k]));
  return { f, calls, toasts, opened, els, store };
}

test('a newer release is announced in the top bar', async () => {
  const h = harness();
  await h.f.checkForUpdate(false);
  assert.equal(h.els.updNote.style.display, '');
  assert.equal(h.els.updLink.textContent, 'Version 1.3.0 available');
  assert.match(h.els.updLink.title, /You have 1\.2\.0/);
  assert.equal(h.toasts.length, 0, 'the startup check shows the notice, not a toast');
});

test('the same version is not offered again once hidden', async () => {
  const h = harness();
  await h.f.checkForUpdate(false);
  h.f.dismissUpdate();
  assert.equal(h.store.updateDismissed, '1.3.0');
  assert.equal(h.els.updNote.style.display, 'none');
  const again = harness({ store: { updateDismissed: '1.3.0' } });
  await again.f.checkForUpdate(false);
  assert.equal(again.els.updNote.style.display, 'none', 'hidden at the next start too');
  const later = harness({ store: { updateDismissed: '1.3.0' }, reply: { ok: true, current: '1.2.0', latest: '1.4.0', newer: true, url: 'u' } });
  await later.f.checkForUpdate(false);
  assert.equal(later.els.updNote.style.display, '', 'a later version is announced again');
  const manual = harness({ store: { updateDismissed: '1.3.0' } });
  await manual.f.checkForUpdate(true);
  assert.equal(manual.els.updNote.style.display, '', '"Check now" shows it even so');
});

test('nothing is announced when the version is current', async () => {
  const h = harness({ reply: { ok: true, current: '1.3.0', latest: '1.3.0', newer: false, url: 'u' } });
  await h.f.checkForUpdate(false);
  assert.equal(h.els.updNote.style.display, 'none');
  await h.f.checkForUpdate(true);
  assert.match(h.toasts.at(-1)[0], /latest version \(1\.3\.0\)/);
});

test('switched off, the startup check does not ask at all', async () => {
  const h = harness({ store: { updateCheck: 'off' } });
  assert.equal(await h.f.checkForUpdate(false), null);
  assert.equal(h.calls.length, 0);
  await h.f.checkForUpdate(true);
  assert.equal(h.calls.length, 1, '"Check now" still asks');
  const s = harness();
  s.f.setUpdateCheck(false);
  assert.equal(s.store.updateCheck, 'off');
  assert.equal(s.f.updateCheckOn(), false);
});

test('a failed check is silent at startup and reported on request', async () => {
  const h = harness({ reply: { ok: false, current: '1.2.0', error: 'no network' } });
  await h.f.checkForUpdate(false);
  assert.equal(h.toasts.length, 0);
  assert.equal(h.els.updNote.style.display, 'none');
  await h.f.checkForUpdate(true);
  assert.match(h.toasts[0][0], /no network/);
  assert.equal(h.toasts[0][1], true);
});

test('the page opens through the backend in the desktop build, and in a new tab otherwise', async () => {
  const t = harness({ tauri: true });
  await t.f.checkForUpdate(false);
  t.calls.length = 0;
  await t.f.openUpdatePage();
  assert.deepEqual(t.calls, [['/api/open-release-page', { url: 'https://github.com/x/releases/tag/v1.3.0' }]]);
  const b = harness();
  await b.f.checkForUpdate(false);
  await b.f.openUpdatePage();
  assert.deepEqual(b.opened, [['https://github.com/x/releases/tag/v1.3.0', '_blank', 'noopener']]);
});

test('the check is started when the page loads, and Settings shows the switch', () => {
  assert.match(html, /setTimeout\(\(\)=>\{checkForUpdate\(false\);\},\d+\);/);
  assert.match(html, /id="cfgUpdateCheck"/);
  assert.match(extractFunction(html, 'openSettings'), /cfgUpdateCheck'\)\.checked=updateCheckOn\(\)/);
  assert.match(html, /id="updNote"/);
});

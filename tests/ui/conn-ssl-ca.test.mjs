// The CA certificate path on a saved connection.
//
// This is a field that several separate code paths all have to remember to carry, which is the
// shape of bug that actually happens: one of them rebuilds the connection object from parts and
// quietly drops it. forgetPassword() did exactly that in the first draft of this feature - it
// reconstructs {host,port,user,ssl,password:''} and saves, so a connection whose password you
// removed would also have silently lost its CA and stopped connecting under "verify".
//
// So rather than test the happy path, these pin the wiring: every save carries it, every load
// restores it, and the one function that assembles the connection for the backend includes it.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

// NOBS_UI_SOURCE lets the PowerShell edition run this same file against NOBSSQL.ps1, which carries
// the identical UI inline.
const html = readFileSync(process.env.NOBS_UI_SOURCE ||
  join(dirname(fileURLToPath(import.meta.url)), '../../ui/index.html'), 'utf8');

// Every call of the form api('/api/conn-save',{...conn:{...}...}), with the conn object extracted
// by brace-matching from "conn:{" so a nested object cannot truncate it.
function connSaveCalls(src) {
  const out = [];
  let i = 0;
  for (;;) {
    // Anchored on the api( call, not the bare route string: in the PowerShell edition the server's
    // own route table lives in the same file and names '/api/conn-save' too.
    const at = src.indexOf("api('/api/conn-save'", i);
    if (at === -1) break;
    i = at + 1;
    const c = src.indexOf('conn:', at);
    if (c === -1 || c - at > 400) { out.push({ at, conn: null }); continue; }
    const open = src.indexOf('{', c);
    // conn:getConn() and friends - a call, not a literal. Recorded as such; checked below.
    if (open === -1 || open > c + 6) { out.push({ at, conn: src.slice(c, c + 40) }); continue; }
    let depth = 0;
    for (let j = open; j < src.length; j++) {
      if (src[j] === '{') depth++;
      else if (src[j] === '}' && --depth === 0) { out.push({ at, conn: src.slice(open, j + 1) }); break; }
    }
  }
  return out;
}

test('every save of a connection carries its CA certificate', () => {
  const calls = connSaveCalls(html);
  assert.ok(calls.length >= 3, `expected to find the conn-save calls, found ${calls.length}`);
  for (const c of calls) {
    assert.ok(c.conn, `a conn-save call at offset ${c.at} has no conn object at all`);
    // Either it hands over the whole form (getConn, which is checked below), or it builds the
    // object by hand - and then it has to include sslCa or saving silently discards it.
    const passesWholeForm = /getConn\(\)/.test(c.conn);
    assert.ok(passesWholeForm || /sslCa\s*:/.test(c.conn),
      `this conn-save drops sslCa, so saving would erase it:\n  ${c.conn.slice(0, 160)}`);
  }
});

test('getConn sends the CA with every request, not just saves', () => {
  const m = html.match(/function getConn\(\)\{[^}]*\}/);
  assert.ok(m, 'getConn() not found - was it renamed?');
  assert.match(m[0], /sslCa\s*:/, 'getConn() must include sslCa, or no query would ever use it');
  assert.match(m[0], /\$\('sslca'\)/, 'getConn() should read the CA from the form field');
});

test('loading a connection restores the CA, everywhere the rest of it is restored', () => {
  // Any line that loads a saved connection into the form sets $('ssl').value. Each one of those
  // must set the CA too, or picking a connection would show the previous one's certificate.
  const all = html.split('\n');
  const sites = all.map((t, n) => ({ t, n }))
    .filter(l => /\$\('ssl'\)\.value\s*=/.test(l.t))
    // The toggle reads the mode rather than restoring it.
    .filter(l => !/function sslCaToggle/.test(l.t));
  assert.ok(sites.length >= 3, `expected several restore sites, found ${sites.length}`);
  for (const s of sites) {
    // The CA restore may sit on the same line or immediately after it, so look at a small window
    // rather than demanding one particular layout.
    const window = all.slice(s.n, s.n + 3).join('\n');
    assert.match(window, /\$\('sslca'\)\.value\s*=/,
      `line ${s.n + 1} restores ssl but not the CA, so it would keep the previous connection's:\n  ${s.t.trim().slice(0, 160)}`);
  }
});

test('the CA field only shows for the modes that use it', () => {
  // "required" and "disabled" verify nothing, so a CA box there invites someone to fill in a
  // value that is then ignored - worse than not offering it. Run the real toggle rather than
  // pattern-matching its source.
  const m = html.match(/function sslCaToggle\(\)\{.*?\}\}?/);
  assert.ok(m, 'sslCaToggle() not found');
  const shown = (mode) => {
    const els = { ssl: { value: mode }, sslcaWrap: { style: { display: '?' } } };
    new Function('$', m[0] + '\nsslCaToggle();')(id => els[id]);
    return els.sslcaWrap.style.display !== 'none';
  };
  assert.equal(shown('verify'), true, 'verify uses the CA');
  assert.equal(shown('verify-ca'), true, 'verify-ca is the mode the CA matters most for');
  for (const mode of ['default', 'disabled', 'required', 'verifyx', 'xverify'])
    assert.equal(shown(mode), false, `${mode} verifies nothing, so the CA should be hidden`);
  assert.match(html, /id="ssl"[^>]*onchange="sslCaToggle\(\)"/,
    'changing the SSL mode must re-evaluate whether the CA field is shown');
  assert.match(html, /id="sslca"/, 'the CA input itself is missing from the form');
});

test('every SSL mode list offers verify-ca', () => {
  // Three separate lists - the inline form and two dialogs - that have to agree. A mode missing
  // from one of them means a connection saved with it cannot be edited without silently changing.
  const inline = html.match(/<select id="ssl"[\s\S]*?<\/select>/);
  assert.ok(inline && /value="verify-ca"/.test(inline[0]), 'the inline SSL select lacks verify-ca');
  const dialogs = html.match(/\{key:'ssl',label:'SSL',type:'select',options:\[[^\]]*\]/g) || [];
  assert.ok(dialogs.length >= 2, `expected the save and edit dialogs, found ${dialogs.length}`);
  for (const d of dialogs) assert.match(d, /value:'verify-ca'/, `a dialog SSL list lacks verify-ca:\n  ${d.slice(0, 120)}`);
});

test('the CA is a path the app browses for, not a browser file input', () => {
  // <input type="file"> hands back a File object and deliberately never a real path, and a path
  // is precisely what has to be written into the client options file.
  assert.doesNotMatch(html, /id="sslca"[^>]*type="file"/,
    'a browser file input cannot give the real path this needs');
  assert.match(html, /onPick:pp=>\$\('sslca'\)\.value=pp/,
    'the CA field should use the app\'s own file browser');
});

// --- the request bridge must not swap out the profile being saved ---------------------------------
// api() fills in the connection for every request, so that queries always go to the server you
// are connected to rather than whatever profile is loaded in the form. It did that for conn-save
// as well, where conn is not a server to talk to but the profile being SAVED - so Save, Edit, Clone
// and Forget-password all stored the connected server's host, port, user, SSL mode, CA and
// password under the other profile's name whenever you were connected somewhere else. Found by
// driving the real Save dialog while connected to a different server: the dialog sent 3308 /
// verify-ca / a CA, and the store received 3306 / default / nothing.
//
// This runs the real api() from the file under test. The Tauri edition sends through
// window.__TAURI__.core.invoke and the PowerShell edition through fetch, so both are stubbed and
// whichever one the function uses is the one that gets checked.
function extractFn(src, name) {
  const start = src.indexOf(`async function ${name}(`);
  assert.ok(start >= 0, `${name}() not found`);
  let depth = 0;
  for (let j = src.indexOf('{', start); j < src.length; j++) {
    if (src[j] === '{') depth++;
    else if (src[j] === '}' && --depth === 0) return src.slice(start, j + 1);
  }
  throw new Error('unbalanced braces in ' + name);
}

function bridge() {
  const sent = [];
  const connected = { host: 'prod.example', port: '3306', user: 'admin', password: 'PROD-SECRET', ssl: 'default', sslCa: '' };
  const form = { host: 'form.example', port: '3310', user: 'formuser', password: 'form-pw', ssl: 'required', sslCa: '' };
  const window = {
    _activeConn: connected, _activeReadOnly: true, readOnly: false,
    __TAURI__: { core: { invoke: async (cmd, args) => { sent.push({ cmd, p: args.req }); return { ok: true }; } } },
  };
  const fetch = async (path, init) => { sent.push({ cmd: path, p: JSON.parse(init.body) }); return { json: async () => ({ ok: true }) }; };
  const api = new Function('window', 'fetch', 'getConn', 'busyStart', 'busyStop', 'showDead', 'TOKEN',
    extractFn(html, 'api') + '\nreturn api;')(window, fetch, () => ({ ...form }), () => {}, () => {}, () => {}, 't');
  return { api, sent, connected, form, window };
}

test('saving a profile stores that profile, not the server you are connected to', async () => {
  const b = bridge();
  const profile = { host: 'other.example', port: '3308', user: 'someone', password: 'OTHER-PW', ssl: 'verify-ca', sslCa: 'C:/certs/ca.pem' };
  await b.api('/api/conn-save', { name: 'Other', conn: { ...profile } });
  assert.equal(b.sent.length, 1);
  assert.deepEqual(b.sent[0].p.conn, profile,
    'conn-save must carry the profile being saved - it was replaced with the connected server');
  assert.notEqual(b.sent[0].p.conn.password, b.connected.password,
    'the connected server\'s password must never be saved under another profile\'s name');
});

test('everything else still goes to the server you are connected to', async () => {
  // The reason the override exists: a profile merely selected in the dropdown must not redirect
  // live queries, or read-only enforcement, to a different server.
  const b = bridge();
  await b.api('/api/query', { sql: 'SELECT 1', conn: { host: 'sneaky.example' } });
  await b.api('/api/conn-get', { name: 'Other' });
  for (const s of b.sent) {
    assert.equal(s.p.conn.host, 'prod.example', `${s.cmd} was not sent to the connected server`);
    assert.equal(s.p.ro, true, `${s.cmd} lost the connected server's read-only flag`);
  }
  // Not connected at all: the form is all there is.
  b.window._activeConn = null; b.sent.length = 0;
  await b.api('/api/query', { sql: 'SELECT 1' });
  assert.equal(b.sent[0].p.conn.host, 'form.example');
  // And connect always uses the form - that is what connecting means.
  b.window._activeConn = b.connected; b.sent.length = 0;
  await b.api('/api/connect', {});
  assert.equal(b.sent[0].p.conn.host, 'form.example');
});

test('a conn-save without a conn still gets one, rather than sending nothing', async () => {
  const b = bridge();
  await b.api('/api/conn-save', { name: 'x' });
  assert.ok(b.sent[0].p.conn, 'the exception is only for a caller that supplied the profile');
});

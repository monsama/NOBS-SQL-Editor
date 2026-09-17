// One step of the upgrade test (.github/workflows/upgrade.yml). Starts an installed copy of the app
// with its real data folders - unlike the GUI tests, which give it a temporary one - and drives it
// over the Chrome DevTools protocol:
//
//   node tests/upgrade/upgrade.mjs --phase before --exe <installed exe> --version <old> --latest <new>
//       saves a connection with its password, a library query, a setting and the theme, and
//       checks that the old version offers the new one
//   node tests/upgrade/upgrade.mjs --phase after --exe <installed exe> --version <new>
//       checks that all of it is still there after the upgrade, and that the password still connects
//   node tests/upgrade/upgrade.mjs --phase clear --exe <installed exe>
//       "Clear all app data", as the README's uninstall steps say, and checks what it removed
//
// NOBS_TEST_DSN (host:port:user:password) names the server the saved connection points at.

import { spawn, spawnSync } from 'node:child_process';

const arg = n => { const i = process.argv.indexOf('--' + n); return i > 0 ? process.argv[i + 1] : undefined; };
const phase = arg('phase'), exe = arg('exe'), version = arg('version'), latest = arg('latest');
const [host, port, user, pass] = (process.env.NOBS_TEST_DSN || '').split(':');
if (!['before', 'after', 'clear'].includes(phase) || !exe || !pass) {
  console.error('usage: node upgrade.mjs --phase before|after|clear --exe <path> [--version <v>] [--latest <v>]  (NOBS_TEST_DSN set)');
  process.exit(2);
}
// It works on the real data folders, and "clear" empties them: never on a machine someone uses.
if (process.env.GITHUB_ACTIONS !== 'true') {
  console.error('This test changes the app data of the user running it, so it only runs on GitHub Actions.');
  process.exit(2);
}
const PROFILE = 'upgrade-test', QUERY = 'upgrade test query', URL_TEMPLATE = 'https://example.invalid/{version}/upgrade-test';
const sleep = ms => new Promise(r => setTimeout(r, ms));
const cdpPort = 9300 + Math.floor(Math.random() * 500);

let failed = 0;
const check = (name, ok, detail) => {
  if (ok) console.log(`  ok    ${name}`);
  else { failed++; console.log(`  FAIL  ${name} -> ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`); }
};

const app = spawn(exe, [], { env: { ...process.env, NOBS_WEBVIEW_DEBUG_PORT: String(cdpPort) }, stdio: 'ignore' });

async function page() {
  for (let i = 0; i < 600; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${cdpPort}/json`)).json();
      const p = list.find(t => t.type === 'page' && !/^about:/.test(t.url));
      if (p) return p.webSocketDebuggerUrl;
    } catch { /* not up yet */ }
    await sleep(100);
  }
  throw new Error('no page to drive');
}

function cdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let id = 0; const pending = new Map();
  ws.onmessage = ev => { const m = JSON.parse(ev.data); const p = pending.get(m.id); if (p) { pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
  const opened = new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error('CDP connection failed')); });
  return async expression => {
    await opened;
    const n = ++id;
    const reply = new Promise((res, rej) => pending.set(n, { res, rej }));
    ws.send(JSON.stringify({ id: n, method: 'Runtime.evaluate', params: { expression, awaitPromise: true, returnByValue: true } }));
    const r = await reply;
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
    return r.result.value;
  };
}

try {
  const ev = cdp(await page());
  let loaded = false;
  for (let i = 0; i < 300 && !loaded; i++) {
    try { loaded = await ev(`typeof connect==='function'&&!!document.getElementById('host')&&document.readyState==='complete'`); }
    catch { /* replaced by a navigation */ }
    if (!loaded) await sleep(100);
  }
  if (!loaded) throw new Error('the page never finished loading');
  const js = v => JSON.stringify(v);
  const info = await ev(`api('/api/app-info')`);
  console.log(`  (${phase}: ${info.name} ${info.version})`);
  if (version) check(`the installed app is ${version}`, info.version === version, info);
  // The notice appears a few seconds after start.
  const notice = async () => { await sleep(8000); return ev(`({shown:$('updNote').style.display!=='none',text:$('updLink').textContent})`); };

  if (phase === 'before') {
    const conn = { host, port, user, password: pass, ssl: 'default' };
    const sv = await ev(`api('/api/conn-save',{name:${js(PROFILE)},conn:${js(conn)},accent:'#3b82f6',env:'test',readonly:false,savepw:true})`);
    check('a connection is saved with its password', sv.ok, sv);
    const lib = await ev(`api('/api/lib-save',{name:${js(QUERY)},sql:'SELECT 42',schema:'',ts:Date.now()})`);
    check('a query is saved to the library', lib.ok, lib);
    const cfg = await ev(`api('/api/save-config',{config:{mariadb_download_url_template:${js(URL_TEMPLATE)}}})`);
    check('a setting is saved', cfg.ok, cfg);
    // The light theme: a choice the app keeps in browser storage.
    await ev(`(localStorage.setItem('theme','light'),true)`);
    const n = await notice();
    check(`the old version offers ${latest}`, n.shown && n.text.includes(latest), n);
  }

  if (phase === 'after') {
    const list = await ev(`api('/api/conn-list')`);
    const item = (list.items || []).find(c => c.name === PROFILE);
    check('the connection is still there, with its password', item && item.hasPassword && item.host === host && String(item.port) === String(port), list);
    const got = await ev(`api('/api/conn-get',{name:${js(PROFILE)}})`);
    check('the saved password is unchanged', got.ok && got.conn.password === pass, got.ok ? 'a different password' : got);
    const connected = await ev(`(async()=>{await refreshConns();$('connlist').value=${js(PROFILE)};await pickConn();await connect();
      for(let i=0;i<300&&!/Connected/.test($('connStatus').textContent);i++)await new Promise(r=>setTimeout(r,100));
      return $('connStatus').textContent;})()`);
    check('and connects with it', /Connected/.test(connected), connected);
    const lib = await ev(`api('/api/lib-list')`);
    check('the library query is still there', (lib.items || []).some(q => q.name === QUERY && q.sql === 'SELECT 42'), lib);
    const cfg = await ev(`api('/api/get-config')`);
    check('the setting is still there', cfg.config && cfg.config.mariadb_download_url_template === URL_TEMPLATE, cfg);
    check('the light theme is still chosen', await ev(`localStorage.getItem('theme')==='light'&&!document.body.classList.contains('dark')`), 'dark again');
    const n = await notice();
    check('no update is offered any more', !n.shown, n);
  }

  if (phase === 'clear') {
    await ev(`(ask=async()=>true,clearAllData(),true)`);
    await sleep(3000);
    const after = cdp(await page());
    await sleep(2000);
    const list = await after(`api('/api/conn-list')`);
    check('Clear all app data removes the connection', list.ok && !(list.items || []).length, list);
    const lib = await after(`api('/api/lib-list')`);
    check('and the library', lib.ok && !(lib.items || []).length, lib);
    check('and the theme choice', await after(`localStorage.getItem('theme')`) === null, 'still there');
  }
  // Not awaited in the page: the app exits before it could answer.
  await ev(`(api('/api/quit'),true)`).catch(() => {});
} catch (e) {
  failed++; console.log(`  FAIL  ${e.message}`);
} finally {
  await sleep(1500);
  if (app.exitCode === null) spawnSync('taskkill', ['/PID', String(app.pid), '/T', '/F'], { stdio: 'ignore' });
  await sleep(1000);
}
console.log(failed ? `\n  ${failed} FAILED` : '\n  all passed');
process.exit(failed ? 1 : 0);

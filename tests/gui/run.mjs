// Drives the real app through its UI and checks what it does - the flows the unit and live tests
// cannot reach: the grid, Compare, the export and import dialogs, a script's results.
//
//   node tests/gui/run.mjs --app desktop --target src-tauri/target/debug/nobs-sql-editor.exe
//   node tests/gui/run.mjs --app ps --target ./NOBSSQL.ps1          (NOBS-SQL-PS)
//
// NOBS_TEST_DSN (host:port:user:password) names the server, as for the live tests; the scenarios
// create and drop their own nobs_gui* databases and remove the connection profiles they save.
// The PowerShell edition runs with its own browser profile in a temporary folder. The desktop app
// keeps its browser storage, so each scenario closes the tabs it opened.
//
// Scenarios live in tests/gui/scenarios and run in name order (later ones use what earlier ones
// left); --only <text> runs those whose file name contains it. Needs Node 22 or later.

import { spawn, spawnSync } from 'node:child_process';
import { readFileSync, readdirSync, mkdtempSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const arg = n => { const i = process.argv.indexOf('--' + n); return i > 0 ? process.argv[i + 1] : undefined; };
const app = arg('app'), target = arg('target') && resolve(arg('target')), only = arg('only');
if (!['ps', 'desktop'].includes(app) || !target || !existsSync(target)) {
  console.error('usage: node run.mjs --app ps|desktop --target <NOBSSQL.ps1 | nobs-sql-editor.exe> [--only <name>]');
  process.exit(2);
}
const dsn = (process.env.NOBS_TEST_DSN || '').split(':');
if (dsn.length !== 4) { console.log('  SKIPPED - NOBS_TEST_DSN is not set, so no GUI test ran.'); process.exit(0); }
const [dbHost, dbPort, dbUser, dbPass] = dsn;

const tmp = mkdtempSync(join(tmpdir(), 'nobs-gui-'));
const cdpPort = 9300 + Math.floor(Math.random() * 500);
const children = [];
const sleep = ms => new Promise(r => setTimeout(r, ms));
const killTree = p => { if (p && p.pid) spawnSync('taskkill', ['/PID', String(p.pid), '/T', '/F'], { stdio: 'ignore' }); };

// The CSV the export/import scenario reads, byte for byte: a quoted CRLF, hex, the text NULL, the
// \N marker, UTF-8.
writeFileSync(join(tmp, 'gp.csv'), Buffer.from(
  'id,t,b,n,bits,l1\n1,"line1\r\nline2",0xDEAD,NULL,0x01,é\n2,,0x,\\N,0x03,\\N\n3,x,\\N,0x42,\\N,Grüße\n', 'utf8'));

async function startApp() {
  if (app === 'desktop') {
    children.push(spawn(target, [], { env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}` }, stdio: 'ignore' }));
    return;
  }
  const shell = spawnSync('where', ['pwsh'], { stdio: 'ignore' }).status === 0 ? 'pwsh' : 'powershell';
  const server = spawn(shell, ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', target, '-NoBrowser'], { stdio: ['ignore', 'pipe', 'pipe'] });
  children.push(server);
  let said = '';
  server.stdout.on('data', d => { said += d; });
  server.stderr.on('data', d => { said += d; });
  let url = null;
  for (let i = 0; i < 600 && !url; i++) { const m = /Open:\s+(http:\/\/127\.0\.0\.1:\d+\/)/.exec(said); if (m) url = m[1]; else await sleep(100); }
  if (!url) throw new Error('the PowerShell server did not start:\n' + said);
  const edge = [process.env['ProgramFiles(x86)'], process.env.ProgramFiles].map(p => p && join(p, 'Microsoft', 'Edge', 'Application', 'msedge.exe')).find(p => p && existsSync(p));
  if (!edge) throw new Error('Microsoft Edge not found');
  children.push(spawn(edge, ['--headless=new', `--remote-debugging-port=${cdpPort}`, `--user-data-dir=${join(tmp, 'edge')}`,
    '--no-first-run', '--window-size=1600,1000', url], { stdio: 'ignore' }));
}

async function connectPage() {
  for (let i = 0; i < 600; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${cdpPort}/json`)).json();
      const page = list.find(t => t.type === 'page' && !/^about:|^edge:/.test(t.url));
      if (page) return page.webSocketDebuggerUrl;
    } catch { /* not up yet */ }
    await sleep(100);
  }
  throw new Error('no page to drive on port ' + cdpPort);
}

function cdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let id = 0; const pending = new Map();
  ws.onmessage = ev => { const m = JSON.parse(ev.data); const p = pending.get(m.id); if (p) { pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } };
  const opened = new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error('CDP connection failed')); });
  const evaluate = async (expression, timeoutMs = 300000) => {
    await opened;
    const n = ++id;
    const reply = new Promise((res, rej) => pending.set(n, { res, rej }));
    ws.send(JSON.stringify({ id: n, method: 'Runtime.evaluate', params: { expression, awaitPromise: true, returnByValue: true } }));
    const timer = new Promise((_, rej) => setTimeout(() => rej(new Error(`timed out after ${timeoutMs / 1000}s`)), timeoutMs));
    const r = await Promise.race([reply, timer]);
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
    return r.result.value;
  };
  return { evaluate, close: () => ws.close() };
}

let failed = 0, passed = 0;
try {
  await startApp();
  const page = cdp(await connectPage());
  // The page may still be navigating when it is found; that throws until it has settled.
  let loaded = false;
  for (let i = 0; i < 300 && !loaded; i++) {
    try { loaded = await page.evaluate(`typeof connect==='function'&&!!document.getElementById('host')&&document.readyState==='complete'`, 5000); }
    catch { /* context replaced by a navigation */ }
    if (!loaded) await sleep(100);
  }
  if (!loaded) throw new Error('the page never finished loading');
  await sleep(1000);
  const env = { edition: app, tmp: tmp.replace(/\\/g, '/'), dbPort };
  await page.evaluate(`window.GUI_ENV=${JSON.stringify(env)};` + readFileSync(join(here, 'prelude.js'), 'utf8'));
  const connected = await page.evaluate(`(async()=>{
    $('connlist').value=''; $('host').value=${JSON.stringify(dbHost)}; $('port').value=${JSON.stringify(dbPort)};
    $('user').value=${JSON.stringify(dbUser)}; $('pass').value=${JSON.stringify(dbPass)}; $('ssl').value='default';
    if($('sslca'))$('sslca').value='';
    await connect(); await G.until(()=>/Connected/.test($('connStatus').textContent),30000);
    return $('connStatus').textContent;})()`);
  console.log(`  (${app} app, ${connected.trim()} on port ${dbPort})`);
  if (!/Connected/.test(connected)) throw new Error('could not connect: ' + connected);

  const files = readdirSync(join(here, 'scenarios')).filter(f => f.endsWith('.js') && (!only || f.includes(only))).sort();
  for (const f of files) {
    console.log(`\n-- ${f} --`);
    const tabsBefore = await page.evaluate('tabs.map(t=>t.id)');
    try {
      const checks = await page.evaluate(readFileSync(join(here, 'scenarios', f), 'utf8'));
      for (const c of checks || []) {
        if (c.skipped) { console.log(`  skip  ${c.name} (${c.skipped})`); continue; }
        if (c.ok) { passed++; console.log(`  ok    ${c.name}`); } else { failed++; console.log(`  FAIL  ${c.name} -> ${c.detail}`); }
      }
      const shown = await page.evaluate('G.errs().splice(0)');
      if (shown.length) { failed++; console.log(`  FAIL  errors shown to the user -> ${JSON.stringify(shown)}`); }
    } catch (e) {
      failed++; console.log(`  FAIL  ${f} stopped: ${e.message}`);
    } finally {
      await page.evaluate(`(()=>{const keep=new Set(${JSON.stringify(tabsBefore)});[...tabs].forEach(t=>{if(!keep.has(t.id))closeTab(t.id);});G.toasts.length=0;return true;})()`).catch(() => {});
    }
  }
  page.close();
} catch (e) {
  failed++; console.log(`  FAIL  ${e.message}`);
} finally {
  children.reverse().forEach(killTree);
  await sleep(500);
  try { rmSync(tmp, { recursive: true, force: true }); } catch { /* a browser may still hold a file */ }
}
console.log(failed ? `\n  ${failed} FAILED (${passed} passed)` : `\n  all ${passed} passed`);
process.exit(failed ? 1 : 0);

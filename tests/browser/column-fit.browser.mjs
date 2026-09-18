// Fitting a column, driven in a real browser engine against a real grid DOM.
//
// Which values a fit measures is unit-tested next door (tests/ui/column-fit.test.mjs) and the whole
// gesture is exercised against the running app by tests/gui/scenarios/13-column-fit.js. Neither
// covers what sits between them: the layout. Both bugs this file was written after were layout,
// both were invisible to the unit tests, and both cost a twenty-minute round trip through CI to
// find. The fit read the header's width with scrollWidth, which never reports less than the width
// the column already has, so every double-click added its slack to the last one and the column
// crept wider; and the handle you grab is positioned from the cell's padding box, which with
// collapsed borders stops half a border short of the line, so it sat 1px left of the line it grabs.
// A browser answers both in seconds and needs no database.
//
// What it costs: the page below restates the markup renderGrid builds rather than calling it, so a
// change there can leave this measuring something the app no longer draws. The scenario against the
// real app is what catches that; this is the fast half of the pair.
//
// Needs Microsoft Edge, which this app's PowerShell edition already requires and windows-latest
// ships.

import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// NOBS_UI_SOURCE lets the PowerShell edition run this same file against NOBSSQL.ps1, which carries
// the identical UI inline.
const src = readFileSync(process.env.NOBS_UI_SOURCE ||
  join(dirname(fileURLToPath(import.meta.url)), '../../ui/index.html'), 'utf8').replace(/\r\n/g, '\n');

function fn(name) {
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
function cons(name) {
  const start = src.indexOf(`const ${name}=`);
  assert.notEqual(start, -1, `const ${name} not found - was it renamed?`);
  return src.slice(start, src.indexOf(';\n', start) + 1);
}

// The three rules this geometry rests on, taken from the file rather than restated here: the
// collapsed borders, the sticky header the handle is positioned inside, and the cell padding a
// measurement has to match.
const css = src.split('\n').filter(l =>
  l.includes('table.grid{border-collapse') || l.includes('table.grid th{position:sticky') || l.startsWith('table.grid td{border:none'));
assert.equal(css.length, 3, `expected the three grid rules, found ${css.length}`);

const code = [cons('CTRL_NAMES'), cons('CTRL_RE'), cons('FIT_SAMPLE'),
  ...['esc', 'clip', 'ctrlBadge', 'textCellHtml', 'decodeCtrlCharCell', 'hexToBitNumber', 'cellHtml',
    'viewIndices', 'widestCandidates', 'fitMeasure', 'autofitCol'].map(fn)].join('\n');

// 900 rows, so the grid is past the point where it draws them all, with the value that decides the
// width at row 880 - below anything drawn - and a 600 character value in another column for the cap
// to bite on.
const page = `<!doctype html><meta charset=utf-8><style>
:root{--bd:#555;--bd2:#3a3a3a;--gridh:#2b2b2b;--even:#232323;--accent:#4aa3ff}
html,body{height:100%;margin:0;font-family:system-ui,"Segoe UI",Roboto,Arial,sans-serif;font-size:13px;color:#ddd;background:#1e1e1e}
${css.join('\n')}
.result{position:absolute;inset:0;overflow:auto}
</style>
<div class="result" id="res_t1"></div>
<script>
${code}
const LONG='this value sits nine hundred rows down where nothing has ever scrolled to it';
const rows=[];for(let i=1;i<=900;i++)rows.push([String(i),'short',i===880?LONG:'short',i===5?'W'.repeat(600):'x']);
const tab={id:'t1',cols:['id','brief','deep','huge'],rows,pk:['id'],pending:{upd:{},del:new Set(),ins:[]},filters:{},sortCol:-1,sortDir:1,binCols:[],bitCols:[]};
function T(id){return id==='t1'?tab:null;}
function $(id){return document.getElementById(id);}
// What renderBody builds: a window of rows, and a spacer row standing in for everything scrolled
// past - the row whose padding a fit once borrowed by accident.
function draw(start){
 const rowH=23,win=40,span=tab.cols.length+3;
 // The handle for a column hangs off the left edge of the next header cell, and the last one off
 // the filler cell at the end - see sortHeader, and the test below that says why.
 let h='<table class="grid"><colgroup><col style="width:30px"><col style="width:34px">'
  +tab.cols.map(()=>'<col style="width:150px">').join('')+'<col></colgroup><thead><tr>'
  +'<th style="width:22px;height:28px;padding:0"><span></span></th><th></th>'
  +tab.cols.map((c,ci)=>'<th style="cursor:pointer;height:28px;padding:0 8px">'+(ci?'<span class="rz" data-ci="'+(ci-1)+'"></span>':'')+'<span style="display:flex;align-items:center;gap:4px;height:28px;min-width:0"><span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0">'+c+'</span></span></th>').join('')
  +'<th><span class="rz" data-ci="'+(tab.cols.length-1)+'"></span></th></tr></thead><tbody>';
 if(start>0)h+='<tr class="vpad" style="height:'+(start*rowH)+'px"><td colspan="'+span+'" style="padding:0;border:none"></td></tr>';
 for(let ri=start;ri<Math.min(start+win,rows.length);ri++)
  h+='<tr data-r="'+ri+'"><td></td><td></td>'+tab.cols.map((c,ci)=>'<td class="editable">'+cellHtml(rows[ri][ci],false,false)+'</td>').join('')+'<td></td></tr>';
 const below=rows.length-Math.min(start+win,rows.length);
 if(below>0)h+='<tr class="vpad" style="height:'+(below*rowH)+'px"><td colspan="'+span+'" style="padding:0;border:none"></td></tr>';
 $('res_t1').innerHTML=h+'</tbody></table>';
}
function widthOf(name){const cg=$('res_t1').querySelector('colgroup');return parseFloat(cg.children[tab.cols.indexOf(name)+2].style.width)||0;}
function fit(name){autofitCol('t1',tab.cols.indexOf(name));return widthOf(name);}
window.probe=()=>{
 const out={};
 draw(0);
 out.drawn=$('res_t1').querySelectorAll('tbody tr[data-r]').length;
 out.longValueDrawn=/nine hundred rows down/.test($('res_t1').innerHTML);
 out.brief=fit('brief'); out.deep=fit('deep'); out.huge=fit('huge');
 out.deepTwice=fit('deep'); out.deepThrice=fit('deep');
 draw(860);
 out.deepFromTheBottom=fit('deep');
 out.pane=$('res_t1').clientWidth;
 const ci=tab.cols.indexOf('deep');
 const th=$('res_t1').querySelectorAll('thead tr:first-child th')[ci+2];
 const rz=$('res_t1').querySelector('thead .rz[data-ci="'+ci+'"]');
 const b=rz.getBoundingClientRect(),line=th.getBoundingClientRect().right;
 out.handle={line,left:b.left,right:b.right,centre:(b.left+b.right)/2,width:b.width};
 // What the pointer would actually land on, three pixels either side of the line. This follows the
 // same stacking order the paint does, which is the whole point: the band can be centred in layout
 // while the half of it past the line is covered by the next header, and then it is neither seen
 // nor clickable there.
 const y=(th.getBoundingClientRect().top+th.getBoundingClientRect().bottom)/2;
 const hit=x=>{const e=document.elementFromPoint(x,y);return e?(e.className||e.tagName):'nothing';};
 out.hits={left:hit(line-3),right:hit(line+3)};
 return JSON.stringify(out);
};
</script>`;

const edge = [process.env['ProgramFiles(x86)'], process.env.ProgramFiles]
  .map(p => p && join(p, 'Microsoft', 'Edge', 'Application', 'msedge.exe')).find(p => p && existsSync(p));
// Not skipped when it is missing: a test that quietly passes without having run is worse than no
// test at all.
assert.ok(edge, 'Microsoft Edge was not found - this measures layout and needs a browser to do it');

const dir = mkdtempSync(join(tmpdir(), 'nobs-fit-'));
writeFileSync(join(dir, 'page.html'), page);
// Port 0, and Edge writes down the one it took - a fixed port collides with whatever else is
// driving a browser on this machine, the GUI runner included.
const proc = spawn(edge, ['--headless=new', '--remote-debugging-port=0', `--user-data-dir=${join(dir, 'profile')}`,
  '--no-first-run', '--window-size=1400,900', `file:///${join(dir, 'page.html').replace(/\\/g, '/')}`], { stdio: 'ignore' });
// Cleanup is best-effort on purpose: the browser holds its profile open for a moment after it is
// killed, and Windows refuses the delete while it does. A leftover directory under the system temp
// is not worth failing a run that measured everything it set out to.
after(() => {
  try { proc.kill(); } catch { /* already gone */ }
  try { rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch { /* the OS will */ }
});

const sleep = ms => new Promise(r => setTimeout(r, ms));
let port = null;
for (let i = 0; i < 200 && !port; i++) {
  try { port = readFileSync(join(dir, 'profile', 'DevToolsActivePort'), 'utf8').split('\n')[0].trim() || null; }
  catch { await sleep(100); }
}
assert.ok(port, 'Edge never reported a debugging port');

let wsUrl = null;
for (let i = 0; i < 200 && !wsUrl; i++) {
  try {
    const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
    const p = list.find(t => t.type === 'page' && t.url.startsWith('file:'));
    if (p) wsUrl = p.webSocketDebuggerUrl;
  } catch { /* not up yet */ }
  if (!wsUrl) await sleep(100);
}
assert.ok(wsUrl, 'no page to measure in');

const sock = new WebSocket(wsUrl);
await new Promise((res, rej) => { sock.onopen = res; sock.onerror = () => rej(new Error('could not connect to the browser')); });
after(() => sock.close());
let id = 0;
const evaluate = expr => new Promise((res, rej) => {
  const n = ++id;
  sock.onmessage = ev => { const m = JSON.parse(ev.data); if (m.id !== n) return; m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); };
  sock.send(JSON.stringify({ id: n, method: 'Runtime.evaluate', params: { expression: expr, returnByValue: true } }));
});

let probe = null;
for (let i = 0; i < 100 && !probe; i++) {
  const r = await evaluate('typeof probe==="function" ? probe() : null');
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
  if (r.result.value) probe = JSON.parse(r.result.value);
  else await sleep(100);
}
assert.ok(probe, 'the page never got as far as measuring anything');

test('the grid draws a window rather than every row', () => {
  assert.ok(probe.drawn > 0 && probe.drawn < 900, `${probe.drawn} of 900 rows drawn`);
  assert.equal(probe.longValueDrawn, false, 'the deciding value was on screen, so nothing below it was tested');
});

test('a column of short values fits to something narrow', () => {
  assert.ok(probe.brief > 40 && probe.brief < 200, `${probe.brief}px`);
});

test('a column whose widest value was never drawn fits to that value', () => {
  assert.ok(probe.deep > probe.brief + 200, `brief ${probe.brief}, deep ${probe.deep}`);
});

test('a value too long for the window stops at the pane', () => {
  assert.ok(probe.huge > probe.deep, `huge ${probe.huge}, deep ${probe.deep}`);
  assert.ok(probe.huge <= probe.pane, `huge ${probe.huge}, pane ${probe.pane}`);
});

// The header's width was read with scrollWidth, which never reports less than the width the column
// already has, so each fit added its slack to the one before: 451, 467, 483.
test('fitting the same column again does not widen it', () => {
  assert.deepEqual([probe.deepTwice, probe.deepThrice], [probe.deep, probe.deep]);
});

test('and it fits the same scrolled to the bottom', () => {
  assert.ok(Math.abs(probe.deepFromTheBottom - probe.deep) < 2, `${probe.deep} then ${probe.deepFromTheBottom}`);
});

// right: is resolved against the cell's padding box, which with collapsed borders stops half a
// border short of the line - so an offset that looks centred lands 1px left of it.
test('the resize handle is centred on the line it grabs', () => {
  assert.ok(Math.abs(probe.handle.centre - probe.handle.line) <= 0.5,
    `band ${probe.handle.left}-${probe.handle.right}, line at ${probe.handle.line}`);
});

test('and is wide enough to hit on either side of it', () => {
  const left = probe.handle.line - probe.handle.left, right = probe.handle.right - probe.handle.line;
  assert.ok(probe.handle.width >= 8 && left >= 3 && right >= 3, `${left} left, ${right} right`);
});

// Being centred in layout was not enough, and this is the check that says so. Each th is sticky
// with a z-index, so it is its own stacking context: a handle on a cell's right edge overhung into
// the next column, and that column's header painted over the half that crossed the line. Hit
// testing follows the same order, so that half could not be grabbed either - of a 9px band, 5px
// were real, all of them left of the line. The handle now hangs off the next cell instead.
test('the handle can be grabbed on both sides of the line, not just the left', () => {
  assert.deepEqual(probe.hits, { left: 'rz', right: 'rz' },
    `three pixels left of the line hit ${probe.hits.left}, three right hit ${probe.hits.right}`);
});

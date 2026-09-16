// Does the shipped UI actually parse?
//
// This exists because it once did not, and every other test still passed. A broken string literal
// in ui/index.html - a message whose escaped newlines had turned into real ones - made the entire
// inline <script> block a syntax error. The app came up as an unstyled white page with no
// connections and nothing clickable, because none of the JavaScript ran at all.
//
// Nothing caught it. The other tests here lift individual functions out of the file by
// brace-matching and evaluate those in isolation, so they were quite happy to confirm that
// normalizeHexInput() behaved correctly inside a file the browser could not load. That is the gap:
// per-function tests cannot see a file-level syntax error, and a file-level syntax error is the
// most damaging thing that can happen to a single-file app.
//
// Cheap, and it fails loudly on exactly the class of mistake that produced it.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const html = readFileSync(join(root, 'ui', 'index.html'), 'utf8');

// Every inline <script> (one with no src=) has to be valid on its own.
function inlineScripts(src) {
  const re = /<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/gi;
  const out = [];
  let m;
  while ((m = re.exec(src)) !== null) {
    out.push({ body: m[1], line: src.slice(0, m.index).split('\n').length });
  }
  return out;
}

test('ui/index.html contains at least one inline script', () => {
  // A guard on the guard: if the markup changes shape and the regex stops matching, this test
  // would otherwise pass by checking nothing at all.
  assert.ok(inlineScripts(html).length > 0, 'no inline <script> found - has the page structure changed?');
});

test('every inline script in ui/index.html parses', () => {
  for (const { body, line } of inlineScripts(html)) {
    try {
      new Function(body);
    } catch (e) {
      assert.fail(`the inline <script> starting at line ${line} is not valid JavaScript: ${e.message}\n` +
                  `The whole page fails to run when this happens - no theme, no connections, nothing clickable.`);
    }
  }
});

test('no string literal spans a raw newline', () => {
  // The specific shape that broke it: an escaped \n inside a single-quoted string that became a
  // real line break during an edit. A syntax check already catches that, but naming it here makes
  // the failure self-explanatory rather than a bare "Invalid or unexpected token".
  const offenders = [];
  html.split('\n').forEach((l, i) => {
    // count unescaped single quotes outside of comments; an odd number means the literal is left open
    const stripped = l.replace(/\\'/g, '').replace(/\/\/.*$/, '');
    const quotes = (stripped.match(/'/g) || []).length;
    if (quotes % 2 === 1 && /await ask\(|toast\(/.test(l)) offenders.push(i + 1);
  });
  assert.deepEqual(offenders, [], `these lines open a string literal and never close it: ${offenders.join(', ')}`);
});

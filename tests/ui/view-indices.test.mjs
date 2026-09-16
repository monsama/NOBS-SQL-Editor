// Regression tests for the grid's sort/filter ordering (viewIndices in ui/index.html).
//
// The UI is a single HTML file with no module system, so rather than duplicating the function
// here - a copy would happily keep passing while the real one regressed - this pulls the actual
// source out of ui/index.html and evaluates it. viewIndices' only external dependency is T(id),
// which is stubbed with a fixture tab.
//
// Run: node --test tests/ui/     (or npm test)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const html = readFileSync(join(root, 'ui', 'index.html'), 'utf8');

// Lifts `function <name>(...) { ... }` out of the file by matching braces from its opening one.
function extractFunction(src, name) {
  const start = src.indexOf(`function ${name}(`);
  assert.notEqual(start, -1, `function ${name} not found in ui/index.html - was it renamed?`);
  let i = src.indexOf('{', start), depth = 0;
  for (let j = i; j < src.length; j++) {
    if (src[j] === '{') depth++;
    else if (src[j] === '}' && --depth === 0) return src.slice(start, j + 1);
  }
  throw new Error(`unbalanced braces while extracting ${name}`);
}

const viewIndicesSrc = extractFunction(html, 'viewIndices');

// Builds viewIndices with a stub T() returning a tab made of the given column values.
function sortColumn(values, { dir = 1, filters = {} } = {}) {
  const tab = {
    rows: values.map(v => [v]),
    filters,
    sortCol: 0,
    sortDir: dir,
  };
  const viewIndices = new Function('T', `${viewIndicesSrc}\nreturn viewIndices;`)(() => tab);
  return viewIndices('t1').map(ri => tab.rows[ri][0]);
}

test('a DECIMAL column sorts numerically even when trailing zeros are present', () => {
  // The regression: parseFloat("1000.10") is 1000.1, so the old per-pair "does it round-trip?"
  // test failed for this value and fell back to string comparison - while "1.37" and "2.74"
  // passed and compared numerically. Mixing the two rules in one sort is an inconsistent
  // comparator, and "1000.10" came back between "1.37" and "2.74".
  const got = sortColumn(['0.00', '1.37', '1000.10', '2.74', '20.00', '999.90']);
  assert.deepEqual(got, ['0.00', '1.37', '2.74', '20.00', '999.90', '1000.10']);
});

test('the same column reversed is exactly the reverse order', () => {
  const asc = sortColumn(['0.00', '1.37', '1000.10', '2.74', '20.00', '999.90']);
  const desc = sortColumn(['0.00', '1.37', '1000.10', '2.74', '20.00', '999.90'], { dir: -1 });
  assert.deepEqual(desc, [...asc].reverse());
});

test('sorted output is monotonic - the comparator defines one consistent order', () => {
  // Values chosen so that string order and numeric order disagree in both directions, and so
  // that roughly half of them fail a parseFloat round-trip.
  const vals = ['9.50', '10.00', '100.10', '2', '0.30', '1000', '99.99', '3.00'];
  const got = sortColumn(vals).map(Number);
  for (let i = 1; i < got.length; i++) {
    assert.ok(got[i - 1] <= got[i], `not ascending at ${i}: ${got.join(', ')}`);
  }
});

test('integers sort numerically, not as text', () => {
  assert.deepEqual(sortColumn(['1', '2', '10', '20', '100']), ['1', '2', '10', '20', '100']);
});

test('a genuinely textual column still sorts as text', () => {
  assert.deepEqual(sortColumn(['banana', 'apple', 'cherry']), ['apple', 'banana', 'cherry']);
});

test('a column that only looks numeric is not treated as numeric', () => {
  // parseFloat("1abc") is 1, which is why the numeric test matches the WHOLE string instead.
  // One non-numeric value puts the entire column on the text path, so the order stays defined.
  const got = sortColumn(['10', '9', '1abc']);
  assert.deepEqual(got, ['10', '1abc', '9']);
});

test('NULLs sort last and do not make the column non-numeric', () => {
  assert.deepEqual(sortColumn(['10.00', null, '2.00']), ['2.00', '10.00', null]);
});

test('filtering still narrows the view', () => {
  const got = sortColumn(['alpha', 'beta', 'alphabet'], { filters: { 0: 'alpha' } });
  assert.deepEqual(got, ['alpha', 'alphabet']);
});

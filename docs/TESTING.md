# Manual test plan

What automated tests cannot reach: a real server, a real installer, and a real
Windows machine. `cargo test` covers the pure helpers; everything below needs
you.

## Running the automated tests first

`cargo test` from `src-tauri/` runs the offline helpers — statement splitting,
read-only enforcement, literal escaping, CSV edge cases — and needs nothing set
up. CI also runs `cargo clippy --all-targets -- -D warnings`, so a new clippy
warning fails the build.

`npm test` runs the frontend tests in `tests/ui/`, which cover the grid logic
that lives in `ui/index.html` and so is out of `cargo test`'s reach: the grid's
sort/filter ordering, the SQL the Users dialog builds, the connection store and its
request bridge (including reading a whole result for callers other than the grid),
which table a result grid saves to and how saving finds each row, the new-version notice, the table designer (driven with real `information_schema` rows
from MySQL 8 and MariaDB 12), and recovery from a failed procedure/trigger recreate. They pull the functions straight out
of the HTML file rather than keeping a copy, so a change to the real code is
what they measure. No dependencies, no `npm ci` needed. The PowerShell edition runs
the same files against its own inline copy of the UI (`NOBS_UI_SOURCE`), so a
change to one of them changes both editions' tests.

Everything below is what neither of those can reach.

## The live tests in CI

The `live` job in `.github/workflows/test.yml` runs every `#[ignore]`d test on each push, once
against MariaDB and once against MySQL. `tests/ci/start-test-servers.ps1` sets that up on the
Windows runner:

- It downloads MariaDB and MySQL as zip archives (the versions are at the top of the script),
  starts both, and loads `tests/fixtures/seed.sql` into each.
- MySQL is unpacked where a real installation lives (`Program Files\MySQL\MySQL Server <series>`),
  so the app finds MySQL's own client tools there. MariaDB's client tools and login plugins go
  where the app's own download puts them.
- Each server's CA certificate is saved to a file: MySQL's from its data directory, MariaDB's off
  the wire with `openssl`. That way the right-CA checks run too.
- It writes `NOBS_CI_*` values and `MYSQL_BIN`/`MYSQLDUMP_BIN` for the later steps. The
  NOBS-SQL-PS repository runs its live suite with the same script.

It also runs locally, on spare ports and in a folder of your choosing, next to servers you
already have:

```powershell
pwsh -File tests/ci/start-test-servers.ps1 -Root C:\nobs-ci -MariaPort 3316 -MysqlPort 3318 `
     -MysqlHome C:\nobs-ci\mysql -AppData C:\nobs-ci\appdata
```

The servers keep running afterwards; stop `mariadbd` and `mysqld` when done. They write their
own log files (`mariadb.err`, `mysql.err` in the root folder). Redirecting their output
instead made them inherit the script's output handles, and a CI step waits for those to close.

The live tests are `#[ignore]`d so a plain `cargo test` stays offline. A test
whose environment is missing prints one line to stderr and then **passes**, so
`cargo test` reporting `ok` does *not* by itself mean the test ran — check the
count and the timing (`finished in 0.00s` means nothing happened), or run with
`--nocapture` and watch for `... not set - skipping`.

| Variable | Gates | Without it |
|---|---|---|
| `NOBS_TEST_DSN` | all 31 live tests (and the live SSL/CA tests, which are not `#[ignore]`d) | skipped silently |
| `MYSQL_BIN` / `MYSQLDUMP_BIN` | the 6 import/export/CSV/compare tests *within* those 31 | they run, then **fail** with `program not found` |
| `NOBS_TEST_SERVER_CA` | the 2 CA tests that need the server's **own** CA | skipped, with a message |

`NOBS_TEST_SERVER_CA` exists because a CA test without it proves very little. `verify` refuses a
self-signed server with no CA, with the wrong CA, and with a CA that is being silently ignored —
all three look identical. Only the right CA succeeding where the wrong one fails, with nothing
else changed, shows the CA is doing anything. The server hands its chain out during the
handshake, so it can be taken from there without access to the server's files:

```sh
echo | openssl s_client -starttls mysql -connect 127.0.0.1:3308 -showcerts \
  | awk '/BEGIN CERT/{n++} {c[n]=c[n] $0 "\n"} END{printf "%s", c[n]}' \
  | sed -n '/BEGIN CERT/,/END CERT/p' > server-ca.pem
```

That keeps the last certificate printed: MySQL's own CA, or MariaDB's single self-signed
certificate. Point the variable at a path with forward slashes or a quoted one — an unexpanded
`$` in a hand-built Windows path makes the tests quietly skip, which is how an earlier run here
looked green without having run them.

`NOBS_TEST_DSN` is `host:port:user:password`. The two `*_BIN` variables are full
paths to the client tools; they fall back to bare `mysql` / `mysqldump`, which
only works if those are on `PATH` — normally they are not.

Point them at a real copy of the tools. Do not assume the path in the app's own
`config.json` is valid: it records where the app *expects* them
(`%APPDATA%\NOBSSQL-Desktop\bin`), which is an empty directory until the in-app
download has actually run. The app itself copes — it falls back to searching the
system (`tools-status` reports `"found on system"` and picks up e.g. a
`C:\Program Files\MariaDB *\bin` install), so Export and Import still work. The
**tests** do not: they read these two variables and otherwise fall back to a
bare command name. So check the path before you trust it here:

```powershell
$env:NOBS_TEST_DSN  = '127.0.0.1:3306:root:yourpassword'
$env:MYSQL_BIN      = "$env:APPDATA\NOBSSQL-Desktop\bin\mysql.exe"
$env:MYSQLDUMP_BIN  = "$env:APPDATA\NOBSSQL-Desktop\bin\mysqldump.exe"
Test-Path $env:MYSQL_BIN, $env:MYSQLDUMP_BIN     # both must be True
cd src-tauri; cargo test -- --ignored --test-threads=1
```

`--test-threads=1` matters: the live tests share `nobs_test` and will interfere
with each other in parallel. (The `compare_tests` pair additionally serialises
itself, because it replaces and restores the one global `connections.json` — run
in parallel, whichever finished first put the real file back under the other and
it failed with `Connection not found.` on a perfectly healthy setup.)

### Run it against MySQL too, not just MariaDB

The two differ in ways that only surface against the real thing. Everything in
this list was found by pointing the suite at a MySQL 8 server, not by reading:

- MySQL authenticates with `caching_sha2_password` by default. That is a
  *client-side* plugin, and Export/Import could not reach a stock MySQL 8 server
  at all until the downloaded tools started shipping it.
- MySQL and MariaDB name their SSL client options mutually exclusively, so the
  wrong dialect is not a weaker connection but an unknown option and none at all.
- MySQL cannot reference the same `TEMPORARY` table twice in one statement, which
  is why `tests/fixtures/seed.sql` builds `bulk_rows` from a plain table.
- `SLEEP()` interrupted by `KILL QUERY` **returns 1** on MySQL and the statement
  succeeds; MariaDB raises `ER_QUERY_INTERRUPTED`. Anything testing cancellation
  needs a real query, not a sleep — a cancel test built on `SLEEP` reports a
  failure on MySQL when nothing is wrong.

```powershell
$env:NOBS_TEST_DSN = '127.0.0.1:3308:root:yourpassword'   # a MySQL 8 instance
cd src-tauri; cargo test -- --include-ignored --test-threads=1
```

The suite is expected to pass unchanged against MariaDB 12.x and MySQL 8.x alike;
it has been run green against MariaDB 12.2, MariaDB 12.3 and MySQL 8.0.46.

### What the compare tests do to your machine

The two `compare_tests` drive `compare_*`, which resolves its servers by saved
connection *name*, not by inline credentials. So they must write profiles into
`%APPDATA%\NOBSSQL-Desktop\connections.json` — the real file the app uses — and
matching entries into the **OS keyring**. Both are snapshotted and restored when
the test ends, including on a failed assert, so a run leaves no trace. If you
ever see `nobs_cmp_test_rw` or `nobs_cmp_test_ro` survive in your connection
list, a test was killed mid-run; delete them by hand.


Load the fixture first. It is self-contained, creates only `nobs_test`, and
touches no other schema:

```bash
mysql -u root -p < tests/fixtures/seed.sql
```

You can also paste the whole file into the app's editor and run it — that path
is tested and reports `OK. 31 statement(s) executed.`

Select `nobs_test` in the sidebar, then confirm the load:

```sql
SELECT (SELECT COUNT(*) FROM ro_canary)      AS ro_canary,      -- 3
       (SELECT COUNT(*) FROM txn_child)      AS txn_child,      -- 5
       (SELECT COUNT(*) FROM txn_composite)  AS txn_composite,  -- 4
       (SELECT COUNT(*) FROM charset_binary) AS charset_binary, -- 3
       (SELECT COUNT(*) FROM bulk_rows)      AS bulk_rows;      -- 100000
```

Remove it all afterwards with `DROP DATABASE nobs_test;`.

> The fixture deliberately does not end on a `SELECT`. A pasted script whose
> last statement is a `SELECT` is split into two steps that run on separate
> connections, so the `USE` in the script would not reach the `SELECT` and it
> would fail with *No database selected*. Worth knowing when writing your own
> scripts, not just this one.

Work through the scenarios in order — they are sorted by what a failure costs.

---

## The GUI tests

`tests/gui/run.mjs` starts the real app, drives it through its UI over the Chrome DevTools
protocol, and checks what it does. That covers the flows the other tests cannot reach: grid
display and saving, Compare, the export and import dialogs, foreign key lookups and quick
filters, a script's results, and the update notice. It runs both editions:

```powershell
$env:NOBS_TEST_DSN = '127.0.0.1:3306:root:yourpassword'
node tests/gui/run.mjs --app desktop --target src-tauri/target/debug/nobs-sql-editor.exe
node tests/gui/run.mjs --app ps --target ..\NOBS-SQL-PS\NOBSSQL.ps1     # headless Edge
```

- **What it needs:** Node 22 or later.
- **Scenarios:** they are in `tests/gui/scenarios` and run in name order, since later ones use
  what earlier ones created. `--only <text>` runs a subset.
- **Output:** each scenario reports its own checks, and any error message the app shows counts
  as a failure.
- **What it changes:** the scenarios create and drop their own `nobs_gui*` databases and remove
  the connection profiles they save. Both editions get browser storage of their own in a
  temporary folder, so your saved tabs are left alone, and a copy of the app you have open does
  not get in the way.
- **How the desktop app is driven:** the runner sets `NOBS_WEBVIEW_DEBUG_PORT` and
  `NOBS_WEBVIEW_DATA_DIR`, which make the app open WebView2's debugging port and use that folder.
  WebView2's own `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` is not enough: it is ignored in the
  elevated session CI runs in.
- **Server time zone:** a check that needs the hour when the clocks go back is skipped on a
  server whose time zone has none (UTC, as in CI).
- **CI:** the `live` job runs these after the live tests, against MariaDB and MySQL. The
  NOBS-SQL-PS repository runs the same scenarios from here against its own script.

## 1. Read-only / safe mode

The guarantee people rely on before pointing this at production. Connect with
**read-only** ticked, select `nobs_test`, and run each of these. **Every one
must be refused**, and `ro_canary` must still hold exactly 3 rows afterwards.

```sql
DELETE FROM ro_canary;
UPDATE ro_canary SET note = 'modified';
DROP TABLE ro_canary;
TRUNCATE ro_canary;
INSERT INTO ro_canary (label) VALUES ('should-not-exist');
ALTER TABLE ro_canary ADD COLUMN x INT;
GRANT ALL ON nobs_test.* TO 'x'@'%';
CALL p_touch_canary('modified via procedure');
```

Then the two that used to get through — a pasted dump can contain the first
quite innocently, since `mysqldump` emits this syntax routinely:

```sql
/*!50000 DELETE FROM ro_canary */;
SELECT 1; /*!DROP TABLE ro_canary */;
SET GLOBAL max_connections = 1;
SET PERSIST max_connections = 1;
SET @@GLOBAL.max_connections = 1;
```

These must still be **allowed**, or safe mode is useless for actual work:

```sql
SELECT * FROM ro_canary;
SHOW TABLES;
EXPLAIN SELECT * FROM bulk_rows WHERE category = 'alpha';
SET autocommit = 0;
WITH x AS (SELECT 1 AS n) SELECT * FROM x;
```

Also try an **inline grid edit** on `ro_canary` and a row delete. Refusal must
come from the server layer, not merely a greyed-out button.

Confirm nothing moved:

```sql
SELECT COUNT(*) AS must_be_3, SUM(label LIKE 'untouched%') AS must_also_be_3 FROM ro_canary;
```

---

## 2. The pending-changes transaction

The worst outcome this app can produce is a **partial** apply. Reconnect
**without** read-only.

Open `txn_child` in the grid. Stage several edits at once, then make exactly
one of them illegal, and apply:

| Make this edit | It fails with |
|---|---|
| `qty` of row `BBB` → `-1` | `ERROR 4025` CHECK `chk_txn_qty` |
| `parent_id` of row `CCC` → `99` | `ERROR 1452` foreign key |
| `code` of row `DDD` → `AAA` | `ERROR 1062` duplicate key |
| `code` of row `EEE` → **NULL** (the grid's set-NULL action, not an empty cell) | `ERROR 1048` column cannot be null |

Note that clearing `code` to an **empty string** succeeds — `''` is a perfectly valid
value for a `NOT NULL VARCHAR`. Only a real NULL is rejected.

Suggested run: change `descr` on `AAA`, `qty` on `CCC`, **and** `qty` on `BBB`
to `-1`. Apply. The error should surface, and then:

```sql
SELECT GROUP_CONCAT(code ORDER BY code) AS codes, SUM(qty) AS total FROM txn_child;
-- must still be: AAA,BBB,CCC,DDD,EEE   and   150
SELECT descr FROM txn_child WHERE code = 'AAA';   -- must be the ORIGINAL 'first'
```

If `descr` changed while the batch failed, the transaction is not covering the
whole apply — stop and report it.

Repeat on **`txn_composite`**, which has a two-column primary key, to exercise
the multi-column `WHERE` the grid builds. Edit `amount` on the row where
`tenant_id = 1 AND item_code = 'X-1'` and confirm the *other* three rows are
untouched — particularly `(2, 'X-1')`, which shares an item code:

```sql
SELECT tenant_id, item_code, amount FROM txn_composite ORDER BY tenant_id, item_code;
```

Also delete a row there and confirm exactly one disappears.

A trigger guards inserts too — adding a row with `qty = -5` must fail with
`ERROR 1644  qty must not be negative`.

---

## 3. Export / Import cancel

Never yet tested against real tooling.

1. Export `nobs_test` (both `bulk_rows` and `bulk_rows_2`, 200k rows total) to
   SQL. While it runs, click **Cancel**.
2. It should stop within a second or two, and the log should say `CANCELLED`.
3. Check Task Manager: **no `mysqldump.exe` left running.**
4. Check the output folder: a partial file may exist, but nothing should still
   be growing.
5. Repeat for Import, using the file from a completed export.

Then do it again and *let it finish*, to confirm cancel did not break the
normal path. Re-import into a scratch database and compare:

```sql
SELECT COUNT(*) FROM bulk_rows;      -- 100000
SELECT SUM(amount) FROM bulk_rows;   -- must match the source
```

---

## 4. Character sets and binary data

`charset_binary` holds an emoji, a ZWJ family sequence, CJK, RTL, accents,
embedded quotes and comment markers, `VARBINARY`, `BLOB`, `BIT(1)`, `BIT(8)`,
and — deliberately — a NULL column beside an empty-string column.

Baseline, straight from the server:

```sql
SELECT id, emoji, CHAR_LENGTH(emoji) AS chars, LENGTH(emoji) AS bytes,
       accents, cjk, rtl, quoted,
       HEX(bin_col) AS bin_hex, HEX(blob_col) AS blob_hex,
       bit_col + 0 AS bit1, bit8 + 0 AS bit8,
       null_col IS NULL AS null_is_null, empty_col = '' AS empty_is_empty
FROM charset_binary;
```

Expected: row 1 emoji is **1 char / 4 bytes**, row 2 is **5 chars / 18 bytes**,
`bin_hex` `00FF10`, `bit1` `1` then `0`, `bit8` `170` then `1`, row 3 entirely
NULL.

Now check the app against that:

- Does the grid render the emoji, CJK and RTL text, or boxes?
- Is `NULL` **visually distinct** from the empty string next to it? This is the
  one most likely to be wrong, and silently.
- Edit `accents` on row 1, save, re-read. Did the characters survive?
- Edit `bit_col` on row 2 from `0` to `1`, save, and confirm with
  `SELECT bit_col + 0 FROM charset_binary WHERE id = 2;` — this is the
  unquoted `0x…` literal path, which is subtle.
- Export the table to **CSV** and open it: are the quotes in
  `O'Brien said "hi"; then left -- and # too` escaped correctly, and is NULL
  still distinguishable from empty?
- Export to **INSERT statements** and re-run them into a scratch schema, then
  diff with the query above.

---

## 5. Clean-VM install

The one unverified link between CI and a working app. On a Windows VM with
**no** Rust, Node, Visual Studio or MySQL tooling:

1. Download the `.exe` from the GitHub release — not a locally built one.
2. Install. Expect SmartScreen's *"Windows protected your PC"*; **More info →
   Run anyway**. Confirm that matches what the README tells users.
3. Launch, connect to a database, run `SELECT 1`.
4. Open Settings — with no client tools present, does it say so clearly, and
   does the **Download MariaDB client tools** button work end to end?
5. Try Export before configuring anything: the error must name Settings and
   offer the **Open Settings…** button.
6. Uninstall, and check `%APPDATA%\NOBSSQL` — saved passwords should be gone
   from the credential store, or the uninstaller should say they remain.

---

## 6. Large results

```sql
SELECT * FROM bulk_rows;                      -- 100k rows, no LIMIT
SELECT * FROM bulk_rows ORDER BY amount DESC; -- forces a sort
SELECT * FROM bulk_rows a JOIN bulk_rows_2 b ON a.id = b.id;   -- slow, cancel this one
```

Watch for: memory in Task Manager, whether the window stays responsive, whether
**Cancel** actually stops the third query, and how long the grid takes to
appear. Then exercise the grid itself — sort by `amount`, filter `category` to
`alpha`, hide a column, and open the row-detail view on a wide row.

Finally, export those 100k rows to CSV and confirm the file has 100,001 lines
(header included) and that rows with a NULL `note` — every 7th — are written
consistently.

---

## Reporting

For anything that fails, the useful details are: which scenario, the exact SQL,
what you expected, what happened, and whether the data survived. A failure in
**1** or **2** is a stop-everything bug — those are the two guarantees that
protect someone's data.

## Checking real data for corruption

Both editions write binary columns the same way, so the same audit covers either. The script lives
in the sibling [NOBS-SQL-PS](https://github.com/monsama/NOBS-SQL-PS) repo, at
`tools/Check-BlobIntegrity.ps1`, and is read-only — it runs SELECTs against a live server and
writes nothing:

```powershell
pwsh -NoProfile -File ..\NOBS-SQL-PS\tools\Check-BlobIntegrity.ps1 -Dsn '127.0.0.1:3306:root:yourpassword'
```

It looks for the signatures this app has actually produced: a value beginning with the two
characters `0x`, a long `0x` hex run embedded in other content, a value that is entirely bare hex,
a U+FFFD replacement character, and NUL bytes in text columns. See that repo's docs/TESTING.md for
what each one means and two worked examples - one a genuine loss, one a false alarm.

Worth running after any session of hand-editing binary columns, and before trusting a backup.

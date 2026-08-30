# Manual test plan

What automated tests cannot reach: a real server, a real installer, and a real
Windows machine. `cargo test` covers the pure helpers; everything below needs
you.

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
| `code` of row `EEE` → empty | `ERROR 1048` column cannot be null |

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

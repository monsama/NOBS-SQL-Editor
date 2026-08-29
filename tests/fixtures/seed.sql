-- ---------------------------------------------------------------------------
-- NOBS SQL Editor - manual test fixture
--
-- Creates a self-contained database, nobs_test, holding the awkward cases the
-- app has to survive: constraints that reject a mid-transaction write, a
-- composite primary key, four-byte emoji, binary and BIT columns, NULLs that
-- must stay distinct from empty strings, and 100k rows for the large-result
-- pass. Every table is prefixed by the scenario it exists for.
--
--   mysql -u root -p < tests/fixtures/seed.sql
--
-- Drop it all again with:  DROP DATABASE nobs_test;
-- Nothing here touches any other schema.
-- ---------------------------------------------------------------------------

DROP DATABASE IF EXISTS nobs_test;
CREATE DATABASE nobs_test DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
USE nobs_test;

-- --- 1. read-only / safe mode ----------------------------------------------
-- A table that is cheap to damage and obvious when damaged.
CREATE TABLE ro_canary (
  id    INT PRIMARY KEY AUTO_INCREMENT,
  label VARCHAR(64) NOT NULL,
  note  VARCHAR(255) NULL
) ENGINE=InnoDB;
INSERT INTO ro_canary (label, note) VALUES
  ('untouched-1', 'if this row disappears, read-only mode failed'),
  ('untouched-2', 'there should be exactly 3 rows at all times'),
  ('untouched-3', NULL);

-- --- 2. the pending-changes transaction ------------------------------------
-- A parent to point foreign keys at.
CREATE TABLE txn_parent (
  id   INT PRIMARY KEY,
  name VARCHAR(32) NOT NULL
) ENGINE=InnoDB;
INSERT INTO txn_parent (id, name) VALUES (1,'alpha'), (2,'beta');

-- Every column here can reject a write in a different way: NOT NULL, a CHECK,
-- a UNIQUE index, and a foreign key. Staging several edits and making ONE of
-- them illegal is the test - none of the others may survive.
CREATE TABLE txn_child (
  id        INT PRIMARY KEY AUTO_INCREMENT,
  parent_id INT          NOT NULL,
  code      VARCHAR(16)  NOT NULL UNIQUE,
  qty       INT          NOT NULL DEFAULT 0,
  descr     VARCHAR(100) NULL,
  CONSTRAINT fk_txn_child_parent FOREIGN KEY (parent_id) REFERENCES txn_parent(id),
  CONSTRAINT chk_txn_qty CHECK (qty >= 0)
) ENGINE=InnoDB;
INSERT INTO txn_child (parent_id, code, qty, descr) VALUES
  (1,'AAA',10,'first'), (1,'BBB',20,'second'), (2,'CCC',30,'third'),
  (2,'DDD',40,NULL),    (1,'EEE',50,'fifth');

-- Composite primary key: editing and deleting rows here exercises the
-- multi-column WHERE the grid has to build to identify a row.
CREATE TABLE txn_composite (
  tenant_id INT         NOT NULL,
  item_code VARCHAR(16) NOT NULL,
  amount    DECIMAL(10,2) NOT NULL DEFAULT 0.00,
  label     VARCHAR(64) NULL,
  PRIMARY KEY (tenant_id, item_code)
) ENGINE=InnoDB;
INSERT INTO txn_composite VALUES
  (1,'X-1',10.50,'tenant one, item one'),
  (1,'X-2',20.00,NULL),
  (2,'X-1',30.25,'same item code, different tenant'),
  (2,'X-2',0.00,'zero amount');

-- --- 4. character sets and binary data -------------------------------------
-- NULL vs empty string is the subtle one: they must stay distinguishable in
-- the grid, in CSV export, and after a round-trip through an edit.
CREATE TABLE charset_binary (
  id       INT PRIMARY KEY AUTO_INCREMENT,
  emoji    VARCHAR(64)  CHARACTER SET utf8mb4 NULL,
  accents  VARCHAR(64)  NULL,
  cjk      VARCHAR(64)  NULL,
  rtl      VARCHAR(64)  NULL,
  quoted   VARCHAR(128) NULL,
  bin_col  VARBINARY(32) NULL,
  blob_col BLOB NULL,
  bit_col  BIT(1) NULL,
  bit8     BIT(8) NULL,
  null_col VARCHAR(16) NULL,
  empty_col VARCHAR(16) NULL
) ENGINE=InnoDB;
-- Explicit ids so the UPDATEs below cannot drift with AUTO_INCREMENT.
INSERT INTO charset_binary
  (id, emoji, accents, cjk, rtl, quoted, bin_col, blob_col, bit_col, bit8, null_col, empty_col) VALUES
  (1, NULL, NULL, NULL, NULL,
   'O''Brien said "hi"; then left -- and # too', 0x00FF10, 0xDEADBEEF, b'1', b'10101010', NULL, ''),
  (2, NULL, 'Ljuca', NULL, NULL,
   'tab\there, newline\nthere, backslash \\ here', 0x000000, NULL, b'0', b'00000001', NULL, ''),
  (3, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);

-- The non-ASCII values go in as hex so this file itself stays pure ASCII and
-- cannot be mangled by an editor, a transfer, or a shell with the wrong locale.
-- Row 3 is left entirely NULL on purpose.
UPDATE charset_binary SET emoji = _utf8mb4 X'F09F9880'                                WHERE id = 1;  -- one emoji, 4 bytes
UPDATE charset_binary SET emoji = _utf8mb4 X'F09F91A8E2808DF09F91A9E2808DF09F91A6'    WHERE id = 2;  -- family ZWJ sequence
UPDATE charset_binary SET accents = _utf8mb4 X'5AC3BC72696368202D204772C3BC657A69'    WHERE id = 1;  -- Zurich - Gruezi
UPDATE charset_binary SET cjk = _utf8mb4 X'E4B8ADE69687E6B58BE8AF95'                  WHERE id = 1;
UPDATE charset_binary SET cjk = _utf8mb4 X'E697A5E69CACE8AA9E'                        WHERE id = 2;
UPDATE charset_binary SET rtl = _utf8mb4 X'D985D8B1D8ADD8A8D8A7'                      WHERE id = 1;
UPDATE charset_binary SET rtl = _utf8mb4 X'D7A9D79CD795D79D'                          WHERE id = 2;

-- --- 6. large results -------------------------------------------------------
-- 100k rows built by cross-joining a digits table: portable across MySQL and
-- MariaDB, and far faster than a recursive CTE or a loop.
CREATE TABLE bulk_rows (
  id       INT PRIMARY KEY,
  name     VARCHAR(64)  NOT NULL,
  category VARCHAR(16)  NOT NULL,
  amount   DECIMAL(12,2) NOT NULL,
  note     VARCHAR(200) NULL,
  created  DATETIME     NOT NULL,
  KEY idx_bulk_category (category)
) ENGINE=InnoDB;

CREATE TEMPORARY TABLE _d (d INT);
INSERT INTO _d VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);
INSERT INTO bulk_rows (id, name, category, amount, note, created)
SELECT n, CONCAT('row-', LPAD(n, 6, '0')),
       ELT(1 + (n % 5), 'alpha','beta','gamma','delta','epsilon'),
       ROUND(n * 1.37, 2),
       CASE WHEN n % 7 = 0 THEN NULL
            ELSE CONCAT('note for row ', n, ' - padding to make the row wider') END,
       DATE_ADD('2020-01-01 00:00:00', INTERVAL n MINUTE)
FROM (SELECT a.d + b.d*10 + c.d*100 + e.d*1000 + f.d*10000 AS n
      FROM _d a, _d b, _d c, _d e, _d f) seq;
DROP TEMPORARY TABLE _d;

-- --- 3. export / import cancel ---------------------------------------------
-- bulk_rows above is what makes an export slow enough to cancel. A second
-- copy gives a multi-table export something to still be working through.
CREATE TABLE bulk_rows_2 LIKE bulk_rows;
INSERT INTO bulk_rows_2 SELECT * FROM bulk_rows;

-- --- a view, a procedure and a trigger, so object browsing has something ----
CREATE VIEW v_bulk_alpha AS SELECT id, name, amount FROM bulk_rows WHERE category = 'alpha';

DELIMITER $$
CREATE PROCEDURE p_touch_canary(IN p_note VARCHAR(255))
BEGIN
  UPDATE ro_canary SET note = p_note WHERE label = 'untouched-1';
  SELECT ROW_COUNT() AS rows_touched;
END$$
CREATE TRIGGER trg_txn_child_guard BEFORE INSERT ON txn_child
FOR EACH ROW
BEGIN
  IF NEW.qty < 0 THEN
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'qty must not be negative';
  END IF;
END$$
DELIMITER ;

SELECT 'seed complete' AS status,
       (SELECT COUNT(*) FROM ro_canary)      AS ro_canary,
       (SELECT COUNT(*) FROM txn_child)      AS txn_child,
       (SELECT COUNT(*) FROM txn_composite)  AS txn_composite,
       (SELECT COUNT(*) FROM charset_binary) AS charset_binary,
       (SELECT COUNT(*) FROM bulk_rows)      AS bulk_rows;

# NOBS SQL Editor

A lightweight desktop client for **MySQL** and **MariaDB**, built with
[Tauri](https://tauri.app) (Rust backend + HTML/JS frontend).

**Windows** is the platform that is built, tested and released. The code itself is
portable and Tauri supports macOS and Linux, so it can be built from source there
(see [Building from source](#building-from-source)) - but no installers are published
for them, and CI does not test them, so treat that as unsupported rather than ready.

**On a machine where you cannot install software**, there is a second edition:
[NOBS SQL Editor, PowerShell edition](https://github.com/monsama/nobs-sql-editor-powershell)
is the same UI as a single `.ps1` script that starts a local server and opens your
browser - nothing to install, no admin rights, and no installer for SmartScreen to
object to. It shells out to `mysql.exe` for everything, so it needs the client tools
where this one can talk to the server directly.

## Download

Windows installers are published on the
[Releases](https://github.com/monsama/nobs-sql-editor/releases) page.

They are **not code-signed**, so Windows SmartScreen shows a "Windows protected
your PC" warning naming an unknown publisher. Choose **More info -> Run anyway**
to continue.

Releases from 1.3.5 onward publish `SHA256SUMS.txt` next to the installers, and print
the same values in their notes, so you can check that what you downloaded is what CI
built (1.3.4 and earlier predate this):

```powershell
Get-FileHash .\NOBS.SQL.Editor_1.3.5_x64-setup.exe -Algorithm SHA256
```

[CODE_SIGNING.md](CODE_SIGNING.md) says what that does and does not prove, and why
there is no certificate.

### Uninstalling

1. To remove saved connections and their passwords as well, first open **Settings -> Clear all
   app data** in the app.
2. Uninstall **NOBS SQL Editor** from Windows **Settings -> Apps -> Installed apps** (or
   "Programs and Features").
3. Optionally delete what the app keeps for your user account:
   - `%APPDATA%\NOBSSQL-Desktop` - connections, settings, the query library, the log, and
     downloaded client tools;
   - `%LOCALAPPDATA%\ch.monsama.nobssqleditor` - the app window's browser data (open tabs,
     layout);
   - in Windows **Credential Manager** (Windows Credentials), any entries ending in
     `.NOBSSQL-Desktop` - saved connection passwords, if step 1 was skipped.

Export files are kept wherever you saved them.

## Features

- Connect to MySQL / MariaDB with saved connection profiles (passwords stored in
  the OS keychain), per-connection accent color, environment label, and a
  **read-only / safe mode** to protect production servers.
- Browse schemas and objects (tables, views, procedures, functions, triggers,
  events) with quick filtering.
- Tabbed SQL editor with syntax highlighting, lightweight autocomplete, run whole
  script or selection, and result grids with per-column filtering and sorting.
  A procedure call, or a script with several SELECTs, shows each result in a tab
  of its own.
- Inline and full-row editing with a staged pending-changes model applied inside
  a transaction; add / delete rows.
- Column resize and show/hide; row-detail form view for wide tables.
- Export whole tables or query results to CSV or INSERT statements (streamed,
  handles large tables); copy CSV/TSV to clipboard.
- Table designer, DDL view/edit, users & privileges, table maintenance,
  CSV import, and a reusable query library (with export/import).
- Data export / import via the MySQL/MariaDB command-line tools.

## Keeping data exact

- **Saving grid edits:** each change is checked, inside the same transaction, to match exactly
  one row. Otherwise nothing is saved. This catches a row changed or deleted since it was loaded,
  and a TIMESTAMP key in the hour the clocks go back (it shows the same as its neighbour). FLOAT
  keys are shown rounded, so they are matched by their text.
- **Grid edits go to the database the query ran in**, including after a leading `USE`.
- **Compare runs both connections in UTC.** TIMESTAMP values therefore copy correctly between
  servers in different time zones, and Compare shows them in UTC.
- **Copies name their columns** (Compare, Duplicate table, INSERT exports). Invisible columns are
  included; generated columns are left out, since the server computes them. CSV exports include
  every column, and the CSV import skips generated ones.
- **INSERT exports skip rows whose key already exists** (`ON DUPLICATE KEY UPDATE`). They used
  `INSERT IGNORE`, which also cuts a value that does not fit instead of failing.
- **The CSV import is strict.** Header names match the table's columns ignoring case. A column
  the table does not have, or a row with more or fewer fields than the header, imports nothing.
  Foreign key and unique checks stay on, and the whole file is one transaction.
- **A per-table export is one snapshot.** The whole database is dumped once and then split into
  one file per table or view, so the files are consistent with each other even while the database
  is being written to. Two tables whose names give the same file name get two files.
- **Schema sync writes each column as the source server defines it**, including its character
  set, collation, comment and generated expression.

## SSL / TLS

Each connection has an SSL mode, and optionally a CA certificate (a `.pem` file) that the two
verifying modes check the server against.

| Mode | Encrypted | Certificate checked against the CA | Host name checked |
|---|---|---|---|
| `default` | as negotiated | – | – |
| `disabled` | no | – | – |
| `required` | yes | no | no |
| `verify-ca` | yes | yes | no |
| `verify` | yes | yes | yes |

Without a CA, the verifying modes check against the Windows trust store.

**If the server uses the certificate MariaDB or MySQL generated for itself** — which is what you
get when nobody configured one — use **`verify-ca` with the server's CA**. That certificate is
self-signed, so no trust store accepts it, and it never names a real host (MySQL's is issued to
`MySQL_Server_<version>_Auto_Generated_Server_Certificate`), so `verify` refuses it even with the
right CA. The connection error says which of the two happened.

Where to get the CA: for MySQL it is `ca.pem` in the server's data directory. MariaDB's generated
certificate has no separate CA — use the certificate itself. Either can also be read off the
connection, which needs no access to the server's files:

```sh
echo | openssl s_client -starttls mysql -connect HOST:PORT -showcerts
```

The CA is the last certificate printed (for MariaDB, the only one).

Export and Import run the command-line client (below) with the same settings. The MariaDB client
has no way to check a CA without also checking the host name, except on connections to the local
machine, so there `verify-ca` is carried out as full `verify`. It never checks less than you asked
for — at worst a remote export fails where a query on the same connection works.

## Client tools (mysql / mysqldump)

Export and Import use the official MySQL/MariaDB command-line tools. These are
**not bundled** with this application. On first use you can either point the app
at an existing install (Settings) or let it download the official MariaDB client
tools from mariadb.org on demand. The archive is checked against the SHA-256 that
MariaDB's own release API publishes for it before anything is unpacked, and a
mismatch installs nothing — the checksum comes from the API, not from the mirror
the bytes came from, so a redirected or altered download fails the check. If the
API lists no checksum, nothing is downloaded at all.

**MySQL servers get MySQL's own tools** when there are any: the two optional
"MySQL servers" paths in Settings, or else the newest MySQL Server installation
(`Program Files\MySQL\MySQL Server *\bin`). Export and Import ask the server
what it is and pick the pair to match; MariaDB servers, and MySQL servers on a
machine without MySQL's tools, use the default pair. It matters because MariaDB's
mysqldump writes values into a MySQL table's generated columns, which MySQL
refuses when the dump is restored. Without MySQL's tools such an export is
refused rather than written.

**No MySQL installed?** Settings can download MySQL's own `mysql` and `mysqldump`
(the current 8.4 LTS release from dev.mysql.com). MySQL publishes Windows binaries
only as the full server archive, so this is a ~270 MB download of which about
14 MB is kept, in `bin\mysql\`. The archive is checked against the MD5 on MySQL's
download page before anything is unpacked, and a mismatch installs nothing. If
MySQL moves its page or files, `mysql_download_page` and
`mysql_download_url_template` (with `{series}`, `{version}`, `{file_name}`) in
the config file override the defaults.

## Updates

A few seconds after it starts, the app asks GitHub (`api.github.com`) for the latest release of
[nobs-sql-editor](https://github.com/monsama/nobs-sql-editor/releases). If a newer version exists, a small
notice with a link appears in the bottom-left corner. Nothing is downloaded or installed. The
request carries nothing beyond what any web request does: your IP address and a user agent
naming the app.

Hide the notice with its **×** and it stays hidden until the next version. Switch the check off,
or run it by hand, under **Settings → Updates**.

## Building from source

Requirements: [Rust](https://rustup.rs) (with the MSVC toolchain on Windows),
[Node.js](https://nodejs.org), and the Tauri prerequisites for your platform.

```bash
npm install
npm run tauri dev     # run in development
npm run tauri build   # produce installers (NSIS .exe / MSI on Windows)
```

## License

This program is free software, licensed under the **GNU General Public License
version 2** (or, at your option, any later version). See [LICENSE](LICENSE).

Copyright (C) 2026 Viktor Ljuca — https://monsama.ch

## Third-party components

Built with Tauri and the Rust crates mysql, reqwest, zip, keyring, tokio, serde,
serde_json, regex, dirs, csv, hex, tempfile and chrono (and their dependencies),
each under its own license (mostly MIT / Apache-2.0). The MariaDB client tools,
when downloaded, are © MariaDB Foundation under GPLv2 and are obtained from
mariadb.org; they are not bundled with this application.

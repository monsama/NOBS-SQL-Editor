# NOBS SQL Editor

A lightweight, cross-platform desktop client for **MySQL** and **MariaDB**, built
with [Tauri](https://tauri.app) (Rust backend + HTML/JS frontend).

## Download

Windows installers are published on the
[Releases](https://github.com/monsama/NOBS-SQL-Editor/releases) page.

They are **not code-signed yet**, so Windows SmartScreen shows a "Windows
protected your PC" warning naming an unknown publisher. Choose **More info ->
Run anyway** to continue. See [CODE_SIGNING.md](CODE_SIGNING.md) for the plan.

## Features

- Connect to MySQL / MariaDB with saved connection profiles (passwords stored in
  the OS keychain), per-connection accent color, environment label, and a
  **read-only / safe mode** to protect production servers.
- Browse schemas and objects (tables, views, procedures, functions, triggers,
  events) with quick filtering.
- Tabbed SQL editor with syntax highlighting, lightweight autocomplete, run whole
  script or selection, and result grids with per-column filtering and sorting.
- Inline and full-row editing with a staged pending-changes model applied inside
  a transaction; add / delete rows.
- Column resize and show/hide; row-detail form view for wide tables.
- Export whole tables or query results to CSV or INSERT statements (streamed,
  handles large tables); copy CSV/TSV to clipboard.
- Table designer, DDL view/edit, users & privileges, table maintenance,
  CSV import, and a reusable query library (with export/import).
- Data export / import via the MySQL/MariaDB command-line tools.

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
tools from mariadb.org on demand.

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

# NOBS SQL Editor

A lightweight, cross-platform desktop client for **MySQL** and **MariaDB**, built
with [Tauri](https://tauri.app) (Rust backend + HTML/JS frontend).

> Free code signing on Windows provided by [SignPath.io](https://signpath.io),
> certificate by the [SignPath Foundation](https://signpath.org).

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
serde_json, dirs, csv, hex, tempfile and chrono (and their dependencies), each
under its own license (mostly MIT / Apache-2.0). The MariaDB client tools, when
downloaded, are © MariaDB Foundation under GPLv2 and are obtained from
mariadb.org; they are not bundled with this application.

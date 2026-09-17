# Code signing policy

Free code signing provided by [SignPath.io](https://signpath.io), certificate by
[SignPath Foundation](https://signpath.org).

## Current status

Releases are **not code-signed**. Windows SmartScreen warns that the publisher
is unknown; choose **More info -> Run anyway** to install.

Code signing via the [SignPath Foundation](https://signpath.org) is planned but
not yet in place. The release workflow already contains the signing steps; they
stay inactive until the project is accepted and SignPath credentials are
configured, so every release until then is published unsigned. This document
describes the policy that will apply once signing is active.

## What will be signed
- Windows installer packages (`.exe` / `.msi`) published on GitHub Releases.

## Build & signing process
- Artifacts are built from this repository using GitHub Actions CI.
- Only CI-built artifacts are submitted to SignPath for signing.
- The signing private key is held by SignPath (HSM-backed); this project does
  not store or have access to the private key.

## Roles
- **Authors** (may change the code): [Viktor Ljuca](https://github.com/monsama)
- **Reviewers** (review and approve changes from others): [Viktor Ljuca](https://github.com/monsama)
- **Approvers** (approve each release for signing): [Viktor Ljuca](https://github.com/monsama)

All team members use multi-factor authentication for GitHub and SignPath.

## Privacy
This application does not transmit user data to the author, and has no
telemetry. Database credentials are stored locally (OS keychain / local
config), and database connections go only to the servers the user configures.

Apart from those, the application makes these network requests:
- **Update check:** a few seconds after start, a request to the GitHub API
  (`api.github.com`) for the latest release of this project, to show a notice
  when a newer version exists. It sends nothing beyond the request itself, and
  can be switched off under Settings -> Updates.
- **Client tools download,** only when the user clicks the button in Settings:
  - the MariaDB client tools, from mariadb.org (`downloads.mariadb.org` and
    `mirror.mariadb.org`);
  - the MySQL client tools, from dev.mysql.com (`dev.mysql.com` and
    `cdn.mysql.com`, or `downloads.mysql.com` for older releases).

The release page (github.com) and the author's donation page open in the
user's browser only when the user clicks them.

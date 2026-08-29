# Code signing policy

Free code signing on Windows is provided by [SignPath.io](https://signpath.io),
certificate by the [SignPath Foundation](https://signpath.org).

## What is signed
- Windows installer packages (`.exe` / `.msi`) published on GitHub Releases.

## Build & signing process
- Artifacts are built from this repository using GitHub Actions CI.
- Only CI-built artifacts are submitted to SignPath for signing.
- The signing private key is held by SignPath (HSM-backed); this project does
  not store or have access to the private key.

## Roles
- Author / maintainer: Viktor Ljuca (https://monsama.ch)
- Reviewers: (add GitHub usernames of anyone who reviews releases)

## Privacy
This application does not transmit user data to the author. Database
credentials are stored locally (OS keychain / local config). The only outbound
network request is an optional, user-initiated download of the MariaDB client
tools from mariadb.org.

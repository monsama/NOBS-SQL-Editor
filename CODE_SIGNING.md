# Code signing policy

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
- Author / maintainer: Viktor Ljuca (https://monsama.ch)
- Release approval: the maintainer, who reviews and tags each release.

## Privacy
This application does not transmit user data to the author. Database
credentials are stored locally (OS keychain / local config). The only outbound
network request is an optional, user-initiated download of the MariaDB client
tools from mariadb.org.

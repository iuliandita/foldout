# Security policy

## Private reporting

Report vulnerabilities through [GitHub private vulnerability reporting](https://github.com/iuliandita/foldout/security/advisories/new). Do not post exploit details, secrets or vulnerable library data in public issues. Include the affected server version, nightly image tag or source commit, impact, and a minimal reproduction using disposable state and fixtures. Redact unrelated credentials and personal data.

The maintainer coordinates fixes and disclosure with the reporter. There is no guaranteed response time or commercial support commitment.

## Supported development builds

Foldout has no tagged releases yet. Security fixes land on `develop` and its nightly images; older commits and nightly images receive no backports. There is no stable support window. Upgrade with a verified backup, retain the previous executable or image, and restore into fresh state for rollback as described in [operations](docs/operations.md).

This policy covers Foldout's server, reader, API, acquisition, backup/restore and container code. Third-party providers, download clients, media and operator infrastructure remain outside this repository's security support. Keep original media read-only and use disposable state when investigating; never exercise real download clients in a reproduction.

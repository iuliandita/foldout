# Documentation maintenance

This is the first, documentation-only increment of [issue #48](https://github.com/iuliandita/foldout/issues/48).
Foldout is unreleased; the nightly channel is a development build, not a tagged release.
There is no released upgrade history or promise of backward schema compatibility.
Automated source-impact checks are still future work. This pilot adds no checker
and does not change the existing acceptance gate.

## Source-to-documentation map

Paths below identify review starting points, not a second inventory of API endpoints,
configuration values, or supported integrations. Follow the changed behavior through
its callers and tests when deciding which documentation needs updating.

| Area | Source authority and review starting points | Documentation to review |
| --- | --- | --- |
| Rust configuration | [src/config.rs](../src/config.rs), [src/main.rs](../src/main.rs), [src/bootstrap.rs](../src/bootstrap.rs) | [README.md](../README.md) environment table and local startup; [operations.md](operations.md) container and recovery configuration |
| CLI commands | [src/cli.rs](../src/cli.rs) | [README.md](../README.md) command summary; [operations.md](operations.md) backup, restore, and service examples |
| HTTP API and browser types | [api/openapi.yaml](../api/openapi.yaml), [src/httpapi/mod.rs](../src/httpapi/mod.rs), [scripts/generate-api](../scripts/generate-api), generated [web/src/lib/api/schema.d.ts](../web/src/lib/api/schema.d.ts), [web/src/lib/api/client.ts](../web/src/lib/api/client.ts) | [README.md](../README.md) API and generation guidance; relevant behavior in [operations.md](operations.md) and [integrations.md](integrations.md) |
| SQLite schema and startup migrations | [migrations/](../migrations/), [src/store/sqlite.rs](../src/store/sqlite.rs), [build.rs](../build.rs) | [operations.md](operations.md) restore, upgrade, and rollback guidance; [README.md](../README.md) schema compatibility summary |
| Backup format and encryption | [src/backup/mod.rs](../src/backup/mod.rs), [src/backup/files.rs](../src/backup/files.rs), [src/settings/encryption.rs](../src/settings/encryption.rs), [tests/backup_restore.rs](../tests/backup_restore.rs) | [operations.md](operations.md) snapshot layout, permissions, key handling, validation, and recovery limits |
| Reader formats and limits | [src/reader/](../src/reader/), [tests/reader_http.rs](../tests/reader_http.rs), [tests/fixtures/manifest.json](../tests/fixtures/manifest.json), [scripts/check-formats](../scripts/check-formats) | [formats.md](formats.md); [README.md](../README.md) reader summary; [operations.md](operations.md) decoder dependencies |
| Providers, clients, and acquisition | [src/providers/](../src/providers/), [src/clients/](../src/clients/), [src/acquisition/](../src/acquisition/) | [integrations.md](integrations.md), [src/providers/README.md](../src/providers/README.md), [src/clients/README.md](../src/clients/README.md), [README.md](../README.md); [formats.md](formats.md) for chapter packaging limits |
| Rust build and deployment configuration | [Cargo.toml](../Cargo.toml), [Cargo.lock](../Cargo.lock), [rust-toolchain.toml](../rust-toolchain.toml), [build.rs](../build.rs), [Dockerfile](../Dockerfile), [compose.yaml](../compose.yaml), [scripts/check](../scripts/check), [CI workflow](../.github/workflows/ci.yml) | [README.md](../README.md) build/check instructions; [operations.md](operations.md) toolchain, image, runtime, and verification guidance |
| Web assets and build tools | [web/src/](../web/src/), [web/index.html](../web/index.html), [web/package.json](../web/package.json), [web/bun.lock](../web/bun.lock), [web/tsconfig.json](../web/tsconfig.json), [web/vite.config.ts](../web/vite.config.ts), [tools/package.json](../tools/package.json), [tools/bun.lock](../tools/bun.lock), [src/webassets.rs](../src/webassets.rs) | [README.md](../README.md) frontend development and embedded builds; [operations.md](operations.md) asset packaging; behavior-specific format/integration docs |

The API contract remains `api/openapi.yaml`; generate browser types with
`./scripts/generate-api`, rather than hand-editing `schema.d.ts` or maintaining
a competing API list. Keep English and Spanish locale key sets equal when changing UI text.

## PR and release review obligation

For each affected area, update the relevant documentation in the PR or give a
specific no-impact explanation in the PR description. Cite the changed source
and the documentation reviewed, and explain why the documented behavior remains
accurate. A generic "docs not needed" is not an assessment. Documentation cited
as updated must actually change in the PR; reassess after source changes.

Review the full PR-base diff. Before a release, review the combined release range:
a later query-only change does not erase an earlier migration or backup-format
change. Check local links and compare documented configuration names, defaults,
and examples with current source. Distinguish supported formats, fixture-tested
integrations, recorded live evidence, and planned capabilities. Do not turn local
protocol tests into claims of production interoperability or invent release history.

For migrations, backup-format changes, or encryption compatibility changes,
provide actionable notes in [operations.md](operations.md):

- **Upgrade:** Affected schema/format versions, prerequisites, migration timing,
  expected data effects, and how to verify success. Add numbered migrations; never edit published migrations.
- **Backup:** How to retain and verify a pre-upgrade database/key recovery point,
  protect permissions and keys, and separately preserve media/configuration as needed.
- **Rollback:** Retain the previous executable/image and original state; restore a
  compatible backup into a fresh directory. Do not run an older binary against
  newly migrated state or imply an automatic downgrade exists.
- **Compatibility:** State which old databases, snapshots, and encrypted settings
  remain readable, which are rejected, and any irreversible steps or recovery limits.

Use disposable copies for upgrade/restore evidence. Backup sources stay read-only.
Run the existing [scripts/check](../scripts/check) gate after dependencies are installed;
it runs offline and compares generated API types with the contract. Required CI
also includes **Check** and **Docker build**. Missing, failed, or stale results do
not establish readiness. These checks do not yet enforce the documentation review
obligation, prove prose correct, or demonstrate a live upgrade/restore.

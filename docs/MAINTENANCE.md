# Documentation maintenance

This is the manual source-to-documentation map for issue [#48](https://github.com/iuliandita/foldout/issues/48), not an automated drift gate. Foldout remains unreleased; do not invent release history or treat planned capabilities as supported behavior.

## Source-to-documentation map

Paths below are relative to this document. Review the corresponding documentation when changing these sources.

| Area | Sources to inspect | Documentation to assess |
| --- | --- | --- |
| Rust configuration and CLI | [Configuration](../src/config.rs), [CLI](../src/cli.rs) | [README](../README.md) environment variables, defaults, local commands; [operations](operations.md) startup, backup and restore commands |
| API contract and browser types | [OpenAPI](../api/openapi.yaml), [HTTP routes](../src/httpapi/mod.rs), [generator](../scripts/generate-api), [generated types](../web/src/lib/api/schema.d.ts), [browser client](../web/src/lib/api/client.ts) | [README](../README.md) API and authentication guidance; [operations](operations.md) operational examples |
| SQLite migrations | [Store and migration handling](../src/store/sqlite.rs), [numbered migrations](../migrations/) | [Operations](operations.md) restore, upgrade, rollback and schema compatibility |
| Backup and encryption | [Backup implementation](../src/backup/mod.rs), [settings encryption](../src/settings/encryption.rs) | [Operations](operations.md) snapshot layout, key handling, permissions, validation and recovery |
| Reader formats | [Reader](../src/reader/mod.rs), [archive decoder](../src/reader/archive.rs), [PDF decoder](../src/reader/pdf.rs) | [Format validation](formats.md) supported formats, limits, fixture evidence and external tools; [README](../README.md) reader claims |
| Providers and download clients | [Providers](../src/providers/mod.rs), [clients](../src/clients/mod.rs) | [Integration status](integrations.md) implemented behavior versus remaining evidence; [README](../README.md) acquisition claims |
| Rust build and web assets | [Cargo manifest](../Cargo.toml), [toolchain](../rust-toolchain.toml), [build script](../build.rs), [embedded assets](../src/webassets.rs), [web package](../web/package.json), [Vite configuration](../web/vite.config.ts), [Dockerfile](../Dockerfile), [Compose](../compose.yaml) | [README](../README.md) build/check and development instructions; [operations](operations.md) container tools, runtime and deployment requirements |
| Web behavior and localization | [Web sources](../web/src/), [English](../web/src/locales/en.ts), [Spanish](../web/src/locales/es.ts) | [README](../README.md) user-visible behavior; [formats](formats.md) and [integrations](integrations.md) when UI claims affect those areas |

The map is a review aid, not a second API inventory. Keep `api/openapi.yaml` as the API contract and regenerate `web/src/lib/api/schema.d.ts` through `scripts/generate-api`; do not hand-edit generated types. Keep English and Spanish locale key sets equal.

## Change-review obligations

- For each affected area, update the relevant documentation in the PR or give a specific no-impact explanation tied to the current source changes. Name the changed paths, the documentation assessed, and why its instructions or claims remain valid. A generic “docs not needed” is insufficient.
- A documentation-update assessment must cite documentation actually changed in the PR. Reassess after source changes; an old assessment is not evidence for a new diff.
- Review the full PR-base diff. For a release, review the combined release range so a later query-only change does not hide earlier migration or backup-format obligations.
- Check relative links and compare documented configuration names, defaults and CLI examples with their sources. Distinguish fixture-tested behavior, live-tested integrations, unsupported behavior and future work; do not imply that local tests prove production transfers.
- Keep changes bounded. This map does not prove prose correctness or a successful live upgrade or restore.

## Migration and backup compatibility

Never edit published migrations; add numbered migrations. Changes to schema, backup format or encryption require actionable notes in [operations](operations.md), not only a no-impact label. State:

- Which source schema or backup versions are accepted, which are rejected, and any breaking change or compatibility limit.
- How to create and verify a pre-upgrade backup, including the saved encryption key and separate media backup where needed.
- The upgrade steps and expected migration behavior, with validation on disposable state.
- How to roll back using the retained executable/image and a verified backup restored into a fresh directory. Do not imply an automatic downgrade or that an older executable can open a newer schema.

Backup sources and published snapshots stay read-only; migrate only disposable validation or restored copies. Preserve original state during drills. There are no tagged releases or promised backward schema compatibility yet; record current behavior and limits without inventing historical upgrade guarantees.

## Existing checks and future work

[`scripts/check`](../scripts/check) is the existing offline acceptance gate after dependencies are installed. It checks Rust formatting, Clippy and tests, compares regenerated API types with the committed types, checks and builds the frontend, and tests/builds embedded assets. It does **not** currently enforce this source-impact map, documentation assessments or local Markdown links.

Automated source-impact checks, documented-configuration/link validation, and focused failure fixtures for stale assessments and missing migration guidance remain future work under #48. Any future gate should reuse the existing contract and checks, inspect the full PR/release range, include new source files, and fail clearly when required tooling is missing. Local hooks would be supplements, not substitutes for required CI.

This documentation increment does not complete #48 or replace final-revision checks, independent review, or owner approval.

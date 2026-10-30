# Contributing to Foldout

Foldout is an unreleased development build. Start with the [local setup](README.md#run-locally), [product behavior](README.md), [operations](docs/operations.md), [formats](docs/formats.md), and [integration status](docs/integrations.md). Use disposable state and fixtures; keep original media read-only and never exercise real download clients during development.

## Changes and checks

Branch from `develop` and open a focused pull request back to `develop`. Keep application behavior and appearance specific to Foldout. Use conventional commits such as `fix(reader): preserve the current page`.

After installing the pinned dependencies described in the README, run:

```sh
./scripts/check
```

This is the same offline gate used by CI. It first tests and checks documented version facts, then checks Rust formatting, Clippy and tests with and without the embedded interface, generated API types, TypeScript, the interface build, and the release executable. Change `api/openapi.yaml` and regenerate with `./scripts/generate-api`; do not hand-edit generated types. Run UI changes in both themes and interface languages, at desktop and mobile widths, with keyboard focus visible. Report what was exercised and any unverified behavior.

## Documentation impact

Use the [documentation routing map](docs/MAINTENANCE.md) to review affected guidance. For each change, update the affected guidance or explain specifically why documented behavior stays the same. Review the whole pull request, including earlier changes to migrations or persistent formats. SQLite migrations and backup or encryption format changes need compatibility, upgrade, backup, and rollback instructions plus appropriate isolated validation. Never edit a published migration.

Keep the README useful for the first run: current development status, prerequisites, setup, and the next useful action. Put detailed configuration and recovery guidance in [operations](docs/operations.md), supported file behavior in [formats](docs/formats.md), and actual integration evidence and limitations in [integration status](docs/integrations.md). Describe permissions, effects on original files, confirmation steps, failures, and recovery where they affect the user's decision. Distinguish configured or planned behavior from tested behavior; a successful build does not establish live integration or recovery qualification.

The version-facts gate in `tools/check-docs-facts.ts` checks the documented package/image and Rust, Bun, Alpine and 7-Zip versions against their source pins. Run `./scripts/bun test tools/check-docs-facts.test.ts` and `./scripts/bun tools/check-docs-facts.ts` for a focused check. The offline routing and link checker validates paths, links, and selected source claims; the diff-based documentation-impact requirement remains pending in [#48](https://github.com/iuliandita/foldout/issues/48); version facts do not assess behavior, compatibility or recovery impact.

Report bugs and feature requests through the [issue forms](https://github.com/iuliandita/foldout/issues/new/choose). Include identifiable development versions and redacted diagnostics; report vulnerabilities [privately](SECURITY.md). Contributions use the repository's [MIT license](LICENSE); preserve fixture and third-party notices.

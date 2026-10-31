# Contributing to Foldout

Foldout is an unreleased development build. Start with the [local setup](README.md#run-locally), [product behavior](README.md), [operations](docs/operations.md), [formats](docs/formats.md), and [integration status](docs/integrations.md). Use disposable state and fixtures; keep original media read-only and never exercise real download clients during development.

## Changes and checks

Branch from `develop` and open a focused pull request back to `develop`. Keep application behavior and appearance specific to Foldout. Use conventional commits such as `fix(reader): preserve the current page`.

After installing the pinned dependencies described in the README, run:

```sh
./scripts/check
```

This is the same offline gate used by CI. It first tests and checks documented version facts, then checks Rust formatting, Clippy and tests with and without the embedded interface, generated API types, TypeScript, the interface build, and the release executable. Change `api/openapi.yaml` and regenerate with `./scripts/generate-api`; do not hand-edit generated types. Run UI changes in both themes and interface languages, at desktop and mobile widths, with keyboard focus visible. Report what was exercised and any unverified behavior.

The opt-in [soak protocol](docs/soak.md) exercises owned loopback fixtures against
a disposable debug service. Its classification test runs in `scripts/check`; the
180-second smoke and multi-day profile are explicit operator commands, not
long-running CI jobs. A fixture pass does not establish household acceptance.

## Documentation impact

Use the [documentation routing map](docs/MAINTENANCE.md) to review affected guidance. For each change, update the affected guidance or explain specifically why documented behavior stays the same. Review the whole pull request, including earlier changes to migrations or persistent formats. SQLite migrations and backup or encryption format changes need compatibility, upgrade, backup, and rollback instructions plus appropriate isolated validation. Never edit a published migration.

Keep the README useful for the first run: current development status, prerequisites, setup, and the next useful action. Put detailed configuration and recovery guidance in [operations](docs/operations.md), supported file behavior in [formats](docs/formats.md), and actual integration evidence and limitations in [integration status](docs/integrations.md). Describe permissions, effects on original files, confirmation steps, failures, and recovery where they affect the user's decision. Distinguish configured or planned behavior from tested behavior; a successful build does not establish live integration or recovery qualification.

The version-facts gate in `tools/check-docs-facts.ts` checks the documented package/image and Rust, Bun, Alpine and 7-Zip versions against their source pins. Run `./scripts/bun test tools/check-docs-facts.test.ts` and `./scripts/bun tools/check-docs-facts.ts` for a focused check. The offline routing and link checker validates paths, links, and selected source claims. The pull-request impact check compares the whole base-to-head range. Run `./scripts/bun tools/check-docs-impact.ts --base origin/develop` locally to include staged, unstaged, and untracked changes. With an explicit `--head`, add `--include-worktree` to include local changes when that head is `HEAD`. Pass `--body-file path` or set `DOCS_IMPACT_BODY` to provide the pull request description. For each affected area without a changed review document, add a line `Docs impact: <Area>: none - <specific reason>` with a reason of at least 20 characters. New migrations and changes to backup or encryption sources always require an update to `docs/operations.md`. These checks do not establish prose accuracy, compatibility, or recovery impact.

Report bugs and feature requests through the [issue forms](https://github.com/iuliandita/foldout/issues/new/choose). Include identifiable development versions and redacted diagnostics; report vulnerabilities [privately](SECURITY.md). Contributions use the repository's [MIT license](LICENSE); preserve fixture and third-party notices.

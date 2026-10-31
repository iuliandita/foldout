import { expect, test } from "bun:test";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { checkDocsMap } from "./check-docs-map";

const paths = ["README.md", "CONTRIBUTING.md", "SECURITY.md", "LICENSE", "docs", "src", "migrations", "api", "web/src", "scripts", "Cargo.toml", "Dockerfile", "build.rs", "tests/soak_report_test.py", "tests/fixtures/LICENSE.txt", "tests/fixtures/rar/LICENSE", "tests/fixtures/manifest.json"];

test.each([
  { name: "valid map and links", file: "", before: "", after: "", expected: "" },
  { name: "missing mapped path", file: "docs/MAINTENANCE.md", before: "`src/config.rs`", after: "`src/missing.rs`", expected: "Missing map path" },
  { name: "missing review doc", file: "docs/MAINTENANCE.md", before: "`docs/formats.md`", after: "`docs/missing.md`", expected: "Missing map path" },
  { name: "path escape", file: "docs/MAINTENANCE.md", before: "`src/config.rs`", after: "`../../etc/passwd`", expected: "outside repository" },
  { name: "missing required area", file: "docs/MAINTENANCE.md", before: "| Reader |", after: "| Other |", expected: "missing area Reader" },
  { name: "missing link target", file: "README.md", before: "docs/operations.md#container", after: "docs/missing.md", expected: "broken link" },
  { name: "broken anchor", file: "README.md", before: "docs/operations.md#container", after: "docs/operations.md#missing", expected: "broken anchor" },
  { name: "malformed map", file: "docs/MAINTENANCE.md", before: "| CLI |", after: "| CLI | extra |", expected: "Malformed map" },
  { name: "stale claim", file: "src/config.rs", before: '"LIBRARY_WORKERS"', after: '"REMOVED_WORKERS"', expected: "Missing environment claim" },
])("$name", ({ name, file, before, after, expected }) => {
  const root = mkdtempSync(resolve(tmpdir(), "foldout-doc-map-"));
  try {
    for (const path of paths) {
      const destination = resolve(root, path);
      mkdirSync(dirname(destination), { recursive: true });
      cpSync(resolve(import.meta.dir, "..", path), destination, { recursive: true, filter: source => !source.includes("/docs/local") });
    }
    const readme = resolve(root, "README.md");
    writeFileSync(readme, readFileSync(readme, "utf8") + "\n## [Linked heading](https://example.com)\n\n`[ignored](nope.md)` [heading](#linked-heading) [root](/docs/operations.md?view=1#container) [source](api/openapi.yaml?view=1#L10)\n");
    if (file) {
      const path = resolve(root, file);
      const original = readFileSync(path, "utf8");
      expect(original).toContain(before);
      writeFileSync(path, original.replaceAll(before, after));
    }
    const errors = checkDocsMap(root);
    if (expected) expect(errors.some(error => error.includes(expected))).toBe(true);
    else expect(errors).toEqual([]);
    if (name === "path escape") {
      writeFileSync(readme, readFileSync(readme, "utf8") + "\n[escape](../../etc/passwd)\n");
      expect(checkDocsMap(root).some(error => error.includes("link outside repository"))).toBe(true);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

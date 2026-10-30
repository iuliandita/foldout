import { expect, test } from "bun:test";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { checkDocsMap } from "./check-docs-map";

const paths = ["README.md", "CONTRIBUTING.md", "SECURITY.md", "LICENSE", "docs", "src", "migrations", "api", "web/src", "scripts", "Cargo.toml", "Dockerfile", "build.rs", "tests/fixtures/LICENSE.txt", "tests/fixtures/rar/LICENSE", "tests/fixtures/manifest.json"];

test.each([
  { name: "valid map and links", file: "", before: "", after: "", expected: "" },
  { name: "missing mapped path", file: "docs/MAINTENANCE.md", before: "`src/config.rs`", after: "`src/missing.rs`", expected: "Missing map path" },
  { name: "missing link target", file: "README.md", before: "docs/operations.md#container", after: "docs/missing.md", expected: "broken link" },
  { name: "broken link", file: "README.md", before: "docs/operations.md#container", after: "docs/operations.md#missing", expected: "broken anchor" },
  { name: "malformed map", file: "docs/MAINTENANCE.md", before: "| CLI |", after: "| CLI | extra |", expected: "Malformed map" },
  { name: "stale claim", file: "src/config.rs", before: '"LIBRARY_WORKERS"', after: '"REMOVED_WORKERS"', expected: "Missing environment claim" },
])("$name", ({ file, before, after, expected }) => {
  const root = mkdtempSync(resolve(tmpdir(), "foldout-doc-map-"));
  try {
    for (const path of paths) {
      const destination = resolve(root, path);
      mkdirSync(dirname(destination), { recursive: true });
      cpSync(resolve(import.meta.dir, "..", path), destination, { recursive: true, filter: source => !source.includes("/docs/local") });
    }
    if (file) {
      const path = resolve(root, file);
      const original = readFileSync(path, "utf8");
      expect(original).toContain(before);
      writeFileSync(path, original.replaceAll(before, after));
    }
    const errors = checkDocsMap(root);
    if (expected) expect(errors.some(error => error.includes(expected))).toBe(true);
    else expect(errors).toEqual([]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

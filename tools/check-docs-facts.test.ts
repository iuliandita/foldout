import { expect, test } from "bun:test";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { checkDocsFacts } from "./check-docs-facts";

const files = ["Cargo.toml", "Dockerfile", "compose.yaml", "rust-toolchain.toml", "tools/package.json", "docs/operations.md", "docs/formats.md", ".github/workflows/ci.yml"];

const bunVersion = JSON.parse(readFileSync(resolve(import.meta.dir, "package.json"), "utf8")).dependencies.bun as string;
const [major, minor, patch] = bunVersion.split(".").map(Number);
const differentBunVersion = `${major}.${minor}.${patch + 1}`;

test.each([
  { name: "aligned facts", file: "", before: "", after: "", expected: "" },
  { name: "package changed without docs", file: "Cargo.toml", before: 'version = "0.1.0"', after: 'version = "0.2.0"', expected: "documented image" },
  { name: "CI tool pin differs", file: ".github/workflows/ci.yml", before: `bun-version: ${bunVersion}`, after: `bun-version: ${differentBunVersion}`, expected: "CI Bun pin" },
])("$name", ({ file, before, after, expected }) => {
  const root = mkdtempSync(resolve(tmpdir(), "foldout-doc-facts-"));
  try {
    for (const path of files) {
      const destination = resolve(root, path);
      mkdirSync(dirname(destination), { recursive: true });
      copyFileSync(resolve(import.meta.dir, "..", path), destination);
    }
    if (file) {
      const path = resolve(root, file);
      const original = readFileSync(path, "utf8");
      expect(original).toContain(before);
      writeFileSync(path, original.replace(before, after));
    }
    const errors = checkDocsFacts(root);
    if (expected) expect(errors.some(error => error.startsWith(expected))).toBe(true);
    else expect(errors).toEqual([]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

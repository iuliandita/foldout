import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { checkDocsImpact } from "./check-docs-impact";

test.each([
  { name: "unmapped files are ignored", steps: [["other.txt"]], body: "", expected: "" },
  { name: "mapped source requires review", steps: [["src/config.rs"]], body: "", expected: "Configuration" },
  { name: "changed review document satisfies area", steps: [["src/config.rs", "README.md"]], body: "", expected: "" },
  { name: "specific explanation satisfies area", steps: [["src/config.rs"]], body: "Docs impact: Configuration: none - The timeout retains its documented default.", expected: "" },
  { name: "generic explanation fails", steps: [["src/config.rs"]], body: "Docs impact: Configuration: none - no impact internal documentation changes", expected: "Configuration" },
  { name: "short explanation fails", steps: [["src/config.rs"]], body: "Docs impact: Configuration: none - unchanged", expected: "Configuration" },
  { name: "new migration cannot be waived", steps: [["migrations/002.sql"]], body: "Docs impact: Database: none - This migration adds a harmless index.", expected: "docs/operations.md" },
  { name: "backup changes cannot be waived", steps: [["src/backup/archive.rs"]], body: "Docs impact: Backup and encryption: none - This only changes the archive logging.", expected: "docs/operations.md" },
  { name: "encryption changes cannot be waived", steps: [["src/settings/encryption.rs"]], body: "Docs impact: Backup and encryption: none - This only changes the encryption logging.", expected: "docs/operations.md" },
  { name: "operations document satisfies migration", steps: [["migrations/002.sql", "docs/operations.md"]], body: "", expected: "" },
  { name: "entire range includes earlier source and later docs", steps: [["src/config.rs"], ["README.md"]], body: "", expected: "" },
  { name: "missing base fails", steps: [["other.txt"]], body: "", expected: "base commit unavailable", missingBase: true },
  { name: "rename routes old and new paths", steps: [["rename"]], body: "", expected: "Configuration" },
])("$name", ({ steps, body, expected, missingBase }) => {
  const root = mkdtempSync(resolve(tmpdir(), "foldout-doc-impact-"));
  const git = (...args: string[]) => execFileSync("git", args, { cwd: root, stdio: "pipe" }).toString().trim();
  const write = (path: string, content: string) => {
    mkdirSync(dirname(resolve(root, path)), { recursive: true });
    writeFileSync(resolve(root, path), content);
  };
  try {
    git("init", "-q");
    git("config", "user.name", "Test");
    git("config", "user.email", "test@example.com");
    mkdirSync(resolve(root, "docs"));
    cpSync(resolve(import.meta.dir, "../docs/MAINTENANCE.md"), resolve(root, "docs/MAINTENANCE.md"));
    write("src/config.rs", "configuration contents\n");
    write("README.md", "readme\n");
    git("add", ".");
    git("commit", "-qm", "base");
    const base = git("rev-parse", "HEAD");
    for (const [index, files] of steps.entries()) {
      for (const file of files) {
        if (file === "rename") git("mv", "src/config.rs", "other-config.rs");
        else write(file, `change ${index}\n`);
      }
      git("add", "-A");
      git("commit", "-qm", `change ${index}`);
    }
    const errors = checkDocsImpact(root, missingBase ? "missing-ref" : base, "HEAD", body);
    if (expected) expect(errors.some(error => error.includes(expected))).toBe(true);
    else expect(errors).toEqual([]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

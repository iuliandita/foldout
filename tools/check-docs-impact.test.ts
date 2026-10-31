import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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
  { name: "entire range retains earlier source without docs", steps: [["src/config.rs"], ["other.txt"]], body: "", expected: "Configuration" },
  { name: "unrelated review document does not satisfy area", steps: [["src/config.rs", "docs/formats.md"]], body: "", expected: "Configuration" },
  { name: "whitespace-only review edit does not satisfy area", steps: [["src/config.rs", "whitespace"]], body: "", expected: "Configuration" },
  { name: "bulleted explanation satisfies area", steps: [["src/config.rs"]], body: "  - Docs impact: Configuration: none - The timeout retains its documented default.", expected: "" },
  { name: "asterisk explanation satisfies area", steps: [["src/config.rs"]], body: "* Docs impact: Configuration: none - The timeout retains its documented default.", expected: "" },
  { name: "quoted explanation satisfies area", steps: [["src/config.rs"]], body: "> Docs impact: Configuration: none - The timeout retains its documented default.", expected: "" },
  { name: "commented explanation is ignored", steps: [["src/config.rs"]], body: "<!--\nDocs impact: Configuration: none - The timeout retains its documented default.\n-->", expected: "Configuration" },
  { name: "whitespace-only operations edit cannot waive migration", steps: [["migrations/002.sql", "operations-whitespace"]], body: "Docs impact: Database: none - This migration adds a harmless index.", expected: "docs/operations.md" },
  { name: "base routing table cannot be narrowed", steps: [["src/config.rs", "narrow-map"]], body: "", expected: "Configuration" },
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
    write("docs/operations.md", "operations\n");
    git("add", ".");
    git("commit", "-qm", "base");
    const base = git("rev-parse", "HEAD");
    for (const [index, files] of steps.entries()) {
      for (const file of files) {
        if (file === "rename") git("mv", "src/config.rs", "other-config.rs");
        else if (file === "whitespace") write("README.md", "  readme  \n\n");
        else if (file === "operations-whitespace") write("docs/operations.md", "  operations  \n\n");
        else if (file === "narrow-map") {
          const path = resolve(root, "docs/MAINTENANCE.md");
          writeFileSync(path, readFileSync(path, "utf8").replace("`src/config.rs`, ", ""));
        }
        else write(file, `change ${index}\n`);
      }
      git("add", "-A");
      git("commit", "-qm", `change ${index}`);
    }
    const errors = checkDocsImpact(root, missingBase ? "missing-ref" : base, "HEAD", body);
    if (expected) {
      expect(errors.some(error => error.includes(expected))).toBe(true);
      if (expected === "Configuration") {
        expect(errors.join("\n")).toContain("src/config.rs");
        expect(errors.join("\n")).toContain("Docs impact: Configuration: none - <specific reason>");
      }
    }
    else expect(errors).toEqual([]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("falls back to head map when base has no routing table", () => {
  const root = mkdtempSync(resolve(tmpdir(), "foldout-doc-base-map-"));
  const git = (...args: string[]) => execFileSync("git", args, { cwd: root, stdio: "pipe" }).toString().trim();
  try {
    git("init", "-q");
    git("config", "user.name", "Test");
    git("config", "user.email", "test@example.com");
    writeFileSync(resolve(root, "initial"), "base\n");
    git("add", ".");
    git("commit", "-qm", "base");
    const base = git("rev-parse", "HEAD");
    mkdirSync(resolve(root, "docs"));
    mkdirSync(resolve(root, "src"));
    cpSync(resolve(import.meta.dir, "../docs/MAINTENANCE.md"), resolve(root, "docs/MAINTENANCE.md"));
    writeFileSync(resolve(root, "src/config.rs"), "changed\n");
    git("add", ".");
    git("commit", "-qm", "head");
    expect(checkDocsImpact(root, base, "HEAD", "").join("\n")).toContain("Configuration");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test.each(["staged", "unstaged", "untracked"])("includes %s changes in local mode", kind => {
  const root = mkdtempSync(resolve(tmpdir(), "foldout-doc-worktree-"));
  const git = (...args: string[]) => execFileSync("git", args, { cwd: root, stdio: "pipe" }).toString().trim();
  try {
    git("init", "-q");
    git("config", "user.name", "Test");
    git("config", "user.email", "test@example.com");
    mkdirSync(resolve(root, "docs"));
    mkdirSync(resolve(root, "src"));
    cpSync(resolve(import.meta.dir, "../docs/MAINTENANCE.md"), resolve(root, "docs/MAINTENANCE.md"));
    writeFileSync(resolve(root, "src/config.rs"), "original\n");
    git("add", ".");
    git("commit", "-qm", "base");
    const base = git("rev-parse", "HEAD");
    const path = kind === "untracked" ? "src/store/new.rs" : "src/config.rs";
    mkdirSync(dirname(resolve(root, path)), { recursive: true });
    writeFileSync(resolve(root, path), "changed\n");
    if (kind === "staged") git("add", path);
    expect(checkDocsImpact(root, base, "HEAD", "", true).join("\n")).toContain(path);
    expect(checkDocsImpact(root, base, "HEAD", "")).toEqual([]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

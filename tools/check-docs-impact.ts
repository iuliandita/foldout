import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { parseDocsMap } from "./check-docs-map";

type Change = { status: string; paths: string[] };

export function checkDocsImpact(root: string, base: string, head: string, body: string): string[] {
  const git = (...args: string[]) => spawnSync("git", args, { cwd: root, encoding: "utf8" });
  const baseCommit = base && git("rev-parse", "--verify", "--quiet", "--end-of-options", `${base}^{commit}`);
  if (!baseCommit || baseCommit.status !== 0) return ["base commit unavailable"];
  const headCommit = head && git("rev-parse", "--verify", "--quiet", "--end-of-options", `${head}^{commit}`);
  if (!headCommit || headCommit.status !== 0) return ["head commit unavailable"];
  const diff = git("diff", "--name-status", "-z", "--find-renames", `${baseCommit.stdout.trim()}...${headCommit.stdout.trim()}`, "--");
  if (diff.status !== 0) return [`Unable to compare commits: ${diff.stderr.trim()}`];
  const fields = diff.stdout.split("\0");
  const changes: Change[] = [];
  for (let i = 0; i < fields.length - 1;) {
    const status = fields[i++];
    const paths = [fields[i++]];
    if (status.startsWith("R") || status.startsWith("C")) paths.push(fields[i++]);
    changes.push({ status, paths });
  }
  const changed = new Set(changes.flatMap(change => change.paths));
  const matches = (path: string, source: string) => path === source || path.startsWith(`${source}/`);
  const map = parseDocsMap(readFileSync(resolve(root, "docs/MAINTENANCE.md"), "utf8"));
  if (map.errors.length) return map.errors;
  const errors: string[] = [];
  const generic = new Set(["a", "an", "and", "are", "as", "at", "be", "by", "change", "changes", "doc", "docs", "documentation", "for", "impact", "in", "internal", "is", "n", "no", "none", "not", "of", "or", "only", "the", "to", "update", "updates", "with"]);
  const explanation = (area: string) => body.split(/\r?\n/).some(line => {
    const prefix = `Docs impact: ${area}: none - `;
    if (!line.startsWith(prefix)) return false;
    const reason = line.slice(prefix.length).trim();
    const words = reason.toLowerCase().match(/[a-z0-9]+/g) ?? [];
    return reason.length >= 20 && words.some(word => !generic.has(word));
  });
  for (const area of map.areas) {
    if (!area.sources.some(source => [...changed].some(path => matches(path, source)))) continue;
    if (!area.docs.some(doc => changed.has(doc)) && !explanation(area.name)) {
      errors.push(`${area.name}: change a review document or add a specific Docs impact explanation`);
    }
  }
  const migrationAdded = changes.some(change => (change.status === "A" || change.status.startsWith("R")) && change.paths.at(-1)?.startsWith("migrations/"));
  const backupChanged = [...changed].some(path => matches(path, "src/backup") || matches(path, "src/settings/encryption.rs"));
  if ((migrationAdded || backupChanged) && !changed.has("docs/operations.md")) {
    errors.push("New migrations and backup or encryption changes require docs/operations.md changes");
  }
  return errors;
}

if (import.meta.main) {
  try {
    let base = "";
    let head = "HEAD";
    let body = process.env.DOCS_IMPACT_BODY ?? "";
    const args = process.argv.slice(2);
    for (let i = 0; i < args.length; i++) {
      const option = args[i];
      const value = args[++i];
      if (!value || value.startsWith("--")) throw new Error(`Missing value for ${option}`);
      if (option === "--base") base = value;
      else if (option === "--head") head = value;
      else if (option === "--body-file") body = readFileSync(value, "utf8");
      else throw new Error(`Unknown option: ${option}`);
    }
    const errors = checkDocsImpact(resolve(import.meta.dir, ".."), base, head, body);
    if (errors.length) {
      console.error(errors.join("\n"));
      process.exitCode = 1;
    } else console.log("Documentation impact requirements are met.");
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}

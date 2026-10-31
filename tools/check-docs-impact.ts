import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { parseDocsMap } from "./check-docs-map";

type Change = { status: string; paths: string[] };

export function checkDocsImpact(root: string, base: string, head: string, body: string, includeWorktree = false): string[] {
  const git = (...args: string[]) => spawnSync("git", args, { cwd: root, encoding: "utf8" });
  const baseCommit = base && git("rev-parse", "--verify", "--quiet", "--end-of-options", `${base}^{commit}`);
  if (!baseCommit || baseCommit.status !== 0) return ["base commit unavailable"];
  const headCommit = head && git("rev-parse", "--verify", "--quiet", "--end-of-options", `${head}^{commit}`);
  if (!headCommit || headCommit.status !== 0) return ["head commit unavailable"];
  const baseSha = baseCommit.stdout.trim();
  const headSha = headCommit.stdout.trim();
  if (includeWorktree && git("rev-parse", "HEAD").stdout.trim() !== headSha) return ["worktree requires head to be HEAD"];
  const mergeBase = git("merge-base", baseSha, headSha);
  if (mergeBase.status !== 0) return [`Unable to find merge base: ${mergeBase.stderr.trim()}`];
  const diffArgs = includeWorktree ? [mergeBase.stdout.trim()] : [mergeBase.stdout.trim(), headSha];
  const diff = git("diff", "--name-status", "-z", "--find-renames", ...diffArgs, "--");
  if (diff.status !== 0) return [`Unable to compare commits: ${diff.stderr.trim()}`];
  const fields = diff.stdout.split("\0");
  const changes: Change[] = [];
  for (let i = 0; i < fields.length - 1;) {
    const status = fields[i++];
    const paths = [fields[i++]];
    if (status.startsWith("R") || status.startsWith("C")) paths.push(fields[i++]);
    changes.push({ status, paths });
  }
  const untracked = new Set<string>();
  if (includeWorktree) {
    const files = git("ls-files", "--others", "--exclude-standard", "-z");
    if (files.status !== 0) return [`Unable to list untracked files: ${files.stderr.trim()}`];
    for (const path of files.stdout.split("\0").filter(Boolean)) {
      untracked.add(path);
      changes.push({ status: "A", paths: [path] });
    }
  }
  const changed = new Set(changes.flatMap(change => change.paths));
  const matches = (path: string, source: string) => path === source || path.startsWith(`${source}/`);
  const mapPath = "docs/MAINTENANCE.md";
  let mapContent = git("show", `${baseSha}:${mapPath}`);
  const baseMapExists = git("cat-file", "-e", `${baseSha}:${mapPath}`);
  if (baseMapExists.status !== 0 || (mapContent.status === 0 && parseDocsMap(mapContent.stdout).errors.includes("Malformed map: missing routing table"))) {
    console.warn("Base has no documentation routing table; using the head table.");
    mapContent = git("show", `${headSha}:${mapPath}`);
  }
  if (mapContent.status !== 0) return [`Unable to read documentation routing table: ${mapContent.stderr.trim()}`];
  const map = parseDocsMap(mapContent.stdout);
  if (map.errors.length) return map.errors;
  const errors: string[] = [];
  const addedContent = (doc: string): boolean => {
    if (untracked.has(doc)) return readFileSync(resolve(root, doc), "utf8").split(/\r?\n/).some(line => line.trim());
    const patch = git("diff", "-w", "--ignore-blank-lines", "--no-ext-diff", "--no-textconv", "--unified=0", ...diffArgs, "--", `:(literal)${doc}`);
    if (patch.status !== 0) throw new Error(`Unable to inspect ${doc}: ${patch.stderr.trim()}`);
    return patch.stdout.split("\n").some(line => line.startsWith("+") && !line.startsWith("+++ ") && line.slice(1).trim());
  };
  const generic = new Set(["a", "an", "and", "are", "as", "at", "be", "by", "change", "changes", "doc", "docs", "documentation", "for", "impact", "in", "internal", "is", "n", "no", "none", "not", "of", "or", "only", "the", "to", "update", "updates", "with"]);
  const visibleBody = body.replace(/<!--[\s\S]*?(?:-->|$)/g, "");
  const explanation = (area: string) => visibleBody.split(/\r?\n/).some(line => {
    const prefix = `Docs impact: ${area}: none - `;
    const text = line.replace(/^\s*(?:[-*>]\s+)?/, "");
    if (!text.startsWith(prefix)) return false;
    const reason = text.slice(prefix.length).trim();
    const words = reason.toLowerCase().match(/[a-z0-9]+/g) ?? [];
    return reason.length >= 20 && words.some(word => !generic.has(word));
  });
  for (const area of map.areas) {
    const sources = [...changed].filter(path => area.sources.some(source => matches(path, source)));
    if (!sources.length) continue;
    if (!area.docs.some(addedContent) && !explanation(area.name)) {
      errors.push(`${area.name}: ${sources.join(", ")}: add content to a review document or use: Docs impact: ${area.name}: none - <specific reason>`);
    }
  }
  const mandatory = changes.filter(change => ((change.status === "A" || change.status.startsWith("R")) && change.paths.at(-1)?.startsWith("migrations/")) || change.paths.some(path => matches(path, "src/backup") || matches(path, "src/settings/encryption.rs")));
  if (mandatory.length && !addedContent("docs/operations.md")) {
    const paths = [...new Set(mandatory.flatMap(change => change.paths))];
    const names = map.areas.filter(area => paths.some(path => area.sources.some(source => matches(path, source)))).map(area => area.name);
    const formats = names.map(name => `Docs impact: ${name}: none - <specific reason>`).join("; ");
    errors.push(`${names.join(", ")}: ${paths.join(", ")}: add content to docs/operations.md; ${formats} cannot waive this requirement`);
  }
  return errors;
}

if (import.meta.main) {
  try {
    let base = "";
    let head = "HEAD";
    let explicitHead = false;
    let includeWorktree = false;
    let body = process.env.DOCS_IMPACT_BODY ?? "";
    const args = process.argv.slice(2);
    for (let i = 0; i < args.length; i++) {
      const option = args[i];
      if (option === "--include-worktree") { includeWorktree = true; continue; }
      const value = args[++i];
      if (!value || value.startsWith("--")) throw new Error(`Missing value for ${option}`);
      if (option === "--base") base = value;
      else if (option === "--head") { head = value; explicitHead = true; }
      else if (option === "--body-file") body = readFileSync(value, "utf8");
      else throw new Error(`Unknown option: ${option}`);
    }
    const errors = checkDocsImpact(resolve(import.meta.dir, ".."), base, head, body, includeWorktree || !explicitHead);
    if (errors.length) {
      console.error(errors.join("\n"));
      process.exitCode = 1;
    } else console.log("Documentation impact requirements are met.");
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}

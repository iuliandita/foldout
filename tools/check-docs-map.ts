import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, resolve } from "node:path";

export function checkDocsMap(root: string): string[] {
  const errors: string[] = [];
  const read = (path: string) => readFileSync(resolve(root, path), "utf8");
  const map = read("docs/MAINTENANCE.md");
  const table = map.match(/\| Area \| Sources \| Review docs \|\n\| --- \| --- \| --- \|\n((?:\|[^\n]*\|\n)+)/)?.[1];
  if (!table) errors.push("Malformed map: missing routing table");
  else {
    const areas = new Set<string>();
    for (const row of table.trimEnd().split("\n")) {
      const cells = row.split("|").slice(1, -1).map(cell => cell.trim());
      if (cells.length !== 3 || !cells[0] || areas.has(cells[0])) {
        errors.push(`Malformed map row: ${row}`);
        continue;
      }
      areas.add(cells[0]);
      for (const cell of cells.slice(1)) {
        if (!/^`[^`]+`(?:, `[^`]+`)*$/.test(cell)) {
          errors.push(`Malformed map paths: ${row}`);
          continue;
        }
        for (const match of cell.matchAll(/`([^`]+)`/g)) {
          if (!existsSync(resolve(root, match[1]))) errors.push(`Missing map path: ${match[1]}`);
        }
      }
    }
    for (const area of ["Configuration", "CLI", "API", "Database", "Backup and encryption", "Reader", "Integrations", "Build and web assets"]) {
      if (!areas.has(area)) errors.push(`Malformed map: missing area ${area}`);
    }
  }
  for (const [label, kind] of [["Environment", "environment"], ["Commands", "commands"]]) {
    const pattern = new RegExp("^- " + label + " in `([^`]+)`, documented in `([^`]+)`: (.+)\\.$", "m");
    const claim = map.match(pattern);
    if (!claim || !/^`[^`]+`(?:, `[^`]+`)*$/.test(claim[3])) {
      errors.push(`Malformed map: ${label} claims`);
      continue;
    }
    const [, source, doc, values] = claim;
    if (!existsSync(resolve(root, source)) || !existsSync(resolve(root, doc))) {
      errors.push(`Missing claim path: ${source} or ${doc}`);
      continue;
    }
    for (const match of values.matchAll(/`([^`]+)`/g)) {
      const value = match[1];
      if (!read(source).includes(`"${value}"`) || !read(doc).includes(value)) errors.push(`Missing ${kind} claim: ${value}`);
    }
  }
  const docs = ["README.md", "CONTRIBUTING.md", ...readdirSync(resolve(root, "docs")).filter(name => name.endsWith(".md")).map(name => `docs/${name}`)];
  for (const doc of docs) {
    const content = read(doc).replace(/^\s*(```|~~~)[^\n]*\n[\s\S]*?^\s*\1[^\n]*$/gm, "");
    for (const link of content.matchAll(/(?<!!)\[[^\]]*\]\(([^\s)]+)(?:\s+"[^"]*")?\)/g)) {
      const target = link[1];
      if (/^(?:[a-z][a-z\d+.-]*:|\/\/)/i.test(target)) continue;
      try {
        const [path, fragment] = target.split("#");
        const file = path ? resolve(root, dirname(doc), decodeURIComponent(path)) : resolve(root, doc);
        if (!existsSync(file)) { errors.push(`${doc}: broken link ${target}`); continue; }
        if (fragment) {
          if (!statSync(file).isFile()) { errors.push(`${doc}: broken anchor ${target}`); continue; }
          const headings = readFileSync(file, "utf8").replace(/^\s*(```|~~~)[^\n]*\n[\s\S]*?^\s*\1[^\n]*$/gm, "");
          const counts = new Map<string, number>();
          const anchors = [...headings.matchAll(/^#{1,6}\s+(.+?)\s*#*\s*$/gm)].map(match => {
            const slug = match[1].toLowerCase().replace(/<[^>]*>/g, "").replace(/[^\p{L}\p{N}_\-\s]/gu, "").replace(/\s/g, "-");
            const count = counts.get(slug) ?? 0;
            counts.set(slug, count + 1);
            return count ? `${slug}-${count}` : slug;
          });
          if (!anchors.includes(decodeURIComponent(fragment))) errors.push(`${doc}: broken anchor ${target}`);
        }
      } catch {
        errors.push(`${doc}: malformed link ${target}`);
      }
    }
  }
  return errors;
}

if (import.meta.main) {
  try {
    const errors = checkDocsMap(resolve(import.meta.dir, ".."));
    if (errors.length) {
      console.error(errors.join("\n"));
      process.exitCode = 1;
    } else console.log("Documentation map and local links are valid.");
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}

import { existsSync, readFileSync, readdirSync, realpathSync, statSync } from "node:fs";
import { dirname, extname, isAbsolute, relative, resolve, sep } from "node:path";

export function checkDocsMap(root: string): string[] {
  const errors: string[] = [];
  root = realpathSync(root);
  const withinRoot = (path: string): boolean => {
    const relativePath = relative(root, path);
    return relativePath !== ".." && !relativePath.startsWith(`..${sep}`) && !isAbsolute(relativePath);
  };
  const safePath = (base: string, path: string): string | undefined => {
    const file = resolve(base, path);
    if (!withinRoot(file)) return undefined;
    // Resolve existing ancestors too, so symlinks cannot escape through a missing target.
    let ancestor = file;
    while (!existsSync(ancestor)) ancestor = dirname(ancestor);
    return withinRoot(realpathSync(ancestor)) ? file : undefined;
  };
  const mapPath = (path: string) => isAbsolute(path) ? undefined : safePath(root, path);
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
          const file = mapPath(match[1]);
          if (!file) errors.push(`Map path outside repository: ${match[1]}`);
          else if (!existsSync(file)) errors.push(`Missing map path: ${match[1]}`);
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
    const sourcePath = mapPath(source);
    const docPath = mapPath(doc);
    if (!sourcePath || !docPath) {
      errors.push(`Claim path outside repository: ${source} or ${doc}`);
      continue;
    }
    if (!existsSync(sourcePath) || !existsSync(docPath)) {
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
    const content = read(doc).replace(/^\s*(```|~~~)[^\n]*\n[\s\S]*?^\s*\1[^\n]*$/gm, "").replace(/(`+)([\s\S]*?)\1/g, "");
    for (const link of content.matchAll(/(?<!!)\[[^\]]*\]\(([^\s)]+)(?:\s+"[^"]*")?\)/g)) {
      const target = link[1];
      if (/^(?:[a-z][a-z\d+.-]*:|\/\/)/i.test(target)) continue;
      try {
        const [address, fragment] = target.split("#");
        const path = decodeURIComponent(address.split("?")[0]);
        const file = path ? safePath(path.startsWith("/") ? root : resolve(root, dirname(doc)), path.replace(/^\/+/, "")) : safePath(root, doc);
        if (!file) { errors.push(`${doc}: link outside repository ${target}`); continue; }
        if (!existsSync(file)) { errors.push(`${doc}: broken link ${target}`); continue; }
        if (fragment && extname(file).toLowerCase() === ".md") {
          if (!statSync(file).isFile()) { errors.push(`${doc}: broken anchor ${target}`); continue; }
          const headings = readFileSync(file, "utf8").replace(/^\s*(```|~~~)[^\n]*\n[\s\S]*?^\s*\1[^\n]*$/gm, "");
          const counts = new Map<string, number>();
          const anchors = [...headings.matchAll(/^#{1,6}\s+(.+?)\s*#*\s*$/gm)].map(match => {
            const slug = match[1].replace(/\[([^\]]+)\]\([^)]*\)/g, "$1").toLowerCase().replace(/<[^>]*>/g, "").replace(/[^\p{L}\p{N}_\-\s]/gu, "").replace(/\s/g, "-");
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

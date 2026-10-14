import { readFileSync } from "node:fs";
import { resolve } from "node:path";

export function checkDocsFacts(root: string): string[] {
  const read = (path: string) => readFileSync(resolve(root, path), "utf8");
  const errors: string[] = [];
  const capture = (text: string, pattern: RegExp, name: string): string => {
    const value = text.match(pattern)?.[1];
    if (!value) throw new Error(`Missing ${name}`);
    return value;
  };
  const expect = (text: string, claim: string, name: string) => {
    if (!text.includes(claim)) errors.push(`${name}: expected ${claim}`);
  };
  const cargo = read("Cargo.toml");
  const docker = read("Dockerfile");
  const operations = read("docs/operations.md");
  const ci = read(".github/workflows/ci.yml");
  const version = capture(cargo, /^version = "([^"]+)"/m, "Cargo version");
  const rust = capture(read("rust-toolchain.toml"), /channel = "([^"]+)"/, "Rust toolchain");
  const builder = capture(docker, /FROM rust:([\d.]+)-alpine/, "default Rust builder");
  const builderAlpine = capture(docker, /FROM rust:[\d.]+-alpine([\d.]+)/, "builder Alpine");
  const alpine = capture(docker, /^FROM alpine:([\d.]+)[^\n]*$/m, "runtime Alpine");
  const bun = JSON.parse(read("tools/package.json")).dependencies.bun as string;
  if (!/^\d+\.\d+\.\d+$/.test(bun)) throw new Error("Bun must be exactly pinned");
  const archive = capture(docker, /7zip\/releases\/download\/([\d.]+)\//, "7-Zip pin");
  expect(read("compose.yaml"), `image: libraryd:${version}\n`, "Compose image");
  expect(operations, `local image is \`libraryd:${version}\``, "documented image");
  expect(operations, `-t libraryd:${version}-debian .`, "documented Debian image");
  expect(operations, `local toolchain pins Rust ${rust}`, "documented local Rust");
  expect(operations, `builder pins Rust\n${builder} on Alpine ${builderAlpine}`, "documented default builder");
  expect(operations, `runtime pins Alpine ${alpine}`, "documented runtime");
  expect(operations, `pins Bun ${bun}`, "documented Bun");
  expect(operations, `pins the Rust ${rust} trixie builder`, "documented Debian builder");
  expect(docker, `FROM rust:${rust}-trixie@`, "Debian Rust pin");
  expect(docker, `FROM oven/bun:${bun}@`, "container Bun pin");
  expect(ci, `bun-version: ${bun}\n`, "CI Bun pin");
  expect(operations, `upstream 7-Zip ${archive}`, "documented 7-Zip");
  expect(read("docs/formats.md"), `installed ${archive} build`, "format 7-Zip");
  expect(ci, `7zip/releases/download/${archive}/`, "CI 7-Zip pin");
  // The URL and archive name must select the same upstream release.
  const archiveCode = archive.replaceAll(".", "");
  expect(docker, `/7z${archiveCode}-linux-`, "container archive name");
  expect(ci, `/7z${archiveCode}-linux-`, "CI archive name");
  const alpinePins = [...docker.matchAll(/^FROM alpine:([\d.]+)/gm)].map(match => match[1]);
  if (alpinePins.some(pin => pin !== alpine)) errors.push("Alpine archive/runtime pins differ");
  return errors;
}

if (import.meta.main) {
  try {
    const errors = checkDocsFacts(resolve(import.meta.dir, ".."));
    if (errors.length) {
      console.error(errors.join("\n"));
      process.exitCode = 1;
    } else {
      console.log("Documented version facts match source pins.");
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}

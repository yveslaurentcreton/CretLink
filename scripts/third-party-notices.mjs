// Writes the third-party notices of one program, or of the programs of one
// release archive (ST-M007-015, ST-M006-019, REQ-N-016): the packages of other
// authors that are linked into them, their licences, the licence texts those
// packages ship, and where their source is.
//
// A released program carries the code of its dependencies, so it carries
// their notices too. The list is what a build of the program links in:
// `cargo tree --edges normal --locked` resolves features the way the build
// does, so what only tests or build scripts need is left out, and so is
// everything below a procedural macro, which runs in the compiler and is not
// part of the program. The application also carries its frontend, so the npm
// packages that the frontend is bundled from are listed with it.
//
// Usage: node scripts/third-party-notices.mjs --package <name>...
//          [--manifest-path <Cargo.toml>]... [--npm <folder>]...
//          [--target <triple>] [--output <file>]
//
//   --package <name>        a package whose program this is about; once per
//                           program of an archive
//   --manifest-path <path>  a Cargo.toml to resolve from (default: the
//                           workspace of the current directory); once per
//                           workspace. A package is resolved from the
//                           workspace it is a member of, which is the
//                           lockfile its program is built with
//   --npm <folder>          an npm package whose installed production
//                           dependencies are part of a program; they must be
//                           installed as its lockfile says (`npm ci`)
//   --target <triple>       only the dependencies of this target platform
//   --output <file>         write there instead of to standard output
//
// Exit status: 0 when written, 1 when a package is unknown, a licence file
// cannot be read or the npm packages are not installed as locked, 2 after a
// usage error or when cargo cannot be run.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const USAGE =
  "Usage: node scripts/third-party-notices.mjs --package <name>... [--manifest-path <Cargo.toml>]... [--npm <folder>]... [--target <triple>] [--output <file>]";

/** Files of a package that hold a licence text or a notice. */
const NOTICE_FILE = /^(licen[cs]e|copying|notice|unlicense|copyright)([-._].*)?$/i;

/** The largest licence file that is taken over, in bytes. */
export const MAX_NOTICE_FILE_BYTES = 512 * 1024;

/** Licences whose terms ask to say where the source is found. */
const SOURCE_ON_REQUEST = /\bMPL-2\.0\b/;

/**
 * Reads the output of `cargo tree --prefix depth --no-dedupe --format "{p}"`
 * and returns the packages that are linked into the program at its root, as
 * `"name version"` keys. A procedural macro and everything below it is left
 * out; a package that is also reached another way stays in.
 */
export function linkedFromTree(tree) {
  const linked = new Set();
  // While inside the subtree of a procedural macro: the depth of that macro.
  let macroDepth = null;
  for (const line of tree.split(/\r?\n/)) {
    const match = /^(\d+)(\S+) v(\S+)(.*)$/.exec(line);
    if (!match) continue;
    const depth = Number(match[1]);
    if (macroDepth !== null && depth > macroDepth) continue;
    macroDepth = null;
    if (/\(proc-macro\)/.test(match[4])) {
      macroDepth = depth;
      continue;
    }
    linked.add(`${match[2]} ${match[3]}`);
  }
  return linked;
}

/**
 * The packages of other authors among `linked` (the keys of
 * [`linkedFromTree`]), with what `cargo metadata` knows about them. The
 * packages of this repository are left out, because the program's own licence
 * covers them: the members of the workspace, and the packages that come from
 * a path and not from a registry, as the application reaches the crates of
 * the workspace.
 */
export function linkedPackages(metadata, linked) {
  const members = new Set(metadata.workspace_members);
  const selected = metadata.packages.filter(
    (pkg) => linked.has(`${pkg.name} ${pkg.version}`) && !members.has(pkg.id) && pkg.source,
  );
  const known = new Set(metadata.packages.map((pkg) => `${pkg.name} ${pkg.version}`));
  const unknown = [...linked].filter((key) => !known.has(key));
  if (unknown.length > 0) {
    throw new Error(`cargo metadata does not know: ${unknown.join(", ")}`);
  }
  return selected.sort(
    (a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version),
  );
}

/**
 * The names of the programs that the workspace member `name` builds: its
 * binaries, or its own name when it has none. `null` when the workspace has
 * no such member.
 */
export function programsOf(metadata, name) {
  const members = new Set(metadata.workspace_members);
  const pkg = metadata.packages.find((candidate) => candidate.name === name && members.has(candidate.id));
  if (!pkg) return null;
  const binaries = (pkg.targets ?? []).filter((target) => target.kind.includes("bin"));
  return binaries.length > 0 ? binaries.map((target) => target.name) : [name];
}

/**
 * The production dependencies of the npm package in `folder`, as its
 * `package-lock.json` (`lock`) pins them, in the shape of the packages of
 * `cargo metadata`. Each one must be installed in the locked version, because
 * its licence files are read from there; what only builds or tests the
 * package is left out, and so is an optional package that is not installed
 * on this platform.
 */
export function npmPackages(folder, lock) {
  const packages = [];
  for (const [path, entry] of Object.entries(lock.packages ?? {})) {
    if (path === "" || entry.dev || entry.devOptional || entry.link) continue;
    const manifestPath = join(folder, path, "package.json");
    if (!existsSync(manifestPath)) {
      if (entry.optional) continue;
      throw new Error(`${join(folder, path)} is not installed; run \`npm ci\` in ${folder} first`);
    }
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    if (manifest.version !== entry.version) {
      throw new Error(
        `${join(folder, path)} is version ${manifest.version}, the lockfile says ${entry.version}; run \`npm ci\` in ${folder} first`,
      );
    }
    packages.push({
      name: manifest.name,
      version: manifest.version,
      license: typeof manifest.license === "string" ? manifest.license : entry.license,
      repository: typeof manifest.repository === "string" ? manifest.repository : manifest.repository?.url,
      source: "npm",
      manifest_path: manifestPath,
    });
  }
  return packages;
}

/**
 * One list of several lists of packages: a package that two programs share
 * appears once, and the list is sorted by name and version.
 */
export function mergePackages(lists) {
  const merged = new Map();
  for (const pkg of lists.flat()) {
    const key = `${pkg.source === "npm" ? "npm" : "cargo"} ${pkg.name} ${pkg.version}`;
    if (!merged.has(key)) merged.set(key, pkg);
  }
  return [...merged.values()].sort(
    (a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version),
  );
}

/** The licence and notice files in the folder of a package, sorted by name. */
export function noticeFiles(folder) {
  return readdirSync(folder, { withFileTypes: true })
    .filter((entry) => entry.isFile() && NOTICE_FILE.test(entry.name))
    .map((entry) => entry.name)
    .sort((a, b) => a.localeCompare(b));
}

/** One text, read with a bound and with its line ends made uniform. */
function readNotice(path) {
  const size = statSync(path).size;
  if (size > MAX_NOTICE_FILE_BYTES) {
    throw new Error(`${path} is larger than ${MAX_NOTICE_FILE_BYTES} bytes`);
  }
  return readFileSync(path, "utf8").replace(/\r\n?/g, "\n").trim();
}

/**
 * Reads the licence files of every package. Returns, per package, the files
 * with their text: `[{ pkg, files: [{ name, text }] }]`.
 */
export function collectNotices(packages) {
  return packages.map((pkg) => {
    const folder = dirname(pkg.manifest_path);
    const files = noticeFiles(folder)
      .map((name) => ({ name, text: readNotice(join(folder, name)) }))
      .filter((file) => file.text !== "");
    return { pkg, files };
  });
}

/**
 * Puts identical texts together, so that a licence that many packages ship
 * word for word appears once. Returns the groups in the order of their first
 * package: `[{ text, users: ["name version (FILE)", ...] }]`.
 */
export function groupTexts(collected) {
  const groups = new Map();
  for (const { pkg, files } of collected) {
    for (const file of files) {
      const key = createHash("sha256").update(file.text).digest("hex");
      if (!groups.has(key)) groups.set(key, { text: file.text, users: [] });
      groups.get(key).users.push(`${pkg.name} ${pkg.version} (${file.name})`);
    }
  }
  return [...groups.values()];
}

/** Where the source of a package is found. */
function sourceOf(pkg) {
  let registry = pkg.source ?? "this repository";
  if (pkg.source === "npm") registry = `https://www.npmjs.com/package/${pkg.name}/v/${pkg.version}`;
  else if (pkg.source?.startsWith("registry+")) registry = `https://crates.io/crates/${pkg.name}/${pkg.version}`;
  return pkg.repository ? `${registry} (${pkg.repository})` : registry;
}

/**
 * The whole notices file as text, for one program (its name) or for the
 * programs of one archive (their names).
 */
export function renderNotices(program, collected) {
  const programs = [program].flat();
  const lines = [];
  const rule = "=".repeat(78);
  lines.push("THIRD-PARTY NOTICES", "");
  if (programs.length === 1) {
    lines.push(
      `${programs[0]} contains the packages listed below, which other authors wrote and`,
      "published under their own licences. This file lists every such package with",
      "its licence, repeats the licence texts and notices that the packages ship,",
      "and says where the source of each package is found.",
      "",
      `${programs[0]} itself is published under the MIT licence; see the file LICENSE.`,
    );
  } else {
    lines.push(
      `The programs ${programs.slice(0, -1).join(", ")} and ${programs.at(-1)} contain the packages listed`,
      "below, which other authors wrote and published under their own licences. A",
      "package is part of at least one of the programs, not always of each. This",
      "file lists every such package with its licence, repeats the licence texts",
      "and notices that the packages ship, and says where the source of each",
      "package is found.",
      "",
      "The programs themselves are published under the MIT licence; see the file",
      "LICENSE.",
    );
  }
  lines.push(
    "",
    rule,
    `PACKAGES (${collected.length})`,
    rule,
    "",
  );
  for (const { pkg } of collected) {
    lines.push(`${pkg.name} ${pkg.version}`);
    lines.push(`    licence: ${pkg.license ?? (pkg.license_file ? `see ${pkg.license_file}` : "not declared")}`);
    lines.push(`    source:  ${sourceOf(pkg)}`);
  }

  const withSource = collected.filter(({ pkg }) => SOURCE_ON_REQUEST.test(pkg.license ?? ""));
  if (withSource.length > 0) {
    lines.push(
      "",
      rule,
      "SOURCE CODE UNDER THE MOZILLA PUBLIC LICENSE 2.0",
      rule,
      "",
      "The files of these packages are covered by the Mozilla Public License 2.0",
      "and are used without changes. Their source code form is available at the",
      "address named with each package:",
      "",
    );
    for (const { pkg } of withSource) {
      lines.push(`${pkg.name} ${pkg.version}: ${sourceOf(pkg)}`);
    }
  }

  const without = collected.filter(({ files }) => files.length === 0);
  if (without.length > 0) {
    lines.push(
      "",
      rule,
      "PACKAGES THAT SHIP NO LICENCE FILE",
      rule,
      "",
      "These packages declare their licence without shipping its text. The",
      "licence named with each of them above applies; its text is found in the",
      "source of the package.",
      "",
    );
    for (const { pkg } of without) {
      lines.push(`${pkg.name} ${pkg.version}: ${pkg.license ?? "not declared"}`);
    }
  }

  lines.push("", rule, "LICENCE TEXTS AND NOTICES", rule);
  for (const group of groupTexts(collected)) {
    lines.push("", "-".repeat(78), "Shipped with:");
    for (const user of group.users) lines.push(`    ${user}`);
    lines.push("-".repeat(78), "", group.text);
  }
  return `${lines.join("\n")}\n`;
}

/** Runs cargo and returns what it prints. */
function cargo(args) {
  return execFileSync("cargo", args, {
    encoding: "utf8",
    maxBuffer: 256 * 1024 * 1024,
    stdio: ["ignore", "pipe", "inherit"],
  });
}

/** The packages a build of `name` links in, from the locked graph. */
function cargoTree(name, manifestPath, target) {
  const args = ["tree", "--package", name, "--edges", "normal", "--locked"];
  args.push("--prefix", "depth", "--no-dedupe", "--format", "{p}");
  if (manifestPath) args.push("--manifest-path", manifestPath);
  if (target) args.push("--target", target);
  return cargo(args);
}

/** What cargo knows about every package of the locked graph. */
function cargoMetadata(manifestPath) {
  const args = ["metadata", "--format-version", "1", "--locked"];
  if (manifestPath) args.push("--manifest-path", manifestPath);
  return JSON.parse(cargo(args));
}

function main() {
  let options;
  try {
    options = parseArgs({
      options: {
        package: { type: "string", multiple: true },
        "manifest-path": { type: "string", multiple: true },
        npm: { type: "string", multiple: true },
        target: { type: "string" },
        output: { type: "string" },
      },
    }).values;
  } catch (error) {
    console.error(`third-party-notices: ${error.message}\n${USAGE}`);
    return 2;
  }
  if (!options.package) {
    console.error(`third-party-notices: --package is required\n${USAGE}`);
    return 2;
  }

  // Each workspace with what cargo knows about its locked graph.
  let workspaces;
  try {
    workspaces = (options["manifest-path"] ?? [undefined]).map((manifestPath) => ({
      manifestPath,
      metadata: cargoMetadata(manifestPath),
    }));
  } catch (error) {
    console.error(`third-party-notices: cargo could not be run: ${error.message}`);
    return 2;
  }

  const programs = [];
  const lists = [];
  try {
    for (const name of options.package) {
      const owners = workspaces.filter(({ metadata }) => programsOf(metadata, name) !== null);
      if (owners.length !== 1) {
        throw new Error(`${name} is a member of ${owners.length} of the workspaces, not of one`);
      }
      const [{ manifestPath, metadata }] = owners;
      let tree;
      try {
        tree = cargoTree(name, manifestPath, options.target);
      } catch (error) {
        console.error(`third-party-notices: cargo could not be run: ${error.message}`);
        return 2;
      }
      const linked = linkedFromTree(tree);
      // The tree starts with the package itself; without it, nothing was read.
      if (linked.size === 0) throw new Error(`cargo tree lists no package for ${name}`);
      programs.push(...programsOf(metadata, name));
      lists.push(linkedPackages(metadata, linked));
    }
    for (const folder of options.npm ?? []) {
      lists.push(npmPackages(folder, JSON.parse(readFileSync(join(folder, "package-lock.json"), "utf8"))));
    }

    const collected = collectNotices(mergePackages(lists));
    const text = renderNotices(programs, collected);
    if (options.output) writeFileSync(options.output, text);
    else process.stdout.write(text);
    const shipped = collected.filter(({ files }) => files.length > 0).length;
    console.error(
      `third-party-notices: ${collected.length} packages in ${programs.join(", ")}, ${shipped} with a licence file`,
    );
    return 0;
  } catch (error) {
    console.error(`third-party-notices: ${error.message}`);
    return 1;
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  process.exitCode = main();
}

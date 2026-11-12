// Writes the third-party notices of one program (ST-M007-015, ST-M006-019,
// REQ-N-016): the packages of other authors that are linked into it, their
// licences, the licence texts those packages ship, and where their source is.
//
// A released program carries the code of its dependencies, so it carries
// their notices too. The list is what a build of the program links in:
// `cargo tree --edges normal --locked` resolves features the way the build
// does, so what only tests or build scripts need is left out, and so is
// everything below a procedural macro, which runs in the compiler and is not
// part of the program.
//
// Usage: node scripts/third-party-notices.mjs --package <name>
//          [--manifest-path <Cargo.toml>] [--target <triple>] [--output <file>]
//
//   --package <name>        the workspace package whose program this is about
//   --manifest-path <path>  the Cargo.toml to resolve from (default: the
//                           workspace of the current directory)
//   --target <triple>       only the dependencies of this target platform
//   --output <file>         write there instead of to standard output
//
// Exit status: 0 when written, 1 when the package is unknown or a licence
// file cannot be read, 2 after a usage error or when cargo cannot be run.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const USAGE =
  "Usage: node scripts/third-party-notices.mjs --package <name> [--manifest-path <Cargo.toml>] [--target <triple>] [--output <file>]";

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
  for (const line of tree.split("\n")) {
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
 * packages of the workspace itself are left out: the program's own licence
 * covers them.
 */
export function linkedPackages(metadata, linked) {
  const members = new Set(metadata.workspace_members);
  const selected = metadata.packages.filter(
    (pkg) => linked.has(`${pkg.name} ${pkg.version}`) && !members.has(pkg.id),
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
  const registry = pkg.source?.startsWith("registry+")
    ? `https://crates.io/crates/${pkg.name}/${pkg.version}`
    : (pkg.source ?? "this repository");
  return pkg.repository ? `${registry} (${pkg.repository})` : registry;
}

/** The whole notices file as text. */
export function renderNotices(program, collected) {
  const lines = [];
  const rule = "=".repeat(78);
  lines.push(
    "THIRD-PARTY NOTICES",
    "",
    `${program} contains the packages listed below, which other authors wrote and`,
    "published under their own licences. This file lists every such package with",
    "its licence, repeats the licence texts and notices that the packages ship,",
    "and says where the source of each package is found.",
    "",
    `${program} itself is published under the MIT licence; see the file LICENSE.`,
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
        package: { type: "string" },
        "manifest-path": { type: "string" },
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

  let metadata;
  let tree;
  try {
    tree = cargoTree(options.package, options["manifest-path"], options.target);
    metadata = cargoMetadata(options["manifest-path"]);
  } catch (error) {
    console.error(`third-party-notices: cargo could not be run: ${error.message}`);
    return 2;
  }

  try {
    const collected = collectNotices(linkedPackages(metadata, linkedFromTree(tree)));
    const text = renderNotices(options.package, collected);
    if (options.output) writeFileSync(options.output, text);
    else process.stdout.write(text);
    const shipped = collected.filter(({ files }) => files.length > 0).length;
    console.error(
      `third-party-notices: ${collected.length} packages in ${options.package}, ${shipped} with a licence file`,
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

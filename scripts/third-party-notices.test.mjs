// Tests of scripts/third-party-notices.mjs (ST-M007-015, ST-M006-019).
// They use made-up dependency data and a temporary folder; cargo is not run.

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import {
  MAX_NOTICE_FILE_BYTES,
  collectNotices,
  groupTexts,
  linkedFromTree,
  linkedPackages,
  mergePackages,
  noticeFiles,
  npmPackages,
  programsOf,
  renderNotices,
} from "./third-party-notices.mjs";

/** A folder that holds the files of one made-up package. */
function packageFolder(root, name, files) {
  const folder = join(root, name);
  mkdirSync(folder, { recursive: true });
  for (const [file, text] of Object.entries(files)) {
    writeFileSync(join(folder, file), text);
  }
  return folder;
}

function pkg(folder, name, version, license, extra = {}) {
  return {
    id: `registry+https://example.org/index#${name}@${version}`,
    name,
    version,
    license,
    repository: `https://example.org/${name}`,
    source: "registry+https://example.org/index",
    manifest_path: join(folder, "Cargo.toml"),
    ...extra,
  };
}

test("the linked packages are read from the tree of a build", () => {
  const tree = [
    "0relay-server v0.0.0 (/work/apps/relay-server)",
    "1relay v0.0.0 (/work/crates/relay)",
    "2thiserror v2.0.19",
    "3thiserror-impl v2.0.19 (proc-macro)",
    "4proc-macro2 v1.0.106",
    "5unicode-ident v1.0.24",
    "4quote v1.0.45",
    "2tokio v1.54.1",
    "3pin-project-lite v0.2.17",
    "1tokio v1.54.1",
    "2pin-project-lite v0.2.17",
    "",
  ].join("\n");
  assert.deepEqual([...linkedFromTree(tree)].sort(), [
    "pin-project-lite 0.2.17",
    "relay 0.0.0",
    "relay-server 0.0.0",
    "thiserror 2.0.19",
    "tokio 1.54.1",
  ]);
});

test("a package below a procedural macro stays when it is also linked in directly", () => {
  const tree = [
    "0program v1.0.0 (/work)",
    "1derive-thing v1.0.0 (proc-macro)",
    "2shared v1.2.3",
    "2only-for-the-macro v0.1.0",
    "1shared v1.2.3",
    // A macro at the same depth as the one before it ends that subtree.
    "1other-derive v2.0.0 (proc-macro)",
    "2only-for-the-macro v0.1.0",
    "1plain v3.0.0",
  ].join("\n");
  assert.deepEqual([...linkedFromTree(tree)].sort(), [
    "plain 3.0.0",
    "program 1.0.0",
    "shared 1.2.3",
  ]);
});

test("the packages of the workspace itself are left out, and the rest is sorted", () => {
  const metadata = {
    workspace_members: ["path+file:///work#program@1.0.0"],
    packages: [
      { id: "path+file:///work#program@1.0.0", name: "program", version: "1.0.0" },
      pkg("/x/zeta", "zeta", "1.0.0", "MIT"),
      pkg("/x/alpha2", "alpha", "2.0.0", "MIT"),
      pkg("/x/alpha1", "alpha", "1.0.0", "MIT"),
      pkg("/x/unused", "unused", "9.9.9", "MIT"),
    ],
  };
  const linked = new Set(["program 1.0.0", "zeta 1.0.0", "alpha 2.0.0", "alpha 1.0.0"]);
  assert.deepEqual(
    linkedPackages(metadata, linked).map((p) => `${p.name} ${p.version}`),
    ["alpha 1.0.0", "alpha 2.0.0", "zeta 1.0.0"],
  );
});

test("the tree is read the same with the line ends of Windows", () => {
  const tree = "0program v1.0.0 (C:\\work)\r\n1plain v3.0.0\r\n1derive v1.0.0 (proc-macro)\r\n2inner v0.1.0\r\n";
  assert.deepEqual([...linkedFromTree(tree)].sort(), ["plain 3.0.0", "program 1.0.0"]);
});

test("a package that comes from a path is left out like a member of the workspace", () => {
  // The application is a workspace of its own and reaches the crates of the
  // repository by path: they are no members there, and still CretLink's own.
  const metadata = {
    workspace_members: ["path+file:///work/app#app@0.0.0"],
    packages: [
      { id: "path+file:///work/app#app@0.0.0", name: "app", version: "0.0.0", source: null },
      { id: "path+file:///work/crates/own#own@0.0.0", name: "own", version: "0.0.0", source: null },
      pkg("/x/other", "other", "1.0.0", "MIT"),
    ],
  };
  const linked = new Set(["app 0.0.0", "own 0.0.0", "other 1.0.0"]);
  assert.deepEqual(
    linkedPackages(metadata, linked).map((p) => `${p.name} ${p.version}`),
    ["other 1.0.0"],
  );
});

test("a program is named after the binaries of its package", () => {
  const metadata = {
    workspace_members: ["path+file:///work/app#app@0.0.0", "path+file:///work/lib#lib@0.0.0"],
    packages: [
      {
        id: "path+file:///work/app#app@0.0.0",
        name: "app",
        targets: [
          { kind: ["lib"], name: "app_lib" },
          { kind: ["bin"], name: "the-program" },
        ],
      },
      { id: "path+file:///work/lib#lib@0.0.0", name: "lib", targets: [{ kind: ["lib"], name: "lib" }] },
      // Reached by path from this workspace, a member of another one.
      { id: "path+file:///work/cli#cli@0.0.0", name: "cli", targets: [{ kind: ["bin"], name: "cli" }] },
    ],
  };
  assert.deepEqual(programsOf(metadata, "app"), ["the-program"]);
  assert.deepEqual(programsOf(metadata, "lib"), ["lib"]);
  assert.equal(programsOf(metadata, "cli"), null);
  assert.equal(programsOf(metadata, "ghost"), null);
});

/** An installed npm package in `folder`/node_modules. */
function installed(folder, name, manifest, files = {}) {
  return packageFolder(join(folder, "node_modules"), name, {
    "package.json": JSON.stringify({ name, ...manifest }),
    ...files,
  });
}

test("the npm packages are the installed production dependencies of the lockfile", () => {
  const root = mkdtempSync(join(tmpdir(), "cretlink-notices-"));
  try {
    installed(root, "@scope/ui", { version: "5.5.0", license: "MIT", repository: "https://example.org/ui" }, { LICENSE: "MIT, by UI" });
    installed(root, "plain", { version: "1.2.3", license: "ISC", repository: { type: "git", url: "git+https://example.org/plain.git" } });
    installed(root, "builder", { version: "9.0.0", license: "MIT" });
    const lock = {
      packages: {
        "": { name: "frontend", dependencies: { "@scope/ui": "^5.5.0" } },
        "node_modules/@scope/ui": { version: "5.5.0", license: "MIT" },
        "node_modules/plain": { version: "1.2.3" },
        // What only builds or tests the package is no part of the program.
        "node_modules/builder": { version: "9.0.0", dev: true },
        "node_modules/builder-helper": { version: "1.0.0", devOptional: true },
        // An optional package for another platform is not installed here.
        "node_modules/for-another-platform": { version: "2.0.0", optional: true },
      },
    };
    const packages = npmPackages(root, lock);
    assert.deepEqual(
      packages.map(({ name, version, license, repository, source }) => ({ name, version, license, repository, source })),
      [
        { name: "@scope/ui", version: "5.5.0", license: "MIT", repository: "https://example.org/ui", source: "npm" },
        { name: "plain", version: "1.2.3", license: "ISC", repository: "git+https://example.org/plain.git", source: "npm" },
      ],
    );
    // The licence files are read from the installed package.
    assert.deepEqual(
      collectNotices(packages).map(({ files }) => files),
      [[{ name: "LICENSE", text: "MIT, by UI" }], []],
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("npm packages that are not installed as the lockfile says stop the run", () => {
  const root = mkdtempSync(join(tmpdir(), "cretlink-notices-"));
  try {
    installed(root, "drifted", { version: "1.0.1", license: "MIT" });
    assert.throws(
      () => npmPackages(root, { packages: { "node_modules/missing": { version: "1.0.0" } } }),
      /missing is not installed; run `npm ci`/,
    );
    assert.throws(
      () => npmPackages(root, { packages: { "node_modules/drifted": { version: "1.0.0" } } }),
      /drifted is version 1\.0\.1, the lockfile says 1\.0\.0/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("the packages of several programs are listed once, sorted", () => {
  const shared = pkg("/x/shared", "shared", "1.2.3", "MIT");
  const merged = mergePackages([
    [pkg("/x/zeta", "zeta", "1.0.0", "MIT"), shared],
    // The same package from the lockfile of another workspace, and a newer one.
    [pkg("/y/shared", "shared", "1.2.3", "MIT"), pkg("/y/shared-2", "shared", "2.0.0", "MIT")],
    // An npm package with the name and version of a crate is another package.
    [{ name: "shared", version: "1.2.3", license: "ISC", source: "npm", manifest_path: "/n/shared/package.json" }],
  ]);
  assert.deepEqual(
    merged.map((p) => `${p.name} ${p.version} ${p.source === "npm" ? "npm" : "cargo"}`),
    ["shared 1.2.3 cargo", "shared 1.2.3 npm", "shared 2.0.0 cargo", "zeta 1.0.0 cargo"],
  );
  assert.equal(merged[0], shared);
});

test("a linked package that the metadata does not know stops the run", () => {
  const metadata = { workspace_members: [], packages: [pkg("/x/a", "a", "1.0.0", "MIT")] };
  assert.throws(
    () => linkedPackages(metadata, new Set(["a 1.0.0", "ghost 0.1.0"])),
    /does not know: ghost 0\.1\.0/,
  );
});

test("licence and notice files are found, other files are not", () => {
  const root = mkdtempSync(join(tmpdir(), "cretlink-notices-"));
  try {
    const folder = packageFolder(root, "pkg", {
      "LICENSE-MIT": "mit",
      "LICENSE-APACHE": "apache",
      "license.txt": "lower case",
      COPYING: "copying",
      "NOTICE.md": "notice",
      UNLICENSE: "unlicense",
      "README.md": "readme",
      "Cargo.toml": "[package]",
      "licenses.rs": "not a licence file",
    });
    mkdirSync(join(folder, "LICENSES"));
    assert.deepEqual(noticeFiles(folder), [
      "COPYING",
      "LICENSE-APACHE",
      "LICENSE-MIT",
      "license.txt",
      "NOTICE.md",
      "UNLICENSE",
    ]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("identical texts are listed once, with every package that ships them", () => {
  const root = mkdtempSync(join(tmpdir(), "cretlink-notices-"));
  try {
    const apache = "Apache License\nVersion 2.0\n";
    const a = packageFolder(root, "a", { "LICENSE-APACHE": apache, "LICENSE-MIT": "MIT, by A" });
    // Windows line ends and blank lines around the text do not make it another text.
    const b = packageFolder(root, "b", { LICENSE: `\r\n${apache.replace(/\n/g, "\r\n")}\r\n` });
    const c = packageFolder(root, "c", { "Cargo.toml": "[package]" });
    const collected = collectNotices([
      pkg(a, "a", "1.0.0", "MIT OR Apache-2.0"),
      pkg(b, "b", "2.0.0", "Apache-2.0"),
      pkg(c, "c", "3.0.0", "MIT"),
    ]);
    assert.deepEqual(
      collected.map(({ files }) => files.map((file) => file.name)),
      [["LICENSE-APACHE", "LICENSE-MIT"], ["LICENSE"], []],
    );
    const groups = groupTexts(collected);
    assert.deepEqual(
      groups.map((group) => group.users),
      [["a 1.0.0 (LICENSE-APACHE)", "b 2.0.0 (LICENSE)"], ["a 1.0.0 (LICENSE-MIT)"]],
    );
    assert.equal(groups[0].text, "Apache License\nVersion 2.0");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("a licence file beyond the bound stops the run instead of being cut", () => {
  const root = mkdtempSync(join(tmpdir(), "cretlink-notices-"));
  try {
    const big = packageFolder(root, "big", { LICENSE: "x".repeat(MAX_NOTICE_FILE_BYTES + 1) });
    assert.throws(() => collectNotices([pkg(big, "big", "1.0.0", "MIT")]), /is larger than/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("the notices name every package, its licence, its source and the texts", () => {
  const root = mkdtempSync(join(tmpdir(), "cretlink-notices-"));
  try {
    const a = packageFolder(root, "a", { LICENSE: "Permission is hereby granted, by A." });
    const m = packageFolder(root, "m", { LICENSE: "Mozilla Public License Version 2.0" });
    const n = packageFolder(root, "n", { "Cargo.toml": "[package]" });
    const text = renderNotices(
      "the-program",
      collectNotices([
        pkg(a, "a", "1.0.0", "MIT"),
        pkg(m, "m", "0.3.0", "MPL-2.0"),
        pkg(n, "n", "2.1.0", "Apache-2.0", { repository: undefined }),
      ]),
    );

    assert.match(text, /^THIRD-PARTY NOTICES\n/);
    assert.match(text, /the-program contains the packages listed below/);
    assert.match(text, /PACKAGES \(3\)/);
    assert.match(text, /a 1\.0\.0\n {4}licence: MIT\n {4}source: {2}https:\/\/crates\.io\/crates\/a\/1\.0\.0 \(https:\/\/example\.org\/a\)/);
    // A package without a repository still says where its source is.
    assert.match(text, /n 2\.1\.0\n {4}licence: Apache-2\.0\n {4}source: {2}https:\/\/crates\.io\/crates\/n\/2\.1\.0\n/);
    // The Mozilla Public License asks to say where the source is.
    assert.match(
      text,
      /SOURCE CODE UNDER THE MOZILLA PUBLIC LICENSE 2\.0[\s\S]*m 0\.3\.0: https:\/\/crates\.io\/crates\/m\/0\.3\.0/,
    );
    assert.doesNotMatch(text.split("LICENCE TEXTS AND NOTICES")[0].split("MOZILLA")[1], /^a 1\.0\.0:/m);
    // A package that ships no text is named with its declared licence.
    assert.match(text, /PACKAGES THAT SHIP NO LICENCE FILE[\s\S]*n 2\.1\.0: Apache-2\.0/);
    // The texts follow, each with the package it came with.
    assert.match(text, /Shipped with:\n {4}a 1\.0\.0 \(LICENSE\)\n-+\n\nPermission is hereby granted, by A\./);
    assert.ok(text.endsWith("Mozilla Public License Version 2.0\n"));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("the notices of an archive name its programs, and an npm package with its source", () => {
  const root = mkdtempSync(join(tmpdir(), "cretlink-notices-"));
  try {
    const a = packageFolder(root, "a", { LICENSE: "MIT text" });
    const ui = packageFolder(root, "ui", { LICENSE: "MIT, by UI" });
    const text = renderNotices(
      ["the-app", "the-cli"],
      collectNotices([
        { name: "@scope/ui", version: "5.5.0", license: "MIT", repository: "https://example.org/ui", source: "npm", manifest_path: join(ui, "package.json") },
        pkg(a, "a", "1.0.0", "MIT"),
      ]),
    );
    assert.match(text, /^THIRD-PARTY NOTICES\n\nThe programs the-app and the-cli contain the packages listed\n/);
    assert.match(text, /A\npackage is part of at least one of the programs, not always of each\./);
    assert.match(text, /The programs themselves are published under the MIT licence; see the file\nLICENSE\./);
    assert.match(
      text,
      /@scope\/ui 5\.5\.0\n {4}licence: MIT\n {4}source: {2}https:\/\/www\.npmjs\.com\/package\/@scope\/ui\/v\/5\.5\.0 \(https:\/\/example\.org\/ui\)/,
    );
    assert.match(text, /Shipped with:\n {4}@scope\/ui 5\.5\.0 \(LICENSE\)\n-+\n\nMIT, by UI/);
    // Three programs read as a list.
    assert.match(renderNotices(["x", "y", "z"], []), /The programs x, y and z contain the packages listed\n/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("without a package under the Mozilla Public License or without a text, those sections are absent", () => {
  const root = mkdtempSync(join(tmpdir(), "cretlink-notices-"));
  try {
    const a = packageFolder(root, "a", { LICENSE: "MIT text" });
    const text = renderNotices("the-program", collectNotices([pkg(a, "a", "1.0.0", "MIT")]));
    assert.doesNotMatch(text, /MOZILLA/);
    assert.doesNotMatch(text, /SHIP NO LICENCE FILE/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

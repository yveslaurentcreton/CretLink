// Tests of scripts/changed-areas.mjs (ST-M006-017). Run them from the
// repository root with: node --test "scripts/*.test.mjs"

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, before, describe, test } from "node:test";

import { changedAreas, changedPaths, classify } from "./changed-areas.mjs";

const SCRIPT = join(import.meta.dirname, "changed-areas.mjs");

describe("classify", () => {
  test("documentation alone touches no area", () => {
    for (const path of [
      "README.md",
      "CONTRIBUTING.md",
      "apps/app/README.md",
      "docs/threat-model.md",
      "docs/images/technician-session.png",
      "LICENSE",
      "crates/protocol/NOTES.MD",
    ]) {
      assert.deepEqual(classify([path]), { code: false, website: false }, path);
    }
    assert.deepEqual(classify([]), { code: false, website: false });
  });

  test("everything else outside the site is code", () => {
    for (const path of [
      "crates/session/src/lib.rs",
      "apps/app/src/technician/views/ai.ts",
      "Cargo.lock",
      ".github/workflows/ci.yml",
      "scripts/changed-areas.mjs",
      "deny.toml",
      "LICENSE.txt",
      "documents/readme.txt",
      "docs.rs",
    ]) {
      assert.deepEqual(classify([path]), { code: true, website: false }, path);
    }
  });

  test("a file of the site is website, whatever its type", () => {
    for (const path of [
      "website/package.json",
      "website/src/content/docs/index.mdx",
      "website/README.md",
      "website/src/components/Session.astro",
    ]) {
      assert.deepEqual(classify([path]), { code: false, website: true }, path);
    }
  });

  test("a mixed change touches every area it holds", () => {
    assert.deepEqual(classify(["README.md", "website/astro.config.mjs"]), {
      code: false,
      website: true,
    });
    assert.deepEqual(classify(["docs/releasing.md", "crates/relay/src/lib.rs"]), {
      code: true,
      website: false,
    });
    assert.deepEqual(classify(["website/package.json", "Cargo.toml", "README.md"]), {
      code: true,
      website: true,
    });
  });
});

describe("in a repository", () => {
  let repo;
  const git = (...args) =>
    execFileSync("git", args, { cwd: repo, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
  const write = (path, text) => {
    mkdirSync(dirname(join(repo, path)), { recursive: true });
    writeFileSync(join(repo, path), text);
  };
  const commit = (message, files) => {
    for (const [path, text] of Object.entries(files)) write(path, text);
    git("add", "-A");
    git("-c", "user.name=Sam", "-c", "user.email=sam@example.org", "commit", "-q", "-m", message);
    return git("rev-parse", "HEAD");
  };
  let first, docs, site, code, side;

  before(() => {
    repo = mkdtempSync(join(tmpdir(), "changed-areas-"));
    git("init", "-q", "-b", "main");
    first = commit("start", { "README.md": "one\n", "src/lib.rs": "// one\n" });
    docs = commit("documentation", { "README.md": "two\n", "docs/guide.md": "guide\n" });
    site = commit("site", { "website/src/content/docs/index.mdx": "# Site\n" });
    code = commit("code", { "src/lib.rs": "// two\n" });
    // A branch from the first commit, as a pull request that is behind main.
    git("checkout", "-q", "-b", "side", first);
    side = commit("side documentation", { "docs/other.md": "other\n" });
    git("checkout", "-q", "main");
  });

  after(() => rmSync(repo, { recursive: true, force: true }));

  test("lists the files of the commits between base and head", () => {
    assert.deepEqual(changedPaths(first, docs, repo).sort(), ["README.md", "docs/guide.md"]);
    assert.deepEqual(changedPaths(docs, site, repo), ["website/src/content/docs/index.mdx"]);
    assert.deepEqual(changedPaths(first, first, repo), []);
  });

  test("reports the areas of a change", () => {
    assert.deepEqual(changedAreas(first, docs, repo), { code: false, website: false });
    assert.deepEqual(changedAreas(docs, site, repo), { code: false, website: true });
    assert.deepEqual(changedAreas(site, code, repo), { code: true, website: false });
    assert.deepEqual(changedAreas(first, code, repo), { code: true, website: true });
  });

  test("ignores what the base branch gained since the pull request started", () => {
    // main moved on with site and code changes; the side branch only adds a document.
    assert.deepEqual(changedPaths(code, side, repo), ["docs/other.md"]);
    assert.deepEqual(changedAreas(code, side, repo), { code: false, website: false });
  });

  test("counts every area as changed when the change is unknown", () => {
    const all = { code: true, website: true };
    assert.deepEqual(changedAreas("0000000000000000000000000000000000000000", code, repo), all);
    assert.deepEqual(changedAreas("", code, repo), all);
    assert.deepEqual(changedAreas(undefined, code, repo), all);
    assert.deepEqual(changedAreas(first, undefined, repo), all);
    assert.deepEqual(changedAreas("1234567890abcdef1234567890abcdef12345678", code, repo), all);
    assert.deepEqual(changedAreas("main; echo oops", code, repo), all);
    assert.equal(changedPaths("--output=oops", code, repo), null);
  });

  test("prints one line per area for the workflow", () => {
    const run = (...args) =>
      execFileSync(process.execPath, [SCRIPT, ...args], { cwd: repo, encoding: "utf8" });
    assert.equal(run(first, docs), "code=false\nwebsite=false\n");
    assert.equal(run(docs, site), "code=false\nwebsite=true\n");
    assert.equal(run(site, code), "code=true\nwebsite=false\n");
    assert.equal(run(), "code=true\nwebsite=true\n");
  });
});

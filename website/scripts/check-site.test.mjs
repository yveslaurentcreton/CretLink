// Tests of scripts/check-site.mjs (ST-M011-001). Run them from the website
// folder with: node --test "scripts/*.test.mjs"

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, before, describe, test } from "node:test";

const SCRIPT = join(import.meta.dirname, "check-site.mjs");

const page = (body, head = "<title>Page</title>") =>
  `<!doctype html><html><head>${head}</head><body><h1 id="top">Page</h1>${body}</body></html>`;

describe("check-site", () => {
  let root;
  let count = 0;

  /** Builds a site from `files` and returns the result of the check on it. */
  const check = (files) => {
    const dist = join(root, `site-${(count += 1)}`);
    for (const [path, content] of Object.entries({ "index.html": page(""), ...files })) {
      mkdirSync(dirname(join(dist, path)), { recursive: true });
      writeFileSync(join(dist, path), content);
    }
    const run = spawnSync(process.execPath, [SCRIPT, dist], { encoding: "utf8" });
    return { status: run.status, output: run.stdout + run.stderr };
  };

  before(() => {
    root = mkdtempSync(join(tmpdir(), "check-site-"));
  });
  after(() => rmSync(root, { recursive: true, force: true }));

  test("passes a site whose links, anchors and pictures are in place", () => {
    const result = check({
      "guide/index.html": page(
        '<h2 id="part">Part</h2>' +
          '<a href="/CretLink/">start</a> <a href="/CretLink/guide/#part">part</a> <a href="#top">top</a> ' +
          '<a href="../">up</a> <a href="https://yveslaurentcreton.github.io/CretLink/guide/">spelled out</a> ' +
          '<img alt="A picture" src="/CretLink/_astro/picture.webp" srcset="/CretLink/_astro/picture.webp 640w, /CretLink/_astro/large.webp 1280w"> ' +
          '<img alt src="/CretLink/_astro/picture.webp"> ' +
          '<a href="https://github.com/yveslaurentcreton/CretLink/blob/main/README.md#license">the readme</a> ' +
          '<a href="https://example.org/elsewhere">elsewhere</a> <a href="mailto:ann@example.org">mail</a>'
      ),
      "_astro/picture.webp": "picture",
      "_astro/large.webp": "large",
    });
    assert.equal(result.status, 0, result.output);
    assert.match(result.output, /2 pages, 10 links and resources checked, 1 external links listed, 0 problems/);
    assert.match(result.output, /external, not fetched: https:\/\/example\.org\/elsewhere/);
  });

  test("reports a link to a page or a picture that does not exist", () => {
    const result = check({
      "guide/index.html": page('<a href="/CretLink/missing/">gone</a> <img alt="x" src="/CretLink/_astro/none.webp" srcset="/CretLink/_astro/none-2.webp 640w">'),
    });
    assert.equal(result.status, 1);
    assert.match(result.output, /guide\/index\.html: \/CretLink\/missing\/ points to nothing/);
    assert.match(result.output, /\/CretLink\/_astro\/none\.webp points to nothing/);
    assert.match(result.output, /\/CretLink\/_astro\/none-2\.webp points to nothing/);
    assert.match(result.output, /3 problems/);
  });

  test("reports an anchor that the target page does not have", () => {
    const result = check({ "guide/index.html": page('<a href="/CretLink/#nowhere">x</a> <a href="#also-nowhere">y</a>') });
    assert.equal(result.status, 1);
    assert.match(result.output, /\/CretLink\/#nowhere names an anchor that index\.html does not have/);
    assert.match(result.output, /#also-nowhere names an anchor that guide\/index\.html does not have/);
  });

  test("reports a link that leaves the base path of the site", () => {
    const result = check({ "guide/index.html": page('<a href="/guide/">without the base</a> <a href="../../other/">above it</a>') });
    assert.equal(result.status, 1);
    assert.match(result.output, /\/guide\/ leaves the site's base path \/CretLink\//);
    assert.match(result.output, /\.\.\/\.\.\/other\/ leaves the site's base path/);
  });

  test("reports a link to a repository file that does not exist", () => {
    const result = check({
      "guide/index.html": page('<a href="https://github.com/yveslaurentcreton/CretLink/blob/main/docs/no-such-file.md">x</a>'),
    });
    assert.equal(result.status, 1);
    assert.match(result.output, /the repository has no file docs\/no-such-file\.md/);
  });

  test("reports a page without a title, without one main heading, or with a picture without alt", () => {
    const result = check({
      "a/index.html": "<!doctype html><html><head></head><body><h1>One</h1></body></html>",
      "b/index.html": "<!doctype html><html><head><title>B</title></head><body><h1>One</h1><h1>Two</h1></body></html>",
      "c/index.html": page('<img src="/CretLink/_astro/picture.webp">'),
      "_astro/picture.webp": "picture",
    });
    assert.equal(result.status, 1);
    assert.match(result.output, /a\/index\.html: no title/);
    assert.match(result.output, /b\/index\.html: 2 main headings \(h1\), expected one/);
    assert.match(result.output, /c\/index\.html: an image without an alt attribute/);
  });

  test("does not follow the address the not-found page names for itself", () => {
    const result = check({
      "404.html": page("", '<title>Not found</title><link rel="canonical" href="https://yveslaurentcreton.github.io/CretLink/404/">'),
    });
    assert.equal(result.status, 0, result.output);
  });

  test("exits with 2 when the site was not built", () => {
    const run = spawnSync(process.execPath, [SCRIPT, join(root, "not-built")], { encoding: "utf8" });
    assert.equal(run.status, 2);
    assert.match(run.stderr, /build the site first/);
  });
});

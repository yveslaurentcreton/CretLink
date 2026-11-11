// Checks the built documentation site (ST-M011-001).
//
// Usage: node scripts/check-site.mjs [dist]   (from the website folder, after the build)
//
// Fails, naming page and target, when:
//
// - a link, image or other resource of the site itself points to a file that
//   the build did not produce, or to an anchor that its page does not have;
// - a link inside the site leaves the base path the site is published under;
// - a link to a file of this repository on GitHub (…/blob/main/<path>) names a
//   file that the repository does not hold;
// - a page has no title, no single main heading, or an image without an `alt`
//   attribute (an empty one marks a picture that only decorates).
//
// Other external links are listed, not fetched: a check that needs the network
// would fail for reasons that have nothing to do with a change.

import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, posix, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const BASE = "/CretLink/";
// The address the site is published at; a link that spells it out is a link inside the site.
const ORIGIN = "https://yveslaurentcreton.github.io";
const REPOSITORY_FILES = "https://github.com/yveslaurentcreton/CretLink/blob/main/";

const here = dirname(fileURLToPath(import.meta.url));
const dist = resolve(process.argv[2] ?? join(here, "..", "dist"));
const repository = resolve(here, "..", "..");

if (!existsSync(dist) || !statSync(dist).isDirectory()) {
  console.error(`check-site: ${dist} does not exist; build the site first (npm run build)`);
  process.exit(2);
}

const walk = (dir) =>
  readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
    entry.isDirectory() ? walk(join(dir, entry.name)) : [join(dir, entry.name)]
  );

const decode = (text) =>
  text
    .replaceAll("&amp;", "&")
    .replaceAll("&#38;", "&")
    .replaceAll("&quot;", '"')
    .replaceAll("&#39;", "'")
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">");

/** The file of the build that a path of the site names. */
const fileOf = (path) => {
  const inside = decodeURIComponent(path.slice(BASE.length));
  return join(dist, inside.endsWith("/") || inside === "" ? join(inside, "index.html") : inside);
};

const pages = walk(dist).filter((file) => file.endsWith(".html"));
const idsOf = new Map();
const ids = (file) => {
  if (!idsOf.has(file)) {
    const html = readFileSync(file, "utf8");
    idsOf.set(file, new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((match) => decode(match[1]))));
  }
  return idsOf.get(file);
};

const problems = [];
const external = new Set();
let checked = 0;

for (const page of pages) {
  const name = relative(dist, page);
  // The not-found page is served for any address, so the address it names for
  // itself does not exist as a file.
  const html =
    name === "404.html"
      ? readFileSync(page, "utf8").replace(/<link\b[^>]*\brel="canonical"[^>]*>/g, "")
      : readFileSync(page, "utf8");
  // The address of this page, to resolve relative links against.
  const address = BASE + name.split("\\").join("/").replace(/index\.html$/, "");

  if (!/<title>[^<]+<\/title>/.test(html)) problems.push(`${name}: no title`);
  const headings = (html.match(/<h1[\s>]/g) ?? []).length;
  if (headings !== 1) problems.push(`${name}: ${headings} main headings (h1), expected one`);
  for (const [tag] of html.matchAll(/<img\b[^>]*>/g)) {
    if (!/\salt(?:=|\s|>|\/)/.test(tag)) problems.push(`${name}: an image without an alt attribute: ${tag.slice(0, 90)}`);
  }

  const targets = [];
  for (const match of html.matchAll(/\s(?:href|src|poster)="([^"]*)"/g)) targets.push(decode(match[1]));
  for (const match of html.matchAll(/\ssrcset="([^"]*)"/g)) {
    for (const candidate of decode(match[1]).split(",")) {
      const url = candidate.trim().split(/\s+/)[0];
      if (url) targets.push(url);
    }
  }

  for (const spelled of targets) {
    if (spelled === "" || /^(?:mailto:|tel:|data:|javascript:)/i.test(spelled)) continue;
    const target = spelled.startsWith(ORIGIN + BASE) ? spelled.slice(ORIGIN.length) : spelled;
    if (/^https?:\/\//i.test(target)) {
      if (target.startsWith(REPOSITORY_FILES)) {
        checked += 1;
        const path = decodeURIComponent(target.slice(REPOSITORY_FILES.length).split(/[?#]/)[0]);
        if (!existsSync(join(repository, path))) problems.push(`${name}: the repository has no file ${path}`);
      } else {
        external.add(target.split("#")[0]);
      }
      continue;
    }
    checked += 1;
    const [pathPart, anchor] = target.split("#");
    const path = pathPart === "" ? address : posix.resolve(posix.dirname(address + "x"), pathPart.split("?")[0]) +
      (pathPart.split("?")[0].endsWith("/") && !pathPart.split("?")[0].endsWith("//") ? "/" : "");
    const normal = path.replace(/\/{2,}/g, "/");
    if (!normal.startsWith(BASE) && normal + "/" !== BASE) {
      problems.push(`${name}: ${target} leaves the site's base path ${BASE}`);
      continue;
    }
    const file = fileOf(normal === BASE.slice(0, -1) ? BASE : normal);
    if (!existsSync(file) || statSync(file).isDirectory()) {
      problems.push(`${name}: ${target} points to nothing (${relative(dist, file)})`);
      continue;
    }
    if (anchor && file.endsWith(".html") && !ids(file).has(decodeURIComponent(anchor))) {
      problems.push(`${name}: ${target} names an anchor that ${relative(dist, file)} does not have`);
    }
  }
}

for (const url of [...external].sort()) console.log(`external, not fetched: ${url}`);
for (const problem of problems) console.error(`check-site: ${problem}`);
console.log(
  `check-site: ${pages.length} pages, ${checked} links and resources checked, ${external.size} external links listed, ${problems.length} problems`
);
process.exit(problems.length === 0 ? 0 : 1);

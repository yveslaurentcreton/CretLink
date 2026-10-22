// Prints the version semantic-release will publish for HEAD, or nothing when
// the commits since the last release publish nothing (ST-M006-014).
//
// The release workflow needs the version *before* it builds, so the binaries
// and the per-OS archives can carry it (REQ-N-021). `semantic-release
// --dry-run` would need a token that may push, so this script reproduces the
// one decision that matters with semantic-release's own pieces: the last
// release is the highest `v<semver>` tag merged into HEAD (the configured
// `tagFormat`), and the release type comes from
// `@semantic-release/commit-analyzer` with the plugin configuration of
// `.releaserc.json`. `scripts/verify-version.cjs` makes semantic-release refuse
// to publish if it ever decides differently.
//
// Usage: node scripts/next-version.mjs   (from the repository root)

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { analyzeCommits } from "@semantic-release/commit-analyzer";

const git = (...args) => execFileSync("git", args, { encoding: "utf8" });

const config = JSON.parse(readFileSync(".releaserc.json", "utf8"));
const analyzer = config.plugins.find(
  (plugin) => (Array.isArray(plugin) ? plugin[0] : plugin) === "@semantic-release/commit-analyzer"
);
const pluginConfig = Array.isArray(analyzer) ? analyzer[1] ?? {} : {};

const parse = (tag) => {
  const match = /^v(\d+)\.(\d+)\.(\d+)$/.exec(tag);
  return match ? match.slice(1, 4).map(Number) : null;
};
const compare = (a, b) => a[0] - b[0] || a[1] - b[1] || a[2] - b[2];

const tags = git("tag", "--merged", "HEAD", "--list", "v*")
  .split("\n")
  .map((tag) => tag.trim())
  .filter((tag) => parse(tag));
if (tags.length === 0) {
  console.error("next-version: no v<semver> release tag is merged into HEAD");
  process.exit(1);
}
tags.sort((a, b) => compare(parse(a), parse(b)));
const lastTag = tags[tags.length - 1];

const separator = "\u001e";
const commits = git("log", `${lastTag}..HEAD`, `--format=%H%x1f%B${separator}`)
  .split(separator)
  .map((entry) => entry.replace(/^\n+/, ""))
  .filter((entry) => entry.includes("\u001f"))
  .map((entry) => {
    const [hash, message] = entry.split("\u001f");
    return { hash, message };
  });

const quiet = { log() {}, warn() {}, error: console.error, success() {} };
const type = await analyzeCommits(pluginConfig, {
  commits,
  logger: quiet,
  cwd: process.cwd(),
  options: {},
});

const [major, minor, patch] = parse(lastTag);
const next = {
  major: `${major + 1}.0.0`,
  minor: `${major}.${minor + 1}.0`,
  patch: `${major}.${minor}.${patch + 1}`,
}[type];
console.error(
  `next-version: last release ${lastTag}, ${commits.length} commit(s) since, release type ${type ?? "none"}`
);
if (next) {
  console.log(next);
}

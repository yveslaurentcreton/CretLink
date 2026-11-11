// Says which areas of the repository a change touches, so that ci.yml can skip
// the jobs a change cannot affect (ST-M006-017).
//
// Usage: node scripts/changed-areas.mjs <base> <head>   (from the repository root)
//
// Prints one `name=true|false` line per area, the form GitHub Actions reads
// from $GITHUB_OUTPUT:
//
// - `website`: a file under `website/`, the documentation site;
// - `code`: any file that is neither under `website/` nor documentation.
//   Documentation is a `*.md` file anywhere, everything under `docs/`, and
//   `LICENSE`. Workflows, scripts and manifests are code.
//
// A change that touches only documentation therefore reports both as false.
//
// The changed files are those between the merge base of <base> and <head>, and
// <head>: the commits of a pull request, or the commits of a push. When that
// cannot be worked out (no base on a first push, a commit that is missing from
// a shallow clone) every area counts as changed, so that a check is skipped
// only for a change that is known not to need it.

import { execFileSync } from "node:child_process";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const AREAS = ["code", "website"];

const isWebsite = (path) => path.startsWith("website/");
const isDocumentation = (path) =>
  path === "LICENSE" || path.startsWith("docs/") || path.toLowerCase().endsWith(".md");

/** The areas that the given repository-relative paths touch. */
export function classify(paths) {
  const areas = { code: false, website: false };
  for (const path of paths) {
    if (isWebsite(path)) areas.website = true;
    else if (!isDocumentation(path)) areas.code = true;
  }
  return areas;
}

const everything = () => Object.fromEntries(AREAS.map((area) => [area, true]));

/** The paths that differ between the merge base of `base` and `head`, and `head`; `null` when unknown. */
export function changedPaths(base, head, cwd = process.cwd()) {
  if (!/^[0-9a-f]{7,64}$/i.test(base ?? "") || /^0+$/.test(base)) return null;
  if (!/^[0-9a-f]{7,64}$/i.test(head ?? "")) return null;
  try {
    const out = execFileSync(
      "git",
      ["diff", "--name-only", "--no-renames", "-z", `${base}...${head}`],
      { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }
    );
    return out.split("\0").filter((path) => path !== "");
  } catch {
    return null;
  }
}

/** The areas a change from `base` to `head` touches; every area when the change is unknown. */
export function changedAreas(base, head, cwd = process.cwd()) {
  const paths = changedPaths(base, head, cwd);
  return paths === null ? everything() : classify(paths);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [base, head] = process.argv.slice(2);
  const areas = changedAreas(base, head);
  for (const area of AREAS) console.log(`${area}=${areas[area]}`);
}

// Checks the locked npm packages of the release tooling for known advisories
// (ST-M006-019, REQ-S-019).
//
// `npm audit` by itself cannot be this check. The release tooling
// (semantic-release) brings a package manager, npm, with npm's own
// dependencies packed inside it. No lockfile can move those: an advisory in
// one of them stays until semantic-release brings a newer npm, and a check
// that fails on it fails every pull request with nothing to do about it. So
// this script reads the report of `npm audit --json` and fails on what can be
// acted on:
//
//   - an advisory of high or critical severity
//   - that has a fix: an update of the lockfile inside the pinned versions
//     (`npm audit fix`), or a newer version of a pinned tool. An older
//     version of a tool is no fix; npm offers one when every later version
//     has the advisory too.
//
// What it does not fail on is counted in its last lines, so it stays visible.
//
// Usage: node scripts/check-npm-audit.mjs   (in the folder of the npm package)
//
// It reads `package-lock.json` there and asks the registry, like `npm audit`;
// nothing has to be installed. Exit status: 0 when nothing has to be fixed,
// 1 when an advisory has to be fixed, 2 when the check could not run.

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

/** The severities that fail the check when there is a fix. */
const FAILING = new Set(["high", "critical"]);

/** `[major, minor, patch]` of a version, or `null` when it is not one. */
function parseVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$/.exec(version ?? "");
  return match ? match.slice(1, 4).map(Number) : null;
}

/**
 * The versions of the packages that are installed at the top, by name, from
 * the content of a `package-lock.json`: the tools the package names itself
 * are among them.
 */
export function installedVersions(lock) {
  const versions = {};
  for (const [path, entry] of Object.entries(lock.packages ?? {})) {
    const match = /^node_modules\/((?:@[^/]+\/)?[^/]+)$/.exec(path);
    if (match && typeof entry.version === "string") versions[match[1]] = entry.version;
  }
  return versions;
}

/**
 * Whether what npm reports as `fixAvailable` is a fix: `true` (an update of
 * the lockfile) or a newer version of a tool. An older version is none. A
 * version that cannot be compared counts as a fix: better a failure to look
 * at than an advisory that is missed.
 */
export function hasFix(fixAvailable, installed) {
  if (fixAvailable === true) return true;
  if (fixAvailable === null || typeof fixAvailable !== "object") return false;
  const offered = parseVersion(fixAvailable.version);
  const current = parseVersion(installed[fixAvailable.name]);
  if (!offered || !current) return true;
  for (let i = 0; i < 3; i += 1) {
    if (offered[i] !== current[i]) return offered[i] > current[i];
  }
  return false;
}

/** How the fix of one package reads. */
function describeFix(fixAvailable) {
  return fixAvailable === true
    ? "npm audit fix"
    : `${fixAvailable.name} ${fixAvailable.version}`;
}

/**
 * Sorts the packages of a report of `npm audit --json` into those that fail
 * the check and the rest. Throws when the report is not one.
 */
export function evaluate(report, installed) {
  if (report?.auditReportVersion !== 2 || typeof report.vulnerabilities !== "object") {
    // npm leaves the summary of an error empty when the registry does not answer.
    const reason = report?.error?.summary || report?.message || "not a report of npm audit, version 2";
    throw new Error(`npm audit gave no report: ${reason}`);
  }
  const result = { failing: [], withoutFix: [], counts: {} };
  for (const entry of Object.values(report.vulnerabilities)) {
    result.counts[entry.severity] = (result.counts[entry.severity] ?? 0) + 1;
    if (!FAILING.has(entry.severity)) continue;
    if (hasFix(entry.fixAvailable, installed)) result.failing.push(entry);
    else result.withoutFix.push(entry);
  }
  const byName = (a, b) => a.name.localeCompare(b.name);
  result.failing.sort(byName);
  result.withoutFix.sort(byName);
  return result;
}

/** The lines the check prints for a result of [`evaluate`]. */
export function formatResult({ failing, withoutFix, counts }) {
  const lines = [];
  for (const entry of failing) {
    lines.push(`${entry.name}: ${entry.severity}, fix: ${describeFix(entry.fixAvailable)}`);
    // npm repeats an advisory for every range of versions it names.
    const reasons = (entry.via ?? []).map((via) =>
      typeof via === "string" ? `    through ${via}` : `    ${via.title} ${via.url}`,
    );
    lines.push(...new Set(reasons));
  }
  const total = Object.values(counts).reduce((sum, count) => sum + count, 0);
  const perSeverity = ["critical", "high", "moderate", "low", "info"]
    .filter((severity) => counts[severity])
    .map((severity) => `${counts[severity]} ${severity}`);
  lines.push(
    total === 0
      ? "check-npm-audit: no package with a known advisory"
      : `check-npm-audit: ${total} packages with a known advisory (${perSeverity.join(", ")})`,
  );
  if (withoutFix.length > 0) {
    lines.push(
      `check-npm-audit: of high or critical severity and without a fix here: ${withoutFix.map((entry) => entry.name).join(", ")}`,
    );
  }
  lines.push(
    failing.length > 0
      ? `check-npm-audit: ${failing.length} of high or critical severity with a fix; update them`
      : "check-npm-audit: none of high or critical severity has a fix",
  );
  return lines;
}

function main() {
  let result;
  try {
    const installed = installedVersions(JSON.parse(readFileSync("package-lock.json", "utf8")));
    // npm ends with status 1 when it found an advisory and also when it could
    // not ask; what it printed tells the two apart. On Windows npm is a
    // command file, which needs a shell.
    const npm = spawnSync("npm", ["audit", "--json"], {
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      shell: process.platform === "win32",
    });
    if (npm.error) throw new Error(`npm could not be run: ${npm.error.message}`);
    let report;
    try {
      report = JSON.parse(npm.stdout);
    } catch {
      throw new Error(`npm audit printed no report (status ${npm.status}): ${npm.stderr.trim()}`);
    }
    result = evaluate(report, installed);
  } catch (error) {
    console.error(`check-npm-audit: ${error.message}`);
    return 2;
  }
  for (const line of formatResult(result)) console.log(line);
  return result.failing.length > 0 ? 1 : 0;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  process.exitCode = main();
}

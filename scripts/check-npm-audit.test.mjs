// Tests of scripts/check-npm-audit.mjs (ST-M006-019). They use made-up
// reports; npm is not run. Run them from the repository root with:
// node --test "scripts/*.test.mjs"

import assert from "node:assert/strict";
import { test } from "node:test";

import { evaluate, formatResult, hasFix, installedVersions } from "./check-npm-audit.mjs";

/** One package of a report, as `npm audit --json` lists it. */
function entry(name, severity, fixAvailable, via = [`${name}-advisory`]) {
  return { name, severity, isDirect: false, via, effects: [], range: "*", nodes: [], fixAvailable };
}

function report(...entries) {
  return {
    auditReportVersion: 2,
    vulnerabilities: Object.fromEntries(entries.map((item) => [item.name, item])),
  };
}

const INSTALLED = { "release-tool": "25.0.9", "@scope/plugin": "12.0.9" };

test("the installed tools are the packages at the top of the lockfile", () => {
  const lock = {
    packages: {
      "": { name: "tooling", devDependencies: { "release-tool": "25.0.9" } },
      "node_modules/release-tool": { version: "25.0.9" },
      "node_modules/@scope/plugin": { version: "12.0.9" },
      // A package inside another one is not a tool of this package.
      "node_modules/release-tool/node_modules/inner": { version: "1.0.0" },
      "node_modules/@scope/plugin/node_modules/@other/inner": { version: "2.0.0" },
    },
  };
  assert.deepEqual(installedVersions(lock), { "release-tool": "25.0.9", "@scope/plugin": "12.0.9" });
  assert.deepEqual(installedVersions({}), {});
});

test("an update of the lockfile and a newer version of a tool are fixes", () => {
  assert.equal(hasFix(true, INSTALLED), true);
  assert.equal(hasFix({ name: "release-tool", version: "25.0.10", isSemVerMajor: false }, INSTALLED), true);
  assert.equal(hasFix({ name: "release-tool", version: "25.1.0", isSemVerMajor: false }, INSTALLED), true);
  assert.equal(hasFix({ name: "release-tool", version: "26.0.0", isSemVerMajor: true }, INSTALLED), true);
  assert.equal(hasFix({ name: "@scope/plugin", version: "12.0.10", isSemVerMajor: false }, INSTALLED), true);
});

test("no fix, and an older version of a tool, are no fixes", () => {
  assert.equal(hasFix(false, INSTALLED), false);
  assert.equal(hasFix(undefined, INSTALLED), false);
  assert.equal(hasFix(null, INSTALLED), false);
  assert.equal(hasFix({ name: "release-tool", version: "21.1.2", isSemVerMajor: true }, INSTALLED), false);
  assert.equal(hasFix({ name: "release-tool", version: "25.0.8", isSemVerMajor: false }, INSTALLED), false);
  assert.equal(hasFix({ name: "release-tool", version: "25.0.9", isSemVerMajor: false }, INSTALLED), false);
  // Numbers are compared as numbers: 9 is older than 10.
  assert.equal(hasFix({ name: "@scope/plugin", version: "9.9.9", isSemVerMajor: true }, INSTALLED), false);
});

test("a version that cannot be compared counts as a fix", () => {
  assert.equal(hasFix({ name: "unknown-tool", version: "1.0.0", isSemVerMajor: true }, INSTALLED), true);
  assert.equal(hasFix({ name: "release-tool", version: "next", isSemVerMajor: true }, INSTALLED), true);
  assert.equal(hasFix({ name: "release-tool" }, INSTALLED), true);
});

test("only high and critical severity with a fix fails", () => {
  const result = evaluate(
    report(
      entry("low-fixable", "low", true),
      entry("moderate-fixable", "moderate", true),
      entry("high-fixable", "high", true),
      entry("critical-newer-tool", "critical", { name: "release-tool", version: "26.0.0", isSemVerMajor: true }),
      entry("high-no-fix", "high", false),
      entry("critical-older-tool", "critical", { name: "release-tool", version: "21.1.2", isSemVerMajor: true }),
    ),
    INSTALLED,
  );
  assert.deepEqual(result.failing.map((item) => item.name), ["critical-newer-tool", "high-fixable"]);
  assert.deepEqual(result.withoutFix.map((item) => item.name), ["critical-older-tool", "high-no-fix"]);
  assert.deepEqual(result.counts, { low: 1, moderate: 1, high: 2, critical: 2 });
});

test("a report without a package passes", () => {
  const result = evaluate(report(), INSTALLED);
  assert.deepEqual(result, { failing: [], withoutFix: [], counts: {} });
  assert.deepEqual(formatResult(result), [
    "check-npm-audit: no package with a known advisory",
    "check-npm-audit: none of high or critical severity has a fix",
  ]);
});

test("what is not a report stops the check instead of passing it", () => {
  assert.throws(
    () => evaluate({ error: { code: "ENOLOCK", summary: "This command requires an existing lockfile." } }, INSTALLED),
    /gave no report: This command requires an existing lockfile\./,
  );
  // As npm prints it when the registry does not answer.
  assert.throws(
    () => evaluate({ message: "request failed", error: { summary: "", detail: "" } }, INSTALLED),
    /gave no report: request failed/,
  );
  assert.throws(() => evaluate({ auditReportVersion: 1, advisories: {} }, INSTALLED), /not a report of npm audit, version 2/);
  assert.throws(() => evaluate(null, INSTALLED), /gave no report/);
});

test("the lines name each failing package, its advisories once, its fix, and the rest in numbers", () => {
  const advisory = (title, id) => ({ source: 1, name: "x", title, url: `https://example.org/advisories/${id}`, severity: "high", range: "<2" });
  const result = evaluate(
    report(
      entry("parser", "high", true, [advisory("Parser reads past the end", "A-1"), advisory("Parser reads past the end", "A-1")]),
      entry("wrapper", "high", { name: "release-tool", version: "25.1.0", isSemVerMajor: false }, ["parser"]),
      entry("packed", "critical", { name: "release-tool", version: "21.1.2", isSemVerMajor: true }),
      entry("other", "moderate", false),
    ),
    INSTALLED,
  );
  assert.deepEqual(formatResult(result), [
    "parser: high, fix: npm audit fix",
    "    Parser reads past the end https://example.org/advisories/A-1",
    "wrapper: high, fix: release-tool 25.1.0",
    "    through parser",
    "check-npm-audit: 4 packages with a known advisory (1 critical, 2 high, 1 moderate)",
    "check-npm-audit: of high or critical severity and without a fix here: packed",
    "check-npm-audit: 2 of high or critical severity with a fix; update them",
  ]);
});

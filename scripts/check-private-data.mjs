// Keeps private data out of the repository (ST-M006-016, REQ-N-022).
//
// Reads every tracked file and, with --commits, the commit messages of a
// range, and reports what could identify a real computer, account, network or
// person:
//
//   ipv4           an IPv4 literal other than 127.x.x.x and the sample
//                  addresses
//   profile-path   a user-profile path (C:\Users\<name>, /home/<name>,
//                  /Users/<name>) of an account that is not a sample account
//   computer-name  a computer name as Windows generates it: DESKTOP- or
//                  LAPTOP- and seven or eight characters A-Z0-9
//   email          an e-mail address outside the sample domains
//   private-term   a term of the list given with --terms or, without that
//                  option, in the environment variable CRETLINK_PRIVATE_TERMS:
//                  one term per line; blank lines and lines starting with #
//                  are ignored. A term is never printed, only its number in
//                  the list. Without a list this rule is skipped.
//
// The first four rules apply to text: the content of text files, the names of
// all files and the commit messages. The terms are also searched in the bytes
// of binary files. The sample values that the first four rules allow are
// listed, each with its reason, in private-data-samples.json next to this
// script. Replace a reported value by a sample value; add a new sample value
// there and to "Sample data" in CONTRIBUTING.md.
//
// Usage: node scripts/check-private-data.mjs [--terms <file>] [--commits <range>] [--root <dir>]
//
//   --terms <file>     the list of private terms
//   --commits <range>  also check the commit messages of this range, for
//                      example origin/main..HEAD
//   --root <dir>       check the tracked files of this directory instead of
//                      the repository that holds this script
//
// Prints one line per problem, `path:line: rule: detail` or
// `commit <sha>: rule: detail`, and one summary line. Exit status: 0 when
// clean, 1 when there is a problem, 2 when the check could not run, as after a
// usage error.

import { execFileSync } from "node:child_process";
import { lstatSync, readFileSync, readlinkSync, realpathSync, statSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";

const SAMPLES_FILE = join(import.meta.dirname, "private-data-samples.json");
const USAGE =
  "Usage: node scripts/check-private-data.mjs [--terms <file>] [--commits <range>] [--root <dir>]";

// ---------------------------------------------------------------------------
// Sample values
// ---------------------------------------------------------------------------

const SAMPLE_LISTS = ["ipv4", "accounts", "computerNames", "emailDomains", "emailExceptions"];

/**
 * Turns the content of the samples file into lookups. Every list is a list of
 * groups, `{ "_comment": "why", "values": [...] }`, so that each value sits
 * next to the reason it is allowed.
 */
export function readSamples(content) {
  const lists = {};
  for (const name of SAMPLE_LISTS) {
    const groups = content?.[name];
    if (!Array.isArray(groups)) {
      throw new Error(`"${name}" must be a list of groups`);
    }
    lists[name] = groups.flatMap((group) => {
      const reason = group?._comment;
      const values = group?.values;
      if (
        typeof reason !== "string" ||
        reason.trim() === "" ||
        !Array.isArray(values) ||
        values.some((value) => typeof value !== "string")
      ) {
        throw new Error(`every group of "${name}" needs a "_comment" and a list of "values"`);
      }
      return values;
    });
  }
  const lower = (value) => value.toLowerCase();
  return {
    ipv4: new Set(lists.ipv4),
    accounts: new Set(lists.accounts.map(lower)),
    computerNames: new Set(lists.computerNames),
    emailDomains: lists.emailDomains.map(lower),
    emailExceptions: new Set(lists.emailExceptions),
  };
}

// ---------------------------------------------------------------------------
// Rules 1 to 4: patterns. Each function takes one line of text and returns
// the values in it that are not sample values, as they are written.
// ---------------------------------------------------------------------------

// A dotted quad that is not part of a longer dotted number such as 1.2.3.4.5.
const IPV4 = /(?<!\d)(?<!\d\.)(?:\d{1,3}\.){3}\d{1,3}(?!\d)(?!\.\d)/g;

/** Rule 1: IPv4 literals other than 127.x.x.x and the sample addresses. */
export function findIpv4(line, samples) {
  const found = [];
  for (const [literal] of line.matchAll(IPV4)) {
    const octets = literal.split(".").map(Number);
    if (octets.some((octet) => octet > 255)) {
      continue; // not an address
    }
    if (octets[0] !== 127 && !samples.ipv4.has(literal)) {
      found.push(literal);
    }
  }
  return found;
}

// `Users` after a drive letter, or `Users` or `home` after a path separator,
// then a separator and the account. A separator is any run of backslashes and
// slashes, because source files escape them (`C:\\Users\\ann`). The account
// ends at white space, a separator, a quote or punctuation that closes a path
// in prose and code; a placeholder in angle brackets is an account as a whole.
// An account with a space in its name therefore counts by its first word.
const PROFILE_PATH =
  /((?:[A-Za-z]:[\\/]+[Uu][Ss][Ee][Rr][Ss]|[\\/](?:Users|home))[\\/]+)(<[^\s\\/<>]+>|[^\s\\/<>"'`:;,|*?()[\]=&#!]+)/g;

/** Rule 2: user-profile paths of accounts that are not sample accounts. */
export function findProfilePaths(line, samples) {
  const found = [];
  for (const [, folder, written] of line.matchAll(PROFILE_PATH)) {
    // Dots that end a sentence do not belong to the account; `..` is its own.
    const account = written.replace(/(?<=[^.])\.+$/, "");
    if (!samples.accounts.has(account.toLowerCase())) {
      found.push(folder + account);
    }
  }
  return found;
}

const GENERATED_COMPUTER_NAME = /(?:DESKTOP|LAPTOP)-[A-Z0-9]{7,8}\b/g;

/** Rule 3: computer names of the form Windows generates. */
export function findComputerNames(line, samples) {
  return [...line.matchAll(GENERATED_COMPUTER_NAME)]
    .map(([name]) => name)
    .filter((name) => !samples.computerNames.has(name));
}

// The last label of the domain is alphabetic, so `package@1.2.3` is no address.
const EMAIL =
  /(?<![A-Za-z0-9._%+-])[A-Za-z0-9._%+-]+@((?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,})(?![A-Za-z0-9-])/g;

/** Rule 4: e-mail addresses outside the sample domains and their subdomains. */
export function findEmails(line, samples) {
  const found = [];
  for (const [address, domain] of line.matchAll(EMAIL)) {
    const host = domain.toLowerCase();
    const atSampleDomain = samples.emailDomains.some(
      (sample) => host === sample || host.endsWith(`.${sample}`)
    );
    if (!atSampleDomain && !samples.emailExceptions.has(address)) {
      found.push(address);
    }
  }
  return found;
}

const PATTERN_RULES = [
  { rule: "ipv4", find: findIpv4, problem: "is not a sample address" },
  {
    rule: "profile-path",
    find: findProfilePaths,
    problem: "is not the profile of a sample account",
  },
  { rule: "computer-name", find: findComputerNames, problem: "is not a sample computer name" },
  { rule: "email", find: findEmails, problem: "is not at a sample domain" },
];

// ---------------------------------------------------------------------------
// Rule 5: private terms
// ---------------------------------------------------------------------------

const escapeRegExp = (text) => text.replace(/[\\^$.*+?()[\]{}|]/g, "\\$&");

/** A copy of the bytes with the ASCII capitals in lower case. */
function foldAscii(bytes) {
  const folded = Buffer.from(bytes);
  for (let index = 0; index < folded.length; index += 1) {
    if (folded[index] >= 0x41 && folded[index] <= 0x5a) {
      folded[index] += 0x20;
    }
  }
  return folded;
}

/**
 * Reads a list of private terms: one per line; blank lines and lines starting
 * with # are ignored. The result holds, for each term, its 1-based position
 * among the terms, which is all a report shows of it, and two ways to find it:
 *
 * - `pattern` finds it in a text as a whole term, ignoring case: the character
 *   directly before and directly after is not a letter or a digit, so `ab-cd`
 *   is not found in `ab-cdx`. White space in a term also matches a line break,
 *   so a name is found where a paragraph wraps it.
 * - `needles` are its bytes in UTF-8 and in UTF-16LE, with the ASCII letters
 *   in lower case, for binary files.
 */
export function parseTerms(content) {
  return content
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== "" && !line.startsWith("#"))
    .map((term, index) => {
      const words = term.split(/\s+/);
      const text = words.join(" ");
      return {
        number: index + 1,
        pattern: new RegExp(
          `(?<![\\p{L}\\p{N}])${words.map(escapeRegExp).join("\\s+")}(?![\\p{L}\\p{N}])`,
          "giu"
        ),
        needles: [foldAscii(Buffer.from(text, "utf8")), foldAscii(Buffer.from(text, "utf16le"))],
      };
    });
}

/**
 * Replaces every private term in a line of output by its number. All output
 * passes through here, because a reported path or value can hold a term.
 */
export function redact(text, terms) {
  return terms.reduce(
    (result, term) => result.replace(term.pattern, `[private term #${term.number}]`),
    text
  );
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/** Returns a function that gives the 1-based line of an offset in the text. */
function lineLocator(lines) {
  const starts = [];
  let next = 0;
  for (const line of lines) {
    starts.push(next);
    next += line.length + 1;
  }
  return (offset) => {
    let low = 0;
    let high = starts.length - 1;
    while (low < high) {
      const middle = Math.ceil((low + high) / 2);
      if (starts[middle] <= offset) {
        low = middle;
      } else {
        high = middle - 1;
      }
    }
    return low + 1;
  };
}

/**
 * Applies the five rules to a text. Returns `{ line, rule, detail }` for every
 * problem, in the order of the lines and once per line when a value repeats.
 */
export function scanText(text, samples, terms = []) {
  const findings = [];
  const lines = text.split("\n");
  lines.forEach((line, index) => {
    for (const { rule, find, problem } of PATTERN_RULES) {
      for (const value of find(line, samples)) {
        findings.push({ line: index + 1, rule, detail: `${value} ${problem}` });
      }
    }
  });
  const lineAt = lineLocator(lines);
  for (const term of terms) {
    for (const match of text.matchAll(term.pattern)) {
      findings.push({
        line: lineAt(match.index),
        rule: "private-term",
        detail: `private term #${term.number}`,
      });
    }
  }
  const seen = new Set();
  return findings
    .filter(({ line, rule, detail }) => {
      const key = `${line}\n${rule}\n${detail}`;
      return !seen.has(key) && seen.add(key);
    })
    .sort((a, b) => a.line - b.line);
}

/** A file with a NUL byte in its first 8000 bytes is binary, as for git. */
export const isBinary = (bytes) => bytes.subarray(0, 8000).includes(0);

/**
 * Rule 5 in a binary file: the bytes of each term in UTF-8 and in UTF-16LE,
 * comparing ASCII letters without case. A binary file packs its strings
 * without separators, so a term counts wherever its bytes are, not only as a
 * whole term. The pattern rules do not apply to a binary file.
 */
export function scanBinary(bytes, terms) {
  const haystack = foldAscii(bytes);
  return terms
    .filter((term) => term.needles.some((needle) => haystack.includes(needle)))
    .map((term) => ({ rule: "private-term", detail: `private term #${term.number}` }));
}

/** One line of output for a problem in a file, a file name or a commit message. */
export function formatFinding({ path, commit, line, rule, detail }) {
  let place = path;
  if (commit !== undefined) {
    place = `commit ${commit}`;
  } else if (line !== undefined) {
    place = `${path}:${line}`;
  }
  return `${place}: ${rule}: ${detail}`;
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

class UsageError extends Error {}

function git(directory, ...args) {
  try {
    return execFileSync("git", args, {
      cwd: directory,
      encoding: "utf8",
      maxBuffer: 2 ** 30,
      stdio: ["ignore", "pipe", "pipe"],
    });
  } catch (error) {
    const reason = String(error.stderr ?? "").trim() || error.message;
    throw new UsageError(`git ${args[0]} failed: ${reason}`);
  }
}

function parseOptions(argv) {
  try {
    return parseArgs({
      args: argv,
      options: {
        terms: { type: "string" },
        commits: { type: "string" },
        root: { type: "string" },
        help: { type: "boolean", short: "h" },
      },
      allowPositionals: false,
      strict: true,
    }).values;
  } catch (error) {
    throw new UsageError(error.message);
  }
}

function loadSamples() {
  try {
    return readSamples(JSON.parse(readFileSync(SAMPLES_FILE, "utf8")));
  } catch (error) {
    throw new UsageError(`cannot use ${SAMPLES_FILE}: ${error.message}`);
  }
}

/** The list of --terms, which must be readable, or else of the environment. */
function loadTerms(file, env) {
  let content = env.CRETLINK_PRIVATE_TERMS ?? "";
  if (file !== undefined) {
    try {
      content = readFileSync(file, "utf8");
    } catch (error) {
      throw new UsageError(`cannot read the terms file ${file}: ${error.code}`);
    }
  }
  try {
    return parseTerms(content);
  } catch {
    // Without the reason, which could quote a term.
    throw new UsageError("cannot use the list of private terms");
  }
}

/** The problems of one tracked file: in its name and in its content. */
function checkFile(root, path, samples, terms) {
  const findings = scanText(path, samples, terms).map(({ rule, detail }) => ({
    path,
    rule,
    detail: `${detail} (file name)`,
  }));
  const location = join(root, path);
  const entry = lstatSync(location, { throwIfNoEntry: false });
  let content;
  if (entry?.isSymbolicLink()) {
    content = Buffer.from(readlinkSync(location)); // git tracks the target of a link
  } else if (entry?.isFile()) {
    content = readFileSync(location);
  } else {
    return findings; // deleted in the working tree, or a submodule
  }
  if (isBinary(content)) {
    return findings.concat(
      scanBinary(content, terms).map(({ rule, detail }) => ({
        path,
        rule,
        detail: `${detail} (binary file)`,
      }))
    );
  }
  return findings.concat(
    scanText(content.toString("utf8"), samples, terms).map((finding) => ({ path, ...finding }))
  );
}

/** The problems in the messages of the commits of a range, once per commit. */
function checkCommits(root, range, samples, terms) {
  // --end-of-options: the range is a revision, whatever it starts with.
  const records = git(
    root,
    "log",
    "-z",
    "--no-show-signature",
    "--format=%h%n%B",
    "--end-of-options",
    range,
    "--"
  )
    .split("\0")
    .filter((record) => record !== "");
  const findings = records.flatMap((record) => {
    const [commit, ...message] = record.split("\n");
    const seen = new Set();
    return scanText(message.join("\n"), samples, terms)
      .map(({ rule, detail }) => ({ commit, rule, detail }))
      .filter(({ rule, detail }) => !seen.has(rule + detail) && seen.add(rule + detail));
  });
  return { count: records.length, findings };
}

function summary(problems, files, commits, terms) {
  const counted = (count, noun) => `${count} ${noun}${count === 1 ? "" : "s"}`;
  const scope =
    commits === undefined
      ? counted(files, "file")
      : `${counted(files, "file")} and ${counted(commits, "commit message")}`;
  const termsNote = `private terms: ${terms === 0 ? "not checked" : `${terms} checked`}`;
  return problems === 0
    ? `check-private-data: no private data in ${scope}; ${termsNote}`
    : `check-private-data: ${counted(problems, "problem")} in ${scope}; ${termsNote}; ` +
        'see "Sample data" in CONTRIBUTING.md';
}

/** Runs the check and returns the exit status. */
function main(argv, env) {
  let terms = [];
  try {
    const options = parseOptions(argv);
    if (options.help) {
      console.log(USAGE);
      return 0;
    }
    const samples = loadSamples();
    terms = loadTerms(options.terms, env);
    if (
      options.root !== undefined &&
      !statSync(options.root, { throwIfNoEntry: false })?.isDirectory()
    ) {
      throw new UsageError(`--root is not a directory: ${options.root}`);
    }
    const root =
      options.root ?? git(import.meta.dirname, "rev-parse", "--show-toplevel").trimEnd();
    const files = [...new Set(git(root, "ls-files", "-z").split("\0"))].filter(
      (path) => path !== ""
    );
    const findings = files.flatMap((path) => checkFile(root, path, samples, terms));
    let commits;
    if (options.commits !== undefined) {
      const checked = checkCommits(root, options.commits, samples, terms);
      commits = checked.count;
      findings.push(...checked.findings);
    }
    for (const finding of findings) {
      console.log(redact(formatFinding(finding), terms));
    }
    console.log(redact(summary(findings.length, files.length, commits, terms.length), terms));
    return findings.length === 0 ? 0 : 1;
  } catch (error) {
    const usage = error instanceof UsageError;
    console.error(redact(`check-private-data: ${usage ? error.message : error.stack}`, terms));
    if (usage) {
      console.error(USAGE);
    }
    return 2;
  }
}

// Run as a program, not when the tests import the functions. Node 22.18 and
// later say which it is; before that, compare with the path of the program.
const isProgram =
  import.meta.main ??
  (process.argv[1] !== undefined && realpathSync(process.argv[1]) === import.meta.filename);
if (isProgram) {
  process.exitCode = main(process.argv.slice(2), process.env);
}

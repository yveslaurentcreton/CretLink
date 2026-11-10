// Tests of scripts/check-private-data.mjs (ST-M006-016). Run them from the
// repository root with: node --test "scripts/*.test.mjs"
//
// The check reads this file too. A value that it must reject is therefore put
// together from parts here and never written as a whole.

import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, before, describe, test } from "node:test";

import {
  findComputerNames,
  findEmails,
  findIpv4,
  findProfilePaths,
  formatFinding,
  isBinary,
  parseTerms,
  readSamples,
  redact,
  scanBinary,
  scanText,
} from "./check-private-data.mjs";

const SCRIPT = join(import.meta.dirname, "check-private-data.mjs");
const SAMPLES_FILE = join(import.meta.dirname, "private-data-samples.json");

const group = (...values) => [{ _comment: "for the tests", values }];
const samples = readSamples({
  ipv4: group("0.0.0.0", "10.0.0.42", "192.0.2.1", "1.0.0.0"),
  accounts: group("ann", "jdoe", "..", "<name>", "%USERNAME%", "$USER", "USERNAME"),
  computerNames: group(),
  emailDomains: group("users.noreply.github.com", "example.org"),
  emailExceptions: group("128x128@2x.png"),
});

// Values that are not samples, each put together from its parts.
const address = (...octets) => octets.join(".");
const mailbox = (local, domain) => [local, domain].join("@");
const PUBLIC_ADDRESS = address(8, 8, 4, 4);
const STRANGER = "mallory"; // an account that is not a sample account
const GENERATED_NAME = ["DESKTOP", "AB12CD3"].join("-");
const OUTSIDE_MAILBOX = mailbox("pat", "corp.invalid");

describe("the samples file", () => {
  test("becomes lookups that ignore case for accounts and domains", () => {
    const read = readSamples({
      ipv4: [
        { _comment: "one group", values: ["10.0.0.42"] },
        { _comment: "another group", values: ["192.0.2.1"] },
      ],
      accounts: group("Ann"),
      computerNames: group(),
      emailDomains: group("Example.ORG"),
      emailExceptions: group("128x128@2x.png"),
    });
    assert.deepEqual([...read.ipv4], ["10.0.0.42", "192.0.2.1"]);
    assert.ok(read.accounts.has("ann"));
    assert.deepEqual(read.emailDomains, ["example.org"]);
    assert.equal(read.computerNames.size, 0);
  });

  test("needs every list, and a reason and values for every group", () => {
    const complete = {
      ipv4: group(),
      accounts: group(),
      computerNames: group(),
      emailDomains: group(),
      emailExceptions: group(),
    };
    assert.throws(
      () => readSamples({ ...complete, accounts: undefined }),
      /"accounts" must be a list of groups/
    );
    for (const group of [
      "0.0.0.0", // a value where a group belongs
      { values: ["0.0.0.0"] },
      { _comment: " ", values: ["0.0.0.0"] },
      { _comment: "why", values: "0.0.0.0" },
      { _comment: "why", values: [0] },
    ]) {
      assert.throws(
        () => readSamples({ ...complete, ipv4: [group] }),
        /every group of "ipv4" needs a "_comment" and a list of "values"/,
        JSON.stringify(group)
      );
    }
  });

  test("of the repository is valid and holds the samples these tests rely on", () => {
    const real = readSamples(JSON.parse(readFileSync(SAMPLES_FILE, "utf8")));
    assert.ok(real.ipv4.has("10.0.0.42"));
    assert.ok(real.accounts.has("ann"));
    assert.ok(real.emailDomains.includes("example.org"));
  });
});

describe("rule 1: IPv4 literals", () => {
  test("allows loopback addresses and the sample addresses", () => {
    for (const line of [
      "listen on 127.0.0.1:7000",
      "any of 127.0.0.0/8, such as 127.12.13.14",
      "never on `0.0.0.0`/`::`",
      'address: "10.0.0.42:51234",',
      "a peer (192.0.2.1).",
      'version="1.0.0.0"',
    ]) {
      assert.deepEqual(findIpv4(line, samples), [], line);
    }
  });

  test("rejects every other address, as it is written", () => {
    const unlisted = address(10, 0, 0, 43);
    const padded = address("010", "000", "000", "042"); // a sample address, written otherwise
    for (const [line, expected] of [
      [`resolver ${PUBLIC_ADDRESS}`, [PUBLIC_ADDRESS]],
      [`"${PUBLIC_ADDRESS}:53"`, [PUBLIC_ADDRESS]],
      [`Ask ${PUBLIC_ADDRESS}.`, [PUBLIC_ADDRESS]],
      [`route ${unlisted}/24 via 10.0.0.42`, [unlisted]],
      [`v${PUBLIC_ADDRESS}`, [PUBLIC_ADDRESS]],
      [`host.${PUBLIC_ADDRESS}.example.org`, [PUBLIC_ADDRESS]],
      [`from ${PUBLIC_ADDRESS} to ${unlisted}`, [PUBLIC_ADDRESS, unlisted]],
      [padded, [padded]],
    ]) {
      assert.deepEqual(findIpv4(line, samples), expected, line);
    }
  });

  test("ignores what is not a dotted quad with valid octets", () => {
    for (const line of [
      "256.1.1.1",
      "999.999.999.999",
      "1.2.3.4.5",
      "1.2.3.4.5.6",
      "1234.5.6.7",
      "1.2.3.4567",
      "version 22.14.0",
      "10.0.19045",
    ]) {
      assert.deepEqual(findIpv4(line, samples), [], line);
    }
  });
});

describe("rule 2: user-profile paths", () => {
  test("allows the profiles of sample accounts, however the path is escaped", () => {
    for (const line of [
      String.raw`C:\Users\ann\AppData\Local\CretLink`,
      String.raw`location: "C:\\Users\\jdoe\\Documents",`,
      String.raw`command = \"C:\\\\Users\\\\Ann Lee\\\\cretlink-cli.exe\"`,
      String.raw`& 'C:\Users\Ann O''Neil\Downloads\CretLink-Support.exe' recover`,
      "C:/Users/ann/image.bin",
      "/home/ann/.local/bin/cretlink",
      "/Users/ann/Library",
      "/home/ANN and /home/Jdoe",
      "ann:x:1000:1000:Ann,,,:/home/ann:/bin/bash",
      "<dd>/home/ann</dd>",
      "(see /home/ann), then /home/ann; or /home/ann, or /home/ann.",
    ]) {
      assert.deepEqual(findProfilePaths(line, samples), [], line);
    }
  });

  test("allows a placeholder only as a sample account", () => {
    for (const line of [
      String.raw`C:\Users\<name>\AppData`,
      String.raw`C:\Users\%USERNAME%\AppData`,
      "/home/$USER/.config",
      String.raw`C:\Users\USERNAME`,
      String.raw`C:\Users\..\Windows`,
      "C:/Users/../Windows",
    ]) {
      assert.deepEqual(findProfilePaths(line, samples), [], line);
    }
    const noPlaceholders = readSamples({
      ipv4: group(),
      accounts: group("ann"),
      computerNames: group(),
      emailDomains: group(),
      emailExceptions: group(),
    });
    assert.deepEqual(findProfilePaths(String.raw`C:\Users\..\Windows`, noPlaceholders), [
      String.raw`C:\Users\..`,
    ]);
    assert.deepEqual(findProfilePaths("/home/<name>/x", noPlaceholders), ["/home/<name>"]);
  });

  test("ignores a path without an account", () => {
    for (const line of [
      String.raw`abs("C:\\Users")`,
      "the /home folder",
      "/home/ and /Users/",
      "home/" + STRANGER,
    ]) {
      assert.deepEqual(findProfilePaths(line, samples), [], line);
    }
  });

  test("rejects the profile of any other account, in every form of the path", () => {
    for (const [written, reported] of [
      ["C:\\Users\\", "C:\\Users\\"],
      ["C:\\\\Users\\\\", "C:\\\\Users\\\\"], // escaped once, as in a source file
      ["C:\\\\\\\\Users\\\\\\\\", "C:\\\\\\\\Users\\\\\\\\"], // escaped twice
      ["C:/Users/", "C:/Users/"],
      ["c:\\users\\", "c:\\users\\"],
      ["D:\\Users\\", "D:\\Users\\"],
      ["/home/", "/home/"],
      ["/Users/", "/Users/"],
      ["file:///home/", "/home/"],
      ["/c/Users/", "/Users/"], // as Git Bash writes a Windows path
      ["\\\\server\\c$\\Users\\", "\\Users\\"],
    ]) {
      const line = `open ${written}${STRANGER}/Documents`;
      assert.deepEqual(findProfilePaths(line, samples), [reported + STRANGER], line);
    }
  });

  test("ends the account at the end of the path", () => {
    const ends = ['"', "'", "`", " ", "\\", "/", ":", ";", ",", ")", "]", "<", "&", "?", ". ", "."];
    for (const end of ends) {
      const line = "/home/" + STRANGER + end;
      assert.deepEqual(findProfilePaths(line, samples), ["/home/" + STRANGER], line);
    }
    assert.deepEqual(findProfilePaths("C:\\Users\\" + STRANGER + " Lee\\x", samples), [
      "C:\\Users\\" + STRANGER,
    ]);
    assert.deepEqual(findProfilePaths("/home/" + STRANGER + ".lee/x", samples), [
      "/home/" + STRANGER + ".lee",
    ]);
  });
});

describe("rule 3: generated Windows computer names", () => {
  const eightCharacters = ["LAPTOP", "AB12CD34"].join("-");

  test("rejects DESKTOP- and LAPTOP- with seven or eight generated characters", () => {
    for (const [line, expected] of [
      [`hostname: "${GENERATED_NAME}",`, [GENERATED_NAME]],
      [`${eightCharacters}\\Jo Doe`, [eightCharacters]],
      [`${GENERATED_NAME}.local and ${eightCharacters}`, [GENERATED_NAME, eightCharacters]],
      [["DESKTOP", "ABCDEFG"].join("-"), [["DESKTOP", "ABCDEFG"].join("-")]],
    ]) {
      assert.deepEqual(findComputerNames(line, samples), expected, line);
    }
  });

  test("allows the sample names and everything of another form", () => {
    for (const line of [
      "TECH-LAPTOP, CUSTOMER-PC, ACME-PC-07, FRONTDESK-2 and MOM-LAPTOP",
      "DESKTOP-7Q2",
      "DESKTOP-AB12CD",
      "DESKTOP-AB12CD345",
      "DESKTOP-ab12cd3",
      "DESKTOP-AB12CD3x",
      "desktop-control",
    ]) {
      assert.deepEqual(findComputerNames(line, samples), [], line);
    }
  });

  test("allows a generated name only when it is listed", () => {
    const listed = readSamples({
      ipv4: group(),
      accounts: group(),
      computerNames: group(GENERATED_NAME),
      emailDomains: group(),
      emailExceptions: group(),
    });
    assert.deepEqual(findComputerNames(`${GENERATED_NAME} and ${eightCharacters}`, listed), [
      eightCharacters,
    ]);
  });
});

describe("rule 4: e-mail addresses", () => {
  test("allows the sample domains, their subdomains and the listed exceptions", () => {
    for (const line of [
      "ann@example.org",
      "Ann.Lee+tag@Example.ORG",
      "<sam@mail.example.org>",
      "12345+sam@users.noreply.github.com",
      '"icons/128x128@2x.png",',
    ]) {
      assert.deepEqual(findEmails(line, samples), [], line);
    }
  });

  test("rejects an address at any other domain, as it is written", () => {
    const lookalike = mailbox("ann", "notexample.org");
    const suffixed = mailbox("ann", "example.org.corp.invalid");
    const parent = mailbox("git", "github.com");
    const icon = mailbox("icon", "2x.png");
    for (const [line, expected] of [
      [`Reported by ${OUTSIDE_MAILBOX}.`, [OUTSIDE_MAILBOX]],
      [`<${OUTSIDE_MAILBOX}>`, [OUTSIDE_MAILBOX]],
      [`mailto:${OUTSIDE_MAILBOX}?subject=x`, [OUTSIDE_MAILBOX]],
      [lookalike, [lookalike]],
      [suffixed, [suffixed]],
      [`${parent}:owner/repository.git`, [parent]],
      [icon, [icon]],
    ]) {
      assert.deepEqual(findEmails(line, samples), expected, line);
    }
  });

  test("ignores what is not an address", () => {
    for (const line of [
      "semantic-release@25.0.9",
      "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1",
      '"@semantic-release/github": "12.0.9"',
      "sam@tech-laptop",
      "@media (min-width: 40em)",
    ]) {
      assert.deepEqual(findEmails(line, samples), [], line);
    }
  });
});

describe("rule 5: private terms", () => {
  const found = (text, list) => scanText(text, samples, parseTerms(list));

  test("reads one term per line and skips blank lines and comments", () => {
    const terms = parseTerms("\uFEFF# the list\r\n\r\nzebra-orchid\r\n  Quill Lane  \n\n#x\nab-cd");
    assert.deepEqual(
      terms.map((term) => term.number),
      [1, 2, 3]
    );
    assert.deepEqual(parseTerms(""), []);
    assert.deepEqual(parseTerms("# only a comment\n\n"), []);
  });

  test("finds a term as a whole term, ignoring case", () => {
    const whole = ["ab-cd", "(ab-cd)", "see ab-cd.", "AB-Cd", "x/ab-cd/y", "ab-cd_old", "_ab-cd"];
    for (const text of whole) {
      assert.deepEqual(
        found(text, "ab-cd"),
        [{ line: 1, rule: "private-term", detail: "private term #1" }],
        text
      );
    }
    const part = ["ab-cdx", "xab-cd", "ab-cd1", "9ab-cd", "\u00e9ab-cd", "ab-cd\u00e9", "ab cd", "abcd"];
    for (const text of part) {
      assert.deepEqual(found(text, "ab-cd"), [], text);
    }
  });

  test("takes a term literally", () => {
    assert.equal(found("call a.c+(x) now", "a.c+(x)").length, 1);
    assert.deepEqual(found("call abc+(x) now", "a.c+(x)"), []);
  });

  test("finds a term of several words where a line break splits it", () => {
    assert.deepEqual(found("one\nmet Quill\n  Lane there", "Quill Lane"), [
      { line: 2, rule: "private-term", detail: "private term #1" },
    ]);
    assert.deepEqual(found("QuillLane", "Quill Lane"), []);
  });

  test("reports the number of the term and the line, never the term", () => {
    const findings = found(
      "first\nthe Zebra-Orchid site\n\nzebra-orchid again",
      "other\n# skipped\nzebra-orchid"
    );
    assert.deepEqual(findings, [
      { line: 2, rule: "private-term", detail: "private term #2" },
      { line: 4, rule: "private-term", detail: "private term #2" },
    ]);
    assert.doesNotMatch(JSON.stringify(findings), /zebra/i);
  });

  test("takes every term out of a line of output", () => {
    const terms = parseTerms("zebra-orchid\nquill lane");
    assert.equal(
      redact("docs/Zebra-Orchid.md:3: email: zebra-orchid@example.org of Quill Lane", terms),
      "docs/[private term #1].md:3: email: [private term #1]@example.org of [private term #2]"
    );
    assert.equal(redact("zebra-orchids stay", terms), "zebra-orchids stay");
    assert.equal(redact("nothing to hide", []), "nothing to hide");
  });

  describe("in a binary file", () => {
    const terms = parseTerms("other\nzebra-orchid");
    const header = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x00, 0x1a]);
    const binary = (...parts) => Buffer.concat([header, ...parts, Buffer.from([0xff, 0x00])]);
    const reported = [{ rule: "private-term", detail: "private term #2" }];

    test("a NUL byte in the first 8000 bytes makes a file binary", () => {
      assert.equal(isBinary(Buffer.from("plain text\n")), false);
      assert.equal(isBinary(Buffer.alloc(0)), false);
      assert.equal(isBinary(binary()), true);
      assert.equal(isBinary(Buffer.concat([Buffer.alloc(7999, 0x61), Buffer.from([0])])), true);
      assert.equal(isBinary(Buffer.concat([Buffer.alloc(8000, 0x61), Buffer.from([0])])), false);
    });

    test("finds a term in UTF-8 and in UTF-16LE, ignoring the case of ASCII letters", () => {
      assert.deepEqual(scanBinary(binary(Buffer.from("Zebra-ORCHID", "utf8")), terms), reported);
      assert.deepEqual(scanBinary(binary(Buffer.from("zebra-Orchid", "utf16le")), terms), reported);
      assert.deepEqual(scanBinary(binary(Buffer.from("zebra orchid", "utf16le")), terms), []);
      assert.deepEqual(scanBinary(binary(), terms), []);
    });

    test("finds it wherever its bytes are, because a binary file packs its strings", () => {
      for (const encoding of ["utf8", "utf16le"]) {
        for (const text of ["zebra-orchid", "name=zebra-orchid;", "couldNotReadZebra-OrchidPath"]) {
          const bytes = binary(Buffer.from(text, encoding));
          assert.deepEqual(scanBinary(bytes, terms), reported, `${text} in ${encoding}`);
        }
      }
      assert.deepEqual(scanBinary(Buffer.from("zebra-orchid"), terms), reported);
      assert.deepEqual(scanBinary(Buffer.from("zebra-orchid and the other"), terms), [
        { rule: "private-term", detail: "private term #1" },
        ...reported,
      ]);
    });
  });
});

describe("a text", () => {
  test("is checked line by line with every rule", () => {
    const text = [
      "listen on 127.0.0.1",
      `resolver ${PUBLIC_ADDRESS}\r`,
      "C:\\Users\\" + STRANGER + "\\Documents",
      `hostname ${GENERATED_NAME}`,
      `mail ${OUTSIDE_MAILBOX} about zebra-orchid`,
      "",
    ].join("\n");
    assert.deepEqual(scanText(text, samples, parseTerms("zebra-orchid")), [
      { line: 2, rule: "ipv4", detail: `${PUBLIC_ADDRESS} is not a sample address` },
      {
        line: 3,
        rule: "profile-path",
        detail: "C:\\Users\\" + STRANGER + " is not the profile of a sample account",
      },
      { line: 4, rule: "computer-name", detail: `${GENERATED_NAME} is not a sample computer name` },
      { line: 5, rule: "email", detail: `${OUTSIDE_MAILBOX} is not at a sample domain` },
      { line: 5, rule: "private-term", detail: "private term #1" },
    ]);
  });

  test("reports a value once per line", () => {
    const text = `${PUBLIC_ADDRESS} and ${PUBLIC_ADDRESS}\n${PUBLIC_ADDRESS}\n`;
    assert.deepEqual(
      scanText(text, samples).map((finding) => finding.line),
      [1, 2]
    );
  });

  test("without problems gives nothing", () => {
    assert.deepEqual(scanText("", samples), []);
    const terms = parseTerms("zebra-orchid");
    assert.deepEqual(scanText("ann@example.org at 10.0.0.42\n", samples, terms), []);
  });
});

describe("a line of output", () => {
  test("names the file and the line, the file, or the commit", () => {
    const problem = { rule: "ipv4", detail: "x is not a sample address" };
    assert.equal(
      formatFinding({ path: "docs/a.md", line: 12, ...problem }),
      "docs/a.md:12: ipv4: x is not a sample address"
    );
    assert.equal(
      formatFinding({ path: "icon.png", ...problem }),
      "icon.png: ipv4: x is not a sample address"
    );
    assert.equal(
      formatFinding({ commit: "1a2b3c4", ...problem }),
      "commit 1a2b3c4: ipv4: x is not a sample address"
    );
  });
});

describe("the command", () => {
  const SUMMARY = "check-private-data: ";
  const HINT = '; see "Sample data" in CONTRIBUTING.md';
  let work;
  let clean;
  let dirty;
  let termsFile;
  let environment;
  let cleanCommit;
  let firstCommit;
  let secondCommit;

  const git = (directory, ...args) =>
    execFileSync("git", args, { cwd: directory, env: environment, encoding: "utf8" }).trim();
  const write = (directory, path, content) => {
    mkdirSync(join(directory, path, ".."), { recursive: true });
    writeFileSync(join(directory, path), content);
  };
  const commit = (directory, message) => {
    git(directory, "add", "--all");
    git(directory, "commit", "--quiet", "--message", message);
    return git(directory, "rev-parse", "--short", "HEAD");
  };
  const check = (args, extraEnvironment = {}) =>
    spawnSync(process.execPath, [SCRIPT, ...args], {
      encoding: "utf8",
      env: { ...environment, ...extraEnvironment },
    });

  before(() => {
    work = mkdtempSync(join(tmpdir(), "check-private-data-"));
    clean = join(work, "clean");
    dirty = join(work, "dirty");
    termsFile = join(work, "terms.txt");
    writeFileSync(termsFile, "# invented terms\nquill lane\n\nZebra-Orchid\n");
    writeFileSync(join(work, "gitconfig"), "");

    // Neither the developer's git settings, nor a repository around the
    // temporary folder, nor a list of terms in the environment reaches the test.
    environment = Object.fromEntries(
      Object.entries(process.env).filter(
        ([name]) => !name.startsWith("GIT_") && name !== "CRETLINK_PRIVATE_TERMS"
      )
    );
    Object.assign(environment, {
      GIT_CONFIG_GLOBAL: join(work, "gitconfig"),
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_CEILING_DIRECTORIES: dirname(work),
      GIT_AUTHOR_NAME: "Sam",
      GIT_AUTHOR_EMAIL: "sam@example.org",
      GIT_COMMITTER_NAME: "Sam",
      GIT_COMMITTER_EMAIL: "sam@example.org",
    });

    for (const directory of [clean, dirty]) {
      mkdirSync(directory);
      git(directory, "-c", "init.defaultBranch=main", "init", "--quiet");
      write(
        directory,
        "README.md",
        [
          "Connect to 10.0.0.42 or 127.0.0.1.",
          "The profile is C:\\Users\\ann.",
          "Write to ann@example.org.",
          "",
        ].join("\n")
      );
    }
    cleanCommit = commit(clean, "docs: describe the connection");
    firstCommit = commit(dirty, "docs: describe the connection");

    write(
      dirty,
      "notes/session.txt",
      [
        `resolver ${PUBLIC_ADDRESS}`,
        "C:\\Users\\" + STRANGER + "\\Documents",
        `hostname ${GENERATED_NAME}`,
        `mail ${OUTSIDE_MAILBOX}`,
        "the zebra-orchid site, not the zebra-orchids",
        "",
      ].join("\n")
    );
    write(
      dirty,
      "capture.bin",
      Buffer.concat([
        Buffer.from([0x00, 0x01, 0x02]),
        Buffer.from("Zebra-Orchid", "utf16le"),
        Buffer.from([0x00, 0x00]),
      ])
    );
    write(dirty, "exports/Zebra-Orchid report.txt", "Nothing private in here.\n");
    write(dirty, `logs/${GENERATED_NAME}.log`, "Nothing private in here either.\n");
    secondCommit = commit(
      dirty,
      `docs: add the session at Zebra-Orchid\n\nSeen from ${PUBLIC_ADDRESS}, twice: ${PUBLIC_ADDRESS}.\n`
    );
  });

  after(() => {
    rmSync(work, { recursive: true, force: true });
  });

  // What the pattern rules report in the files of the second commit.
  const patternProblems = () => {
    const profile = "C:\\Users\\" + STRANGER;
    const named = `${GENERATED_NAME} is not a sample computer name`;
    return [
      `logs/${GENERATED_NAME}.log: computer-name: ${named} (file name)`,
      `notes/session.txt:1: ipv4: ${PUBLIC_ADDRESS} is not a sample address`,
      `notes/session.txt:2: profile-path: ${profile} is not the profile of a sample account`,
      `notes/session.txt:3: computer-name: ${named}`,
      `notes/session.txt:4: email: ${OUTSIDE_MAILBOX} is not at a sample domain`,
    ];
  };

  test("prints one summary line when the files and the commit messages are clean", () => {
    const plain = check(["--root", clean]);
    assert.equal(plain.stderr, "");
    assert.equal(plain.stdout, `${SUMMARY}no private data in 1 file; private terms: not checked\n`);
    assert.equal(plain.status, 0);

    const complete = check(["--root", clean, "--terms", termsFile, "--commits", cleanCommit]);
    assert.equal(complete.stderr, "");
    assert.equal(
      complete.stdout,
      `${SUMMARY}no private data in 1 file and 1 commit message; private terms: 2 checked\n`
    );
    assert.equal(complete.status, 0);
  });

  test("reports every problem in the files, the file names and the commit messages", () => {
    const range = `${firstCommit}..${secondCommit}`;
    const result = check(["--root", dirty, "--terms", termsFile, "--commits", range]);
    assert.equal(result.stderr, "");
    assert.deepEqual(result.stdout.split("\n"), [
      "capture.bin: private-term: private term #2 (binary file)",
      "exports/[private term #2] report.txt: private-term: private term #2 (file name)",
      ...patternProblems(),
      "notes/session.txt:5: private-term: private term #2",
      `commit ${secondCommit}: private-term: private term #2`,
      `commit ${secondCommit}: ipv4: ${PUBLIC_ADDRESS} is not a sample address`,
      `${SUMMARY}10 problems in 5 files and 1 commit message; private terms: 2 checked${HINT}`,
      "",
    ]);
    assert.doesNotMatch(result.stdout, /zebra|orchid|quill/i);
    assert.equal(result.status, 1);
  });

  test("checks the commit messages of the given range only", () => {
    const result = check(["--root", dirty, "--terms", termsFile, "--commits", firstCommit]);
    assert.doesNotMatch(result.stdout, /^commit /m);
    assert.match(result.stdout, / in 5 files and 1 commit message; /);
    assert.equal(result.status, 1);
  });

  test("skips the private terms without a list, and says so", () => {
    const result = check(["--root", dirty]);
    assert.deepEqual(result.stdout.split("\n"), [
      ...patternProblems(),
      `${SUMMARY}5 problems in 5 files; private terms: not checked${HINT}`,
      "",
    ]);
    assert.equal(result.status, 1);
  });

  test("takes the terms from CRETLINK_PRIVATE_TERMS when --terms is absent", () => {
    const listed = check(["--root", dirty], { CRETLINK_PRIVATE_TERMS: "zebra-orchid\n" });
    const lines = listed.stdout.split("\n");
    const summary = `${SUMMARY}8 problems in 5 files; private terms: 1 checked${HINT}`;
    assert.ok(lines.includes("capture.bin: private-term: private term #1 (binary file)"), listed.stdout);
    assert.ok(lines.includes(summary), listed.stdout);
    assert.doesNotMatch(listed.stdout, /zebra|orchid/i);
    assert.equal(listed.status, 1);

    const empty = check(["--root", dirty], { CRETLINK_PRIVATE_TERMS: "" });
    assert.match(empty.stdout, /; private terms: not checked; /);

    // "session" is in a file name; the list of --terms takes its place.
    const both = check(["--root", dirty, "--terms", termsFile], {
      CRETLINK_PRIVATE_TERMS: "session",
    });
    assert.match(both.stdout, /; private terms: 2 checked; /);
    assert.doesNotMatch(both.stdout, /private term #1/);
  });

  test("checks a link by the path it points to", { skip: process.platform === "win32" }, () => {
    symlinkSync("/home/" + STRANGER + "/notes", join(dirty, "shortcut"));
    try {
      git(dirty, "add", "shortcut");
      const result = check(["--root", dirty]);
      const expected =
        "shortcut:1: profile-path: /home/" + STRANGER + " is not the profile of a sample account";
      assert.ok(result.stdout.split("\n").includes(expected), result.stdout);
      assert.equal(result.status, 1);
    } finally {
      git(dirty, "rm", "--quiet", "--cached", "--ignore-unmatch", "shortcut");
      rmSync(join(dirty, "shortcut"));
    }
  });

  test("passes over a tracked file that is missing from the working tree", () => {
    rmSync(join(clean, "README.md"));
    try {
      const result = check(["--root", clean]);
      assert.equal(
        result.stdout,
        `${SUMMARY}no private data in 1 file; private terms: not checked\n`
      );
      assert.equal(result.status, 0);
    } finally {
      git(clean, "checkout", "--", "README.md");
    }
  });

  test("exits with 2 and the usage when it cannot run", () => {
    for (const args of [
      ["--unknown"],
      ["extra"],
      ["--terms"],
      ["--root", dirty, "--terms", join(work, "missing.txt")],
      ["--root", join(work, "missing")],
      ["--root", work], // a folder outside any repository
      ["--root", dirty, "--commits", "no-such-branch..HEAD"],
      ["--root", dirty, "--commits=--all"], // an option of git is not a range
    ]) {
      const result = check(args);
      assert.equal(result.status, 2, args.join(" "));
      assert.equal(result.stdout, "", args.join(" "));
      assert.match(
        result.stderr,
        /^check-private-data: .+\nUsage: node scripts\/check-private-data\.mjs /s,
        args.join(" ")
      );
    }
  });

  test("prints the usage for --help", () => {
    const result = check(["--help"]);
    assert.match(result.stdout, /^Usage: node scripts\/check-private-data\.mjs /);
    assert.equal(result.status, 0);
  });
});

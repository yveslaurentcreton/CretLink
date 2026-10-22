// A local semantic-release plugin (ST-M006-014): refuse to publish when the
// version semantic-release decided differs from the one the release workflow
// computed with `scripts/next-version.mjs` and stamped into the binaries and
// archives. Without CRETLINK_EXPECTED_VERSION (a local dry run) it does nothing.

module.exports = {
  verifyRelease(_pluginConfig, { nextRelease, logger }) {
    const expected = process.env.CRETLINK_EXPECTED_VERSION;
    if (!expected) {
      return;
    }
    if (nextRelease.version !== expected) {
      throw new Error(
        `semantic-release decided ${nextRelease.version}, but the artifacts were built as ${expected}; nothing is published.`
      );
    }
    logger.log(`Release version ${expected} matches the built artifacts.`);
  },
};

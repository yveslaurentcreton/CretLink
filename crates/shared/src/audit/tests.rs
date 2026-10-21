//! Deterministic audit-module tests (ST-M002-007, ADR-0008).
//!
//! Every test uses an injected [`FixedClock`] and a fixed [`SessionId`], so the
//! serialized bytes — and therefore the golden hash chain — are a pure function
//! of the inputs. Tests may `unwrap`/`panic`; library code never does.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::Cursor;

use cretlink_protocol::SessionId;

use super::{
    AUDIT_SCHEMA_VERSION, Actor, AuditArgs, AuditFileMeta, AuditRecord, AuditValue, AuditWriter,
    ChainBreak, ChainReport, FixedClock, MAX_FIELD_BYTES, MIN_DIGEST_LEN, Outcome, RetentionPolicy,
    ZERO_PREV, files_to_prune, format_rfc3339_millis, redact, redact_no_digest, session_file_name,
    truncate_field, verify_chain, verify_sealed,
};

/// A fixed instant: 2026-08-09T00:34:56.789Z (arbitrary but frozen, so the
/// golden chain hashes are reproducible).
const FIXED_MILLIS: i64 = 1_786_235_696_789;

/// The literal secret used to prove structural exclusion. It must never appear
/// in any serialized audit bytes.
const SECRET: &str = "SESSION-APPROVAL-CODE-871234-bearer-tok-DEADBEEF";

fn fixed_session() -> SessionId {
    // Build a fixed SessionId with no `uuid` dependency by deserializing the
    // transparent UUID string form.
    serde_json::from_str("\"2b8f1d3c-1111-4222-8333-444455556666\"").unwrap()
}

fn fixed_clock() -> FixedClock {
    FixedClock::from_unix_millis(FIXED_MILLIS)
}

/// Builds a fixed multi-record session log and returns its exact bytes.
///
/// `close` consumes the writer, so the buffer is threaded out through a
/// `Cursor` we own. Record sequence (seq in parentheses):
/// header/session_opened (0), discovered (1), pairing_started (2),
/// code_confirmed (3), consent_granted (4), state_changed (5),
/// exec-with-redacted-output (6), revoked (7), cleanup (8),
/// session_closed (9).
fn capture_log() -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = Cursor::new(&mut buf);
        let mut writer = AuditWriter::new_session(cursor, fixed_session(), fixed_clock()).unwrap();
        writer
            .append(
                Actor::Mcp,
                "discovered",
                AuditArgs::new().with("target", AuditValue::String("SIMULATED-TARGET".to_string())),
                Outcome::Ok,
            )
            .unwrap();
        writer
            .append(
                Actor::Console,
                "pairing_started",
                AuditArgs::new(),
                Outcome::Ok,
            )
            .unwrap();
        writer
            .append(
                Actor::Console,
                "code_confirmed",
                AuditArgs::new(),
                Outcome::Ok,
            )
            .unwrap();
        writer
            .append(
                Actor::Console,
                "consent_granted",
                AuditArgs::new().with("scope", AuditValue::String("session".to_string())),
                Outcome::Ok,
            )
            .unwrap();
        writer
            .append(
                Actor::Mcp,
                "state_changed",
                AuditArgs::new()
                    .with("from", AuditValue::String("paired".to_string()))
                    .with("to", AuditValue::String("controlling".to_string())),
                Outcome::Ok,
            )
            .unwrap();
        writer
            .append(
                Actor::Mcp,
                "exec",
                AuditArgs::new().with(
                    "output",
                    redact("command output may contain secrets", SECRET.as_bytes()),
                ),
                Outcome::Ok,
            )
            .unwrap();
        writer
            .append(Actor::Console, "revoked", AuditArgs::new(), Outcome::Ok)
            .unwrap();
        writer
            .append(
                Actor::System,
                "cleanup",
                AuditArgs::new().with("residual_count", AuditValue::Int(0)),
                Outcome::Ok,
            )
            .unwrap();
        writer.close().unwrap();
    }
    buf
}

fn parse_lines(bytes: &[u8]) -> Vec<AuditRecord> {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<AuditRecord>(l).unwrap())
        .collect()
}

#[test]
fn clock_formats_rfc3339_millis() {
    assert_eq!(
        format_rfc3339_millis(FIXED_MILLIS),
        "2026-08-09T00:34:56.789Z"
    );
    // Epoch and a pre-epoch instant round-trip through the calendar math.
    assert_eq!(format_rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(format_rfc3339_millis(-1), "1969-12-31T23:59:59.999Z");
}

#[test]
fn record_round_trips() {
    let mut digests = BTreeMap::new();
    digests.insert("file".to_string(), "ab".repeat(32));
    let record = AuditRecord {
        v: AUDIT_SCHEMA_VERSION,
        seq: 7,
        ts: format_rfc3339_millis(FIXED_MILLIS),
        session: fixed_session(),
        actor: Actor::Console,
        action: "state_changed".to_string(),
        args: AuditArgs::new().with("to", AuditValue::String("controlling".to_string())),
        outcome: Outcome::Ok,
        digests,
        duration_ms: Some(42),
        prev: ZERO_PREV.to_string(),
    };
    let json = serde_json::to_string(&record).unwrap();
    let back: AuditRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(record, back);
}

#[test]
fn record_rejects_unknown_fields() {
    let line = &parse_lines(&capture_log())[0];
    let mut value = serde_json::to_value(line).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("rogue".to_string(), serde_json::json!(true));
    let reparsed = serde_json::from_value::<AuditRecord>(value);
    assert!(
        reparsed.is_err(),
        "deny_unknown_fields must reject extra keys"
    );
}

#[test]
fn header_uses_zero_prev_and_seq_zero() {
    let records = parse_lines(&capture_log());
    let header = &records[0];
    assert_eq!(header.seq, 0);
    assert_eq!(header.prev, ZERO_PREV);
    assert_eq!(header.actor, Actor::System);
    assert_eq!(header.action, "session_opened");
}

#[test]
fn seq_is_monotonic_from_zero() {
    let records = parse_lines(&capture_log());
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "seq must equal line index");
    }
}

#[test]
fn close_seals_total_record_count() {
    let records = parse_lines(&capture_log());
    let last = records.last().unwrap();
    assert_eq!(last.action, "session_closed");
    assert_eq!(last.actor, Actor::System);
    let count = match last.args.0.get("record_count") {
        Some(AuditValue::Int(n)) => *n,
        other => panic!("expected record_count int, got {other:?}"),
    };
    assert_eq!(count as usize, records.len());
}

#[test]
fn golden_chain_has_frozen_prev_hashes() {
    let log = capture_log();
    let records = parse_lines(&log);

    // Frozen chain: fixed clock + fixed session id make these SHA-256 links a
    // pure function of the record bytes. If serialization ever changes, this
    // breaks on purpose (ADR-0008 couples the chain to serde_json output).
    let expected_prevs: [&str; 10] = [
        ZERO_PREV,
        "bb3a2ff8f3db5be2ca5daae5ee9f3637ef301ce469fa0a9779c61928cd43207e",
        "0b405833e0554b657628605b5fe480f8dbc2a5677a7abb1d30c144a000a53cab",
        "63437892f6a4c1d6342393f8500d8711474cbccff2ca71ab9445df87d82a12d5",
        "3be1b939ca5f58fd547ca9cf1e108b50d1b182ad7b068ea39d66d91ac5be325d",
        "3d7f6121d219bec2a40c256ed0348fa489ef78f54292b2bdef65e4209ba0b086",
        "f764f05c1657f87acb18a9a3f0b7aaaf60af26b83678faeaafc2140756bd4c3b",
        "29379869c7e514210c543bd16d58ae4a3bebb300a2102ab33ad57f0dc94dc154",
        "2188ebea5b591d442d390024cae0073a7d52b959574436a27358886a758fee42",
        "622b0bc177afdedee8b84d7068bf541345892ff1fa1c256820ea29a1a1efa22d",
    ];
    let actual: Vec<String> = records.iter().map(|r| r.prev.clone()).collect();
    assert_eq!(actual.len(), expected_prevs.len());
    for (index, expected) in expected_prevs.iter().enumerate() {
        assert_eq!(&actual[index], expected, "prev mismatch at line {index}");
    }
    // The whole log must also verify end to end under the same recomputation.
    assert_eq!(
        verify_chain(Cursor::new(log)).unwrap(),
        ChainReport::Valid { records: 10 }
    );
}

#[test]
fn verify_chain_accepts_intact_log() {
    let log = capture_log();
    match verify_chain(Cursor::new(log)).unwrap() {
        ChainReport::Valid { records } => assert_eq!(records, 10),
        other => panic!("expected Valid, got {other:?}"),
    }
}

#[test]
fn verify_chain_detects_tampered_line() {
    let log = capture_log();
    let text = String::from_utf8(log).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // Tamper the content of a middle line (line 5, state_changed) without
    // touching its `prev`. `controlling` appears only in that record's args, so
    // the line still parses; the break surfaces at the *next* line, whose
    // stored `prev` no longer matches the tampered line's recomputed hash.
    lines[5] = lines[5].replace("controlling", "compromised");
    let tampered = format!("{}\n", lines.join("\n"));
    match verify_chain(Cursor::new(tampered.into_bytes())).unwrap() {
        ChainReport::Broken {
            line,
            reason: ChainBreak::PrevMismatch { .. },
        } => assert_eq!(line, 6),
        other => panic!("expected PrevMismatch at line 6, got {other:?}"),
    }
}

#[test]
fn verify_chain_detects_bad_header_prev() {
    let log = capture_log();
    let text = String::from_utf8(log).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[0] = lines[0].replace(ZERO_PREV, &"1".repeat(64));
    let tampered = format!("{}\n", lines.join("\n"));
    match verify_chain(Cursor::new(tampered.into_bytes())).unwrap() {
        ChainReport::Broken {
            line: 0,
            reason: ChainBreak::BadHeaderPrev { .. },
        } => {}
        other => panic!("expected BadHeaderPrev at line 0, got {other:?}"),
    }
}

#[test]
fn verify_chain_detects_malformed_line() {
    let mut log = capture_log();
    log.extend_from_slice(b"{ not valid json\n");
    match verify_chain(Cursor::new(log)).unwrap() {
        ChainReport::Broken {
            line: 10,
            reason: ChainBreak::Malformed { .. },
        } => {}
        other => panic!("expected Malformed at line 10, got {other:?}"),
    }
}

#[test]
fn verify_chain_detects_seq_gap() {
    // Rewrite only the `seq` of line 5 (state_changed), leaving its `prev`
    // intact: the prev check still passes, so verification reaches — and trips —
    // the seq check on the same line. This exercises the `SeqGap` branch.
    let log = capture_log();
    let text = String::from_utf8(log).unwrap();
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    assert!(lines[5].contains("\"seq\":5"));
    lines[5] = lines[5].replace("\"seq\":5", "\"seq\":99");
    let tampered = format!("{}\n", lines.join("\n"));
    match verify_chain(Cursor::new(tampered.into_bytes())).unwrap() {
        ChainReport::Broken {
            line: 5,
            reason:
                ChainBreak::SeqGap {
                    expected: 5,
                    found: 99,
                },
        } => {}
        other => panic!("expected SeqGap at line 5, got {other:?}"),
    }
}

#[test]
fn verify_chain_tolerates_trailing_blank_but_rejects_interior_blank() {
    let log = capture_log();

    // A single extra newline at true EOF is tolerated.
    let mut trailing = log.clone();
    trailing.push(b'\n');
    assert_eq!(
        verify_chain(Cursor::new(trailing)).unwrap(),
        ChainReport::Valid { records: 10 }
    );

    // A blank line inserted before the end is corruption, reported at its
    // physical position rather than silently skipped.
    let text = String::from_utf8(log).unwrap();
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    lines.insert(3, String::new());
    let with_blank = format!("{}\n", lines.join("\n"));
    match verify_chain(Cursor::new(with_blank.into_bytes())).unwrap() {
        ChainReport::Broken {
            line: 3,
            reason: ChainBreak::Malformed { .. },
        } => {}
        other => panic!("expected Malformed at blank line 3, got {other:?}"),
    }
}

#[test]
fn verify_sealed_accepts_a_cleanly_closed_log() {
    let log = capture_log();
    assert_eq!(
        verify_sealed(Cursor::new(log)).unwrap(),
        ChainReport::Valid { records: 10 }
    );
}

#[test]
fn verify_sealed_detects_a_truncated_tail() {
    let log = capture_log();
    let text = String::from_utf8(log).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();

    // Drop the trailing session_closed seal. The remaining 9 records still form
    // a valid *chain*, so plain verify_chain is fooled into reporting Valid...
    let truncated = format!("{}\n", lines[..lines.len() - 1].join("\n"));
    assert_eq!(
        verify_chain(Cursor::new(truncated.clone().into_bytes())).unwrap(),
        ChainReport::Valid { records: 9 }
    );
    // ...but verify_sealed flags the missing seal.
    assert_eq!(
        verify_sealed(Cursor::new(truncated.into_bytes())).unwrap(),
        ChainReport::Unsealed { records: 9 }
    );

    // Dropping two records is likewise reported as unsealed.
    let shorter = format!("{}\n", lines[..lines.len() - 2].join("\n"));
    assert_eq!(
        verify_sealed(Cursor::new(shorter.into_bytes())).unwrap(),
        ChainReport::Unsealed { records: 8 }
    );
}

#[test]
fn verify_sealed_detects_a_forged_record_count() {
    // Editing only the seal's record_count keeps the chain valid (nothing links
    // past the last line) but breaks the sealed count.
    let log = capture_log();
    let text = String::from_utf8(log).unwrap();
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let last = lines.len() - 1;
    assert!(lines[last].contains("\"record_count\":{\"int\":10}"));
    lines[last] = lines[last].replace(
        "\"record_count\":{\"int\":10}",
        "\"record_count\":{\"int\":99}",
    );
    let forged = format!("{}\n", lines.join("\n"));
    // The chain itself still links (the seal is the last line).
    assert_eq!(
        verify_chain(Cursor::new(forged.clone().into_bytes())).unwrap(),
        ChainReport::Valid { records: 10 }
    );
    match verify_sealed(Cursor::new(forged.into_bytes())).unwrap() {
        ChainReport::Broken {
            line: 9,
            reason:
                ChainBreak::CountMismatch {
                    expected: 10,
                    found: Some(99),
                },
        } => {}
        other => panic!("expected CountMismatch at line 9, got {other:?}"),
    }
}

#[test]
fn redacted_secret_renders_as_marker_and_never_appears_in_bytes() {
    let log = capture_log();

    // Byte-scan: the raw secret must not appear anywhere in the serialized log.
    let haystack = String::from_utf8(log.clone()).unwrap();
    assert!(
        !haystack.contains(SECRET),
        "the raw secret leaked into the audit bytes"
    );
    assert!(
        haystack.contains("[REDACTED]") || haystack.contains("\"redacted\""),
        "the redacted marker should be present in the log"
    );

    // The captured value is only representable as a Redacted marker.
    let records = parse_lines(&log);
    let exec = records.iter().find(|r| r.action == "exec").unwrap();
    match exec.args.0.get("output") {
        Some(value @ AuditValue::Redacted { original_len, .. }) => {
            assert_eq!(*original_len, SECRET.len());
            assert_eq!(value.to_string(), "[REDACTED]");
        }
        other => panic!("expected Redacted output, got {other:?}"),
    }
}

#[test]
fn secret_is_representable_only_as_marker() {
    // `redact` is the only entry point that accepts raw secret bytes, and it
    // yields a marker — the digest of the secret, never the secret itself.
    let value = redact("api token", SECRET.as_bytes());
    let json = serde_json::to_string(&value).unwrap();
    assert!(!json.contains(SECRET));
    match value {
        AuditValue::Redacted {
            original_len,
            sha256: Some(_),
            ..
        } => assert_eq!(original_len, SECRET.len()),
        other => panic!("expected Redacted marker, got {other:?}"),
    }
}

#[test]
fn low_entropy_redaction_omits_the_brute_forceable_digest() {
    // A 6-digit Session Approval Code has ~20 bits of entropy: an unsalted
    // SHA-256 of it is recoverable by hashing all 10^6 candidates, so neither
    // redaction path may attach a correlation digest (REQ-S-017).
    let code = "871234";
    assert!(code.len() < MIN_DIGEST_LEN);
    let auto = redact("session approval code", code.as_bytes());
    let explicit = redact_no_digest("session approval code", code.len());

    for value in [&auto, &explicit] {
        match value {
            AuditValue::Redacted {
                sha256,
                original_len,
                ..
            } => {
                assert!(
                    sha256.is_none(),
                    "low-entropy redaction must omit the digest"
                );
                assert_eq!(*original_len, code.len());
            }
            other => panic!("expected Redacted, got {other:?}"),
        }
    }

    // No digest is serialized at all, so *no* candidate's hash — including the
    // true code's, the exact preimage a brute-forcer searches for — can appear.
    // Omitting the digest is strictly stronger than making an exhaustive 10^6
    // match fail, so the absence is asserted directly.
    let serialized = serde_json::to_string(&auto).unwrap();
    assert!(!serialized.contains("sha256"));
    assert!(!serialized.contains(&super::sha256_hex(code.as_bytes())));
}

#[test]
fn high_entropy_redaction_keeps_its_correlation_digest() {
    // A long, high-entropy value (>= MIN_DIGEST_LEN) still gets a digest: it is
    // safe there and lets a reviewer match occurrences across the log.
    assert!(SECRET.len() >= MIN_DIGEST_LEN);
    match redact("bearer token", SECRET.as_bytes()) {
        AuditValue::Redacted {
            sha256: Some(hex), ..
        } => assert_eq!(hex, super::sha256_hex(SECRET.as_bytes())),
        other => panic!("expected a digested Redacted marker, got {other:?}"),
    }
}

#[test]
fn truncate_field_caps_at_32_kib() {
    // Exactly at the cap stays a plain string.
    let at_cap = "a".repeat(MAX_FIELD_BYTES);
    assert!(matches!(truncate_field(&at_cap), AuditValue::String(_)));

    // Over the cap becomes a Truncated marker keeping a bounded prefix.
    let over = "a".repeat(MAX_FIELD_BYTES + 10);
    match truncate_field(&over) {
        AuditValue::Truncated { kept, original_len } => {
            assert_eq!(kept.len(), MAX_FIELD_BYTES);
            assert_eq!(original_len, MAX_FIELD_BYTES + 10);
        }
        other => panic!("expected Truncated, got {other:?}"),
    }
}

#[test]
fn truncate_field_respects_char_boundaries() {
    // Fill up to one byte below the cap, then a 3-byte char straddles the cap.
    let mut s = "a".repeat(MAX_FIELD_BYTES - 1);
    s.push('☃'); // 3 bytes, crosses the 32 KiB boundary
    match truncate_field(&s) {
        AuditValue::Truncated { kept, original_len } => {
            // Must back off to the char boundary, never split the snowman.
            assert_eq!(kept.len(), MAX_FIELD_BYTES - 1);
            assert!(kept.is_char_boundary(kept.len()));
            assert_eq!(original_len, MAX_FIELD_BYTES - 1 + 3);
        }
        other => panic!("expected Truncated, got {other:?}"),
    }
}

#[test]
fn write_error_is_surfaced_not_swallowed() {
    // A sink that always fails must make `new_session` return an error rather
    // than silently dropping the header (REQ-S-017).
    struct FailingSink;
    impl std::io::Write for FailingSink {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let result = AuditWriter::new_session(FailingSink, fixed_session(), fixed_clock());
    assert!(result.is_err(), "a write failure must surface as an error");
}

#[test]
fn session_file_name_is_filesystem_safe() {
    let name = session_file_name(FIXED_MILLIS, fixed_session());
    assert_eq!(
        name,
        "20260809T003456789Z-2b8f1d3c-1111-4222-8333-444455556666.jsonl"
    );
    assert!(
        !name.contains(':'),
        "file name must not contain colons (Windows)"
    );
    assert!(name.ends_with(".jsonl"));
}

#[test]
fn files_to_prune_age_policy() {
    let now = FIXED_MILLIS;
    let day = 86_400_000_i64;
    let files = vec![
        AuditFileMeta {
            name: "old-a.jsonl".to_string(),
            size_bytes: 100,
            modified_unix_millis: now - 100 * day,
        },
        AuditFileMeta {
            name: "recent-b.jsonl".to_string(),
            size_bytes: 100,
            modified_unix_millis: now - 10 * day,
        },
        AuditFileMeta {
            name: "edge-c.jsonl".to_string(),
            size_bytes: 100,
            modified_unix_millis: now - 91 * day,
        },
    ];
    // Age only; disable the size cap.
    let policy = RetentionPolicy {
        max_age_days: 90,
        max_total_bytes: 0,
    };
    let prune = files_to_prune(&files, now, &policy);
    // Oldest first: a (100d) then c (91d); b (10d) is retained.
    assert_eq!(
        prune,
        vec!["old-a.jsonl".to_string(), "edge-c.jsonl".to_string()]
    );
}

#[test]
fn files_to_prune_size_policy() {
    let now = FIXED_MILLIS;
    let files = vec![
        AuditFileMeta {
            name: "oldest.jsonl".to_string(),
            size_bytes: 100,
            modified_unix_millis: 1_000,
        },
        AuditFileMeta {
            name: "middle.jsonl".to_string(),
            size_bytes: 100,
            modified_unix_millis: 2_000,
        },
        AuditFileMeta {
            name: "newest.jsonl".to_string(),
            size_bytes: 100,
            modified_unix_millis: 3_000,
        },
    ];
    // Total 300 > 250; prune oldest-first until <= 250 → just the oldest.
    let policy = RetentionPolicy {
        max_age_days: 0,
        max_total_bytes: 250,
    };
    let prune = files_to_prune(&files, now, &policy);
    assert_eq!(prune, vec!["oldest.jsonl".to_string()]);
}

#[test]
fn files_to_prune_combines_age_then_size() {
    let now = FIXED_MILLIS;
    let day = 86_400_000_i64;
    let files = vec![
        AuditFileMeta {
            name: "expired.jsonl".to_string(),
            size_bytes: 100,
            modified_unix_millis: now - 200 * day,
        },
        AuditFileMeta {
            name: "big-old.jsonl".to_string(),
            size_bytes: 300,
            modified_unix_millis: now - 5 * day,
        },
        AuditFileMeta {
            name: "fresh.jsonl".to_string(),
            size_bytes: 300,
            modified_unix_millis: now - day,
        },
    ];
    // Age prunes `expired`. Remaining size = 600 > 400, so size pruning removes
    // the oldest remaining (`big-old`, 300) → 300 <= 400, stop.
    let policy = RetentionPolicy {
        max_age_days: 90,
        max_total_bytes: 400,
    };
    let prune = files_to_prune(&files, now, &policy);
    assert_eq!(
        prune,
        vec!["expired.jsonl".to_string(), "big-old.jsonl".to_string()]
    );
}

#[test]
fn default_retention_matches_adr() {
    let policy = RetentionPolicy::default();
    assert_eq!(policy.max_age_days, 90);
    assert_eq!(policy.max_total_bytes, 1024 * 1024 * 1024);
}

#[test]
fn actor_terminal_is_additive_and_existing_variants_serialize_unchanged() {
    // ST-M003-006 freezes the actor vocabulary once: `terminal` is reserved
    // for ST-M003-009 and its addition must not change the serialized form of
    // any existing variant (the golden-chain vectors above depend on it).
    assert_eq!(
        serde_json::to_string(&Actor::Terminal).unwrap(),
        "\"terminal\""
    );
    assert_eq!(serde_json::to_string(&Actor::Mcp).unwrap(), "\"mcp\"");
    assert_eq!(
        serde_json::to_string(&Actor::Console).unwrap(),
        "\"console\""
    );
    assert_eq!(serde_json::to_string(&Actor::Agent).unwrap(), "\"agent\"");
    assert_eq!(serde_json::to_string(&Actor::System).unwrap(), "\"system\"");
    let back: Actor = serde_json::from_str("\"terminal\"").unwrap();
    assert_eq!(back, Actor::Terminal);
}

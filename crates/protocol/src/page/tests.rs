//! Tests of the folder page that fits one record (ST-M010-005).
//!
//! The size a page counts is compared with the real encoding of the answer,
//! so the bound is proven against the wire form and not against an estimate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{ByteCount, DirPage};
use crate::{FileEntry, FileKind, Message, OpResult, RequestId, SessionId, limits};

/// An entry named `name` with ordinary facts.
fn entry(name: &str) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        kind: FileKind::File,
        len: 1234,
        modified_ms: Some(1_700_000_000_000),
        readonly: false,
    }
}

/// The largest entry the protocol carries: a name at its bound in bytes, every
/// one of them a character that the encoding writes as six, and every other
/// field at its longest.
fn largest_entry() -> FileEntry {
    FileEntry {
        name: "\u{1}".repeat(limits::MAX_NAME_BYTES),
        kind: FileKind::Symlink,
        len: u64::MAX,
        modified_ms: Some(u64::MAX),
        readonly: false,
    }
}

/// The answer that carries `page`, as it is encoded for the wire, without the
/// validation of its fields.
fn encoded(page: &DirPage, id: u64, end: bool) -> Vec<u8> {
    serde_json::to_vec(&Message::Response {
        id: RequestId(id),
        session: SessionId::new(),
        result: page.clone().into_result(end),
    })
    .expect("the answer is encoded")
}

/// A name of `chars` characters of three bytes each, told apart by `n`, which
/// is written in digits of three bytes too.
fn three_byte_name(n: usize, chars: usize) -> String {
    let digits: String = format!("{n:03}")
        .chars()
        .map(|digit| {
            let value = digit.to_digit(10).expect("a decimal digit");
            // The fullwidth digits U+FF10 to U+FF19.
            char::from_u32(0xFF10 + value).expect("a fullwidth digit")
        })
        .collect();
    let name = format!("{digits}{}", "\u{8A9E}".repeat(chars - 3));
    assert_eq!(name.chars().count(), chars);
    assert_eq!(name.len(), 3 * chars);
    name
}

/// What a page counts is the length of the answer as it is really encoded,
/// with the longest request identifier and `end` at `false`: after every
/// entry, for names that the encoding writes longer and for names it writes as
/// they are.
#[test]
fn the_counted_size_is_the_size_of_the_real_encoding() {
    let names = [
        "a.txt".to_owned(),
        "with \"quotes\" and \\backslashes\\".to_owned(),
        "line\nbreak\ttab\u{1}\u{1f}".to_owned(),
        "\u{2028}separator\u{7f}delete".to_owned(),
        three_byte_name(7, 250),
        "\u{1F600}".repeat(60),
        String::new(),
        "\"".repeat(limits::MAX_NAME_BYTES),
    ];
    let mut page = DirPage::new();
    assert_eq!(page.encoded_len(), encoded(&page, u64::MAX, false).len());
    for (n, name) in names.iter().enumerate() {
        let mut added = entry(name);
        // Every shape of the other fields takes its turn.
        added.kind = [
            FileKind::File,
            FileKind::Dir,
            FileKind::Symlink,
            FileKind::Other,
        ][n % 4];
        added.len = [0, 9, u64::MAX][n % 3];
        added.modified_ms = [None, Some(0), Some(u64::MAX)][n % 3];
        added.readonly = n % 2 == 0;
        page.push(added).expect("the entry fits");
        assert_eq!(
            page.encoded_len(),
            encoded(&page, u64::MAX, false).len(),
            "after {name:?}"
        );
    }
    assert_eq!(page.len(), names.len());
}

/// The answer that is really sent has another request identifier and may be
/// the last page. It is never longer than what the page counted.
#[test]
fn the_real_answer_is_never_longer_than_counted() {
    let mut page = DirPage::new();
    for n in 0..40 {
        page.push(entry(&three_byte_name(n, 250)))
            .expect("the entry fits");
    }
    let mut ids = vec![0, 1, 7, 9, u64::MAX - 1, u64::MAX];
    ids.extend((1..20).map(|digits| 10u64.pow(digits)));
    for id in ids {
        for end in [false, true] {
            let real = encoded(&page, id, end).len();
            assert!(
                real <= page.encoded_len(),
                "id {id}, end {end}: {real} bytes, counted {}",
                page.encoded_len()
            );
        }
    }
}

/// Names of 250 characters of three bytes each: the page ends before the
/// entry that would make the answer larger than one record, long before it
/// holds as many entries as a page may. The entry it hands back is the one
/// that did not fit, and with it the answer would not have fitted.
#[test]
fn a_page_ends_before_the_entry_that_would_not_fit() {
    let mut page = DirPage::new();
    let mut refused = None;
    for n in 0..limits::MAX_DIR_PAGE {
        if let Err(back) = page.push(entry(&three_byte_name(n, 250))) {
            refused = Some((n, back));
            break;
        }
    }
    let (n, back) = refused.expect("the page ends before it is full by count");
    assert_eq!(back, entry(&three_byte_name(n, 250)), "handed back whole");
    assert_eq!(page.len(), n, "the entries before it are in the page");
    assert!(page.len() > 1 && page.len() < limits::MAX_DIR_PAGE);

    let sent = Message::Response {
        id: RequestId(u64::MAX),
        session: SessionId::new(),
        result: page.clone().into_result(false),
    }
    .encode()
    .expect("the page is within the bounds of the wire");
    assert!(
        sent.len() <= limits::MAX_RECORD_BYTES,
        "{} bytes",
        sent.len()
    );
    assert_eq!(sent.len(), page.encoded_len());

    // The page did not end early: with that entry the answer is too large.
    let one_more = serde_json::to_vec(&back)
        .expect("the entry is encoded")
        .len();
    assert!(page.encoded_len() + 1 + one_more > limits::MAX_RECORD_BYTES);
    // And a page that ended stays as it is: nothing is added after that.
    let before = page.encoded_len();
    page.push(back).expect_err("still too large");
    assert_eq!((page.len(), page.encoded_len()), (n, before));
}

/// Short names: the page ends at its number of entries, as before.
#[test]
fn a_page_of_short_names_ends_at_its_number_of_entries() {
    let mut page = DirPage::new();
    for n in 0..limits::MAX_DIR_PAGE {
        page.push(entry(&format!("file-{n:04}.txt")))
            .expect("within the number of entries of a page");
    }
    let back = page
        .push(entry("one-too-many.txt"))
        .expect_err("a page holds no more entries than that");
    assert_eq!(back.name, "one-too-many.txt");
    assert_eq!(page.len(), limits::MAX_DIR_PAGE);
    assert!(page.encoded_len() <= limits::MAX_RECORD_BYTES);
    match page.into_result(true) {
        OpResult::DirEntries { entries, end } => {
            assert_eq!(entries.len(), limits::MAX_DIR_PAGE);
            assert_eq!(entries[0].name, "file-0000.txt");
            assert!(end);
        }
        other => panic!("expected DirEntries, got {other:?}"),
    }
}

/// No character is written as more than six bytes for each byte it has, and
/// the one the largest entry is made of is written as exactly that.
#[test]
fn no_character_is_written_longer_than_six_bytes_for_one() {
    let written = |text: &str| {
        let mut counted = ByteCount(0);
        serde_json::to_writer(&mut counted, text).expect("a text is encoded");
        // Without the two quotation marks around it.
        counted.0 - 2
    };
    let mut buffer = [0u8; 4];
    for character in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
        let text = character.encode_utf8(&mut buffer);
        assert!(
            written(text) <= 6 * text.len(),
            "U+{:04X} is written as {} bytes",
            u32::from(character),
            written(text)
        );
    }
    assert_eq!(written("\u{1}"), 6);
}

/// The bound test for the largest single entry: a name at its bound, made of
/// the characters that the encoding writes longest, with every other field at
/// its longest. It fits an empty page, so a page that says more follows always
/// holds at least one entry; and the answer of a stat, which carries one
/// entry, fits as well.
#[test]
fn the_largest_single_entry_always_fits() {
    let largest = largest_entry();
    assert_eq!(largest.name.len(), limits::MAX_NAME_BYTES);

    let mut page = DirPage::new();
    page.push(largest.clone())
        .expect("the largest entry fits an empty page");
    let sent = Message::Response {
        id: RequestId(u64::MAX),
        session: SessionId::new(),
        result: page.clone().into_result(false),
    }
    .encode()
    .expect("the largest entry is within the bounds of the wire");
    assert_eq!(sent.len(), page.encoded_len());
    assert!(
        sent.len() <= limits::MAX_RECORD_BYTES,
        "{} bytes",
        sent.len()
    );
    // It is a small part of a record: a page of such entries still holds
    // several.
    assert!(
        sent.len() * 8 < limits::MAX_RECORD_BYTES,
        "{} bytes",
        sent.len()
    );

    let stat = Message::Response {
        id: RequestId(u64::MAX),
        session: SessionId::new(),
        result: OpResult::PathStat {
            entry: Some(largest),
            partial_len: Some(u64::MAX),
        },
    }
    .encode()
    .expect("the answer of a stat is within the bounds of the wire");
    assert!(
        stat.len() <= limits::MAX_RECORD_BYTES,
        "{} bytes",
        stat.len()
    );
}

/// A page of the largest entries holds as many as fit and no more, and its
/// answer fits one record.
#[test]
fn a_page_of_the_largest_entries_fits_one_record() {
    let mut page = DirPage::new();
    let mut taken = 0;
    while page.push(largest_entry()).is_ok() {
        taken += 1;
        assert!(taken <= limits::MAX_DIR_PAGE, "a page ends");
    }
    assert!(taken >= 8, "{taken} entries");
    assert!(encoded(&page, u64::MAX, false).len() <= limits::MAX_RECORD_BYTES);
}

/// An entry that no record can carry — no file system the agent runs on has
/// such a name — is handed back by an empty page too, and the page stays
/// empty.
#[test]
fn an_entry_that_no_record_can_carry_is_handed_back() {
    let mut page = DirPage::new();
    let empty = page.encoded_len();
    let back = page
        .push(entry(&"n".repeat(2 * limits::MAX_RECORD_BYTES)))
        .expect_err("larger than a record on its own");
    assert_eq!(back.name.len(), 2 * limits::MAX_RECORD_BYTES);
    assert!(page.is_empty());
    assert_eq!(page.encoded_len(), empty);
}

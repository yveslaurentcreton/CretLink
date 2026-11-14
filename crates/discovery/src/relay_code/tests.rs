//! Tests of the relay code (ST-M007-012): frozen vectors, the tolerance on
//! input, the checksum, and how one code field tells the formats apart.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use super::{RELAY_CODE_LEN, RELAY_CODE_SEED_LEN, RelayCode, crc12, payload68};
use crate::code::ALPHABET;
use crate::{ConnectionCode, ManualEntry, ManualEntryError, TargetSource, parse_entry};

/// FROZEN: changing one of these is a change of the code format. Derived
/// independently of this crate, by polynomial division.
const VECTORS: [([u8; RELAY_CODE_SEED_LEN], &str); 4] = [
    (
        [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07],
        "4001-081G-8186-0ZGQ",
    ),
    ([0x00; RELAY_CODE_SEED_LEN], "4000-0000-0000-03BJ"),
    ([0xff; RELAY_CODE_SEED_LEN], "5ZZZ-ZZZZ-ZZZZ-ZW75"),
    (
        [0x8f, 0x3a, 0x51, 0xc2, 0xd9, 0x4e, 0x7b, 0x06],
        "53ST-A71D-JKKV-0VGH",
    ),
];

/// The symbols of a code, without the hyphens.
fn symbols(code: &str) -> Vec<char> {
    code.chars().filter(|ch| *ch != '-').collect()
}

/// Groups sixteen symbols the way a code is shown.
fn grouped(symbols: &[char]) -> String {
    symbols
        .chunks(4)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Renders an 80-bit value as a grouped code, bypassing the checksum.
fn raw_code(value: u128) -> String {
    let symbols: Vec<char> = (0..RELAY_CODE_LEN)
        .map(|index| ALPHABET[((value >> (75 - 5 * index as u32)) & 0x1f) as usize] as char)
        .collect();
    grouped(&symbols)
}

#[test]
fn frozen_vectors_encode_and_parse() {
    for (seed, code) in VECTORS {
        assert_eq!(RelayCode::from_seed(seed).encode(), code);
        assert_eq!(RelayCode::parse(code).unwrap().seed(), &seed, "{code}");
    }
}

#[test]
fn the_frozen_checksum_is_crc_12_with_polynomial_80f() {
    // The checksum of the first vector, from the independent derivation.
    let seed = u64::from_be_bytes(VECTORS[0].0);
    assert_eq!(crc12(payload68(RelayCode::FORMAT_VERSION, seed)), 0xe17);
    assert_eq!(crc12(payload68(RelayCode::FORMAT_VERSION, 0)), 0xd72);
}

#[test]
fn every_seed_round_trips() {
    // A fixed sequence of seeds that touches every bit position.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..2_000 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let code = RelayCode::from_seed(seed.to_be_bytes());
        let text = code.encode();
        assert_eq!(text.len(), RELAY_CODE_LEN + 3);
        assert_eq!(RelayCode::parse(&text).unwrap(), code, "{text}");
    }
}

#[test]
fn a_code_is_shown_in_four_groups_of_four() {
    let text = RelayCode::from_seed(VECTORS[3].0).encode();
    let groups: Vec<&str> = text.split('-').collect();
    assert_eq!(groups.len(), 4);
    assert!(groups.iter().all(|group| group.len() == 4), "{text}");
    // A relay code starts with 4 or 5 and a LAN code with 2 or 3: the format
    // version is the first four bits.
    assert!(text.starts_with('4') || text.starts_with('5'), "{text}");
}

#[test]
fn input_tolerates_case_spacing_and_look_alike_characters() {
    let (seed, code) = VECTORS[0];
    for typed in [
        "4001-081g-8186-0zgq",
        "4001081G81860ZGQ",
        "  4001 081G 8186 0ZGQ  ",
        "4001-081G\t8186-0ZGQ",
        // O for 0, and I and L for 1.
        "4OOI-O8LG-8l86-oZGQ",
    ] {
        assert_eq!(
            RelayCode::parse(typed).unwrap().seed(),
            &seed,
            "{typed} should read as {code}"
        );
    }
}

#[test]
fn one_mistyped_character_is_always_rejected() {
    for (_, code) in VECTORS {
        let original = symbols(code);
        for position in 0..RELAY_CODE_LEN {
            for replacement in ALPHABET {
                let replacement = replacement as char;
                if replacement == original[position] {
                    continue;
                }
                let mut typed = original.clone();
                typed[position] = replacement;
                let typed = grouped(&typed);
                assert!(
                    RelayCode::parse(&typed).is_err(),
                    "{typed} was accepted for {code}"
                );
            }
        }
    }
}

#[test]
fn two_swapped_neighbours_are_always_rejected() {
    for (_, code) in VECTORS {
        let original = symbols(code);
        for position in 0..RELAY_CODE_LEN - 1 {
            if original[position] == original[position + 1] {
                continue;
            }
            let mut typed = original.clone();
            typed.swap(position, position + 1);
            let typed = grouped(&typed);
            assert!(
                RelayCode::parse(&typed).is_err(),
                "{typed} was accepted for {code}"
            );
        }
    }
}

#[test]
fn a_typing_error_is_reported_as_a_checksum_mismatch() {
    // The last character of the first vector, changed.
    assert_eq!(
        RelayCode::parse("4001-081G-8186-0ZGR"),
        Err(ManualEntryError::ChecksumMismatch)
    );
}

#[test]
fn the_length_and_the_alphabet_are_checked_first() {
    for typed in ["", "4001", "4001-081G-8186-0ZG", "4001-081G-8186-0ZGQ-0"] {
        assert_eq!(
            RelayCode::parse(typed),
            Err(ManualEntryError::BadLength),
            "{typed:?}"
        );
    }
    // A LAN connection code has twelve characters: it is no relay code.
    assert_eq!(
        RelayCode::parse("3G58-04SB-F63B"),
        Err(ManualEntryError::BadLength)
    );
    // `U` is not in the alphabet, and neither is anything outside ASCII.
    for typed in ["4001-081G-8186-0ZGU", "4001-081G-8186-0ZG\u{e9}"] {
        assert_eq!(
            RelayCode::parse(typed),
            Err(ManualEntryError::BadChar),
            "{typed:?}"
        );
    }
}

#[test]
fn another_format_version_is_rejected_as_unsupported() {
    // Well-formed sixteen-character codes that declare version 3 and
    // version 1, with a valid checksum.
    for typed in ["6001-081G-8186-0Y5E", "2001-081G-8186-0XEW"] {
        assert_eq!(
            RelayCode::parse(typed),
            Err(ManualEntryError::UnsupportedVersion),
            "{typed}"
        );
    }
}

#[test]
fn the_checksum_is_no_secret() {
    // For one payload exactly one of the 4096 checksum fields is valid, and
    // it is the one anyone can compute: the checksum adds nothing to guess.
    let payload = payload68(RelayCode::FORMAT_VERSION, 0x0123_4567_89ab_cdef);
    let accepted: Vec<u16> = (0..4096u16)
        .filter(|field| RelayCode::parse(&raw_code((payload << 12) | u128::from(*field))).is_ok())
        .collect();
    assert_eq!(accepted, vec![crc12(payload)]);

    // What must be guessed is the seed: a code built from any guessed seed is
    // well formed, so only the 64 random bits stand between a stranger and a
    // rendezvous — and behind that, the approval-code comparison.
    for guess in [0u64, 1, 0xdead_beef, u64::MAX] {
        let text = RelayCode::from_seed(guess.to_be_bytes()).encode();
        assert!(RelayCode::parse(&text).is_ok(), "{text}");
    }
}

#[test]
fn minted_codes_differ_and_are_not_logged() {
    let first = RelayCode::generate().unwrap();
    let second = RelayCode::generate().unwrap();
    assert_ne!(first, second, "two minted seeds must not collide");
    assert_eq!(format!("{first:?}"), "RelayCode(<redacted>)");
}

#[test]
fn one_field_tells_a_lan_code_a_relay_code_and_an_address_apart() {
    // A LAN connection code.
    let lan = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), 7000);
    match parse_entry(&ConnectionCode::encode(lan)).unwrap() {
        ManualEntry::Lan(target) => {
            assert_eq!(target.addr(), SocketAddr::V4(lan));
            assert_eq!(
                target.source(),
                TargetSource::ConnectionCode { format_version: 1 }
            );
        }
        other => panic!("expected a LAN target, got {other:?}"),
    }
    // A relay code, typed loosely.
    match parse_entry("  4001 081g 8186 0zgq ").unwrap() {
        ManualEntry::Relay(code) => assert_eq!(code.seed(), &VECTORS[0].0),
        other => panic!("expected a relay code, got {other:?}"),
    }
    // A raw address.
    match parse_entry("192.168.1.20:7000").unwrap() {
        ManualEntry::Lan(target) => assert_eq!(target.source(), TargetSource::HostPort),
        other => panic!("expected a raw address, got {other:?}"),
    }
}

#[test]
fn a_mistyped_relay_code_is_never_tried_as_a_lan_code() {
    // Sixteen characters with a typing error: a checksum mismatch of the
    // relay format, not a LAN code of the wrong length.
    assert_eq!(
        parse_entry("4001-081G-8186-0ZGR"),
        Err(ManualEntryError::ChecksumMismatch)
    );
    // Neither twelve nor sixteen characters.
    assert_eq!(
        parse_entry("4001-081G-8186-0ZG"),
        Err(ManualEntryError::BadLength)
    );
    // A host name is no address literal.
    assert_eq!(
        parse_entry("relay.example.net:443"),
        Err(ManualEntryError::BadHostPort)
    );
}

//! Deterministic tests for discovery (ST-M002-003).
//!
//! Everything here is CI-safe: connection-code round trips, `is_private_addr`
//! boundaries, TXT/version parsing, and `DiscoveredConsole` construction from
//! synthetic records. The one test that needs real multicast is `#[ignore]`d
//! (see the bottom of the file) because CI runners may block it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};

use cretlink_protocol::ProtocolVersion;

use crate::advertiser::{advertise_properties_for_tests, service_info_for_tests};
use crate::browser::{instance_from_fullname_for_tests, parse_protocol_version_for_tests};
use crate::code::{crc8_for_tests, decode_symbol_for_tests, encode_symbols_for_tests};
use crate::interface::{advertisable_for_tests, prefer_routable_private_for_tests};
use crate::listen::order_candidates_for_tests;
use crate::{
    ConnectionCode, DiscoveredConsole, DiscoveryError, ListenCandidate, ManualEntryError,
    SERVICE_TYPE, TXT_KEY_NAME, TXT_KEY_VERSION, TargetSource, default_route_addr, first_choice,
    is_private_addr, listen_candidates, parse_manual_entry, private_interface_addrs,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

fn v6(s: &str) -> IpAddr {
    IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
}

/// Rebuilds the 60-bit code value for an address (mirrors `ConnectionCode`),
/// so tests can corrupt individual bits.
fn code_value(ip: Ipv4Addr, port: u16, version: u8) -> u64 {
    let payload =
        ((u64::from(version) & 0x0f) << 48) | (u64::from(u32::from(ip)) << 16) | u64::from(port);
    (payload << 8) | u64::from(crc8_for_tests(payload))
}

// ---------------------------------------------------------------------------
// is_private_addr — exhaustive boundaries
// ---------------------------------------------------------------------------

#[test]
fn is_private_addr_ipv4_boundaries() {
    // RFC1918 10/8.
    assert!(is_private_addr(v4(10, 0, 0, 1)));
    assert!(is_private_addr(v4(10, 255, 255, 255)));
    assert!(!is_private_addr(v4(9, 255, 255, 255)));
    assert!(!is_private_addr(v4(11, 0, 0, 0)));

    // RFC1918 172.16/12 boundaries.
    assert!(!is_private_addr(v4(172, 15, 255, 255)));
    assert!(is_private_addr(v4(172, 16, 0, 0)));
    assert!(is_private_addr(v4(172, 31, 255, 255)));
    assert!(!is_private_addr(v4(172, 32, 0, 0)));

    // RFC1918 192.168/16 boundaries.
    assert!(!is_private_addr(v4(192, 167, 255, 255)));
    assert!(is_private_addr(v4(192, 168, 0, 0)));
    assert!(!is_private_addr(v4(192, 169, 0, 0)));

    // Link-local 169.254/16.
    assert!(is_private_addr(v4(169, 254, 0, 1)));
    assert!(!is_private_addr(v4(169, 253, 0, 1)));
    assert!(!is_private_addr(v4(169, 255, 0, 1)));

    // Public and unspecified.
    assert!(!is_private_addr(v4(8, 8, 8, 8)));
    assert!(!is_private_addr(v4(0, 0, 0, 0)));
    assert!(!is_private_addr(v4(255, 255, 255, 255)));

    // Loopback is treated as private/local (documented).
    assert!(is_private_addr(v4(127, 0, 0, 1)));
}

#[test]
fn is_private_addr_ipv6_boundaries() {
    // Loopback and link-local and ULA -> yes.
    assert!(is_private_addr(v6("::1")));
    assert!(is_private_addr(v6("fe80::")));
    assert!(is_private_addr(v6("fe80::1")));
    assert!(is_private_addr(v6("febf::")));
    assert!(is_private_addr(v6("fc00::")));
    assert!(is_private_addr(v6("fd00::1")));

    // Just outside the ranges -> no.
    assert!(!is_private_addr(v6("fec0::"))); // not link-local, not ULA
    assert!(!is_private_addr(v6("fe00::"))); // 0xfe & 0xfe != 0xfc
    assert!(!is_private_addr(v6("2001::")));
    assert!(!is_private_addr(v6("2001:4860:4860::8888")));
    assert!(!is_private_addr(v6("::"))); // unspecified
}

#[test]
fn advertisable_excludes_loopback_and_unspecified() {
    // is_private_addr says loopback is private, but it must never be in the
    // advertise/browse bind set.
    assert!(is_private_addr(v4(127, 0, 0, 1)));
    assert!(!advertisable_for_tests(v4(127, 0, 0, 1)));
    assert!(!advertisable_for_tests(v6("::1")));
    assert!(!advertisable_for_tests(v4(0, 0, 0, 0)));
    assert!(!advertisable_for_tests(v6("::")));

    // The three LAN families are advertisable.
    assert!(advertisable_for_tests(v4(10, 1, 2, 3)));
    assert!(advertisable_for_tests(v4(192, 168, 1, 1)));
    assert!(advertisable_for_tests(v4(169, 254, 1, 1)));
    assert!(advertisable_for_tests(v6("fe80::1")));
    assert!(advertisable_for_tests(v6("fd12::34")));
}

#[test]
fn private_interface_addrs_only_returns_advertisable_addresses() {
    // Host-dependent, possibly empty, but the invariant must always hold: no
    // loopback, no unspecified, no public address ever escapes.
    let addrs = private_interface_addrs().expect("interface enumeration should succeed");
    for addr in &addrs {
        assert!(is_private_addr(*addr), "{addr} classified public");
        assert!(advertisable_for_tests(*addr), "{addr} not advertisable");
        assert!(!addr.is_loopback(), "{addr} is loopback");
        assert!(!addr.is_unspecified(), "{addr} is unspecified");
    }
    // ST-M002-010: when the host has any routable-private address, no
    // link-local (169.254 / fe80::) may survive in the bind/advertise set —
    // otherwise the advertised record and the bound listener could diverge onto
    // an unreachable interface.
    let has_routable = addrs
        .iter()
        .any(|a| !matches!(a, IpAddr::V4(v4) if v4.octets()[..2] == [169, 254]) && !is_ipv6_ll(*a));
    if has_routable {
        for addr in &addrs {
            let is_v4_ll = matches!(addr, IpAddr::V4(v4) if v4.octets()[..2] == [169, 254]);
            assert!(
                !is_v4_ll && !is_ipv6_ll(*addr),
                "{addr} link-local survived alongside a routable-private address"
            );
        }
    }
}

/// True for an IPv6 `fe80::/10` link-local address (test-local mirror of the
/// crate's classifier, kept here so the invariant check stays self-contained).
fn is_ipv6_ll(addr: IpAddr) -> bool {
    matches!(addr, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
}

/// True for an IPv4 or IPv6 link-local address (test-local, as above).
fn is_link_local(addr: IpAddr) -> bool {
    matches!(addr, IpAddr::V4(v4) if v4.octets()[..2] == [169, 254]) || is_ipv6_ll(addr)
}

#[test]
fn routable_private_is_preferred_over_link_local() {
    // A host with a stray APIPA address alongside a routable private one: the
    // routable 192.168 must win over the stray 169.254 even though 169 sorts
    // numerically before 192.
    let picked =
        prefer_routable_private_for_tests(vec![v4(169, 254, 248, 109), v4(192, 168, 0, 140)]);
    assert_eq!(picked, vec![v4(192, 168, 0, 140)]);

    // 10/8 also sorts before 169.254 numerically, yet must still be preferred,
    // and link-local is dropped entirely once a routable address exists.
    let picked = prefer_routable_private_for_tests(vec![v4(169, 254, 1, 1), v4(10, 0, 0, 5)]);
    assert_eq!(picked, vec![v4(10, 0, 0, 5)]);

    // IPv6 ULA is routable-private and outranks IPv6 link-local.
    let picked = prefer_routable_private_for_tests(vec![v6("fe80::1"), v6("fd00::1")]);
    assert_eq!(picked, vec![v6("fd00::1")]);

    // Link-local survives as the last resort when it is the ONLY option (a
    // direct cable / no-DHCP LAN), sorted deterministically — REQ-S-010 permits
    // it and the direct-cable pairing path must keep working.
    let picked = prefer_routable_private_for_tests(vec![v4(169, 254, 5, 5), v4(169, 254, 1, 1)]);
    assert_eq!(picked, vec![v4(169, 254, 1, 1), v4(169, 254, 5, 5)]);

    // A routable-only set is returned sorted and de-duplicated, unchanged.
    let picked = prefer_routable_private_for_tests(vec![
        v4(192, 168, 0, 140),
        v4(10, 0, 0, 5),
        v4(10, 0, 0, 5),
    ]);
    assert_eq!(picked, vec![v4(10, 0, 0, 5), v4(192, 168, 0, 140)]);

    // Empty stays empty (host has no private LAN address at all).
    assert!(prefer_routable_private_for_tests(Vec::new()).is_empty());
}

// ---------------------------------------------------------------------------
// Connection code
// ---------------------------------------------------------------------------

#[test]
fn connection_code_frozen_golden_vector() {
    // FROZEN: changing this value is a wire-format change. Independently
    // derived by the Python oracle in the ST-M002-003 build notes.
    let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 47000);
    assert_eq!(ConnectionCode::encode(addr), "3G58-04SB-F63B");

    let target = ConnectionCode::parse("3G58-04SB-F63B").unwrap();
    assert_eq!(target.addr(), SocketAddr::V4(addr));
    assert_eq!(
        target.source(),
        TargetSource::ConnectionCode { format_version: 1 }
    );
}

#[test]
fn connection_code_round_trips_including_extremes() {
    let cases = [
        SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0),
        SocketAddrV4::new(Ipv4Addr::new(255, 255, 255, 255), 65535),
        SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 47000),
        SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 22),
        SocketAddrV4::new(Ipv4Addr::new(172, 16, 5, 4), 8080),
        SocketAddrV4::new(Ipv4Addr::new(169, 254, 13, 37), 1),
        SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 5678),
    ];
    for addr in cases {
        let code = ConnectionCode::encode(addr);
        // Grouped form is exactly XXXX-XXXX-XXXX.
        assert_eq!(code.len(), 14);
        assert_eq!(code.as_bytes()[4], b'-');
        assert_eq!(code.as_bytes()[9], b'-');

        let target = ConnectionCode::parse(&code).unwrap();
        assert_eq!(target.addr(), SocketAddr::V4(addr), "round trip {addr}");
        assert_eq!(
            target.source(),
            TargetSource::ConnectionCode { format_version: 1 }
        );
    }
}

#[test]
fn connection_code_single_bit_corruption_is_rejected_by_crc() {
    // Every single-bit flip in the 60-bit value must be caught by the CRC,
    // before any connection attempt, as a distinct ChecksumMismatch.
    let ip = Ipv4Addr::new(192, 168, 1, 50);
    let port = 47000u16;
    let value = code_value(ip, port, 1);
    for bit in 0..60u32 {
        let corrupted = value ^ (1u64 << bit);
        let code = encode_symbols_for_tests(corrupted);
        assert_eq!(
            ConnectionCode::parse(&code),
            Err(ManualEntryError::ChecksumMismatch),
            "bit {bit} flip should fail the checksum"
        );
    }
}

#[test]
fn decode_symbol_is_case_insensitive_with_crockford_substitutions() {
    // I/L -> 1, O -> 0, case-insensitive.
    assert_eq!(decode_symbol_for_tests('I'), Some(1));
    assert_eq!(decode_symbol_for_tests('L'), Some(1));
    assert_eq!(decode_symbol_for_tests('i'), Some(1));
    assert_eq!(decode_symbol_for_tests('l'), Some(1));
    assert_eq!(decode_symbol_for_tests('1'), Some(1));
    assert_eq!(decode_symbol_for_tests('O'), Some(0));
    assert_eq!(decode_symbol_for_tests('o'), Some(0));
    assert_eq!(decode_symbol_for_tests('0'), Some(0));
    assert_eq!(decode_symbol_for_tests('A'), Some(10));
    assert_eq!(decode_symbol_for_tests('a'), Some(10));
    assert_eq!(decode_symbol_for_tests('Z'), Some(31));
    assert_eq!(decode_symbol_for_tests('z'), Some(31));

    // U is excluded from the alphabet, and non-alphabet characters fail.
    assert_eq!(decode_symbol_for_tests('U'), None);
    assert_eq!(decode_symbol_for_tests('u'), None);
    assert_eq!(decode_symbol_for_tests('!'), None);
    assert_eq!(decode_symbol_for_tests('é'), None);
}

#[test]
fn connection_code_accepts_substituted_and_mixed_case() {
    // Golden code "3G58-04SB-F63B" contains a '0'; replacing it with 'O' and
    // lower-casing must decode identically.
    let canonical = ConnectionCode::parse("3G58-04SB-F63B").unwrap();
    let substituted = ConnectionCode::parse("3g58-o4sb-f63b").unwrap();
    assert_eq!(canonical.addr(), substituted.addr());

    // A code that contains a '1' decodes the same via 'I' or 'L'.
    let addr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 22);
    let code = ConnectionCode::encode(addr); // "22G0-000G-05PB"
    let with_letters: String = code
        .chars()
        .map(|c| match c {
            '1' => 'I',
            '0' => 'O',
            other => other.to_ascii_lowercase(),
        })
        .collect();
    assert_eq!(
        ConnectionCode::parse(&with_letters).unwrap().addr(),
        SocketAddr::V4(addr)
    );
}

#[test]
fn connection_code_tolerates_hyphens_and_whitespace() {
    let want = ConnectionCode::parse("3G58-04SB-F63B").unwrap().addr();
    for variant in [
        "3G5804SBF63B",
        "3G58-04SB-F63B",
        "  3G58 04SB F63B  ",
        "3G58\t04SB\nF63B",
        "3-G-5-8-0-4-S-B-F-6-3-B",
    ] {
        assert_eq!(
            ConnectionCode::parse(variant).unwrap().addr(),
            want,
            "{variant}"
        );
    }
}

#[test]
fn connection_code_error_taxonomy() {
    // BadLength.
    assert_eq!(
        ConnectionCode::parse("AAA"),
        Err(ManualEntryError::BadLength)
    );
    assert_eq!(
        ConnectionCode::parse("AAAAAAAAAAAAA"),
        Err(ManualEntryError::BadLength)
    );
    assert_eq!(
        ConnectionCode::parse("AAAA-AAAA-AAA"),
        Err(ManualEntryError::BadLength)
    );

    // BadChar (12 symbols, one is 'U', which is outside the alphabet).
    assert_eq!(
        ConnectionCode::parse("AAAAAAAAAAAU"),
        Err(ManualEntryError::BadChar)
    );

    // ChecksumMismatch: valid length and charset, wrong checksum.
    let value = code_value(Ipv4Addr::new(192, 168, 1, 50), 47000, 1) ^ 1;
    assert_eq!(
        ConnectionCode::parse(&encode_symbols_for_tests(value)),
        Err(ManualEntryError::ChecksumMismatch)
    );
}

#[test]
fn connection_code_unsupported_version_is_distinct() {
    // Frozen version-2 code (valid checksum, unsupported version) from the
    // oracle.
    assert_eq!(
        ConnectionCode::parse("5G58-04SB-F60D"),
        Err(ManualEntryError::UnsupportedVersion)
    );

    // Programmatically confirm any well-formed version != 1 is rejected as
    // UnsupportedVersion (checksum valid), not as corruption.
    for version in [0u8, 2, 3, 15] {
        let value = code_value(Ipv4Addr::new(192, 168, 1, 50), 47000, version);
        let code = encode_symbols_for_tests(value);
        assert_eq!(
            ConnectionCode::parse(&code),
            Err(ManualEntryError::UnsupportedVersion),
            "version {version}"
        );
    }

    // Sanity: version 1 (the supported version) parses successfully.
    let v1 = code_value(Ipv4Addr::new(192, 168, 1, 50), 47000, 1);
    assert!(ConnectionCode::parse(&encode_symbols_for_tests(v1)).is_ok());
}

// ---------------------------------------------------------------------------
// Manual entry: host:port / [ipv6]:port escape hatch
// ---------------------------------------------------------------------------

#[test]
fn parse_manual_entry_accepts_raw_host_port() {
    let t = parse_manual_entry("192.168.1.10:7000").unwrap();
    assert_eq!(
        t.addr(),
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 10), 7000))
    );
    assert_eq!(t.source(), TargetSource::HostPort);

    let t = parse_manual_entry("10.0.0.5:1").unwrap();
    assert_eq!(t.source(), TargetSource::HostPort);
    assert_eq!(t.addr().port(), 1);
}

#[test]
fn parse_manual_entry_accepts_bracketed_ipv6() {
    let t = parse_manual_entry("[fe80::1]:7000").unwrap();
    assert_eq!(t.source(), TargetSource::HostPort);
    assert_eq!(t.addr().port(), 7000);
    assert!(t.addr().is_ipv6());

    assert!(parse_manual_entry("[::1]:22").is_ok());
}

#[test]
fn parse_manual_entry_routes_codes_and_reports_bad_host_port() {
    // A 12-char code (no colon) routes to the code parser.
    let t = parse_manual_entry("3G58-04SB-F63B").unwrap();
    assert_eq!(
        t.source(),
        TargetSource::ConnectionCode { format_version: 1 }
    );

    // Colon-shaped but not a valid IP literal -> BadHostPort (no DNS).
    assert_eq!(
        parse_manual_entry("example.com:80"),
        Err(ManualEntryError::BadHostPort)
    );
    // A bare hostname (no colon, no port) routes to the code branch and fails
    // deterministically on length.
    assert_eq!(
        parse_manual_entry("myhost"),
        Err(ManualEntryError::BadLength)
    );
    assert_eq!(
        parse_manual_entry("fe80::1"),
        Err(ManualEntryError::BadHostPort) // colon, but no port / not bracketed
    );
}

// ---------------------------------------------------------------------------
// DiscoveredConsole parsing from synthetic records
// ---------------------------------------------------------------------------

#[test]
fn discovered_console_parses_well_formed_record() {
    let dc = DiscoveredConsole::from_parts_for_tests(
        "Front Desk._cretlink._tcp.local.",
        vec![v4(192, 168, 1, 20), v4(192, 168, 1, 20), v4(10, 0, 0, 5)],
        7000,
        Some("1.0"),
        Some("Front Desk"),
    );
    assert_eq!(dc.instance_name, "Front Desk");
    assert_eq!(dc.display_name.as_deref(), Some("Front Desk"));
    assert_eq!(dc.port, 7000);
    assert_eq!(dc.protocol_version, Some(ProtocolVersion::new(1, 0)));
    // Sorted + de-duplicated.
    assert_eq!(dc.addresses, vec![v4(10, 0, 0, 5), v4(192, 168, 1, 20)]);
}

#[test]
fn discovered_console_degrades_malformed_hints_to_none() {
    // Malformed version, empty name, junk fullname -> no panics, None fields.
    let dc = DiscoveredConsole::from_parts_for_tests("weird", vec![], 0, Some("banana"), Some(""));
    assert_eq!(dc.instance_name, "weird");
    assert_eq!(dc.display_name, None);
    assert_eq!(dc.protocol_version, None);
    assert!(dc.addresses.is_empty());

    // Absent TXT entirely.
    let dc = DiscoveredConsole::from_parts_for_tests(
        "x._cretlink._tcp.local.",
        vec![v4(192, 168, 0, 2)],
        22,
        None,
        None,
    );
    assert_eq!(dc.display_name, None);
    assert_eq!(dc.protocol_version, None);
    assert_eq!(dc.instance_name, "x");
}

#[test]
fn instance_name_strips_suffix_and_unescapes() {
    assert_eq!(
        instance_from_fullname_for_tests("MyConsole._cretlink._tcp.local."),
        "MyConsole"
    );
    // DNS-SD escapes a dot in the label as "\.".
    assert_eq!(
        instance_from_fullname_for_tests("Front\\.Desk._cretlink._tcp.local."),
        "Front.Desk"
    );
    assert_eq!(
        instance_from_fullname_for_tests("a\\\\b._cretlink._tcp.local."),
        "a\\b"
    );
    // No recognizable suffix -> pass through.
    assert_eq!(instance_from_fullname_for_tests("bare"), "bare");
}

#[test]
fn parse_protocol_version_is_lenient_and_panic_free() {
    assert_eq!(
        parse_protocol_version_for_tests("1.0"),
        Some(ProtocolVersion::new(1, 0))
    );
    assert_eq!(
        parse_protocol_version_for_tests("2.5"),
        Some(ProtocolVersion::new(2, 5))
    );
    for bad in [
        "1",
        "1.0.0",
        "",
        ".",
        "x.y",
        "1.",
        ".0",
        "-1.0",
        "1.-1",
        "99999999.0",
    ] {
        assert_eq!(parse_protocol_version_for_tests(bad), None, "{bad}");
    }
}

// ---------------------------------------------------------------------------
// Constants and provenance
// ---------------------------------------------------------------------------

#[test]
fn constants_are_frozen() {
    assert_eq!(SERVICE_TYPE, "_cretlink._tcp.local.");
    assert_eq!(TXT_KEY_VERSION, "v");
    assert_eq!(TXT_KEY_NAME, "name");
}

#[test]
fn advertise_properties_emits_exactly_version_and_name() {
    // AC1 / REQ-S-007: the advertised pre-pairing metadata is exactly
    // {v => version, name => display_name} and nothing else. The array type
    // fixes the count at two; this assertion pins the keys and values. A later
    // TXT addition (e.g. the instance UUID ADR-0007 contemplates) must change
    // this array and will fail here until reviewed as intentional and
    // non-secret — closing the AC1 coverage gap without needing multicast.
    let props = advertise_properties_for_tests("1.0", "Front Desk");
    assert_eq!(
        props,
        [(TXT_KEY_VERSION, "1.0"), (TXT_KEY_NAME, "Front Desk")]
    );
}

#[test]
fn discovery_provenance_conveys_no_authority() {
    // AC2: a discovered or manually-entered peer can never grant control or
    // authenticate. TargetSource is pure provenance with only these two
    // non-authoritative variants, and ConnectionTarget exposes only an
    // addressing hint via addr() — no connect/authenticate method exists. This
    // exhaustive match is a compile-time guard: adding any authority-conveying
    // variant (e.g. "Paired"/"Trusted") breaks this test until a reviewer
    // updates it, giving AC2 an explicit backing test.
    let target = ConnectionCode::parse("3G58-04SB-F63B").unwrap();
    match target.source() {
        TargetSource::ConnectionCode { format_version } => assert_eq!(format_version, 1),
        TargetSource::HostPort => unreachable!("a connection code is not a host:port entry"),
    }
    let _addressing_hint: SocketAddr = target.addr();
}

// ---------------------------------------------------------------------------
// The address a console waits on (ST-M009-016)
// ---------------------------------------------------------------------------

fn candidate(addr: IpAddr, adapter: &str) -> ListenCandidate {
    ListenCandidate {
        addr,
        adapter: adapter.to_string(),
        point_to_point: false,
    }
}

fn tunnel(addr: IpAddr, adapter: &str) -> ListenCandidate {
    ListenCandidate {
        point_to_point: true,
        ..candidate(addr, adapter)
    }
}

/// The technician's computer of FND-0010 with its VPN connected: the VPN
/// adapter sorts first, a virtual switch second, the local network last.
fn three_networks() -> Vec<ListenCandidate> {
    order_candidates_for_tests(vec![
        candidate(v4(192, 168, 1, 20), "Wi-Fi"),
        candidate(v4(172, 20, 0, 1), "vEthernet (Default Switch)"),
        candidate(v4(10, 0, 0, 5), "VPN"),
    ])
}

#[test]
fn candidates_are_in_numeric_order_with_one_entry_for_each_address() {
    let ordered = order_candidates_for_tests(vec![
        candidate(v4(192, 168, 1, 20), "Wi-Fi"),
        candidate(v4(10, 0, 0, 5), "VPN"),
        candidate(v4(192, 168, 1, 20), "Wi-Fi"),
        candidate(v6("fd00::1"), "Wi-Fi"),
        candidate(v4(172, 20, 0, 1), "vEthernet (Default Switch)"),
    ]);
    let addrs: Vec<IpAddr> = ordered.iter().map(|c| c.addr).collect();
    assert_eq!(
        addrs,
        vec![
            v4(10, 0, 0, 5),
            v4(172, 20, 0, 1),
            v4(192, 168, 1, 20),
            v6("fd00::1")
        ]
    );
    assert_eq!(ordered[2].adapter, "Wi-Fi");
}

#[test]
fn candidates_hold_a_link_local_address_only_when_nothing_else_exists() {
    // ST-M002-010 still holds for the list the technician chooses from.
    let ordered = order_candidates_for_tests(vec![
        candidate(v4(169, 254, 248, 109), "Ethernet 2"),
        candidate(v4(192, 168, 1, 20), "Wi-Fi"),
        candidate(v6("fe80::1"), "Wi-Fi"),
    ]);
    assert_eq!(ordered, vec![candidate(v4(192, 168, 1, 20), "Wi-Fi")]);

    // A direct cable without DHCP: link-local is the genuine address.
    let cable = order_candidates_for_tests(vec![candidate(v4(169, 254, 1, 1), "Ethernet")]);
    assert_eq!(cable, vec![candidate(v4(169, 254, 1, 1), "Ethernet")]);

    assert!(order_candidates_for_tests(Vec::new()).is_empty());
}

#[test]
fn first_choice_follows_the_default_route_not_the_numeric_order() {
    let networks = three_networks();
    // Numerically the VPN address comes first; that was the choice of FND-0010.
    assert_eq!(networks[0].adapter, "VPN");

    // The computer reaches other networks through its Wi-Fi.
    let chosen = first_choice(&networks, Some(v4(192, 168, 1, 20))).unwrap();
    assert_eq!(chosen.adapter, "Wi-Fi");
    assert_eq!(chosen.addr, v4(192, 168, 1, 20));
}

#[test]
fn first_choice_passes_over_a_virtual_adapter_that_routes_nowhere() {
    // Without the VPN: a virtual switch (172.x) sorts before the local network
    // (192.168.x), but only the local network has the default route.
    let networks = order_candidates_for_tests(vec![
        candidate(v4(192, 168, 1, 20), "Wi-Fi"),
        candidate(v4(172, 20, 0, 1), "vEthernet (Default Switch)"),
    ]);
    assert_eq!(networks[0].adapter, "vEthernet (Default Switch)");
    assert_eq!(
        first_choice(&networks, Some(v4(192, 168, 1, 20)))
            .unwrap()
            .adapter,
        "Wi-Fi"
    );
}

#[test]
fn first_choice_does_not_take_a_point_to_point_link_for_the_local_network() {
    // A VPN that carries the default route is still not where the customer is.
    let networks = order_candidates_for_tests(vec![
        tunnel(v4(10, 0, 0, 5), "VPN"),
        candidate(v4(192, 168, 1, 20), "Wi-Fi"),
    ]);
    assert_eq!(
        first_choice(&networks, Some(v4(10, 0, 0, 5)))
            .unwrap()
            .adapter,
        "Wi-Fi"
    );
    // Without any default route the first adapter that is no tunnel is taken.
    assert_eq!(first_choice(&networks, None).unwrap().adapter, "Wi-Fi");
}

#[test]
fn first_choice_falls_back_to_the_order_when_the_default_route_does_not_help() {
    let networks = three_networks();
    // No default route (an isolated network).
    assert_eq!(first_choice(&networks, None).unwrap().adapter, "VPN");
    // The default route leaves through an address that is no candidate, such
    // as a public one.
    assert_eq!(
        first_choice(&networks, Some(v4(192, 0, 2, 1)))
            .unwrap()
            .adapter,
        "VPN"
    );
    // Only point-to-point links: the first one, rather than none.
    let tunnels = vec![
        tunnel(v4(10, 0, 0, 5), "VPN"),
        tunnel(v4(10, 0, 0, 9), "VPN 2"),
    ];
    assert_eq!(
        first_choice(&tunnels, Some(v4(10, 0, 0, 9)))
            .unwrap()
            .adapter,
        "VPN"
    );
    // Link-local only, and nothing at all.
    let cable = vec![candidate(v4(169, 254, 1, 1), "Ethernet")];
    assert_eq!(first_choice(&cable, None).unwrap().adapter, "Ethernet");
    assert!(first_choice(&[], Some(v4(192, 168, 1, 20))).is_none());
}

#[test]
fn listen_candidates_of_this_host_are_private_and_named() {
    // Host-dependent, so only the invariants are asserted (REQ-S-010).
    let candidates = listen_candidates().expect("interface enumeration should work on CI hosts");
    for candidate in &candidates {
        assert!(advertisable_for_tests(candidate.addr), "{candidate:?}");
        assert!(!candidate.addr.is_loopback(), "{candidate:?}");
        assert!(!candidate.adapter.is_empty(), "{candidate:?}");
    }
    let has_routable = candidates.iter().any(|c| !is_link_local(c.addr));
    if has_routable {
        assert!(candidates.iter().all(|c| !is_link_local(c.addr)));
    }
    // The first choice is one of the candidates, whatever this host routes through.
    if let Some(chosen) = first_choice(&candidates, default_route_addr()) {
        assert!(candidates.contains(chosen));
    }
}

#[test]
fn the_default_route_address_is_a_real_local_address_or_unknown() {
    // A host without a default route answers `None`; one with a route answers
    // a concrete address. Either way nothing is sent and nothing listens.
    if let Some(addr) = default_route_addr() {
        assert!(!addr.is_unspecified());
    }
}

#[test]
fn a_console_announces_exactly_the_address_it_listens_on() {
    let listener = SocketAddr::new(v4(192, 168, 1, 20), 47141);
    let info = service_info_for_tests("TECH-LAPTOP", listener, ProtocolVersion::new(1, 11), "Sam")
        .expect("a private address is announced");
    let announced: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
    assert_eq!(announced, vec![v4(192, 168, 1, 20)]);
    assert_eq!(info.get_port(), 47141);
    assert_eq!(info.get_property_val_str(TXT_KEY_NAME), Some("Sam"));
    assert_eq!(info.get_property_val_str(TXT_KEY_VERSION), Some("1.11"));
}

#[test]
fn a_console_refuses_to_announce_an_address_that_is_not_private() {
    for addr in [
        v4(127, 0, 0, 1),
        v4(0, 0, 0, 0),
        v4(192, 0, 2, 1),
        v6("::1"),
        v6("::"),
    ] {
        let refused = service_info_for_tests(
            "TECH-LAPTOP",
            SocketAddr::new(addr, 47141),
            ProtocolVersion::new(1, 11),
            "Sam",
        );
        assert!(
            matches!(refused, Err(DiscoveryError::NotPrivate(ip)) if ip == addr),
            "{addr} must not be announced"
        );
    }
}

// ---------------------------------------------------------------------------
// Live mDNS round trip — requires multicast, run manually
// ---------------------------------------------------------------------------

/// Registers a console and browses for it over real mDNS. Ignored because CI
/// runners frequently block multicast; run manually on a host with a private
/// interface: `cargo test -p cretlink-discovery -- --ignored`.
#[test]
#[ignore = "requires multicast on a real private interface; run manually"]
fn loopback_advertise_then_browse_round_trip() {
    use std::time::Duration;

    use crate::{Advertiser, Browser};

    let candidates = listen_candidates().expect("interfaces should be readable");
    let chosen = first_choice(&candidates, default_route_addr())
        .expect("this test needs a private interface");
    let advertiser = Advertiser::advertise(
        "cretlink-test-console",
        SocketAddr::new(chosen.addr, 47001),
        ProtocolVersion::new(1, 0),
        "Test Console",
    )
    .expect("advertise should succeed on a private interface");

    let browser = Browser::new().expect("browser should scope to a private interface");
    let found = browser
        .browse(Duration::from_secs(3))
        .expect("browse should not error");

    assert!(
        found
            .iter()
            .any(|c| c.instance_name == "cretlink-test-console"),
        "advertised console should be discovered; saw {found:?}"
    );
    drop(advertiser);
}

//! Unit tests: serde round-trips per variant, every limit violated once,
//! unknown-field rejection, oversized-before-parse, and version
//! negotiation (ST-M001-003, REQ-N-012).

use std::collections::BTreeMap;

use serde_json::json;

use crate::{
    B64Bytes, CAP_DESKTOP_V1, CAP_FS_V1, CAP_REBOOT_RESUME_V1, CAP_TERM_V1, CleanupFailure,
    CleanupPolicy, CleanupTrigger, ControlMessage, DesktopAccess, DesktopFrameMeta, DesktopState,
    DigestAlgo, ErrorCode, ErrorStage, EventKind, FileEntry, FileKind, ImageFormat, JobId,
    JobOutcome, JobSummary, KeyAction, Message, OpResult, Operation, Origin, OutputStream,
    PROTOCOL_V1, PairingMessage, PointerAction, PointerButton, ProtocolError, ProtocolVersion,
    REBOOT_RESUME_NONCE_LEN, REBOOT_RESUME_PROOF_LEN, REBOOT_RESUME_TICKET_ID_LEN, RemotePath,
    RequestId, RevokeReason, Role, SAC_COMMITMENT_LEN, SAC_NONCE_LEN, SessionId, Shell,
    StagedEntry, TargetIdentity, canonical_capabilities, limits,
};
use crate::{Purpose, PurposeError};

fn simulated_target() -> TargetIdentity {
    TargetIdentity {
        hostname: "SIMULATED-TARGET".to_owned(),
        os_version: "Simulated Windows".to_owned(),
        arch: "x86_64".to_owned(),
        elevated: false,
        simulated: true,
        agent_version: "0.0.0".to_owned(),
    }
}

fn exec_operation() -> Operation {
    Operation::Exec {
        shell: Shell::PowerShell,
        command: "sim:ok hello".to_owned(),
        working_dir: Some("C:\\Windows".to_owned()),
        env: BTreeMap::from([("CRETLINK_TEST".to_owned(), "1".to_owned())]),
        timeout_ms: Some(30_000),
    }
}

fn request(op: Operation) -> Message {
    Message::Request {
        id: RequestId(7),
        session: SessionId::new(),
        origin: Origin::Console,
        purpose: None,
        op,
    }
}

#[track_caller]
fn round_trip(message: Message) {
    let bytes = message.encode().unwrap();
    let decoded = Message::decode(&bytes).unwrap();
    assert_eq!(decoded, message);
}

#[track_caller]
fn assert_limit_exceeded(message: &Message) {
    let err = message.validate().unwrap_err();
    assert!(matches!(err, ProtocolError::LimitExceeded { .. }), "{err}");
    assert_eq!(err.code(), ErrorCode::LimitExceeded);
    // encode must refuse the same message.
    let err = message.encode().unwrap_err();
    assert_eq!(err.code(), ErrorCode::LimitExceeded);
}

// --- serde round-trips, one per Message variant and nested kind ---

#[test]
fn round_trip_hello() {
    round_trip(Message::Hello {
        protocol: PROTOCOL_V1,
        role: Role::Console,
        capabilities: vec!["exec".to_owned(), "jobs".to_owned()],
    });
}

#[test]
fn round_trip_hello_ack() {
    round_trip(Message::HelloAck {
        protocol: PROTOCOL_V1,
        session: SessionId::new(),
        target: simulated_target(),
    });
}

#[test]
fn round_trip_request_system_info() {
    round_trip(request(Operation::SystemInfo {}));
}

#[test]
fn round_trip_request_exec() {
    round_trip(request(exec_operation()));
}

#[test]
fn round_trip_request_write_stdin() {
    round_trip(request(Operation::WriteStdin {
        job: JobId(3),
        data: B64Bytes::new(vec![0, 1, 2, 255]),
    }));
}

#[test]
fn round_trip_request_list_jobs() {
    round_trip(request(Operation::ListJobs {}));
}

#[test]
fn round_trip_response_system_info() {
    round_trip(Message::Response {
        id: RequestId(1),
        session: SessionId::new(),
        result: OpResult::SystemInfo {
            target: simulated_target(),
            reboot_pending: false,
        },
    });
}

/// Cross-architecture identity fixtures (ST-M003-003): a console on any host
/// architecture (ADR-0002 allows an aarch64-apple-darwin console against the
/// x64-only Windows agent) must carry identities with differing `arch` and
/// `os_version` values through the schema unchanged. This runs on every CI
/// platform, so Windows-shaped identity data is exercised off-Windows too.
#[test]
fn cross_architecture_identity_fixtures_round_trip() {
    let fixtures = [
        TargetIdentity {
            hostname: "CUSTOMER-PC".to_owned(),
            os_version: "Windows 11 Pro 24H2 (build 26100.1234)".to_owned(),
            arch: "x86_64".to_owned(),
            elevated: true,
            simulated: false,
            agent_version: "1.2.3".to_owned(),
        },
        TargetIdentity {
            hostname: "ARM-CONSOLE-VIEW".to_owned(),
            os_version: "Windows 10 Pro 22H2 (build 19045.5011)".to_owned(),
            arch: "aarch64".to_owned(),
            elevated: false,
            simulated: false,
            agent_version: "0.9.0".to_owned(),
        },
    ];
    for target in fixtures {
        // The identity travels in both carriers: negotiation and SystemInfo.
        round_trip(Message::HelloAck {
            protocol: PROTOCOL_V1,
            session: SessionId::new(),
            target: target.clone(),
        });
        round_trip(Message::Response {
            id: RequestId(1),
            session: SessionId::new(),
            result: OpResult::SystemInfo {
                target: target.clone(),
                reboot_pending: true,
            },
        });
        // The arch and version strings survive as opaque values, byte for byte.
        let json = serde_json::to_value(&target).unwrap();
        assert_eq!(json["arch"], json!(target.arch));
        assert_eq!(json["os_version"], json!(target.os_version));
    }
}

#[test]
fn round_trip_response_exec_started() {
    round_trip(Message::Response {
        id: RequestId(2),
        session: SessionId::new(),
        result: OpResult::ExecStarted {
            job: JobId(9),
            working_dir: "C:\\Users\\customer".to_owned(),
        },
    });
}

#[test]
fn round_trip_response_ack() {
    round_trip(Message::Response {
        id: RequestId(3),
        session: SessionId::new(),
        result: OpResult::Ack {},
    });
}

#[test]
fn round_trip_response_jobs() {
    round_trip(Message::Response {
        id: RequestId(4),
        session: SessionId::new(),
        result: OpResult::Jobs {
            jobs: vec![JobSummary {
                job: JobId(9),
                command_preview: "sim:hang".to_owned(),
                running: true,
            }],
        },
    });
}

#[test]
fn round_trip_event_job_output() {
    round_trip(Message::Event {
        session: SessionId::new(),
        event: EventKind::JobOutput {
            job: JobId(9),
            stream: OutputStream::Stderr,
            // Raw VT escape bytes pass through unmodified (ADR-0010).
            data: B64Bytes::new(b"\x1b[31mred\x1b[0m\n".to_vec()),
        },
    });
}

#[test]
fn round_trip_event_job_exited() {
    for outcome in [
        JobOutcome::Completed,
        JobOutcome::Failed,
        JobOutcome::Cancelled,
        JobOutcome::TimedOut,
        JobOutcome::Unknown,
    ] {
        round_trip(Message::Event {
            session: SessionId::new(),
            event: EventKind::JobExited {
                job: JobId(9),
                exit_code: Some(1),
                duration_ms: 42,
                outcome,
            },
        });
    }
}

#[test]
fn round_trip_event_disconnected() {
    round_trip(Message::Event {
        session: SessionId::new(),
        event: EventKind::Disconnected {
            job: JobId(9),
            process_continues: true,
        },
    });
}

#[test]
fn round_trip_event_desktop_activity() {
    // Both interaction classes survive the wire; the event carries the class
    // only, never a captured pixel, keystroke, or clipboard text
    // (ST-M005-005, REQ-S-016 through REQ-S-018).
    for access in [DesktopAccess::Observe, DesktopAccess::Control] {
        round_trip(Message::Event {
            session: SessionId::new(),
            event: EventKind::DesktopActivity { access },
        });
    }
}

#[test]
fn desktop_activity_wire_tag_is_snake_case() {
    let bytes = Message::Event {
        session: SessionId::new(),
        event: EventKind::DesktopActivity {
            access: DesktopAccess::Observe,
        },
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // The `EventKind` is internally tagged (`tag = "event"`) and nested under the
    // envelope's `event` field, so the wire tag lives at `event.event`.
    assert_eq!(value["event"]["event"], "desktop_activity");
    assert_eq!(value["event"]["access"], "observe");
}

#[test]
fn round_trip_cancel() {
    round_trip(Message::Cancel {
        id: RequestId(5),
        session: SessionId::new(),
        origin: Origin::Console,
        purpose: None,
        job: JobId(9),
    });
}

#[test]
fn round_trip_error() {
    round_trip(Message::Error {
        id: Some(RequestId(6)),
        session: None,
        code: ErrorCode::Unsupported,
        message: "unsupported operation".to_owned(),
        stage: ErrorStage::Validation,
    });
}

#[test]
fn round_trip_pairing_commit() {
    round_trip(Message::Pairing {
        pairing: PairingMessage::Commit {
            commitment: B64Bytes::new(vec![0xAB; SAC_COMMITMENT_LEN]),
        },
    });
}

#[test]
fn round_trip_pairing_nonce() {
    round_trip(Message::Pairing {
        pairing: PairingMessage::Nonce {
            nc: B64Bytes::new(vec![0x11; SAC_NONCE_LEN]),
        },
    });
}

#[test]
fn round_trip_pairing_reveal() {
    round_trip(Message::Pairing {
        pairing: PairingMessage::Reveal {
            na: B64Bytes::new(vec![0x22; SAC_NONCE_LEN]),
        },
    });
}

// --- session-control messages (ST-M002-005/006) ---

fn control(control: ControlMessage) -> Message {
    Message::Control {
        session: SessionId::new(),
        control,
    }
}

#[test]
fn round_trip_control_consent_requested() {
    round_trip(control(ControlMessage::ConsentRequested {
        disclosure: "The technician can run commands, install software, and \
             change settings; output and files may be sent to the technician \
             and their AI service."
            .to_owned(),
    }));
}

#[test]
fn round_trip_control_consent_granted() {
    round_trip(control(ControlMessage::ConsentGranted {
        target: simulated_target(),
    }));
}

#[test]
fn round_trip_control_consent_denied() {
    round_trip(control(ControlMessage::ConsentDenied {
        reason: "customer declined".to_owned(),
    }));
}

#[test]
fn round_trip_control_revoke() {
    for reason in [
        RevokeReason::TechnicianRequested,
        RevokeReason::CustomerRequested,
        RevokeReason::Expired,
        RevokeReason::Error,
    ] {
        round_trip(control(ControlMessage::Revoke { reason }));
    }
}

#[test]
fn round_trip_control_heartbeat() {
    round_trip(control(ControlMessage::Heartbeat { seq: 42 }));
}

// The compare-confirm frame is fieldless on the wire: it signals only that the
// human Session Approval Code comparison passed, carrying no key material and no
// code (REQ-S-018, ST-M002-013).
#[test]
fn round_trip_control_approval_compare_confirmed() {
    round_trip(control(ControlMessage::ApprovalCompareConfirmed {}));
}

#[test]
fn round_trip_control_reboot_resume_armed() {
    round_trip(control(ControlMessage::RebootResumeArmed {
        ticket_id: B64Bytes(vec![0xAB; REBOOT_RESUME_TICKET_ID_LEN]),
        target: simulated_target(),
        not_after_unix_millis: 1_760_000_000_000,
    }));
}

#[test]
fn round_trip_control_reboot_resume_disarmed() {
    round_trip(control(ControlMessage::RebootResumeDisarmed {
        ticket_id: B64Bytes(vec![0x01; REBOOT_RESUME_TICKET_ID_LEN]),
    }));
}

#[test]
fn reboot_resume_armed_wire_format_is_stable() {
    let session = SessionId::new();
    let bytes = Message::Control {
        session,
        control: ControlMessage::RebootResumeArmed {
            // 16 zero bytes base64-encode to a fixed, known string.
            ticket_id: B64Bytes(vec![0u8; REBOOT_RESUME_TICKET_ID_LEN]),
            target: simulated_target(),
            not_after_unix_millis: 42,
        },
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["control"]["control"], "reboot_resume_armed");
    assert_eq!(value["control"]["ticket_id"], "AAAAAAAAAAAAAAAAAAAAAA==");
    assert_eq!(value["control"]["not_after_unix_millis"], 42);
}

#[test]
fn reboot_resume_control_rejects_unknown_field() {
    // deny_unknown_fields must hold on the new variants (REQ-N-012): an extra
    // key is a schema change, never a silent extension.
    let err = serde_json::from_str::<ControlMessage>(
        r#"{"control":"reboot_resume_disarmed","ticket_id":"AAAAAAAAAAAAAAAAAAAAAA==","rogue":1}"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("rogue") || err.to_string().contains("unknown field"));
}

#[test]
fn oversized_reboot_resume_ticket_id_rejected() {
    assert_limit_exceeded(&control(ControlMessage::RebootResumeDisarmed {
        ticket_id: B64Bytes(vec![0u8; REBOOT_RESUME_TICKET_ID_LEN + 1]),
    }));
    assert_limit_exceeded(&control(ControlMessage::RebootResumeArmed {
        ticket_id: B64Bytes(vec![0u8; REBOOT_RESUME_TICKET_ID_LEN + 1]),
        target: simulated_target(),
        not_after_unix_millis: 0,
    }));
}

#[test]
fn round_trip_control_reboot_resume_redemption_messages() {
    // The bilateral arming exchange (ADR-0014 §2) and the post-reboot
    // challenge/response (§3) all round-trip through the wire codec.
    round_trip(control(ControlMessage::RebootResumeArmCommit {
        na_commit: B64Bytes(vec![0x11; REBOOT_RESUME_NONCE_LEN]),
    }));
    round_trip(control(ControlMessage::RebootResumeArmChallenge {
        nc_rr: B64Bytes(vec![0x22; REBOOT_RESUME_NONCE_LEN]),
    }));
    round_trip(control(ControlMessage::RebootResumeArmReveal {
        na_rr: B64Bytes(vec![0x33; REBOOT_RESUME_NONCE_LEN]),
    }));
    round_trip(control(ControlMessage::RebootResumeChallenge {
        nc_prime: B64Bytes(vec![0x44; REBOOT_RESUME_NONCE_LEN]),
    }));
    round_trip(control(ControlMessage::RebootResumeProof {
        ticket_id: B64Bytes(vec![0x55; REBOOT_RESUME_TICKET_ID_LEN]),
        proof: B64Bytes(vec![0x66; REBOOT_RESUME_PROOF_LEN]),
    }));
}

#[test]
fn oversized_reboot_resume_redemption_fields_rejected() {
    // Every fixed-length field is bounded on both send and receive, so an
    // oversized commitment, nonce, challenge, or proof fails closed.
    assert_limit_exceeded(&control(ControlMessage::RebootResumeArmCommit {
        na_commit: B64Bytes(vec![0u8; REBOOT_RESUME_NONCE_LEN + 1]),
    }));
    assert_limit_exceeded(&control(ControlMessage::RebootResumeArmChallenge {
        nc_rr: B64Bytes(vec![0u8; REBOOT_RESUME_NONCE_LEN + 1]),
    }));
    assert_limit_exceeded(&control(ControlMessage::RebootResumeArmReveal {
        na_rr: B64Bytes(vec![0u8; REBOOT_RESUME_NONCE_LEN + 1]),
    }));
    assert_limit_exceeded(&control(ControlMessage::RebootResumeChallenge {
        nc_prime: B64Bytes(vec![0u8; REBOOT_RESUME_NONCE_LEN + 1]),
    }));
    assert_limit_exceeded(&control(ControlMessage::RebootResumeProof {
        ticket_id: B64Bytes(vec![0u8; REBOOT_RESUME_TICKET_ID_LEN]),
        proof: B64Bytes(vec![0u8; REBOOT_RESUME_PROOF_LEN + 1]),
    }));
    assert_limit_exceeded(&control(ControlMessage::RebootResumeProof {
        ticket_id: B64Bytes(vec![0u8; REBOOT_RESUME_TICKET_ID_LEN + 1]),
        proof: B64Bytes(vec![0u8; REBOOT_RESUME_PROOF_LEN]),
    }));
}

#[test]
fn reboot_resume_challenge_wire_format_is_stable() {
    // Freeze the tag of the post-reboot challenge so a rename is a conscious
    // wire-compat break.
    let session = SessionId::new();
    let bytes = Message::Control {
        session,
        control: ControlMessage::RebootResumeChallenge {
            nc_prime: B64Bytes(vec![0u8; REBOOT_RESUME_NONCE_LEN]),
        },
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["control"]["control"], "reboot_resume_challenge");
}

#[test]
fn reboot_resume_capability_string_is_frozen() {
    // The wire capability token is a compatibility contract; pin it.
    assert_eq!(CAP_REBOOT_RESUME_V1, "reboot_resume.v1");
}

#[test]
fn canonical_capabilities_are_frozen_for_the_current_version() {
    // The prologue capability set is a pure function of the negotiated version
    // so both endpoints compute it identically (ST-M002-008, REQ-S-008). Pin the
    // exact set for the shipped version; a change here is a wire change.
    assert_eq!(
        canonical_capabilities(PROTOCOL_V1),
        vec![
            CAP_FS_V1.to_string(),
            CAP_TERM_V1.to_string(),
            CAP_REBOOT_RESUME_V1.to_string(),
            CAP_DESKTOP_V1.to_string(),
        ]
    );
}

#[test]
fn canonical_capabilities_grow_by_minor_and_ignore_other_majors() {
    // Each capability was added by exactly one minor; earlier minors carry
    // fewer. This is what keeps the prologue set consistent with the version
    // the prologue already binds.
    assert_eq!(
        canonical_capabilities(ProtocolVersion::new(1, 0)),
        Vec::<String>::new()
    );
    assert_eq!(
        canonical_capabilities(ProtocolVersion::new(1, 1)),
        vec![CAP_FS_V1.to_string()]
    );
    assert_eq!(
        canonical_capabilities(ProtocolVersion::new(1, 4)),
        vec![CAP_FS_V1.to_string(), CAP_TERM_V1.to_string()]
    );
    // A different major never inherits major-1 tokens by minor number alone.
    assert_eq!(
        canonical_capabilities(ProtocolVersion::new(2, 6)),
        Vec::<String>::new()
    );
}

#[test]
fn control_wire_format_is_stable() {
    let session = SessionId::new();
    let bytes = Message::Control {
        session,
        control: ControlMessage::Revoke {
            reason: RevokeReason::CustomerRequested,
        },
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value,
        json!({
            "type": "control",
            "session": session.to_string(),
            "control": { "control": "revoke", "reason": "customer_requested" },
        })
    );
}

// The compare-confirm frame pins to exactly one tag and no fields, so an older
// peer can reject the tag it does not know rather than misparse a payload
// (REQ-N-012, ST-M002-013).
#[test]
fn approval_compare_confirmed_wire_format_is_stable() {
    let session = SessionId::new();
    let bytes = Message::Control {
        session,
        control: ControlMessage::ApprovalCompareConfirmed {},
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value,
        json!({
            "type": "control",
            "session": session.to_string(),
            "control": { "control": "approval_compare_confirmed" },
        })
    );
}

// deny_unknown_fields holds on the fieldless variant too: a tampered frame that
// smuggles a field alongside the compare-confirm tag is rejected, so nothing —
// least of all an approval code — can ride the gate frame (REQ-N-012,
// REQ-S-018).
#[test]
fn approval_compare_confirmed_rejects_unknown_field() {
    let payload = json!({
        "type": "control",
        "session": SessionId::new().to_string(),
        "control": { "control": "approval_compare_confirmed", "code": "11-22-33" },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

// An agent on a pre-minor-8 binary has no `approval_compare_confirmed` arm, so
// it rejects the frame and fails the session closed rather than silently
// skipping the compare gate — the step cannot be downgraded away (REQ-S-008,
// REQ-S-009). Modelled by an older-shaped decoder that faithfully lacks the
// variant but keeps the same internally-tagged, unknown-rejecting shape.
#[test]
fn older_minor_rejects_the_compare_confirm_frame_fail_closed() {
    #[derive(serde::Deserialize)]
    #[serde(tag = "control", rename_all = "snake_case", deny_unknown_fields)]
    #[allow(dead_code)]
    enum OlderControlMessage {
        ConsentRequested { disclosure: String },
        ConsentGranted { target: serde_json::Value },
        ConsentDenied { reason: String },
        Revoke { reason: serde_json::Value },
        Heartbeat { seq: u64 },
    }

    let confirmed = serde_json::to_value(ControlMessage::ApprovalCompareConfirmed {}).unwrap();
    // The older decoder never treats the frame as a no-op: it fails the decode,
    // which the runtime turns into a fail-closed session teardown.
    assert!(
        serde_json::from_value::<OlderControlMessage>(confirmed).is_err(),
        "a pre-minor-8 peer must reject the compare-confirm frame, not skip the gate"
    );
}

#[test]
fn oversized_control_disclosure_rejected() {
    assert_limit_exceeded(&control(ControlMessage::ConsentRequested {
        disclosure: "x".repeat(limits::MAX_DISCLOSURE_BYTES + 1),
    }));
}

#[test]
fn oversized_control_reason_rejected() {
    assert_limit_exceeded(&control(ControlMessage::ConsentDenied {
        reason: "x".repeat(limits::MAX_REASON_BYTES + 1),
    }));
}

#[test]
fn unknown_control_field_rejected() {
    let payload = json!({
        "type": "control",
        "session": SessionId::new().to_string(),
        "control": { "control": "heartbeat", "seq": 1, "extra": true },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

#[test]
fn unknown_control_kind_rejected() {
    let payload = json!({
        "type": "control",
        "session": SessionId::new().to_string(),
        "control": { "control": "surprise" },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

// --- wire-format goldens (schema compatibility, REQ-N-012) ---

#[test]
fn hello_wire_format_is_stable() {
    let bytes = Message::Hello {
        protocol: PROTOCOL_V1,
        role: Role::Console,
        capabilities: vec!["exec".to_owned()],
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value,
        json!({
            "type": "hello",
            // Deliberately updated with the 1.9 minor bump (ST-M009-007).
            "protocol": { "major": 1, "minor": 9 },
            "role": "console",
            "capabilities": ["exec"],
        })
    );
}

#[test]
fn exec_request_wire_format_is_stable() {
    let session = SessionId::new();
    let bytes = Message::Request {
        id: RequestId(7),
        session,
        origin: Origin::Console,
        purpose: None,
        op: exec_operation(),
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value,
        json!({
            "type": "request",
            "id": 7,
            "session": session.to_string(),
            "op": {
                "op": "exec",
                "shell": "power_shell",
                "command": "sim:ok hello",
                "working_dir": "C:\\Windows",
                "env": { "CRETLINK_TEST": "1" },
                "timeout_ms": 30_000,
            },
        }),
        "a console-origin request omits the origin field on the wire, so the \
         minor-2 golden is unchanged (REQ-N-012)"
    );
}

#[test]
fn mcp_origin_request_serializes_origin() {
    // A non-default origin IS carried on the wire (that is the whole point of
    // the field): the agent can then attribute the operation to the AI.
    let session = SessionId::new();
    let bytes = Message::Request {
        id: RequestId(7),
        session,
        origin: Origin::Mcp,
        purpose: None,
        op: Operation::SystemInfo {},
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["origin"], json!("mcp"));
}

#[test]
fn request_without_origin_defaults_to_console() {
    // A minor-2-shaped request omits `origin`; a minor-3 decoder fills the
    // default rather than rejecting the frame (REQ-N-012).
    let payload = json!({
        "type": "request",
        "id": 7,
        "session": SessionId::new().to_string(),
        "op": { "op": "system_info" },
    });
    match Message::decode(payload.to_string().as_bytes()).unwrap() {
        Message::Request { origin, .. } => assert_eq!(origin, Origin::Console),
        other => panic!("expected request, got {other:?}"),
    }
}

#[test]
fn round_trip_request_carries_origin() {
    for origin in [Origin::Console, Origin::Mcp, Origin::Terminal] {
        round_trip(Message::Request {
            id: RequestId(7),
            session: SessionId::new(),
            origin,
            purpose: None,
            op: Operation::SystemInfo {},
        });
    }
}

// --- Stated purpose of AI actions (REQ-F-026, ST-M009-007, minor 9) ---

fn purpose(text: &str) -> Purpose {
    Purpose::new(text).unwrap()
}

#[test]
fn purpose_is_trimmed_and_bounded_in_characters() {
    assert_eq!(
        purpose("  Checking free disk space  ").as_str(),
        "Checking free disk space"
    );
    // The bound counts characters, not bytes: 200 multi-byte characters pass.
    let at_limit = "é".repeat(limits::MAX_PURPOSE_CHARS);
    assert_eq!(purpose(&at_limit).as_str().chars().count(), 200);
    // Surrounding whitespace does not count against the bound.
    let padded = format!("  {}  ", "a".repeat(limits::MAX_PURPOSE_CHARS));
    assert!(Purpose::new(&padded).is_ok());
}

#[test]
fn purpose_rejects_empty_whitespace_too_long_and_multi_line() {
    assert_eq!(Purpose::new(""), Err(PurposeError::Empty));
    assert_eq!(Purpose::new(" \t  "), Err(PurposeError::Empty));
    assert_eq!(
        Purpose::new(&"a".repeat(limits::MAX_PURPOSE_CHARS + 1)),
        Err(PurposeError::TooLong {
            chars: limits::MAX_PURPOSE_CHARS + 1,
            max: limits::MAX_PURPOSE_CHARS,
        })
    );
    for bad in [
        "Checking disk\nspace",
        "Checking disk\r\nspace",
        "Checking\tdisk space",
        "Checking\u{0}disk",
        "Checking\u{7f}disk",
        "Checking\u{85}disk",
        "Checking\u{2028}disk",
        "Checking\u{2029}disk",
        // A right-to-left override could visually reorder the line the
        // customer reads; it is refused outright.
        "Checking \u{202e}ksid",
        "Checking \u{2066}disk\u{2069}",
    ] {
        assert_eq!(
            Purpose::new(bad),
            Err(PurposeError::NotSingleLine),
            "{bad:?} must be rejected"
        );
    }
    // Every rejection message tells the caller what to send instead.
    assert!(PurposeError::Empty.to_string().contains("short sentence"));
}

#[test]
fn request_without_purpose_keeps_the_minor_8_wire_form() {
    // `purpose: None` is omitted, so the frame is byte-for-byte what a minor-8
    // peer produces and decodes (REQ-N-012).
    let bytes = request(Operation::SystemInfo {}).encode().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value.get("purpose").is_none(), "{value}");
}

#[test]
fn request_without_purpose_field_decodes_to_none() {
    // A minor-8-shaped request has no `purpose`; a minor-9 decoder fills `None`
    // rather than rejecting the frame (REQ-N-012).
    let payload = json!({
        "type": "request",
        "id": 7,
        "session": SessionId::new().to_string(),
        "origin": "mcp",
        "op": { "op": "system_info" },
    });
    match Message::decode(payload.to_string().as_bytes()).unwrap() {
        Message::Request {
            origin, purpose, ..
        } => {
            assert_eq!(origin, Origin::Mcp);
            assert_eq!(purpose, None);
        }
        other => panic!("expected request, got {other:?}"),
    }
}

#[test]
fn round_trip_request_and_cancel_carry_purpose() {
    let stated = purpose("Checking free disk space");
    let message = Message::Request {
        id: RequestId(7),
        session: SessionId::new(),
        origin: Origin::Mcp,
        purpose: Some(stated.clone()),
        op: Operation::SystemInfo {},
    };
    round_trip(message.clone());
    let value: serde_json::Value = serde_json::from_slice(&message.encode().unwrap()).unwrap();
    assert_eq!(value["purpose"], json!("Checking free disk space"));

    round_trip(Message::Cancel {
        id: RequestId(8),
        session: SessionId::new(),
        origin: Origin::Mcp,
        purpose: Some(stated),
        job: JobId(3),
    });
}

#[test]
fn cancel_without_origin_or_purpose_keeps_the_minor_8_wire_form() {
    let session = SessionId::new();
    let bytes = Message::Cancel {
        id: RequestId(5),
        session,
        origin: Origin::Console,
        purpose: None,
        job: JobId(9),
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value,
        json!({ "type": "cancel", "id": 5, "session": session.to_string(), "job": 9 })
    );
    // And the minor-8 form decodes with the defaults.
    match Message::decode(&bytes).unwrap() {
        Message::Cancel {
            origin, purpose, ..
        } => {
            assert_eq!(origin, Origin::Console);
            assert_eq!(purpose, None);
        }
        other => panic!("expected cancel, got {other:?}"),
    }
}

#[test]
fn decode_rejects_an_invalid_purpose() {
    // A peer cannot smuggle an empty, multi-line, or over-long purpose past the
    // decoder: the frame fails closed as malformed (ST-M009-007).
    for bad in [
        json!(""),
        json!("   "),
        json!("line one\nline two"),
        json!("a".repeat(limits::MAX_PURPOSE_CHARS + 1)),
        json!(42),
    ] {
        let payload = json!({
            "type": "request",
            "id": 7,
            "session": SessionId::new().to_string(),
            "origin": "mcp",
            "purpose": bad,
            "op": { "op": "system_info" },
        });
        let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
        assert_eq!(err.code(), ErrorCode::MalformedMessage, "{bad}: {err}");
    }
}

#[test]
fn decoded_purpose_is_trimmed() {
    let payload = json!({
        "type": "request",
        "id": 7,
        "session": SessionId::new().to_string(),
        "origin": "mcp",
        "purpose": "  Checking free disk space ",
        "op": { "op": "system_info" },
    });
    match Message::decode(payload.to_string().as_bytes()).unwrap() {
        Message::Request {
            purpose: decoded, ..
        } => {
            assert_eq!(decoded, Some(purpose("Checking free disk space")));
        }
        other => panic!("expected request, got {other:?}"),
    }
}

#[test]
fn stdin_data_serializes_as_base64_string() {
    let session = SessionId::new();
    let bytes = Message::Request {
        id: RequestId(1),
        session,
        origin: Origin::Console,
        purpose: None,
        op: Operation::WriteStdin {
            job: JobId(3),
            data: B64Bytes::new(vec![1, 2, 3]),
        },
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["op"]["data"], json!("AQID"));
}

#[test]
fn pairing_commit_wire_format_is_stable() {
    // The 32-byte fields travel as base64 strings inside a nested, internally
    // tagged pairing object (ADR-0003).
    let bytes = Message::Pairing {
        pairing: PairingMessage::Commit {
            commitment: B64Bytes::new(vec![1, 2, 3]),
        },
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value,
        json!({
            "type": "pairing",
            "pairing": { "kind": "commit", "commitment": "AQID" },
        })
    );
}

#[test]
fn pairing_reveal_serializes_nonce_as_base64() {
    let bytes = Message::Pairing {
        pairing: PairingMessage::Reveal {
            na: B64Bytes::new(vec![0, 255]),
        },
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["pairing"]["kind"], json!("reveal"));
    assert_eq!(value["pairing"]["na"], json!("AP8="));
}

#[test]
fn b64_debug_redacts_content() {
    let debug = format!("{:?}", B64Bytes::new(b"secret".to_vec()));
    assert_eq!(debug, "B64Bytes(len=6)");
    assert!(!debug.contains("secret"));
}

#[test]
fn invalid_base64_is_malformed() {
    let payload = json!({
        "type": "request",
        "id": 1,
        "session": SessionId::new().to_string(),
        "op": { "op": "write_stdin", "job": 3, "data": "!!not base64!!" },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

// --- every limit violated once (bounded inputs, REQ-N-012) ---

#[test]
fn too_many_capabilities_rejected() {
    assert_limit_exceeded(&Message::Hello {
        protocol: PROTOCOL_V1,
        role: Role::Console,
        capabilities: vec!["cap".to_owned(); limits::MAX_CAPABILITIES + 1],
    });
}

#[test]
fn oversized_capability_rejected() {
    assert_limit_exceeded(&Message::Hello {
        protocol: PROTOCOL_V1,
        role: Role::Agent,
        capabilities: vec!["c".repeat(limits::MAX_CAPABILITY_LEN + 1)],
    });
}

#[test]
fn oversized_command_rejected() {
    assert_limit_exceeded(&request(Operation::Exec {
        shell: Shell::Cmd,
        command: "x".repeat(limits::MAX_COMMAND_BYTES + 1),
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
    }));
}

#[test]
fn oversized_working_dir_rejected() {
    assert_limit_exceeded(&request(Operation::Exec {
        shell: Shell::Cmd,
        command: "dir".to_owned(),
        working_dir: Some(format!("C:\\{}", "p".repeat(limits::MAX_PATH_BYTES))),
        env: BTreeMap::new(),
        timeout_ms: None,
    }));
}

/// Builds an `Exec` request whose only suspect field is `working_dir`.
fn exec_with_working_dir(dir: &str) -> Message {
    request(Operation::Exec {
        shell: Shell::Cmd,
        command: "dir".to_owned(),
        working_dir: Some(dir.to_owned()),
        env: BTreeMap::new(),
        timeout_ms: None,
    })
}

/// `Exec.working_dir` receives the same adversarial path rules as an absolute
/// `RemotePath`: traversal, UNC shares, device namespaces, ADS colons, and
/// relative paths are rejected on both ends (ST-M003-003, REQ-F-009).
#[test]
fn adversarial_working_dir_is_rejected() {
    for bad in [
        // Traversal (both separators are normalized).
        "C:\\Users\\..\\Windows\\system32",
        "C:/Users/../Windows",
        // UNC shares and device namespaces.
        "\\\\server\\share\\dir",
        "\\\\?\\C:\\Windows",
        "\\\\.\\PhysicalDrive0",
        // Alternate-data-stream colon inside a component.
        "C:\\Temp\\dir:stream",
        // Relative paths: never ambiguous between local and remote.
        "Windows\\system32",
        "..\\up",
        ".",
    ] {
        assert_invalid_path(&exec_with_working_dir(bad));
    }
}

#[test]
fn drive_qualified_working_dir_is_accepted() {
    for good in ["C:\\", "C:\\Users\\customer", "D:/logs/app"] {
        exec_with_working_dir(good).validate().expect(good);
    }
}

/// A bare drive designator is drive-RELATIVE on Windows (Win32 resolves it
/// against the acting process's per-drive current directory), so it is
/// rejected everywhere the absolute-path rules apply: `Exec.working_dir` and
/// the shared file-operation validator alike (ST-M003-003).
#[test]
fn bare_drive_designator_is_rejected_as_drive_relative() {
    for bad in ["C:", "c:"] {
        assert_invalid_path(&exec_with_working_dir(bad));
        assert_invalid_path(&request(Operation::FsStat { path: abs(bad) }));
    }
    // The rooted forms stay accepted on both surfaces.
    exec_with_working_dir("C:\\").validate().expect("C:\\ exec");
    request(Operation::FsStat { path: abs("C:\\") })
        .validate()
        .expect("C:\\ stat");
}

/// An `exec_started` result without `working_dir` (a same-major peer on an
/// older minor) still decodes; the field defaults to the documented honest
/// empty marker (REQ-N-012).
#[test]
fn exec_started_without_working_dir_decodes_for_older_minors() {
    let session = SessionId::new();
    let payload = json!({
        "type": "response",
        "id": 2,
        "session": session.to_string(),
        "result": { "result": "exec_started", "job": 9 },
    });
    match Message::decode(payload.to_string().as_bytes()).unwrap() {
        Message::Response {
            result: OpResult::ExecStarted { job, working_dir },
            ..
        } => {
            assert_eq!(job, JobId(9));
            assert!(
                working_dir.is_empty(),
                "an absent field means \"not reported\", never an invented path"
            );
        }
        other => panic!("expected exec_started, got {other:?}"),
    }
}

/// The `exec_started` result's effective working directory is bounded.
#[test]
fn oversized_exec_started_working_dir_rejected() {
    assert_limit_exceeded(&Message::Response {
        id: RequestId(2),
        session: SessionId::new(),
        result: OpResult::ExecStarted {
            job: JobId(9),
            working_dir: "w".repeat(limits::MAX_PATH_BYTES + 1),
        },
    });
}

#[test]
fn too_many_env_entries_rejected() {
    let env: BTreeMap<String, String> = (0..=limits::MAX_ENV_ENTRIES)
        .map(|index| (format!("KEY_{index}"), "value".to_owned()))
        .collect();
    assert_limit_exceeded(&request(Operation::Exec {
        shell: Shell::Cmd,
        command: "dir".to_owned(),
        working_dir: None,
        env,
        timeout_ms: None,
    }));
}

#[test]
fn oversized_env_key_rejected() {
    assert_limit_exceeded(&request(Operation::Exec {
        shell: Shell::Cmd,
        command: "dir".to_owned(),
        working_dir: None,
        env: BTreeMap::from([("K".repeat(limits::MAX_ENV_KEY + 1), "value".to_owned())]),
        timeout_ms: None,
    }));
}

#[test]
fn oversized_env_value_rejected() {
    assert_limit_exceeded(&request(Operation::Exec {
        shell: Shell::Cmd,
        command: "dir".to_owned(),
        working_dir: None,
        env: BTreeMap::from([("KEY".to_owned(), "v".repeat(limits::MAX_ENV_VALUE + 1))]),
        timeout_ms: None,
    }));
}

#[test]
fn oversized_timeout_rejected() {
    assert_limit_exceeded(&request(Operation::Exec {
        shell: Shell::Cmd,
        command: "dir".to_owned(),
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: Some(limits::MAX_TIMEOUT_MS + 1),
    }));
}

#[test]
fn oversized_stdin_chunk_rejected() {
    assert_limit_exceeded(&request(Operation::WriteStdin {
        job: JobId(1),
        data: B64Bytes::new(vec![0; limits::MAX_CHUNK_BYTES + 1]),
    }));
}

#[test]
fn oversized_output_chunk_rejected() {
    assert_limit_exceeded(&Message::Event {
        session: SessionId::new(),
        event: EventKind::JobOutput {
            job: JobId(1),
            stream: OutputStream::Stdout,
            data: B64Bytes::new(vec![0; limits::MAX_CHUNK_BYTES + 1]),
        },
    });
}

#[test]
fn oversized_error_message_rejected() {
    assert_limit_exceeded(&Message::Error {
        id: None,
        session: None,
        code: ErrorCode::Internal,
        message: "e".repeat(limits::MAX_ERROR_MESSAGE + 1),
        stage: ErrorStage::Execution,
    });
}

#[test]
fn oversized_pairing_commitment_rejected() {
    assert_limit_exceeded(&Message::Pairing {
        pairing: PairingMessage::Commit {
            commitment: B64Bytes::new(vec![0; SAC_COMMITMENT_LEN + 1]),
        },
    });
}

#[test]
fn oversized_pairing_nonce_rejected() {
    assert_limit_exceeded(&Message::Pairing {
        pairing: PairingMessage::Nonce {
            nc: B64Bytes::new(vec![0; SAC_NONCE_LEN + 1]),
        },
    });
}

#[test]
fn oversized_pairing_reveal_rejected() {
    assert_limit_exceeded(&Message::Pairing {
        pairing: PairingMessage::Reveal {
            na: B64Bytes::new(vec![0; SAC_NONCE_LEN + 1]),
        },
    });
}

#[test]
fn too_many_jobs_listed_rejected() {
    let jobs: Vec<JobSummary> = (0..=limits::MAX_JOBS_LISTED as u64)
        .map(|index| JobSummary {
            job: JobId(index),
            command_preview: "sim:hang".to_owned(),
            running: true,
        })
        .collect();
    assert_limit_exceeded(&Message::Response {
        id: RequestId(1),
        session: SessionId::new(),
        result: OpResult::Jobs { jobs },
    });
}

#[test]
fn oversized_command_preview_rejected() {
    assert_limit_exceeded(&Message::Response {
        id: RequestId(1),
        session: SessionId::new(),
        result: OpResult::Jobs {
            jobs: vec![JobSummary {
                job: JobId(1),
                command_preview: "x".repeat(limits::MAX_COMMAND_BYTES + 1),
                running: false,
            }],
        },
    });
}

#[test]
fn decode_validates_limits_too() {
    // A syntactically valid frame whose field violates a bound must fail
    // validation on the receive path as well.
    let payload = json!({
        "type": "error",
        "id": null,
        "session": null,
        "code": "internal",
        "message": "e".repeat(limits::MAX_ERROR_MESSAGE + 1),
        "stage": "execution",
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::LimitExceeded);
}

// --- frame-size cap ---

#[test]
fn oversized_input_rejected_before_parse() {
    // Garbage that is far from valid JSON: if the size check did not run
    // first, decoding would report MalformedMessage instead.
    let bytes = vec![b'{'; limits::MAX_FRAME_BYTES + 1];
    let err = Message::decode(&bytes).unwrap_err();
    assert!(matches!(err, ProtocolError::Oversized { .. }), "{err}");
    assert_eq!(err.code(), ErrorCode::OversizedMessage);
}

#[test]
fn encode_rejects_oversized_frame() {
    // Every field is within its own bound, but the whole frame exceeds
    // MAX_FRAME_BYTES.
    let jobs: Vec<JobSummary> = (0..limits::MAX_JOBS_LISTED as u64)
        .map(|index| JobSummary {
            job: JobId(index),
            command_preview: "x".repeat(limits::MAX_COMMAND_BYTES),
            running: false,
        })
        .collect();
    let message = Message::Response {
        id: RequestId(1),
        session: SessionId::new(),
        result: OpResult::Jobs { jobs },
    };
    let err = message.encode().unwrap_err();
    assert!(matches!(err, ProtocolError::Oversized { .. }), "{err}");
    assert_eq!(err.code(), ErrorCode::OversizedMessage);
}

// --- unknown-field policy (REQ-N-012) ---

#[test]
fn unknown_top_level_field_rejected() {
    let payload = json!({
        "type": "hello",
        "protocol": { "major": 1, "minor": 0 },
        "role": "console",
        "capabilities": [],
        "surprise": true,
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

#[test]
fn unknown_nested_struct_field_rejected() {
    let payload = json!({
        "type": "hello_ack",
        "protocol": { "major": 1, "minor": 0 },
        "session": SessionId::new().to_string(),
        "target": {
            "hostname": "SIMULATED-TARGET",
            "os_version": "Simulated Windows",
            "arch": "x86_64",
            "elevated": false,
            "simulated": true,
            "agent_version": "0.0.0",
            "extra": "field",
        },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

#[test]
fn unknown_operation_field_rejected() {
    // Payload-free variants are empty struct variants precisely so this
    // rejection holds; serde ignores deny_unknown_fields for unit
    // variants of internally tagged enums.
    let payload = json!({
        "type": "request",
        "id": 1,
        "session": SessionId::new().to_string(),
        "op": { "op": "system_info", "extra": 1 },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

#[test]
fn unknown_result_field_rejected() {
    let payload = json!({
        "type": "response",
        "id": 1,
        "session": SessionId::new().to_string(),
        "result": { "result": "ack", "extra": 1 },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

#[test]
fn unknown_pairing_field_rejected() {
    // An extra field inside the nested pairing object must be rejected
    // (deny_unknown_fields holds through the nested PairingMessage too).
    let payload = json!({
        "type": "pairing",
        "pairing": { "kind": "commit", "commitment": "AQID", "extra": 1 },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

#[test]
fn unknown_pairing_kind_rejected() {
    let payload = json!({
        "type": "pairing",
        "pairing": { "kind": "surprise", "commitment": "AQID" },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

#[test]
fn pairing_wrong_field_for_kind_rejected() {
    // `na` belongs to `reveal`, not `commit`; the mismatch is an unknown
    // field for the selected variant.
    let payload = json!({
        "type": "pairing",
        "pairing": { "kind": "commit", "na": "AQID" },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

#[test]
fn payload_free_variants_serialize_as_tag_only() {
    let session = SessionId::new();
    let bytes = Message::Request {
        id: RequestId(1),
        session,
        origin: Origin::Console,
        purpose: None,
        op: Operation::SystemInfo {},
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["op"], json!({ "op": "system_info" }));
}

// --- version negotiation (REQ-N-012) ---

#[test]
fn negotiation_requires_equal_major() {
    let local = ProtocolVersion::new(1, 3);
    let offered = ProtocolVersion::new(2, 0);
    assert!(!local.is_compatible_with(offered));
    let err = local.negotiate(offered).unwrap_err();
    assert!(
        matches!(err, ProtocolError::IncompatibleVersion { .. }),
        "{err}"
    );
    assert_eq!(err.code(), ErrorCode::IncompatibleVersion);
}

#[test]
fn negotiation_answers_with_own_minor() {
    let responder = ProtocolVersion::new(1, 3);
    let offered = ProtocolVersion::new(1, 9);
    assert!(responder.is_compatible_with(offered));
    let answered = responder.negotiate(offered).unwrap();
    assert_eq!(answered, responder);
}

#[test]
fn protocol_v1_is_one_dot_nine() {
    assert_eq!(PROTOCOL_V1, ProtocolVersion::new(1, 9));
    assert_eq!(PROTOCOL_V1.to_string(), "1.9");
}

// --- File operations and transfer (ADR-0012, ST-M003-004/005) ---

fn digest32() -> B64Bytes {
    B64Bytes::new(vec![0u8; limits::DIGEST_LEN])
}

fn abs(path: &str) -> RemotePath {
    RemotePath::Absolute {
        path: path.to_owned(),
    }
}

fn staging(name: &str) -> RemotePath {
    RemotePath::Staging {
        name: name.to_owned(),
    }
}

fn response(result: OpResult) -> Message {
    Message::Response {
        id: RequestId(11),
        session: SessionId::new(),
        result,
    }
}

fn file_entry(name: &str) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        kind: FileKind::File,
        len: 42,
        modified_ms: Some(1_700_000_000_000),
        readonly: false,
    }
}

fn staged_entry() -> StagedEntry {
    StagedEntry {
        canonical_path: "C:\\ProgramData\\CretLink\\staging\\s1\\upload.bin".to_owned(),
        source: "tech:patch".to_owned(),
        len: 3,
        algo: DigestAlgo::Sha256,
        digest: digest32(),
        cleanup: CleanupPolicy::OnSessionEnd,
        created_ms: 1_700_000_000_000,
    }
}

#[track_caller]
fn assert_invalid_path(message: &Message) {
    let err = message.validate().unwrap_err();
    assert!(matches!(err, ProtocolError::InvalidPath { .. }), "{err}");
    assert_eq!(err.code(), ErrorCode::InvalidPath);
    // encode must refuse the same message before serializing it.
    let err = message.encode().unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidPath);
}

// Round-trips for the new file operations.

#[test]
fn round_trip_request_fs_stat() {
    round_trip(request(Operation::FsStat {
        path: abs("C:\\Users\\tech\\report.txt"),
    }));
}

#[test]
fn round_trip_request_fs_list() {
    round_trip(request(Operation::FsList {
        path: abs("C:\\Users\\tech"),
        continue_after: Some("alice.txt".to_owned()),
    }));
    round_trip(request(Operation::FsList {
        path: abs("C:\\"),
        continue_after: None,
    }));
}

#[test]
fn round_trip_request_create_dir() {
    round_trip(request(Operation::CreateDir {
        path: abs("C:\\Temp\\cretlink-out"),
    }));
}

#[test]
fn round_trip_request_fs_read() {
    round_trip(request(Operation::FsRead {
        path: abs("C:\\Temp\\image.bin"),
        offset: 4096,
        len: 65536,
    }));
}

#[test]
fn round_trip_request_fs_write() {
    round_trip(request(Operation::FsWrite {
        dest: staging("upload.bin"),
        offset: 0,
        data: B64Bytes::new(vec![0, 1, 2, 254, 255]),
    }));
}

#[test]
fn round_trip_request_fs_commit() {
    round_trip(request(Operation::FsCommit {
        dest: staging("upload.bin"),
        len: 5,
        algo: DigestAlgo::Sha256,
        digest: digest32(),
        overwrite: false,
        source: Some("tech:driver-package".to_owned()),
        cleanup: CleanupPolicy::OnSessionEnd,
    }));
    round_trip(request(Operation::FsCommit {
        dest: abs("C:\\Temp\\out.bin"),
        len: 0,
        algo: DigestAlgo::Sha256,
        digest: digest32(),
        overwrite: true,
        source: None,
        cleanup: CleanupPolicy::Keep,
    }));
}

#[test]
fn round_trip_request_fs_abort() {
    round_trip(request(Operation::FsAbort {
        dest: staging("upload.bin"),
    }));
}

#[test]
fn round_trip_request_fs_digest() {
    round_trip(request(Operation::FsDigest {
        path: abs("C:\\Temp\\image.bin"),
        prefix_len: Some(1024),
    }));
    round_trip(request(Operation::FsDigest {
        path: abs("C:\\Temp\\image.bin"),
        prefix_len: None,
    }));
}

#[test]
fn round_trip_request_fs_move() {
    round_trip(request(Operation::FsMove {
        from: abs("C:\\Temp\\a.tmp"),
        to: abs("C:\\Temp\\a.txt"),
        overwrite: true,
    }));
}

#[test]
fn round_trip_request_fs_delete_and_remove_dir() {
    round_trip(request(Operation::FsDelete {
        path: abs("C:\\Temp\\a.tmp"),
    }));
    round_trip(request(Operation::RemoveDir {
        path: abs("C:\\Temp\\cretlink-out"),
    }));
}

#[test]
fn round_trip_request_staging() {
    round_trip(request(Operation::ListStaged {}));
    round_trip(request(Operation::StagingCleanup {
        trigger: CleanupTrigger::SessionEnd,
    }));
    round_trip(request(Operation::StagingCleanup {
        trigger: CleanupTrigger::All,
    }));
}

// Round-trips for the new results.

#[test]
fn round_trip_response_path_stat() {
    round_trip(response(OpResult::PathStat {
        entry: Some(file_entry("report.txt")),
        partial_len: None,
    }));
    round_trip(response(OpResult::PathStat {
        entry: None,
        partial_len: Some(4096),
    }));
}

#[test]
fn round_trip_response_dir_entries() {
    round_trip(response(OpResult::DirEntries {
        entries: vec![
            file_entry("a.txt"),
            FileEntry {
                name: "sub".to_owned(),
                kind: FileKind::Dir,
                len: 0,
                modified_ms: None,
                readonly: false,
            },
        ],
        end: true,
    }));
}

#[test]
fn round_trip_response_file_data() {
    round_trip(response(OpResult::FileData {
        data: B64Bytes::new(vec![9, 8, 7, 0, 255]),
        eof: false,
        file_len: 1_000_000,
    }));
}

#[test]
fn round_trip_response_write_accepted() {
    round_trip(response(OpResult::WriteAccepted { part_len: 65536 }));
}

#[test]
fn round_trip_response_committed() {
    round_trip(response(OpResult::Committed {
        canonical_path: "C:\\Temp\\out.bin".to_owned(),
        len: 5,
        algo: DigestAlgo::Sha256,
        digest: digest32(),
        replaced: true,
    }));
}

#[test]
fn round_trip_response_file_digest() {
    round_trip(response(OpResult::FileDigest {
        algo: DigestAlgo::Sha256,
        digest: digest32(),
        len: 5,
    }));
}

#[test]
fn round_trip_response_staged_list() {
    round_trip(response(OpResult::StagedList {
        entries: vec![staged_entry()],
    }));
}

#[test]
fn round_trip_response_cleanup_report() {
    round_trip(response(OpResult::CleanupReport {
        removed: 2,
        failed: vec![CleanupFailure {
            canonical_path: "C:\\ProgramData\\CretLink\\staging\\s1\\locked.bin".to_owned(),
            reason: "file is in use".to_owned(),
        }],
    }));
}

// Limit enforcement on the new fields.

#[test]
fn fs_read_len_over_chunk_is_rejected() {
    assert_limit_exceeded(&request(Operation::FsRead {
        path: abs("C:\\a.bin"),
        offset: 0,
        len: (limits::MAX_CHUNK_BYTES + 1) as u32,
    }));
}

#[test]
fn fs_read_offset_over_max_is_rejected() {
    assert_limit_exceeded(&request(Operation::FsRead {
        path: abs("C:\\a.bin"),
        offset: limits::MAX_FILE_OFFSET + 1,
        len: 16,
    }));
}

#[test]
fn fs_write_data_over_chunk_is_rejected() {
    assert_limit_exceeded(&request(Operation::FsWrite {
        dest: staging("upload.bin"),
        offset: 0,
        data: B64Bytes::new(vec![0u8; limits::MAX_CHUNK_BYTES + 1]),
    }));
}

#[test]
fn fs_commit_len_over_transfer_is_rejected() {
    assert_limit_exceeded(&request(Operation::FsCommit {
        dest: staging("upload.bin"),
        len: limits::MAX_TRANSFER_BYTES + 1,
        algo: DigestAlgo::Sha256,
        digest: digest32(),
        overwrite: false,
        source: None,
        cleanup: CleanupPolicy::OnSessionEnd,
    }));
}

#[test]
fn fs_commit_oversized_digest_is_rejected() {
    assert_limit_exceeded(&request(Operation::FsCommit {
        dest: staging("upload.bin"),
        len: 1,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(vec![0u8; limits::DIGEST_LEN + 1]),
        overwrite: false,
        source: None,
        cleanup: CleanupPolicy::OnSessionEnd,
    }));
}

#[test]
fn fs_commit_oversized_source_is_rejected() {
    assert_limit_exceeded(&request(Operation::FsCommit {
        dest: staging("upload.bin"),
        len: 1,
        algo: DigestAlgo::Sha256,
        digest: digest32(),
        overwrite: false,
        source: Some("s".repeat(limits::MAX_LABEL_BYTES + 1)),
        cleanup: CleanupPolicy::OnSessionEnd,
    }));
}

#[test]
fn fs_list_oversized_cursor_is_rejected() {
    assert_limit_exceeded(&request(Operation::FsList {
        path: abs("C:\\Users"),
        continue_after: Some("n".repeat(limits::MAX_NAME_BYTES + 1)),
    }));
}

#[test]
fn fs_digest_prefix_over_max_is_rejected() {
    assert_limit_exceeded(&request(Operation::FsDigest {
        path: abs("C:\\a.bin"),
        prefix_len: Some(limits::MAX_FILE_OFFSET + 1),
    }));
}

#[test]
fn dir_entries_over_page_is_rejected() {
    let entries = (0..=limits::MAX_DIR_PAGE)
        .map(|i| file_entry(&format!("f{i}")))
        .collect();
    assert_limit_exceeded(&response(OpResult::DirEntries { entries, end: true }));
}

#[test]
fn file_data_over_chunk_is_rejected() {
    assert_limit_exceeded(&response(OpResult::FileData {
        data: B64Bytes::new(vec![0u8; limits::MAX_CHUNK_BYTES + 1]),
        eof: false,
        file_len: 0,
    }));
}

#[test]
fn committed_oversized_digest_is_rejected() {
    assert_limit_exceeded(&response(OpResult::Committed {
        canonical_path: "C:\\Temp\\out.bin".to_owned(),
        len: 1,
        algo: DigestAlgo::Sha256,
        digest: B64Bytes::new(vec![0u8; limits::DIGEST_LEN + 1]),
        replaced: false,
    }));
}

#[test]
fn cleanup_report_oversized_reason_is_rejected() {
    assert_limit_exceeded(&response(OpResult::CleanupReport {
        removed: 0,
        failed: vec![CleanupFailure {
            canonical_path: "C:\\x".to_owned(),
            reason: "r".repeat(limits::MAX_REASON_BYTES + 1),
        }],
    }));
}

// Adversarial path rejection (ADR-0012 path-safety, REQ-S-013).

#[test]
fn absolute_path_traversal_is_rejected() {
    assert_invalid_path(&request(Operation::FsStat {
        path: abs("C:\\Users\\..\\Windows\\system32"),
    }));
    // Forward slashes are normalized, so traversal is caught either way.
    assert_invalid_path(&request(Operation::FsStat {
        path: abs("C:/Users/../Windows"),
    }));
}

#[test]
fn unc_and_device_namespace_paths_are_rejected() {
    for bad in [
        "\\\\server\\share\\file",
        "\\\\?\\C:\\Windows",
        "\\\\.\\PhysicalDrive0",
        "\\\\?\\GLOBALROOT\\Device\\HarddiskVolume1",
        "//server/share/file",
    ] {
        assert_invalid_path(&request(Operation::FsStat { path: abs(bad) }));
    }
}

#[test]
fn reserved_device_names_are_rejected() {
    for bad in [
        "C:\\Temp\\CON",
        "C:\\Temp\\con.txt",
        "C:\\NUL",
        "C:\\COM1",
        "C:\\Temp\\LPT9.log",
        "C:\\PRN",
        "C:\\AUX",
    ] {
        assert_invalid_path(&request(Operation::FsStat { path: abs(bad) }));
    }
}

#[test]
fn alternate_data_stream_colon_is_rejected() {
    assert_invalid_path(&request(Operation::FsStat {
        path: abs("C:\\Temp\\file.txt:hidden"),
    }));
}

#[test]
fn non_drive_qualified_paths_are_rejected() {
    for bad in [
        "\\Windows\\system32",
        "Windows\\system32",
        "report.txt",
        "1:\\bad",
    ] {
        assert_invalid_path(&request(Operation::FsStat { path: abs(bad) }));
    }
}

#[test]
fn trailing_dot_or_space_component_is_rejected() {
    assert_invalid_path(&request(Operation::FsStat {
        path: abs("C:\\Temp\\report."),
    }));
    assert_invalid_path(&request(Operation::FsStat {
        path: abs("C:\\Temp\\report "),
    }));
}

#[test]
fn control_character_in_path_is_rejected() {
    assert_invalid_path(&request(Operation::FsStat {
        path: abs("C:\\Temp\\a\u{0007}b"),
    }));
}

#[test]
fn staging_name_with_separator_or_traversal_is_rejected() {
    for bad in ["../evil", "a/b", "a\\b", "..", ".", "a:b", "CON", ""] {
        assert_invalid_path(&request(Operation::FsStat { path: staging(bad) }));
    }
}

#[test]
fn path_depth_over_max_is_rejected() {
    let mut path = String::from("C:");
    for _ in 0..=limits::MAX_PATH_DEPTH {
        path.push_str("\\a");
    }
    assert_limit_exceeded(&request(Operation::FsStat { path: abs(&path) }));
}

#[test]
fn path_byte_length_over_max_is_rejected() {
    let path = format!("C:\\{}", "a".repeat(limits::MAX_PATH_BYTES));
    assert_limit_exceeded(&request(Operation::FsStat { path: abs(&path) }));
}

#[test]
fn component_byte_length_over_max_is_rejected() {
    let path = format!("C:\\{}", "a".repeat(limits::MAX_NAME_BYTES + 1));
    assert_limit_exceeded(&request(Operation::FsStat { path: abs(&path) }));
}

#[test]
fn valid_paths_are_accepted() {
    for good in [
        "C:\\",
        "C:/",
        "C:\\Users\\tech\\Documents\\report final.txt",
        "C:/Users/tech/image.bin",
        "D:\\a\\b\\c",
    ] {
        request(Operation::FsStat { path: abs(good) })
            .validate()
            .expect(good);
    }
    for good in ["upload.bin", "report.2026.txt", "a b c.log"] {
        request(Operation::FsStat {
            path: staging(good),
        })
        .validate()
        .expect(good);
    }
}

#[test]
fn cap_fs_v1_is_stable() {
    assert_eq!(CAP_FS_V1, "fs.v1");
}

// --- Interactive terminal (ADR-0013, ST-M003-009) ---

#[test]
fn cap_term_v1_is_stable() {
    assert_eq!(CAP_TERM_V1, "term.v1");
}

#[test]
fn round_trip_request_term_open() {
    round_trip(request(Operation::TermOpen {
        shell: Shell::PowerShell,
        cols: 120,
        rows: 30,
        working_dir: Some("C:\\Users\\tech".to_owned()),
        env: BTreeMap::from([("TERM".to_owned(), "xterm-256color".to_owned())]),
    }));
    // The minimal form (no working_dir, no env) also round-trips.
    round_trip(request(Operation::TermOpen {
        shell: Shell::Cmd,
        cols: limits::MIN_TERM_COLS,
        rows: limits::MAX_TERM_ROWS,
        working_dir: None,
        env: BTreeMap::new(),
    }));
}

#[test]
fn round_trip_request_term_resize() {
    round_trip(request(Operation::TermResize {
        job: JobId(4),
        cols: 80,
        rows: 24,
    }));
}

#[test]
fn round_trip_request_term_close() {
    round_trip(request(Operation::TermClose { job: JobId(4) }));
}

#[test]
fn round_trip_response_term_opened() {
    round_trip(response(OpResult::TermOpened { job: JobId(9) }));
}

/// A `TermOpen` omits its default-empty `working_dir`/`env` on the wire, and its
/// dimensions are plain integers (the schema is inspectable, ADR-0010).
#[test]
fn term_open_wire_format_is_stable() {
    let session = SessionId::new();
    let bytes = Message::Request {
        id: RequestId(7),
        session,
        origin: Origin::Terminal,
        purpose: None,
        op: Operation::TermOpen {
            shell: Shell::Cmd,
            cols: 80,
            rows: 24,
            working_dir: None,
            env: BTreeMap::new(),
        },
    }
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["op"],
        json!({ "op": "term_open", "shell": "cmd", "cols": 80, "rows": 24 })
    );
    // A non-default origin (a technician terminal) IS carried on the wire.
    assert_eq!(value["origin"], json!("terminal"));
}

#[test]
fn term_open_dimensions_out_of_bounds_are_rejected() {
    // Above the ceiling on either axis.
    assert_limit_exceeded(&request(Operation::TermOpen {
        shell: Shell::Cmd,
        cols: limits::MAX_TERM_COLS + 1,
        rows: 24,
        working_dir: None,
        env: BTreeMap::new(),
    }));
    assert_limit_exceeded(&request(Operation::TermOpen {
        shell: Shell::Cmd,
        cols: 80,
        rows: limits::MAX_TERM_ROWS + 1,
        working_dir: None,
        env: BTreeMap::new(),
    }));
    // Below the floor (a zero dimension is malformed, never acted on).
    assert_limit_exceeded(&request(Operation::TermOpen {
        shell: Shell::Cmd,
        cols: limits::MIN_TERM_COLS - 1,
        rows: 24,
        working_dir: None,
        env: BTreeMap::new(),
    }));
}

#[test]
fn term_resize_dimensions_out_of_bounds_are_rejected() {
    assert_limit_exceeded(&request(Operation::TermResize {
        job: JobId(1),
        cols: limits::MAX_TERM_COLS + 1,
        rows: 24,
    }));
    assert_limit_exceeded(&request(Operation::TermResize {
        job: JobId(1),
        cols: 80,
        rows: 0,
    }));
}

#[test]
fn term_open_adversarial_working_dir_is_rejected() {
    for bad in [
        "C:\\Users\\..\\Windows",
        "\\\\server\\share\\dir",
        "Windows\\system32",
    ] {
        assert_invalid_path(&request(Operation::TermOpen {
            shell: Shell::Cmd,
            cols: 80,
            rows: 24,
            working_dir: Some(bad.to_owned()),
            env: BTreeMap::new(),
        }));
    }
}

#[test]
fn term_open_oversized_env_is_rejected() {
    assert_limit_exceeded(&request(Operation::TermOpen {
        shell: Shell::Cmd,
        cols: 80,
        rows: 24,
        working_dir: None,
        env: BTreeMap::from([("KEY".to_owned(), "v".repeat(limits::MAX_ENV_VALUE + 1))]),
    }));
}

#[test]
fn unknown_term_open_field_rejected() {
    let payload = json!({
        "type": "request",
        "id": 1,
        "session": SessionId::new().to_string(),
        "op": { "op": "term_open", "shell": "cmd", "cols": 80, "rows": 24, "extra": true },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

// --- Interactive desktop control (ADR-0015, ST-M005-001/002/003) ---

#[test]
fn cap_desktop_v1_is_stable() {
    assert_eq!(CAP_DESKTOP_V1, "desktop.v1");
}

fn desktop_meta(state: DesktopState) -> DesktopFrameMeta {
    DesktopFrameMeta {
        state,
        wts_session: 1,
        desktop: "WinSta0\\Default".to_owned(),
        display: 0,
        width: 1920,
        height: 1080,
        physical_width: 2560,
        physical_height: 1440,
        dpi: 144,
        format: ImageFormat::Png,
        captured_unix_ms: 1_700_000_000_000,
    }
}

#[test]
fn round_trip_request_desktop_capture() {
    round_trip(request(Operation::DesktopCapture {
        display: 1,
        format: ImageFormat::Jpeg,
        max_width: Some(1280),
        max_height: Some(720),
        quality: Some(80),
    }));
    // The minimal form (native size, agent-default quality) also round-trips.
    round_trip(request(Operation::DesktopCapture {
        display: 0,
        format: ImageFormat::Png,
        max_width: None,
        max_height: None,
        quality: None,
    }));
}

#[test]
fn round_trip_request_desktop_pointer() {
    for action in [
        PointerAction::Move,
        PointerAction::Down,
        PointerAction::Up,
        PointerAction::Click,
        PointerAction::DoubleClick,
    ] {
        for button in [
            PointerButton::Left,
            PointerButton::Right,
            PointerButton::Middle,
        ] {
            round_trip(request(Operation::DesktopPointer {
                action,
                button,
                // A negative coordinate (a second monitor left of the primary)
                // is legal and must survive the round-trip.
                x: -1024,
                y: 512,
            }));
        }
    }
}

#[test]
fn round_trip_request_desktop_scroll() {
    round_trip(request(Operation::DesktopScroll {
        x: 100,
        y: 200,
        dx: -3,
        dy: 5,
    }));
}

#[test]
fn round_trip_request_desktop_key_type_hotkey_wait() {
    for action in [KeyAction::Down, KeyAction::Up, KeyAction::Press] {
        round_trip(request(Operation::DesktopKey {
            action,
            key: "enter".to_owned(),
        }));
    }
    round_trip(request(Operation::DesktopType {
        text: "Hello, wêreld — 🌍".to_owned(),
    }));
    round_trip(request(Operation::DesktopHotkey {
        keys: vec!["ctrl".to_owned(), "shift".to_owned(), "esc".to_owned()],
    }));
    round_trip(request(Operation::DesktopWait { millis: 250 }));
}

#[test]
fn round_trip_request_clipboard() {
    round_trip(request(Operation::ClipboardGet {}));
    round_trip(request(Operation::ClipboardSet {
        text: "pasted text".to_owned(),
    }));
    // Clipboard text is Unicode (ST-M005-003): a string mixing scripts, an
    // emoji, and a combining mark must survive the JSON round trip intact so the
    // wire never mangles what an operator pastes.
    round_trip(request(Operation::ClipboardSet {
        text: "café — 日本語 — 🔒 — a\u{0301}".to_owned(),
    }));
}

#[test]
fn round_trip_response_desktop_frame() {
    // Active desktop with an inline image.
    round_trip(response(OpResult::DesktopFrame {
        meta: desktop_meta(DesktopState::Active),
        image: Some(B64Bytes::from(vec![0x89, 0x50, 0x4e, 0x47])),
        staged: None,
    }));
    // Active desktop whose (large) frame was staged instead of inlined.
    round_trip(response(OpResult::DesktopFrame {
        meta: desktop_meta(DesktopState::Active),
        image: None,
        staged: Some(staging("frame-0001.png")),
    }));
    // Every non-Active state carries no image and no stale frame.
    for state in [
        DesktopState::Locked,
        DesktopState::Switched,
        DesktopState::Disconnected,
        DesktopState::NoInputDesktop,
        DesktopState::Secure,
    ] {
        round_trip(response(OpResult::DesktopFrame {
            meta: desktop_meta(state),
            image: None,
            staged: None,
        }));
    }
}

#[test]
fn round_trip_response_clipboard_contents() {
    round_trip(response(OpResult::ClipboardContents {
        text: Some("clipboard text".to_owned()),
    }));
    // Unicode read-back survives intact (ST-M005-003), mirroring the write side.
    round_trip(response(OpResult::ClipboardContents {
        text: Some("café — 日本語 — 🔒 — a\u{0301}".to_owned()),
    }));
    round_trip(response(OpResult::ClipboardContents { text: None }));
}

/// A `DesktopCapture` omits its optional size and quality hints on the wire, so
/// the schema stays inspectable (ADR-0010).
#[test]
fn desktop_capture_wire_format_is_stable() {
    let bytes = request(Operation::DesktopCapture {
        display: 0,
        format: ImageFormat::Png,
        max_width: None,
        max_height: None,
        quality: None,
    })
    .encode()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["op"],
        json!({ "op": "desktop_capture", "display": 0, "format": "png" })
    );
}

#[test]
fn desktop_capture_out_of_bounds_are_rejected() {
    // Dimension hint above the ceiling / below the floor.
    assert_limit_exceeded(&request(Operation::DesktopCapture {
        display: 0,
        format: ImageFormat::Png,
        max_width: Some(limits::MAX_DESKTOP_DIM + 1),
        max_height: None,
        quality: None,
    }));
    assert_limit_exceeded(&request(Operation::DesktopCapture {
        display: 0,
        format: ImageFormat::Png,
        max_width: None,
        max_height: Some(limits::MIN_DESKTOP_DIM - 1),
        quality: None,
    }));
    // Quality out of the 1..=100 range.
    assert_limit_exceeded(&request(Operation::DesktopCapture {
        display: 0,
        format: ImageFormat::Jpeg,
        max_width: None,
        max_height: None,
        quality: Some(limits::MAX_DESKTOP_QUALITY + 1),
    }));
}

#[test]
fn desktop_pointer_and_scroll_out_of_bounds_are_rejected() {
    assert_limit_exceeded(&request(Operation::DesktopPointer {
        action: PointerAction::Move,
        button: PointerButton::Left,
        x: limits::MAX_DESKTOP_COORD + 1,
        y: 0,
    }));
    // i32::MIN has no positive counterpart; it must still be rejected.
    assert_limit_exceeded(&request(Operation::DesktopPointer {
        action: PointerAction::Move,
        button: PointerButton::Left,
        x: 0,
        y: i32::MIN,
    }));
    assert_limit_exceeded(&request(Operation::DesktopScroll {
        x: 0,
        y: 0,
        dx: limits::MAX_DESKTOP_SCROLL + 1,
        dy: 0,
    }));
}

#[test]
fn desktop_input_oversized_fields_are_rejected() {
    assert_limit_exceeded(&request(Operation::DesktopKey {
        action: KeyAction::Press,
        key: "k".repeat(limits::MAX_DESKTOP_KEY_BYTES + 1),
    }));
    assert_limit_exceeded(&request(Operation::DesktopType {
        text: "t".repeat(limits::MAX_DESKTOP_TEXT_BYTES + 1),
    }));
    assert_limit_exceeded(&request(Operation::DesktopHotkey {
        keys: vec!["ctrl".to_owned(); limits::MAX_DESKTOP_HOTKEY_KEYS + 1],
    }));
    assert_limit_exceeded(&request(Operation::DesktopWait {
        millis: limits::MAX_DESKTOP_WAIT_MS + 1,
    }));
    assert_limit_exceeded(&request(Operation::ClipboardSet {
        text: "c".repeat(limits::MAX_CLIPBOARD_BYTES + 1),
    }));
}

#[test]
fn desktop_frame_out_of_bounds_are_rejected() {
    // An inline image beyond the inline cap.
    assert_limit_exceeded(&response(OpResult::DesktopFrame {
        meta: desktop_meta(DesktopState::Active),
        image: Some(B64Bytes::from(vec![
            0u8;
            limits::MAX_DESKTOP_INLINE_BYTES + 1
        ])),
        staged: None,
    }));
    // An oversized desktop-name identity string.
    let mut meta = desktop_meta(DesktopState::Active);
    meta.desktop = "d".repeat(limits::MAX_DESKTOP_NAME_BYTES + 1);
    assert_limit_exceeded(&response(OpResult::DesktopFrame {
        meta,
        image: None,
        staged: None,
    }));
    // Oversized clipboard contents.
    assert_limit_exceeded(&response(OpResult::ClipboardContents {
        text: Some("c".repeat(limits::MAX_CLIPBOARD_BYTES + 1)),
    }));
}

#[test]
fn desktop_frame_adversarial_staged_path_is_rejected() {
    assert_invalid_path(&response(OpResult::DesktopFrame {
        meta: desktop_meta(DesktopState::Active),
        image: None,
        staged: Some(abs("C:\\Users\\..\\Windows")),
    }));
}

#[test]
fn unknown_desktop_capture_field_rejected() {
    let payload = json!({
        "type": "request",
        "id": 1,
        "session": SessionId::new().to_string(),
        "op": { "op": "desktop_capture", "display": 0, "format": "png", "extra": true },
    });
    let err = Message::decode(payload.to_string().as_bytes()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::MalformedMessage);
}

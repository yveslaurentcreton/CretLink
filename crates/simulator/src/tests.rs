//! Simulator scenario tests through `Framed` over `tokio::io::duplex`
//! (ST-M001-003). No administrator permission is required (REQ-N-011).

use std::collections::BTreeMap;

use cretlink_protocol::{
    B64Bytes, CleanupPolicy, CleanupTrigger, DigestAlgo, ErrorCode, ErrorStage, EventKind, JobId,
    JobOutcome, Message, OpResult, Operation, Origin, OutputStream, PROTOCOL_V1, ProtocolError,
    ProtocolVersion, RemotePath, RequestId, Role, SessionId, Shell,
};
use cretlink_transport::{Framed, TransportError};
use sha2::{Digest, Sha256};
use tokio::io::DuplexStream;

use super::Simulator;

fn hello(protocol: ProtocolVersion) -> Message {
    Message::Hello {
        protocol,
        role: Role::Console,
        capabilities: vec!["exec".to_string()],
    }
}

fn exec(id: u64, session: SessionId, command: &str, timeout_ms: Option<u64>) -> Message {
    Message::Request {
        id: RequestId(id),
        session,
        origin: Origin::Console,
        purpose: None,
        op: Operation::Exec {
            shell: Shell::PowerShell,
            command: command.to_string(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms,
        },
    }
}

async fn start(simulator: Simulator) -> Framed<DuplexStream> {
    let (client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = simulator.serve(server).await;
    });
    Framed::new(client)
}

async fn connect() -> (Framed<DuplexStream>, SessionId) {
    let mut framed = start(Simulator::new()).await;
    framed.send(&hello(PROTOCOL_V1)).await.unwrap();
    match framed.recv().await.unwrap() {
        Message::HelloAck {
            protocol,
            session,
            target,
        } => {
            assert!(PROTOCOL_V1.is_compatible_with(protocol));
            assert_eq!(target, Simulator::target_identity());
            (framed, session)
        }
        other => panic!("expected hello_ack, got {other:?}"),
    }
}

async fn expect_exec_started(framed: &mut Framed<DuplexStream>, id: u64) -> JobId {
    match framed.recv().await.unwrap() {
        Message::Response {
            id: got,
            result: OpResult::ExecStarted { job, working_dir },
            ..
        } => {
            assert_eq!(got, RequestId(id));
            assert!(
                !working_dir.is_empty(),
                "the simulator reports a plausible effective working directory"
            );
            job
        }
        other => panic!("expected exec_started, got {other:?}"),
    }
}

#[tokio::test]
async fn negotiation_reports_the_simulated_identity() {
    let (_framed, _session) = connect().await;
    let target = Simulator::target_identity();
    assert_eq!(target.hostname, "SIMULATED-TARGET");
    assert_eq!(target.os_version, "Simulated Windows");
    assert_eq!(target.arch, "x86_64");
    assert!(target.simulated);
    assert!(!target.elevated);
    assert!(!target.agent_version.is_empty());
}

#[tokio::test]
async fn incompatible_major_version_is_rejected_at_negotiation() {
    let mut framed = start(Simulator::new()).await;
    let offered = ProtocolVersion::new(PROTOCOL_V1.major.wrapping_add(1), 0);
    framed.send(&hello(offered)).await.unwrap();
    match framed.recv().await.unwrap() {
        Message::Error { code, stage, .. } => {
            assert_eq!(code, ErrorCode::IncompatibleVersion);
            assert_eq!(stage, ErrorStage::Negotiation);
        }
        other => panic!("expected error, got {other:?}"),
    }
    assert!(matches!(
        framed.recv().await.unwrap_err(),
        TransportError::Eof
    ));
}

#[tokio::test]
async fn same_major_different_minor_negotiates_with_own_minor() {
    let mut framed = start(Simulator::new()).await;
    let offered = ProtocolVersion::new(PROTOCOL_V1.major, PROTOCOL_V1.minor.wrapping_add(7));
    framed.send(&hello(offered)).await.unwrap();
    match framed.recv().await.unwrap() {
        Message::HelloAck { protocol, .. } => assert_eq!(protocol, PROTOCOL_V1),
        other => panic!("expected hello_ack, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_version_misbehaviour_answers_with_incompatible_major() {
    let mut framed = start(Simulator::new().with_wrong_version()).await;
    framed.send(&hello(PROTOCOL_V1)).await.unwrap();
    match framed.recv().await.unwrap() {
        Message::HelloAck { protocol, .. } => {
            assert!(!PROTOCOL_V1.is_compatible_with(protocol));
        }
        other => panic!("expected hello_ack, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_frame_misbehaviour_is_rejected_by_the_transport() {
    let mut framed = start(Simulator::new().with_oversized_frame()).await;
    framed.send(&hello(PROTOCOL_V1)).await.unwrap();
    assert!(matches!(
        framed.recv().await.unwrap_err(),
        TransportError::Oversized { .. }
    ));
}

#[tokio::test]
async fn system_info_reports_the_simulated_identity() {
    let (mut framed, session) = connect().await;
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(1),
            session,
            op: Operation::SystemInfo {},
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Response {
            id,
            session: got_session,
            result:
                OpResult::SystemInfo {
                    target,
                    reboot_pending,
                },
        } => {
            assert_eq!(id, RequestId(1));
            assert_eq!(got_session, session);
            assert_eq!(target, Simulator::target_identity());
            assert!(!reboot_pending);
        }
        other => panic!("expected system_info, got {other:?}"),
    }
}

#[tokio::test]
async fn sim_ok_streams_stdout_and_completes() {
    let (mut framed, session) = connect().await;
    framed
        .send(&exec(1, session, "sim:ok hello world", None))
        .await
        .unwrap();
    let job = expect_exec_started(&mut framed, 1).await;

    match framed.recv().await.unwrap() {
        Message::Event {
            session: got,
            event:
                EventKind::JobOutput {
                    job: out_job,
                    stream,
                    data,
                },
        } => {
            assert_eq!(got, session);
            assert_eq!(out_job, job);
            assert_eq!(stream, OutputStream::Stdout);
            assert_eq!(data.as_bytes(), b"hello world\n");
        }
        other => panic!("expected job_output, got {other:?}"),
    }
    match framed.recv().await.unwrap() {
        Message::Event {
            event:
                EventKind::JobExited {
                    job: exited,
                    exit_code,
                    outcome,
                    ..
                },
            ..
        } => {
            assert_eq!(exited, job);
            assert_eq!(exit_code, Some(0));
            assert_eq!(outcome, JobOutcome::Completed);
        }
        other => panic!("expected job_exited, got {other:?}"),
    }
}

#[tokio::test]
async fn sim_fail_streams_stderr_and_fails_with_the_code() {
    let (mut framed, session) = connect().await;
    framed
        .send(&exec(1, session, "sim:fail 7", None))
        .await
        .unwrap();
    let job = expect_exec_started(&mut framed, 1).await;

    match framed.recv().await.unwrap() {
        Message::Event {
            event: EventKind::JobOutput { stream, data, .. },
            ..
        } => {
            assert_eq!(stream, OutputStream::Stderr);
            assert!(!data.is_empty());
        }
        other => panic!("expected job_output, got {other:?}"),
    }
    match framed.recv().await.unwrap() {
        Message::Event {
            event:
                EventKind::JobExited {
                    job: exited,
                    exit_code,
                    outcome,
                    ..
                },
            ..
        } => {
            assert_eq!(exited, job);
            assert_eq!(exit_code, Some(7));
            assert_eq!(outcome, JobOutcome::Failed);
        }
        other => panic!("expected job_exited, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn sim_delay_completes_after_the_scripted_time() {
    let (mut framed, session) = connect().await;
    framed
        .send(&exec(1, session, "sim:delay 5000", None))
        .await
        .unwrap();
    let job = expect_exec_started(&mut framed, 1).await;

    match framed.recv().await.unwrap() {
        Message::Event {
            event:
                EventKind::JobExited {
                    job: exited,
                    exit_code,
                    duration_ms,
                    outcome,
                },
            ..
        } => {
            assert_eq!(exited, job);
            assert_eq!(exit_code, Some(0));
            assert_eq!(duration_ms, 5000);
            assert_eq!(outcome, JobOutcome::Completed);
        }
        other => panic!("expected job_exited, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn sim_timeout_honours_the_request_deadline() {
    let (mut framed, session) = connect().await;
    framed
        .send(&exec(1, session, "sim:timeout", Some(1500)))
        .await
        .unwrap();
    let job = expect_exec_started(&mut framed, 1).await;

    match framed.recv().await.unwrap() {
        Message::Event {
            event:
                EventKind::JobExited {
                    job: exited,
                    exit_code,
                    duration_ms,
                    outcome,
                },
            ..
        } => {
            assert_eq!(exited, job);
            assert_eq!(exit_code, None);
            assert_eq!(duration_ms, 1500);
            assert_eq!(outcome, JobOutcome::TimedOut);
        }
        other => panic!("expected job_exited, got {other:?}"),
    }
}

#[tokio::test]
async fn sim_hang_supports_stdin_echo_listing_and_cancel() {
    let (mut framed, session) = connect().await;
    framed
        .send(&exec(1, session, "sim:hang", None))
        .await
        .unwrap();
    let job = expect_exec_started(&mut framed, 1).await;

    // Stdin bytes are acknowledged and echoed back as stdout.
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(2),
            session,
            op: Operation::WriteStdin {
                job,
                data: B64Bytes::new(b"ping".to_vec()),
            },
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Response {
            id,
            result: OpResult::Ack {},
            ..
        } => assert_eq!(id, RequestId(2)),
        other => panic!("expected ack, got {other:?}"),
    }
    match framed.recv().await.unwrap() {
        Message::Event {
            event:
                EventKind::JobOutput {
                    job: out_job,
                    stream,
                    data,
                },
            ..
        } => {
            assert_eq!(out_job, job);
            assert_eq!(stream, OutputStream::Stdout);
            assert_eq!(data.as_bytes(), b"ping");
        }
        other => panic!("expected echoed stdin, got {other:?}"),
    }

    // The job listing reflects the running job.
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(3),
            session,
            op: Operation::ListJobs {},
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Response {
            result: OpResult::Jobs { jobs },
            ..
        } => {
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].job, job);
            assert_eq!(jobs[0].command_preview, "sim:hang");
            assert!(jobs[0].running);
        }
        other => panic!("expected jobs, got {other:?}"),
    }

    // Cancel ends the job with an honest Cancelled outcome.
    framed
        .send(&Message::Cancel {
            id: RequestId(4),
            session,
            origin: cretlink_protocol::Origin::Console,
            purpose: None,
            job,
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Response {
            id,
            result: OpResult::Ack {},
            ..
        } => assert_eq!(id, RequestId(4)),
        other => panic!("expected ack, got {other:?}"),
    }
    match framed.recv().await.unwrap() {
        Message::Event {
            event:
                EventKind::JobExited {
                    job: exited,
                    exit_code,
                    outcome,
                    ..
                },
            ..
        } => {
            assert_eq!(exited, job);
            assert_eq!(exit_code, None);
            assert_eq!(outcome, JobOutcome::Cancelled);
        }
        other => panic!("expected job_exited, got {other:?}"),
    }

    // The listing now shows the job as no longer running.
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(5),
            session,
            op: Operation::ListJobs {},
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Response {
            result: OpResult::Jobs { jobs },
            ..
        } => {
            assert_eq!(jobs.len(), 1);
            assert!(!jobs[0].running);
        }
        other => panic!("expected jobs, got {other:?}"),
    }
}

#[tokio::test]
async fn finished_job_history_is_bounded_and_running_jobs_stay_listed() {
    let (mut framed, session) = connect().await;

    // A long-running job started before the history churns.
    framed
        .send(&exec(1, session, "sim:hang", None))
        .await
        .unwrap();
    let hang_job = expect_exec_started(&mut framed, 1).await;

    // Finish more jobs than the history keeps.
    let total = super::FINISHED_JOB_HISTORY + 8;
    for index in 0..total {
        let id = 100 + u64::try_from(index).unwrap();
        framed
            .send(&exec(id, session, "sim:ok x", None))
            .await
            .unwrap();
        let _job = expect_exec_started(&mut framed, id).await;
        // Consume the JobOutput and JobExited events.
        let _ = framed.recv().await.unwrap();
        let _ = framed.recv().await.unwrap();
    }

    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(9000),
            session,
            op: Operation::ListJobs {},
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Response {
            result: OpResult::Jobs { jobs },
            ..
        } => {
            // The running job stays listed despite the long history.
            assert!(
                jobs.iter()
                    .any(|summary| summary.job == hang_job && summary.running),
                "the running job must never be pushed out of the listing"
            );
            let finished = jobs.iter().filter(|summary| !summary.running).count();
            assert!(
                finished <= super::FINISHED_JOB_HISTORY,
                "finished history must stay bounded, got {finished}"
            );
        }
        other => panic!("expected jobs, got {other:?}"),
    }
}

#[tokio::test]
async fn running_jobs_beyond_the_cap_are_refused() {
    let (mut framed, session) = connect().await;
    for index in 0..super::MAX_RUNNING_JOBS {
        let id = u64::try_from(index).unwrap();
        framed
            .send(&exec(id, session, "sim:hang", None))
            .await
            .unwrap();
        let _ = expect_exec_started(&mut framed, id).await;
    }

    let over = u64::try_from(super::MAX_RUNNING_JOBS).unwrap();
    framed
        .send(&exec(over, session, "sim:hang", None))
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error { id, code, .. } => {
            assert_eq!(id, Some(RequestId(over)));
            assert_eq!(code, ErrorCode::LimitExceeded);
        }
        other => panic!("expected a limit error, got {other:?}"),
    }
}

#[tokio::test]
async fn sim_disconnect_drops_the_connection_mid_job() {
    let (mut framed, session) = connect().await;
    framed
        .send(&exec(1, session, "sim:disconnect", None))
        .await
        .unwrap();
    let _job = expect_exec_started(&mut framed, 1).await;
    // No exit event ever arrives: the stream just ends, so the peer must
    // report the outcome as unknown (REQ-N-008), never as success.
    assert!(matches!(
        framed.recv().await.unwrap_err(),
        TransportError::Eof
    ));
}

#[tokio::test]
async fn sim_malformed_writes_a_valid_frame_with_invalid_json() {
    let (mut framed, session) = connect().await;
    framed
        .send(&exec(1, session, "sim:malformed", None))
        .await
        .unwrap();
    assert!(matches!(
        framed.recv().await.unwrap_err(),
        TransportError::Decode(ProtocolError::Malformed(_))
    ));
    assert!(matches!(
        framed.recv().await.unwrap_err(),
        TransportError::Eof
    ));
}

#[tokio::test]
async fn unscripted_commands_are_unsupported() {
    let (mut framed, session) = connect().await;
    framed
        .send(&exec(1, session, "whoami", None))
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error {
            id, code, stage, ..
        } => {
            assert_eq!(id, Some(RequestId(1)));
            assert_eq!(code, ErrorCode::Unsupported);
            assert_eq!(stage, ErrorStage::Execution);
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn requests_for_a_foreign_session_are_rejected() {
    let (mut framed, _session) = connect().await;
    let foreign = SessionId::new();
    framed
        .send(&exec(1, foreign, "sim:ok hi", None))
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error {
            id, code, stage, ..
        } => {
            assert_eq!(id, Some(RequestId(1)));
            assert_eq!(code, ErrorCode::UnknownSession);
            assert_eq!(stage, ErrorStage::Validation);
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn stdin_and_cancel_for_unknown_jobs_are_rejected() {
    let (mut framed, session) = connect().await;
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(1),
            session,
            op: Operation::WriteStdin {
                job: JobId(42),
                data: B64Bytes::new(b"x".to_vec()),
            },
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error { id, code, .. } => {
            assert_eq!(id, Some(RequestId(1)));
            assert_eq!(code, ErrorCode::UnknownJob);
        }
        other => panic!("expected error, got {other:?}"),
    }

    framed
        .send(&Message::Cancel {
            id: RequestId(2),
            session,
            origin: cretlink_protocol::Origin::Console,
            purpose: None,
            job: JobId(42),
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error { id, code, .. } => {
            assert_eq!(id, Some(RequestId(2)));
            assert_eq!(code, ErrorCode::UnknownJob);
        }
        other => panic!("expected error, got {other:?}"),
    }
}

#[tokio::test]
async fn list_jobs_is_empty_before_any_exec() {
    let (mut framed, session) = connect().await;
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(1),
            session,
            op: Operation::ListJobs {},
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Response {
            result: OpResult::Jobs { jobs },
            ..
        } => assert!(jobs.is_empty()),
        other => panic!("expected jobs, got {other:?}"),
    }
}

// --- simulated file surface (ST-M003-005, ADR-0012) -----------------------

fn staging(name: &str) -> RemotePath {
    RemotePath::Staging {
        name: name.to_string(),
    }
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

/// Sends `op` on `framed` and returns the correlated `OpResult`, panicking on an
/// error frame or an id mismatch.
async fn op_ok(
    framed: &mut Framed<DuplexStream>,
    id: u64,
    session: SessionId,
    op: Operation,
) -> OpResult {
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(id),
            session,
            op,
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Response {
            id: got, result, ..
        } => {
            assert_eq!(got, RequestId(id), "response correlates to the request");
            result
        }
        other => panic!("expected a response, got {other:?}"),
    }
}

/// Drives a full staging transfer of `bytes` (one `FsWrite` then `FsCommit`) and
/// returns the commit result.
async fn stage(
    framed: &mut Framed<DuplexStream>,
    session: SessionId,
    name: &str,
    bytes: &[u8],
    cleanup: CleanupPolicy,
) -> OpResult {
    let accepted = op_ok(
        framed,
        1,
        session,
        Operation::FsWrite {
            dest: staging(name),
            offset: 0,
            data: B64Bytes::new(bytes.to_vec()),
        },
    )
    .await;
    assert!(
        matches!(accepted, OpResult::WriteAccepted { part_len } if part_len == bytes.len() as u64),
        "the part grows to the written length, got {accepted:?}"
    );
    op_ok(
        framed,
        2,
        session,
        Operation::FsCommit {
            dest: staging(name),
            len: bytes.len() as u64,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(sha256(bytes)),
            overwrite: false,
            source: Some("C:\\tech\\tool.zip".to_string()),
            cleanup,
        },
    )
    .await
}

#[tokio::test]
async fn simulator_stages_commits_and_reads_back_an_artifact() {
    let (mut framed, session) = connect().await;
    let payload = b"faithful-vfs-bytes";

    let committed = stage(
        &mut framed,
        session,
        "tool.zip",
        payload,
        CleanupPolicy::Keep,
    )
    .await;
    match committed {
        OpResult::Committed {
            len,
            digest,
            replaced,
            ..
        } => {
            assert_eq!(len, payload.len() as u64);
            assert_eq!(digest.as_bytes(), sha256(payload).as_slice());
            assert!(!replaced, "a fresh commit replaces nothing");
        }
        other => panic!("expected committed, got {other:?}"),
    }

    // The committed bytes read back verbatim.
    let read = op_ok(
        &mut framed,
        3,
        session,
        Operation::FsRead {
            path: staging("tool.zip"),
            offset: 0,
            len: 4096,
        },
    )
    .await;
    match read {
        OpResult::FileData {
            data,
            eof,
            file_len,
        } => {
            assert_eq!(data.as_bytes(), payload);
            assert!(eof, "the whole file fit in one read");
            assert_eq!(file_len, payload.len() as u64);
        }
        other => panic!("expected file_data, got {other:?}"),
    }

    // ListStaged shows the artifact with its required technician source label.
    let listed = op_ok(&mut framed, 4, session, Operation::ListStaged {}).await;
    match listed {
        OpResult::StagedList { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].source, "C:\\tech\\tool.zip");
            assert_eq!(entries[0].cleanup, CleanupPolicy::Keep);
        }
        other => panic!("expected staged_list, got {other:?}"),
    }
}

#[tokio::test]
async fn simulator_rejects_a_commit_whose_digest_is_wrong() {
    let (mut framed, session) = connect().await;
    let payload = b"honest-bytes";
    op_ok(
        &mut framed,
        1,
        session,
        Operation::FsWrite {
            dest: staging("bad.bin"),
            offset: 0,
            data: B64Bytes::new(payload.to_vec()),
        },
    )
    .await;

    // A commit with a mismatched digest must fail closed — an unverified
    // artifact never reaches its final name (REQ-N-007).
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(2),
            session,
            op: Operation::FsCommit {
                dest: staging("bad.bin"),
                len: payload.len() as u64,
                algo: DigestAlgo::Sha256,
                digest: B64Bytes::new(sha256(b"different-bytes")),
                overwrite: false,
                source: Some("C:\\tech\\bad.bin".to_string()),
                cleanup: CleanupPolicy::Keep,
            },
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error {
            id, code, stage, ..
        } => {
            assert_eq!(id, Some(RequestId(2)));
            assert_eq!(code, ErrorCode::IntegrityMismatch);
            assert_eq!(stage, ErrorStage::Execution);
        }
        other => panic!("expected an integrity error, got {other:?}"),
    }
}

#[tokio::test]
async fn simulator_cleanup_removes_a_session_end_artifact() {
    let (mut framed, session) = connect().await;
    stage(
        &mut framed,
        session,
        "ephemeral.bin",
        b"gone-at-session-end",
        CleanupPolicy::OnSessionEnd,
    )
    .await;

    let report = op_ok(
        &mut framed,
        3,
        session,
        Operation::StagingCleanup {
            trigger: CleanupTrigger::SessionEnd,
        },
    )
    .await;
    match report {
        OpResult::CleanupReport { removed, failed } => {
            assert_eq!(removed, 1, "the session-end artifact was removed");
            assert!(failed.is_empty(), "a clean cleanup reports no failures");
        }
        other => panic!("expected cleanup_report, got {other:?}"),
    }

    // It is gone: a follow-up stat reports no entry.
    let stat = op_ok(
        &mut framed,
        4,
        session,
        Operation::FsStat {
            path: staging("ephemeral.bin"),
        },
    )
    .await;
    match stat {
        OpResult::PathStat { entry, .. } => assert!(entry.is_none(), "the artifact is gone"),
        other => panic!("expected path_stat, got {other:?}"),
    }
}

#[tokio::test]
async fn simulator_refuses_to_overwrite_a_committed_artifact() {
    let (mut framed, session) = connect().await;
    stage(
        &mut framed,
        session,
        "once.bin",
        b"first",
        CleanupPolicy::Keep,
    )
    .await;

    // A second commit to the same name without overwrite must be refused, so the
    // engine never silently clobbers an existing artifact (REQ-N-007).
    op_ok(
        &mut framed,
        3,
        session,
        Operation::FsWrite {
            dest: staging("once.bin"),
            offset: 0,
            data: B64Bytes::new(b"second".to_vec()),
        },
    )
    .await;
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(4),
            session,
            op: Operation::FsCommit {
                dest: staging("once.bin"),
                len: b"second".len() as u64,
                algo: DigestAlgo::Sha256,
                digest: B64Bytes::new(sha256(b"second")),
                overwrite: false,
                source: Some("C:\\tech\\once.bin".to_string()),
                cleanup: CleanupPolicy::Keep,
            },
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error {
            id, code, stage, ..
        } => {
            assert_eq!(id, Some(RequestId(4)));
            assert_eq!(code, ErrorCode::AlreadyExists);
            assert_eq!(stage, ErrorStage::Execution);
        }
        other => panic!("expected an already-exists error, got {other:?}"),
    }
}

// --- simulated drives and folder removal (ST-M010-001, ADR-0012) -----------

/// Sends `op` on `framed` and returns the code of the error it is answered
/// with, panicking on a response.
async fn op_err(
    framed: &mut Framed<DuplexStream>,
    id: u64,
    session: SessionId,
    op: Operation,
) -> ErrorCode {
    framed
        .send(&Message::Request {
            origin: Origin::Console,
            purpose: None,
            id: RequestId(id),
            session,
            op,
        })
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error {
            id: got,
            code,
            stage,
            ..
        } => {
            assert_eq!(got, Some(RequestId(id)), "the error correlates");
            assert_eq!(stage, ErrorStage::Execution);
            code
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

/// Creates the folder `name` in the simulated staging area over the wire and
/// returns where it really lies, so a test can put content in it.
async fn staged_folder(
    framed: &mut Framed<DuplexStream>,
    session: SessionId,
    name: &str,
) -> std::path::PathBuf {
    let created = op_ok(
        framed,
        1,
        session,
        Operation::CreateDir {
            path: staging(name),
        },
    )
    .await;
    assert_eq!(created, OpResult::Ack {});
    super::vfs_root(session)
        .join(session.to_string())
        .join(name)
}

#[tokio::test]
async fn simulator_lists_the_drives_of_its_host() {
    let (mut framed, session) = connect().await;
    match op_ok(&mut framed, 1, session, Operation::FsDrives {}).await {
        OpResult::Drives { drives } => {
            assert!(!drives.is_empty(), "a computer has at least one drive");
            // Off Windows the one drive is the root file system.
            #[cfg(unix)]
            assert_eq!(
                drives.iter().map(|d| d.root.as_str()).collect::<Vec<_>>(),
                ["/"]
            );
        }
        other => panic!("expected drives, got {other:?}"),
    }
}

/// The simulated target removes a folder with the real engine: everything in
/// it goes, the folder goes, and the answer counts both.
#[tokio::test]
async fn simulator_removes_a_folder_with_its_content() {
    use cretlink_protocol::TreeState;

    let (mut framed, session) = connect().await;
    let folder = staged_folder(&mut framed, session, "unpacked").await;
    std::fs::create_dir(folder.join("drivers")).unwrap();
    std::fs::write(folder.join("setup.exe"), b"setup").unwrap();
    std::fs::write(folder.join("drivers").join("net.inf"), b"[Version]").unwrap();

    let removed = op_ok(
        &mut framed,
        2,
        session,
        Operation::RemoveTree {
            path: staging("unpacked"),
            stop: false,
            folder_id: None,
        },
    )
    .await;
    assert_eq!(
        removed,
        OpResult::TreeRemoval {
            state: TreeState::Removed,
            removed_files: 2,
            removed_dirs: 2,
            failed: Vec::new(),
            failed_more: 0,
            folder_id: None,
        }
    );
    assert!(!folder.exists(), "the folder is gone");

    // What is not there cannot be removed, and the answer says so.
    let code = op_err(
        &mut framed,
        3,
        session,
        Operation::RemoveTree {
            path: staging("unpacked"),
            stop: false,
            folder_id: None,
        },
    )
    .await;
    assert_eq!(code, ErrorCode::PathNotFound);
}

/// A request that only ends a removal is answered as stopped and removes
/// nothing; a file is not removed as a folder.
#[tokio::test]
async fn simulator_stops_a_removal_and_refuses_a_file() {
    use cretlink_protocol::TreeState;

    let (mut framed, session) = connect().await;
    let folder = staged_folder(&mut framed, session, "kept").await;
    std::fs::write(folder.join("a.txt"), b"a").unwrap();

    let stopped = op_ok(
        &mut framed,
        2,
        session,
        Operation::RemoveTree {
            path: staging("kept"),
            stop: true,
            folder_id: None,
        },
    )
    .await;
    assert!(matches!(
        stopped,
        OpResult::TreeRemoval {
            state: TreeState::Stopped,
            removed_files: 0,
            removed_dirs: 0,
            ..
        }
    ));
    assert!(folder.join("a.txt").exists(), "nothing was removed");

    stage(
        &mut framed,
        session,
        "tool.zip",
        b"not-a-folder",
        CleanupPolicy::OnSessionEnd,
    )
    .await;
    let code = op_err(
        &mut framed,
        3,
        session,
        Operation::RemoveTree {
            path: staging("tool.zip"),
            stop: false,
            folder_id: None,
        },
    )
    .await;
    assert_eq!(code, ErrorCode::WrongPathKind);
}

// --- Interactive terminal grammar (ADR-0013, ST-M003-009) ---

/// A `Request` carrying a terminal `op`, stamped with the technician-terminal
/// origin the console uses for these operations.
fn term_request(id: u64, session: SessionId, op: Operation) -> Message {
    Message::Request {
        id: RequestId(id),
        session,
        origin: Origin::Terminal,
        purpose: None,
        op,
    }
}

/// Awaits a `Response` carrying an `Ack {}` for the given request id.
async fn expect_ack(framed: &mut Framed<DuplexStream>, id: u64) {
    match framed.recv().await.unwrap() {
        Message::Response {
            id: got,
            result: OpResult::Ack {},
            ..
        } => assert_eq!(got, RequestId(id)),
        other => panic!("expected an ack for {id}, got {other:?}"),
    }
}

/// The deterministic, plain-text terminal grammar drives a full simulated
/// attach/stream/detach with no real ConPTY, so it runs on every CI platform
/// (ST-M003-009): open emits a prompt, written input echoes back as merged
/// output, an in-range resize acknowledges, and close ends the job with an
/// honest `Cancelled` exit.
#[tokio::test]
async fn terminal_attach_stream_detach_round_trip() {
    let (mut framed, session) = connect().await;

    // Open: TermOpened{job} then a fixed prompt as the first merged-output chunk.
    framed
        .send(&term_request(
            1,
            session,
            Operation::TermOpen {
                shell: Shell::Cmd,
                cols: 80,
                rows: 24,
                working_dir: None,
                env: BTreeMap::new(),
            },
        ))
        .await
        .unwrap();
    let job = match framed.recv().await.unwrap() {
        Message::Response {
            id,
            result: OpResult::TermOpened { job },
            ..
        } => {
            assert_eq!(id, RequestId(1));
            job
        }
        other => panic!("expected term_opened, got {other:?}"),
    };
    match framed.recv().await.unwrap() {
        Message::Event {
            event:
                EventKind::JobOutput {
                    job: j,
                    stream,
                    data,
                },
            ..
        } => {
            assert_eq!(j, job);
            assert_eq!(stream, OutputStream::Stdout);
            assert!(
                !data.into_inner().is_empty(),
                "a terminal emits an initial prompt as merged output"
            );
        }
        other => panic!("expected a prompt job_output, got {other:?}"),
    }

    // Stream: written stdin is echoed back as merged output (the stream path).
    framed
        .send(&term_request(
            2,
            session,
            Operation::WriteStdin {
                job,
                data: B64Bytes::new(b"whoami\n".to_vec()),
            },
        ))
        .await
        .unwrap();
    expect_ack(&mut framed, 2).await;
    match framed.recv().await.unwrap() {
        Message::Event {
            event: EventKind::JobOutput { job: j, data, .. },
            ..
        } => {
            assert_eq!(j, job);
            assert_eq!(data.into_inner(), b"whoami\n", "the terminal echoes input");
        }
        other => panic!("expected an echoed job_output, got {other:?}"),
    }

    // Resize within bounds acknowledges (last-writer-wins).
    framed
        .send(&term_request(
            3,
            session,
            Operation::TermResize {
                job,
                cols: 120,
                rows: 40,
            },
        ))
        .await
        .unwrap();
    expect_ack(&mut framed, 3).await;

    // Detach/close: Ack, then an honest Cancelled exit (a terminal is a job).
    framed
        .send(&term_request(4, session, Operation::TermClose { job }))
        .await
        .unwrap();
    expect_ack(&mut framed, 4).await;
    match framed.recv().await.unwrap() {
        Message::Event {
            event: EventKind::JobExited {
                job: j, outcome, ..
            },
            ..
        } => {
            assert_eq!(j, job);
            assert_eq!(outcome, JobOutcome::Cancelled);
        }
        other => panic!("expected a job_exited cancelled, got {other:?}"),
    }
}

/// Closing or resizing a terminal id that does not exist is refused honestly,
/// never silently accepted (ST-M003-009, REQ-N-008).
#[tokio::test]
async fn terminal_ops_on_unknown_job_are_refused() {
    let (mut framed, session) = connect().await;

    framed
        .send(&term_request(
            1,
            session,
            Operation::TermResize {
                job: JobId(999),
                cols: 80,
                rows: 24,
            },
        ))
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error { id, code, .. } => {
            assert_eq!(id, Some(RequestId(1)));
            assert_eq!(code, ErrorCode::UnknownJob);
        }
        other => panic!("expected an unknown-job error, got {other:?}"),
    }

    framed
        .send(&term_request(
            2,
            session,
            Operation::TermClose { job: JobId(999) },
        ))
        .await
        .unwrap();
    match framed.recv().await.unwrap() {
        Message::Error { id, code, .. } => {
            assert_eq!(id, Some(RequestId(2)));
            assert_eq!(code, ErrorCode::UnknownJob);
        }
        other => panic!("expected an unknown-job error, got {other:?}"),
    }
}

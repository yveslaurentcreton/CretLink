//! Unified console-side audit end-to-end tests (ST-M003-006, ADR-0008).
//!
//! One real loopback session feeds the one bounded audit channel from all
//! three producers — the session runtime's lifecycle hooks
//! (`host_with_audit`), routed MCP bridge calls over real local IPC, and
//! manual console commands — and the produced per-session `.jsonl` is then
//! verified as a whole: `verify_sealed` passes, exec correlation holds (one
//! job id across start and finish), the transfer digest equals the report
//! digest, attribution distinguishes `mcp` from `console` records, sensitive
//! command text and environment override values are redacted, and no session
//! secret appears in the produced bytes (REQ-F-017, REQ-S-017, REQ-S-018).
//!
//! Retention, soft-cap, and high-volume behaviour are driven directly against
//! the audit service with injected small caps (REQ-N-006).
//!
//! `unwrap`/`expect`/`panic` are allowed here: test-only code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cretlink_console::{
    AuditEntry, AuditServiceConfig, ChannelAudit, ConsoleRoute, ConsoleSessionService, IpcAudit,
    IpcServer, ManualActivity, console_cancel, console_exec_started, open_session_log,
    prune_audit_dir, spawn_audit_service,
};
use cretlink_mcp::{IpcEndpoint, IpcListener, IpcTargetSession, McpBridge};
use cretlink_protocol::{JobId, PROTOCOL_V1, SessionId, TargetIdentity};
use cretlink_session::{
    ConsoleHints, ExecRequest, HostHandle, JoinHandle, SessionEvent, SessionHost, SessionJoiner,
    Shell,
};
use cretlink_shared::{
    Actor, AuditArgs, AuditRecord, AuditValue, ChainReport, MAX_FIELD_BYTES, Outcome,
    RetentionPolicy, verify_sealed,
};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, serve_client, serve_server};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(5);
const SEAL: Duration = Duration::from_secs(30);

/// A high-entropy environment value that must never appear in audit bytes.
const ENV_SECRET: &str = "ENV-SECRET-VALUE-cAnArY-0f9d2a71";
/// A command marked sensitive whose text must never appear in audit bytes.
/// Uppercase letters keep it impossible to collide with lowercase hex digests.
const SENSITIVE_COMMAND_MARKER: &str = "SENSITIVE-CMD-CANARY-XYZZY";

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn simulated_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "SIMULATED-TARGET".to_string(),
        os_version: "Simulated Windows".to_string(),
        arch: "x86_64".to_string(),
        elevated: false,
        simulated: true,
        agent_version: "audit-e2e-agent".to_string(),
    }
}

fn unique_endpoint() -> IpcEndpoint {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    IpcEndpoint::named(format!("cretlink-audit-e2e-{}-{n}", std::process::id()))
}

fn temp_dir_for(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cretlink-audit-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn config_for(dir: &std::path::Path) -> AuditServiceConfig {
    AuditServiceConfig {
        dir: dir.to_path_buf(),
        retention: RetentionPolicy::default(),
        soft_cap_bytes: 0,
    }
}

async fn next_event(handle: &mut HostHandle) -> SessionEvent {
    timeout(STEP, handle.next_event())
        .await
        .expect("timed out waiting for a session event")
        .expect("session event stream ended unexpectedly")
}

/// Pairs a host (wired to the audit sink) with a consenting simulated agent.
async fn active_session_with_audit(
    audit: &ChannelAudit,
) -> (HostHandle, JoinHandle, SessionId, TargetIdentity) {
    let hints = ConsoleHints {
        label: "This computer (technician)".to_string(),
    };
    let mut host = SessionHost::host_with_audit(
        loopback(),
        hints,
        PROTOCOL_V1,
        Vec::new(),
        Arc::new(audit.clone()),
    )
    .await
    .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let mut joiner = SessionJoiner::join(addr, simulated_identity(), PROTOCOL_V1, Vec::new())
        .await
        .expect("dial agent");

    assert_eq!(next_event(&mut host).await, SessionEvent::Connected);
    assert_eq!(
        next_event(&mut host).await,
        SessionEvent::AwaitingApprovalCompare
    );
    host.confirm_code().expect("confirm the approval code");
    assert_eq!(next_event(&mut host).await, SessionEvent::AwaitingConsent);
    joiner.grant_consent().expect("grant consent");

    let identity = match next_event(&mut host).await {
        SessionEvent::ConsentGranted { identity } => identity,
        other => panic!("expected ConsentGranted, got {other:?}"),
    };
    assert_eq!(next_event(&mut host).await, SessionEvent::Active);
    let session = host.session().expect("session id once active");
    let _ = timeout(STEP, joiner.next_event()).await;
    (host, joiner, session, identity)
}

async fn bridge_client(endpoint: IpcEndpoint) -> RunningService<RoleClient, ()> {
    let target = IpcTargetSession::connect(endpoint).await;
    let bridge = McpBridge::new(Arc::new(target));
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        if let Ok(running) = serve_server(bridge, server_io).await {
            let _ = running.waiting().await;
        }
    });
    serve_client((), client_io)
        .await
        .expect("rmcp client initialization")
}

fn structured(result: &rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("tool result carries structured content")
}

/// Reads the single produced session log: raw bytes plus parsed records.
fn read_session_log(dir: &std::path::Path) -> (Vec<u8>, Vec<AuditRecord>) {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read audit dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert_eq!(logs.len(), 1, "exactly one session log expected: {logs:?}");
    let path = logs.remove(0);
    let bytes = std::fs::read(&path).expect("read session log");
    let records = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<AuditRecord>(line).expect("well-formed record"))
        .collect();
    (bytes, records)
}

fn arg<'a>(record: &'a AuditRecord, key: &str) -> &'a AuditValue {
    record.args.0.get(key).unwrap_or_else(|| {
        panic!(
            "record {} lacks arg {key}: {:?}",
            record.action, record.args
        )
    })
}

fn int_arg(record: &AuditRecord, key: &str) -> i64 {
    match arg(record, key) {
        AuditValue::Int(value) => *value,
        other => panic!("arg {key} of {} is not an int: {other:?}", record.action),
    }
}

fn find<'a>(records: &'a [AuditRecord], action: &str) -> Vec<&'a AuditRecord> {
    records
        .iter()
        .filter(|record| record.action == action)
        .collect()
}

fn exec_args(command: &str) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("shell".to_string(), json!("cmd"));
    args.insert("command".to_string(), json!(command));
    args
}

/// The full-session test: correlation, attribution, redaction, secret
/// exclusion, and a sealed verifiable chain from one real session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unified_audit_correlates_attributes_redacts_and_seals() {
    let dir = temp_dir_for("e2e");
    let (audit, audit_rx) = ChannelAudit::bounded(1024);
    let (start_tx, start_rx) = oneshot::channel();
    let audit_task = spawn_audit_service(config_for(&dir), audit_rx, start_rx);

    let (host, joiner, session, identity) = active_session_with_audit(&audit).await;
    let approval_code = host
        .approval_code()
        .expect("approval code after pairing")
        .to_string();
    let log = open_session_log(&config_for(&dir), session).expect("open session log");
    start_tx.send(log).expect("deliver session log");

    let (service, _driver) = ConsoleSessionService::start(host, session, identity);
    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
    let ipc_audit: Arc<dyn IpcAudit> = Arc::new(audit.clone());
    let in_flight = service.activity_counter();
    let (ipc_shutdown_tx, ipc_shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(IpcServer::serve(
        listener,
        route,
        ipc_audit,
        in_flight,
        ipc_shutdown_rx,
    ));
    let client = bridge_client(endpoint).await;

    // 1. A bridge exec with an environment override whose value must never
    //    reach the log (REQ-S-018: the name may appear, the value never).
    let mut args = exec_args("echo bridge-mark");
    args.insert("env".to_string(), json!({ "CL_AUDIT_PROBE": ENV_SECRET }));
    let exec = client
        .call_tool(CallToolRequestParams::new("remote_exec").with_arguments(args))
        .await
        .expect("bridge exec");
    assert_ne!(exec.is_error, Some(true));
    let exec_body = structured(&exec);
    let bridge_job = exec_body["result"]["job"].as_u64().expect("bridge job id");

    // 2. A bridge exec marked sensitive: its text must be redacted while the
    //    action, job id, and outcome remain (REQ-S-018).
    let mut args = exec_args(&format!("echo {SENSITIVE_COMMAND_MARKER}"));
    args.insert("sensitive".to_string(), json!(true));
    let sensitive = client
        .call_tool(CallToolRequestParams::new("remote_exec").with_arguments(args))
        .await
        .expect("sensitive exec");
    assert_ne!(sensitive.is_error, Some(true));
    let sensitive_job = structured(&sensitive)["result"]["job"]
        .as_u64()
        .expect("sensitive job id");

    // 3. The same remote action manually from the console path, audited with
    //    actor `console` exactly as the CLI loop records it.
    let mut events = service.subscribe();
    let mut manual = ManualActivity::new();
    let started = service
        .run_command(ExecRequest::new(Shell::Cmd, "echo manual-mark".to_string()))
        .await
        .expect("manual exec starts");
    manual.command_started(&audit, session, started.job, "echo manual-mark");
    assert!(manual.is_manual(started.job));
    let manual_job = started.job;
    loop {
        let event = timeout(STEP, events.recv())
            .await
            .expect("manual job finishes in time")
            .expect("event stream open");
        if let SessionEvent::ExecFinished {
            job,
            exit_code,
            outcome,
        } = event
            && job == manual_job
        {
            assert!(
                manual.job_finished(&audit, session, job, exit_code, outcome),
                "the CLI's own job must be recognised as manual"
            );
            break;
        }
    }
    // A bridge job is NOT manual: its finish is audited by the routed call,
    // never double-recorded by the CLI bookkeeping.
    assert!(!manual.job_finished(
        &audit,
        session,
        cretlink_protocol::JobId(bridge_job),
        Some(0),
        cretlink_protocol::JobOutcome::Completed
    ));

    // 4. A verified upload through the bridge; its audit record must carry the
    //    canonical committed path and the same digest the report carries.
    let payload = b"cretlink-audit-e2e-upload-payload".repeat(64);
    let source = dir.join("upload-src.bin");
    std::fs::write(&source, &payload).expect("write upload source");
    let mut upload_args = Map::new();
    upload_args.insert(
        "local_path".to_string(),
        json!(source.to_string_lossy().into_owned()),
    );
    upload_args.insert("dest_name".to_string(), json!("audit-e2e.bin"));
    upload_args.insert("source_label".to_string(), json!("test:audit-e2e"));
    upload_args.insert("cleanup".to_string(), json!("on_session_end"));
    let upload = client
        .call_tool(CallToolRequestParams::new("remote_upload").with_arguments(upload_args))
        .await
        .expect("bridge upload");
    assert_ne!(upload.is_error, Some(true));
    let upload_body = structured(&upload);
    assert_eq!(upload_body["result"]["outcome"], json!("completed"));
    let report_digest = upload_body["result"]["digest_hex"]
        .as_str()
        .expect("digest hex")
        .to_string();
    let report_path = upload_body["result"]["remote_path"]
        .as_str()
        .expect("committed path")
        .to_string();

    // Tear everything down so every audit sender drops and the log seals:
    // the IPC server shuts down gracefully so in-flight completion records
    // still flush.
    client.cancel().await.ok();
    let _ = ipc_shutdown_tx.send(());
    timeout(Duration::from_secs(10), server)
        .await
        .expect("ipc server stops within its grace window")
        .expect("ipc server joins");
    service.stop();
    drop(service);
    drop(joiner);
    drop(audit);
    timeout(SEAL, audit_task)
        .await
        .expect("audit drain ends")
        .expect("audit drain joins");

    let (bytes, records) = read_session_log(&dir);

    // The chain is sealed and every record belongs to the routed session.
    let report = verify_sealed(Cursor::new(&bytes)).expect("verify runs");
    assert!(
        matches!(report, ChainReport::Valid { .. }),
        "sealed log must verify: {report:?}"
    );
    assert!(records.iter().all(|record| record.session == session));

    // Lifecycle events landed durably (host_with_audit, ST-M003-006).
    assert!(!find(&records, "pairing_completed").is_empty());
    assert!(!find(&records, "consent_granted").is_empty());

    // Bridge exec correlation: job id, command text, exit code, duration,
    // honest outcome — all under actor `mcp`.
    let exec_records = find(&records, "mcp_remote_exec");
    let normal = exec_records
        .iter()
        .find(|record| int_arg(record, "job_id") == i64::try_from(bridge_job).unwrap())
        .expect("bridge exec record");
    assert_eq!(normal.actor, Actor::Mcp);
    assert_eq!(normal.outcome, Outcome::Ok);
    assert_eq!(
        arg(normal, "command"),
        &AuditValue::String("echo bridge-mark".to_string())
    );
    assert!(normal.args.0.contains_key("exit_code"));
    assert!(normal.duration_ms.is_some());

    // Capture-time start record: written before the call ran, sharing the
    // console-minted call_id with the completion and already carrying the
    // command — an executed action can never be traceless (ST-M003-006).
    let call_id = int_arg(normal, "call_id");
    let start = find(&records, "mcp_remote_exec_start")
        .into_iter()
        .find(|record| int_arg(record, "call_id") == call_id)
        .expect("capture-time start record sharing the completion's call_id");
    assert_eq!(start.actor, Actor::Mcp);
    assert_eq!(
        arg(start, "command"),
        &AuditValue::String("echo bridge-mark".to_string())
    );
    assert!(
        start.seq < normal.seq,
        "the start record precedes the completion in the chain"
    );
    // Every routed call carries its start record: the upload too.
    assert!(!find(&records, "mcp_remote_upload_start").is_empty());
    match arg(normal, "env") {
        AuditValue::Map(env) => {
            assert!(env.contains_key("CL_AUDIT_PROBE"), "env NAME may appear");
            assert!(
                matches!(
                    env["CL_AUDIT_PROBE"],
                    AuditValue::Redacted { sha256: None, .. }
                ),
                "env VALUE must be a digest-free redaction marker"
            );
        }
        other => panic!("env must be a map: {other:?}"),
    }

    // Sensitive exec: the command is only a redaction marker, but the action,
    // job id, and outcome remain (REQ-S-018).
    let redacted = exec_records
        .iter()
        .find(|record| int_arg(record, "job_id") == i64::try_from(sensitive_job).unwrap())
        .expect("sensitive exec record");
    assert!(
        matches!(
            arg(redacted, "command"),
            AuditValue::Redacted { sha256: None, .. }
        ),
        "sensitive command must be redacted without a correlation digest"
    );
    assert_eq!(redacted.outcome, Outcome::Ok);

    // Manual exec correlation and attribution: start and finish share one job
    // id under actor `console`, distinguishable from the bridge records by
    // actor alone.
    let starts = find(&records, "console_exec_start");
    let finishes = find(&records, "console_exec_finish");
    assert_eq!(starts.len(), 1);
    assert_eq!(finishes.len(), 1);
    assert_eq!(starts[0].actor, Actor::Console);
    assert_eq!(finishes[0].actor, Actor::Console);
    let manual = i64::try_from(manual_job.0).unwrap();
    assert_eq!(int_arg(starts[0], "job_id"), manual);
    assert_eq!(int_arg(finishes[0], "job_id"), manual);
    assert!(finishes[0].args.0.contains_key("exit_code"));
    assert!(finishes[0].duration_ms.is_some());
    assert_eq!(finishes[0].outcome, Outcome::Ok);

    // Transfer record: canonical committed path and digest equality with the
    // routed report (the ST-M003-004 audit half).
    let uploads = find(&records, "mcp_remote_upload");
    assert_eq!(uploads.len(), 1);
    let upload_record = uploads[0];
    assert_eq!(upload_record.actor, Actor::Mcp);
    assert_eq!(upload_record.outcome, Outcome::Ok);
    assert_eq!(
        arg(upload_record, "path"),
        &AuditValue::String(report_path.clone())
    );
    assert_eq!(
        int_arg(upload_record, "len"),
        i64::try_from(payload.len()).unwrap()
    );
    assert_eq!(
        upload_record.digests.get("sha256"),
        Some(&report_digest),
        "audit digest must equal the verified transfer report digest"
    );

    // Negative byte scan (REQ-S-017): the sensitive command text and the
    // environment secret never appear anywhere in the produced bytes. Both
    // contain uppercase letters, so no lowercase-hex digest can mask a miss.
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains(SENSITIVE_COMMAND_MARKER),
        "sensitive command text leaked into the audit log"
    );
    assert!(
        !text.contains(ENV_SECRET),
        "environment override value leaked into the audit log"
    );

    // The six-digit Session Approval Code is numeric, so scan the records with
    // the hex chain/digest fields stripped to keep the check deterministic
    // (a random 64-char hex string could contain any 6-digit sequence).
    for line in bytes.split(|byte| *byte == b'\n').filter(|l| !l.is_empty()) {
        let mut value: Value = serde_json::from_slice(line).expect("record json");
        let object = value.as_object_mut().expect("record object");
        object.remove("prev");
        object.remove("digests");
        let stripped = serde_json::to_string(object).expect("re-serialize");
        assert!(
            !stripped.contains(&approval_code),
            "the Session Approval Code must never be written to the audit log"
        );
    }

    // Nothing from the agent side double-books the console's exec records:
    // the runtime's agent-side hooks stay non-durable (NoAudit) by design.
    assert!(find(&records, "exec_start").is_empty());
    assert!(find(&records, "exec_finish").is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Thousands of records through the bounded channel: nothing is dropped, the
/// chain still verifies, and the log seals (REQ-N-006, ADR-0008).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn high_volume_records_are_never_dropped() {
    const THREADS: i64 = 4;
    const PER_THREAD: i64 = 1000;

    let dir = temp_dir_for("high-volume");
    // Sends never block (REQ-S-004), so the bound must hold the full burst:
    // with a healthy drain nothing may ever be dropped.
    let (audit, audit_rx) = ChannelAudit::bounded(8192);
    let (start_tx, start_rx) = oneshot::channel();
    let audit_task = spawn_audit_service(config_for(&dir), audit_rx, start_rx);
    let session = SessionId::new();
    let log = open_session_log(&config_for(&dir), session).expect("open session log");
    start_tx.send(log).expect("deliver session log");

    let producers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let audit = audit.clone();
            std::thread::spawn(move || {
                for seq in 0..PER_THREAD {
                    let mut entry =
                        AuditEntry::event(Actor::Console, Some(session), "hv_probe", Outcome::Ok);
                    entry.args = AuditArgs::new()
                        .with("thread", AuditValue::Int(thread))
                        .with("seq", AuditValue::Int(seq));
                    audit.record_entry(entry);
                }
            })
        })
        .collect();
    for producer in producers {
        producer.join().expect("producer thread");
    }
    assert_eq!(audit.dropped(), 0, "a healthy drain must drop nothing");
    drop(audit);
    timeout(Duration::from_secs(120), audit_task)
        .await
        .expect("audit drain ends")
        .expect("audit drain joins");

    let (bytes, records) = read_session_log(&dir);
    let report = verify_sealed(Cursor::new(&bytes)).expect("verify runs");
    match report {
        ChainReport::Valid { records: verified } => {
            // Header + every probe + seal: flush-per-record, no drops.
            assert_eq!(verified, u64::try_from(THREADS * PER_THREAD).unwrap() + 2);
        }
        other => panic!("high-volume log must verify sealed: {other:?}"),
    }
    assert_eq!(
        records
            .iter()
            .filter(|record| record.action == "hv_probe")
            .count(),
        usize::try_from(THREADS * PER_THREAD).unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Above the per-session soft cap, payload args are replaced by the explicit
/// `elided` marker while every action record keeps being written (ADR-0008).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soft_cap_elides_payload_but_keeps_every_action() {
    const ENTRIES: usize = 40;

    let dir = temp_dir_for("soft-cap");
    let mut config = config_for(&dir);
    config.soft_cap_bytes = 4 * 1024;
    let (audit, audit_rx) = ChannelAudit::bounded(64);
    let (start_tx, start_rx) = oneshot::channel();
    let audit_task = spawn_audit_service(config.clone(), audit_rx, start_rx);
    let session = SessionId::new();
    let log = open_session_log(&config, session).expect("open session log");
    start_tx.send(log).expect("deliver session log");

    for seq in 0..ENTRIES {
        let mut entry = AuditEntry::event(Actor::Console, Some(session), "cap_probe", Outcome::Ok);
        entry.args = AuditArgs::new()
            .with("payload", AuditValue::String("p".repeat(512)))
            .with("job_id", AuditValue::Int(i64::try_from(seq).unwrap()));
        audit.record_entry(entry);
    }
    drop(audit);
    timeout(SEAL, audit_task)
        .await
        .expect("audit drain ends")
        .expect("audit drain joins");

    let (bytes, records) = read_session_log(&dir);
    assert!(
        matches!(
            verify_sealed(Cursor::new(&bytes)).expect("verify runs"),
            ChainReport::Valid { .. }
        ),
        "capped log must still verify sealed"
    );
    let probes: Vec<_> = records
        .iter()
        .filter(|record| record.action == "cap_probe")
        .collect();
    // Every action record was written — the cap elides detail, never actions.
    assert_eq!(probes.len(), ENTRIES);
    let with_payload = probes
        .iter()
        .filter(|record| record.args.0.contains_key("payload"))
        .count();
    let elided: Vec<_> = probes
        .iter()
        .filter(|record| record.args.0.get("elided") == Some(&AuditValue::Bool(true)))
        .collect();
    assert!(with_payload > 0, "records below the cap keep their payload");
    assert!(
        !elided.is_empty(),
        "records above the cap carry the elided marker"
    );
    assert_eq!(with_payload + elided.len(), ENTRIES);
    // Elision removes only the large payload; the small correlation fields
    // survive so request -> job -> result stays reconstructable (ST-M003-004).
    for record in &elided {
        assert!(
            record.args.0.contains_key("job_id"),
            "elided records must keep their correlation fields: {:?}",
            record.args
        );
        assert!(
            !record.args.0.contains_key("payload"),
            "elided records must lose the large payload"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Startup pruning: over-cap logs are removed oldest-first, and the exempt
/// (active) file is never considered (ADR-0008, REQ-N-006).
#[test]
fn startup_prune_removes_over_cap_files_and_never_the_exempt_one() {
    let dir = temp_dir_for("prune-start");
    std::fs::write(dir.join("a.jsonl"), vec![b'x'; 800]).expect("write a");
    std::fs::write(dir.join("b.jsonl"), vec![b'y'; 800]).expect("write b");
    let policy = RetentionPolicy {
        max_age_days: 0,
        max_total_bytes: 1024,
    };

    // 1600 > 1024: the oldest (tie broken by name: `a.jsonl`) is pruned, then
    // 800 <= 1024 stops the pass.
    let report = prune_audit_dir(&dir, &policy, None);
    assert_eq!(report.removed, vec!["a.jsonl".to_string()]);
    assert!(report.failed.is_empty());
    assert!(!dir.join("a.jsonl").exists());
    assert!(dir.join("b.jsonl").exists());

    // The exempt (active) file is invisible to the pruner: with `b.jsonl`
    // exempt only the recreated `a.jsonl` is considered, and 800 <= 1024
    // prunes nothing.
    std::fs::write(dir.join("a.jsonl"), vec![b'x'; 800]).expect("recreate a");
    let report = prune_audit_dir(&dir, &policy, Some("b.jsonl"));
    assert!(report.removed.is_empty());
    assert!(dir.join("a.jsonl").exists());
    assert!(dir.join("b.jsonl").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// After seal, the drain prunes expired logs but never the file it just wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seal_prunes_old_logs_but_never_the_fresh_one() {
    let dir = temp_dir_for("prune-seal");
    std::fs::write(dir.join("00000000T000000000Z-old.jsonl"), vec![b'o'; 4096])
        .expect("write stale log");
    let mut config = config_for(&dir);
    // Any size is over this cap, so everything except the exempt fresh file
    // must be pruned after seal.
    config.retention = RetentionPolicy {
        max_age_days: 0,
        max_total_bytes: 1,
    };
    let (audit, audit_rx) = ChannelAudit::bounded(16);
    let (start_tx, start_rx) = oneshot::channel();
    let audit_task = spawn_audit_service(config.clone(), audit_rx, start_rx);
    let session = SessionId::new();
    let log = open_session_log(&config, session).expect("open session log");
    start_tx.send(log).expect("deliver session log");
    audit.record_entry(AuditEntry::event(
        Actor::Console,
        Some(session),
        "probe",
        Outcome::Ok,
    ));
    drop(audit);
    timeout(SEAL, audit_task)
        .await
        .expect("audit drain ends")
        .expect("audit drain joins");

    assert!(
        !dir.join("00000000T000000000Z-old.jsonl").exists(),
        "the stale log must be pruned after seal"
    );
    let (bytes, _) = read_session_log(&dir);
    assert!(
        matches!(
            verify_sealed(Cursor::new(&bytes)).expect("verify runs"),
            ChainReport::Valid { .. }
        ),
        "the fresh session log survives its own retention pass and verifies"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A session that never negotiates a session id produces no log file and no
/// stray artifacts (there is no session to name a file after).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_session_id_means_no_log_file() {
    let dir = temp_dir_for("no-session");
    let (audit, audit_rx) = ChannelAudit::bounded(16);
    let (start_tx, start_rx) = oneshot::channel::<cretlink_console::SessionAuditStart>();
    let audit_task = spawn_audit_service(config_for(&dir), audit_rx, start_rx);
    audit.record_entry(AuditEntry::event(
        Actor::Console,
        None,
        "probe",
        Outcome::Ok,
    ));
    drop(start_tx);
    drop(audit);
    timeout(SEAL, audit_task)
        .await
        .expect("audit drain ends")
        .expect("audit drain joins");
    let logs = std::fs::read_dir(&dir)
        .expect("read audit dir")
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .count();
    assert_eq!(logs, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Audit degrades honestly, never blocks: with the drain stalled (no session
/// log yet) and the channel full, sends return immediately, overflow is
/// counted, and the sealed log ends with an `audit_degraded` record carrying
/// the count (REQ-S-004: the stop path can never freeze on audit).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_drain_drops_are_counted_and_surfaced() {
    const CAPACITY: usize = 4;
    const SENT: u64 = 20;

    let dir = temp_dir_for("stall");
    let (audit, audit_rx) = ChannelAudit::bounded(CAPACITY);
    let (start_tx, start_rx) = oneshot::channel();
    let audit_task = spawn_audit_service(config_for(&dir), audit_rx, start_rx);
    let session = SessionId::new();

    // The drain is stalled waiting for the session log; only CAPACITY entries
    // fit. Every send returns immediately — none may block (REQ-S-004).
    let before = std::time::Instant::now();
    for seq in 0..SENT {
        let mut entry =
            AuditEntry::event(Actor::Console, Some(session), "stall_probe", Outcome::Ok);
        entry.args = AuditArgs::new().with("job_id", AuditValue::Int(i64::try_from(seq).unwrap()));
        audit.record_entry(entry);
    }
    assert!(
        before.elapsed() < Duration::from_secs(2),
        "sends against a stalled drain must never block"
    );
    let expected_dropped = SENT - u64::try_from(CAPACITY).unwrap();
    assert_eq!(audit.dropped(), expected_dropped);

    // Un-stall the drain; it writes what the channel held plus the honest
    // degradation record, then seals.
    let log = open_session_log(&config_for(&dir), session).expect("open session log");
    start_tx.send(log).expect("deliver session log");
    drop(audit);
    timeout(SEAL, audit_task)
        .await
        .expect("audit drain ends")
        .expect("audit drain joins");

    let (bytes, records) = read_session_log(&dir);
    assert!(
        matches!(
            verify_sealed(Cursor::new(&bytes)).expect("verify runs"),
            ChainReport::Valid { .. }
        ),
        "the degraded log still seals and verifies"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.action == "stall_probe")
            .count(),
        CAPACITY
    );
    let degraded = find(&records, "audit_degraded");
    assert_eq!(degraded.len(), 1);
    assert_eq!(degraded[0].outcome, Outcome::Failed);
    assert_eq!(
        int_arg(degraded[0], "dropped"),
        i64::try_from(expected_dropped).unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The real CLI bookkeeping path ([`ManualActivity`], driven by `main.rs`):
/// start+finish pairs share the job id with actor `console`, the redact
/// toggle redacts at capture time, cancellations are audited, and a foreign
/// (bridge) job is never double-recorded (ST-M003-006 attribution).
#[test]
fn manual_activity_records_the_cli_path_with_correct_attribution() {
    let (audit, rx) = ChannelAudit::bounded(64);
    let session = SessionId::new();
    let mut manual = ManualActivity::new();

    // Start + finish of a manual job.
    manual.command_started(&audit, session, JobId(11), "echo attributed");
    assert!(manual.is_manual(JobId(11)));
    assert!(manual.job_finished(
        &audit,
        session,
        JobId(11),
        Some(0),
        cretlink_protocol::JobOutcome::Completed
    ));
    assert!(!manual.is_manual(JobId(11)), "finished jobs leave the set");

    // A job the CLI never started (a bridge job) records nothing here.
    assert!(!manual.job_finished(
        &audit,
        session,
        JobId(99),
        Some(0),
        cretlink_protocol::JobOutcome::Completed
    ));

    // Redact toggle honored at capture time; refusal and cancel audited.
    manual.redact_commands = true;
    manual.command_refused(&audit, session, "type C:\\secrets.txt", "not active");
    manual.cancel_requested(&audit, session, JobId(11), true);

    let entries: Vec<AuditEntry> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(entries.len(), 4);

    assert_eq!(entries[0].action, "console_exec_start");
    assert_eq!(entries[0].actor, Actor::Console);
    assert_eq!(entries[0].args.0.get("job_id"), Some(&AuditValue::Int(11)));
    assert_eq!(
        entries[0].args.0.get("command"),
        Some(&AuditValue::String("echo attributed".to_string()))
    );

    assert_eq!(entries[1].action, "console_exec_finish");
    assert_eq!(entries[1].actor, Actor::Console);
    assert_eq!(entries[1].args.0.get("job_id"), Some(&AuditValue::Int(11)));
    assert_eq!(entries[1].outcome, Outcome::Ok);
    assert!(
        entries[1].duration_ms.is_some(),
        "manual finish records carry a real duration"
    );

    assert_eq!(entries[2].action, "console_exec_start");
    assert_eq!(entries[2].outcome, Outcome::Failed);
    assert!(
        matches!(
            entries[2].args.0.get("command"),
            Some(AuditValue::Redacted { sha256: None, .. })
        ),
        "the redact toggle must redact refused commands too"
    );

    assert_eq!(entries[3].action, "console_cancel");
    assert_eq!(entries[3].args.0.get("job_id"), Some(&AuditValue::Int(11)));
    assert_eq!(entries[3].outcome, Outcome::Ok);
}

/// The manual-command audit builders honor the field bound and the redact
/// toggle at capture time (REQ-S-018).
#[test]
fn manual_command_entries_truncate_and_redact() {
    let session = SessionId::new();

    // An overlong free-text field is truncated to the audit bound.
    let long = "x".repeat(MAX_FIELD_BYTES + 100);
    let entry = console_exec_started(session, JobId(7), &long, false);
    match entry.args.0.get("command").expect("command arg") {
        AuditValue::Truncated { kept, original_len } => {
            assert_eq!(*original_len, MAX_FIELD_BYTES + 100);
            assert!(kept.len() <= MAX_FIELD_BYTES);
        }
        other => panic!("overlong command must be truncated: {other:?}"),
    }

    // The redact toggle replaces the text with a digest-free marker while the
    // job id remains for correlation.
    let entry = console_exec_started(session, JobId(7), "type C:\\secrets.txt", true);
    assert!(matches!(
        entry.args.0.get("command"),
        Some(AuditValue::Redacted { sha256: None, .. })
    ));
    assert_eq!(entry.args.0.get("job_id"), Some(&AuditValue::Int(7)));
    assert_eq!(entry.actor, Actor::Console);

    // Cancel entries correlate by job id and record the honest outcome.
    let entry = console_cancel(session, JobId(9), false);
    assert_eq!(entry.outcome, Outcome::Failed);
    assert_eq!(entry.args.0.get("job_id"), Some(&AuditValue::Int(9)));
}

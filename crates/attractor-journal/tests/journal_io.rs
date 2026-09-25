//! Writer / reader / tail acceptance tests for the Run Journal (spec C3).

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use attractor_journal::{
    read_all, read_all_raw, tail_with_interval, AttemptEndReason, EventData, JournalEvent,
    JournalWriter, EVENTS_FILE, JOURNAL_VERSION,
};
use tokio_stream::StreamExt;

const RUN_ID: &str = "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b";
const POLL: Duration = Duration::from_millis(20);
const QUIET: Duration = Duration::from_millis(200);

fn stage_started(node: &str) -> EventData {
    EventData::StageStarted {
        node_id: node.to_string(),
        handler_type: "codergen".to_string(),
    }
}

fn raw_line(seq: u64, type_name: &str, data: &str) -> String {
    format!(
        "{{\"v\":1,\"seq\":{seq},\"ts\":\"2026-09-24T10:03:11.123Z\",\"run_id\":\"{RUN_ID}\",\"attempt\":1,\"type\":\"{type_name}\",\"data\":{data}}}\n"
    )
}

fn append_raw(path: &Path, bytes: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(bytes.as_bytes()).unwrap();
}

// AC-1
#[test]
fn writer_reader_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let writer = JournalWriter::open(dir.path(), RUN_ID, 1).unwrap();
    let written = vec![
        writer
            .append(EventData::RunStarted {
                pipeline_name: "X".into(),
                pipeline_path: "/abs/pipelines/x.dot".into(),
                workdir: "/abs/repo".into(),
                epic_id: Some("attractor-ino".into()),
                max_budget_usd: Some(50.5),
                max_steps: Some(200),
                shared_workdir: true,
            })
            .unwrap(),
        writer
            .append(EventData::EdgeSelected {
                from_node: "a".into(),
                to_node: "b".into(),
                edge_label: None,
            })
            .unwrap(),
        writer
            .append(EventData::AttemptEnded {
                attempt: 1,
                reason: AttemptEndReason::BudgetExhausted,
                message: Some("spent".into()),
            })
            .unwrap(),
    ];

    let read = read_all(dir.path().join(EVENTS_FILE)).unwrap();
    assert_eq!(read, written);
    assert_eq!(
        read.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    for e in &read {
        assert_eq!(e.v, JOURNAL_VERSION);
        assert_eq!(e.run_id, RUN_ID);
        assert_eq!(e.attempt, 1);
    }
    assert_eq!(read[2].data.type_name(), "AttemptEnded");

    let text = std::fs::read_to_string(dir.path().join(EVENTS_FILE)).unwrap();
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("{\"v\":1,\"seq\":1,\"ts\":\""), "{first}");
    // Envelope key order is part of the contract.
    let order = [
        "\"v\"",
        "\"seq\"",
        "\"ts\"",
        "\"run_id\"",
        "\"attempt\"",
        "\"type\"",
        "\"data\"",
    ];
    let positions: Vec<usize> = order.iter().map(|k| first.find(k).unwrap()).collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{first}");
    // ts is RFC 3339 UTC with exactly millisecond precision and a Z suffix.
    let ts = &first[first.find("\"ts\":\"").unwrap() + 6..];
    let ts = &ts[..ts.find('"').unwrap()];
    assert_eq!(ts.len(), "2026-09-24T10:03:11.123Z".len(), "{ts}");
    assert!(ts.ends_with('Z') && ts.as_bytes()[19] == b'.', "{ts}");
    // Absent optional fields are omitted, and the ending reason is snake_case.
    assert!(!text.lines().nth(1).unwrap().contains("edge_label\":\""));
    assert!(text.contains("\"reason\":\"budget_exhausted\""), "{text}");
    assert!(text.ends_with('\n'));
}

#[test]
fn spec_example_line_parses() {
    let line = r#"{"v":1,"seq":42,"ts":"2026-09-24T10:03:11.123Z","run_id":"0192...","attempt":2,"type":"StageStarted","data":{"node_id":"implement","handler_type":"codergen"}}"#;
    let e: JournalEvent = serde_json::from_str(line).unwrap();
    assert_eq!(e.seq, 42);
    assert_eq!(e.attempt, 2);
    assert_eq!(e.data, stage_started("implement"));
    assert_eq!(serde_json::to_string(&e).unwrap(), line);
}

// AC-2
#[test]
fn open_continues_seq_after_last_event() {
    let dir = tempfile::tempdir().unwrap();
    {
        let w = JournalWriter::open(dir.path(), RUN_ID, 1).unwrap();
        assert_eq!(w.next_seq(), 1);
        for n in ["a", "b", "c"] {
            w.append(stage_started(n)).unwrap();
        }
    }
    let w = JournalWriter::open(dir.path(), RUN_ID, 2).unwrap();
    assert_eq!(w.next_seq(), 4);
    let e = w.append(stage_started("d")).unwrap();
    assert_eq!((e.seq, e.attempt), (4, 2));
    let seqs: Vec<u64> = read_all(dir.path().join(EVENTS_FILE))
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
}

#[test]
fn open_counts_unknown_type_lines_for_seq() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(
        &path,
        &raw_line(6, "StageStarted", r#"{"node_id":"a","handler_type":"x"}"#),
    );
    append_raw(&path, &raw_line(7, "FutureEvent", r#"{"x":1}"#));
    let w = JournalWriter::open(dir.path(), RUN_ID, 3).unwrap();
    assert_eq!(w.append(stage_started("b")).unwrap().seq, 8);
}

#[test]
fn open_truncates_torn_final_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(
        &path,
        &raw_line(1, "StageStarted", r#"{"node_id":"a","handler_type":"x"}"#),
    );
    let torn = raw_line(2, "StageStarted", r#"{"node_id":"b","handler_type":"x"}"#);
    append_raw(&path, &torn[..torn.len() / 2]);

    let w = JournalWriter::open(dir.path(), RUN_ID, 2).unwrap();
    let e = w.append(stage_started("c")).unwrap();
    assert_eq!(e.seq, 2);
    let read = read_all(&path).unwrap();
    assert_eq!(read.len(), 2);
    assert_eq!(read[1], e);
}

#[test]
fn open_on_empty_file_starts_at_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(EVENTS_FILE), "").unwrap();
    let w = JournalWriter::open(dir.path(), RUN_ID, 1).unwrap();
    assert_eq!(w.next_seq(), 1);
}

#[test]
fn open_creates_missing_run_dir() {
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("runs").join(RUN_ID);
    let w = JournalWriter::open(&run_dir, RUN_ID, 1).unwrap();
    w.append(stage_started("a")).unwrap();
    assert!(run_dir.join(EVENTS_FILE).is_file());
}

// AC-3
#[test]
fn read_all_skips_unknown_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(
        &path,
        &raw_line(1, "StageStarted", r#"{"node_id":"a","handler_type":"x"}"#),
    );
    append_raw(&path, &raw_line(2, "FutureEvent", r#"{"x":1}"#));
    append_raw(&path, &raw_line(3, "CheckpointSaved", r#"{"node_id":"a"}"#));

    let seqs: Vec<u64> = read_all(&path).unwrap().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 3]);

    // The raw reader keeps the unknown Event, losslessly.
    let raw = read_all_raw(&path).unwrap();
    assert_eq!(raw.len(), 3);
    match &raw[1].data {
        EventData::Unknown { type_name, data } => {
            assert_eq!(type_name, "FutureEvent");
            assert_eq!(data, &serde_json::json!({"x": 1}));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
    let line = raw_line(2, "FutureEvent", r#"{"x":1}"#);
    assert_eq!(
        serde_json::to_string(&raw[1]).unwrap(),
        line.trim_end_matches('\n')
    );
}

#[test]
fn read_all_ignores_torn_final_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(&path, &raw_line(1, "CheckpointSaved", r#"{"node_id":"a"}"#));
    append_raw(&path, "{\"v\":1,\"seq\":2,");
    assert_eq!(read_all(&path).unwrap().len(), 1);
}

#[test]
fn read_all_rejects_corrupt_complete_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(&path, &raw_line(1, "CheckpointSaved", r#"{"node_id":"a"}"#));
    append_raw(&path, "not json\n");
    let err = read_all(&path).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("line 2"), "{err}");
}

#[test]
fn read_all_rejects_known_type_with_bad_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(&path, &raw_line(1, "CheckpointSaved", r#"{"wrong":1}"#));
    assert_eq!(
        read_all(&path).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn read_all_rejects_unsupported_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(
        &path,
        &raw_line(1, "CheckpointSaved", r#"{"node_id":"a"}"#).replacen("\"v\":1", "\"v\":2", 1),
    );
    assert_eq!(
        read_all(&path).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn read_all_missing_file_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        read_all(dir.path().join(EVENTS_FILE)).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

// AC-4
#[tokio::test]
async fn tail_waits_for_newline_then_emits_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(&path, &raw_line(1, "CheckpointSaved", r#"{"node_id":"a"}"#));
    let line2 = raw_line(2, "CheckpointSaved", r#"{"node_id":"b"}"#);
    let (head, rest) = line2.split_at(line2.len() / 2);
    append_raw(&path, head);

    let mut stream = Box::pin(tail_with_interval(&path, POLL));
    let first = tokio::time::timeout(QUIET, stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.seq, 1);
    assert!(
        tokio::time::timeout(QUIET, stream.next()).await.is_err(),
        "torn line must not be emitted"
    );

    // Finish the line, but hold back the newline: still nothing.
    let (body, newline) = rest.split_at(rest.len() - 1);
    append_raw(&path, body);
    assert!(tokio::time::timeout(QUIET, stream.next()).await.is_err());

    append_raw(&path, newline);
    let second = tokio::time::timeout(QUIET * 5, stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.seq, 2);
    assert!(
        tokio::time::timeout(QUIET, stream.next()).await.is_err(),
        "line must be emitted exactly once"
    );
}

// AC-5
#[tokio::test]
async fn tail_emits_appends_in_seq_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    let w = JournalWriter::open(dir.path(), RUN_ID, 1).unwrap();
    w.append(stage_started("1")).unwrap();
    w.append(stage_started("2")).unwrap();

    let mut stream = Box::pin(tail_with_interval(&path, POLL));
    for n in 3..=5 {
        tokio::time::sleep(Duration::from_millis(15)).await;
        w.append(stage_started(&n.to_string())).unwrap();
    }
    let mut seqs = Vec::new();
    while seqs.len() < 5 {
        let e = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("tail stalled")
            .expect("tail ended");
        seqs.push(e.seq);
    }
    assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    assert!(tokio::time::timeout(QUIET, stream.next()).await.is_err());
}

#[tokio::test]
async fn tail_waits_for_missing_file_and_skips_unknown_and_corrupt_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    let mut stream = Box::pin(tail_with_interval(&path, POLL));
    assert!(tokio::time::timeout(QUIET, stream.next()).await.is_err());

    append_raw(&path, &raw_line(1, "FutureEvent", r#"{"x":1}"#));
    append_raw(&path, "garbage\n");
    append_raw(&path, &raw_line(2, "CheckpointSaved", r#"{"node_id":"a"}"#));
    let e = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(e.seq, 2);
}

#[tokio::test]
async fn tail_follows_writer_that_truncated_a_torn_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(EVENTS_FILE);
    append_raw(&path, &raw_line(1, "CheckpointSaved", r#"{"node_id":"a"}"#));
    append_raw(&path, "{\"v\":1,\"seq\":2,\"ts\":\"2026-09-24T10:03:11.123Z\",\"run_id\":\"x\",\"attempt\":1,\"type\":\"Checkpoint");

    let mut stream = Box::pin(tail_with_interval(&path, POLL));
    let first = tokio::time::timeout(QUIET, stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.seq, 1);
    tokio::time::sleep(POLL * 3).await;

    // A resumed Attempt truncates the torn tail and appends a fresh line.
    let w = JournalWriter::open(dir.path(), RUN_ID, 2).unwrap();
    w.append(stage_started("resume")).unwrap();
    let e = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((e.seq, e.attempt), (2, 2));
}

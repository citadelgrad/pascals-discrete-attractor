//! Run Journal readers (spec C3, read side).

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

use crate::event::JournalEvent;

/// Default poll interval of [`tail`].
const TAIL_INTERVAL: Duration = Duration::from_millis(200);

/// Read every known Event of a journal, in file order.
///
/// Unknown Event types are skipped. A trailing line without `\n` is still
/// being written and is ignored. A complete line that is not a valid v1
/// envelope is an `InvalidData` error naming the line.
pub fn read_all(path: impl AsRef<Path>) -> io::Result<Vec<JournalEvent>> {
    let mut events = read_all_raw(path)?;
    events.retain(|e| !e.data.is_unknown());
    Ok(events)
}

/// Like [`read_all`], but keeps Events of unknown type as
/// [`EventData::Unknown`](crate::EventData::Unknown).
pub fn read_all_raw(path: impl AsRef<Path>) -> io::Result<Vec<JournalEvent>> {
    let bytes = std::fs::read(path)?;
    let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut events = Vec::new();
    for (i, line) in bytes[..complete].split(|&b| b == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let event = serde_json::from_slice::<JournalEvent>(line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("journal line {}: {e}", i + 1),
            )
        })?;
        events.push(event);
    }
    Ok(events)
}

/// Follow a journal: replay it from the start, then emit Events as they are
/// appended, in file (= `seq`) order. Must be called inside a tokio runtime.
///
/// A line is emitted only once its `\n` has been written, and exactly once.
/// The stream waits if the file does not exist yet, and skips unknown Event
/// types and unreadable lines. It never ends on its own; the consumer stops
/// (e.g. after `AttemptEnded`) by dropping it.
pub fn tail(path: impl Into<PathBuf>) -> impl Stream<Item = JournalEvent> {
    tail_with_interval(path, TAIL_INTERVAL)
}

/// [`tail`] with a custom poll interval.
pub fn tail_with_interval(
    path: impl Into<PathBuf>,
    interval: Duration,
) -> impl Stream<Item = JournalEvent> {
    let path = path.into();
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        // Byte offset just past the last complete line already emitted. Any
        // partial line after it is re-read on the next poll, which also
        // copes with a writer that cut off a torn line and appended anew.
        let mut committed: u64 = 0;
        loop {
            if tx.is_closed() {
                return;
            }
            if let Ok(lines) = read_complete_lines(&path, &mut committed).await {
                for line in lines {
                    let Ok(event) = serde_json::from_slice::<JournalEvent>(&line) else {
                        continue;
                    };
                    if event.data.is_unknown() {
                        continue;
                    }
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
    ReceiverStream::new(rx)
}

/// Read the complete lines after `committed` and advance it past them.
async fn read_complete_lines(path: &Path, committed: &mut u64) -> io::Result<Vec<Vec<u8>>> {
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    if len < *committed {
        // The file shrank below what was emitted; follow it from its new end.
        *committed = len;
    }
    if len == *committed {
        return Ok(Vec::new());
    }
    file.seek(io::SeekFrom::Start(*committed)).await?;
    let mut buf = Vec::with_capacity((len - *committed) as usize);
    file.read_to_end(&mut buf).await?;
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(Vec::new());
    };
    buf.truncate(end + 1);
    // `read_to_end` may take several reads. If a new Attempt cut off a torn
    // line and appended between them, `buf` splices old and new bytes. Only
    // advance once a second read of the same range agrees; else retry.
    if !unchanged(&mut file, *committed, &buf).await? {
        return Ok(Vec::new());
    }
    *committed += end as u64 + 1;
    Ok(buf[..end]
        .split(|&b| b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .map(<[u8]>::to_vec)
        .collect())
}

/// Whether the file still holds `expected` at `offset`.
async fn unchanged(file: &mut tokio::fs::File, offset: u64, expected: &[u8]) -> io::Result<bool> {
    file.seek(io::SeekFrom::Start(offset)).await?;
    let mut again = vec![0; expected.len()];
    match file.read_exact(&mut again).await {
        Ok(_) => Ok(again == expected),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &[u8] = b"{\"seq\":1}\n{\"seq\":2,\"type\":\"Torn";
    const NEW: &[u8] = b"{\"seq\":1}\n{\"seq\":2,\"type\":\"Fresh\",\"attempt\":2}\n";
    const LINE1: u64 = 11;

    // A read that began before a resumed writer cut off the torn line and
    // finished after it appended holds spliced bytes; they must not match.
    #[tokio::test]
    async fn spliced_read_is_rejected_and_fresh_line_follows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, NEW).unwrap();
        let mut spliced = OLD[LINE1 as usize..].to_vec();
        spliced.extend_from_slice(&NEW[OLD.len()..]);

        let mut file = tokio::fs::File::open(&path).await.unwrap();
        assert!(!unchanged(&mut file, LINE1, &spliced).await.unwrap());

        let mut committed = LINE1;
        let lines = read_complete_lines(&path, &mut committed).await.unwrap();
        assert_eq!(lines, vec![NEW[LINE1 as usize..NEW.len() - 1].to_vec()]);
        assert_eq!(committed, NEW.len() as u64);
    }

    #[tokio::test]
    async fn range_cut_short_is_not_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, &OLD[..LINE1 as usize]).unwrap();
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        assert!(unchanged(&mut file, 0, &OLD[..LINE1 as usize])
            .await
            .unwrap());
        assert!(!unchanged(&mut file, 0, OLD).await.unwrap());
    }

    // Regression: a spliced read used to advance past the fresh line, which
    // was then never emitted. A writer repeatedly tears and resumes while the
    // tail follows; every complete line must arrive exactly once, in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tail_never_loses_a_line_across_torn_resumes() {
        use tokio_stream::StreamExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, b"").unwrap();
        let mut stream = Box::pin(tail_with_interval(&path, Duration::from_millis(1)));
        let rounds = 200u64;
        let writer_path = path.clone();
        let writer = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            for seq in 1..=rounds {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&writer_path)
                    .unwrap();
                f.write_all(&vec![b'x'; 4000]).unwrap(); // torn tail
                std::thread::sleep(Duration::from_micros(300));
                let keep = std::fs::read(&writer_path).unwrap().len() as u64 - 4000;
                f.set_len(keep).unwrap();
                let line = format!(
                    "{{\"v\":1,\"seq\":{seq},\"ts\":\"2026-09-24T10:03:11.123Z\",\"run_id\":\"r\",\"attempt\":{seq},\"type\":\"CheckpointSaved\",\"data\":{{\"node_id\":\"{}\"}}}}\n",
                    "n".repeat(3000)
                );
                f.write_all(line.as_bytes()).unwrap();
            }
        });
        for want in 1..=rounds {
            let e = tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .unwrap_or_else(|_| panic!("line {want} was lost"))
                .unwrap();
            assert_eq!(e.seq, want);
        }
        writer.await.unwrap();
    }
}

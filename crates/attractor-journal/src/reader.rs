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
    *committed += end as u64 + 1;
    Ok(buf[..end]
        .split(|&b| b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
        .map(<[u8]>::to_vec)
        .collect())
}

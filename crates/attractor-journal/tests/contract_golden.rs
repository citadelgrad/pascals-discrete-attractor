//! Golden-file contract tests for Run Journal v1 (spec C3) and Run Index v1
//! (spec C4).
//!
//! `tests/golden/journal/<Type>.jsonl` holds hand-written journal lines for
//! every v1 Event type; the first line of each file sets every field to a
//! non-empty value. `tests/golden/index/entry.jsonl` holds one Index line.
//!
//! Every golden line must decode, and encoding it again must reproduce every
//! golden field with the same value. The output may add a field only if its
//! value is empty (`null`, `false`, `0`, `""`, `[]`, `{}`): that is how a new
//! optional field looks, and it must not break these tests. A renamed or
//! removed field either fails to decode or goes missing from the output.
//!
//! The golden files are never regenerated from code. Editing or deleting an
//! existing golden line is a breaking v1 change: bump `v` and read both
//! versions (C3). The only allowed edit is adding a newly introduced optional
//! field to the first line of its file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use attractor_journal::{
    append_entry_at, read_all, read_index_at, EventData, JournalEvent, INDEX_VERSION,
    JOURNAL_VERSION,
};
use serde_json::{Map, Value};

/// Envelope keys of a journal line, in contract order.
const ENVELOPE: [&str; 7] = ["v", "seq", "ts", "run_id", "attempt", "type", "data"];
/// Keys of a Run Index line.
const INDEX_KEYS: [&str; 6] = [
    "v",
    "run_id",
    "started_at",
    "workdir",
    "pipeline_path",
    "run_dir",
];

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn index_golden() -> PathBuf {
    golden_dir().join("index/entry.jsonl")
}

/// `(type, lines)` of every journal golden file, sorted by type.
fn journal_goldens() -> Vec<(String, Vec<String>)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(golden_dir().join("journal"))
        .expect("tests/golden/journal must exist")
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            assert_eq!(
                path.extension().and_then(|e| e.to_str()),
                Some("jsonl"),
                "unexpected file {}",
                path.display()
            );
            let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
            let text = std::fs::read_to_string(&path).unwrap();
            (stem, text.lines().map(str::to_string).collect())
        })
        .collect()
}

fn all_journal_lines() -> Vec<(String, String)> {
    journal_goldens()
        .into_iter()
        .flat_map(|(ty, lines)| lines.into_iter().map(move |l| (ty.clone(), l)))
        .collect()
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// Why `output` does not reproduce `golden`, if it does not. Every golden key
/// must be in the output with a covering value; a key only in the output is
/// allowed if its value is empty. Numbers compare as written, so `50.0` does
/// not cover `50`.
fn covers(golden: &Value, output: &Value, path: &str) -> Result<(), String> {
    match (golden, output) {
        (Value::Object(g), Value::Object(o)) => {
            for (k, gv) in g {
                let p = format!("{path}.{k}");
                let ov = o.get(k).ok_or_else(|| format!("{p} is missing"))?;
                covers(gv, ov, &p)?;
            }
            for (k, ov) in o {
                if !g.contains_key(k) && !is_empty_value(ov) {
                    return Err(format!("{path}.{k} = {ov} is not in the golden"));
                }
            }
            Ok(())
        }
        (Value::Array(g), Value::Array(o)) => {
            if g.len() != o.len() {
                return Err(format!("{path} has {} items, want {}", o.len(), g.len()));
            }
            for (i, (gv, ov)) in g.iter().zip(o).enumerate() {
                covers(gv, ov, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        _ if golden == output => Ok(()),
        _ => Err(format!("{path} = {output}, want {golden}")),
    }
}

/// Why a journal line breaks the v1 contract, if it does.
fn contract_violation(line: &str) -> Option<String> {
    let event: JournalEvent = match serde_json::from_str(line) {
        Ok(e) => e,
        Err(e) => return Some(format!("does not decode: {e}")),
    };
    if event.data.is_unknown() {
        return Some(format!(
            "decodes as unknown type {}",
            event.data.type_name()
        ));
    }
    let encoded = serde_json::to_string(&event).unwrap();
    let golden: Value = serde_json::from_str(line).unwrap();
    let output: Value = serde_json::from_str(&encoded).unwrap();
    if let Err(e) = covers(&golden, &output, "") {
        return Some(format!("output {encoded} does not cover the golden: {e}"));
    }
    // The envelope keys come first and in contract order; the order of the
    // keys inside `data` is not part of the contract.
    let prefix = format!(
        "{{\"v\":{},\"seq\":{},\"ts\":{},\"run_id\":{},\"attempt\":{},\"type\":{},\"data\":",
        golden["v"],
        golden["seq"],
        golden["ts"],
        golden["run_id"],
        golden["attempt"],
        golden["type"]
    );
    if !encoded.starts_with(&prefix) {
        return Some(format!(
            "envelope of {encoded} does not start with {prefix}"
        ));
    }
    None
}

fn index_violation(line: &str) -> Option<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.jsonl");
    std::fs::write(&path, format!("{line}\n")).unwrap();
    // The Index reader skips lines it cannot read, so count the entries.
    let entries = read_index_at(&path).unwrap();
    if entries.len() != 1 {
        return Some(format!("read {} entries, want 1", entries.len()));
    }
    let encoded = serde_json::to_string(&entries[0]).unwrap();
    let golden: Value = serde_json::from_str(line).unwrap();
    let output: Value = serde_json::from_str(&encoded).unwrap();
    covers(&golden, &output, "")
        .err()
        .map(|e| format!("output {encoded} does not cover the golden: {e}"))
}

/// Copies of `line` with one field key renamed each: envelope keys, `data`
/// keys, and keys of objects inside `data` arrays (`tasks[]`, `commits[]`).
/// Map-valued fields such as `blocked_by` are not recursed into: their keys
/// are Task IDs, not fields.
fn renamed_variants(line: &str) -> Vec<(String, String)> {
    fn rename(obj: &mut Map<String, Value>, key: &str) {
        let v = obj.remove(key).unwrap();
        obj.insert(format!("{key}_renamed"), v);
    }
    let golden: Value = serde_json::from_str(line).unwrap();
    let mut out = Vec::new();
    for key in golden.as_object().unwrap().keys() {
        let mut v = golden.clone();
        rename(v.as_object_mut().unwrap(), key);
        out.push((key.clone(), v.to_string()));
    }
    let Some(data) = golden["data"].as_object() else {
        return out;
    };
    for (key, field) in data {
        let mut v = golden.clone();
        rename(v["data"].as_object_mut().unwrap(), key);
        out.push((format!("data.{key}"), v.to_string()));
        if let Value::Array(items) = field {
            for (i, item) in items.iter().enumerate() {
                for inner in item.as_object().into_iter().flat_map(Map::keys) {
                    let mut v = golden.clone();
                    rename(v["data"][key][i].as_object_mut().unwrap(), inner);
                    out.push((format!("data.{key}[{i}].{inner}"), v.to_string()));
                }
            }
        }
    }
    out
}

// AC-1: a golden file exists for each Event type in C3 and for one Index entry.
#[test]
fn golden_file_for_every_event_type() {
    let goldens = journal_goldens();
    let stems: BTreeSet<&str> = goldens.iter().map(|(t, _)| t.as_str()).collect();
    let known: BTreeSet<&str> = EventData::KNOWN_TYPES.iter().copied().collect();
    assert_eq!(stems, known, "golden files must match the v1 Event types");
    assert_eq!(known.len(), 24, "C3 lists 24 Event types");

    let mut files: Vec<PathBuf> = std::fs::read_dir(golden_dir().join("journal"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.push(index_golden());
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.is_empty(), "{} is empty", path.display());
        assert!(text.ends_with('\n'), "{} must end with \\n", path.display());
        assert!(
            !text.contains('\r'),
            "{} must not contain \\r",
            path.display()
        );
        assert!(
            text.lines().all(|l| !l.trim().is_empty()),
            "{} has a blank line",
            path.display()
        );
    }
    let index = std::fs::read_to_string(index_golden()).unwrap();
    assert_eq!(index.lines().count(), 1, "the Index golden holds one entry");
}

// AC-1
#[test]
fn every_golden_line_has_its_file_type() {
    for (ty, line) in all_journal_lines() {
        let event: JournalEvent =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("{ty}: {e}\n{line}"));
        assert_eq!(event.v, JOURNAL_VERSION, "{line}");
        assert_eq!(event.data.type_name(), ty, "{line}");
        assert!(!event.data.is_unknown(), "{line}");
    }
}

// AC-1, AC-2: both directions, for every golden line.
#[test]
fn every_golden_line_meets_the_contract() {
    let violations: Vec<String> = all_journal_lines()
        .into_iter()
        .filter_map(|(ty, line)| contract_violation(&line).map(|e| format!("{ty}: {e}")))
        .collect();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

// AC-2: the first line of each file sets every field, so a field the code
// drops on read cannot hide behind an empty value.
#[test]
fn first_golden_line_sets_every_field() {
    for (ty, lines) in journal_goldens() {
        let golden: Value = serde_json::from_str(&lines[0]).unwrap();
        for (key, value) in golden["data"].as_object().unwrap() {
            assert!(
                !is_empty_value(value),
                "{ty}: first line sets data.{key} to an empty value"
            );
        }
    }
}

// AC-2: renaming an enum value fails to decode, so every value must appear.
#[test]
fn golden_lines_cover_every_enum_value() {
    let values = |ty: &str, field: &str| -> BTreeSet<String> {
        journal_goldens()
            .into_iter()
            .filter(|(t, _)| t == ty)
            .flat_map(|(_, lines)| lines)
            .map(|l| {
                let v: Value = serde_json::from_str(&l).unwrap();
                v["data"][field].as_str().unwrap().to_string()
            })
            .collect()
    };
    let set = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    assert_eq!(
        values("AttemptEnded", "reason"),
        set(&[
            "completed",
            "failed",
            "stopped",
            "budget_exhausted",
            "max_steps",
            "error"
        ])
    );
    assert_eq!(
        values("HumanInputAnswered", "source"),
        set(&["terminal", "monitor", "cli"])
    );
}

// AC-2: renaming a key in a golden line is what the code sees after it
// renames or removes that field: the golden key is no longer read. Every such
// rename must break the contract.
#[test]
fn renaming_any_golden_field_breaks_the_contract() {
    let mut survivors = Vec::new();
    for (ty, lines) in journal_goldens() {
        let mut visited = 0;
        for line in &lines {
            for (field, renamed) in renamed_variants(line) {
                visited += 1;
                if contract_violation(&renamed).is_none() {
                    survivors.push(format!("{ty}: renaming {field} went unnoticed"));
                }
            }
        }
        assert!(visited > ENVELOPE.len(), "{ty}: no data fields visited");
    }
    assert!(survivors.is_empty(), "{}", survivors.join("\n"));
}

// AC-2: the sweep reaches the fields of nested objects.
#[test]
fn rename_sweep_reaches_nested_fields() {
    let fields: BTreeSet<String> = journal_goldens()
        .into_iter()
        .flat_map(|(_, lines)| lines)
        .flat_map(|l| renamed_variants(&l).into_iter().map(|(f, _)| f))
        .collect();
    for want in [
        "data.tasks[0].id",
        "data.tasks[0].title",
        "data.tasks[0].status",
        "data.commits[0].sha",
        "data.commits[0].subject",
        "data.commits[0].author",
        "data.commits[0].ts",
    ] {
        assert!(fields.contains(want), "sweep never renames {want}");
    }
    for key in ENVELOPE {
        assert!(fields.contains(key), "sweep never renames {key}");
    }
}

// AC-3: the reader ignores fields it does not know, in the envelope and in
// `data`, so an Event with a newly added field still reads.
#[test]
fn unknown_fields_in_golden_lines_are_ignored() {
    for (ty, line) in all_journal_lines() {
        let original: JournalEvent = serde_json::from_str(&line).unwrap();
        let mut v: Value = serde_json::from_str(&line).unwrap();
        v["added_later"] = Value::from("x");
        v["data"]["added_later"] = serde_json::json!({"nested": [1, 2]});
        let extended: JournalEvent = serde_json::from_str(&v.to_string())
            .unwrap_or_else(|e| panic!("{ty}: extra fields broke decoding: {e}"));
        assert_eq!(extended, original, "{ty}");
    }
}

// AC-3: the rule that lets a new optional field pass.
#[test]
fn covers_allows_new_empty_fields_only() {
    let golden = serde_json::json!({"a": 1, "b": {"c": [1.5]}});
    let ok = |o: Value| covers(&golden, &o, "").is_ok();
    assert!(ok(serde_json::json!({"a": 1, "b": {"c": [1.5]}})));
    for empty in [
        Value::Null,
        Value::Bool(false),
        Value::from(0),
        Value::from(""),
        serde_json::json!([]),
        serde_json::json!({}),
    ] {
        assert!(ok(
            serde_json::json!({"a": 1, "b": {"c": [1.5], "new": empty}})
        ));
    }
    assert!(!ok(
        serde_json::json!({"a": 1, "b": {"c": [1.5]}, "new": "x"})
    ));
    assert!(!ok(
        serde_json::json!({"a": 1, "b": {"c": [1.5]}, "new": true})
    ));
    assert!(!ok(serde_json::json!({"b": {"c": [1.5]}})));
    assert!(!ok(serde_json::json!({"a": 2, "b": {"c": [1.5]}})));
    assert!(!ok(serde_json::json!({"a": 1.0, "b": {"c": [1.5]}})));
    assert!(!ok(serde_json::json!({"a": 1, "b": {"c": []}})));
    assert!(!ok(serde_json::json!({"a": 1, "b": {"c": [1.5, 0]}})));
}

// AC-1: the golden lines form a journal the reader takes whole.
#[test]
fn golden_journal_reads_through_read_all() {
    let lines = all_journal_lines();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let text: String = lines.iter().map(|(_, l)| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();
    let events = read_all(&path).unwrap();
    assert_eq!(events.len(), lines.len(), "read_all dropped golden lines");
    for (event, (ty, _)) in events.iter().zip(&lines) {
        assert_eq!(event.data.type_name(), ty);
    }
}

// AC-1: the Index golden reads, and appending it again writes the same entry.
#[test]
fn index_golden_round_trips() {
    let line = std::fs::read_to_string(index_golden()).unwrap();
    let line = line.trim_end_matches('\n');
    assert_eq!(index_violation(line), None);

    let golden: Value = serde_json::from_str(line).unwrap();
    let keys: BTreeSet<&str> = golden
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, INDEX_KEYS.into_iter().collect());
    assert!(golden.get("status").is_none(), "status is never stored");

    let entries = read_index_at(&index_golden()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].v, INDEX_VERSION);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.jsonl");
    append_entry_at(&path, &entries[0]).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    let output: Value = serde_json::from_str(written.trim_end_matches('\n')).unwrap();
    covers(&golden, &output, "").unwrap();
    assert_eq!(read_index_at(&path).unwrap(), entries);
}

// AC-2 for the Index.
#[test]
fn renaming_any_index_field_breaks_the_contract() {
    let line = std::fs::read_to_string(index_golden()).unwrap();
    let golden: Value = serde_json::from_str(line.trim_end_matches('\n')).unwrap();
    for key in INDEX_KEYS {
        let mut v = golden.clone();
        let obj = v.as_object_mut().unwrap();
        let value = obj.remove(key).unwrap();
        obj.insert(format!("{key}_renamed"), value);
        assert!(
            index_violation(&v.to_string()).is_some(),
            "renaming {key} went unnoticed"
        );
    }
}

// AC-3 for the Index: an unknown field does not drop the entry.
#[test]
fn unknown_fields_in_the_index_golden_are_ignored() {
    let line = std::fs::read_to_string(index_golden()).unwrap();
    let mut v: Value = serde_json::from_str(line.trim_end_matches('\n')).unwrap();
    v["added_later"] = Value::from("x");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.jsonl");
    std::fs::write(&path, format!("{v}\n")).unwrap();
    assert_eq!(
        read_index_at(&path).unwrap(),
        read_index_at(&index_golden()).unwrap()
    );
}

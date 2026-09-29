//! ADR 0001: the Monitor observes through journals only and never links the
//! engine or planning code. This test inspects `cargo metadata` to enforce it.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::process::Command;

const ROOT: &str = "attractor-monitor";
const FORBIDDEN: [&str; 5] = [
    "attractor-pipeline",
    "attractor-llm",
    "attractor-agent",
    "attractor-tools",
    "attractor-cli",
];

/// Walks every dependency edge (normal, build and dev) from `root` and returns
/// one message per reachable forbidden crate, naming it and the chain to it.
fn forbidden_reachable(metadata: &Value, root: &str, forbidden: &[&str]) -> Vec<String> {
    let names: HashMap<&str, &str> = metadata["packages"]
        .as_array()
        .expect("packages array")
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p["name"].as_str().unwrap()))
        .collect();
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve nodes")
    {
        let deps = node["deps"]
            .as_array()
            .map(|d| d.iter().map(|d| d["pkg"].as_str().unwrap()).collect())
            .unwrap_or_default();
        edges.insert(node["id"].as_str().unwrap(), deps);
    }
    let root_id = names
        .iter()
        .find(|(_, n)| **n == root)
        .map(|(id, _)| *id)
        .unwrap_or_else(|| panic!("{root} not found in cargo metadata"));

    let mut parent: HashMap<&str, &str> = HashMap::new();
    let mut queue = VecDeque::from([root_id]);
    let mut seen = vec![root_id];
    while let Some(id) = queue.pop_front() {
        for &dep in edges.get(id).map(Vec::as_slice).unwrap_or(&[]) {
            if !seen.contains(&dep) {
                seen.push(dep);
                parent.insert(dep, id);
                queue.push_back(dep);
            }
        }
    }
    let mut out = Vec::new();
    for &id in &seen {
        let name = names[id];
        if forbidden.contains(&name) {
            let mut chain = vec![name];
            let mut cur = id;
            while let Some(&p) = parent.get(cur) {
                chain.push(names[p]);
                cur = p;
            }
            chain.reverse();
            out.push(format!(
                "{root} must not depend on {name}: {}",
                chain.join(" -> ")
            ));
        }
    }
    out
}

fn reaches(metadata: &Value, root: &str, target: &str) -> bool {
    !forbidden_reachable(metadata, root, &[target]).is_empty()
}

fn cargo_metadata() -> Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let out = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--all-features",
            "--manifest-path",
            manifest,
        ])
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("parse cargo metadata")
}

#[test]
fn monitor_does_not_depend_on_engine_crates() {
    let metadata = cargo_metadata();
    // Non-vacuous: the walk must actually traverse edges.
    assert!(
        reaches(&metadata, ROOT, "attractor-journal"),
        "sanity: attractor-journal should be reachable from {ROOT}"
    );
    let bad = forbidden_reachable(&metadata, ROOT, &FORBIDDEN);
    assert!(
        bad.is_empty(),
        "attractor-monitor must not depend on engine crates (ADR 0001): {}",
        bad.join("; ")
    );
}

fn synthetic(edges: &[(&str, &str, &str)]) -> Value {
    let all = [
        ROOT,
        "attractor-journal",
        "attractor-pipeline",
        "attractor-llm",
        "attractor-agent",
        "attractor-tools",
        "attractor-cli",
    ];
    let packages: Vec<Value> = all.iter().map(|n| json!({"id": n, "name": n})).collect();
    let nodes: Vec<Value> = all
        .iter()
        .map(|n| {
            let deps: Vec<Value> = edges
                .iter()
                .filter(|(from, _, _)| from == n)
                .map(|(_, to, kind)| {
                    let kind = if kind.is_empty() {
                        Value::Null
                    } else {
                        json!(kind)
                    };
                    json!({"pkg": to, "dep_kinds": [{"kind": kind}]})
                })
                .collect();
            json!({"id": n, "deps": deps})
        })
        .collect();
    json!({"packages": packages, "resolve": {"nodes": nodes}})
}

#[test]
fn forbidden_dependency_is_reported_by_name() {
    for name in FORBIDDEN {
        for (label, edges) in [
            ("direct", vec![(ROOT, name, "")]),
            ("dev", vec![(ROOT, name, "dev")]),
            ("build", vec![(ROOT, name, "build")]),
            (
                "transitive",
                vec![
                    (ROOT, "attractor-journal", ""),
                    ("attractor-journal", name, ""),
                ],
            ),
        ] {
            let bad = forbidden_reachable(&synthetic(&edges), ROOT, &FORBIDDEN);
            assert_eq!(bad.len(), 1, "{label} {name}: {bad:?}");
            assert!(bad[0].contains(name), "{label} {name}: {}", bad[0]);
        }
    }
}

#[test]
fn transitive_report_shows_the_chain() {
    let g = synthetic(&[
        (ROOT, "attractor-journal", ""),
        ("attractor-journal", "attractor-llm", ""),
    ]);
    let bad = forbidden_reachable(&g, ROOT, &FORBIDDEN);
    assert!(
        bad[0].contains("attractor-monitor -> attractor-journal -> attractor-llm"),
        "{bad:?}"
    );
}

#[test]
fn clean_graph_and_unrelated_edges_are_not_reported() {
    // Forbidden crates that the monitor does not reach (reverse edge) are fine.
    let g = synthetic(&[(ROOT, "attractor-journal", ""), ("attractor-cli", ROOT, "")]);
    assert!(forbidden_reachable(&g, ROOT, &FORBIDDEN).is_empty());
}

#[test]
#[should_panic(expected = "not found in cargo metadata")]
fn missing_root_panics_instead_of_passing_vacuously() {
    forbidden_reachable(&synthetic(&[]), "no-such-crate", &FORBIDDEN);
}

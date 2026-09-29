//! The Proposal (draft Epic + Tasks + dependencies) as the Monitor edits it.
//!
//! The Monitor cannot link the CLI's `Proposal` (ADR 0002), so this is its own
//! copy of the C6 v1 shape. Serde defaults match `pas decompose`, so a file
//! saved here is accepted by `--from-proposal`. Dependencies are indices into
//! `tasks`; only [`Proposal::remove_task`] changes indices.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(default = "one")]
    pub v: u32,
    pub epic: Epic,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub dependencies: Vec<Dep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Epic {
    pub title: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub title: String,
    #[serde(default = "default_type")]
    pub r#type: String,
    #[serde(default = "default_priority")]
    pub priority: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dep {
    pub blocked: usize,
    pub blocker: usize,
}

fn one() -> u32 {
    1
}
fn default_type() -> String {
    "task".into()
}
fn default_priority() -> String {
    "P2".into()
}

fn field<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Empty text means "not set" for the optional fields.
fn opt(text: &str) -> Option<String> {
    let t = text.trim();
    (!t.is_empty()).then(|| text.to_string())
}

impl Proposal {
    /// `Task N "title"`, N counted from 1 as the editor shows it.
    fn label(&self, i: usize) -> String {
        format!("Task {} \"{}\"", i + 1, self.tasks[i].title.trim())
    }

    /// Delete Task `i`, drop every dependency that mentions it and renumber
    /// the rest, so each remaining dependency still names the same Tasks.
    pub fn remove_task(&mut self, i: usize) {
        if i >= self.tasks.len() {
            return;
        }
        self.tasks.remove(i);
        self.dependencies
            .retain(|d| d.blocked != i && d.blocker != i);
        for d in &mut self.dependencies {
            if d.blocked > i {
                d.blocked -= 1;
            }
            if d.blocker > i {
                d.blocker -= 1;
            }
        }
    }

    /// The CLI's checks plus cycle detection, which the CLI lacks.
    pub fn validate(&self) -> Result<(), String> {
        if self.v != 1 {
            return Err(format!("unsupported Proposal version {}", self.v));
        }
        if self.tasks.is_empty() {
            return Err("Proposal has no Tasks".into());
        }
        for (i, t) in self.tasks.iter().enumerate() {
            if t.title.trim().is_empty() {
                return Err(format!("Task {} has an empty title", i + 1));
            }
        }
        let n = self.tasks.len();
        for d in &self.dependencies {
            if d.blocked >= n || d.blocker >= n {
                return Err(format!(
                    "dependency index out of range (blocked={}, blocker={}, tasks={n})",
                    d.blocked, d.blocker
                ));
            }
            if d.blocked == d.blocker {
                return Err(format!("{} cannot depend on itself", self.label(d.blocked)));
            }
        }
        self.find_cycle()
    }

    /// DFS along "blocked by" edges; names the Tasks of the first cycle found.
    fn find_cycle(&self) -> Result<(), String> {
        let n = self.tasks.len();
        let mut out = vec![Vec::new(); n];
        for d in &self.dependencies {
            out[d.blocked].push(d.blocker);
        }
        // 0 = unvisited, 1 = on the current path, 2 = done.
        let mut state = vec![0u8; n];
        let mut path = Vec::new();
        for start in 0..n {
            if state[start] == 0 {
                if let Some(cycle) = self.dfs(start, &out, &mut state, &mut path) {
                    let names: Vec<String> = cycle.iter().map(|&i| self.label(i)).collect();
                    return Err(format!(
                        "dependency cycle: {} (each is blocked by the next)",
                        names.join(" → ")
                    ));
                }
            }
        }
        Ok(())
    }

    fn dfs(
        &self,
        at: usize,
        out: &[Vec<usize>],
        state: &mut [u8],
        path: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        state[at] = 1;
        path.push(at);
        for &next in &out[at] {
            if state[next] == 1 {
                let from = path.iter().position(|&p| p == next).unwrap_or(0);
                let mut cycle = path[from..].to_vec();
                cycle.push(next);
                return Some(cycle);
            }
            if state[next] == 0 {
                if let Some(c) = self.dfs(next, out, state, path) {
                    return Some(c);
                }
            }
        }
        path.pop();
        state[at] = 2;
        None
    }

    /// Apply the editor's fields to `self`. Only fields present are changed
    /// (so `acceptance`, `design` and `notes` survive a form that omits them);
    /// `dep` entries replace the dependencies whenever `epic_title` is sent,
    /// since unchecked boxes send nothing. Removal is applied last, because
    /// the form's Task numbers are the pre-removal ones.
    pub fn apply_form(&mut self, pairs: &[(String, String)]) -> Result<(), String> {
        if let Some(v) = field(pairs, "epic_title") {
            self.epic.title = v.to_string();
        }
        if let Some(v) = field(pairs, "epic_description") {
            self.epic.description = v.to_string();
        }
        for (i, t) in self.tasks.iter_mut().enumerate() {
            if let Some(v) = field(pairs, &format!("t{i}_title")) {
                t.title = v.to_string();
            }
            if let Some(v) = field(pairs, &format!("t{i}_type")) {
                t.r#type = v.trim().to_string();
            }
            if let Some(v) = field(pairs, &format!("t{i}_priority")) {
                t.priority = v.trim().to_string();
            }
            if let Some(v) = field(pairs, &format!("t{i}_description")) {
                t.description = v.to_string();
            }
            if let Some(v) = field(pairs, &format!("t{i}_acceptance")) {
                t.acceptance = opt(v);
            }
            if let Some(v) = field(pairs, &format!("t{i}_design")) {
                t.design = opt(v);
            }
            if let Some(v) = field(pairs, &format!("t{i}_notes")) {
                t.notes = opt(v);
            }
        }
        if field(pairs, "epic_title").is_some() {
            let mut deps: Vec<Dep> = Vec::new();
            for (k, v) in pairs.iter().filter(|(k, _)| k == "dep") {
                let bad = || format!("malformed dependency {k}={v}");
                let (b, k2) = v.split_once(':').ok_or_else(bad)?;
                let dep = Dep {
                    blocked: b.parse().map_err(|_| bad())?,
                    blocker: k2.parse().map_err(|_| bad())?,
                };
                if !deps.contains(&dep) {
                    deps.push(dep);
                }
            }
            self.dependencies = deps;
        }
        if let Some(r) = field(pairs, "remove") {
            let i: usize = r.parse().map_err(|_| format!("malformed remove={r}"))?;
            if i >= self.tasks.len() {
                return Err(format!("no Task {} to remove", i + 1));
            }
            self.remove_task(i);
        }
        Ok(())
    }
}

pub fn read(path: &Path) -> io::Result<Proposal> {
    serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)
}

/// Write via a temp file and rename, so a reader never sees half a file.
pub fn write_atomic(path: &Path, p: &Proposal) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(p).map_err(io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(title: &str) -> Task {
        Task {
            title: title.into(),
            r#type: "task".into(),
            priority: "P2".into(),
            description: format!("{title} body"),
            acceptance: None,
            design: None,
            notes: None,
        }
    }

    fn prop(titles: &[&str], deps: &[(usize, usize)]) -> Proposal {
        Proposal {
            v: 1,
            epic: Epic {
                title: "E".into(),
                description: "d".into(),
            },
            tasks: titles.iter().map(|t| task(t)).collect(),
            dependencies: deps
                .iter()
                .map(|&(blocked, blocker)| Dep { blocked, blocker })
                .collect(),
        }
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn remove_task_drops_deps_and_renumbers() {
        let mut p = prop(&["A", "B", "C", "D"], &[(1, 0), (2, 1), (3, 2)]);
        p.remove_task(1);
        let titles: Vec<_> = p.tasks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["A", "C", "D"]);
        // Only "D blocked by C" survives, renumbered.
        assert_eq!(
            p.dependencies,
            vec![Dep {
                blocked: 2,
                blocker: 1
            }]
        );
        assert!(p.validate().is_ok());
    }

    #[test]
    fn remove_out_of_range_changes_nothing() {
        let mut p = prop(&["A"], &[]);
        p.remove_task(5);
        assert_eq!(p.tasks.len(), 1);
    }

    #[test]
    fn cycle_names_the_tasks() {
        let p = prop(
            &["Add API", "Add UI", "Add docs"],
            &[(0, 1), (1, 2), (2, 0)],
        );
        let msg = p.validate().unwrap_err();
        for name in [
            "Task 1 \"Add API\"",
            "Task 2 \"Add UI\"",
            "Task 3 \"Add docs\"",
        ] {
            assert!(msg.contains(name), "{msg}");
        }
        assert!(msg.contains("cycle"));
    }

    #[test]
    fn two_task_cycle_and_acyclic_diamond() {
        assert!(prop(&["A", "B"], &[(0, 1), (1, 0)]).validate().is_err());
        let diamond = prop(&["A", "B", "C", "D"], &[(3, 1), (3, 2), (1, 0), (2, 0)]);
        assert!(diamond.validate().is_ok());
    }

    #[test]
    fn other_validation_failures() {
        assert!(prop(&["A"], &[(0, 0)])
            .validate()
            .unwrap_err()
            .contains("itself"));
        assert!(prop(&["A"], &[(0, 3)])
            .validate()
            .unwrap_err()
            .contains("out of range"));
        assert!(prop(&["A", " "], &[])
            .validate()
            .unwrap_err()
            .contains("Task 2"));
        assert!(prop(&[], &[]).validate().unwrap_err().contains("no Tasks"));
        let mut p = prop(&["A"], &[]);
        p.v = 2;
        assert!(p.validate().unwrap_err().contains("version"));
    }

    #[test]
    fn form_edits_fields_and_keeps_omitted_ones() {
        let mut p = prop(&["A", "B"], &[]);
        p.tasks[1].acceptance = Some("acc".into());
        p.apply_form(&pairs(&[
            ("epic_title", "New epic"),
            ("t0_title", "A2"),
            ("t1_design", ""),
            ("dep", "1:0"),
            ("dep", "1:0"),
        ]))
        .unwrap();
        assert_eq!(p.epic.title, "New epic");
        assert_eq!(p.tasks[0].title, "A2");
        assert_eq!(p.tasks[1].acceptance.as_deref(), Some("acc"));
        assert_eq!(p.tasks[1].design, None);
        assert_eq!(
            p.dependencies,
            vec![Dep {
                blocked: 1,
                blocker: 0
            }]
        );
    }

    #[test]
    fn form_without_epic_title_leaves_dependencies_alone() {
        let mut p = prop(&["A", "B"], &[(1, 0)]);
        p.apply_form(&[]).unwrap();
        assert_eq!(p.dependencies.len(), 1);
    }

    #[test]
    fn form_remove_uses_pre_removal_numbers() {
        let mut p = prop(&["A", "B", "C"], &[(2, 1), (2, 0)]);
        p.apply_form(&pairs(&[
            ("epic_title", "E"),
            ("t2_title", "C2"),
            ("dep", "2:1"),
            ("dep", "2:0"),
            ("remove", "1"),
        ]))
        .unwrap();
        let titles: Vec<_> = p.tasks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["A", "C2"]);
        assert_eq!(
            p.dependencies,
            vec![Dep {
                blocked: 1,
                blocker: 0
            }]
        );
    }

    #[test]
    fn bad_form_values_are_errors() {
        let mut p = prop(&["A"], &[]);
        assert!(p
            .apply_form(&pairs(&[("epic_title", "E"), ("dep", "x")]))
            .is_err());
        assert!(p.apply_form(&pairs(&[("remove", "9")])).is_err());
        assert!(p.apply_form(&pairs(&[("remove", "-1")])).is_err());
    }

    #[test]
    fn json_matches_the_cli_shape() {
        let p: Proposal = serde_json::from_str(
            r#"{"epic":{"title":"E","description":"d"},"tasks":[{"title":"A","description":"a"}]}"#,
        )
        .unwrap();
        assert_eq!(
            (
                p.v,
                p.tasks[0].r#type.as_str(),
                p.tasks[0].priority.as_str()
            ),
            (1, "task", "P2")
        );
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("acceptance"));
    }
}

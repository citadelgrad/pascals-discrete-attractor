#![cfg(not(feature = "monitor"))]

use std::process::Command;

fn pas() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pas"))
}

#[test]
fn monitor_is_not_listed_without_the_feature() {
    let out = pas().arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.lines().any(|l| l.trim_start().starts_with("monitor")),
        "{text}"
    );
}

#[test]
fn monitor_subcommand_is_a_usage_error_without_the_feature() {
    let out = pas().arg("monitor").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

//! End-to-end checks for the clap-parsed `:` groups.
//!
//! Help and usage errors are produced during parsing, before the database is
//! opened, so these tests never touch the user's state.

use std::process::Command;

fn im(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_im"))
        .args(args)
        .output()
        .expect("failed to run im")
}

#[test]
fn group_help_uses_colon_usage() {
    let out = im(&[":tool", "mood", "--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage: im :tool mood [MOOD]..."));

    let out = im(&[":c", "--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage: im :tool config [TARGET]"));
}

#[test]
fn bad_group_form_exits_two() {
    let out = im(&[":db", "wat"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unrecognized subcommand 'wat'"));

    assert_eq!(im(&[":db"]).status.code(), Some(2));
    assert_eq!(im(&[":db", "doctor", "extra"]).status.code(), Some(2));
}

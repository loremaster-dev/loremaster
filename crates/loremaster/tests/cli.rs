//! Command-line surface.

use std::process::{Command, Output};

fn loremaster(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn help_lists_the_commands() {
    let output = loremaster(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for command in ["hook", "daemon", "enable", "login", "status"] {
        assert!(help.contains(command), "{help}");
    }
    assert_eq!(help.contains("dev "), cfg!(feature = "dev"), "{help}");
}

#[cfg(not(feature = "dev"))]
#[test]
fn dev_commands_do_not_exist_in_a_normal_build() {
    let output = loremaster(&["dev", "seed", "x.jsonl"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'dev'"));
}

#[test]
fn daemon_without_any_home_fails() {
    let output = Command::new(env!("CARGO_BIN_EXE_loremaster"))
        .arg("daemon")
        .env_remove("HOME")
        .env_remove("LOREMASTER_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("LOREMASTER_HOME"));
}

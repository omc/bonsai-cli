//! `bonsai skill`, which needs no API.

use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn bonsai(home: &Path) -> Command {
    let mut c = Command::cargo_bin("bonsai").unwrap();
    c.env_remove("CLAUDE_CONFIG_DIR")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("BONSAI_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .env("CI", "1");
    c
}

fn status(out: &std::process::Output) -> String {
    assert!(out.status.success(), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["status"].as_str().unwrap().to_owned()
}

#[test]
fn install_writes_what_print_prints() {
    let home = tempfile::tempdir().unwrap();
    let out = bonsai(home.path())
        .args(["skill", "install"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("Installed the bonsai skill"), "{stderr}");

    let printed = bonsai(home.path())
        .args(["skill", "print"])
        .output()
        .unwrap()
        .stdout;
    let written = std::fs::read(home.path().join(".claude/skills/bonsai/SKILL.md")).unwrap();
    assert_eq!(written, printed);
}

#[test]
fn install_reports_unchanged_then_updated() {
    let home = tempfile::tempdir().unwrap();
    let install = || {
        bonsai(home.path())
            .args(["skill", "install", "--json"])
            .output()
            .unwrap()
    };
    assert_eq!(status(&install()), "installed");
    assert_eq!(status(&install()), "unchanged");
    std::fs::write(home.path().join(".claude/skills/bonsai/SKILL.md"), "old").unwrap();
    assert_eq!(status(&install()), "updated");
}

#[test]
fn install_honours_claude_config_dir() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join("elsewhere");
    let out = bonsai(home.path())
        .env("CLAUDE_CONFIG_DIR", &claude)
        .args(["skill", "install", "--json"])
        .output()
        .unwrap();
    assert_eq!(status(&out), "installed");
    assert!(claude.join("skills/bonsai/SKILL.md").is_file());
    assert!(!home.path().join(".claude").exists());
}

#[test]
fn project_installs_into_the_working_directory() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let out = bonsai(home.path())
        .current_dir(project.path())
        .args(["skill", "install", "--project", "--json"])
        .output()
        .unwrap();
    assert_eq!(status(&out), "installed");
    assert!(
        project
            .path()
            .join(".claude/skills/bonsai/SKILL.md")
            .is_file()
    );
    assert!(!home.path().join(".claude").exists());
}

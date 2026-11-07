//! `install.cmd /dry-run` on Windows (Python `test_install.py` InstallCmdTests).
#![cfg(windows)]

mod common;

use std::process::Command;

use common::*;

#[test]
fn dry_run_prints_the_steps_and_changes_nothing() {
    let temp = Temp::new();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::create_dir_all(temp.join("brand")).unwrap();
    std::fs::copy(root.join("install.cmd"), temp.join("install.cmd")).unwrap();
    std::fs::copy(root.join("brand").join("icon.ico"), temp.join("brand").join("icon.ico")).unwrap();
    let before: Vec<_> = std::fs::read_dir(temp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    let run = || {
        let output = Command::new("cmd")
            .args(["/c", "install.cmd", "/dry-run"])
            .current_dir(temp.path())
            .env("INSTALL_SHORTCUT_DIR", temp.join("shortcuts"))
            .output()
            .unwrap();
        (output.status.code(), String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let (code, output) = run();
    assert_eq!(code, Some(0), "{output}");
    for text in [
        "[dry-run]",
        "Step 1",
        "SHA256SUMS",
        "dead-air-v0.3.0-x86_64-pc-windows-msvc.zip",
        "Dead Air.lnk",
        "INSTALL_SHORTCUT_DIR",
        "Done. Nothing was changed.",
    ] {
        assert!(output.contains(text), "{text}\n{output}");
    }
    assert!(output.contains("running: dead-air.exe gui --config dead-air.toml"));
    assert!(!output.contains("--watch"));
    let after: Vec<_> = std::fs::read_dir(temp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(before, after);
    std::fs::write(temp.join("dead-air.toml"), "[[streams]]\nid = \"a\"\n").unwrap();
    let (_, output) = run();
    assert!(output.contains("running: dead-air.exe gui --config dead-air.toml --watch"), "{output}");
}

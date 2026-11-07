//! `install.sh` (Python `test_install.py`): dry run, verified release download, missing-release
//! fallback to cargo, the rustup hint, and launchers. Network and real builds are faked.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

fn bash() -> Option<PathBuf> {
    which("bash").filter(|b| Command::new(b).args(["-c", "exit 0"]).status().map(|s| s.success()).unwrap_or(false))
}

fn tool(dir: &Path, name: &str, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Setup {
    temp: Temp,
    repo: PathBuf,
    bin: PathBuf,
    home_bin: PathBuf,
    shortcuts: PathBuf,
}

/// A copy of install.sh in a folder with a space, plus fake uname/curl tools.
fn setup(system: &str, machine: &str) -> Setup {
    let temp = Temp::new();
    let repo = temp.join("repo copy");
    std::fs::create_dir_all(repo.join("brand")).unwrap();
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"), repo.join("install.sh")).unwrap();
    let bin = temp.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    tool(&bin, "uname", &format!("#!/bin/sh\ncase \"$1\" in -m) echo {machine} ;; *) echo {system} ;; esac\n"));
    // curl: serve file:// URLs from the fake release folder, 404 (exit 22) for anything else.
    tool(
        &bin,
        "curl",
        "#!/bin/sh\nout=; url=\nwhile [ $# -gt 0 ]; do case \"$1\" in -o) out=$2; shift 2; continue ;; -*) ;; *) url=$1 ;; esac; shift; done\ncase \"$url\" in file://*) p=${url#file://}; [ -f \"$p\" ] && cp \"$p\" \"$out\" && exit 0 ;; esac\nexit 22\n",
    );
    let home_bin = temp.join("home").join(".local").join("bin");
    let shortcuts = temp.join("shortcuts");
    Setup { temp, repo, bin, home_bin, shortcuts }
}

fn run(setup: &Setup, args: &[&str], release: Option<&Path>, path_dirs: &[&Path]) -> (i32, String) {
    let mut path = path_dirs.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>();
    path.push(setup.bin.to_string_lossy().into_owned());
    path.push("/usr/bin:/bin".into());
    let mut command = Command::new(bash().unwrap());
    command
        .arg("install.sh")
        .args(args)
        .current_dir(&setup.repo)
        .env_clear()
        .env("PATH", path.join(":"))
        .env("HOME", setup.temp.join("home"))
        .env("DEAD_AIR_BIN_DIR", &setup.home_bin)
        .env("INSTALL_SHORTCUT_DIR", &setup.shortcuts);
    command.env(
        "DEAD_AIR_RELEASE_BASE",
        match release {
            Some(dir) => format!("file://{}", dir.display()),
            None => "https://release.invalid/none".into(),
        },
    );
    let output = command.output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr),
    )
}

/// A fake release folder with one archive and its SHA256SUMS.
fn release(setup: &Setup, target: &str, corrupt: bool) -> PathBuf {
    let dir = setup.temp.join("release");
    let stage = setup.temp.join("stage").join(format!("dead-air-v0.3.0-{target}"));
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    tool(&stage, "dead-air", "#!/bin/sh\necho dead-air 0.3.0\n");
    let archive = dir.join(format!("dead-air-v0.3.0-{target}.tar.gz"));
    assert!(
        Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(stage.parent().unwrap())
            .arg(stage.file_name().unwrap())
            .status()
            .unwrap()
            .success()
    );
    let digest = {
        use sha2::Digest;
        dead_air::random::hex(&sha2::Sha256::digest(std::fs::read(&archive).unwrap()))
    };
    let digest = if corrupt { "0".repeat(64) } else { digest };
    std::fs::write(dir.join("SHA256SUMS"), format!("{digest}  {}\n", archive.file_name().unwrap().to_string_lossy()))
        .unwrap();
    dir
}

#[test]
fn syntax() {
    let Some(bash) = bash() else { return };
    let status =
        Command::new(bash).args(["-n", "install.sh"]).current_dir(env!("CARGO_MANIFEST_DIR")).status().unwrap();
    assert!(status.success());
}

#[test]
fn dry_run_prints_the_steps_and_changes_nothing() {
    if bash().is_none() {
        return;
    }
    let setup = setup("Linux", "x86_64");
    let (code, output) = run(&setup, &["--dry-run"], None, &[]);
    assert_eq!(code, 0, "{output}");
    for text in [
        "[dry-run] Nothing will be changed.",
        "dead-air-v0.3.0-x86_64-unknown-linux-musl.tar.gz",
        "SHA256SUMS",
        "rustup",
        "Done. Nothing was changed.",
    ] {
        assert!(output.contains(text), "{text}\n{output}");
    }
    let names: Vec<String> = std::fs::read_dir(&setup.repo)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(!setup.home_bin.exists() && !setup.shortcuts.exists());
}

#[test]
fn a_verified_release_is_installed_and_the_desktop_entry_points_at_it() {
    if bash().is_none() {
        return;
    }
    let setup = setup("Linux", "x86_64");
    let dir = release(&setup, "x86_64-unknown-linux-musl", false);
    let (code, output) = run(&setup, &[], Some(&dir), &[]);
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("verified release 0.3.0"), "{output}");
    let exe = setup.home_bin.join("dead-air");
    assert_eq!(Command::new(&exe).output().unwrap().stdout, b"dead-air 0.3.0\n");
    let entry = std::fs::read_to_string(setup.shortcuts.join("dead-air.desktop")).unwrap();
    let expected = format!("Exec=\"{}\" gui --config \"{}/dead-air.toml\"\n", exe.display(), setup.repo.display());
    assert!(entry.contains(&expected), "{entry}");
    assert!(entry.contains("Name=Dead Air") && entry.contains("Terminal=false"));
}

#[test]
fn a_checksum_mismatch_installs_nothing() {
    if bash().is_none() {
        return;
    }
    let setup = setup("Linux", "x86_64");
    let dir = release(&setup, "x86_64-unknown-linux-musl", true);
    let (code, output) = run(&setup, &[], Some(&dir), &[]);
    assert_eq!(code, 1);
    assert!(output.contains("does not match"), "{output}");
    assert!(!setup.home_bin.join("dead-air").exists());
}

#[test]
fn a_missing_release_falls_back_to_cargo_and_then_to_the_rustup_hint() {
    if bash().is_none() {
        return;
    }
    let setup = setup("Linux", "x86_64");
    let cargo_dir = setup.temp.join("cargo-bin");
    std::fs::create_dir_all(&cargo_dir).unwrap();
    let log = setup.temp.join("cargo.log");
    tool(
        &cargo_dir,
        "cargo",
        &format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nroot=\nwhile [ $# -gt 0 ]; do [ \"$1\" = --root ] && root=$2; shift; done\nmkdir -p \"$root/bin\" && printf '#!/bin/sh\\necho built\\n' > \"$root/bin/dead-air\" && chmod +x \"$root/bin/dead-air\"\n",
            log.display()
        ),
    );
    let (code, output) = run(&setup, &[], None, &[&cargo_dir]);
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("no release download"), "{output}");
    assert!(std::fs::read_to_string(&log).unwrap().contains("install --locked --path"));
    assert_eq!(Command::new(setup.home_bin.join("dead-air")).output().unwrap().stdout, b"built\n");

    let bare = self::setup("Linux", "x86_64");
    let (code, output) = run(&bare, &[], None, &[]);
    assert_eq!(code, 1);
    assert!(output.contains("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"), "{output}");
}

#[test]
fn launchers_pass_watch_only_when_the_config_already_has_a_stream() {
    if bash().is_none() {
        return;
    }
    let setup = setup("Linux", "x86_64");
    let dir = release(&setup, "x86_64-unknown-linux-musl", false);
    std::fs::write(setup.repo.join("dead-air.toml"), "[[streams]]\nid = \"a\"\n").unwrap();
    assert_eq!(run(&setup, &[], Some(&dir), &[]).0, 0);
    let entry = std::fs::read_to_string(setup.shortcuts.join("dead-air.desktop")).unwrap();
    assert!(entry.lines().any(|l| l.starts_with("Exec=") && l.ends_with("dead-air.toml\" --watch")), "{entry}");

    let mac = self::setup("Darwin", "arm64");
    let dir = release(&mac, "aarch64-apple-darwin", false);
    let (code, output) = run(&mac, &[], Some(&dir), &[]);
    assert_eq!(code, 0, "{output}");
    let script = std::fs::read_to_string(mac.shortcuts.join("Dead Air.command")).unwrap();
    assert!(script.starts_with("#!/bin/bash\n"));
    assert!(script.contains(&format!("cd \"{}\" || exit 1\n", mac.repo.display())));
    assert!(
        script.contains(&format!("exec \"{}\" gui --config dead-air.toml\n", mac.home_bin.join("dead-air").display()))
    );
}

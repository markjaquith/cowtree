#![cfg(target_os = "macos")]

use std::{fs, path::Path, process::Command};

fn git(cwd: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn is_apfs(path: &Path) -> bool {
    let output = Command::new("stat")
        .args(["-f", "%T"])
        .arg(path)
        .output()
        .unwrap();
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "apfs"
}

#[test]
fn preserves_divergent_and_dirty_paths_and_writes_receipt() {
    let root = tempfile::tempdir().unwrap();
    if !is_apfs(root.path()) {
        return;
    }
    let repo = root.path().join("repo");
    let target = root.path().join("feature");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.email", "cowtree@example.com"]);
    git(&repo, &["config", "user.name", "cowtree test"]);
    fs::write(repo.join("same"), "same\n").unwrap();
    fs::write(repo.join("committed"), "main\n").unwrap();
    fs::write(repo.join("dirty"), "clean\n").unwrap();
    fs::write(repo.join("staged"), "clean\n").unwrap();
    std::os::unix::fs::symlink("same", repo.join("link")).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    git(
        &repo,
        &["worktree", "add", "-b", "feature", target.to_str().unwrap()],
    );
    fs::write(target.join("committed"), "feature\n").unwrap();
    git(&target, &["add", "committed"]);
    git(&target, &["commit", "-m", "diverge"]);
    fs::write(target.join("dirty"), "dirty\n").unwrap();
    fs::write(target.join("staged"), "staged\n").unwrap();
    git(&target, &["add", "staged"]);

    let binary = env!("CARGO_BIN_EXE_cowtree");
    let compact = Command::new(binary)
        .current_dir(&repo)
        .args(["compact", "feature", "--json"])
        .output()
        .unwrap();
    assert!(
        compact.status.success(),
        "{}",
        String::from_utf8_lossy(&compact.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&compact.stdout).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["results"][0]["cloned_files"], 1);
    assert_eq!(
        fs::read_to_string(target.join("committed")).unwrap(),
        "feature\n"
    );
    assert_eq!(fs::read_to_string(target.join("dirty")).unwrap(), "dirty\n");
    assert_eq!(
        fs::read_to_string(target.join("staged")).unwrap(),
        "staged\n"
    );
    assert!(
        fs::symlink_metadata(target.join("link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let git_status = Command::new("git")
        .arg("-C")
        .arg(&target)
        .args(["status", "--short"])
        .output()
        .unwrap();
    let status_text = String::from_utf8_lossy(&git_status.stdout);
    assert!(status_text.contains(" M dirty\n"));
    assert!(status_text.contains("M  staged\n"));

    let git_dir = Command::new("git")
        .arg("-C")
        .arg(&target)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .unwrap();
    let receipt =
        Path::new(String::from_utf8_lossy(&git_dir.stdout).trim()).join("cowtree-compaction");
    assert!(receipt.is_file());
    let legacy_receipt = receipt.with_file_name("wt-prewarm-compaction");
    fs::rename(&receipt, &legacy_receipt).unwrap();
    let status = Command::new(binary)
        .current_dir(&repo)
        .args(["status", "feature", "--json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(json["results"][0]["state"], "compacted");
    assert!(receipt.is_file());
    assert!(!legacy_receipt.exists());
}

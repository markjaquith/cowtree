#![cfg(target_os = "macos")]

use std::{
    ffi::{CStr, CString},
    fs,
    mem::MaybeUninit,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
    process::Command,
};

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
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and stat points to writable storage.
    let result = unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) };
    assert_eq!(
        result,
        0,
        "statfs failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a successful statfs initializes stat, including the NUL-terminated name.
    let stat = unsafe { stat.assume_init() };
    let filesystem = unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) };
    filesystem.to_bytes() == b"apfs"
}

fn compact_all(repo: &Path) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_cowtree"))
        .current_dir(repo)
        .args(["compact", "--all", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "compact --all failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn compact(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cowtree"))
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn retry_removes_only_stale_clone_files_and_dry_run_removes_nothing() {
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
    fs::write(
        repo.join(".gitignore"),
        format!("same.cowtree-clone.{}.9.*\n", i32::MAX),
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);
    git(
        &repo,
        &["worktree", "add", "-b", "feature", target.to_str().unwrap()],
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let stale = target.join(format!("same.cowtree-clone.{}.0.{now}", i32::MAX));
    let active = target.join(format!("same.cowtree-clone.{}.1.{now}", std::process::id()));
    let malformed = target.join("same.cowtree-clone.not-a-pid.2.3");
    let ignored = target.join(format!("same.cowtree-clone.{}.9.{now}", i32::MAX));
    for path in [&stale, &active, &malformed, &ignored] {
        fs::write(path, "temporary clone").unwrap();
    }

    let dry_run = compact(&repo, &["compact", "feature", "--dry-run"]);
    assert!(dry_run.status.success());
    assert!(stale.exists());
    assert!(active.exists());
    assert!(malformed.exists());
    assert!(ignored.exists());

    let listed = Command::new("git")
        .arg("-C")
        .arg(&target)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .unwrap();
    assert!(
        listed
            .stdout
            .split(|byte| *byte == 0)
            .any(|path| path == stale.file_name().unwrap().as_bytes()),
        "stale clone was not reported as untracked: {:?}",
        String::from_utf8_lossy(&listed.stdout)
    );

    let retry = compact(&repo, &["compact", "feature"]);
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert!(
        !stale.exists(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert!(active.exists());
    assert!(malformed.exists());
    assert!(ignored.exists());
}

#[test]
fn compact_all_compacts_new_worktrees_and_skips_current_receipts() {
    let root = tempfile::tempdir().unwrap();
    if !is_apfs(root.path()) {
        return;
    }
    let repo = root.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.email", "cowtree@example.com"]);
    git(&repo, &["config", "user.name", "cowtree test"]);
    fs::write(repo.join("same"), "same\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "fixture"]);

    for branch in ["feature-one", "feature-two"] {
        let target = root.path().join(branch);
        git(
            &repo,
            &["worktree", "add", "-b", branch, target.to_str().unwrap()],
        );
    }

    let first = compact_all(&repo);
    assert_eq!(first["schema_version"], 1);
    assert_eq!(
        first["summary"],
        serde_json::json!({"compacted": 2, "skipped": 0, "failed": 0})
    );
    let results = first["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let mut snapshots = Vec::new();
    for branch in ["feature-one", "feature-two"] {
        let target = root.path().join(branch);
        let canonical_target = target.canonicalize().unwrap();
        let result = results
            .iter()
            .find(|result| result["worktree"] == canonical_target.to_str().unwrap())
            .unwrap();
        assert_eq!(result["outcome"], "compacted");
        assert_eq!(result["cloned_files"], 1);
        assert_eq!(fs::read_to_string(target.join("same")).unwrap(), "same\n");
        let receipt = repo
            .join(".git/worktrees")
            .join(branch)
            .join("cowtree-compaction");
        snapshots.push((
            target.join("same"),
            fs::metadata(target.join("same")).unwrap().ino(),
            receipt.clone(),
            fs::metadata(&receipt).unwrap().ino(),
            fs::read(&receipt).unwrap(),
        ));
    }
    assert!(!repo.join(".git/cowtree-compaction").exists());

    let fresh = root.path().join("feature-three");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature-three",
            fresh.to_str().unwrap(),
        ],
    );

    // Exercise both a mixed batch and a batch with nothing left to compact.
    for (compacted, skipped) in [(1, 2), (0, 3)] {
        let batch = compact_all(&repo);
        assert_eq!(
            batch["summary"],
            serde_json::json!({"compacted": compacted, "skipped": skipped, "failed": 0})
        );
        let results = batch["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        for branch in ["feature-one", "feature-two", "feature-three"] {
            let target = root.path().join(branch).canonicalize().unwrap();
            let result = results
                .iter()
                .find(|result| result["worktree"] == target.to_str().unwrap())
                .unwrap();
            if branch == "feature-three" && compacted == 1 {
                assert_eq!(result["outcome"], "compacted");
                assert_eq!(result["cloned_files"], 1);
            } else {
                assert_eq!(result["outcome"], "skipped_current_receipt");
                assert_eq!(result["cloned_files"], 0);
            }
            assert_eq!(fs::read_to_string(target.join("same")).unwrap(), "same\n");
        }
        for (file, file_inode, receipt, receipt_inode, contents) in &snapshots {
            assert_eq!(fs::metadata(file).unwrap().ino(), *file_inode);
            assert_eq!(fs::metadata(receipt).unwrap().ino(), *receipt_inode);
            assert_eq!(fs::read(receipt).unwrap(), *contents);
        }
        assert!(!repo.join(".git/cowtree-compaction").exists());
    }
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
    // Enough files to exercise the bounded parallel cloning path, with shared
    // ancestors to cover the directory-validation cache as well.
    for directory in 0..16 {
        let directory = repo.join(format!("nested/{directory}"));
        fs::create_dir_all(&directory).unwrap();
        for file in 0..64 {
            fs::write(directory.join(format!("{file}.txt")), "shared contents\n").unwrap();
        }
    }
    fs::set_permissions(
        repo.join("nested/0/2.txt"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
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

    fs::set_permissions(
        target.join("nested/0/1.txt"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let before = fs::metadata(target.join("nested/0/0.txt")).unwrap();
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
    assert_eq!(json["results"][0]["cloned_files"], 1025);
    let after = fs::metadata(target.join("nested/0/0.txt")).unwrap();
    assert_ne!(before.ino(), after.ino());
    assert_eq!(before.mode(), after.mode());
    assert_eq!(before.mtime(), after.mtime());
    assert_eq!(before.mtime_nsec(), after.mtime_nsec());
    assert_eq!(
        fs::metadata(target.join("nested/0/1.txt")).unwrap().mode() & 0o7777,
        0o640
    );
    assert_eq!(
        fs::metadata(target.join("nested/0/2.txt")).unwrap().mode() & 0o7777,
        0o755
    );
    for directory in 0..16 {
        for file in 0..64 {
            assert_eq!(
                fs::read_to_string(target.join(format!("nested/{directory}/{file}.txt"))).unwrap(),
                "shared contents\n"
            );
        }
    }
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

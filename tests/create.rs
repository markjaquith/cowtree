#![cfg(unix)]

use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    _root: tempfile::TempDir,
    repo: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "cowtree test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        fs::write(repo.join("same"), "unchanged payload\n").unwrap();
        fs::write(repo.join("changed"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "fixture"]);
        Self { _root: root, repo }
    }
    fn target(&self, name: &str) -> PathBuf {
        self._root.path().join(name)
    }
    fn cow(&self, args: &[&str]) -> Output {
        command(&self.repo, env!("CARGO_BIN_EXE_cowtree"))
            .args(args)
            .output()
            .unwrap()
    }
    fn add(&self, name: &str, args: &[&str]) -> Output {
        command(&self.repo, env!("CARGO_BIN_EXE_cowtree"))
            .args(["git", "worktree", "add"])
            .args(args)
            .arg(self.target(name))
            .output()
            .unwrap()
    }
}

fn command(cwd: &Path, executable: &str) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT");
    command
}
fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = command(cwd, "git").args(args).output().unwrap();
    success(&output);
    output.stdout
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn apfs(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        assert_eq!(unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) }, 0);
        unsafe { std::ffi::CStr::from_ptr(stat.assume_init().f_fstypename.as_ptr()) }.to_bytes()
            == b"apfs"
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        false
    }
}

#[test]
fn passthrough_is_byte_exact_and_works_outside_a_repository() {
    let f = Fixture::new();
    for args in [
        vec!["worktree", "list", "--porcelain", "-z"],
        vec!["worktree", "list", "--bad-option"],
        vec!["worktree", "add", "-h"],
        vec!["--version"],
    ] {
        let native = command(&f.repo, "git").args(&args).output().unwrap();
        let wrapped = command(&f.repo, env!("CARGO_BIN_EXE_cowtree"))
            .arg("git")
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(native.status.code(), wrapped.status.code());
        assert_eq!(native.stdout, wrapped.stdout);
        assert_eq!(native.stderr, wrapped.stderr);
    }
    success(
        &command(f._root.path(), env!("CARGO_BIN_EXE_cowtree"))
            .args(["git", "--version"])
            .output()
            .unwrap(),
    );
}

#[test]
fn no_checkout_and_orphan_keep_native_empty_semantics() {
    let f = Fixture::new();
    success(&f.add("empty", &["--no-checkout", "-b", "empty"]));
    assert_eq!(fs::read_dir(f.target("empty")).unwrap().count(), 1);
    success(&f.add("orphan", &["--orphan", "-b", "orphan"]));
    assert_eq!(fs::read_dir(f.target("orphan")).unwrap().count(), 1);
    assert!(git(&f.target("orphan"), &["ls-files"]).is_empty());
}

#[test]
fn clone_first_is_clean_and_independent_and_has_creation_status() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    success(&f.add("feature", &["-b", "feature"]));
    let target = f.target("feature");
    assert!(git(&target, &["status", "--porcelain"]).is_empty());
    assert_ne!(
        fs::metadata(f.repo.join("same")).unwrap().ino(),
        fs::metadata(target.join("same")).unwrap().ino()
    );
    fs::write(target.join("same"), "target edit").unwrap();
    assert_eq!(
        fs::read(f.repo.join("same")).unwrap(),
        b"unchanged payload\n"
    );
    fs::write(f.repo.join("changed"), "source edit").unwrap();
    assert_eq!(fs::read(target.join("changed")).unwrap(), b"before\n");
    let output = f.cow(&["status", target.to_str().unwrap(), "--json"]);
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"created\""));
}

#[test]
fn divergent_commit_dirty_source_modes_symlinks_and_weird_names() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    let weird = unusual_name("line\nspace-☃", b"line\nnonutf8-\xff");
    fs::write(f.repo.join(&weird), "weird\n").unwrap();
    fs::write(f.repo.join("exec"), "executable\n").unwrap();
    fs::set_permissions(f.repo.join("exec"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("same", f.repo.join("link")).unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "extra files"]);
    git(&f.repo, &["branch", "old"]);
    fs::write(f.repo.join("changed"), "after\n").unwrap();
    git(&f.repo, &["commit", "-qam", "divergence"]);
    // Hide a modification from status; source index flags cannot authorize it.
    git(&f.repo, &["update-index", "--assume-unchanged", "exec"]);
    fs::write(f.repo.join("exec"), "dirty hidden bytes\n").unwrap();
    let output = command(&f.repo, env!("CARGO_BIN_EXE_cowtree"))
        .args(["git", "worktree", "add", "--detach"])
        .arg(f.target("old"))
        .arg("old")
        .output()
        .unwrap();
    success(&output);
    let target = f.target("old");
    assert_eq!(fs::read(target.join("changed")).unwrap(), b"before\n");
    assert_eq!(fs::read(target.join("exec")).unwrap(), b"executable\n");
    assert_ne!(fs::metadata(target.join("exec")).unwrap().mode() & 0o111, 0);
    assert_eq!(
        fs::read_link(target.join("link")).unwrap(),
        Path::new("same")
    );
    assert_eq!(fs::read(target.join(&weird)).unwrap(), b"weird\n");
    assert!(git(&target, &["status", "--porcelain"]).is_empty());
}

#[test]
fn sparse_cone_and_noncone_match_git() {
    for cone in [true, false] {
        let f = Fixture::new();
        if !apfs(&f.repo) {
            return;
        }
        for dir in ["yes", "no"] {
            fs::create_dir(f.repo.join(dir)).unwrap();
            fs::write(f.repo.join(dir).join("file"), dir).unwrap();
        }
        git(&f.repo, &["add", "."]);
        git(&f.repo, &["commit", "-qm", "directories"]);
        if cone {
            git(
                &f.repo,
                &["sparse-checkout", "set", "--cone", "--sparse-index", "yes"],
            );
        } else {
            git(
                &f.repo,
                &["sparse-checkout", "set", "--no-cone", "/yes/", "/same"],
            );
        }
        success(&f.add("sparse", &["-b", "sparse"]));
        assert!(f.target("sparse").join("yes/file").exists());
        assert!(!f.target("sparse").join("no/file").exists());
        assert!(git(&f.target("sparse"), &["status", "--porcelain"]).is_empty());
        assert_eq!(
            git(&f.repo, &["ls-files", "-t"]),
            git(&f.target("sparse"), &["ls-files", "-t"])
        );
    }
}

#[test]
fn filters_run_and_hook_runs_once_at_the_target_with_native_arguments() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::write(f.repo.join(".gitattributes"), "changed filter=demo\n").unwrap();
    git(&f.repo, &["add", ".gitattributes"]);
    git(&f.repo, &["commit", "-qm", "attributes"]);
    git(
        &f.repo,
        &["config", "filter.demo.smudge", "sed 's/before/AFTER/'"],
    );
    git(
        &f.repo,
        &["config", "filter.demo.clean", "sed 's/AFTER/before/'"],
    );
    let hook = f.repo.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\" >> hook-record\n",
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    success(&f.add("filtered", &["-b", "filtered"]));
    let target = f.target("filtered");
    assert_eq!(fs::read(target.join("changed")).unwrap(), b"AFTER\n");
    let record = fs::read_to_string(target.join("hook-record")).unwrap();
    let lines: Vec<_> = record.lines().collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(Path::new(lines[0]), target.canonicalize().unwrap());
    assert_eq!(lines[1], "0".repeat(40));
    assert_eq!(
        lines[2],
        String::from_utf8(git(&target, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
    );
    assert_eq!(lines[3], "1");
}

#[test]
fn hook_failure_retains_worktree_and_propagates_exit_status() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    let hook = f.repo.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\necho hook > same\nexit 7\n").unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let output = f.add("failed-hook", &["-b", "failed-hook"]);
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        fs::read(f.target("failed-hook").join("same")).unwrap(),
        b"hook\n"
    );
    assert!(
        !f.repo
            .join(".git/worktrees/failed-hook/cowtree-creation")
            .exists()
    );
}

#[test]
fn no_eligible_source_fails_without_checkout_and_preserves_empty_directory() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::write(f.repo.join("same"), "dirty").unwrap();
    fs::write(f.repo.join("changed"), "dirty").unwrap();
    fs::create_dir(f.target("failed")).unwrap();
    fs::set_permissions(f.target("failed"), fs::Permissions::from_mode(0o700)).unwrap();
    let output = f.add("failed", &["-b", "failed"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no verified"));
    assert_eq!(fs::read_dir(f.target("failed")).unwrap().count(), 0);
    assert_eq!(
        fs::metadata(f.target("failed")).unwrap().mode() & 0o777,
        0o700
    );
    assert!(!f.repo.join(".git/worktrees/failed").exists());
}

#[test]
fn global_context_nonutf8_destination_quiet_and_lock() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::create_dir(f.repo.join("subdir")).unwrap();
    let target_name = unusual_name("destination-☃", b"destination-\xff");
    let output = command(f._root.path(), env!("CARGO_BIN_EXE_cowtree"))
        .args([
            OsStr::new("git"),
            OsStr::new("-C"),
            f.repo.as_os_str(),
            OsStr::new("-C"),
            OsStr::new("subdir"),
            OsStr::new("-c"),
            OsStr::new("core.hooksPath=/dev/null"),
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-q"),
            OsStr::new("--lock"),
            OsStr::new("--reason"),
            OsStr::new("testing"),
            OsStr::new("-b"),
            OsStr::new("global"),
        ])
        .arg(Path::new("../..").join(&target_name))
        .output()
        .unwrap();
    success(&output);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let target = f._root.path().join(target_name);
    assert!(target.join("same").exists());
    assert!(
        String::from_utf8_lossy(&git(&f.repo, &["worktree", "list", "--porcelain"]))
            .contains("locked testing")
    );
}

fn unusual_name(unicode: &str, bytes: &[u8]) -> OsString {
    if cfg!(target_os = "macos") {
        unicode.into()
    } else {
        OsString::from_vec(bytes.to_vec())
    }
}

#[test]
fn tracking_force_reset_and_passthrough_lifecycle() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    git(&f.repo, &["config", "remote.origin.url", "."]);
    git(
        &f.repo,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    let commit = String::from_utf8(git(&f.repo, &["rev-parse", "HEAD"])).unwrap();
    git(
        &f.repo,
        &[
            "update-ref",
            "refs/remotes/origin/remote-feature",
            commit.trim(),
        ],
    );
    // Git resolves the remote and creates the local tracking branch.
    let output = command(&f.repo, env!("CARGO_BIN_EXE_cowtree"))
        .args(["git", "worktree", "add"])
        .arg(f.target("tracked"))
        .arg("remote-feature")
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        git(&f.repo, &["config", "branch.remote-feature.remote"]),
        b"origin\n"
    );
    success(&f.cow(&[
        "git",
        "worktree",
        "move",
        f.target("tracked").to_str().unwrap(),
        f.target("moved").to_str().unwrap(),
    ]));
    success(&f.cow(&[
        "git",
        "worktree",
        "lock",
        "--reason",
        "lock",
        f.target("moved").to_str().unwrap(),
    ]));
    success(&f.cow(&[
        "git",
        "worktree",
        "unlock",
        f.target("moved").to_str().unwrap(),
    ]));
    success(&f.cow(&[
        "git",
        "worktree",
        "repair",
        f.target("moved").to_str().unwrap(),
    ]));
    success(&f.cow(&[
        "git",
        "worktree",
        "remove",
        f.target("moved").to_str().unwrap(),
    ]));
    success(&f.cow(&["git", "worktree", "prune", "--dry-run"]));
    // Abbreviated options and short option clusters must survive interception.
    success(&f.add("reset", &["--det", "--qui"]));
    success(&f.add("cluster", &["-qbcluster"]));
    success(&f.add("force", &["-qf", "-B", "remote-feature"]));
    assert_eq!(
        git(&f.target("force"), &["symbolic-ref", "--short", "HEAD"]),
        b"remote-feature\n"
    );
}

#[test]
fn checkout_representation_and_umask_match_native_git() {
    use std::os::unix::process::CommandExt;
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::write(
        f.repo.join(".gitattributes"),
        "same -text\nchanged text eol=crlf\nencoded working-tree-encoding=UTF-16LE\nident ident\n",
    )
    .unwrap();
    fs::write(f.repo.join("encoded"), [b'h', 0, b'i', 0, b'\n', 0]).unwrap();
    fs::write(f.repo.join("ident"), "$Id$\n").unwrap();
    fs::write(f.repo.join("exec"), "execute\n").unwrap();
    fs::set_permissions(f.repo.join("exec"), fs::Permissions::from_mode(0o755)).unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "representations"]);
    for cow in [false, true] {
        let mut cmd = command(
            &f.repo,
            if cow {
                env!("CARGO_BIN_EXE_cowtree")
            } else {
                "git"
            },
        );
        if cow {
            cmd.arg("git");
        }
        unsafe {
            cmd.pre_exec(|| {
                libc::umask(0o027);
                Ok(())
            });
        }
        let output = cmd
            .args(["-c", "core.autocrlf=true", "worktree", "add", "--detach"])
            .arg(f.target(if cow { "cow" } else { "native" }))
            .output()
            .unwrap();
        success(&output);
    }
    for name in [
        "same",
        "changed",
        "encoded",
        "ident",
        "exec",
        ".gitattributes",
    ] {
        assert_eq!(
            fs::read(f.target("cow").join(name)).unwrap(),
            fs::read(f.target("native").join(name)).unwrap(),
            "{name}"
        );
        assert_eq!(
            fs::metadata(f.target("cow").join(name)).unwrap().mode() & 0o7777,
            fs::metadata(f.target("native").join(name)).unwrap().mode() & 0o7777,
            "{name}"
        );
    }
    assert_eq!(
        git(&f.target("cow"), &["ls-files", "--stage", "-z"]),
        git(&f.target("native"), &["ls-files", "--stage", "-z"])
    );
}

#[test]
fn failed_filter_retains_recovery_state_and_never_runs_hook() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::write(f.repo.join(".gitattributes"), "changed filter=broken\n").unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "filter"]);
    git(
        &f.repo,
        &[
            "config",
            "filter.broken.smudge",
            "echo external > external-file; exit 1",
        ],
    );
    git(&f.repo, &["config", "filter.broken.required", "true"]);
    let output = f.add("broken", &["-b", "broken"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("incomplete worktree retained"));
    assert!(f.target("broken").join("external-file").exists());
    assert!(f.repo.join(".git/worktrees/broken/locked").exists());
    assert!(
        !f.repo
            .join(".git/worktrees/broken/cowtree-creation")
            .exists()
    );
}

#[test]
fn bare_repository_uses_detached_linked_source_and_sha256() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    // A SHA-256 repository exercises both hashing and hook null-OID length.
    let sha = f.target("sha");
    fs::create_dir(&sha).unwrap();
    let init = command(&sha, "git")
        .args([
            "init",
            "-q",
            "--object-format=sha256",
            "--initial-branch=main",
        ])
        .output()
        .unwrap();
    if !init.status.success() {
        return;
    }
    git(&sha, &["config", "user.name", "test"]);
    git(&sha, &["config", "user.email", "test@example.com"]);
    fs::write(sha.join("file"), "sha256\n").unwrap();
    git(&sha, &["add", "."]);
    git(&sha, &["commit", "-qm", "sha"]);
    let output = command(&sha, env!("CARGO_BIN_EXE_cowtree"))
        .args(["git", "worktree", "add", "--detach"])
        .arg(f.target("sha-linked"))
        .output()
        .unwrap();
    success(&output);
    let bare = f.target("bare.git");
    success(
        &command(&f.repo, "git")
            .args(["clone", "--bare"])
            .arg(&f.repo)
            .arg(&bare)
            .output()
            .unwrap(),
    );
    // The fixture intentionally materializes a donor using native Git.
    success(
        &command(&bare, "git")
            .args(["worktree", "add", "--detach"])
            .arg(f.target("donor"))
            .output()
            .unwrap(),
    );
    let output = command(&bare, env!("CARGO_BIN_EXE_cowtree"))
        .args(["git", "worktree", "add", "--detach"])
        .arg(f.target("bare-target"))
        .output()
        .unwrap();
    success(&output);
    assert!(git(&f.target("bare-target"), &["status", "--porcelain"]).is_empty());
}

#[test]
fn empty_commit_and_implicit_orphan_need_no_donor() {
    let f = Fixture::new();
    let repo = f.target("empty-repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "--initial-branch=main"]);
    let output = command(&repo, env!("CARGO_BIN_EXE_cowtree"))
        .args(["git", "worktree", "add"])
        .arg(f.target("implicit"))
        .output()
        .unwrap();
    success(&output);
    assert_eq!(fs::read_dir(f.target("implicit")).unwrap().count(), 1);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["commit", "--allow-empty", "-qm", "empty"]);
    let output = command(&repo, env!("CARGO_BIN_EXE_cowtree"))
        .args(["git", "worktree", "add", "--detach"])
        .arg(f.target("empty-commit"))
        .output()
        .unwrap();
    success(&output);
    assert_eq!(fs::read_dir(f.target("empty-commit")).unwrap().count(), 1);
}

#[test]
fn interruption_during_filter_returns_signal_status_and_retains_recovery() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::write(f.repo.join(".gitattributes"), "changed filter=slow\n").unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "filter"]);
    git(
        &f.repo,
        &[
            "config",
            "filter.slow.smudge",
            "echo ready > ready; sleep 1; cat",
        ],
    );
    let child = command(&f.repo, env!("CARGO_BIN_EXE_cowtree"))
        .args(["git", "worktree", "add", "-b", "interrupted"])
        .arg(f.target("interrupted"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !f.target("interrupted").join("ready").exists() {
        assert!(std::time::Instant::now() < deadline, "filter never started");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143));
    assert!(String::from_utf8_lossy(&output.stderr).contains("incomplete worktree retained"));
    assert!(f.target("interrupted").join("ready").exists());
    assert!(
        !f.repo
            .join(".git/worktrees/interrupted/cowtree-creation")
            .exists()
    );
}

#[test]
fn seed_interference_during_checkout_is_detected() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::write(
        f.repo.join(".gitattributes"),
        "changed filter=interference\n",
    )
    .unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "filter"]);
    git(
        &f.repo,
        &[
            "config",
            "filter.interference.smudge",
            "echo external > same; cat",
        ],
    );
    let output = f.add("interference", &["-b", "interference"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("incomplete worktree retained"));
    assert_eq!(
        fs::read(f.target("interference").join("same")).unwrap(),
        b"external\n"
    );
    assert_eq!(
        fs::read(f.repo.join("same")).unwrap(),
        b"unchanged payload\n"
    );
}

#[test]
fn git_environment_selectors_do_not_modify_the_caller_index() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    let original_index = fs::read(f.repo.join(".git/index")).unwrap();
    let output = command(f._root.path(), env!("CARGO_BIN_EXE_cowtree"))
        .env("GIT_DIR", f.repo.join(".git"))
        .env("GIT_WORK_TREE", &f.repo)
        .env("GIT_INDEX_FILE", f.repo.join(".git/index"))
        .args(["git", "worktree", "add", "-b", "env"])
        .arg(f.target("env"))
        .output()
        .unwrap();
    success(&output);
    assert_eq!(fs::read(f.repo.join(".git/index")).unwrap(), original_index);
    assert!(git(&f.target("env"), &["status", "--porcelain"]).is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn user_extended_attributes_are_not_copied() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    success(
        &command(&f.repo, "xattr")
            .args(["-w", "user.cowtree-test", "donor-only", "same"])
            .output()
            .unwrap(),
    );
    success(&f.add("xattrs", &["-b", "xattrs"]));
    assert_eq!(
        fs::read(f.target("xattrs").join("same")).unwrap(),
        b"unchanged payload\n"
    );
    let output = command(&f.target("xattrs"), "xattr")
        .args(["-p", "user.cowtree-test", "same"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn relative_hooks_and_global_checkout_overrides_match_native() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    let hooks = f.repo.join("custom-hooks");
    fs::create_dir(&hooks).unwrap();
    fs::create_dir(f.repo.join("nested")).unwrap();
    let hook = hooks.join("post-checkout");
    // Deliberately no shebang: Git supports executable shell scripts too.
    fs::write(&hook, "printf '%s\\n' \"${GIT_DIR-unset}\" \"${GIT_WORK_TREE-unset}\" \"${GIT_PREFIX-unset}\" > hook-env\ngit config --get cowtree.test >> hook-env\ngit config --get cowtree.env >> hook-env\necho hook-output\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let mut outputs = Vec::new();
    for cow in [false, true] {
        let mut cmd = command(
            &f.repo.join("nested"),
            if cow {
                env!("CARGO_BIN_EXE_cowtree")
            } else {
                "git"
            },
        );
        if cow {
            cmd.arg("git");
        }
        let output = cmd
            .env("COWTREE_TEST_VALUE", "environment value")
            .args([
                "-c",
                "core.hooksPath=custom-hooks",
                "-c",
                "cowtree.test=quoted ' value",
                "--config-env=cowtree.env=COWTREE_TEST_VALUE",
                "-c",
            ])
            .arg(format!("core.worktree={}", f.repo.display()))
            .args(["worktree", "add", "--detach"])
            .arg(f.target(if cow { "cow-hook" } else { "native-hook" }))
            .output()
            .unwrap();
        success(&output);
        outputs.push(output);
    }
    assert_eq!(
        fs::read(f.target("cow-hook").join("hook-env")).unwrap(),
        fs::read(f.target("native-hook").join("hook-env")).unwrap()
    );
    assert!(String::from_utf8_lossy(&outputs[1].stderr).contains("hook-output"));
    assert!(!String::from_utf8_lossy(&outputs[1].stdout).contains("hook-output"));
}

#[test]
fn file_directory_transitions_and_gitlinks_match_native_checkout() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::create_dir(f.repo.join("directory")).unwrap();
    fs::write(f.repo.join("directory/file"), "old\n").unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "directory"]);
    let oid = String::from_utf8(git(&f.repo, &["rev-parse", "HEAD"])).unwrap();
    git(
        &f.repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},module", oid.trim()),
        ],
    );
    git(&f.repo, &["commit", "-qm", "gitlink"]);
    git(&f.repo, &["branch", "with-directory"]);
    fs::remove_file(f.repo.join("directory/file")).unwrap();
    fs::remove_dir(f.repo.join("directory")).unwrap();
    fs::write(f.repo.join("directory"), "new file\n").unwrap();
    git(&f.repo, &["add", "directory"]);
    git(&f.repo, &["commit", "-qm", "transition"]);
    for cow in [false, true] {
        let mut cmd = command(
            &f.repo,
            if cow {
                env!("CARGO_BIN_EXE_cowtree")
            } else {
                "git"
            },
        );
        if cow {
            cmd.arg("git");
        }
        let target = f.target(if cow {
            "cow-transition"
        } else {
            "native-transition"
        });
        success(
            &cmd.args(["worktree", "add", "--detach"])
                .arg(&target)
                .arg("with-directory")
                .output()
                .unwrap(),
        );
        assert_eq!(fs::read(target.join("directory/file")).unwrap(), b"old\n");
        assert!(target.join("module").is_dir());
        assert_eq!(fs::read_dir(target.join("module")).unwrap().count(), 0);
        assert!(git(&target, &["status", "--porcelain"]).is_empty());
    }
}

#[test]
fn creation_receipt_survives_donor_removal_and_becomes_stale_on_commit_change() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    success(&f.add("donor", &["-b", "donor"]));
    // Make the main tree unusable as a donor, so the linked donor is selected.
    fs::write(f.repo.join("same"), "dirty").unwrap();
    fs::write(f.repo.join("changed"), "dirty").unwrap();
    success(&f.add("receipt", &["-b", "receipt"]));
    success(&f.cow(&[
        "git",
        "worktree",
        "remove",
        f.target("donor").to_str().unwrap(),
    ]));
    git(&f.repo, &["branch", "-D", "donor"]);
    let target = f.target("receipt");
    let status = f.cow(&["status", target.to_str().unwrap(), "--json"]);
    success(&status);
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"created\""));
    fs::write(target.join("same"), "new target\n").unwrap();
    git(&target, &["commit", "-qam", "changed target"]);
    let status = f.cow(&["status", target.to_str().unwrap(), "--json"]);
    success(&status);
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"stale\""));
}

#[test]
fn non_apfs_creation_fails_without_payload_fallback() {
    let f = Fixture::new();
    if apfs(&f.repo) {
        return;
    }
    let output = f.add("unsupported", &["-b", "unsupported"]);
    assert!(!output.status.success());
    assert!(!f.target("unsupported").exists());
    assert!(!f.repo.join(".git/worktrees/unsupported").exists());
}

#[test]
fn empty_attribute_values_match_native_checkout() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::write(f.repo.join(".gitattributes"), "changed filter=\n").unwrap();
    git(&f.repo, &["add", ".gitattributes"]);
    git(&f.repo, &["commit", "-qm", "empty attribute"]);
    for cow in [false, true] {
        let mut cmd = command(
            &f.repo,
            if cow {
                env!("CARGO_BIN_EXE_cowtree")
            } else {
                "git"
            },
        );
        if cow {
            cmd.arg("git");
        }
        let target = f.target(if cow {
            "cow-attribute"
        } else {
            "native-attribute"
        });
        success(
            &cmd.args(["worktree", "add", "--detach"])
                .arg(&target)
                .output()
                .unwrap(),
        );
        assert!(git(&target, &["status", "--porcelain"]).is_empty());
        assert_eq!(fs::read(target.join("changed")).unwrap(), b"before\n");
    }
}

#[test]
fn global_pathspec_options_reach_checkout_hooks() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    fs::write(f.repo.join("glob*.txt"), "literal filename").unwrap();
    fs::write(f.repo.join("glob-other.txt"), "wildcard match").unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-qm", "pathspec fixture"]);
    let hook = f.repo.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nprintf '%s:%s:%s:%s\\n' \"$GIT_LITERAL_PATHSPECS\" \"$GIT_GLOB_PATHSPECS\" \"$GIT_NOGLOB_PATHSPECS\" \"$GIT_ICASE_PATHSPECS\" > hook-result\ngit ls-files -- 'glob*.txt' >> hook-result\n").unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    for (n, option) in [
        "--literal-pathspecs",
        "--glob-pathspecs",
        "--noglob-pathspecs",
        "--icase-pathspecs",
    ]
    .iter()
    .enumerate()
    {
        let mut results = Vec::new();
        for cow in [false, true] {
            let mut cmd = command(
                &f.repo,
                if cow {
                    env!("CARGO_BIN_EXE_cowtree")
                } else {
                    "git"
                },
            );
            if cow {
                cmd.arg("git");
            }
            let target = f.target(&format!("pathspec-{n}-{cow}"));
            success(
                &cmd.args([option, "worktree", "add", "--detach"])
                    .arg(&target)
                    .output()
                    .unwrap(),
            );
            results.push(fs::read(target.join("hook-result")).unwrap());
        }
        assert_eq!(results[0], results[1], "{option}");
    }
}

#[test]
fn receipt_failure_does_not_fail_a_completed_checkout() {
    let f = Fixture::new();
    if !apfs(&f.repo) {
        return;
    }
    let hook = f.repo.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\nmkdir \"$(git rev-parse --git-path cowtree-creation)\"\n",
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let output = f.add("receipt-error", &["-b", "receipt-error"]);
    success(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("receipt"));
    assert!(git(&f.target("receipt-error"), &["status", "--porcelain"]).is_empty());
    assert_eq!(
        fs::read(f.target("receipt-error").join("same")).unwrap(),
        b"unchanged payload\n"
    );
    assert!(
        f.repo
            .join(".git/worktrees/receipt-error/cowtree-creation")
            .is_dir()
    );
}

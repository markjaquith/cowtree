//! Parse `cowtree add` arguments and delegate worktree registration to Git.
use std::{
    ffi::{CString, OsString},
    io::Write,
    os::{
        fd::AsFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

use crate::{
    create,
    error::{Error, Result},
};

#[derive(Debug)]
pub struct Add {
    pub path: PathBuf,
    pub checkout: bool,
    pub orphan: bool,
    pub locked: bool,
    pub quiet: bool,
    pub explicit_target: bool,
    pub detach: bool,
    pub end_of_options: Option<usize>,
}

pub struct Git;

impl Git {
    pub fn original(&self) -> Command {
        Command::new("git")
    }

    pub fn at(&self, path: &Path) -> Command {
        let mut command = Command::new("git");
        // These locate the caller's repository, not the newly registered tree.
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_PREFIX",
        ] {
            command.env_remove(key);
        }
        command.arg("-C").arg(path);
        // Match native add's explicit child checkout context. In particular,
        // shared or command-line core.worktree must not redirect index writes.
        command
            .env("GIT_DIR", path.join(".git"))
            .env("GIT_WORK_TREE", path);
        command
    }

    pub fn bytes(&self, path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        capture(self.at(path).args(args), input)
    }

    pub fn text(&self, path: &Path, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes(path, args, None)?)
            .trim()
            .to_owned())
    }
}

/// Native worktree add resolves its hook in the invoking repository, then runs
/// that hook in the new worktree with GIT_DIR/GIT_WORK_TREE removed. `git hook
/// run` in the target cannot reproduce relative core.hooksPath or a hook changed
/// by the target commit, so preserve this small, explicit launch contract here.
pub fn post_checkout(git: &Git, target: &Path, commit: &str) -> Result<i32> {
    let configured = git
        .original()
        .args(["config", "--path", "--get", "core.hooksPath"])
        .output()?;
    let mut hook = if configured.status.success() {
        let mut directory = configured.stdout;
        if directory.last() == Some(&b'\n') {
            directory.pop();
        }
        let mut directory = PathBuf::from(OsString::from_vec(directory));
        if !directory.is_absolute() {
            let root = git
                .original()
                .args(["rev-parse", "--show-toplevel"])
                .output()?;
            let base = if root.status.success() {
                let mut bytes = root.stdout;
                if bytes.last() == Some(&b'\n') {
                    bytes.pop();
                }
                PathBuf::from(OsString::from_vec(bytes))
            } else {
                std::env::current_dir()?
            };
            directory = base.join(directory);
        }
        directory.join("post-checkout").into_os_string().into_vec()
    } else if configured.status.code() == Some(1) {
        capture(
            git.original().args([
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "hooks/post-checkout",
            ]),
            None,
        )?
    } else {
        return Err(Error::GitProcess {
            code: exit_code(configured.status),
            message: String::from_utf8_lossy(&configured.stderr).into_owned(),
        });
    };
    if hook.last() == Some(&b'\n') {
        hook.pop();
    }
    let cpath =
        CString::new(hook.clone()).map_err(|_| Error::Message("invalid hook path".into()))?;
    if unsafe { libc::access(cpath.as_ptr(), libc::X_OK) } != 0 {
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied {
            let advice = git
                .original()
                .args(["config", "--type=bool", "--get", "advice.ignoredHook"])
                .output()?;
            if advice.stdout != b"false\n" {
                eprintln!(
                    "hint: The '{}' hook was ignored because it's not set as executable.",
                    String::from_utf8_lossy(&hook)
                );
            }
        }
        return Ok(0);
    }
    let hook = PathBuf::from(OsString::from_vec(hook));
    let null = "0".repeat(commit.len());
    let configure = |command: &mut Command| -> Result<()> {
        command
            .current_dir(target)
            .args([&null, commit, "1"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::null())
            .stdout(Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?));
        hook_environment(git, command)?;
        Ok(())
    };
    let mut command = Command::new(&hook);
    configure(&mut command)?;
    let status = match command.status() {
        Ok(status) => status,
        Err(error) if error.raw_os_error() == Some(libc::ENOEXEC) => {
            // Git retries executable scripts without a shebang through sh.
            let mut shell = Command::new("/bin/sh");
            shell.arg(&hook);
            configure(&mut shell)?;
            shell.status()?
        }
        Err(error) => return Err(error.into()),
    };
    Ok(exit_code(status))
}

fn hook_environment(git: &Git, command: &mut Command) -> Result<()> {
    let mut prefix = capture(git.original().args(["rev-parse", "--show-prefix"]), None)?;
    if prefix.last() == Some(&b'\n') {
        prefix.pop();
    }
    command.env("GIT_PREFIX", OsString::from_vec(prefix));
    Ok(())
}

pub fn capture(command: &mut Command, input: Option<&[u8]>) -> Result<Vec<u8>> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command.spawn()?;
    // Drain output concurrently with stdin: check-attr/check-rules can fill a
    // pipe before consuming a large inventory.
    let output = std::thread::scope(|scope| {
        let writer = input.map(|input| {
            let mut stdin = child.stdin.take().unwrap();
            scope.spawn(move || stdin.write_all(input))
        });
        let output = child.wait_with_output();
        let written = writer.map(|writer| writer.join().expect("Git stdin writer panicked"));
        (output, written)
    });
    let result = output.0?;
    if !result.status.success() {
        return Err(Error::GitProcess {
            code: exit_code(result.status),
            message: String::from_utf8_lossy(&result.stderr).trim().to_owned(),
        });
    }
    if let Some(written) = output.1 {
        written?;
    }
    if !result.stderr.is_empty() {
        std::io::stderr().write_all(&result.stderr)?;
    }
    Ok(result.stdout)
}

pub fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

fn native_add(args: &[OsString]) -> Result<i32> {
    Err(Command::new("git")
        .args(["worktree", "add"])
        .args(args)
        .exec()
        .into())
}

pub fn run(args: Vec<OsString>) -> Result<i32> {
    let Some(add) = parse_add(&args)? else {
        return native_add(&args);
    };
    if !add.checkout || add.orphan {
        return native_add(&args);
    }
    let git = Git;
    // Git infers an unborn branch when no refs exist. Let Git handle that case,
    // including its remote/force checks, without injecting --no-checkout.
    if !add.explicit_target && !add.detach {
        let refs = capture(
            git.original()
                .args(["for-each-ref", "--count=1", "--format=%(objectname)"]),
            None,
        )?;
        let head = git
            .original()
            .args(["rev-parse", "--verify", "HEAD^{commit}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if refs.is_empty() && !head.success() {
            return native_add(&args);
        }
    }
    create::run(&git, &args, &add)
}

fn parse_add(args: &[OsString]) -> Result<Option<Add>> {
    let mut add = Add {
        path: PathBuf::new(),
        checkout: true,
        orphan: false,
        locked: false,
        quiet: false,
        explicit_target: false,
        detach: false,
        end_of_options: None,
    };
    let mut positional = Vec::new();
    let mut reason = false;
    let mut end = false;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let normalized;
        let b = if !end && arg.as_bytes().starts_with(b"--") && arg != "--" {
            normalized = normalize_long(arg.as_bytes())?;
            normalized.as_slice()
        } else {
            arg.as_bytes()
        };
        if end || !b.starts_with(b"-") || b == b"-" {
            positional.push(arg);
        } else if b == b"--" {
            end = true;
            add.end_of_options = Some(i);
        } else if matches!(b, b"-h" | b"--help") {
            return Ok(None);
        } else if matches!(b, b"-b" | b"-B" | b"--reason") {
            reason |= b == b"--reason";
            i += 1;
            if args.get(i).is_none() {
                return Ok(None);
            }
        } else if b.starts_with(b"--reason=") {
            reason = true;
        } else if b.starts_with(b"--track=")
            || ((b.starts_with(b"-b") || b.starts_with(b"-B")) && b.len() > 2)
        {
        } else {
            match b {
                b"--checkout" => add.checkout = true,
                b"--no-checkout" => add.checkout = false,
                b"--orphan" => add.orphan = true,
                b"--no-orphan" => add.orphan = false,
                b"--lock" => add.locked = true,
                b"--no-lock" => add.locked = false,
                b"--quiet" => add.quiet = true,
                b"--no-quiet" => add.quiet = false,
                b"--detach" => add.detach = true,
                b"--no-detach" => add.detach = false,
                b"--no-reason" => reason = false,
                b"--force" | b"--no-force" | b"--track" | b"--no-track" | b"--guess-remote"
                | b"--no-guess-remote" => {}
                _ if !b.starts_with(b"--") => {
                    for (offset, option) in b[1..].iter().enumerate() {
                        match option {
                            b'f' => {}
                            b'q' => add.quiet = true,
                            b'd' => add.detach = true,
                            b'b' | b'B' => {
                                if offset + 2 == b.len() {
                                    i += 1;
                                    if args.get(i).is_none() {
                                        return Ok(None);
                                    }
                                }
                                break;
                            }
                            _ => {
                                return Err(Error::Message(format!(
                                    "unsupported worktree add option: {}",
                                    arg.to_string_lossy()
                                )));
                            }
                        }
                    }
                }
                _ => {
                    return Err(Error::Message(format!(
                        "unsupported worktree add option: {}; cannot safely transform creation",
                        arg.to_string_lossy()
                    )));
                }
            }
        }
        i += 1;
    }
    // Delegate invalid arity and lock syntax for Git's own diagnostics.
    if positional.is_empty() || positional.len() > 2 || (reason && !add.locked) {
        return Ok(None);
    }
    add.path = PathBuf::from(positional[0]);
    add.explicit_target = positional.len() == 2;
    Ok(Some(add))
}

fn normalize_long(raw: &[u8]) -> Result<Vec<u8>> {
    let split = raw.iter().position(|b| *b == b'=').unwrap_or(raw.len());
    let name = &raw[..split];
    const OPTIONS: &[&[u8]] = &[
        b"--help",
        b"--checkout",
        b"--no-checkout",
        b"--orphan",
        b"--no-orphan",
        b"--lock",
        b"--no-lock",
        b"--reason",
        b"--no-reason",
        b"--quiet",
        b"--no-quiet",
        b"--detach",
        b"--no-detach",
        b"--force",
        b"--no-force",
        b"--track",
        b"--no-track",
        b"--guess-remote",
        b"--no-guess-remote",
    ];
    let option = if let Some(exact) = OPTIONS.iter().find(|option| **option == name) {
        *exact
    } else {
        let candidates: Vec<_> = OPTIONS
            .iter()
            .filter(|option| option.starts_with(name))
            .collect();
        if candidates.len() != 1 {
            return Err(Error::Message(format!(
                "unknown or ambiguous worktree add option: {}",
                String::from_utf8_lossy(raw)
            )));
        }
        *candidates[0]
    };
    let mut normalized = option.to_vec();
    normalized.extend_from_slice(&raw[split..]);
    Ok(normalized)
}

pub fn registration_args(args: &[OsString], add: &Add) -> Vec<OsString> {
    let mut result = args.to_vec();
    // Insert before the end-of-options marker, after the user's options, so the
    // internal checkout/lock settings win without disturbing positional args.
    let i = add.end_of_options.unwrap_or(result.len());
    result.splice(
        i..i,
        [OsString::from("--no-checkout"), OsString::from("--lock")],
    );
    if !add.locked {
        let at = i + 2;
        result.splice(
            at..at,
            [
                OsString::from("--reason"),
                OsString::from("cowtree: initializing"),
            ],
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }
    #[test]
    fn operands_are_not_options_or_commands() {
        let add = parse_add(&args(&["--reason", "--", "--lock", "--", "-target"]))
            .unwrap()
            .unwrap();
        assert_eq!(add.path, Path::new("-target"));
        assert_eq!(
            registration_args(&args(&["--reason", "--", "--lock", "--", "-target"]), &add),
            args(&[
                "--reason",
                "--",
                "--lock",
                "--no-checkout",
                "--lock",
                "--",
                "-target"
            ])
        );
    }
    #[test]
    fn last_boolean_wins_and_short_clusters_work() {
        let add = parse_add(&args(&["-qfd", "--no-checkout", "--checkout", "path"]))
            .unwrap()
            .unwrap();
        assert!(add.quiet && add.detach && add.checkout);
        assert!(parse_add(&args(&["--future-option", "path"])).is_err());
    }
}

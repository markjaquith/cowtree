//! Register first, seed only proven checkout bytes, and let Git write the rest.
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use sha2::Digest;

use crate::{
    add::{self, Add, Git},
    eligibility::validate_relative,
    error::{Error, Result},
    git::nul_paths,
    platform::{self, CloneOutcome, ClonePlatform, SystemPlatform},
    receipt, worktree,
};

#[derive(Clone, Debug)]
struct Entry {
    path: PathBuf,
    oid: String,
    executable: bool,
    regular: bool,
    gitlink: bool,
}

type Identity = (u64, u64, u64, i64, i64, i64, i64);

struct Timings {
    enabled: bool,
    started: Instant,
    previous: Instant,
}

impl Timings {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            enabled: std::env::var_os("COWTREE_TIMING").is_some_and(|value| value != "0"),
            started: now,
            previous: now,
        }
    }

    fn mark(&mut self, label: &str) {
        let now = Instant::now();
        if self.enabled {
            eprintln!(
                "cowtree timing: {label}: {:.3} ms (total {:.3} ms)",
                now.duration_since(self.previous).as_secs_f64() * 1000.0,
                now.duration_since(self.started).as_secs_f64() * 1000.0,
            );
        }
        self.previous = now;
    }

    fn worker_time(&self, label: &str, duration: Duration) {
        if self.enabled {
            eprintln!(
                "cowtree timing: {label}: {:.3} ms aggregate worker time",
                duration.as_secs_f64() * 1000.0,
            );
        }
    }
}

fn identity(path: &Path) -> Result<Identity> {
    let m = fs::symlink_metadata(path)?;
    Ok((
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}

struct Creation<'a> {
    git: &'a Git,
    path: PathBuf,
    admin: PathBuf,
    owned: HashMap<PathBuf, Identity>,
    directories: HashMap<PathBuf, (u64, u64)>,
    original_directory: Option<fs::Permissions>,
    materializing: bool,
    complete: bool,
    cancelled: Arc<AtomicUsize>,
}

impl Creation<'_> {
    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) != 0 {
            return Err(Error::Message("worktree creation interrupted".into()));
        }
        Ok(())
    }

    fn verify_authority(&self) -> Result<()> {
        let meta = fs::symlink_metadata(&self.path)?;
        if self.directories.get(Path::new("")) != Some(&(meta.dev(), meta.ino()))
            || self.owned.get(Path::new(".git")) != Some(&identity(&self.path.join(".git"))?)
        {
            return Err(Error::Message(
                "worktree registration changed during creation".into(),
            ));
        }
        Ok(())
    }

    fn remember(&mut self, path: &Path) -> Result<()> {
        self.owned
            .insert(path.to_owned(), identity(&self.path.join(path))?);
        Ok(())
    }

    fn parents(&mut self, relative: &Path) -> Result<()> {
        let mut parent = PathBuf::new();
        for component in relative.parent().unwrap_or(Path::new("")).components() {
            parent.push(component);
            let full = self.path.join(&parent);
            if let Some(expected) = self.directories.get(&parent) {
                let meta = fs::symlink_metadata(&full)?;
                if !meta.is_dir() || *expected != (meta.dev(), meta.ino()) {
                    return Err(Error::UnsafePath(parent));
                }
            } else {
                fs::create_dir(&full)?;
                let meta = fs::symlink_metadata(&full)?;
                self.directories
                    .insert(parent.clone(), (meta.dev(), meta.ino()));
            }
        }
        Ok(())
    }

    fn cleanup(&self) {
        if self.complete {
            return;
        }
        // A failed filter can create arbitrary files. Preserve those and any
        // externally changed state, rather than recursively deleting it.
        let safe = !self.materializing && self.only_owned(&self.path).unwrap_or(false);
        if safe {
            let removed = self
                .git
                .at(&self.path)
                .args(["worktree", "remove", "--force", "--force"])
                .arg(&self.path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if removed.is_ok_and(|status| status.success()) {
                if let Some(permissions) = &self.original_directory
                    && let Err(error) = fs::create_dir(&self.path)
                        .and_then(|()| fs::set_permissions(&self.path, permissions.clone()))
                {
                    eprintln!(
                        "cowtree: could not restore the original empty directory {}: {error}",
                        self.path.display()
                    );
                }
                return;
            }
        }
        eprintln!(
            "cowtree: incomplete worktree retained at {}; inspect it, then use git worktree unlock/remove to recover (admin: {})",
            self.path.display(),
            self.admin.display()
        );
    }

    fn only_owned(&self, directory: &Path) -> Result<bool> {
        let meta = fs::symlink_metadata(directory)?;
        if !meta.is_dir()
            || self
                .directories
                .get(directory.strip_prefix(&self.path).unwrap())
                != Some(&(meta.dev(), meta.ino()))
        {
            return Ok(false);
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if !self.only_owned(&path)? {
                    return Ok(false);
                }
            } else {
                let relative = path.strip_prefix(&self.path).unwrap();
                if self.owned.get(relative) != Some(&identity(&path)?) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

pub fn run(git: &Git, args: &[OsString], add: &Add) -> Result<i32> {
    let mut timings = Timings::new();
    let cwd = std::env::current_dir()?.canonicalize()?;
    let path = if add.path.is_absolute() {
        add.path.clone()
    } else {
        cwd.join(&add.path)
    };
    let original_directory = fs::metadata(&path)
        .ok()
        .filter(|meta| meta.is_dir())
        .map(|meta| meta.permissions());
    let sources_raw = add::capture(
        git.original()
            .args(["worktree", "list", "--porcelain", "-z"]),
        None,
    )?;
    let mut sources = worktree::parse_porcelain(&sources_raw)?;
    timings.mark("discover worktrees");
    // Capture umask before starting worker threads. The clone receives the same
    // permission mask as an ordinary checkout's open(O_CREAT).
    let mask = unsafe {
        let value = libc::umask(0);
        libc::umask(value);
        value as u32
    };
    let cancelled = Arc::new(AtomicUsize::new(0));
    let mut signals = Vec::new();
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        match signal_hook::flag::register_usize(signal, Arc::clone(&cancelled), signal as usize) {
            Ok(id) => signals.push(id),
            Err(error) => {
                for id in signals {
                    signal_hook::low_level::unregister(id);
                }
                return Err(error.into());
            }
        }
    }
    let result = (|| {
        let status = git
            .original()
            .args(["worktree", "add"])
            .args(add::registration_args(args, add))
            .status()?;
        if !status.success() {
            return Ok(add::exit_code(status));
        }
        timings.mark("register worktree");
        let path = path.canonicalize()?;
        let admin = path_output(git.bytes(&path, &["rev-parse", "--absolute-git-dir"], None)?)?;
        let meta = fs::symlink_metadata(&path)?;
        let mut creation = Creation {
            git,
            path,
            admin,
            owned: HashMap::new(),
            directories: HashMap::from([(PathBuf::new(), (meta.dev(), meta.ino()))]),
            original_directory,
            materializing: false,
            complete: false,
            cancelled: Arc::clone(&cancelled),
        };
        let result = creation.remember(Path::new(".git")).and_then(|()| {
            timings.mark("initialize creation");
            populate(&mut creation, add, &mut sources, &cwd, mask, &mut timings)
        });
        if result.is_err() {
            creation.cleanup();
        }
        result
    })();
    for registration in signals {
        signal_hook::low_level::unregister(registration);
    }
    let signal = cancelled.load(Ordering::Relaxed);
    if signal != 0 {
        Ok(128 + signal as i32)
    } else {
        result
    }
}

fn populate(
    creation: &mut Creation<'_>,
    add: &Add,
    sources: &mut [worktree::Worktree],
    cwd: &Path,
    mask: u32,
    timings: &mut Timings,
) -> Result<i32> {
    creation.check_cancelled()?;
    creation.verify_authority()?;
    let git = creation.git;
    let path = creation.path.clone();
    let commit = git.text(&path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    git.bytes(&path, &["read-tree", "--reset", &commit], None)?;
    let entries = tree(git.bytes(&path, &["ls-tree", "-r", "-z", &commit], None)?)?;
    let sparse = config_bool(git, &path, "core.sparseCheckout")?;
    let included: HashSet<PathBuf> = if sparse {
        let input = paths(entries.iter().map(|entry| entry.path.as_path()));
        let matched = git.bytes(
            &path,
            &["sparse-checkout", "check-rules", "-z"],
            Some(&input),
        )?;
        nul_paths(&matched)
            .map(|raw| PathBuf::from(OsString::from_vec(raw.to_vec())))
            .collect()
    } else {
        entries.iter().map(|entry| entry.path.clone()).collect()
    };
    let omitted = paths(
        entries
            .iter()
            .filter(|entry| !included.contains(&entry.path))
            .map(|entry| entry.path.as_path()),
    );
    if !omitted.is_empty() {
        git.bytes(
            &path,
            &["update-index", "--skip-worktree", "-z", "--stdin"],
            Some(&omitted),
        )?;
    }
    timings.mark("prepare target index");
    let wanted: Vec<_> = entries
        .iter()
        .filter(|entry| included.contains(&entry.path))
        .collect();
    if !wanted.is_empty() {
        SystemPlatform.validate(&path, &path)?;
    }
    let safe_attributes = checkout_safe(git, &path, &wanted)?;
    timings.mark("check checkout attributes");
    // Exact commit first, then the invoking worktree, then deterministic paths.
    sources.sort_by_key(|source| {
        (
            source.head != commit,
            !cwd.starts_with(&source.path),
            source.path.clone(),
        )
    });
    let mut inventories = Vec::new();
    for source in sources.iter() {
        if source.path == path || SystemPlatform.validate(&source.path, &path).is_err() {
            continue;
        }
        let Ok(raw) = git.bytes(&source.path, &["ls-tree", "-r", "-z", &source.head], None) else {
            continue;
        };
        let entries = tree(raw)?
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<HashMap<_, _>>();
        inventories.push((source, entries));
    }
    timings.mark("inventory donor trees");
    let mut jobs = Vec::new();
    for entry in &wanted {
        creation.check_cancelled()?;
        if !entry.regular || !safe_attributes.contains(&entry.path) {
            continue;
        }
        let donors: Vec<_> = inventories
            .iter()
            .filter_map(|(source, inventory)| {
                inventory
                    .get(&entry.path)
                    .filter(|original| original.regular && original.oid == entry.oid)
                    .map(|_| *source)
            })
            .collect();
        if !donors.is_empty() {
            creation.parents(&entry.path)?;
            jobs.push(SeedJob { entry, donors });
        }
    }
    timings.mark("plan clone jobs");
    let (cloned, provenance, clone_timings) = seed_jobs(creation, &jobs, mask, timings.enabled)?;
    timings.mark("verify and clone files");
    for (label, duration) in clone_timings {
        timings.worker_time(label, duration);
    }
    if !wanted.is_empty() && cloned.is_empty() {
        return Err(Error::Message("COW creation unavailable: no verified, checkout-compatible source files on the destination APFS volume; no full-copy fallback was used".into()));
    }
    creation.check_cancelled()?;
    let seeds: HashSet<_> = cloned.iter().collect();
    let missing = paths(
        wanted
            .iter()
            .filter(|entry| !seeds.contains(&entry.path) && !entry.gitlink)
            .map(|entry| entry.path.as_path()),
    );
    // Git must see no existing files at paths it is about to materialize.
    for entry in wanted.iter().filter(|entry| !seeds.contains(&entry.path)) {
        if fs::symlink_metadata(path.join(&entry.path)).is_ok() {
            return Err(Error::Message(format!(
                "unexpected destination path: {}",
                entry.path.display()
            )));
        }
    }
    creation.materializing = true;
    creation.verify_authority()?;
    if !missing.is_empty() {
        git.bytes(
            &path,
            &["checkout-index", "--index", "-z", "--stdin"],
            Some(&missing),
        )?;
    }
    // Native non-recursive checkout creates empty gitlink directories.
    for entry in wanted.iter().filter(|entry| entry.gitlink) {
        fs::create_dir_all(path.join(&entry.path))?;
    }
    if sparse && config_bool(git, &path, "index.sparse")? {
        git.bytes(
            &path,
            &["sparse-checkout", "reapply", "--sparse-index"],
            None,
        )?;
    }
    timings.mark("materialize remaining files");
    git.bytes(&path, &["update-index", "--refresh"], None)?;
    timings.mark("refresh index");
    creation.check_cancelled()?;
    creation.verify_authority()?;
    for seed in &cloned {
        if creation.owned.get(seed) != Some(&identity(&path.join(seed))?) {
            return Err(Error::Message(format!(
                "seeded clone changed during Git materialization: {}",
                seed.display()
            )));
        }
    }
    if git.text(&path, &["rev-parse", "HEAD"])? != commit {
        return Err(Error::Message("target HEAD changed during creation".into()));
    }
    if !git
        .bytes(
            &path,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
            None,
        )?
        .is_empty()
    {
        return Err(Error::Message(
            "checkout validation failed; worktree is not clean".into(),
        ));
    }
    timings.mark("validate checkout");
    if !add.locked {
        git.bytes(&path, &["worktree", "unlock", "."], None)?;
    }
    creation.complete = true;
    if !add.quiet {
        let summary = git.bytes(&path, &["log", "-1", "--format=HEAD is now at %h %s"], None)?;
        std::io::stdout().write_all(&summary)?;
    }
    // Git's native add retains the completed worktree when a hook fails.
    let code = add::post_checkout(git, &path, &commit)?;
    let record = || -> Result<()> {
        // Hooks may rewrite clones or switch HEAD. Record only clones whose
        // identities survived the hook, never hook-generated copies.
        let retained = cloned
            .iter()
            .filter(|seed| {
                identity(&path.join(seed))
                    .is_ok_and(|value| creation.owned.get(*seed) == Some(&value))
            })
            .count();
        if retained > 0
            && creation.verify_authority().is_ok()
            && git.text(&path, &["rev-parse", "HEAD"])? == commit
        {
            let mut provenance: Vec<_> = provenance.into_iter().collect();
            provenance.sort();
            receipt::write_creation(&creation.admin, &commit, &provenance, retained as u64)?;
        }
        Ok(())
    };
    // Creation and the hook have finished. Optional bookkeeping must not turn
    // their success into a failure that encourages callers to retry `add`.
    if code == 0
        && let Err(error) = record()
    {
        eprintln!("cowtree: worktree created, but could not record creation receipt: {error}");
    }
    timings.mark("finalize and run hook");
    Ok(code)
}

fn path_output(mut bytes: Vec<u8>) -> Result<PathBuf> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.is_empty() {
        return Err(Error::Message("Git returned an empty path".into()));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn paths<'a>(paths: impl Iterator<Item = &'a Path>) -> Vec<u8> {
    let mut result = Vec::new();
    for path in paths {
        result.extend_from_slice(path.as_os_str().as_bytes());
        result.push(0);
    }
    result
}

fn tree(raw: Vec<u8>) -> Result<Vec<Entry>> {
    nul_paths(&raw)
        .map(|record| {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| Error::Message("malformed Git tree".into()))?;
            let fields: Vec<_> = record[..tab].split(|b| *b == b' ').collect();
            if fields.len() != 3 {
                return Err(Error::Message("malformed Git tree entry".into()));
            }
            let path = PathBuf::from(OsString::from_vec(record[tab + 1..].to_vec()));
            validate_relative(&path)?;
            Ok(Entry {
                path,
                oid: String::from_utf8_lossy(fields[2]).into_owned(),
                executable: fields[0] == b"100755",
                regular: matches!(fields[0], b"100644" | b"100755"),
                gitlink: fields[0] == b"160000",
            })
        })
        .collect()
}

fn config(git: &Git, path: &Path, key: &str) -> Result<String> {
    let output = git.at(path).args(["config", "--get", key]).output()?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    if output.status.code() == Some(1) {
        return Ok(String::new());
    }
    Err(Error::Git(
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn config_bool(git: &Git, path: &Path, key: &str) -> Result<bool> {
    let output = git
        .at(path)
        .args(["config", "--type=bool", "--get", key])
        .output()?;
    if output.status.success() {
        return Ok(output.stdout == b"true\n");
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    Err(Error::Git(
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn checkout_safe(git: &Git, path: &Path, entries: &[&Entry]) -> Result<HashSet<PathBuf>> {
    if entries.is_empty() {
        return Ok(HashSet::new());
    }
    let input = paths(entries.iter().map(|entry| entry.path.as_path()));
    let output = git.bytes(
        path,
        &[
            "check-attr",
            "--cached",
            "-z",
            "--stdin",
            "filter",
            "text",
            "eol",
            "working-tree-encoding",
            "ident",
        ],
        Some(&input),
    )?;
    let autocrlf = config(git, path, "core.autocrlf")?;
    let eol = config(git, path, "core.eol")?;
    let converts = autocrlf != "input"
        && (config_bool(git, path, "core.autocrlf")? || eol.eq_ignore_ascii_case("crlf"));
    let mut safe: HashSet<_> = entries
        .iter()
        .filter(|entry| entry.regular)
        .map(|entry| entry.path.clone())
        .collect();
    // Attribute values can legitimately be empty (for example, `filter=`).
    // A path-list parser discards them and misaligns the remaining records.
    let mut fields: Vec<_> = output.split(|byte| *byte == 0).collect();
    if fields.pop() != Some(&[]) || fields.len() != entries.len() * 15 {
        return Err(Error::Message(
            "malformed checkout attribute inventory".into(),
        ));
    }
    for record in fields.as_chunks::<15>().0 {
        let relative = PathBuf::from(OsString::from_vec(record[0].to_vec()));
        let mut text_unset = false;
        for attr in record.as_chunks::<3>().0 {
            if attr[1] == b"text" && attr[2] == b"unset" {
                text_unset = true;
            }
            if !matches!(attr[2], b"unspecified" | b"unset") {
                safe.remove(&relative);
            }
        }
        if converts && !text_unset {
            safe.remove(&relative);
        }
    }
    Ok(safe)
}

struct SeedJob<'a> {
    entry: &'a Entry,
    donors: Vec<&'a worktree::Worktree>,
}

type SeedResult = (Vec<PathBuf>, HashSet<String>, Vec<(&'static str, Duration)>);

fn seed_jobs(
    creation: &mut Creation<'_>,
    jobs: &[SeedJob<'_>],
    mask: u32,
    measure_timings: bool,
) -> Result<SeedResult> {
    if jobs.is_empty() {
        return Ok((Vec::new(), HashSet::new(), Vec::new()));
    }
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4)
        .min((jobs.len() / 256).max(1));
    let chunk_size = jobs.len().div_ceil(workers);
    let stopped = AtomicBool::new(false);
    let clone_timings = platform::CloneTimings::new();
    let measured = measure_timings.then_some(&clone_timings);
    let target = &creation.path;
    let signal = &creation.cancelled;
    let clone_chunk = |jobs: &[SeedJob<'_>]| {
        let mut completed = Vec::new();
        let mut directories = platform::CloneDirectoryCache::new();
        let result = (|| -> Result<()> {
            for job in jobs {
                if stopped.load(Ordering::Relaxed) || signal.load(Ordering::Relaxed) != 0 {
                    break;
                }
                let mode = (if job.entry.executable { 0o777 } else { 0o666 }) & !mask;
                for donor in &job.donors {
                    // Hash through the pinned source fd. Its identity snapshot
                    // spans both hashing and cloning, including hidden edits.
                    match platform::clone_new(
                        &donor.path,
                        target,
                        &job.entry.path,
                        mode,
                        &mut directories,
                        measured,
                        |file| blob_matches(file, &job.entry.oid),
                    )? {
                        CloneOutcome::Cloned => {
                            completed.push((
                                job.entry.path.clone(),
                                identity(&target.join(&job.entry.path))?,
                                donor.head.clone(),
                            ));
                            break;
                        }
                        CloneOutcome::NotRegular | CloneOutcome::ChangedDuringClone => {}
                    }
                }
            }
            Ok(())
        })();
        if result.is_err() {
            stopped.store(true, Ordering::Relaxed);
        }
        (completed, result)
    };
    let results = if workers == 1 {
        vec![clone_chunk(jobs)]
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = jobs
                .chunks(chunk_size)
                .map(|chunk| scope.spawn(|| clone_chunk(chunk)))
                .collect();
            // Join every worker, even after failure, before ownership checks or
            // cleanup. Successfully created files from failed chunks count too.
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        (
                            Vec::new(),
                            Err(Error::Message("creation worker panicked".into())),
                        )
                    })
                })
                .collect()
        })
    };
    let mut paths = Vec::new();
    let mut sources = HashSet::new();
    let mut error = None;
    for (completed, result) in results {
        for (path, identity, source) in completed {
            creation.owned.insert(path.clone(), identity);
            paths.push(path);
            sources.insert(source);
        }
        if let Err(value) = result {
            error.get_or_insert(value);
        }
    }
    creation.check_cancelled()?;
    if let Some(error) = error {
        return Err(error);
    }
    Ok((paths, sources, clone_timings.summary()))
}

fn blob_matches(file: &mut File, oid: &str) -> Result<bool> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(false);
    }
    let header = format!("blob {}\0", meta.len());
    let hash = if oid.len() == 64 {
        hash_blob::<sha2::Sha256>(file, header.as_bytes())?
    } else {
        hash_blob::<sha1::Sha1>(file, header.as_bytes())?
    };
    Ok(hash == oid)
}

fn hash_blob<D: Digest>(file: &mut File, header: &[u8]) -> Result<String> {
    let mut hash = D::new();
    hash.update(header);
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn parallel_failure_joins_workers_and_records_all_owned_clones() {
        let root = tempfile::tempdir().unwrap();
        if SystemPlatform.validate(root.path(), root.path()).is_err() {
            return;
        }
        let donor = root.path().join("donor");
        let target = root.path().join("target");
        fs::create_dir(&donor).unwrap();
        fs::create_dir(&target).unwrap();
        let oid: String = sha1::Sha1::digest(b"blob 4\0data")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let entries: Vec<_> = (0..1024)
            .map(|n| {
                let path = PathBuf::from(format!("file-{n}"));
                fs::write(donor.join(&path), "data").unwrap();
                Entry {
                    path,
                    oid: oid.clone(),
                    executable: false,
                    regular: true,
                    gitlink: false,
                }
            })
            .collect();
        let source = worktree::Worktree {
            path: donor,
            head: "source".into(),
            branch: None,
            locked: false,
        };
        let jobs: Vec<_> = entries
            .iter()
            .map(|entry| SeedJob {
                entry,
                donors: vec![&source],
            })
            .collect();
        fs::write(target.join("file-700"), "external").unwrap();
        let git = Git;
        let mut creation = Creation {
            git: &git,
            path: target.clone(),
            admin: root.path().join("admin"),
            owned: HashMap::new(),
            directories: HashMap::new(),
            original_directory: None,
            materializing: false,
            complete: false,
            cancelled: Arc::new(AtomicUsize::new(0)),
        };
        assert!(seed_jobs(&mut creation, &jobs, 0o022, false).is_err());
        assert_eq!(fs::read(target.join("file-700")).unwrap(), b"external");
        let files: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), creation.owned.len() + 1);
        for path in files {
            if path.file_name() == Some(OsStr::new("file-700")) {
                continue;
            }
            assert_eq!(
                creation.owned.get(path.strip_prefix(&target).unwrap()),
                Some(&identity(&path).unwrap())
            );
        }
    }
}

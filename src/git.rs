use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::Read,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::Digest;

use crate::{
    eligibility::validate_relative,
    error::{Error, Result},
};

#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub oid: String,
    pub executable: bool,
    pub regular: bool,
    pub gitlink: bool,
    pub tree: bool,
}

pub fn output<I, S>(cwd: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let result = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(Error::Io)?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        return Err(Error::Git(if stderr.is_empty() {
            format!("exited with {}", result.status)
        } else {
            stderr
        }));
    }
    Ok(result.stdout)
}

pub fn text<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let bytes = output(cwd, args)?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
}

pub fn nul_paths(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
}

pub fn parse_tree(raw: Vec<u8>) -> Result<Vec<TreeEntry>> {
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
            Ok(TreeEntry {
                path,
                oid: String::from_utf8_lossy(fields[2]).into_owned(),
                executable: fields[0] == b"100755",
                regular: matches!(fields[0], b"100644" | b"100755"),
                gitlink: fields[0] == b"160000",
                tree: fields[0] == b"040000",
            })
        })
        .collect()
}

pub fn blob_matches(file: &mut File, oid: &str) -> Result<bool> {
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

    #[test]
    fn parses_nul_paths_without_interpreting_content() {
        let input = b"space name\0tab\tname\0line\nname\0-leading\0unicode-\xe2\x98\x83\0";
        assert_eq!(
            nul_paths(input).collect::<Vec<_>>(),
            vec![
                &b"space name"[..],
                &b"tab\tname"[..],
                &b"line\nname"[..],
                &b"-leading"[..],
                &b"unicode-\xe2\x98\x83"[..],
            ]
        );
    }
}

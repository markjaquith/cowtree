use std::{ffi::OsStr, path::Path, process::Command};

use crate::error::{Error, Result};

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

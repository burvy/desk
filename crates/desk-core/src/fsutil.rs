//! Small filesystem helpers. Every write in desk goes through these, so a
//! reader never sees a half-written file.

use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Write to a temp file next to `path`, then rename over it.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Create `path` only if it doesn't exist. Returns false if it does.
pub fn create_new(path: &Path, contents: &str) -> Result<bool> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(contents.as_bytes())?;
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

pub fn append_line(path: &Path, line: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    // One write call per line, so concurrent appenders don't interleave.
    f.write_all(format!("{line}\n").as_bytes())?;
    Ok(())
}

pub fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// Next free 3-digit id among `dir`'s entries (`014`, `014-slug`, `014.md`).
pub fn next_id(dir: &Path) -> Result<u32> {
    let mut max = 0;
    if dir.exists() {
        for e in fs::read_dir(dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse::<u32>() {
                max = max.max(n);
            }
        }
    }
    Ok(max + 1)
}

pub fn fmt_id(n: u32) -> String {
    format!("{n:03}")
}

/// A cross-process mutex: a file created with `create_new`.
/// Stale locks (older than 30s, i.e. a crashed writer) are broken.
pub struct FileLock(PathBuf);

impl FileLock {
    pub fn acquire(path: PathBuf) -> Result<Self> {
        for _ in 0..200 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(FileLock(path)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > Duration::from_secs(30));
                    if stale {
                        let _ = fs::remove_file(&path);
                    } else {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        bail!("timed out waiting for {}", path.display())
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// Lowercase, dashes, ascii only. For task folder names.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Files in `dir` (not subdirectories) ending in `ext`, sorted by name.
pub fn files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == ext))
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Explosions → sand, not craters!"), "explosions-sand-not-craters");
    }
}

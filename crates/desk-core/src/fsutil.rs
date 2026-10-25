//! Small filesystem helpers. Every write in desk goes through these, so a
//! reader never sees a half-written file.

use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

/// A temp file next to `path`, unique to this process and call. Its
/// extension isn't `.md` or `.toml`, so nothing reads it as the real thing.
fn temp_for(path: &Path, contents: &str) -> Result<PathBuf> {
    static N: AtomicU32 = AtomicU32::new(0);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let n = N.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{n}", std::process::id()));
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    Ok(tmp)
}

/// Write to a temp file next to `path`, then rename over it.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = temp_for(path, contents)?;
    // On Windows a rename fails while someone (an indexer, an editor) has the
    // target open; that passes quickly, so try a few times.
    for attempt in 0.. {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::PermissionDenied && attempt < 10 => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e).with_context(|| format!("replacing {}", path.display()));
            }
        }
    }
    unreachable!()
}

/// Create `path` with its contents in one step, only if it doesn't exist.
/// Returns false if it does. A hard link from a finished temp file means no
/// reader ever sees it empty or half-written.
pub fn create_new(path: &Path, contents: &str) -> Result<bool> {
    let tmp = temp_for(path, contents)?;
    let linked = fs::hard_link(&tmp, path);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        // Some network drives can't hard-link: create, then write.
        Err(_) => match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                f.write_all(contents.as_bytes())?;
                Ok(true)
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
        },
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

/// A cross-process mutex: a file created with `create_new`, holding a token
/// unique to its holder. Stale locks (older than 30s, i.e. a crashed writer)
/// are broken. Every holder here keeps it for milliseconds.
pub struct FileLock {
    path: PathBuf,
    token: String,
}

impl FileLock {
    pub fn acquire(path: PathBuf) -> Result<Self> {
        static N: AtomicU32 = AtomicU32::new(0);
        let token = format!("{}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed), now_rfc3339());
        for _ in 0..200 {
            if create_new(&path, &token)? {
                return Ok(FileLock { path, token });
            }
            let stale = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > Duration::from_secs(30));
            if stale {
                // Rename, don't delete: if two waiters both see it stale, only
                // one rename succeeds, and neither deletes a fresh lock.
                let broken = path.with_extension(format!("stale-{token}"));
                if fs::rename(&path, &broken).is_ok() {
                    let _ = fs::remove_file(&broken);
                }
            } else {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        bail!("timed out waiting for {}", path.display())
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Only our own: a lock broken as stale may belong to someone else now.
        if fs::read_to_string(&self.path).is_ok_and(|t| t == self.token) {
            let _ = fs::remove_file(&self.path);
        }
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

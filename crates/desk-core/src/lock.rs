//! Locks on shared resources (a server, a port): `locks/<name>.toml`.
//! The supervisor releases an agent's locks when it's killed, reset, or stops
//! after repeated failures.

use crate::fsutil;
use crate::workspace::Workspace;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lock {
    #[serde(skip)]
    pub name: String,
    pub holder: String,
    pub since: String,
    #[serde(default)]
    pub note: String,
}

pub fn list(ws: &Workspace) -> Vec<Lock> {
    fsutil::files_with_ext(&ws.locks_dir(), "toml")
        .into_iter()
        .filter_map(|p| {
            let mut l: Lock = toml::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
            l.name = p.file_stem()?.to_string_lossy().into_owned();
            Some(l)
        })
        .collect()
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        bail!("lock names are letters, digits, - _ and .");
    }
    Ok(())
}

pub fn acquire(ws: &Workspace, name: &str, holder: &str, note: &str) -> Result<()> {
    check_name(name)?;
    let lock =
        Lock { name: name.into(), holder: holder.into(), since: fsutil::now_rfc3339(), note: note.into() };
    let path = ws.locks_dir().join(format!("{name}.toml"));
    if !fsutil::create_new(&path, &toml::to_string(&lock)?)? {
        match list(ws).into_iter().find(|l| l.name == name) {
            Some(l) if l.holder == holder => return Ok(()),
            Some(l) => bail!("`{name}` is held by {} since {} {}", l.holder, l.since, l.note),
            None => bail!("`{name}` is locked"),
        }
    }
    ws.log("lock", serde_json::json!({ "lock": name, "holder": holder }));
    Ok(())
}

/// Only the holder (or the human) can release a lock.
pub fn release(ws: &Workspace, name: &str, by: &str) -> Result<()> {
    let Some(l) = list(ws).into_iter().find(|l| l.name == name) else { bail!("`{name}` isn't locked") };
    if l.holder != by && by != ws.config.human {
        bail!("`{name}` is held by {}, not {by}", l.holder);
    }
    std::fs::remove_file(ws.locks_dir().join(format!("{name}.toml")))?;
    ws.log("unlock", serde_json::json!({ "lock": name, "by": by }));
    Ok(())
}

pub fn release_all(ws: &Workspace, holder: &str) {
    for l in list(ws).into_iter().filter(|l| l.holder == holder) {
        let _ = release(ws, &l.name, holder);
    }
}

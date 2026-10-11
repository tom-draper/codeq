//! Atomic file replacement and the installer ownership journal.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub version: u32,
    pub entries: BTreeMap<String, Owned>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Owned {
    pub owners: BTreeSet<String>,
    pub installed: Vec<u8>,
    pub previous: Option<Vec<u8>>,
}

pub(super) fn state_path(home: &Path) -> PathBuf {
    home.join(".codeq/install.json")
}

pub(super) fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub(super) fn linked(home: &Path, path: &Path) -> bool {
    path.ancestors()
        .take_while(|p| *p != home)
        .any(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
}

pub(super) fn require_regular(home: &Path, path: &Path) -> Result<()> {
    if linked(home, path) {
        bail!("{} is symlinked; manage it externally", path.display());
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if !meta.is_file() {
            bail!("{} is not a regular file", path.display());
        }
    }
    Ok(())
}

pub(super) fn load(home: &Path) -> Result<Manifest> {
    let path = state_path(home);
    require_regular(home, &path)?;
    let Some(bytes) = read(&path)? else {
        return Ok(Manifest {
            version: 1,
            ..Manifest::default()
        });
    };
    let state: Manifest =
        serde_json::from_slice(&bytes).context("invalid installer ownership journal")?;
    if state.version != 1 {
        bail!("unsupported installer journal version {}", state.version);
    }
    Ok(state)
}

/// The lock is advisory and serializes codeq installers. A changed-file check
/// also detects edits by other applications before replacing a file.
pub(super) fn lock(home: &Path) -> Result<File> {
    let path = home.join(".codeq/install.lock");
    require_regular(home, &path)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    FileExt::try_lock_exclusive(&file).context("another codeq installer is running")?;
    Ok(file)
}

pub(super) fn save(home: &Path, state: &Manifest) -> Result<()> {
    let path = state_path(home);
    replace(
        home,
        &path,
        &read(&path)?,
        Some(&serde_json::to_vec_pretty(state)?),
    )
}

/// Prepare on the same filesystem, sync, then atomically rename. Existing
/// permission bits survive. A failed preparation never truncates the target.
pub(super) fn replace(
    home: &Path,
    path: &Path,
    before: &Option<Vec<u8>>,
    after: Option<&[u8]>,
) -> Result<()> {
    require_regular(home, path)?;
    if read(path)? != *before {
        bail!("{} changed during installation; rerun", path.display());
    }
    match after {
        None => {
            if before.is_some() {
                std::fs::remove_file(path)?;
            }
        }
        Some(bytes) => {
            let parent = path.parent().context("file has no parent directory")?;
            std::fs::create_dir_all(parent)?;
            let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
            if let Ok(meta) = std::fs::metadata(path) {
                tmp.as_file().set_permissions(meta.permissions())?;
            }
            tmp.write_all(bytes)?;
            tmp.as_file().sync_all()?;
            require_regular(home, path)?;
            if read(path)? != *before {
                bail!("{} changed during installation; rerun", path.display());
            }
            tmp.persist(path).map_err(|e| e.error)?;
        }
    }
    // Directory syncing is supported on Unix. On other platforms atomic rename
    // still protects against partial writes, without claiming power-loss durability.
    #[cfg(unix)]
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

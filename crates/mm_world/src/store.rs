//! Maps on disk, named by the blake3 hash of their bytes. Files are written
//! to a temporary name and renamed, so a crash never leaves a partial map.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MapHash(pub [u8; 32]);

impl MapHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    pub fn parse(hex: &str) -> anyhow::Result<Self> {
        Ok(Self(*blake3::Hash::from_hex(hex).context("bad map hash")?.as_bytes()))
    }
}

impl fmt::Display for MapHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(blake3::Hash::from(self.0).to_hex().as_str())
    }
}

#[derive(Clone, Debug)]
pub struct WorldStore {
    dir: PathBuf,
}

impl WorldStore {
    pub fn open(dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn path(&self, hash: MapHash) -> PathBuf {
        self.dir.join(format!("{hash}.mmw"))
    }

    /// Stores `bytes`, returning their hash. Blocking; call off the main thread.
    pub fn put(&self, bytes: &[u8]) -> anyhow::Result<MapHash> {
        let hash = MapHash::of(bytes);
        let path = self.path(hash);
        if path.exists() {
            return Ok(hash);
        }
        let tmp = self.dir.join(format!(".{hash}.{}.tmp", std::process::id()));
        let mut file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        sync_dir(&self.dir);
        Ok(hash)
    }

    /// Reads and verifies a map. Blocking.
    pub fn get(&self, hash: MapHash) -> anyhow::Result<Vec<u8>> {
        let path = self.path(hash);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        ensure!(MapHash::of(&bytes) == hash, "{} is corrupt", path.display());
        Ok(bytes)
    }
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_and_detect_corruption() {
        let dir = std::env::temp_dir().join(format!("mm_world_store_{}", std::process::id()));
        let store = WorldStore::open(&dir).unwrap();
        let hash = store.put(b"hello").unwrap();
        assert_eq!(store.put(b"hello").unwrap(), hash);
        assert_eq!(store.get(hash).unwrap(), b"hello");
        assert_eq!(MapHash::parse(&hash.to_string()).unwrap(), hash);

        std::fs::write(dir.join(format!("{hash}.mmw")), b"tampered").unwrap();
        assert!(store.get(hash).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

//! Content-addressed filesystem store for `.apworld` blobs.
//!
//! Files live at `<root>/blobs/<sha256[0:2]>/<sha256>.apworld`. Writes go
//! through a tempfile + atomic rename so concurrent writers of the same
//! sha256 cannot leave the destination empty or partially written. The
//! authoritative answer to "have I seen this blob" lives in the database
//! (`apworld_artifacts` UNIQUE constraint); the filesystem write is
//! unconditional and idempotent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Final destination path for a blob. Does not perform I/O.
    pub fn path_for(&self, sha256_hex: &str) -> PathBuf {
        debug_assert!(sha256_hex.len() >= 2, "sha256 hex too short");
        self.root
            .join("blobs")
            .join(&sha256_hex[..2])
            .join(format!("{sha256_hex}.apworld"))
    }

    /// Write `bytes` to the canonical blob path. Safe under concurrent writes
    /// of the same key: tempfile-then-rename ensures no observer sees a
    /// partially-written file at the destination.
    pub async fn put(&self, sha256_hex: &str, bytes: &[u8]) -> Result<()> {
        let bytes = bytes.to_vec();
        let final_path = self.path_for(sha256_hex);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let parent = final_path
                .parent()
                .context("blob path has no parent directory")?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all failed for {}", parent.display()))?;
            let mut tmp = tempfile::NamedTempFile::new_in(parent)
                .with_context(|| format!("NamedTempFile in {}", parent.display()))?;
            std::io::Write::write_all(&mut tmp, &bytes)
                .with_context(|| format!("write to {}", tmp.path().display()))?;
            tmp.persist(&final_path)
                .map_err(|e| e.error)
                .with_context(|| format!("rename into {}", final_path.display()))?;
            Ok(())
        })
        .await
        .context("blob write task panicked")??;
        Ok(())
    }

    /// Read the full contents of a blob into memory. Returns the raw bytes;
    /// callers parse / extract as appropriate.
    pub async fn read(&self, sha256_hex: &str) -> Result<Vec<u8>> {
        let path = self.path_for(sha256_hex);
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("read blob {}", path.display()))
    }

    /// True if a blob with the given sha256 already exists on disk.
    /// Cheap probe; useful for tests and for shortcutting writes when the DB
    /// hasn't been consulted.
    pub async fn exists(&self, sha256_hex: &str) -> bool {
        let path = self.path_for(sha256_hex);
        tokio::fs::metadata(&path).await.is_ok()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[tokio::test]
    async fn put_then_read_roundtrips_identical_bytes() {
        let tmp = TempDir::new().unwrap();
        let store = BlobStore::new(tmp.path());
        let payload = b"hello, blob store";
        let hex = sha256_hex(payload);

        store.put(&hex, payload).await.unwrap();
        let got = store.read(&hex).await.unwrap();

        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn put_creates_two_char_prefix_directory() {
        let tmp = TempDir::new().unwrap();
        let store = BlobStore::new(tmp.path());
        let payload = b"prefix layout";
        let hex = sha256_hex(payload);

        store.put(&hex, payload).await.unwrap();

        let expected = tmp
            .path()
            .join("blobs")
            .join(&hex[..2])
            .join(format!("{hex}.apworld"));
        assert!(expected.exists(), "expected blob at {}", expected.display());
    }

    #[tokio::test]
    async fn concurrent_put_of_same_key_is_safe() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(BlobStore::new(tmp.path()));
        let payload = b"concurrent writers";
        let hex = sha256_hex(payload);

        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = Arc::clone(&store);
            let hex = hex.clone();
            handles.push(tokio::spawn(async move {
                store.put(&hex, payload).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let got = store.read(&hex).await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn exists_reports_correctly() {
        let tmp = TempDir::new().unwrap();
        let store = BlobStore::new(tmp.path());
        let payload = b"existence";
        let hex = sha256_hex(payload);

        assert!(!store.exists(&hex).await);
        store.put(&hex, payload).await.unwrap();
        assert!(store.exists(&hex).await);
    }
}

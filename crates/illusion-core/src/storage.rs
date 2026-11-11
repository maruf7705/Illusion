use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::task;

#[derive(Clone)]
pub struct DiskStorage {
    file: Arc<File>,
    written: Arc<AtomicU64>,
}

impl DiskStorage {
    /// Open or create the sparse/preallocated file without truncating existing bytes,
    /// preserving previously written segments on resume (Hydra `SparseSink` invariant).
    pub async fn create_or_open(path: &Path, preallocate_bytes: Option<u64>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("Failed to create destination directory: {:?}", parent)
            })?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("Failed to open destination file: {:?}", path))?;

        if let Some(size) = preallocate_bytes {
            if size > 0 {
                let file_ref = file.try_clone()?;
                task::spawn_blocking(move || {
                    use std::os::unix::io::AsRawFd;
                    let fd = file_ref.as_raw_fd();
                    // Fast Linux kernel posix_fallocate; ignores EOPNOTSUPP on special filesystems
                    let _ = nix::fcntl::posix_fallocate(fd, 0, size as i64);
                    let _ = file_ref.set_len(size);
                })
                .await?;
            }
        }

        Ok(Self {
            file: Arc::new(file),
            written: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Lock-free, zero-copy positioned write directly via Linux `pwrite64` (`write_all_at`).
    /// Eliminates per-packet heap `.to_vec()` copies and `spawn_blocking` scheduling churn
    /// on the hot transfer path (ported from Hydra `SparseSink::write_at`).
    #[inline]
    pub fn write_at_slice(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        if !bytes.is_empty() {
            self.file.write_all_at(bytes, offset)?;
            self.written.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    pub async fn write_at(&self, offset: u64, bytes: Vec<u8>) -> Result<()> {
        let storage = self.clone();
        task::spawn_blocking(move || {
            storage.write_at_slice(offset, &bytes)?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub fn total_written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub async fn sync_all(&self) -> Result<()> {
        let file = self.file.clone();
        task::spawn_blocking(move || {
            file.sync_all()?;
            Ok::<(), std::io::Error>(())
        })
        .await??;
        Ok(())
    }
}

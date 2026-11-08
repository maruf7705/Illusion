use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use tokio::task;

#[derive(Clone)]
pub struct DiskStorage {
    file: Arc<File>,
}

impl DiskStorage {
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
                    // Attempt fast Linux kernel fallocate
                    let _ = nix::fcntl::posix_fallocate(fd, 0, size as i64);
                    // Also ensure file length matches
                    let _ = file_ref.set_len(size);
                })
                .await?;
            }
        }

        Ok(Self {
            file: Arc::new(file),
        })
    }

    pub async fn write_at(&self, offset: u64, bytes: Vec<u8>) -> Result<()> {
        let file = self.file.clone();
        task::spawn_blocking(move || {
            file.write_all_at(&bytes, offset)?;
            Ok::<(), std::io::Error>(())
        })
        .await??;
        Ok(())
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

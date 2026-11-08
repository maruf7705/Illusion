use crate::task::DownloadTask;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct AppState {
    pub tasks: Arc<RwLock<Vec<DownloadTask>>>,
    pub config_dir: PathBuf,
    pub default_download_dir: PathBuf,
}

impl AppState {
    pub fn load_or_default() -> Self {
        let (config_dir, default_download_dir) = if let Some(proj) = ProjectDirs::from("com", "illusion", "Illusion") {
            let conf = proj.config_dir().to_path_buf();
            let downloads = directories::UserDirs::new()
                .and_then(|u| u.download_dir().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| dirs_fallback_downloads());
            (conf, downloads)
        } else {
            (PathBuf::from(".config/illusion"), PathBuf::from("Downloads"))
        };

        let _ = fs::create_dir_all(&config_dir);
        let _ = fs::create_dir_all(&default_download_dir);

        let tasks_file = config_dir.join("tasks.json");
        let tasks = if tasks_file.exists() {
            match fs::read_to_string(&tasks_file) {
                Ok(content) => serde_json::from_str::<Vec<DownloadTask>>(&content).unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };

        Self {
            tasks: Arc::new(RwLock::new(tasks)),
            config_dir,
            default_download_dir,
        }
    }

    pub async fn save_tasks(&self) -> Result<()> {
        let tasks = self.tasks.read().await;
        let data = serde_json::to_string_pretty(&*tasks).context("Failed to serialize tasks")?;
        let tasks_file = self.config_dir.join("tasks.json");
        tokio::fs::write(&tasks_file, data)
            .await
            .with_context(|| format!("Failed to write tasks to {:?}", tasks_file))?;
        Ok(())
    }

    pub async fn add_task(&self, task: DownloadTask) {
        let mut list = self.tasks.write().await;
        list.insert(0, task);
        drop(list);
        let _ = self.save_tasks().await;
    }

    pub async fn get_task(&self, id: &str) -> Option<DownloadTask> {
        let list = self.tasks.read().await;
        list.iter().find(|t| t.id == id).cloned()
    }

    pub async fn remove_task(&self, id: &str) {
        let mut list = self.tasks.write().await;
        list.retain(|t| t.id != id);
        drop(list);
        let _ = self.save_tasks().await;
    }
}

fn dirs_fallback_downloads() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Downloads")
    } else {
        PathBuf::from("./Downloads")
    }
}

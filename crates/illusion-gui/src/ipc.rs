use illusion_core::{AppState, DownloadTask};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcDownloadRequest {
    pub action: String,
    pub url: Option<String>,
    pub filename: Option<String>,
    pub referer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcResponse {
    pub status: String,
    pub message: Option<String>,
    pub task_id: Option<String>,
    pub filename: Option<String>,
}

pub fn get_ipc_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("illusion.sock")
    } else {
        let uid = std::process::id();
        PathBuf::from(format!("/tmp/illusion-{}.sock", uid))
    }
}

pub fn start_ipc_server(
    rt: Arc<tokio::runtime::Runtime>,
    state: AppState,
    task_notifier: mpsc::UnboundedSender<String>,
) {
    let socket_path = get_ipc_socket_path();

    rt.spawn(async move {
        // Clean up previous dead socket if present
        let _ = tokio::fs::remove_file(&socket_path).await;

        let listener = match UnixListener::bind(&socket_path) {
            Ok(l) => {
                info!("⚡ Illusion Unix Socket IPC server bound at {:?}", socket_path);
                l
            }
            Err(e) => {
                error!("Failed to bind Illusion IPC socket at {:?}: {:#}", socket_path, e);
                return;
            }
        };

        loop {
            match listener.accept().await {
                Ok((mut stream, _addr)) => {
                    let state = state.clone();
                    let notifier = task_notifier.clone();

                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        match stream.read(&mut buf).await {
                            Ok(n) if n > 0 => {
                                if let Ok(req) = serde_json::from_slice::<IpcDownloadRequest>(&buf[..n]) {
                                    if req.action == "ping" {
                                        let resp = IpcResponse {
                                            status: "pong".to_string(),
                                            message: Some("Illusion IPC Daemon ready".to_string()),
                                            task_id: None,
                                            filename: None,
                                        };
                                        let res_bytes = serde_json::to_vec(&resp).unwrap_or_default();
                                        let _ = stream.write_all(&res_bytes).await;
                                        return;
                                    }

                                    if let Some(url) = req.url {
                                        let mut filename = req.filename.unwrap_or_default();
                                        if filename.is_empty() {
                                            filename = url.split('/').last().unwrap_or("download").to_string();
                                        }
                                        filename = illusion_core::prober::sanitize_filename(&filename);

                                        let mut headers = req.headers.unwrap_or_default();
                                        if let Some(ref r) = req.referer {
                                            headers.insert("referer".to_string(), r.clone());
                                        }

                                        let save_path = state.default_download_dir.join(&filename);
                                        let task = DownloadTask::new(
                                            url,
                                            filename.clone(),
                                            save_path,
                                            None,
                                            headers,
                                            req.cookies,
                                            req.user_agent,
                                        );

                                        let task_id = task.id.clone();
                                        state.add_task(task).await;

                                        // Notify GUI to auto-start or highlight
                                        let _ = notifier.send(task_id.clone());

                                        let resp = IpcResponse {
                                            status: "success".to_string(),
                                            message: Some("Download queued in live Illusion GUI".to_string()),
                                            task_id: Some(task_id),
                                            filename: Some(filename),
                                        };
                                        let res_bytes = serde_json::to_vec(&resp).unwrap_or_default();
                                        let _ = stream.write_all(&res_bytes).await;
                                    }
                                }
                            }
                            _ => {}
                        }
                    });
                }
                Err(e) => {
                    error!("Error accepting IPC connection: {:#}", e);
                }
            }
        }
    });
}

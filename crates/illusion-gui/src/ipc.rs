use eframe::egui;
use illusion_core::{AppState, DownloadTask};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub const HTTP_BRIDGE_PORT: u16 = 9095;

fn default_action() -> String {
    "add_download".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcDownloadRequest {
    #[serde(default = "default_action")]
    pub action: String,
    pub url: Option<String>,
    pub filename: Option<String>,
    pub referer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub audio_url: Option<String>,
    pub quality: Option<String>,
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcResponse {
    pub status: String,
    pub message: Option<String>,
    pub task_id: Option<String>,
    pub filename: Option<String>,
}

fn get_user_id() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

pub fn get_ipc_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("illusion.sock")
    } else {
        let uid = get_user_id();
        let run_user = PathBuf::from(format!("/run/user/{}", uid));
        if run_user.exists() {
            run_user.join("illusion.sock")
        } else {
            PathBuf::from(format!("/tmp/illusion-{}.sock", uid))
        }
    }
}

pub async fn read_ipc_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    // Support both length-prefixed frames AND raw JSON fallback (if starts with '{' / 0x7b)
    if len_bytes[0] == b'{' {
        let mut data = len_bytes.to_vec();
        let mut rest = vec![0u8; 65536];
        if let Ok(n) = reader.read(&mut rest).await {
            data.extend_from_slice(&rest[..n]);
        }
        return Ok(Some(data));
    }

    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > 10 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC message exceeds 10MB limit",
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

pub async fn write_ipc_message<W: AsyncWriteExt + Unpin>(writer: &mut W, data: &[u8]) -> std::io::Result<()> {
    let len = (data.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(data).await?;
    writer.flush().await?;
    Ok(())
}

pub fn start_ipc_server(
    rt: Arc<tokio::runtime::Runtime>,
    state: AppState,
    task_notifier: mpsc::UnboundedSender<String>,
    egui_ctx: egui::Context,
) {
    // 1. Spawn Unix Domain Socket IPC Daemon
    let socket_path = get_ipc_socket_path();
    let state_unix = state.clone();
    let notifier_unix = task_notifier.clone();
    let ctx_unix = egui_ctx.clone();

    rt.spawn(async move {
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
                    let state = state_unix.clone();
                    let notifier = notifier_unix.clone();
                    let ctx = ctx_unix.clone();

                    tokio::spawn(async move {
                        match read_ipc_message(&mut stream).await {
                            Ok(Some(buf)) => {
                                if let Ok(req) = serde_json::from_slice::<IpcDownloadRequest>(&buf) {
                                    if req.action == "ping" {
                                        let resp = IpcResponse {
                                            status: "pong".to_string(),
                                            message: Some("Illusion IPC Daemon ready".to_string()),
                                            task_id: None,
                                            filename: None,
                                        };
                                        let res_bytes = serde_json::to_vec(&resp).unwrap_or_default();
                                        let _ = write_ipc_message(&mut stream, &res_bytes).await;
                                        return;
                                    }

                                    if let Some(raw_url) = req.url {
                                        let url = illusion_core::prober::clean_media_url(&raw_url);
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
                                        let mut task = DownloadTask::new(
                                            url,
                                            filename.clone(),
                                            save_path,
                                            None,
                                            headers,
                                            req.cookies,
                                            req.user_agent,
                                        );
                                        task.audio_url = req.audio_url.map(|a| illusion_core::prober::clean_media_url(&a));
                                        task.quality = req.quality;

                                        let task_id = task.id.clone();
                                        state.add_task(task).await;

                                        // Notify GUI to auto-start, bring window forward, and repaint immediately!
                                        let _ = notifier.send(task_id.clone());
                                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                                        ctx.request_repaint();

                                        let resp = IpcResponse {
                                            status: "success".to_string(),
                                            message: Some("Download queued in live Illusion GUI".to_string()),
                                            task_id: Some(task_id),
                                            filename: Some(filename),
                                        };
                                        let res_bytes = serde_json::to_vec(&resp).unwrap_or_default();
                                        let _ = write_ipc_message(&mut stream, &res_bytes).await;
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

    // 2. Spawn IDM-Grade Local HTTP/TCP Bridge Server on 127.0.0.1:9095
    let state_http = state;
    let notifier_http = task_notifier;
    let ctx_http = egui_ctx;

    rt.spawn(async move {
        let addr = format!("127.0.0.1:{}", HTTP_BRIDGE_PORT);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => {
                info!("⚡ Illusion IDM-Grade HTTP Bridge server bound at http://{}", addr);
                l
            }
            Err(e) => {
                warn!("Could not bind HTTP Bridge server to {}: {:#}", addr, e);
                return;
            }
        };

        loop {
            match listener.accept().await {
                Ok((stream, _peer_addr)) => {
                    let state = state_http.clone();
                    let notifier = notifier_http.clone();
                    let ctx = ctx_http.clone();

                    tokio::spawn(async move {
                        handle_http_client(stream, state, notifier, ctx).await;
                    });
                }
                Err(e) => {
                    error!("Error accepting HTTP Bridge connection: {:#}", e);
                }
            }
        }
    });
}

async fn handle_http_client(
    mut stream: TcpStream,
    state: AppState,
    notifier: mpsc::UnboundedSender<String>,
    ctx: egui::Context,
) {
    let mut buf = vec![0u8; 128 * 1024]; // 128KB buffer
    let mut total_read = 0;

    // Read initial request headers
    loop {
        match stream.read(&mut buf[total_read..]).await {
            Ok(0) => return,
            Ok(n) => {
                total_read += n;
                if buf[..total_read].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if total_read == buf.len() {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    let header_end = match buf[..total_read].windows(4).position(|w| w == b"\r\n\r\n") {
        Some(pos) => pos,
        None => return,
    };

    let header_str = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_str.lines();
    let request_line = match lines.next() {
        Some(l) => l,
        None => return,
    };

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1];

    // Common CORS headers for all browser extension calls
    let cors_headers = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nAccess-Control-Max-Age: 86400\r\n";

    // Handle CORS preflight
    if method == "OPTIONS" {
        let resp = format!(
            "HTTP/1.1 204 No Content\r\n{}Connection: close\r\n\r\n",
            cors_headers
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;
        return;
    }

    // Handle Health/Ping
    if method == "GET" && (path == "/ping" || path == "/health") {
        let body = r#"{"status":"pong","message":"Illusion HTTP Bridge ready","version":"0.1.0"}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
            body.len(),
            cors_headers,
            body
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;
        return;
    }

    // Handle Add Download
    if method == "POST" && (path == "/add_download" || path == "/") {
        // Parse Content-Length
        let mut content_length = 0;
        for line in lines {
            let lower = line.to_lowercase();
            if lower.starts_with("content-length:") {
                if let Some(val) = line.split(':').nth(1) {
                    content_length = val.trim().parse::<usize>().unwrap_or(0);
                }
            }
        }

        let body_start = header_end + 4;
        let mut body_bytes = buf[body_start..total_read].to_vec();

        // If body was not fully read yet, read remaining content_length bytes
        while body_bytes.len() < content_length {
            let mut chunk = vec![0u8; 8192];
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => body_bytes.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }

        if let Ok(req) = serde_json::from_slice::<IpcDownloadRequest>(&body_bytes) {
            if let Some(raw_url) = req.url {
                let url = illusion_core::prober::clean_media_url(&raw_url);
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
                let mut task = DownloadTask::new(
                    url,
                    filename.clone(),
                    save_path,
                    None,
                    headers,
                    req.cookies,
                    req.user_agent,
                );
                task.audio_url = req.audio_url.map(|a| illusion_core::prober::clean_media_url(&a));
                task.quality = req.quality;

                let task_id = task.id.clone();
                state.add_task(task).await;

                // Notify GUI & trigger instant repaint and viewport focus
                let _ = notifier.send(task_id.clone());
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();

                let resp_payload = IpcResponse {
                    status: "success".to_string(),
                    message: Some("Download queued in live Illusion GUI".to_string()),
                    task_id: Some(task_id),
                    filename: Some(filename),
                };

                let resp_body = serde_json::to_string(&resp_payload).unwrap_or_default();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                    resp_body.len(),
                    cors_headers,
                    resp_body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.flush().await;
                return;
            }
        }

        let err_body = r#"{"status":"error","message":"Invalid JSON payload or missing URL"}"#;
        let resp = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
            err_body.len(),
            cors_headers,
            err_body
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;
        return;
    }

    // 404 for unknown routes
    let not_found = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n{}Connection: close\r\n\r\n",
        cors_headers
    );
    let _ = stream.write_all(not_found.as_bytes()).await;
    let _ = stream.flush().await;
}

use anyhow::{anyhow, Result};
use illusion_core::{AppState, DownloadTask};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn default_action() -> String {
    "add_download".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeMessage {
    #[serde(default = "default_action")]
    action: String,
    url: Option<String>,
    filename: Option<String>,
    referer: Option<String>,
    cookies: Option<String>,
    user_agent: Option<String>,
    audio_url: Option<String>,
    quality: Option<String>,
    headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeResponse {
    status: String,
    message: Option<String>,
    task_id: Option<String>,
    filename: Option<String>,
}

fn get_ipc_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("illusion.sock")
    } else {
        let uid = std::process::id();
        PathBuf::from(format!("/tmp/illusion-{}.sock", uid))
    }
}

fn read_message<R: Read>(reader: &mut R) -> Result<Option<NativeMessage>> {
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(anyhow!("Failed to read message length: {}", e)),
    }

    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > 10 * 1024 * 1024 {
        return Err(anyhow!("Message too large: {} bytes", len));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;

    let msg = serde_json::from_slice::<NativeMessage>(&buf)?;
    Ok(Some(msg))
}

fn write_response<W: Write>(writer: &mut W, response: &NativeResponse) -> Result<()> {
    let json_bytes = serde_json::to_vec(response)?;
    let len = json_bytes.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&json_bytes)?;
    writer.flush()?;
    Ok(())
}

async fn try_forward_to_ipc_socket(msg: &NativeMessage) -> Result<Option<NativeResponse>> {
    let socket_path = get_ipc_socket_path();
    if !socket_path.exists() {
        return Ok(None);
    }

    let mut stream = match UnixStream::connect(&socket_path).await {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    let payload = serde_json::to_vec(msg)?;
    stream.write_all(&payload).await?;

    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf).await?;
    if n > 0 {
        if let Ok(resp) = serde_json::from_slice::<NativeResponse>(&buf[..n]) {
            return Ok(Some(resp));
        }
    }

    Ok(None)
}

#[tokio::main]
async fn main() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    let state = AppState::load_or_default();

    while let Ok(Some(msg)) = read_message(&mut reader) {
        // Try live Unix Domain Socket forwarding first
        if let Ok(Some(ipc_resp)) = try_forward_to_ipc_socket(&msg).await {
            let _ = write_response(&mut writer, &ipc_resp);
            continue;
        }

        // Fallback if GUI is not running
        match msg.action.as_str() {
            "ping" => {
                let resp = NativeResponse {
                    status: "pong".to_string(),
                    message: Some("Illusion Native Host 0.1.0 ready (standalone fallback)".to_string()),
                    task_id: None,
                    filename: None,
                };
                let _ = write_response(&mut writer, &resp);
            }
            "add_download" => {
                if let Some(url) = msg.url {
                    let mut filename = msg.filename.unwrap_or_default();
                    if filename.is_empty() {
                        filename = url.split('/').last().unwrap_or("download").to_string();
                    }
                    filename = illusion_core::prober::sanitize_filename(&filename);

                    let mut headers = msg.headers.unwrap_or_default();
                    if let Some(r) = msg.referer {
                        headers.insert("referer".to_string(), r);
                    }

                    let save_path = state.default_download_dir.join(&filename);
                    let mut task = DownloadTask::new(
                        url,
                        filename.clone(),
                        save_path,
                        None,
                        headers,
                        msg.cookies,
                        msg.user_agent,
                    );
                    task.audio_url = msg.audio_url;
                    task.quality = msg.quality;
                    let task_id = task.id.clone();
                    state.add_task(task).await;

                    // Spawn GUI if not already open with decoupled stdio so Chromium pipe never hangs
                    let exe_path = std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|dir| dir.join("illusion")))
                        .unwrap_or_else(|| PathBuf::from("illusion"));

                    let _ = Command::new(exe_path)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();

                    let resp = NativeResponse {
                        status: "success".to_string(),
                        message: Some("Download added to Illusion state & spawned GUI".to_string()),
                        task_id: Some(task_id),
                        filename: Some(filename),
                    };
                    let _ = write_response(&mut writer, &resp);
                } else {
                    let resp = NativeResponse {
                        status: "error".to_string(),
                        message: Some("Missing URL in download request".to_string()),
                        task_id: None,
                        filename: None,
                    };
                    let _ = write_response(&mut writer, &resp);
                }
            }
            _ => {
                let resp = NativeResponse {
                    status: "error".to_string(),
                    message: Some(format!("Unknown action: {}", msg.action)),
                    task_id: None,
                    filename: None,
                };
                let _ = write_response(&mut writer, &resp);
            }
        }
    }

    Ok(())
}

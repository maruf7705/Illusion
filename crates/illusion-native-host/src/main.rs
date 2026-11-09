use anyhow::{anyhow, Result};
use illusion_core::{AppState, DownloadTask, MediaCandidate, MediaCandidateKind, RequestContext};
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
    #[serde(alias = "audioUrl")]
    audio_url: Option<String>,
    quality: Option<String>,
    headers: Option<HashMap<String, String>>,
    #[serde(default, alias = "mediaCandidate")]
    media_candidate: Option<MediaCandidate>,
}

#[derive(Debug, Serialize, Deserialize)]
struct NativeResponse {
    status: String,
    message: Option<String>,
    task_id: Option<String>,
    filename: Option<String>,
}

fn build_download_task(msg: NativeMessage, download_dir: PathBuf) -> Result<DownloadTask> {
    let request_url = msg.url.clone().unwrap_or_default();
    let request_headers = msg.headers.clone().unwrap_or_default();
    let request_referer = msg.referer.clone();
    let request_cookies = msg.cookies.clone();
    let request_user_agent = msg.user_agent.clone();
    let request_audio_url = msg.audio_url.clone();
    let request_quality = msg.quality.clone();

    let mut candidate = msg.media_candidate.unwrap_or_else(|| {
        let mut headers = request_headers.clone();
        if let Some(referer) = request_referer.as_ref().filter(|value| !value.is_empty()) {
            headers.entry("referer".to_string()).or_insert_with(|| referer.clone());
        }
        MediaCandidate {
            id: None,
            pair_id: request_audio_url
                .as_ref()
                .map(|audio| format!("legacy:{}:{}", request_url, audio)),
            kind: if request_audio_url.is_some() {
                MediaCandidateKind::PairedAudioVideo
            } else {
                MediaCandidateKind::Progressive
            },
            page_url: request_referer.clone(),
            primary: RequestContext {
                url: request_url.clone(),
                headers,
                cookies: request_cookies.clone(),
                user_agent: request_user_agent.clone(),
                referer: request_referer.clone(),
                ..RequestContext::default()
            },
            audio: request_audio_url.clone().map(|url| RequestContext {
                url,
                headers: HashMap::new(),
                cookies: request_cookies.clone(),
                user_agent: request_user_agent.clone(),
                referer: request_referer.clone(),
                ..RequestContext::default()
            }),
            quality: request_quality.clone(),
            codecs: None,
            unsupported_reason: None,
        }
    });

    if candidate.primary.url.trim().is_empty() {
        return Err(anyhow!("Missing URL in download request"));
    }
    for context in std::iter::once(&candidate.primary).chain(candidate.audio.iter()) {
        illusion_core::prober::validate_media_response(
            &context.url,
            context.content_type.as_deref(),
        )?;
    }
    if candidate.kind == MediaCandidateKind::ProtectedOrUnsupported {
        return Err(anyhow!(candidate.unsupported_reason.unwrap_or_else(|| {
            "This protected media stream cannot be downloaded".to_string()
        })));
    }
    if candidate.kind == MediaCandidateKind::PairedAudioVideo
        && (candidate.audio.is_none()
            || candidate
                .pair_id
                .as_ref()
                .map_or(true, |pair_id| pair_id.trim().is_empty()))
    {
        return Err(anyhow!(
            "Paired media requires both an audio request and an explicit pair ID"
        ));
    }
    for (name, value) in request_headers {
        candidate.primary.headers.entry(name).or_insert(value);
    }
    if candidate.primary.cookies.is_none() {
        candidate.primary.cookies = request_cookies;
    }
    if candidate.primary.user_agent.is_none() {
        candidate.primary.user_agent = request_user_agent;
    }
    if candidate.primary.referer.is_none() {
        candidate.primary.referer = request_referer;
    }
    if candidate.quality.is_none() {
        candidate.quality = request_quality;
    }

    let mut filename = msg.filename.unwrap_or_default();
    if filename.is_empty() {
        filename = candidate
            .primary
            .url
            .split('/')
            .next_back()
            .unwrap_or("download")
            .split('?')
            .next()
            .unwrap_or("download")
            .to_string();
    }
    filename = candidate.output_filename(filename);
    filename = illusion_core::prober::sanitize_filename(&filename);
    Ok(DownloadTask::from_media_candidate(
        candidate,
        filename.clone(),
        download_dir.join(filename),
    ))
}

fn get_user_id() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

fn get_ipc_socket_path() -> PathBuf {
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
    let len_bytes = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len_bytes).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;

    let mut resp_len_bytes = [0u8; 4];
    stream.read_exact(&mut resp_len_bytes).await?;
    let resp_bytes = if resp_len_bytes[0] == b'{' {
        let mut data = resp_len_bytes.to_vec();
        let mut rest = vec![0u8; 65536];
        if let Ok(n) = stream.read(&mut rest).await {
            data.extend_from_slice(&rest[..n]);
        }
        data
    } else {
        let r_len = u32::from_be_bytes(resp_len_bytes) as usize;
        let mut buf = vec![0u8; r_len];
        stream.read_exact(&mut buf).await?;
        buf
    };

    if let Ok(resp) = serde_json::from_slice::<NativeResponse>(&resp_bytes) {
        return Ok(Some(resp));
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
                match build_download_task(msg, state.default_download_dir.clone()) {
                    Ok(task) => {
                    let filename = task.filename.clone();
                    let task_id = task.id.clone();
                    state.add_task(task).await;

                    // Spawn GUI if not already open with decoupled stdio and inherited desktop environment
                    let exe_path = std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|dir| dir.join("illusion")))
                        .unwrap_or_else(|| PathBuf::from("illusion"));

                    let mut cmd = Command::new(&exe_path);
                    for var in [
                        "WAYLAND_DISPLAY",
                        "DISPLAY",
                        "XDG_RUNTIME_DIR",
                        "XDG_CURRENT_DESKTOP",
                        "XDG_SESSION_TYPE",
                        "HOME",
                        "USER",
                    ] {
                        if let Ok(val) = std::env::var(var) {
                            cmd.env(var, val);
                        }
                    }

                    let log_dir = state.config_dir.clone();
                    let _ = std::fs::create_dir_all(&log_dir);
                    let log_file = log_dir.join("gui_spawn.log");
                    if let Ok(file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_file)
                    {
                        if let Ok(file_clone) = file.try_clone() {
                            cmd.stdout(file_clone);
                        } else {
                            cmd.stdout(std::process::Stdio::null());
                        }
                        cmd.stderr(file);
                    } else {
                        cmd.stdout(std::process::Stdio::null());
                        cmd.stderr(std::process::Stdio::null());
                    }
                    cmd.stdin(std::process::Stdio::null());
                    let _ = cmd.spawn();

                    let resp = NativeResponse {
                        status: "success".to_string(),
                        message: Some("Download added to Illusion state & spawned GUI".to_string()),
                        task_id: Some(task_id),
                        filename: Some(filename),
                    };
                    let _ = write_response(&mut writer, &resp);
                    }
                    Err(error) => {
                    let resp = NativeResponse {
                        status: "error".to_string(),
                        message: Some(error.to_string()),
                        task_id: None,
                        filename: None,
                    };
                    let _ = write_response(&mut writer, &resp);
                    }
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

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
use tokio::process::Command;
use tracing::info;

#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    pub stream_url: String,
    pub audio_url: Option<String>,
    pub filename: String,
    pub title: String,
    pub total_size: Option<u64>,
    pub headers: HashMap<String, String>,
    pub user_agent: Option<String>,
}

pub fn is_video_page_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("youtube.com/")
        || lower.contains("youtu.be/")
        || lower.contains("facebook.com/")
        || lower.contains("fb.watch/")
        || lower.contains("instagram.com/")
        || lower.contains("twitter.com/")
        || lower.contains("x.com/")
        || lower.contains("pinterest.com/")
        || lower.contains("tiktok.com/")
        || lower.contains("vimeo.com/")
}

pub async fn resolve_media_url(
    page_url: &str,
    preferred_format: Option<&str>,
) -> Result<ResolvedMedia> {
    info!("Resolving media stream for video page: {}", page_url);

    // Format selection:
    // Default to 18 (progressive 360p mp4) or best format under 360p, or user requested
    let format_arg = preferred_format
        .map(|s| s.to_string())
        .unwrap_or_else(|| "18/best[height<=360]/best".to_string());

    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "-j",
        "--no-playlist",
        "--extractor-args",
        "youtube:player_client=android",
        "-f",
        &format_arg,
        page_url,
    ]);

    let output = match cmd.output().await {
        Ok(out) => out,
        Err(e) => {
            return Err(anyhow!("Failed to execute stream resolver: {}", e));
        }
    };

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Stream resolution failed: {}", err_msg.trim()));
    }

    let json_text = String::from_utf8_lossy(&output.stdout);
    let v: Value = serde_json::from_str(&json_text)
        .map_err(|e| anyhow!("Failed to parse stream metadata JSON: {}", e))?;

    let stream_url = v
        .get("url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("No stream URL found in resolved metadata"))?
        .to_string();

    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("video")
        .to_string();

    let ext = v
        .get("ext")
        .and_then(|e| e.as_str())
        .unwrap_or("mp4");

    let clean_title = title.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
    let filename = format!("{}.{}", clean_title.trim_matches('_'), ext);

    let total_size = v
        .get("filesize")
        .and_then(|s| s.as_u64())
        .or_else(|| v.get("filesize_approx").and_then(|s| s.as_u64()));

    let mut headers = HashMap::new();
    let mut user_agent = None;

    if let Some(h) = v.get("http_headers").and_then(|h| h.as_object()) {
        for (k, val) in h {
            if let Some(val_str) = val.as_str() {
                if k.eq_ignore_ascii_case("user-agent") {
                    user_agent = Some(val_str.to_string());
                }
                headers.insert(k.clone(), val_str.to_string());
            }
        }
    }

    // Check if separate audio format is needed
    let mut audio_url = None;
    if let Some(req_fmts) = v.get("requested_formats").and_then(|f| f.as_array()) {
        for fmt in req_fmts {
            let vcodec = fmt.get("vcodec").and_then(|c| c.as_str()).unwrap_or("none");
            let acodec = fmt.get("acodec").and_then(|c| c.as_str()).unwrap_or("none");
            if vcodec == "none" && acodec != "none" {
                if let Some(u) = fmt.get("url").and_then(|u| u.as_str()) {
                    audio_url = Some(u.to_string());
                }
            }
        }
    }

    Ok(ResolvedMedia {
        stream_url,
        audio_url,
        filename,
        title,
        total_size,
        headers,
        user_agent,
    })
}

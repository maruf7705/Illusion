use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::HashMap;
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
    (lower.contains("youtube.com/") && !lower.contains("/videoplayback"))
        || lower.contains("youtu.be/")
        || (lower.contains("facebook.com/") && !lower.contains("fbcdn.net"))
        || lower.contains("fb.watch/")
        || lower.contains("instagram.com/")
        || lower.contains("twitter.com/")
        || lower.contains("x.com/")
        || lower.contains("pinterest.com/")
        || (lower.contains("tiktok.com/") && !lower.contains("tiktokcdn.com"))
        || (lower.contains("vimeo.com/") && !lower.contains("vimeocdn.com"))
        || lower.contains("dailymotion.com/")
        || lower.contains("dai.ly/")
        || lower.contains("vk.com/")
        || lower.contains("reddit.com/")
}

pub fn is_media_stream_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("googlevideo.com/videoplayback")
        || lower.contains("fbcdn.net")
        || lower.contains("tiktokcdn.com")
        || lower.contains("byteoversea.com")
        || lower.contains("vimeocdn.com")
        || lower.contains(".m3u8")
        || lower.contains(".mpd")
        || lower.contains(".mp4")
        || lower.contains(".webm")
        || lower.contains(".mkv")
        || lower.contains(".ts")
}

pub async fn resolve_media_url(
    page_url: &str,
    _preferred_format: Option<&str>,
) -> Result<ResolvedMedia> {
    info!("Resolving media stream natively (Pure Native / Zero External Tools) for: {}", page_url);

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // 1. If it's already a direct media stream URL (e.g. from browser extension or direct link)
    if is_media_stream_url(page_url) {
        let parsed_url = url::Url::parse(page_url)?;
        let title = parsed_url
            .query_pairs()
            .find(|(k, _)| k == "title" || k == "filename")
            .map(|(_, v)| v.to_string())
            .unwrap_or_else(|| {
                parsed_url
                    .path_segments()
                    .and_then(|s| s.last())
                    .unwrap_or("media_stream")
                    .to_string()
            });

        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string());
        if let Some(host) = parsed_url.host_str() {
            headers.insert("Host".to_string(), host.to_string());
        }

        let clean_filename = crate::prober::sanitize_filename(&title);
        return Ok(ResolvedMedia {
            stream_url: page_url.to_string(),
            audio_url: None,
            filename: if clean_filename.contains('.') { clean_filename } else { format!("{}.mp4", clean_filename) },
            title,
            total_size: None,
            headers,
            user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
        });
    }

    // 2. Fetch HTML page to inspect OpenGraph tags, HTML5 video tags, or embedded metadata
    let resp = client.get(page_url).send().await?;
    let html = resp.text().await?;

    // Extract title from og:title or <title>
    let mut title = "video".to_string();
    if let Some(caps) = html.lines().find_map(|line| {
        if line.contains("property=\"og:title\"") || line.contains("name=\"twitter:title\"") {
            let start = line.find("content=\"")?;
            let rest = &line[start + 9..];
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        } else {
            None
        }
    }) {
        title = caps;
    } else if let Some(start) = html.find("<title>") {
        let rest = &html[start + 7..];
        if let Some(end) = rest.find("</title>") {
            title = rest[..end].replace(" - YouTube", "").trim().to_string();
        }
    }

    // Check OpenGraph video stream URL
    let mut stream_url: Option<String> = None;
    for line in html.lines() {
        if (line.contains("property=\"og:video:secure_url\"")
            || line.contains("property=\"og:video:url\"")
            || line.contains("property=\"og:video\""))
            && line.contains("content=\"http")
        {
            if let Some(start) = line.find("content=\"") {
                let rest = &line[start + 9..];
                if let Some(end) = rest.find('"') {
                    let candidate = &rest[..end];
                    if candidate.contains(".mp4") || candidate.contains(".m3u8") || candidate.contains("fbcdn.net") {
                        stream_url = Some(candidate.to_string());
                        break;
                    }
                }
            }
        }
    }

    // Check YouTube ytInitialPlayerResponse in pure Rust
    if stream_url.is_none() && (page_url.contains("youtube.com") || page_url.contains("youtu.be")) {
        if let Some(start) = html.find("ytInitialPlayerResponse = {") {
            let rest = &html[start + 26..];
            if let Some(end) = rest.find("};") {
                let json_slice = &rest[..=end];
                if let Ok(v) = serde_json::from_str::<Value>(json_slice) {
                    if let Some(t) = v.pointer("/videoDetails/title").and_then(|s| s.as_str()) {
                        title = t.to_string();
                    }
                    if let Some(fmts) = v.pointer("/streamingData/formats").and_then(|f| f.as_array()) {
                        for fmt in fmts {
                            if let Some(u) = fmt.get("url").and_then(|u| u.as_str()) {
                                stream_url = Some(u.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(url) = stream_url {
        let clean_title = title.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
        let ext = if url.contains(".m3u8") { "mp4" } else { "mp4" };
        let filename = format!("{}.{}", clean_title.trim_matches('_'), ext);

        let mut headers = HashMap::new();
        headers.insert("Referer".to_string(), page_url.to_string());
        headers.insert("User-Agent".to_string(), "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string());

        return Ok(ResolvedMedia {
            stream_url: url,
            audio_url: None,
            filename,
            title,
            total_size: None,
            headers,
            user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
        });
    }

    Err(anyhow!(
        "Protected streaming video on '{}' requires active browser playback session.\n\
         👉 Like Internet Download Manager (IDM), open this video in your browser with the Illusion Browser Extension installed.\n\
         Illusion will intercept the live authenticated video stream, cookies, and tokens, and download natively with 8 connections.",
        page_url
    ))
}

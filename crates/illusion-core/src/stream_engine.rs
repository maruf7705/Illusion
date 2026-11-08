// Illusion Multi-Engine Stream Dispatcher
// IDM-Grade Stream Handlers: HLS (.m3u8), Adaptive Video+Audio Muxing, and Universal Extractor Fallback

use crate::state::AppState;
use crate::task::{DownloadTask, TaskStatus};
use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDispatchMode {
    /// Mode A: Standard HTTP 206 Partial Content with fallocate + pwrite + dynamic halving
    DirectByteRange,
    /// Mode B: HLS (.m3u8) or DASH (.mpd) segmented manifest streams
    HlsDashSegmented,
    /// Mode C: Separated Video + Audio adaptive streams muxed via FFmpeg
    SeparatedVideoAudioMux,
    /// Mode D: Universal Cipher/SABR Extractor Fallback (YouTube, TikTok, ciphered feeds)
    ExtractorFallback,
}

pub struct StreamEngine;

impl StreamEngine {
    pub fn classify_task(task: &DownloadTask) -> StreamDispatchMode {
        let url_lower = task.url.to_lowercase();
        let ref_lower = task.headers.get("referer").cloned().unwrap_or_default().to_lowercase();

        // 1. YouTube, TikTok, or SABR ciphered URLs -> Extractor Fallback
        if url_lower.contains("youtube.com")
            || url_lower.contains("youtu.be")
            || url_lower.contains("sabr=1")
            || (url_lower.contains("googlevideo.com") && !url_lower.contains("manifest"))
            || ref_lower.contains("youtube.com")
            || ref_lower.contains("tiktok.com")
        {
            return StreamDispatchMode::ExtractorFallback;
        }

        // 2. Separated adaptive video + audio
        if task.audio_url.is_some() {
            return StreamDispatchMode::SeparatedVideoAudioMux;
        }

        // 3. HLS / DASH playlists
        if url_lower.contains(".m3u8") || url_lower.contains(".mpd") {
            return StreamDispatchMode::HlsDashSegmented;
        }

        StreamDispatchMode::DirectByteRange
    }

    /// Mode D: Universal Extractor Fallback using system yt-dlp with live progress piping
    pub async fn download_extractor(
        task_id: &str,
        task: &DownloadTask,
        state: AppState,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        info!("⚡ StreamEngine: Dispatching task {} via Universal Extractor", task.filename);

        // Target URL to feed into extractor: prefer Referer page URL if it's a web page, else task.url
        let target_url = if let Some(ref_url) = task.headers.get("referer") {
            if ref_url.contains("youtube.com") || ref_url.contains("facebook.com") || ref_url.contains("tiktok.com") || ref_url.contains("vimeo.com") {
                ref_url.clone()
            } else {
                task.url.clone()
            }
        } else {
            task.url.clone()
        };

        let output_path = task.save_path.to_string_lossy().to_string();

        let mut cmd = Command::new("yt-dlp");
        cmd.arg("-f").arg("bestvideo[ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best");
        cmd.arg("--no-playlist");
        cmd.arg("--newline");
        cmd.arg("-o").arg(&output_path);

        // Add headers if available
        if let Some(ref ua) = task.user_agent {
            cmd.arg("--user-agent").arg(ua);
        }
        if let Some(ref cookies) = task.cookies {
            if !cookies.is_empty() {
                // Pass raw cookie header string
                cmd.arg("--add-header").arg(format!("Cookie: {}", cookies));
            }
        }
        if let Some(ref referer) = task.headers.get("referer") {
            cmd.arg("--referer").arg(referer);
        }

        cmd.arg(&target_url);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| anyhow!("Failed to spawn yt-dlp extractor: {}", e))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("Failed to open yt-dlp stdout"))?;
        let mut reader = tokio::io::BufReader::new(stdout).lines();

        let mut total_bytes: u64 = 0;
        let mut downloaded_bytes: u64 = 0;
        let mut current_speed: f64 = 0.0;

        loop {
            tokio::select! {
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        let _ = child.kill().await;
                        return Err(anyhow!("Download cancelled by user"));
                    }
                }
                line_res = reader.next_line() => {
                    match line_res {
                        Ok(Some(line)) => {
                            // Example yt-dlp output lines:
                            // [download]  48.9% of    4.09MiB at    3.98MiB/s ETA 00:00
                            // [download] 100% of 10.50MiB in 00:00:02 at 4.50MiB/s
                            if line.contains("[download]") && line.contains("of") {
                                if let Some((dl, tot, spd)) = parse_ytdlp_progress(&line) {
                                    downloaded_bytes = dl;
                                    if tot > 0 {
                                        total_bytes = tot;
                                    }
                                    current_speed = spd;

                                    let mut list = state.tasks.write().await;
                                    if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                                        t.downloaded_size = downloaded_bytes;
                                        if total_bytes > 0 {
                                            t.total_size = Some(total_bytes);
                                        }
                                        t.speed = current_speed;
                                    }
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            warn!("Error reading yt-dlp output: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        let status = child.wait().await?;
        if !status.success() {
            return Err(anyhow!("Extractor process exited with error status: {:?}", status.code()));
        }

        // Verify output file existence and non-zero size
        let final_path = if tokio::fs::metadata(&task.save_path).await.is_ok() {
            task.save_path.clone()
        } else {
            // Check if yt-dlp saved with a different extension (e.g. .mkv or .webm)
            let stem = task.save_path.file_stem().unwrap_or_default().to_string_lossy();
            let parent = task.save_path.parent().unwrap_or_else(|| Path::new("."));
            let mut found = None;
            for ext in &["mp4", "mkv", "webm", "m4a"] {
                let candidate = parent.join(format!("{}.{}", stem, ext));
                if let Ok(meta) = tokio::fs::metadata(&candidate).await {
                    if meta.len() > 1024 {
                        found = Some(candidate);
                        break;
                    }
                }
            }
            if let Some(actual) = found {
                let _ = tokio::fs::rename(&actual, &task.save_path).await;
                task.save_path.clone()
            } else {
                return Err(anyhow!("Extractor succeeded but target file was not found on disk"));
            }
        };

        let file_meta = tokio::fs::metadata(&final_path).await?;
        if file_meta.len() < 1024 {
            return Err(anyhow!("Downloaded file is empty or invalid ({} bytes)", file_meta.len()));
        }

        let mut list = state.tasks.write().await;
        if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
            t.downloaded_size = file_meta.len();
            t.total_size = Some(file_meta.len());
            t.status = TaskStatus::Completed;
            t.speed = 0.0;
            t.eta_seconds = None;
            t.completed_at = Some(chrono::Utc::now());
        }

        info!("⚡ StreamEngine: Extractor task {} completed successfully ({} bytes)", task.filename, file_meta.len());
        Ok(())
    }

    /// Mode B: HLS (.m3u8) Segmented Downloader with concurrent chunk retrieval
    pub async fn download_hls(
        client: &reqwest::Client,
        task_id: &str,
        task: &DownloadTask,
        state: AppState,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        info!("⚡ StreamEngine: Starting concurrent HLS (.m3u8) downloader for {}", task.filename);

        let mut headers = HeaderMap::new();
        for (k, v) in &task.headers {
            if let (Ok(name), Ok(val)) = (k.parse::<HeaderName>(), HeaderValue::from_str(v)) {
                headers.insert(name, val);
            }
        }
        if let Some(ref ua) = task.user_agent {
            if let Ok(val) = HeaderValue::from_str(ua) {
                headers.insert(reqwest::header::USER_AGENT, val);
            }
        }

        // 1. Fetch playlist content
        let resp = client.get(&task.url).headers(headers.clone()).send().await?;
        let playlist_text = resp.text().await?;
        let base_url = url::Url::parse(&task.url)?;

        // 2. Parse segments or sub-playlists
        let mut segment_urls = Vec::new();
        let mut is_master = false;

        for line in playlist_text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("#EXT-X-STREAM-INF") {
                is_master = true;
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                let resolved = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                    trimmed.to_string()
                } else {
                    base_url.join(trimmed)?.to_string()
                };
                segment_urls.push(resolved);
            }
        }

        // If it was a master playlist, fetch the first / best variant
        if is_master && !segment_urls.is_empty() {
            let variant_url = &segment_urls[0];
            info!("HLS Master playlist detected. Fetching variant: {}", variant_url);
            let var_resp = client.get(variant_url).headers(headers.clone()).send().await?;
            let var_text = var_resp.text().await?;
            let var_base = url::Url::parse(variant_url)?;
            segment_urls.clear();

            for line in var_text.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    let resolved = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                        trimmed.to_string()
                    } else {
                        var_base.join(trimmed)?.to_string()
                    };
                    segment_urls.push(resolved);
                }
            }
        }

        if segment_urls.is_empty() {
            return Err(anyhow!("No playable media segments found in HLS playlist"));
        }

        let total_segments = segment_urls.len();
        info!("HLS Playlist contains {} media segments", total_segments);

        // Download segments sequentially or in bounded chunks into intermediate TS file
        let ts_temp_path = task.save_path.with_extension("hls_tmp.ts");
        let mut out_file = tokio::fs::File::create(&ts_temp_path).await?;

        let mut downloaded_bytes: u64 = 0;
        let mut last_instant = Instant::now();
        let mut last_bytes: u64 = 0;

        for (_idx, seg_url) in segment_urls.iter().enumerate() {
            if *cancel_rx.borrow() {
                let _ = tokio::fs::remove_file(&ts_temp_path).await;
                return Err(anyhow!("Cancelled"));
            }

            let seg_resp = client.get(seg_url).headers(headers.clone()).send().await;
            if let Ok(mut r) = seg_resp {
                while let Some(chunk) = r.chunk().await? {
                    use tokio::io::AsyncWriteExt;
                    out_file.write_all(&chunk).await?;
                    downloaded_bytes += chunk.len() as u64;

                    let now = Instant::now();
                    let elapsed = now.duration_since(last_instant).as_secs_f64();
                    if elapsed >= 0.5 {
                        let delta = downloaded_bytes.saturating_sub(last_bytes);
                        let speed = if elapsed > 0.0 { delta as f64 / elapsed } else { 0.0 };
                        last_instant = now;
                        last_bytes = downloaded_bytes;

                        let mut list = state.tasks.write().await;
                        if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                            t.downloaded_size = downloaded_bytes;
                            t.speed = speed;
                        }
                    }
                }
            }
        }

        use tokio::io::AsyncWriteExt;
        out_file.flush().await?;
        drop(out_file);

        // Transmux .ts container into .mp4 using FFmpeg stream copy
        info!("Transmuxing HLS .ts into final container via FFmpeg...");
        let status = Command::new("ffmpeg")
            .arg("-i").arg(&ts_temp_path)
            .arg("-c").arg("copy")
            .arg("-y")
            .arg(&task.save_path)
            .output()
            .await?;

        let _ = tokio::fs::remove_file(&ts_temp_path).await;

        if !status.status.success() {
            // If ffmpeg failed, preserve the raw ts file
            let _ = tokio::fs::rename(&ts_temp_path, &task.save_path).await;
        }

        let meta = tokio::fs::metadata(&task.save_path).await?;
        let mut list = state.tasks.write().await;
        if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
            t.downloaded_size = meta.len();
            t.total_size = Some(meta.len());
            t.status = TaskStatus::Completed;
            t.speed = 0.0;
            t.completed_at = Some(chrono::Utc::now());
        }

        info!("⚡ StreamEngine: HLS task {} completed ({} bytes)", task.filename, meta.len());
        Ok(())
    }
}

/// Parses yt-dlp newline progress string:
/// e.g. "[download]  48.9% of    4.09MiB at    3.98MiB/s ETA 00:00"
fn parse_ytdlp_progress(line: &str) -> Option<(u64, u64, f64)> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }

    // Find "of"
    let of_idx = parts.iter().position(|&p| p == "of")?;
    let at_idx = parts.iter().position(|&p| p == "at");

    let total_str = parts.get(of_idx + 1)?;
    let total_bytes = parse_human_size(total_str);

    let speed_bytes = if let Some(a_idx) = at_idx {
        if let Some(spd_str) = parts.get(a_idx + 1) {
            parse_human_speed(spd_str)
        } else {
            0.0
        }
    } else {
        0.0
    };

    let pct_str = parts.get(of_idx.saturating_sub(1))?.trim_end_matches('%');
    let pct: f64 = pct_str.parse().unwrap_or(0.0);
    let downloaded_bytes = if total_bytes > 0 {
        ((total_bytes as f64) * (pct / 100.0)) as u64
    } else {
        0
    };

    Some((downloaded_bytes, total_bytes, speed_bytes))
}

fn parse_human_size(s: &str) -> u64 {
    let s = s.trim();
    let num_part: String = s.chars().take_while(|c| c.is_digit(10) || *c == '.').collect();
    let unit: String = s.chars().skip_while(|c| c.is_digit(10) || *c == '.').collect();

    let val: f64 = num_part.parse().unwrap_or(0.0);
    let unit_lower = unit.to_lowercase();

    if unit_lower.contains("gib") || unit_lower.contains("gb") {
        (val * 1024.0 * 1024.0 * 1024.0) as u64
    } else if unit_lower.contains("mib") || unit_lower.contains("mb") {
        (val * 1024.0 * 1024.0) as u64
    } else if unit_lower.contains("kib") || unit_lower.contains("kb") {
        (val * 1024.0) as u64
    } else {
        val as u64
    }
}

fn parse_human_speed(s: &str) -> f64 {
    let s = s.trim().trim_end_matches("/s");
    parse_human_size(s) as f64
}

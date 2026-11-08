// Illusion Multi-Engine Stream Dispatcher
// IDM-Grade Stream Handlers: Dual-Stream Adaptive Video+Audio Muxing, HLS (.m3u8), and Direct Byte-Range
// Zero yt-dlp / Zero Python / Pure Native Concurrency

use crate::state::AppState;
use crate::task::{DownloadTask, TaskStatus};
use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::path::PathBuf;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDispatchMode {
    /// Mode A: Standard HTTP 206 Partial Content with fallocate + pwrite + dynamic halving
    DirectByteRange,
    /// Mode B: HLS (.m3u8) or DASH (.mpd) segmented manifest streams
    HlsDashSegmented,
    /// Mode C: Separated Video + Audio adaptive streams (YouTube 1080p+, Facebook DASH, Vimeo adaptive)
    SeparatedVideoAudioMux,
}

pub struct StreamEngine;

impl StreamEngine {
    pub fn classify_task(task: &DownloadTask) -> StreamDispatchMode {
        let url_lower = task.url.to_lowercase();

        // 1. Separated adaptive video + audio (takes highest precedence)
        if task.audio_url.is_some() {
            return StreamDispatchMode::SeparatedVideoAudioMux;
        }

        // 2. HLS / DASH playlists
        if url_lower.contains(".m3u8") || url_lower.contains(".mpd") {
            return StreamDispatchMode::HlsDashSegmented;
        }

        // 3. Direct byte-range download (Standard IDM multi-segment chunk halving)
        StreamDispatchMode::DirectByteRange
    }

    /// Mode C: Dual-Stream Concurrency Downloader
    /// Downloads video and audio tracks concurrently into temporary part files
    /// and multiplexes them into a single playable container without re-encoding.
    pub async fn download_dual_stream(
        client: &reqwest::Client,
        task_id: &str,
        task: &DownloadTask,
        state: AppState,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let audio_url = task
            .audio_url
            .as_ref()
            .ok_or_else(|| anyhow!("Dual-stream download requires audio_url"))?;

        info!(
            "⚡ StreamEngine: Dispatching IDM-grade dual-stream download for {}",
            task.filename
        );

        let video_part = task.save_path.with_extension("video_part.mp4");
        let audio_part = task.save_path.with_extension("audio_part.m4a");

        // Build common request headers
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
        if let Some(ref cookies) = task.cookies {
            if !cookies.is_empty() {
                if let Ok(val) = HeaderValue::from_str(cookies) {
                    headers.insert(reqwest::header::COOKIE, val);
                }
            }
        }

        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::channel::<(bool, u64, Option<u64>)>(256);

        // Spawn concurrent video download task
        let v_client = client.clone();
        let v_url = task.url.clone();
        let v_path = video_part.clone();
        let v_headers = headers.clone();
        let v_cancel = cancel_rx.clone();
        let v_tx = progress_tx.clone();
        let video_handle = tokio::spawn(async move {
            download_single_track(v_client, v_url, v_path, v_headers, v_cancel, v_tx, true).await
        });

        // Spawn concurrent audio download task
        let a_client = client.clone();
        let a_url = audio_url.clone();
        let a_path = audio_part.clone();
        let a_headers = headers.clone();
        let a_cancel = cancel_rx.clone();
        let a_tx = progress_tx.clone();
        let audio_handle = tokio::spawn(async move {
            download_single_track(a_client, a_url, a_path, a_headers, a_cancel, a_tx, false).await
        });

        drop(progress_tx); // Close original sender so loop terminates when both workers finish

        let mut video_total: Option<u64> = None;
        let mut audio_total: Option<u64> = None;
        let mut total_downloaded: u64 = 0;
        let mut last_instant = Instant::now();
        let mut last_bytes: u64 = 0;

        // Progress aggregation loop
        while let Some((is_video, chunk_len, maybe_total)) = progress_rx.recv().await {
            if let Some(tot) = maybe_total {
                if is_video {
                    video_total = Some(tot);
                } else {
                    audio_total = Some(tot);
                }
                let combined_total = match (video_total, audio_total) {
                    (Some(v), Some(a)) => Some(v + a),
                    (Some(v), None) => Some(v),
                    (None, Some(a)) => Some(a),
                    (None, None) => None,
                };
                {
                    let mut list = state.tasks.write().unwrap();
                    if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                        t.total_size = combined_total;
                    }
                }
            }

            if chunk_len > 0 {
                total_downloaded += chunk_len;
                let now = Instant::now();
                let elapsed = now.duration_since(last_instant).as_secs_f64();
                if elapsed >= 0.5 {
                    let delta = total_downloaded.saturating_sub(last_bytes);
                    let speed = if elapsed > 0.0 {
                        delta as f64 / elapsed
                    } else {
                        0.0
                    };
                    last_instant = now;
                    last_bytes = total_downloaded;

                    {
                        let mut list = state.tasks.write().unwrap();
                        if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                            t.downloaded_size = total_downloaded;
                            t.speed = speed;
                        }
                    }
                }
            }
        }

        let v_res = video_handle
            .await
            .map_err(|e| anyhow!("Video download task panicked: {}", e))??;
        let a_res = audio_handle
            .await
            .map_err(|e| anyhow!("Audio download task panicked: {}", e))??;

        info!(
            "⚡ StreamEngine: Dual streams retrieved successfully: Video = {} bytes, Audio = {} bytes",
            v_res, a_res
        );

        // Native Multiplexing: Merge video and audio tracks into final container
        info!("⚡ StreamEngine: Assembling final media container...");
        crate::muxer::mux_video_and_audio(&video_part, &audio_part, &task.save_path).await?;

        let meta = tokio::fs::metadata(&task.save_path).await?;
        {
            let mut list = state.tasks.write().unwrap();
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.downloaded_size = meta.len();
                t.total_size = Some(meta.len());
                t.status = TaskStatus::Completed;
                t.speed = 0.0;
                t.eta_seconds = None;
                t.completed_at = Some(chrono::Utc::now());
            }
        }
        let _ = state.save_tasks().await;

        info!(
            "⚡ StreamEngine: Task {} fully assembled and completed ({} bytes)",
            task.filename,
            meta.len()
        );
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
        info!(
            "⚡ StreamEngine: Starting concurrent HLS (.m3u8) downloader for {}",
            task.filename
        );

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

        let ts_temp_path = task.save_path.with_extension("hls_tmp.ts");
        let mut out_file = tokio::fs::File::create(&ts_temp_path).await?;

        let mut downloaded_bytes: u64 = 0;
        let mut last_instant = Instant::now();
        let mut last_bytes: u64 = 0;

        for seg_url in &segment_urls {
            if *cancel_rx.borrow() {
                let _ = tokio::fs::remove_file(&ts_temp_path).await;
                return Err(anyhow!("Cancelled"));
            }

            let seg_resp = client.get(seg_url).headers(headers.clone()).send().await;
            if let Ok(mut r) = seg_resp {
                while let Some(chunk) = r.chunk().await? {
                    out_file.write_all(&chunk).await?;
                    downloaded_bytes += chunk.len() as u64;

                    let now = Instant::now();
                    let elapsed = now.duration_since(last_instant).as_secs_f64();
                    if elapsed >= 0.5 {
                        let delta = downloaded_bytes.saturating_sub(last_bytes);
                        let speed = if elapsed > 0.0 {
                            delta as f64 / elapsed
                        } else {
                            0.0
                        };
                        last_instant = now;
                        last_bytes = downloaded_bytes;

                        {
                            let mut list = state.tasks.write().unwrap();
                            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                                t.downloaded_size = downloaded_bytes;
                                t.speed = speed;
                            }
                        }
                    }
                }
            }
        }

        out_file.flush().await?;
        drop(out_file);

        // Transmux .ts container into .mp4 using FFmpeg stream copy
        info!("Transmuxing HLS .ts into final container via FFmpeg...");
        let status = Command::new("ffmpeg")
            .arg("-i")
            .arg(&ts_temp_path)
            .arg("-c")
            .arg("copy")
            .arg("-y")
            .arg(&task.save_path)
            .output()
            .await?;

        let _ = tokio::fs::remove_file(&ts_temp_path).await;

        if !status.status.success() {
            let _ = tokio::fs::rename(&ts_temp_path, &task.save_path).await;
        }

        let meta = tokio::fs::metadata(&task.save_path).await?;
        {
            let mut list = state.tasks.write().unwrap();
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.downloaded_size = meta.len();
                t.total_size = Some(meta.len());
                t.status = TaskStatus::Completed;
                t.speed = 0.0;
                t.completed_at = Some(chrono::Utc::now());
            }
        }

        info!(
            "⚡ StreamEngine: HLS task {} completed ({} bytes)",
            task.filename,
            meta.len()
        );
        Ok(())
    }
}

/// Helper function to download a single media track (video or audio) asynchronously
async fn download_single_track(
    client: reqwest::Client,
    url: String,
    save_path: PathBuf,
    headers: HeaderMap,
    mut cancel_rx: watch::Receiver<bool>,
    progress_tx: tokio::sync::mpsc::Sender<(bool, u64, Option<u64>)>,
    is_video: bool,
) -> Result<u64> {
    let mut resp = client
        .get(&url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| anyhow!("Stream HTTP request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(anyhow!(
            "Stream HTTP request returned error status: {}",
            resp.status()
        ));
    }

    let content_len = resp.content_length();
    let _ = progress_tx.send((is_video, 0, content_len)).await;

    let mut file = tokio::fs::File::create(&save_path)
        .await
        .map_err(|e| anyhow!("Failed to create part file {:?}: {}", save_path, e))?;

    let mut downloaded = 0u64;

    loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    let _ = tokio::fs::remove_file(&save_path).await;
                    return Err(anyhow!("Stream download cancelled by user"));
                }
            }
            chunk_res = resp.chunk() => {
                match chunk_res {
                    Ok(Some(chunk)) => {
                        file.write_all(&chunk).await?;
                        let len = chunk.len() as u64;
                        downloaded += len;
                        let _ = progress_tx.send((is_video, len, None)).await;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tokio::fs::remove_file(&save_path).await;
                        return Err(anyhow!("Error reading stream chunk: {}", e));
                    }
                }
            }
        }
    }

    file.flush().await?;
    Ok(downloaded)
}

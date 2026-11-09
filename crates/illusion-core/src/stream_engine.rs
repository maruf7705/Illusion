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

fn strip_query_param(url: &str, param: &str) -> String {
    let mut res = url.to_string();
    let p_amp = format!("&{}=", param);
    let p_q = format!("?{}=", param);

    // Replace &param=val
    while let Some(pos) = res.find(&p_amp) {
        let rest = &res[pos + p_amp.len()..];
        let end = rest.find('&').map(|e| pos + p_amp.len() + e).unwrap_or(res.len());
        res.replace_range(pos..end, "");
    }

    // Replace ?param=val
    if let Some(pos) = res.find(&p_q) {
        let rest = &res[pos + p_q.len()..];
        if let Some(amp_idx) = rest.find('&') {
            let end = pos + p_q.len() + amp_idx;
            // ?param=val&other -> ?other
            res.replace_range(pos + 1..end + 1, "");
        } else {
            // ?param=val at end of url
            res.truncate(pos);
        }
    }
    res
}

pub fn clean_stream_url(raw_url: &str) -> String {
    let lower = raw_url.to_lowercase();
    if lower.contains("googlevideo.com") || lower.contains("videoplayback") {
        // Extract sparams to know what is protected by HMAC
        let sparams = if let Some(idx) = raw_url.find("sparams=") {
            let rest = &raw_url[idx + 8..];
            let end = rest.find('&').unwrap_or(rest.len());
            rest[..end].to_string()
        } else {
            String::new()
        };
        let sparams_list: Vec<&str> = sparams.split("%2C").flat_map(|s| s.split(',')).collect();

        let mut url = raw_url.to_string();

        // 1. Convert sig to signature if signature is not already present (IDM VA 0x00427af1)
        if (url.contains("?sig=") || url.contains("&sig=")) && !url.contains("signature=") {
            url = url.replace("?sig=", "?signature=");
            url = url.replace("&sig=", "&signature=");
        }

        // 2. Strip non-signed range and slicing params (IDM VA 0x004277c0)
        let strip_candidates = ["range", "begin", "ump", "len", "sq", "segment", "frag", "sabr", "rqh"];
        for param in strip_candidates {
            if !sparams_list.contains(&param) {
                url = strip_query_param(&url, param);
            }
        }
        return url;
    }

    if lower.contains("fbcdn.net") || lower.contains("cdninstagram.com") || lower.contains("facebook.com") {
        let mut url = raw_url.to_string();
        url = strip_query_param(&url, "bytestart");
        url = strip_query_param(&url, "byteend");
        return url;
    }

    raw_url.to_string()
}

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
        let v_url = clean_stream_url(&task.url);
        let v_path = video_part.clone();
        let v_headers = headers.clone();
        let v_cancel = cancel_rx.clone();
        let v_tx = progress_tx.clone();
        let video_handle = tokio::spawn(async move {
            download_single_track(v_client, v_url, v_path, v_headers, v_cancel, v_tx, true).await
        });

        // Spawn concurrent audio download task
        let a_client = client.clone();
        let a_url = clean_stream_url(audio_url);
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

        if v_res == 0 {
            return Err(anyhow!("Video stream downloaded 0 bytes - download aborted"));
        }
        if a_res == 0 {
            return Err(anyhow!("Audio stream downloaded 0 bytes - download aborted"));
        }

        info!(
            "⚡ StreamEngine: Dual streams retrieved successfully: Video = {} bytes, Audio = {} bytes",
            v_res, a_res
        );

        // Native Multiplexing: Merge video and audio tracks into final container
        info!("⚡ StreamEngine: Assembling final media container...");
        crate::muxer::mux_video_and_audio(&video_part, &audio_part, &task.save_path).await?;

        let meta = tokio::fs::metadata(&task.save_path).await?;
        if meta.len() == 0 {
            return Err(anyhow!("Assembled media container has 0 bytes - aborting completion"));
        }

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

    /// Mode B: HLS (.m3u8) IDM-Grade Segmented Downloader with fMP4 (#EXT-X-MAP) & Master Variant selection
    pub async fn download_hls(
        client: &reqwest::Client,
        task_id: &str,
        task: &DownloadTask,
        state: AppState,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        info!(
            "⚡ StreamEngine: Starting IDM-grade HLS (.m3u8) downloader for {}",
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

        struct HlsVariant {
            bandwidth: u64,
            uri: String,
            audio_group: Option<String>,
        }

        struct HlsAudioTrack {
            group_id: String,
            uri: String,
        }

        let mut variants: Vec<HlsVariant> = Vec::new();
        let mut audio_tracks: Vec<HlsAudioTrack> = Vec::new();
        let mut is_master = false;

        let lines: Vec<&str> = playlist_text.lines().map(|l| l.trim()).collect();
        let mut idx = 0;
        while idx < lines.len() {
            let line = lines[idx];
            if line.starts_with("#EXT-X-STREAM-INF:") {
                is_master = true;
                let info = &line["#EXT-X-STREAM-INF:".len()..];
                let mut bandwidth: u64 = 0;
                let mut audio_group: Option<String> = None;

                for part in info.split(',') {
                    let kv: Vec<&str> = part.splitn(2, '=').collect();
                    if kv.len() == 2 {
                        let k = kv[0].trim();
                        let v = kv[1].trim().trim_matches('"');
                        if k == "BANDWIDTH" {
                            bandwidth = v.parse::<u64>().unwrap_or(0);
                        } else if k == "AUDIO" {
                            audio_group = Some(v.to_string());
                        }
                    }
                }

                let mut j = idx + 1;
                while j < lines.len() && (lines[j].is_empty() || lines[j].starts_with('#')) {
                    j += 1;
                }
                if j < lines.len() {
                    let uri = lines[j];
                    let resolved = if uri.starts_with("http://") || uri.starts_with("https://") {
                        uri.to_string()
                    } else {
                        base_url.join(uri)?.to_string()
                    };
                    variants.push(HlsVariant {
                        bandwidth,
                        uri: resolved,
                        audio_group,
                    });
                    idx = j;
                }
            } else if line.starts_with("#EXT-X-MEDIA:") && line.contains("TYPE=AUDIO") {
                is_master = true;
                let info = &line["#EXT-X-MEDIA:".len()..];
                let mut group_id = String::new();
                let mut uri = String::new();

                for part in info.split(',') {
                    let kv: Vec<&str> = part.splitn(2, '=').collect();
                    if kv.len() == 2 {
                        let k = kv[0].trim();
                        let v = kv[1].trim().trim_matches('"');
                        if k == "GROUP-ID" {
                            group_id = v.to_string();
                        } else if k == "URI" {
                            uri = v.to_string();
                        }
                    }
                }
                if !uri.is_empty() {
                    let resolved = if uri.starts_with("http://") || uri.starts_with("https://") {
                        uri.to_string()
                    } else {
                        base_url.join(&uri)?.to_string()
                    };
                    audio_tracks.push(HlsAudioTrack {
                        group_id,
                        uri: resolved,
                    });
                }
            }
            idx += 1;
        }

        let (video_playlist_url, maybe_audio_url) = if is_master && !variants.is_empty() {
            variants.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
            let best = &variants[0];
            let matched_audio = best.audio_group.as_ref().and_then(|grp| {
                audio_tracks.iter().find(|a| &a.group_id == grp).map(|a| a.uri.clone())
            });
            info!("HLS Master: selected variant (bandwidth {}): {}", best.bandwidth, best.uri);
            (best.uri.clone(), matched_audio)
        } else {
            (task.url.clone(), None)
        };

        // 2. Parse media playlist segments and init map
        let parse_playlist = |playlist_str: &str, playlist_base: &url::Url| -> (Option<String>, Vec<String>, bool) {
            let mut init_url = None;
            let mut segments = Vec::new();
            let mut is_fmp4 = false;

            for line in playlist_str.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("#EXT-X-MAP:") {
                    is_fmp4 = true;
                    if let Some(uri_idx) = trimmed.find("URI=\"") {
                        let rest = &trimmed[uri_idx + 5..];
                        if let Some(end_idx) = rest.find('"') {
                            let map_uri = &rest[..end_idx];
                            let resolved = if map_uri.starts_with("http://") || map_uri.starts_with("https://") {
                                map_uri.to_string()
                            } else if let Ok(u) = playlist_base.join(map_uri) {
                                u.to_string()
                            } else {
                                map_uri.to_string()
                            };
                            init_url = Some(resolved);
                        }
                    }
                } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    let resolved = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                        trimmed.to_string()
                    } else if let Ok(u) = playlist_base.join(trimmed) {
                        u.to_string()
                    } else {
                        trimmed.to_string()
                    };
                    if resolved.contains(".m4s") || resolved.contains(".mp4") {
                        is_fmp4 = true;
                    }
                    segments.push(resolved);
                }
            }
            (init_url, segments, is_fmp4)
        };

        let v_resp = client.get(&video_playlist_url).headers(headers.clone()).send().await?;
        let v_text = v_resp.text().await?;
        let v_base = url::Url::parse(&video_playlist_url)?;
        let (v_init, v_segs, v_fmp4) = parse_playlist(&v_text, &v_base);

        if v_segs.is_empty() {
            return Err(anyhow!("No playable media segments found in HLS playlist"));
        }

        let total_segments = v_segs.len();
        info!("HLS Playlist contains {} segments (fMP4={})", total_segments, v_fmp4);

        if let Some(audio_url) = maybe_audio_url {
            info!("HLS: downloading separated video and audio tracks...");
            let a_resp = client.get(&audio_url).headers(headers.clone()).send().await?;
            let a_text = a_resp.text().await?;
            let a_base = url::Url::parse(&audio_url)?;
            let (a_init, a_segs, _a_fmp4) = parse_playlist(&a_text, &a_base);

            let temp_video = task.save_path.with_extension("hls_v.tmp");
            let temp_audio = task.save_path.with_extension("hls_a.tmp");

            // Download Video track
            {
                let mut v_file = tokio::fs::OpenOptions::new()
                    .create(true).write(true).truncate(true).open(&temp_video).await?;
                if let Some(init_url) = v_init {
                    let init_b = client.get(&init_url).headers(headers.clone()).send().await?.bytes().await?;
                    v_file.write_all(&init_b).await?;
                }
                for s in &v_segs {
                    if *cancel_rx.borrow() { return Err(anyhow!("Cancelled")); }
                    let b = client.get(s).headers(headers.clone()).send().await?.bytes().await?;
                    v_file.write_all(&b).await?;
                }
                v_file.flush().await?;
            }

            // Download Audio track
            {
                let mut a_file = tokio::fs::OpenOptions::new()
                    .create(true).write(true).truncate(true).open(&temp_audio).await?;
                if let Some(init_url) = a_init {
                    let init_b = client.get(&init_url).headers(headers.clone()).send().await?.bytes().await?;
                    a_file.write_all(&init_b).await?;
                }
                for s in &a_segs {
                    if *cancel_rx.borrow() { return Err(anyhow!("Cancelled")); }
                    let b = client.get(s).headers(headers.clone()).send().await?.bytes().await?;
                    a_file.write_all(&b).await?;
                }
                a_file.flush().await?;
            }

            // Transmux video + audio with FFmpeg
            let status = Command::new("ffmpeg")
                .arg("-i").arg(&temp_video)
                .arg("-i").arg(&temp_audio)
                .arg("-c").arg("copy")
                .arg("-bsf:a").arg("aac_adtstoasc")
                .arg("-y").arg(&task.save_path)
                .output().await;

            let ok = status.as_ref().map(|s| s.status.success()).unwrap_or(false);
            if !ok {
                // Retry without bsf filter
                let _ = Command::new("ffmpeg")
                    .arg("-i").arg(&temp_video)
                    .arg("-i").arg(&temp_audio)
                    .arg("-c").arg("copy")
                    .arg("-y").arg(&task.save_path)
                    .output().await;
            }

            let _ = tokio::fs::remove_file(&temp_video).await;
            let _ = tokio::fs::remove_file(&temp_audio).await;
        } else {
            // Single track or multiplexed fMP4 / MPEG-TS
            let temp_part = task.save_path.with_extension("hls_tmp.part");
            let mut out_file = tokio::fs::OpenOptions::new()
                .create(true).write(true).truncate(true).open(&temp_part).await?;

            let mut downloaded_bytes: u64 = 0;
            let mut last_instant = Instant::now();
            let mut last_bytes: u64 = 0;

            if let Some(init_url) = v_init {
                let init_b = client.get(&init_url).headers(headers.clone()).send().await?.bytes().await?;
                out_file.write_all(&init_b).await?;
                downloaded_bytes += init_b.len() as u64;
            }

            for seg_url in &v_segs {
                if *cancel_rx.borrow() {
                    let _ = out_file.flush().await;
                    return Err(anyhow!("Cancelled"));
                }

                if let Ok(mut r) = client.get(seg_url).headers(headers.clone()).send().await {
                    while let Some(chunk) = r.chunk().await? {
                        out_file.write_all(&chunk).await?;
                        downloaded_bytes += chunk.len() as u64;

                        let now = Instant::now();
                        let elapsed = now.duration_since(last_instant).as_secs_f64();
                        if elapsed >= 0.5 {
                            let delta = downloaded_bytes.saturating_sub(last_bytes);
                            let speed = if elapsed > 0.0 { delta as f64 / elapsed } else { 0.0 };
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

            if v_fmp4 {
                // Fragmented MP4 (Twitter / X, TikTok, Vimeo fMP4):
                // Init + m4s concatenation is already valid MP4!
                // Fast transmux copy to optimize container; fallback to rename if ffmpeg absent
                let ff_res = Command::new("ffmpeg")
                    .arg("-i").arg(&temp_part)
                    .arg("-c").arg("copy")
                    .arg("-y").arg(&task.save_path)
                    .output().await;

                if ff_res.map(|s| s.status.success()).unwrap_or(false) {
                    let _ = tokio::fs::remove_file(&temp_part).await;
                } else {
                    let _ = tokio::fs::rename(&temp_part, &task.save_path).await;
                }
            } else {
                // MPEG-TS (.ts segments):
                let ff_res = Command::new("ffmpeg")
                    .arg("-i").arg(&temp_part)
                    .arg("-c").arg("copy")
                    .arg("-bsf:a").arg("aac_adtstoasc")
                    .arg("-y").arg(&task.save_path)
                    .output().await;

                if ff_res.map(|s| s.status.success()).unwrap_or(false) {
                    let _ = tokio::fs::remove_file(&temp_part).await;
                } else {
                    let _ = tokio::fs::rename(&temp_part, &task.save_path).await;
                }
            }
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
        let _ = state.save_tasks().await;

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
    mut headers: HeaderMap,
    mut cancel_rx: watch::Receiver<bool>,
    progress_tx: tokio::sync::mpsc::Sender<(bool, u64, Option<u64>)>,
    is_video: bool,
) -> Result<u64> {
    let clean_url = clean_stream_url(&url);
    let is_googlevideo = clean_url.contains("googlevideo.com") || clean_url.contains("videoplayback");

    // IDM Standard: Strip browser internal & routing headers that corrupt CDN routing
    headers.remove(reqwest::header::HOST);
    headers.remove(reqwest::header::CONTENT_LENGTH);
    headers.remove(reqwest::header::ORIGIN);
    headers.remove("sec-fetch-mode");
    headers.remove("sec-fetch-site");
    headers.remove("sec-fetch-dest");

    // Check existing bytes for resumption (IDM-grade stream recovery)
    let existing_len = tokio::fs::metadata(&save_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    // IDM Standard: Media streaming endpoints (like YouTube GVS CDN) STRICTLY require Range header!
    // A bare GET without Range triggers 403 Forbidden with application/vnd.yt-ump.
    let range_header = if existing_len > 0 {
        format!("bytes={}-", existing_len)
    } else {
        "bytes=0-".to_string()
    };
    if let Ok(range_val) = reqwest::header::HeaderValue::from_str(&range_header) {
        headers.insert(reqwest::header::RANGE, range_val);
    }

    // IDM sends Referer: https://www.youtube.com/ for googlevideo streams, but does not force Origin
    if is_googlevideo && !headers.contains_key(reqwest::header::REFERER) {
        if let Ok(val) = reqwest::header::HeaderValue::from_str("https://www.youtube.com/") {
            headers.insert(reqwest::header::REFERER, val);
        }
    }

    let mut resp = client
        .get(&clean_url)
        .headers(headers.clone())
        .send()
        .await
        .map_err(|e| anyhow!("Stream HTTP request failed: {}", e))?;

    let mut status = resp.status();
    if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
        // If 403 on googlevideo, try IDM CDN fallback via 'mn' parameter (IDM VA 0x00567c91)
        if status == reqwest::StatusCode::FORBIDDEN && is_googlevideo {
            if let Ok(u) = url::Url::parse(&clean_url) {
                let mn = u.query_pairs().find(|(k, _)| k == "mn").map(|(_, v)| v.to_string());
                if let Some(mn_val) = mn {
                    let servers: Vec<&str> = mn_val.split(',').collect();
                    for srv in servers {
                        let new_host = format!("{}.googlevideo.com", srv);
                        if let Some(curr_h) = u.host_str() {
                            if !curr_h.contains(srv) {
                                let fb_url = clean_url.replace(curr_h, &new_host);
                                info!("GVS 403 node fallback: retrying on {}", new_host);
                                if let Ok(fb_resp) = client.get(&fb_url).headers(headers.clone()).send().await {
                                    if fb_resp.status().is_success() || fb_resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                                        resp = fb_resp;
                                        status = resp.status();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(anyhow!(
                "Stream HTTP request returned error status: {}",
                status
            ));
        }
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if content_type.contains("application/vnd.yt-ump") {
        return Err(anyhow!("Stream request returned UMP protocol error"));
    }

    let is_resume_ok = existing_len > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut downloaded = if is_resume_ok { existing_len } else { 0 };

    let total_len = if let Some(cl) = resp.content_length() {
        if is_resume_ok {
            Some(existing_len + cl)
        } else {
            Some(cl)
        }
    } else {
        None
    };
    let _ = progress_tx.send((is_video, downloaded, total_len)).await;

    let mut file = if is_resume_ok {
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&save_path)
            .await
            .map_err(|e| anyhow!("Failed to open part file {:?} for append: {}", save_path, e))?
    } else {
        tokio::fs::File::create(&save_path)
            .await
            .map_err(|e| anyhow!("Failed to create part file {:?}: {}", save_path, e))?
    };

    loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    let _ = file.flush().await;
                    // Preserve partial part file on Pause - DO NOT DELETE!
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
                        let _ = file.flush().await;
                        // Preserve partial bytes on network drop
                        return Err(anyhow!("Error reading stream chunk: {}", e));
                    }
                }
            }
        }
    }

    file.flush().await?;
    if downloaded < 1024 {
        return Err(anyhow!("Stream track download completed with only {} bytes (invalid or error payload)", downloaded));
    }
    Ok(downloaded)
}

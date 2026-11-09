// Illusion Multi-Engine Stream Dispatcher
// IDM-Grade Stream Handlers: Dual-Stream Adaptive Video+Audio Muxing, HLS (.m3u8), MPEG-DASH (.mpd), and Direct Byte-Range
// Zero yt-dlp / Zero Python / Pure Native Concurrency

use crate::media::MediaCandidateKind;
use crate::prober::build_request_headers;
use crate::state::AppState;
use crate::task::{DownloadTask, TaskStatus};
use anyhow::{anyhow, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest::header::{HeaderMap, HeaderValue, RANGE};
use std::path::PathBuf;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;
use tracing::{info, warn};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDispatchMode {
    /// Mode A: Standard HTTP 206 Partial Content with fallocate + pwrite + dynamic halving
    DirectByteRange,
    /// Mode B1: HLS (.m3u8) segmented manifest streams
    HlsSegmented,
    /// Mode B2: MPEG-DASH (.mpd) segmented manifest streams
    DashSegmented,
    /// Mode C: Separated Video + Audio adaptive streams (YouTube 1080p+, Facebook DASH, Vimeo adaptive)
    SeparatedVideoAudioMux,
    /// The browser identified a protected or unavailable playback source.
    ProtectedOrUnsupported,
}

pub struct StreamEngine;

pub fn clean_stream_url(raw_url: &str) -> String {
    // The captured URL is a signed request capability.  Parameters such as
    // `range` and `ump` must not be rewritten; our independent HTTP Range
    // header is the only range control the downloader adds.
    raw_url.to_string()
}

/// Resolves a manifest child using standard relative URL resolution.  A
/// parent playlist query is not automatically valid for a child request.
pub fn resolve_url_preserving_query(base: &Url, rel: &str) -> String {
    let trimmed = rel.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
    if let Ok(resolved) = base.join(trimmed) {
        resolved.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parses an ISO 8601 duration string (e.g. "PT1H2M30.5S" or "PT90S") into seconds.
pub fn parse_iso8601_duration(d: &str) -> Option<f64> {
    let trimmed = d.trim();
    if !trimmed.starts_with('P') {
        return None;
    }
    let s = trimmed.trim_start_matches('P').trim_start_matches('T');
    let mut total = 0.0;
    let mut num_str = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num_str.push(ch);
        } else {
            let val = num_str.parse::<f64>().unwrap_or(0.0);
            num_str.clear();
            match ch {
                'H' => total += val * 3600.0,
                'M' => total += val * 60.0,
                'S' => total += val,
                'D' => total += val * 86400.0,
                _ => {}
            }
        }
    }
    Some(total)
}

/// Formats an MPEG-DASH template identifier string according to ISO/IEC 23009-1.
pub fn format_dash_template(
    template: &str,
    rep_id: &str,
    number: Option<u64>,
    time: Option<u64>,
    bandwidth: Option<u64>,
) -> String {
    let mut result = template.replace("$$", "\x01");
    result = result.replace("$RepresentationID$", rep_id);

    if let Some(bw) = bandwidth {
        result = result.replace("$Bandwidth$", &bw.to_string());
    }

    if let Some(num) = number {
        while let Some(start) = result.find("$Number%") {
            if let Some(rel_end) = result[start + 1..].find('$') {
                let end = start + 1 + rel_end;
                let spec = result[start..=end].to_string();
                let inner = spec.trim_start_matches("$Number%").trim_end_matches("d$").trim_end_matches('$');
                let inner = inner.trim_start_matches('0');
                let width = inner.parse::<usize>().unwrap_or(1);
                let formatted = format!("{:0width$}", num, width = width);
                result = result.replace(&spec, &formatted);
            } else {
                break;
            }
        }
        result = result.replace("$Number$", &num.to_string());
    }

    if let Some(t) = time {
        while let Some(start) = result.find("$Time%") {
            if let Some(rel_end) = result[start + 1..].find('$') {
                let end = start + 1 + rel_end;
                let spec = result[start..=end].to_string();
                let inner = spec.trim_start_matches("$Time%").trim_end_matches("d$").trim_end_matches('$');
                let inner = inner.trim_start_matches('0');
                let width = inner.parse::<usize>().unwrap_or(1);
                let formatted = format!("{:0width$}", t, width = width);
                result = result.replace(&spec, &formatted);
            } else {
                break;
            }
        }
        result = result.replace("$Time$", &t.to_string());
    }

    result.replace("\x01", "$")
}

#[derive(Debug, Clone)]
pub struct MediaChunk {
    pub url: String,
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Default)]
struct DashSegmentTemplateData {
    initialization: Option<String>,
    media: Option<String>,
    timescale: u64,
    start_number: u64,
    duration: u64,
    timeline: Vec<(Option<u64>, u64, Option<u64>)>, // (t, d, r)
}

#[derive(Debug, Clone)]
struct DashRepresentationData {
    id: String,
    bandwidth: u64,
    segment_template: Option<DashSegmentTemplateData>,
    initialization_url: Option<String>,
    initialization_range: Option<(u64, u64)>,
    segment_urls: Vec<MediaChunk>,
    base_url: Option<String>,
}

#[derive(Debug, Clone)]
struct DashAdaptationSetData {
    is_video: bool,
    is_audio: bool,
    segment_template: Option<DashSegmentTemplateData>,
    representations: Vec<DashRepresentationData>,
}

impl StreamEngine {
    pub fn classify_task(task: &DownloadTask) -> StreamDispatchMode {
        // Candidate kind is the browser/native protocol.  URL heuristics are
        // retained only for tasks written by older extension versions.
        if task.media_candidate.is_some() {
            return match task.candidate_kind() {
                MediaCandidateKind::Progressive => StreamDispatchMode::DirectByteRange,
                MediaCandidateKind::PairedAudioVideo => StreamDispatchMode::SeparatedVideoAudioMux,
                MediaCandidateKind::HlsManifest => StreamDispatchMode::HlsSegmented,
                MediaCandidateKind::DashManifest => StreamDispatchMode::DashSegmented,
                MediaCandidateKind::ProtectedOrUnsupported => {
                    StreamDispatchMode::ProtectedOrUnsupported
                }
            };
        }
        let url_lower = task.url.to_lowercase();
        let audio_url_lower = task.audio_url.as_ref().map(|s| s.to_lowercase());

        // 1. MPEG-DASH (.mpd) manifests (manifest inspection takes precedence over dual-stream)
        if url_lower.contains(".mpd")
            || task
                .headers
                .get("content-type")
                .map_or(false, |ct| ct.contains("dash+xml"))
            || audio_url_lower.as_ref().map_or(false, |s| s.contains(".mpd"))
        {
            return StreamDispatchMode::DashSegmented;
        }

        // 2. HLS (.m3u8) playlists (manifest inspection takes precedence over dual-stream)
        if url_lower.contains(".m3u8")
            || task
                .headers
                .get("content-type")
                .map_or(false, |ct| ct.contains("mpegurl"))
            || audio_url_lower.as_ref().map_or(false, |s| s.contains(".m3u8"))
        {
            return StreamDispatchMode::HlsSegmented;
        }

        // 3. Separated adaptive video + audio (takes precedence over direct byte-range)
        if task.audio_url.is_some() {
            return StreamDispatchMode::SeparatedVideoAudioMux;
        }

        // 4. Direct byte-range download (Standard IDM multi-segment chunk halving)
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
        let video_request = task.primary_request();
        let audio_request = task
            .audio_request()
            .ok_or_else(|| anyhow!("Dual-stream download requires an independently captured audio request"))?;

        info!(
            "⚡ StreamEngine: Dispatching IDM-grade dual-stream download for {}",
            task.filename
        );

        // Missing response metadata remains unknown until a valid response
        // probe or the completed download establishes the byte count.
        let v_clen = video_request.content_length;
        let a_clen = audio_request.content_length;
        if let (Some(v), Some(a)) = (v_clen, a_clen) {
            let mut list = state.tasks.write().unwrap();
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.total_size = Some(v + a);
            }
        }

        let video_part = task.save_path.with_extension("video_part.mp4");
        let audio_part = task.save_path.with_extension("audio_part.m4a");

        let video_headers = build_request_headers(
            &video_request.normalized_headers(),
            video_request.cookies.as_deref(),
            video_request.user_agent.as_deref(),
        );
        let audio_headers = build_request_headers(
            &audio_request.normalized_headers(),
            audio_request.cookies.as_deref(),
            audio_request.user_agent.as_deref(),
        );

        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::channel::<(bool, u64, Option<u64>)>(256);

        // Spawn concurrent video download task
        let v_client = client.clone();
        let v_url = video_request.url;
        let v_path = video_part.clone();
        let v_headers = video_headers;
        let v_cancel = cancel_rx.clone();
        let v_tx = progress_tx.clone();
        let video_handle = tokio::spawn(async move {
            download_single_track(v_client, v_url, v_path, v_headers, v_cancel, v_tx, true).await
        });

        // Spawn concurrent audio download task
        let a_client = client.clone();
        let a_url = audio_request.url;
        let a_path = audio_part.clone();
        let a_headers = audio_headers;
        let a_cancel = cancel_rx.clone();
        let a_tx = progress_tx.clone();
        let audio_handle = tokio::spawn(async move {
            download_single_track(a_client, a_url, a_path, a_headers, a_cancel, a_tx, false).await
        });

        drop(progress_tx); // Close original sender so loop terminates when both workers finish

        let mut video_total: Option<u64> = v_clen;
        let mut audio_total: Option<u64> = a_clen;
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
                    _ => None,
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

    /// Mode B1: HLS (.m3u8) IDM-Grade Segmented Downloader with fMP4 (#EXT-X-MAP), Byterange support, and Master Variant selection
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

        let primary_request = task.primary_request();
        let headers = build_request_headers(
            &primary_request.normalized_headers(),
            primary_request.cookies.as_deref(),
            primary_request.user_agent.as_deref(),
        );

        // 1. Fetch playlist content
        let resp = client
            .get(&primary_request.url)
            .headers(headers.clone())
            .send()
            .await?
            .error_for_status()?;
        let playlist_text = resp.text().await?;
        if playlist_text.lines().any(|line| {
            let line = line.trim().to_ascii_uppercase();
            line.starts_with("#EXT-X-KEY:") && !line.contains("METHOD=NONE")
        }) {
            return Err(anyhow!("Encrypted HLS streams are not supported"));
        }
        let base_url = Url::parse(&primary_request.url)?;

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
                    let resolved = resolve_url_preserving_query(&base_url, uri);
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
                    let resolved = resolve_url_preserving_query(&base_url, &uri);
                    audio_tracks.push(HlsAudioTrack {
                        group_id,
                        uri: resolved,
                    });
                }
            }
            idx += 1;
        }

        let mut selected_bandwidth: u64 = 0;
        let (video_playlist_url, maybe_audio_url) = if is_master && !variants.is_empty() {
            variants.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
            let best = &variants[0];
            selected_bandwidth = best.bandwidth;
            let matched_audio = best.audio_group.as_ref().and_then(|grp| {
                audio_tracks
                    .iter()
                    .find(|a| &a.group_id == grp)
                    .map(|a| a.uri.clone())
            });
            info!(
                "HLS Master: selected variant (bandwidth {}): {}",
                best.bandwidth, best.uri
            );
            (best.uri.clone(), matched_audio)
        } else {
            (task.url.clone(), None)
        };

        // Explicit audio_url (e.g. from Floating Bar / CLI dual-stream) takes precedence over master audio group
        let maybe_audio_url = task.audio_url.clone().or(maybe_audio_url);

        // 2. Parse media playlist segments and init map with byterange support
        let parse_playlist = |playlist_str: &str,
                              playlist_base: &Url|
         -> (Option<MediaChunk>, Vec<MediaChunk>, bool, f64) {
            let mut init_chunk: Option<MediaChunk> = None;
            let mut segments: Vec<MediaChunk> = Vec::new();
            let mut is_fmp4 = false;
            let mut total_duration = 0.0;
            let mut current_range: Option<(u64, u64)> = None;
            let mut current_offset: u64 = 0;

            for line in playlist_str.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("#EXTINF:") {
                    let dur_str = trimmed["#EXTINF:".len()..]
                        .split(',')
                        .next()
                        .unwrap_or("0");
                    if let Ok(d) = dur_str.trim().parse::<f64>() {
                        total_duration += d;
                    }
                } else if trimmed.starts_with("#EXT-X-BYTERANGE:") {
                    let spec = trimmed["#EXT-X-BYTERANGE:".len()..].trim();
                    if let Some(at_idx) = spec.find('@') {
                        let len = spec[..at_idx].parse::<u64>().unwrap_or(0);
                        let off = spec[at_idx + 1..].parse::<u64>().unwrap_or(0);
                        current_offset = off + len;
                        current_range = Some((off, off + len.saturating_sub(1)));
                    } else if let Ok(len) = spec.parse::<u64>() {
                        let off = current_offset;
                        current_offset += len;
                        current_range = Some((off, off + len.saturating_sub(1)));
                    }
                } else if trimmed.starts_with("#EXT-X-MAP:") {
                    is_fmp4 = true;
                    let info = &trimmed["#EXT-X-MAP:".len()..];
                    let mut map_uri = "";
                    let mut map_range: Option<(u64, u64)> = None;

                    for part in info.split(',') {
                        let kv: Vec<&str> = part.splitn(2, '=').collect();
                        if kv.len() == 2 {
                            let k = kv[0].trim();
                            let v = kv[1].trim().trim_matches('"');
                            if k == "URI" {
                                map_uri = v;
                            } else if k == "BYTERANGE" {
                                if let Some(at) = v.find('@') {
                                    let len = v[..at].parse::<u64>().unwrap_or(0);
                                    let off = v[at + 1..].parse::<u64>().unwrap_or(0);
                                    map_range = Some((off, off + len.saturating_sub(1)));
                                }
                            }
                        }
                    }

                    if !map_uri.is_empty() {
                        let resolved = resolve_url_preserving_query(playlist_base, map_uri);
                        init_chunk = Some(MediaChunk {
                            url: resolved,
                            range: map_range,
                        });
                    }
                } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    let resolved = resolve_url_preserving_query(playlist_base, trimmed);
                    if resolved.contains(".m4s") || resolved.contains(".mp4") {
                        is_fmp4 = true;
                    }
                    segments.push(MediaChunk {
                        url: resolved,
                        range: current_range,
                    });
                    current_range = None; // Reset for next segment
                }
            }
            (init_chunk, segments, is_fmp4, total_duration)
        };

        let v_resp = client
            .get(&video_playlist_url)
            .headers(headers.clone())
            .send()
            .await?
            .error_for_status()?;
        let v_text = v_resp.text().await?;
        if v_text.lines().any(|line| {
            let line = line.trim().to_ascii_uppercase();
            line.starts_with("#EXT-X-KEY:") && !line.contains("METHOD=NONE")
        }) {
            return Err(anyhow!("Encrypted HLS streams are not supported"));
        }
        let v_base = Url::parse(&video_playlist_url)?;
        let (v_init, v_segs, v_fmp4, v_duration) = parse_playlist(&v_text, &v_base);

        if v_segs.is_empty() {
            return Err(anyhow!("No playable media segments found in HLS playlist"));
        }

        let total_segments = v_segs.len();
        info!(
            "HLS Playlist contains {} segments (duration: {:.1}s, fMP4: {})",
            total_segments, v_duration, v_fmp4
        );

        // Estimate total file size upfront so GUI displays meaningful size
        if selected_bandwidth > 0 && v_duration > 0.0 {
            let estimated_total = (selected_bandwidth as f64 * v_duration / 8.0) as u64;
            let mut list = state.tasks.write().unwrap();
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.total_size = Some(estimated_total);
            }
        }

        if let Some(audio_url) = maybe_audio_url {
            info!("HLS: downloading separated video and audio tracks...");
            let a_resp = client
                .get(&audio_url)
                .headers(headers.clone())
                .send()
                .await?
                .error_for_status()?;
            let a_text = a_resp.text().await?;
            if a_text.lines().any(|line| {
                let line = line.trim().to_ascii_uppercase();
                line.starts_with("#EXT-X-KEY:") && !line.contains("METHOD=NONE")
            }) {
                return Err(anyhow!("Encrypted HLS streams are not supported"));
            }
            let a_base = Url::parse(&audio_url)?;
            let (a_init, a_segs, a_fmp4, _a_duration) = parse_playlist(&a_text, &a_base);

            let temp_video = task.save_path.with_extension("hls_v.tmp");
            let temp_audio = task.save_path.with_extension("hls_a.tmp");

            let mut downloaded_bytes: u64 = 0;
            let mut last_instant = Instant::now();
            let mut last_bytes: u64 = 0;

            // Download Video track
            {
                let mut v_file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&temp_video)
                    .await?;
                if let Some(init_chunk) = v_init {
                    let mut req = client.get(&init_chunk.url).headers(headers.clone());
                    if let Some((s, e)) = init_chunk.range {
                        req = req.header(RANGE, format!("bytes={}-{}", s, e));
                    }
                    let init_b = req.send().await?.error_for_status()?.bytes().await?;
                    v_file.write_all(&init_b).await?;
                    downloaded_bytes += init_b.len() as u64;
                }
                for s in &v_segs {
                    if *cancel_rx.borrow() {
                        return Err(anyhow!("Cancelled"));
                    }
                    let mut req = client.get(&s.url).headers(headers.clone());
                    if let Some((start, end)) = s.range {
                        req = req.header(RANGE, format!("bytes={}-{}", start, end));
                    }
                    let mut r = req.send().await?.error_for_status()?;
                    while let Some(chunk) = r.chunk().await? {
                            v_file.write_all(&chunk).await?;
                            downloaded_bytes += chunk.len() as u64;

                            let now = Instant::now();
                            let elapsed = now.duration_since(last_instant).as_secs_f64();
                            if elapsed >= 0.5 {
                                let delta = downloaded_bytes.saturating_sub(last_bytes);
                                let speed = if elapsed > 0.0 { delta as f64 / elapsed } else { 0.0 };
                                last_instant = now;
                                last_bytes = downloaded_bytes;
                                let mut list = state.tasks.write().unwrap();
                                if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                                    t.downloaded_size = downloaded_bytes;
                                    t.speed = speed;
                                }
                            }
                    }
                }
                v_file.flush().await?;
            }

            // Download Audio track
            {
                let mut a_file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&temp_audio)
                    .await?;
                if let Some(init_chunk) = a_init {
                    let mut req = client.get(&init_chunk.url).headers(headers.clone());
                    if let Some((s, e)) = init_chunk.range {
                        req = req.header(RANGE, format!("bytes={}-{}", s, e));
                    }
                    let init_b = req.send().await?.error_for_status()?.bytes().await?;
                    a_file.write_all(&init_b).await?;
                    downloaded_bytes += init_b.len() as u64;
                }
                for s in &a_segs {
                    if *cancel_rx.borrow() {
                        return Err(anyhow!("Cancelled"));
                    }
                    let mut req = client.get(&s.url).headers(headers.clone());
                    if let Some((start, end)) = s.range {
                        req = req.header(RANGE, format!("bytes={}-{}", start, end));
                    }
                    let mut r = req.send().await?.error_for_status()?;
                    while let Some(chunk) = r.chunk().await? {
                            a_file.write_all(&chunk).await?;
                            downloaded_bytes += chunk.len() as u64;

                            let now = Instant::now();
                            let elapsed = now.duration_since(last_instant).as_secs_f64();
                            if elapsed >= 0.5 {
                                let delta = downloaded_bytes.saturating_sub(last_bytes);
                                let speed = if elapsed > 0.0 { delta as f64 / elapsed } else { 0.0 };
                                last_instant = now;
                                last_bytes = downloaded_bytes;
                                let mut list = state.tasks.write().unwrap();
                                if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                                    t.downloaded_size = downloaded_bytes;
                                    t.speed = speed;
                                }
                            }
                    }
                }
                a_file.flush().await?;
            }

            // Transmux video + audio with FFmpeg
            // Only apply -bsf:a aac_adtstoasc if input was MPEG-TS (not fMP4)
            let mut cmd = Command::new("ffmpeg");
            cmd.arg("-i")
                .arg(&temp_video)
                .arg("-i")
                .arg(&temp_audio)
                .arg("-c")
                .arg("copy");
            if !v_fmp4 && !a_fmp4 {
                cmd.arg("-bsf:a").arg("aac_adtstoasc");
            }
            cmd.arg("-y").arg(&task.save_path);

            let status = cmd.output().await;
            let ok = status.as_ref().map(|s| s.status.success()).unwrap_or(false);
            if !ok {
                return Err(anyhow!("HLS audio/video muxing failed"));
            }

            let _ = tokio::fs::remove_file(&temp_video).await;
            let _ = tokio::fs::remove_file(&temp_audio).await;

            let meta = tokio::fs::metadata(&task.save_path).await?;
            if meta.len() == 0 {
                return Err(anyhow!("Muxed HLS file has 0 bytes - muxing failed"));
            }
        } else {
            // Single track or multiplexed fMP4 / MPEG-TS
            let temp_part = task.save_path.with_extension("hls_tmp.part");
            let mut out_file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temp_part)
                .await?;

            let mut downloaded_bytes: u64 = 0;
            let mut last_instant = Instant::now();
            let mut last_bytes: u64 = 0;

            if let Some(init_chunk) = v_init {
                let mut req = client.get(&init_chunk.url).headers(headers.clone());
                if let Some((s, e)) = init_chunk.range {
                    req = req.header(RANGE, format!("bytes={}-{}", s, e));
                }
                let init_b = req.send().await?.error_for_status()?.bytes().await?;
                out_file.write_all(&init_b).await?;
                downloaded_bytes += init_b.len() as u64;
            }

            for seg in &v_segs {
                if *cancel_rx.borrow() {
                    let _ = out_file.flush().await;
                    return Err(anyhow!("Cancelled"));
                }

                let mut req = client.get(&seg.url).headers(headers.clone());
                if let Some((start, end)) = seg.range {
                    req = req.header(RANGE, format!("bytes={}-{}", start, end));
                }

                let mut r = req.send().await?.error_for_status()?;
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

            out_file.flush().await?;
            drop(out_file);

            if v_fmp4 {
                let ff_res = Command::new("ffmpeg")
                    .arg("-i")
                    .arg(&temp_part)
                    .arg("-c")
                    .arg("copy")
                    .arg("-y")
                    .arg(&task.save_path)
                    .output()
                    .await;

                if ff_res.map(|s| s.status.success()).unwrap_or(false) {
                    let _ = tokio::fs::remove_file(&temp_part).await;
                } else {
                    let _ = tokio::fs::rename(&temp_part, &task.save_path).await;
                }
            } else {
                let ff_res = Command::new("ffmpeg")
                    .arg("-i")
                    .arg(&temp_part)
                    .arg("-c")
                    .arg("copy")
                    .arg("-bsf:a")
                    .arg("aac_adtstoasc")
                    .arg("-y")
                    .arg(&task.save_path)
                    .output()
                    .await;

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

    /// Mode B2: MPEG-DASH (.mpd) IDM-Grade Segmented Downloader
    /// Parses MPD XML manifest, extracts video and audio tracks, generates segment lists,
    /// downloads concurrently and multiplexes into final container.
    pub async fn download_dash(
        client: &reqwest::Client,
        task_id: &str,
        task: &DownloadTask,
        state: AppState,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        info!(
            "⚡ StreamEngine: Starting IDM-grade MPEG-DASH (.mpd) downloader for {}",
            task.filename
        );

        let primary_request = task.primary_request();
        let headers = build_request_headers(
            &primary_request.normalized_headers(),
            primary_request.cookies.as_deref(),
            primary_request.user_agent.as_deref(),
        );

        let resp = client
            .get(&primary_request.url)
            .headers(headers.clone())
            .send()
            .await?
            .error_for_status()?;
        let manifest_text = resp.text().await?;
        if manifest_text.to_ascii_lowercase().contains("contentprotection") {
            return Err(anyhow!("DRM-protected DASH streams are not supported"));
        }
        let base_url = Url::parse(&primary_request.url)?;

        // Parse MPD XML using quick-xml
        let mut reader = Reader::from_str(&manifest_text);
        reader.trim_text(true);

        let mut media_duration = 0.0;
        let mut root_base_url: Option<String> = None;
        let mut adaptation_sets: Vec<DashAdaptationSetData> = Vec::new();

        let mut current_adaptation_set: Option<DashAdaptationSetData> = None;
        let mut current_representation: Option<DashRepresentationData> = None;
        let mut current_template: Option<DashSegmentTemplateData> = None;
        let mut in_timeline = false;
        let mut inside_tag = String::new();

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    inside_tag = tag_name.clone();

                    match tag_name.as_str() {
                        "MPD" => {
                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                if key == "mediaPresentationDuration" {
                                    let val = String::from_utf8_lossy(attr.value.as_ref());
                                    if let Some(dur) = parse_iso8601_duration(&val) {
                                        media_duration = dur;
                                    }
                                }
                            }
                        }
                        "AdaptationSet" => {
                            let mut is_video = false;
                            let mut is_audio = false;
                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(attr.value.as_ref()).to_lowercase();
                                if key == "contentType" {
                                    if val == "video" {
                                        is_video = true;
                                    } else if val == "audio" {
                                        is_audio = true;
                                    }
                                } else if key == "mimeType" {
                                    if val.starts_with("video/") {
                                        is_video = true;
                                    } else if val.starts_with("audio/") {
                                        is_audio = true;
                                    }
                                }
                            }
                            current_adaptation_set = Some(DashAdaptationSetData {
                                is_video,
                                is_audio,
                                segment_template: None,
                                representations: Vec::new(),
                            });
                        }
                        "Representation" => {
                            let mut id = String::new();
                            let mut bandwidth: u64 = 0;
                            let mut is_rep_video = false;
                            let mut is_rep_audio = false;

                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(attr.value.as_ref());
                                if key == "id" {
                                    id = val.to_string();
                                } else if key == "bandwidth" {
                                    bandwidth = val.parse::<u64>().unwrap_or(0);
                                } else if key == "mimeType" {
                                    if val.starts_with("video/") {
                                        is_rep_video = true;
                                    } else if val.starts_with("audio/") {
                                        is_rep_audio = true;
                                    }
                                }
                            }

                            if let Some(ref mut ad) = current_adaptation_set {
                                if !ad.is_video && !ad.is_audio {
                                    ad.is_video = is_rep_video;
                                    ad.is_audio = is_rep_audio;
                                }
                            }

                            current_representation = Some(DashRepresentationData {
                                id,
                                bandwidth,
                                segment_template: None,
                                initialization_url: None,
                                initialization_range: None,
                                segment_urls: Vec::new(),
                                base_url: None,
                            });
                        }
                        "SegmentTemplate" => {
                            let mut tmpl = DashSegmentTemplateData {
                                initialization: None,
                                media: None,
                                timescale: 1,
                                start_number: 1,
                                duration: 0,
                                timeline: Vec::new(),
                            };

                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(attr.value.as_ref());
                                if key == "initialization" {
                                    tmpl.initialization = Some(val.to_string());
                                } else if key == "media" {
                                    tmpl.media = Some(val.to_string());
                                } else if key == "timescale" {
                                    tmpl.timescale = val.parse::<u64>().unwrap_or(1);
                                } else if key == "startNumber" {
                                    tmpl.start_number = val.parse::<u64>().unwrap_or(1);
                                } else if key == "duration" {
                                    tmpl.duration = val.parse::<u64>().unwrap_or(0);
                                }
                            }

                            current_template = Some(tmpl);
                        }
                        "SegmentTimeline" => {
                            in_timeline = true;
                        }
                        "S" => {
                            if in_timeline {
                                let mut t: Option<u64> = None;
                                let mut d: u64 = 0;
                                let mut r: Option<u64> = None;

                                for attr in e.attributes().flatten() {
                                    let key = String::from_utf8_lossy(attr.key.as_ref());
                                    let val = String::from_utf8_lossy(attr.value.as_ref());
                                    if key == "t" {
                                        t = val.parse::<u64>().ok();
                                    } else if key == "d" {
                                        d = val.parse::<u64>().unwrap_or(0);
                                    } else if key == "r" {
                                        r = val.parse::<u64>().ok();
                                    }
                                }

                                if let Some(ref mut tmpl) = current_template {
                                    tmpl.timeline.push((t, d, r));
                                }
                            }
                        }
                        "Initialization" => {
                            let mut source_url: Option<String> = None;
                            let mut range: Option<(u64, u64)> = None;

                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(attr.value.as_ref());
                                if key == "sourceURL" {
                                    source_url = Some(val.to_string());
                                } else if key == "range" {
                                    if let Some(dash_idx) = val.find('-') {
                                        let start = val[..dash_idx].parse::<u64>().unwrap_or(0);
                                        let end = val[dash_idx + 1..].parse::<u64>().unwrap_or(0);
                                        range = Some((start, end));
                                    }
                                }
                            }

                            if let Some(ref mut rep) = current_representation {
                                rep.initialization_url = source_url;
                                rep.initialization_range = range;
                            }
                        }
                        "SegmentURL" => {
                            let mut media_url = String::new();
                            let mut media_range: Option<(u64, u64)> = None;

                            for attr in e.attributes().flatten() {
                                let key = String::from_utf8_lossy(attr.key.as_ref());
                                let val = String::from_utf8_lossy(attr.value.as_ref());
                                if key == "media" {
                                    media_url = val.to_string();
                                } else if key == "mediaRange" {
                                    if let Some(dash_idx) = val.find('-') {
                                        let start = val[..dash_idx].parse::<u64>().unwrap_or(0);
                                        let end = val[dash_idx + 1..].parse::<u64>().unwrap_or(0);
                                        media_range = Some((start, end));
                                    }
                                }
                            }

                            if !media_url.is_empty() {
                                if let Some(ref mut rep) = current_representation {
                                    rep.segment_urls.push(MediaChunk {
                                        url: media_url,
                                        range: media_range,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Text(ref e)) => {
                    if inside_tag == "BaseURL" {
                        if let Ok(txt) = e.unescape() {
                            let t = txt.trim().to_string();
                            if !t.is_empty() {
                                if let Some(ref mut rep) = current_representation {
                                    rep.base_url = Some(t);
                                } else if root_base_url.is_none() {
                                    root_base_url = Some(t);
                                }
                            }
                        }
                    }
                }
                Ok(Event::End(ref e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    match tag_name.as_str() {
                        "SegmentTimeline" => {
                            in_timeline = false;
                        }
                        "SegmentTemplate" => {
                            if let Some(tmpl) = current_template.take() {
                                if let Some(ref mut rep) = current_representation {
                                    rep.segment_template = Some(tmpl);
                                } else if let Some(ref mut ad) = current_adaptation_set {
                                    ad.segment_template = Some(tmpl);
                                }
                            }
                        }
                        "Representation" => {
                            if let Some(rep) = current_representation.take() {
                                if let Some(ref mut ad) = current_adaptation_set {
                                    ad.representations.push(rep);
                                }
                            }
                        }
                        "AdaptationSet" => {
                            if let Some(ad) = current_adaptation_set.take() {
                                adaptation_sets.push(ad);
                            }
                        }
                        _ => {}
                    }
                    inside_tag.clear();
                }
                Ok(Event::Eof) => break,
                Err(err) => {
                    warn!("Error parsing MPD XML: {:#}", err);
                    break;
                }
                _ => {}
            }
            buf.clear();
        }

        let effective_base_url = if let Some(ref r_base) = root_base_url {
            base_url.join(r_base).unwrap_or_else(|_| base_url.clone())
        } else {
            base_url.clone()
        };

        // Separate video and audio adaptation sets
        let mut video_reps: Vec<(DashRepresentationData, Option<DashSegmentTemplateData>)> =
            Vec::new();
        let mut audio_reps: Vec<(DashRepresentationData, Option<DashSegmentTemplateData>)> =
            Vec::new();

        for ad in adaptation_sets {
            for rep in ad.representations {
                let tmpl = rep.segment_template.clone().or_else(|| ad.segment_template.clone());
                if ad.is_video {
                    video_reps.push((rep, tmpl));
                } else if ad.is_audio {
                    audio_reps.push((rep, tmpl));
                } else if rep.bandwidth > 500_000 {
                    video_reps.push((rep, tmpl));
                } else {
                    audio_reps.push((rep, tmpl));
                }
            }
        }

        if video_reps.is_empty() && audio_reps.is_empty() {
            return Err(anyhow!("No playable video or audio representations found in MPD manifest"));
        }

        // Helper to expand a representation into concrete init + media chunk URLs
        let build_track_chunks = |rep: &DashRepresentationData,
                                  tmpl_opt: Option<&DashSegmentTemplateData>,
                                  track_base: &Url|
         -> (Option<MediaChunk>, Vec<MediaChunk>) {
            let mut init_chunk: Option<MediaChunk> = None;
            let mut media_chunks: Vec<MediaChunk> = Vec::new();

            if let Some(ref init_url) = rep.initialization_url {
                let resolved = resolve_url_preserving_query(track_base, init_url);
                init_chunk = Some(MediaChunk {
                    url: resolved,
                    range: rep.initialization_range,
                });
            }

            if !rep.segment_urls.is_empty() {
                for seg in &rep.segment_urls {
                    let resolved = resolve_url_preserving_query(track_base, &seg.url);
                    media_chunks.push(MediaChunk {
                        url: resolved,
                        range: seg.range,
                    });
                }
                return (init_chunk, media_chunks);
            }

            if let Some(tmpl) = tmpl_opt {
                if let Some(ref init_tmpl) = tmpl.initialization {
                    let formatted = format_dash_template(
                        init_tmpl,
                        &rep.id,
                        None,
                        None,
                        Some(rep.bandwidth),
                    );
                    let resolved = resolve_url_preserving_query(track_base, &formatted);
                    init_chunk = Some(MediaChunk {
                        url: resolved,
                        range: None,
                    });
                }

                if let Some(ref media_tmpl) = tmpl.media {
                    if !tmpl.timeline.is_empty() {
                        let mut curr_t: u64 = 0;
                        let mut curr_n = tmpl.start_number;
                        for (st, d, r) in &tmpl.timeline {
                            if let Some(t) = st {
                                curr_t = *t;
                            }
                            let repeat = r.unwrap_or(0);
                            for _ in 0..=repeat {
                                let formatted = format_dash_template(
                                    media_tmpl,
                                    &rep.id,
                                    Some(curr_n),
                                    Some(curr_t),
                                    Some(rep.bandwidth),
                                );
                                let resolved = resolve_url_preserving_query(track_base, &formatted);
                                media_chunks.push(MediaChunk {
                                    url: resolved,
                                    range: None,
                                });
                                curr_t += d;
                                curr_n += 1;
                            }
                        }
                    } else if tmpl.duration > 0 && media_duration > 0.0 {
                        let seg_duration = tmpl.duration as f64 / tmpl.timescale as f64;
                        let seg_count = (media_duration / seg_duration).ceil() as u64;
                        for i in 0..seg_count {
                            let n = tmpl.start_number + i;
                            let t = i * tmpl.duration;
                            let formatted = format_dash_template(
                                media_tmpl,
                                &rep.id,
                                Some(n),
                                Some(t),
                                Some(rep.bandwidth),
                            );
                            let resolved = resolve_url_preserving_query(track_base, &formatted);
                            media_chunks.push(MediaChunk {
                                url: resolved,
                                range: None,
                            });
                        }
                    }
                }
            } else if let Some(ref b_url) = rep.base_url {
                let resolved = resolve_url_preserving_query(track_base, b_url);
                media_chunks.push(MediaChunk {
                    url: resolved,
                    range: None,
                });
            }

            (init_chunk, media_chunks)
        };

        // Select best video and best audio representations by bandwidth
        video_reps.sort_by(|a, b| b.0.bandwidth.cmp(&a.0.bandwidth));
        audio_reps.sort_by(|a, b| b.0.bandwidth.cmp(&a.0.bandwidth));

        let best_video = video_reps.first();
        let best_audio = audio_reps.first();

        let mut total_bw: u64 = 0;
        if let Some((v, _)) = best_video {
            total_bw += v.bandwidth;
            info!(
                "DASH: Selected highest quality video: id={}, bandwidth={}",
                v.id, v.bandwidth
            );
        }
        if let Some((a, _)) = best_audio {
            total_bw += a.bandwidth;
            info!(
                "DASH: Selected highest quality audio: id={}, bandwidth={}",
                a.id, a.bandwidth
            );
        }

        // Estimate total file size upfront
        if total_bw > 0 && media_duration > 0.0 {
            let estimated_size = (total_bw as f64 * media_duration / 8.0) as u64;
            let mut list = state.tasks.write().unwrap();
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.total_size = Some(estimated_size);
            }
        }

        let temp_video = task.save_path.with_extension("dash_v.tmp");
        let temp_audio = task.save_path.with_extension("dash_a.tmp");

        let mut has_video = false;
        let mut has_audio = false;
        let mut downloaded_bytes: u64 = 0;
        let mut last_instant = Instant::now();
        let mut last_bytes: u64 = 0;

        // Download Video track
        if let Some((v_rep, v_tmpl)) = best_video {
            has_video = true;
            let (v_init, v_segs) =
                build_track_chunks(v_rep, v_tmpl.as_ref(), &effective_base_url);

            let mut v_file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temp_video)
                .await?;

            if let Some(init_chunk) = v_init {
                let mut req = client.get(&init_chunk.url).headers(headers.clone());
                if let Some((s, e)) = init_chunk.range {
                    req = req.header(RANGE, format!("bytes={}-{}", s, e));
                }
                let b = req.send().await?.error_for_status()?.bytes().await?;
                v_file.write_all(&b).await?;
                downloaded_bytes += b.len() as u64;
            }

            for seg in &v_segs {
                if *cancel_rx.borrow() {
                    return Err(anyhow!("Cancelled"));
                }
                let mut req = client.get(&seg.url).headers(headers.clone());
                if let Some((start, end)) = seg.range {
                    req = req.header(RANGE, format!("bytes={}-{}", start, end));
                }
                let mut r = req.send().await?.error_for_status()?;
                while let Some(chunk) = r.chunk().await? {
                        v_file.write_all(&chunk).await?;
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
            v_file.flush().await?;
        }

        // Download Audio track
        if let Some((a_rep, a_tmpl)) = best_audio {
            has_audio = true;
            let (a_init, a_segs) =
                build_track_chunks(a_rep, a_tmpl.as_ref(), &effective_base_url);

            let mut a_file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temp_audio)
                .await?;

            if let Some(init_chunk) = a_init {
                let mut req = client.get(&init_chunk.url).headers(headers.clone());
                if let Some((s, e)) = init_chunk.range {
                    req = req.header(RANGE, format!("bytes={}-{}", s, e));
                }
                let b = req.send().await?.error_for_status()?.bytes().await?;
                a_file.write_all(&b).await?;
                downloaded_bytes += b.len() as u64;
            }

            for seg in &a_segs {
                if *cancel_rx.borrow() {
                    return Err(anyhow!("Cancelled"));
                }
                let mut req = client.get(&seg.url).headers(headers.clone());
                if let Some((start, end)) = seg.range {
                    req = req.header(RANGE, format!("bytes={}-{}", start, end));
                }
                let mut r = req.send().await?.error_for_status()?;
                while let Some(chunk) = r.chunk().await? {
                        a_file.write_all(&chunk).await?;
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
            a_file.flush().await?;
        }

        // Multiplex or finalize media
        if has_video && has_audio {
            info!("DASH: Multiplexing video and audio tracks with FFmpeg...");
            crate::muxer::mux_video_and_audio(&temp_video, &temp_audio, &task.save_path).await?;
        } else if has_video {
            let ff_res = Command::new("ffmpeg")
                .arg("-i")
                .arg(&temp_video)
                .arg("-c")
                .arg("copy")
                .arg("-y")
                .arg(&task.save_path)
                .output()
                .await;
            if ff_res.map(|s| s.status.success()).unwrap_or(false) {
                let _ = tokio::fs::remove_file(&temp_video).await;
            } else {
                let _ = tokio::fs::rename(&temp_video, &task.save_path).await;
            }
        } else if has_audio {
            let ff_res = Command::new("ffmpeg")
                .arg("-i")
                .arg(&temp_audio)
                .arg("-c")
                .arg("copy")
                .arg("-y")
                .arg(&task.save_path)
                .output()
                .await;
            if ff_res.map(|s| s.status.success()).unwrap_or(false) {
                let _ = tokio::fs::remove_file(&temp_audio).await;
            } else {
                let _ = tokio::fs::rename(&temp_audio, &task.save_path).await;
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
            "⚡ StreamEngine: DASH task {} completed ({} bytes)",
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
    let clean_url = url;

    // Check existing bytes for resumption (IDM-grade stream recovery)
    let existing_len = tokio::fs::metadata(&save_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    // Range is generated independently; the captured URL itself is unchanged.
    let range_header = if existing_len > 0 {
        format!("bytes={}-", existing_len)
    } else {
        "bytes=0-".to_string()
    };
    if let Ok(range_val) = HeaderValue::from_str(&range_header) {
        headers.insert(RANGE, range_val);
    }

    let mut resp = client
        .get(&clean_url)
        .headers(headers.clone())
        .send()
        .await
        .map_err(|e| anyhow!("Stream HTTP request failed: {}", e))?;

    let status = resp.status();
    if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(anyhow!(
            "Stream HTTP request returned error status: {}",
            status
        ));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    // UMP is not a directly playable container.  Do not mutate the signed
    // request to guess a different representation.
    if content_type.contains("application/vnd.yt-ump") {
        return Err(anyhow!("Captured stream returned unsupported UMP data"));
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
                        return Err(anyhow!("Error reading stream chunk: {}", e));
                    }
                }
            }
        }
    }

    file.flush().await?;
    if downloaded < 1024 {
        return Err(anyhow!(
            "Stream track download completed with only {} bytes (invalid or error payload)",
            downloaded
        ));
    }
    Ok(downloaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_dash_template() {
        let t1 = "chunk-$RepresentationID$-$Number%05d$.m4s";
        assert_eq!(
            format_dash_template(t1, "video_1", Some(12), None, None),
            "chunk-video_1-00012.m4s"
        );

        let t2 = "segment-$Time$.m4s";
        assert_eq!(
            format_dash_template(t2, "rep", None, Some(90000), None),
            "segment-90000.m4s"
        );

        let t3 = "track-$Bandwidth$.m4s";
        assert_eq!(
            format_dash_template(t3, "rep", None, None, Some(2500000)),
            "track-2500000.m4s"
        );
    }

    #[test]
    fn test_parse_iso8601_duration() {
        assert_eq!(parse_iso8601_duration("PT1H2M30S"), Some(3750.0));
        assert_eq!(parse_iso8601_duration("PT90.5S"), Some(90.5));
        assert_eq!(parse_iso8601_duration("PT4M"), Some(240.0));
    }

    #[test]
    fn test_resolve_url_preserving_query() {
        let base = Url::parse("https://video.twimg.com/ext/master.m3u8?tag=12").unwrap();
        let rel = "init.mp4";
        assert_eq!(
            resolve_url_preserving_query(&base, rel),
            "https://video.twimg.com/ext/init.mp4"
        );

        let with_own_query = "chunk.m4s?foo=bar";
        assert_eq!(
            resolve_url_preserving_query(&base, with_own_query),
            "https://video.twimg.com/ext/chunk.m4s?foo=bar"
        );
    }

    #[test]
    fn captured_candidate_kind_overrides_url_heuristics() {
        use crate::{MediaCandidate, MediaCandidateKind, RequestContext};

        let mut task = DownloadTask::from_media_candidate(
            MediaCandidate {
                id: Some("fixture".to_string()),
                kind: MediaCandidateKind::HlsManifest,
                page_url: None,
                primary: RequestContext {
                    // A signed manifest need not end with .m3u8.
                    url: "https://cdn.example.invalid/play?token=signed".to_string(),
                    ..RequestContext::default()
                },
                audio: None,
                quality: None,
                codecs: None,
                unsupported_reason: None,
            },
            "fixture.mp4".to_string(),
            PathBuf::from("/tmp/fixture.mp4"),
        );
        task.url = "https://cdn.example.invalid/play?token=signed".to_string();
        assert_eq!(
            StreamEngine::classify_task(&task),
            StreamDispatchMode::HlsSegmented
        );
    }
}

use crate::prober::probe_url;
use crate::segment::{Segment, SegmentStatus};
use crate::state::AppState;
use crate::storage::DiskStorage;
use crate::task::TaskStatus;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE, USER_AGENT};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::sleep;
use tracing::{error, info, warn};

#[derive(Clone, Debug)]
pub struct RateLimiter {
    pub limit_bytes_per_sec: Arc<AtomicU64>,
}

impl RateLimiter {
    pub fn new(limit_bytes_per_sec: u64) -> Self {
        Self {
            limit_bytes_per_sec: Arc::new(AtomicU64::new(limit_bytes_per_sec)),
        }
    }

    pub fn set_limit(&self, limit: u64) {
        self.limit_bytes_per_sec.store(limit, Ordering::Relaxed);
    }

    pub fn get_limit(&self) -> u64 {
        self.limit_bytes_per_sec.load(Ordering::Relaxed)
    }

    pub async fn throttle(&self, bytes: usize) {
        let limit = self.get_limit();
        if limit == 0 || bytes == 0 {
            return;
        }

        let secs = bytes as f64 / limit as f64;
        let sleep_duration = Duration::from_secs_f64(secs);
        sleep(sleep_duration).await;
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(0)
    }
}

#[derive(Clone)]
pub struct DownloadEngine {
    client: reqwest::Client,
    max_connections: usize,
    pub rate_limiter: RateLimiter,
}

impl DownloadEngine {
    pub fn new(max_connections: usize) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(32)
            .build()
            .unwrap_or_default();

        Self {
            client,
            max_connections: max_connections.clamp(1, 32),
            rate_limiter: RateLimiter::default(),
        }
    }

    pub fn with_rate_limiter(mut self, rate_limiter: RateLimiter) -> Self {
        self.rate_limiter = rate_limiter;
        self
    }

    pub fn set_speed_limit(&self, limit_bytes_per_sec: u64) {
        self.rate_limiter.set_limit(limit_bytes_per_sec);
    }

    pub async fn run_task(
        &self,
        task_id: String,
        state: AppState,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Result<()> {
        let task = {
            let list = state.tasks.read().await;
            list.iter().find(|t| t.id == task_id).cloned()
        }
        .ok_or_else(|| anyhow!("Task {} not found", task_id))?;

        info!("Starting download for task: {} ({})", task.filename, task.url);

        // Update task status to Downloading
        {
            let mut list = state.tasks.write().await;
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.status = TaskStatus::Downloading;
                t.error_message = None;
            }
        }

        // Multi-Engine Classification: Check if stream requires Extractor or HLS
        let mode = crate::stream_engine::StreamEngine::classify_task(&task);
        match mode {
            crate::stream_engine::StreamDispatchMode::ExtractorFallback => {
                return crate::stream_engine::StreamEngine::download_extractor(
                    &task_id,
                    &task,
                    state,
                    cancel_rx,
                )
                .await;
            }
            crate::stream_engine::StreamDispatchMode::HlsDashSegmented => {
                return crate::stream_engine::StreamEngine::download_hls(
                    &self.client,
                    &task_id,
                    &task,
                    state,
                    &mut cancel_rx,
                )
                .await;
            }
            _ => {}
        }

        // Probe if total size or range capability unknown
        let remote_info = probe_url(
            &self.client,
            &task.url,
            Some(&task.headers),
            Some(&task.filename),
        )
        .await
        .map_err(|e| {
            let msg = format!("Probe failed: {:#}", e);
            error!("{}", msg);
            msg
        });

        let (total_size, accept_ranges) = match remote_info {
            Ok(info) => {
                let mut list = state.tasks.write().await;
                if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                    if t.total_size.is_none() {
                        t.total_size = info.total_size;
                    }
                    if t.filename.is_empty() || t.filename == "download" {
                        t.filename = info.filename.clone();
                    }
                }
                (info.total_size, info.accept_ranges)
            }
            Err(e) => {
                let mut list = state.tasks.write().await;
                if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                    t.status = TaskStatus::Error;
                    t.error_message = Some(e.clone());
                }
                let _ = state.save_tasks().await;
                return Err(anyhow!(e));
            }
        };

        // Ensure user agent
        let user_agent = task.user_agent.clone().or_else(|| {
            Some("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36".to_string())
        });

        // Initialize Disk Storage
        let storage = DiskStorage::create_or_open(&task.save_path, total_size).await?;

        let res = if accept_ranges && total_size.is_some() && total_size.unwrap() > 0 {
            self.download_multisegment(
                &task_id,
                &task.url,
                total_size.unwrap(),
                &task.headers,
                task.cookies.as_deref(),
                user_agent.as_deref(),
                storage.clone(),
                state.clone(),
                &mut cancel_rx,
            )
            .await
        } else {
            self.download_single_stream(
                &task_id,
                &task.url,
                &task.headers,
                task.cookies.as_deref(),
                user_agent.as_deref(),
                storage.clone(),
                state.clone(),
                &mut cancel_rx,
            )
            .await
        };

        match res {
            Ok(_) => {
                let _ = storage.sync_all().await;

                // Adaptive Audio+Video Muxer (IDM-grade stream combiner)
                if let Some(ref a_url) = task.audio_url {
                    let audio_tmp_path = task.save_path.with_extension("audio_tmp.m4a");
                    let video_tmp_path = task.save_path.with_extension("video_tmp.mp4");

                    if let Ok(_) = tokio::fs::rename(&task.save_path, &video_tmp_path).await {
                        info!("Downloading separate audio track for adaptive stream muxing...");
                        if let Ok(audio_storage) = DiskStorage::create_or_open(&audio_tmp_path, None).await {
                            let a_res = self.download_single_stream(
                                &task_id,
                                a_url,
                                &task.headers,
                                task.cookies.as_deref(),
                                user_agent.as_deref(),
                                audio_storage.clone(),
                                state.clone(),
                                &mut cancel_rx,
                            ).await;
                            let _ = audio_storage.sync_all().await;

                            if a_res.is_ok() {
                                info!("Multiplexing video + audio with FFmpeg...");
                                let _ = crate::muxer::mux_video_and_audio(&video_tmp_path, &audio_tmp_path, &task.save_path).await;
                            } else {
                                let _ = tokio::fs::rename(&video_tmp_path, &task.save_path).await;
                            }
                        }
                    }
                }

                let meta = tokio::fs::metadata(&task.save_path).await.ok();
                let actual_bytes = meta.map(|m| m.len()).unwrap_or(0);
                if actual_bytes < 128 {
                    let mut list = state.tasks.write().await;
                    if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                        t.status = TaskStatus::Error;
                        t.error_message = Some("Downloaded file is empty or corrupted".to_string());
                        t.speed = 0.0;
                    }
                    let _ = state.save_tasks().await;
                    return Err(anyhow!("Downloaded file is empty or corrupted ({} bytes)", actual_bytes));
                }

                let mut list = state.tasks.write().await;
                if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                    t.status = TaskStatus::Completed;
                    t.speed = 0.0;
                    t.eta_seconds = None;
                    t.completed_at = Some(chrono::Utc::now());
                    t.downloaded_size = actual_bytes;
                    if t.total_size.is_none() {
                        t.total_size = Some(actual_bytes);
                    }
                }
                drop(list);
                let _ = state.save_tasks().await;
                info!("Task {} finished successfully! ({} bytes)", task.filename, actual_bytes);

                // Send desktop notification
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("notify-send")
                    .arg("⚡ Illusion Download Complete")
                    .arg(format!("{} has finished downloading.", task.filename))
                    .spawn();
            }
            Err(e) => {
                let is_cancelled = *cancel_rx.borrow();
                let mut list = state.tasks.write().await;
                if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                    if is_cancelled {
                        t.status = TaskStatus::Paused;
                        t.speed = 0.0;
                        t.eta_seconds = None;
                        info!("Task {} paused by user.", task.filename);
                    } else {
                        t.status = TaskStatus::Error;
                        t.error_message = Some(e.to_string());
                        t.speed = 0.0;
                        t.eta_seconds = None;
                        error!("Task {} failed: {:#}", task.filename, e);
                    }
                }
                drop(list);
                let _ = state.save_tasks().await;
            }
        }

        Ok(())
    }

    async fn download_multisegment(
        &self,
        task_id: &str,
        url: &str,
        total_size: u64,
        headers_map: &HashMap<String, String>,
        cookies: Option<&str>,
        user_agent: Option<&str>,
        storage: DiskStorage,
        state: AppState,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let conn_count = self.max_connections;

        // Initialize segments if not present
        {
            let mut list = state.tasks.write().await;
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                if t.segments.is_empty() {
                    let chunk_size = total_size / conn_count as u64;
                    let mut segs = Vec::new();
                    for i in 0..conn_count {
                        let start = i as u64 * chunk_size;
                        let end = if i == conn_count - 1 {
                            total_size - 1
                        } else {
                            (i as u64 + 1) * chunk_size - 1
                        };
                        segs.push(Segment::new(i, start, end));
                    }
                    t.segments = segs;
                }
            }
        }

        let segments_mutex = Arc::new(Mutex::new({
            let list = state.tasks.read().await;
            list.iter()
                .find(|t| t.id == task_id)
                .map(|t| t.segments.clone())
                .unwrap_or_default()
        }));

        let total_downloaded = Arc::new(AtomicU64::new({
            let list = state.tasks.read().await;
            list.iter().find(|t| t.id == task_id).map(|t| t.downloaded_size).unwrap_or(0)
        }));

        let stop_flag = Arc::new(AtomicBool::new(false));
        let (done_tx, mut done_rx) = mpsc::channel::<()>(conn_count * 2);

        // Spawn segment download workers
        let segs_snapshot = {
            let l = segments_mutex.lock().await;
            l.clone()
        };

        for seg in segs_snapshot {
            if seg.is_finished() {
                continue;
            }

            let client = self.client.clone();
            let url = url.to_string();
            let storage = storage.clone();
            let segs_ref = segments_mutex.clone();
            let total_dl = total_downloaded.clone();
            let stop = stop_flag.clone();
            let done_tx = done_tx.clone();
            let headers_map = headers_map.clone();
            let cookies = cookies.map(|s| s.to_string());
            let user_agent = user_agent.map(|s| s.to_string());
            let rate_limiter = self.rate_limiter.clone();
            let seg_id = seg.id;

            tokio::spawn(async move {
                let _ = run_segment_worker(
                    client,
                    url,
                    seg_id,
                    segs_ref,
                    storage,
                    total_dl,
                    stop,
                    headers_map,
                    cookies,
                    user_agent,
                    rate_limiter,
                )
                .await;
                let _ = done_tx.send(()).await;
            });
        }

        // Monitoring and dynamic rebalancing loop
        let mut last_instant = Instant::now();
        let mut last_bytes = total_downloaded.load(Ordering::Relaxed);

        loop {
            tokio::select! {
                _ = sleep(Duration::from_millis(500)) => {
                    if *cancel_rx.borrow() {
                        stop_flag.store(true, Ordering::Relaxed);
                        return Err(anyhow!("Cancelled"));
                    }

                    let now = Instant::now();
                    let elapsed = now.duration_since(last_instant).as_secs_f64();
                    let current_bytes = total_downloaded.load(Ordering::Relaxed);
                    let delta = current_bytes.saturating_sub(last_bytes);
                    let speed = if elapsed > 0.0 { delta as f64 / elapsed } else { 0.0 };

                    last_instant = now;
                    last_bytes = current_bytes;

                    let remaining = total_size.saturating_sub(current_bytes);
                    let eta = if speed > 1024.0 { Some((remaining as f64 / speed) as u64) } else { None };

                    // Update task in state
                    {
                        let segs = {
                            let l = segments_mutex.lock().await;
                            l.clone()
                        };
                        let mut list = state.tasks.write().await;
                        if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                            t.downloaded_size = current_bytes;
                            t.speed = speed;
                            t.eta_seconds = eta;
                            t.segments = segs;
                        }
                    }

                    // Check if complete
                    if current_bytes >= total_size {
                        break;
                    }
                }
                _ = done_rx.recv() => {
                    // An active worker finished. Try dynamic chunk rebalancing (IDM core mechanism!)
                    let mut segs = segments_mutex.lock().await;
                    let next_id = segs.len();

                    // Find an active segment with the largest remaining bytes (>= 2MB)
                    let mut best_idx = None;
                    let mut max_remaining = 2_000_000; // 2MB min threshold

                    for (idx, s) in segs.iter().enumerate() {
                        if s.status == SegmentStatus::Downloading {
                            let rem = s.remaining_bytes();
                            if rem > max_remaining {
                                max_remaining = rem;
                                best_idx = Some(idx);
                            }
                        }
                    }

                    if let Some(idx) = best_idx {
                        if let Some(new_seg) = segs[idx].split_remaining(next_id, 1_000_000) {
                            info!("Dynamic IDM rebalance: Split segment {} into new segment {}", idx, next_id);
                            segs.push(new_seg.clone());
                            drop(segs);

                            // Launch worker for the newly split segment
                            let client = self.client.clone();
                            let url = url.to_string();
                            let storage = storage.clone();
                            let segs_ref = segments_mutex.clone();
                            let total_dl = total_downloaded.clone();
                            let stop = stop_flag.clone();
                            let done_tx = done_tx.clone();
                            let headers_map = headers_map.clone();
                            let cookies = cookies.map(|s| s.to_string());
                            let user_agent = user_agent.map(|s| s.to_string());
                            let rate_limiter = self.rate_limiter.clone();

                            tokio::spawn(async move {
                                let _ = run_segment_worker(
                                    client,
                                    url,
                                    new_seg.id,
                                    segs_ref,
                                    storage,
                                    total_dl,
                                    stop,
                                    headers_map,
                                    cookies,
                                    user_agent,
                                    rate_limiter,
                                )
                                .await;
                                let _ = done_tx.send(()).await;
                            });
                        }
                    } else {
                        // Check if all finished
                        let all_done = segs.iter().all(|s| s.is_finished());
                        if all_done {
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn download_single_stream(
        &self,
        task_id: &str,
        url: &str,
        headers_map: &HashMap<String, String>,
        cookies: Option<&str>,
        user_agent: Option<&str>,
        storage: DiskStorage,
        state: AppState,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let mut headers = HeaderMap::new();
        for (k, v) in headers_map {
            if let (Ok(name), Ok(val)) = (k.parse::<HeaderName>(), HeaderValue::from_str(v)) {
                headers.insert(name, val);
            }
        }
        if let Some(ua) = user_agent {
            if let Ok(val) = HeaderValue::from_str(ua) {
                headers.insert(USER_AGENT, val);
            }
        }
        if let Some(c) = cookies {
            if let Ok(val) = HeaderValue::from_str(c) {
                headers.insert("cookie", val);
            }
        }

        if url.contains("googlevideo.com") && !headers.contains_key(reqwest::header::REFERER) {
            headers.insert(
                reqwest::header::REFERER,
                HeaderValue::from_static("https://www.youtube.com/"),
            );
        }

        let mut resp = self.client.get(url).headers(headers).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("Server returned HTTP {}", resp.status()));
        }
        let mut downloaded: u64 = 0;
        let mut last_instant = Instant::now();
        let mut last_bytes: u64 = 0;

        while let Some(chunk_res) = resp.chunk().await? {
            if *cancel_rx.borrow() {
                return Err(anyhow!("Cancelled"));
            }

            let len = chunk_res.len() as u64;
            storage.write_at(downloaded, chunk_res.to_vec()).await?;
            downloaded += len;

            // Throttle speed if rate limiter is active
            self.rate_limiter.throttle(len as usize).await;

            let now = Instant::now();
            let elapsed = now.duration_since(last_instant).as_secs_f64();
            if elapsed >= 0.5 {
                let delta = downloaded.saturating_sub(last_bytes);
                let speed = if elapsed > 0.0 { delta as f64 / elapsed } else { 0.0 };
                last_instant = now;
                last_bytes = downloaded;

                let mut list = state.tasks.write().await;
                if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                    t.downloaded_size = downloaded;
                    t.speed = speed;
                }
            }
        }

        // Always update total downloaded size at end
        {
            let mut list = state.tasks.write().await;
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.downloaded_size = downloaded;
            }
        }

        if downloaded < 128 {
            return Err(anyhow!("Received empty or invalid payload ({} bytes)", downloaded));
        }

        Ok(())
    }
}

async fn run_segment_worker(
    client: reqwest::Client,
    url: String,
    seg_id: usize,
    segs: Arc<Mutex<Vec<Segment>>>,
    storage: DiskStorage,
    total_dl: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    headers_map: HashMap<String, String>,
    cookies: Option<String>,
    user_agent: Option<String>,
    rate_limiter: RateLimiter,
) -> Result<()> {
    // Mark segment as downloading
    let (mut current, mut end) = {
        let mut list = segs.lock().await;
        if let Some(s) = list.iter_mut().find(|s| s.id == seg_id) {
            s.status = SegmentStatus::Downloading;
            (s.current, s.end)
        } else {
            return Ok(());
        }
    };

    while current <= end && !stop.load(Ordering::Relaxed) {
        let range_str = format!("bytes={}-{}", current, end);
        let mut req_headers = HeaderMap::new();
        req_headers.insert(RANGE, HeaderValue::from_str(&range_str)?);

        for (k, v) in &headers_map {
            if let (Ok(name), Ok(val)) = (k.parse::<HeaderName>(), HeaderValue::from_str(v)) {
                req_headers.insert(name, val);
            }
        }

        if url.contains("googlevideo.com") && !req_headers.contains_key(reqwest::header::REFERER) {
            req_headers.insert(
                reqwest::header::REFERER,
                HeaderValue::from_static("https://www.youtube.com/"),
            );
        }
        if let Some(ref ua) = user_agent {
            if let Ok(val) = HeaderValue::from_str(ua) {
                req_headers.insert(USER_AGENT, val);
            }
        }
        if let Some(ref c) = cookies {
            if let Ok(val) = HeaderValue::from_str(c) {
                req_headers.insert("cookie", val);
            }
        }

        let resp = match client.get(&url).headers(req_headers).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!("Segment {} HTTP status {}", seg_id, r.status());
                sleep(Duration::from_millis(500)).await;
                continue;
            }
            Err(e) => {
                warn!("Segment {} request error: {:#}", seg_id, e);
                sleep(Duration::from_millis(500)).await;
                continue;
            }
        };

        let mut stream = resp.bytes_stream();
        while let Some(item) = stream.next().await {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    warn!("Segment {} chunk read error: {:#}", seg_id, e);
                    break;
                }
            };

            let len = chunk.len() as u64;
            storage.write_at(current, chunk.to_vec()).await?;
            current += len;
            total_dl.fetch_add(len, Ordering::Relaxed);

            // Throttle speed if rate limiter is active
            rate_limiter.throttle(len as usize).await;

            // Update segment current pointer and get possibly updated end
            {
                let mut list = segs.lock().await;
                if let Some(s) = list.iter_mut().find(|s| s.id == seg_id) {
                    s.current = current;
                    end = s.end; // Dynamic split might have shrunk end!
                }
            }

            if current > end {
                break;
            }
        }
    }

    // Mark completed or pending
    let mut list = segs.lock().await;
    if let Some(s) = list.iter_mut().find(|s| s.id == seg_id) {
        if s.is_finished() {
            s.status = SegmentStatus::Completed;
        } else {
            s.status = SegmentStatus::Pending;
        }
    }

    Ok(())
}

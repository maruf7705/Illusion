use crate::prober::{build_request_headers, probe_request};
use crate::segment::{Segment, SegmentStatus};
use crate::state::AppState;
use crate::storage::DiskStorage;
use crate::task::TaskStatus;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderValue, RANGE};
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
            let list = state.tasks.read().unwrap();
            list.iter().find(|t| t.id == task_id).cloned()
        }
        .ok_or_else(|| anyhow!("Task {} not found", task_id))?;

        let primary_request = task.primary_request();
        let task_url = primary_request.url.clone();
        info!("Starting download for task: {} ({})", task.filename, task_url);

        // Update task status to Downloading
        {
            let mut list = state.tasks.write().unwrap();
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.status = TaskStatus::Downloading;
                t.error_message = None;
            }
        }

        // Multi-Engine Classification: Check if stream requires Dual-Stream Adaptive Muxing or HLS
        let mode = crate::stream_engine::StreamEngine::classify_task(&task);
        let stream_res = match mode {
            crate::stream_engine::StreamDispatchMode::SeparatedVideoAudioMux => {
                Some(crate::stream_engine::StreamEngine::download_dual_stream(
                    &self.client,
                    &task_id,
                    &task,
                    state.clone(),
                    &mut cancel_rx,
                )
                .await)
            }
            crate::stream_engine::StreamDispatchMode::DashSegmented => {
                Some(crate::stream_engine::StreamEngine::download_dash(
                    &self.client,
                    &task_id,
                    &task,
                    state.clone(),
                    &mut cancel_rx,
                )
                .await)
            }
            crate::stream_engine::StreamDispatchMode::HlsSegmented => {
                Some(crate::stream_engine::StreamEngine::download_hls(
                    &self.client,
                    &task_id,
                    &task,
                    state.clone(),
                    &mut cancel_rx,
                )
                .await)
            }
            crate::stream_engine::StreamDispatchMode::ProtectedOrUnsupported => {
                Some(Err(anyhow!(task
                    .media_candidate
                    .as_ref()
                    .and_then(|candidate| candidate.unsupported_reason.clone())
                    .unwrap_or_else(|| "This media is protected or unsupported".to_string()))))
            }
            crate::stream_engine::StreamDispatchMode::DirectByteRange => None,
        };

        if let Some(res) = stream_res {
            if let Err(e) = res {
                {
                    let mut list = state.tasks.write().unwrap();
                    if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                        t.status = TaskStatus::Error;
                        t.error_message = Some(e.to_string());
                        t.speed = 0.0;
                    }
                }
                let _ = state.save_tasks().await;
                return Err(e);
            }
            let _ = state.save_tasks().await;
            return Ok(());
        }

        // Probe if total size or range capability unknown (with responsive cancellation support)
        let remote_info = tokio::select! {
            res = cancel_rx.changed() => {
                if res.is_ok() && *cancel_rx.borrow() {
                    return Err(anyhow!("Cancelled"));
                }
                // If channel closed or not true, don't abort probe
                std::future::pending::<Result<crate::prober::RemoteInfo, String>>().await
            }
            res = probe_request(&self.client, &primary_request, Some(&task.filename)) => {
                res.map_err(|e| format!("Probe failed: {:#}", e))
            }
        };

        let (total_size, accept_ranges) = match remote_info {
            Ok(info) => {
                {
                    let mut list = state.tasks.write().unwrap();
                    if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                        if t.total_size.is_none() {
                            t.total_size = info.total_size;
                        }
                        if t.filename.is_empty() || t.filename == "download" {
                            t.filename = info.filename.clone();
                        }
                    }
                }
                (info.total_size, info.accept_ranges)
            }
            Err(e) => {
                {
                    let mut list = state.tasks.write().unwrap();
                    if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                        t.status = TaskStatus::Error;
                        t.error_message = Some(e.clone());
                    }
                }
                let _ = state.save_tasks().await;
                return Err(anyhow!(e));
            }
        };

        // Ensure user agent
        let user_agent = primary_request.user_agent.clone().or_else(|| {
            Some("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36".to_string())
        });

        // Initialize Disk Storage
        let storage = DiskStorage::create_or_open(&task.save_path, total_size).await?;

        let res = if accept_ranges && total_size.is_some() && total_size.unwrap() > 0 {
            match self.download_multisegment(
                &task_id,
                &task_url,
                total_size.unwrap(),
                &primary_request.normalized_headers(),
                primary_request.cookies.as_deref(),
                user_agent.as_deref(),
                storage.clone(),
                state.clone(),
                &mut cancel_rx,
            )
            .await {
                Ok(()) => Ok(()),
                Err(e) => {
                    warn!("Multisegment download failed ({:#}), falling back to single stream", e);
                    self.download_single_stream(
                        &task_id,
                        &task_url,
                        &primary_request.normalized_headers(),
                        primary_request.cookies.as_deref(),
                        user_agent.as_deref(),
                        storage.clone(),
                        state.clone(),
                        &mut cancel_rx,
                    )
                    .await
                }
            }
        } else {
            self.download_single_stream(
                &task_id,
                &task_url,
                &primary_request.normalized_headers(),
                primary_request.cookies.as_deref(),
                user_agent.as_deref(),
                storage.clone(),
                state.clone(),
                &mut cancel_rx,
            )
            .await
        };

        match res {
            Ok(_) => {
                let written_size = {
                    let list = state.tasks.read().unwrap();
                    list.iter()
                        .find(|download| download.id == task_id)
                        .map(|download| download.downloaded_size)
                        .unwrap_or(0)
                };
                if let Some(expected) = total_size {
                    if written_size != expected {
                        let message = format!(
                            "Incomplete response: expected {} bytes, wrote {} bytes",
                            expected, written_size
                        );
                        {
                            let mut list = state.tasks.write().unwrap();
                            if let Some(download) = list.iter_mut().find(|download| download.id == task_id) {
                                download.status = TaskStatus::Error;
                                download.error_message = Some(message.clone());
                                download.speed = 0.0;
                            }
                        }
                        let _ = state.save_tasks().await;
                        return Err(anyhow!(message));
                    }
                }
                let _ = storage.sync_all().await;

                let meta = tokio::fs::metadata(&task.save_path).await.ok();
                let actual_bytes = meta.map(|m| m.len()).unwrap_or(0);
                if actual_bytes < 128 {
                    {
                        let mut list = state.tasks.write().unwrap();
                        if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                            t.status = TaskStatus::Error;
                            t.error_message = Some("Downloaded file is empty or corrupted".to_string());
                            t.speed = 0.0;
                        }
                    }
                    let _ = state.save_tasks().await;
                    return Err(anyhow!("Downloaded file is empty or corrupted ({} bytes)", actual_bytes));
                }

                {
                    let mut list = state.tasks.write().unwrap();
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
                }
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
                {
                    let mut list = state.tasks.write().unwrap();
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
                }
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
            let mut list = state.tasks.write().unwrap();
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
            let list = state.tasks.read().unwrap();
            list.iter()
                .find(|t| t.id == task_id)
                .map(|t| t.segments.clone())
                .unwrap_or_default()
        }));

        let total_downloaded = Arc::new(AtomicU64::new({
            let list = state.tasks.read().unwrap();
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
                        let mut list = state.tasks.write().unwrap();
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
        let clean_url = url;
        let headers = build_request_headers(headers_map, cookies, user_agent);
        let mut resp = self.client.get(clean_url).headers(headers).send().await?;
        let status = resp.status();
        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(anyhow!("Server returned HTTP {}", status));
        }
        if resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"))
        {
            return Err(anyhow!("Server returned HTML instead of media"));
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

                {
                    let mut list = state.tasks.write().unwrap();
                    if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                        t.downloaded_size = downloaded;
                        t.speed = speed;
                    }
                }
            }
        }

        // Always update total downloaded size at end
        {
            let mut list = state.tasks.write().unwrap();
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

fn content_range_starts_at(header: Option<&str>, expected_start: u64) -> bool {
    header
        .and_then(|value| value.strip_prefix("bytes "))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, _)| start.parse::<u64>().ok())
        == Some(expected_start)
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

    let clean_url = url;

    while current <= end && !stop.load(Ordering::Relaxed) {
        let mut req_headers = build_request_headers(&headers_map, cookies.as_deref(), user_agent.as_deref());

        let range_str = format!("bytes={}-{}", current, end);
        req_headers.insert(RANGE, HeaderValue::from_str(&range_str)?);

        let mut retry_count = 0;
        let resp = loop {
            if retry_count >= 3 {
                return Err(anyhow!("Segment {} failed after 3 attempts", seg_id));
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }

            match client.get(&clean_url).headers(req_headers.clone()).send().await {
                Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT
                    && content_range_starts_at(r.headers().get(reqwest::header::CONTENT_RANGE).and_then(|value| value.to_str().ok()), current) => break r,
                Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                    return Err(anyhow!("Segment {} returned a mismatched Content-Range", seg_id));
                }
                Ok(r) if current == 0 && r.status() == reqwest::StatusCode::OK => break r,
                Ok(r) if r.status() == reqwest::StatusCode::OK && current > 0 => {
                    // Server does not support byte-range slicing (returned 200 OK for sub-range)
                    return Err(anyhow!("Server does not support byte ranges (returned 200 OK)"));
                }
                Ok(r) => {
                    warn!("Segment {} HTTP status {} unexpected for range request", seg_id, r.status());
                    retry_count += 1;
                    sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    warn!("Segment {} request error: {:#}", seg_id, e);
                    retry_count += 1;
                    sleep(Duration::from_millis(500)).await;
                }
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

            let remaining = (end - current + 1) as usize;
            let chunk = if chunk.len() > remaining {
                chunk.slice(..remaining)
            } else {
                chunk
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

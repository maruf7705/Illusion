use crate::prober::{build_request_headers, probe_request};
use crate::scheduler::{AdaptiveRangeScheduler, StealAction, WorkerSlot};
use crate::segment::{Segment, SegmentStatus};
use crate::state::AppState;
use crate::storage::DiskStorage;
use crate::task::{DownloadTask, TaskStatus};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderValue, RANGE, RETRY_AFTER};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
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

fn find_associated_page_url(task: &DownloadTask, primary_request: &crate::media::RequestContext) -> Option<String> {
    if crate::resolver::is_video_page_url(&primary_request.url) {
        return Some(primary_request.url.clone());
    }
    if let Some(ref r) = primary_request.referer {
        if crate::resolver::is_video_page_url(r) {
            return Some(r.clone());
        }
    }
    if let Some(ref mc) = task.media_candidate {
        if let Some(ref pu) = mc.page_url {
            if crate::resolver::is_video_page_url(pu) {
                return Some(pu.clone());
            }
        }
        if let Some(ref r) = mc.primary.referer {
            if crate::resolver::is_video_page_url(r) {
                return Some(r.clone());
            }
        }
    }
    if let Some(r) = task.headers.get("referer").or_else(|| task.headers.get("Referer")) {
        if crate::resolver::is_video_page_url(r) {
            return Some(r.clone());
        }
    }
    None
}

impl DownloadEngine {
    pub fn new(max_connections: usize) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(32)
            .tcp_nodelay(true)
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
        let mut task = {
            let list = state.tasks.read().unwrap();
            list.iter().find(|t| t.id == task_id).cloned()
        }
        .ok_or_else(|| anyhow!("Task {} not found", task_id))?;

        let mut primary_request = task.primary_request();
        let mut task_url = primary_request.url.clone();

        // 0. Auto-resolve video page URLs or browser-captured Web streams with raw n-challenges
        if let Some(page_url) = find_associated_page_url(&task, &primary_request) {
            let is_direct_page = crate::resolver::is_video_page_url(&task_url);
            let is_web_googlevideo = task_url.contains("googlevideo.com")
                && (task_url.contains("c=WEB") || task_url.contains("&n=") || task_url.contains("?n="));

            if is_direct_page || is_web_googlevideo {
                info!("Auto-resolving video page stream: {}", page_url);
                let quality_pref = task.media_candidate.as_ref().and_then(|c| c.quality.as_deref());
                match crate::resolver::resolve_media_url(&page_url, quality_pref).await {
                    Ok(resolved) => {
                        info!("Successfully resolved video stream: {} -> {}", page_url, resolved.stream_url);
                        task_url = resolved.stream_url.clone();
                        primary_request.url = resolved.stream_url.clone();
                        for (k, v) in resolved.headers {
                            primary_request.headers.insert(k, v);
                        }
                        if let Some(ua) = resolved.user_agent {
                            primary_request.user_agent = Some(ua);
                        }
                        if task.total_size.is_none() {
                            task.total_size = resolved.total_size;
                        }
                        if let Some(audio) = resolved.audio_url {
                            task.audio_url = Some(audio);
                        }
                        let is_generic_filename = task.filename.is_empty()
                            || task.filename == "download"
                            || task.filename.starts_with("watch_")
                            || task.filename.starts_with("Facebook_")
                            || task.filename.starts_with("Facebook.mp4")
                            || task.filename.starts_with("Twitter_")
                            || task.filename.contains("videoplayback");
                        if is_generic_filename {
                            task.filename = resolved.filename.clone();
                            if let Some(parent) = task.save_path.parent() {
                                task.save_path = parent.join(&resolved.filename);
                            }
                        }
                        // Sync updated info to state
                        {
                            let mut list = state.tasks.write().unwrap();
                            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                                t.url = task_url.clone();
                                t.filename = task.filename.clone();
                                t.save_path = task.save_path.clone();
                                t.total_size = task.total_size;
                                t.audio_url = task.audio_url.clone();
                                t.headers = primary_request.headers.clone();
                                t.user_agent = primary_request.user_agent.clone();
                            }
                        }
                        let _ = state.save_tasks().await;
                    }
                    Err(e) => {
                        warn!("Stream resolver note: {:#}", e);
                    }
                }
            }
        }

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
                // If probe failed (e.g. 403 Forbidden or HTML returned), attempt recovery via associated page URL
                let mut recovered = None;
                if let Some(page_url) = find_associated_page_url(&task, &primary_request) {
                    info!("Probe failed ({:#}). Attempting recovery via page resolver: {}", e, page_url);
                    let quality_pref = task.media_candidate.as_ref().and_then(|c| c.quality.as_deref());
                    if let Ok(resolved) = crate::resolver::resolve_media_url(&page_url, quality_pref).await {
                        task_url = resolved.stream_url.clone();
                        primary_request.url = resolved.stream_url.clone();
                        for (k, v) in resolved.headers {
                            primary_request.headers.insert(k, v);
                        }
                        if let Some(ua) = resolved.user_agent {
                            primary_request.user_agent = Some(ua);
                        }
                        if task.total_size.is_none() {
                            task.total_size = resolved.total_size;
                        }
                        if let Some(audio) = resolved.audio_url {
                            task.audio_url = Some(audio);
                        }
                        let is_generic = task.filename.is_empty()
                            || task.filename == "download"
                            || task.filename.starts_with("watch_")
                            || task.filename.starts_with("Facebook_")
                            || task.filename.starts_with("Facebook.mp4")
                            || task.filename.contains("videoplayback");
                        if is_generic {
                            task.filename = resolved.filename.clone();
                            if let Some(parent) = task.save_path.parent() {
                                task.save_path = parent.join(&resolved.filename);
                            }
                        }
                        // Re-probe with new stream
                        if let Ok(info) = probe_request(&self.client, &primary_request, Some(&task.filename)).await {
                            recovered = Some((info.total_size.or(resolved.total_size), info.accept_ranges));
                        } else if let Some(size) = resolved.total_size {
                            recovered = Some((Some(size), true));
                        }
                    }
                }

                if let Some((rec_size, rec_ranges)) = recovered {
                    {
                        let mut list = state.tasks.write().unwrap();
                        if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                            t.url = task_url.clone();
                            t.filename = task.filename.clone();
                            t.save_path = task.save_path.clone();
                            t.total_size = rec_size;
                            t.audio_url = task.audio_url.clone();
                            t.headers = primary_request.headers.clone();
                            t.user_agent = primary_request.user_agent.clone();
                            t.error_message = None;
                        }
                    }
                    let _ = state.save_tasks().await;
                    (rec_size, rec_ranges)
                } else {
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
                let meta = tokio::fs::metadata(&task.save_path).await.ok();
                let actual_bytes = meta.map(|m| m.len()).unwrap_or(written_size);
                if let Some(expected) = total_size {
                    if actual_bytes != expected && written_size != expected {
                        let message = format!(
                            "Incomplete response: expected {} bytes, wrote {} bytes",
                            expected, actual_bytes
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
        // Avoid over-splitting small objects into many tiny range requests (Hydra principle)
        let conn_count = ((total_size / (128 * 1024)).max(1) as usize).min(self.max_connections);

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

        let segs_snapshot = {
            let list = state.tasks.read().unwrap();
            list.iter()
                .find(|t| t.id == task_id)
                .map(|t| t.segments.clone())
                .unwrap_or_default()
        };

        let initial_downloaded: u64 = segs_snapshot.iter().map(|s| s.downloaded_bytes()).sum();
        let total_downloaded = Arc::new(AtomicU64::new(initial_downloaded));

        let slots: Vec<WorkerSlot> = segs_snapshot
            .iter()
            .map(|s| WorkerSlot::new(s.id, s.start, s.current, s.end, 0.0))
            .collect();

        let max_cap = self.max_connections.max(conn_count);
        let mut scheduler = AdaptiveRangeScheduler::new(slots, max_cap, 2.0);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let (event_tx, mut event_rx) = mpsc::channel::<WorkerEvent>(256);

        let mut in_flight: usize = 0;
        let initial_slots = scheduler.slots.clone();
        for slot in &initial_slots {
            if slot.finished {
                continue;
            }
            in_flight += 1;
            spawn_slot_worker(
                self.client.clone(),
                url.to_string(),
                slot,
                storage.clone(),
                total_downloaded.clone(),
                stop_flag.clone(),
                event_tx.clone(),
                headers_map.clone(),
                cookies.map(|s| s.to_string()),
                user_agent.map(|s| s.to_string()),
                self.rate_limiter.clone(),
            );
        }

        let start_instant = Instant::now();
        let mut last_ui_sync = Instant::now();
        let mut last_bytes = total_downloaded.load(Ordering::Acquire);
        let mut tick_interval = tokio::time::interval(Duration::from_millis(100));
        tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_err: Option<String> = None;

        while in_flight > 0 {
            if total_downloaded.load(Ordering::Acquire) >= total_size
                && scheduler.slots.iter().all(|s| s.pos.load(Ordering::Acquire) > s.end.load(Ordering::Acquire))
            {
                stop_flag.store(true, Ordering::Release);
                break;
            }
            tokio::select! {
                _ = tick_interval.tick() => {
                    if *cancel_rx.borrow() {
                        stop_flag.store(true, Ordering::Release);
                        return Err(anyhow!("Cancelled"));
                    }

                    // Drain batched worker telemetry events non-blockingly
                    let now_secs = start_instant.elapsed().as_secs_f64();
                    while let Ok(ev) = event_rx.try_recv() {
                        match ev {
                            WorkerEvent::Ttfb { secs, .. } => {
                                scheduler.observe_ttfb(secs);
                            }
                            WorkerEvent::Progress { slot_id, bytes, dt_secs } => {
                                scheduler.on_bytes(slot_id, bytes, now_secs, dt_secs);
                            }
                            WorkerEvent::Refused { retry_after_secs } => {
                                // Server returned 429/503: lower concurrency ceiling cleanly
                                let active_now = scheduler.active_count().saturating_sub(1).max(1);
                                scheduler.max_concurrent = scheduler.max_concurrent.min(active_now);
                                warn!("Origin refused excess concurrency ({}s backoff); clamped max_concurrent to {}", retry_after_secs, scheduler.max_concurrent);
                            }
                            WorkerEvent::Finished { slot_id, err } => {
                                in_flight = in_flight.saturating_sub(1);
                                scheduler.mark_finished(slot_id);
                                if let Some(msg) = err {
                                    last_err = Some(msg);
                                }
                            }
                        }
                    }

                    // Run Hydra CUSUM collapse + divergence steal scheduler
                    let actions = scheduler.tick(now_secs);
                    for action in actions {
                        match action {
                            StealAction::ShrinkAndSpawn { victim_id, new_victim_end, new_id, steal_start, steal_end } => {
                                if let Some(new_slot) = scheduler.slots.iter().find(|s| s.id == new_id).cloned() {
                                    info!(
                                        "Hydra scheduler steal: shrunk worker {} end -> {}, spawning worker {} [{}-{}]",
                                        victim_id, new_victim_end, new_id, steal_start, steal_end
                                    );
                                    in_flight += 1;
                                    spawn_slot_worker(
                                        self.client.clone(),
                                        url.to_string(),
                                        &new_slot,
                                        storage.clone(),
                                        total_downloaded.clone(),
                                        stop_flag.clone(),
                                        event_tx.clone(),
                                        headers_map.clone(),
                                        cookies.map(|s| s.to_string()),
                                        user_agent.map(|s| s.to_string()),
                                        self.rate_limiter.clone(),
                                    );
                                }
                            }
                        }
                    }

                    // Sync UI & segment state periodically (every ~250ms, never on hot read loop)
                    let now = Instant::now();
                    if now.duration_since(last_ui_sync) >= Duration::from_millis(250) {
                        let elapsed = now.duration_since(last_ui_sync).as_secs_f64();
                        let current_bytes = total_downloaded.load(Ordering::Acquire);
                        let delta = current_bytes.saturating_sub(last_bytes);
                        let speed = if elapsed > 0.0 { delta as f64 / elapsed } else { 0.0 };
                        last_ui_sync = now;
                        last_bytes = current_bytes;

                        let remaining = total_size.saturating_sub(current_bytes);
                        let eta = if speed > 1024.0 { Some((remaining as f64 / speed) as u64) } else { None };
                        let segs_view = snapshot_segments_from_slots(&scheduler.slots);

                        let mut list = state.tasks.write().unwrap();
                        if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                            t.downloaded_size = current_bytes;
                            t.speed = speed;
                            t.eta_seconds = eta;
                            t.segments = segs_view;
                        }
                    }
                }
                maybe_ev = event_rx.recv() => {
                    if let Some(ev) = maybe_ev {
                        let now_secs = start_instant.elapsed().as_secs_f64();
                        match ev {
                            WorkerEvent::Ttfb { secs, .. } => scheduler.observe_ttfb(secs),
                            WorkerEvent::Progress { slot_id, bytes, dt_secs } => {
                                scheduler.on_bytes(slot_id, bytes, now_secs, dt_secs);
                            }
                            WorkerEvent::Refused { .. } => {
                                let active_now = scheduler.active_count().saturating_sub(1).max(1);
                                scheduler.max_concurrent = scheduler.max_concurrent.min(active_now);
                            }
                            WorkerEvent::Finished { slot_id, err } => {
                                in_flight = in_flight.saturating_sub(1);
                                scheduler.mark_finished(slot_id);
                                if let Some(msg) = err {
                                    last_err = Some(msg);
                                }
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        // Final update to state after all workers exit
        let final_downloaded = total_downloaded.load(Ordering::Acquire);
        let final_segs = snapshot_segments_from_slots(&scheduler.slots);
        {
            let mut list = state.tasks.write().unwrap();
            if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                t.downloaded_size = final_downloaded;
                t.segments = final_segs;
                t.speed = 0.0;
                t.eta_seconds = None;
            }
        }

        if final_downloaded < total_size {
            return Err(anyhow!(
                last_err.unwrap_or_else(|| format!("Incomplete multisegment transfer ({}/{})", final_downloaded, total_size))
            ));
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
        const WRITE_COALESCE_CAP: usize = 64 * 1024;
        let mut write_buf = Vec::with_capacity(WRITE_COALESCE_CAP);
        let mut buf_offset: u64 = 0;

        while let Some(chunk_res) = resp.chunk().await? {
            if *cancel_rx.borrow() {
                if !write_buf.is_empty() {
                    storage.write_at_slice(buf_offset, &write_buf)?;
                    write_buf.clear();
                }
                return Err(anyhow!("Cancelled"));
            }

            let len = chunk_res.len() as u64;
            if write_buf.len() + chunk_res.len() > WRITE_COALESCE_CAP && !write_buf.is_empty() {
                storage.write_at_slice(buf_offset, &write_buf)?;
                downloaded += write_buf.len() as u64;
                write_buf.clear();
                buf_offset = downloaded;
            }
            if chunk_res.len() >= WRITE_COALESCE_CAP {
                storage.write_at_slice(downloaded, &chunk_res)?;
                downloaded += len;
                buf_offset = downloaded;
            } else {
                if write_buf.is_empty() {
                    buf_offset = downloaded;
                }
                write_buf.extend_from_slice(&chunk_res);
            }

            self.rate_limiter.throttle(len as usize).await;

            let now = Instant::now();
            let elapsed = now.duration_since(last_instant).as_secs_f64();
            if elapsed >= 0.5 {
                let delta = (downloaded + write_buf.len() as u64).saturating_sub(last_bytes);
                let speed = if elapsed > 0.0 { delta as f64 / elapsed } else { 0.0 };
                last_instant = now;
                last_bytes = downloaded + write_buf.len() as u64;

                let mut list = state.tasks.write().unwrap();
                if let Some(t) = list.iter_mut().find(|t| t.id == task_id) {
                    t.downloaded_size = last_bytes;
                    t.speed = speed;
                }
            }
        }

        if !write_buf.is_empty() {
            storage.write_at_slice(buf_offset, &write_buf)?;
            downloaded += write_buf.len() as u64;
            write_buf.clear();
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

#[derive(Debug)]
enum WorkerEvent {
    Ttfb {
        #[allow(dead_code)]
        slot_id: usize,
        secs: f64,
    },
    Progress {
        slot_id: usize,
        bytes: u64,
        dt_secs: f64,
    },
    Refused {
        retry_after_secs: u64,
    },
    Finished {
        slot_id: usize,
        err: Option<String>,
    },
}

fn snapshot_segments_from_slots(slots: &[WorkerSlot]) -> Vec<Segment> {
    slots
        .iter()
        .map(|slot| {
            let current = slot.pos.load(Ordering::Acquire);
            let end = slot.end.load(Ordering::Acquire);
            let status = if current > end || slot.finished {
                SegmentStatus::Completed
            } else if slot.active {
                SegmentStatus::Downloading
            } else {
                SegmentStatus::Pending
            };
            Segment {
                id: slot.id,
                start: slot.start,
                current,
                end,
                status,
                speed: slot.rate_est,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn spawn_slot_worker(
    client: reqwest::Client,
    url: String,
    slot: &WorkerSlot,
    storage: DiskStorage,
    total_dl: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    event_tx: mpsc::Sender<WorkerEvent>,
    headers_map: HashMap<String, String>,
    cookies: Option<String>,
    user_agent: Option<String>,
    rate_limiter: RateLimiter,
) {
    let slot_id = slot.id;
    let pos_atomic = slot.pos.clone();
    let end_atomic = slot.end.clone();

    tokio::spawn(async move {
        let res = run_segment_worker(
            client,
            url,
            slot_id,
            pos_atomic,
            end_atomic,
            storage,
            total_dl,
            stop,
            event_tx.clone(),
            headers_map,
            cookies,
            user_agent,
            rate_limiter,
        )
        .await;
        let _ = event_tx
            .send(WorkerEvent::Finished {
                slot_id,
                err: res.err().map(|e| e.to_string()),
            })
            .await;
    });
}

fn content_range_starts_at(header: Option<&str>, expected_start: u64) -> bool {
    header
        .and_then(|value| value.strip_prefix("bytes "))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, _)| start.parse::<u64>().ok())
        == Some(expected_start)
}

#[allow(clippy::too_many_arguments)]
async fn run_segment_worker(
    client: reqwest::Client,
    url: String,
    seg_id: usize,
    pos_atomic: Arc<AtomicU64>,
    end_atomic: Arc<AtomicU64>,
    storage: DiskStorage,
    total_dl: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    event_tx: mpsc::Sender<WorkerEvent>,
    headers_map: HashMap<String, String>,
    cookies: Option<String>,
    user_agent: Option<String>,
    rate_limiter: RateLimiter,
) -> Result<()> {
    const WRITE_COALESCE_CAP: usize = 64 * 1024;
    const REPORT_BYTES_THRESHOLD: u64 = 64 * 1024;

    let mut current = pos_atomic.load(Ordering::Acquire);
    let mut write_buf = Vec::with_capacity(WRITE_COALESCE_CAP);
    let mut stall_strikes: u32 = 0;

    loop {
        let iter_start_pos = current;
        let cur_end = end_atomic.load(Ordering::Acquire);
        if current > cur_end || stop.load(Ordering::Acquire) {
            break;
        }

        let mut req_headers = build_request_headers(&headers_map, cookies.as_deref(), user_agent.as_deref());
        let range_str = format!("bytes={}-{}", current, cur_end);
        req_headers.insert(RANGE, HeaderValue::from_str(&range_str)?);

        let mut retry_count = 0;
        let resp = loop {
            if retry_count >= 4 {
                return Err(anyhow!("Segment {} failed after retries", seg_id));
            }
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }

            // Refresh end in case scheduler shrunk us while retrying
            let latest_end = end_atomic.load(Ordering::Acquire);
            if current > latest_end {
                return Ok(());
            }
            if latest_end != cur_end {
                let updated_range = format!("bytes={}-{}", current, latest_end);
                req_headers.insert(RANGE, HeaderValue::from_str(&updated_range)?);
            }

            let t0 = Instant::now();
            match client.get(&url).headers(req_headers.clone()).send().await {
                Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT
                    && content_range_starts_at(
                        r.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()),
                        current,
                    ) =>
                {
                    let _ = event_tx.try_send(WorkerEvent::Ttfb {
                        slot_id: seg_id,
                        secs: t0.elapsed().as_secs_f64(),
                    });
                    break r;
                }
                Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                    return Err(anyhow!("Segment {} returned a mismatched Content-Range", seg_id));
                }
                Ok(r) if current == 0 && r.status() == reqwest::StatusCode::OK => {
                    let _ = event_tx.try_send(WorkerEvent::Ttfb {
                        slot_id: seg_id,
                        secs: t0.elapsed().as_secs_f64(),
                    });
                    break r;
                }
                Ok(r) if r.status() == reqwest::StatusCode::OK && current > 0 => {
                    return Err(anyhow!("Server does not support byte ranges (returned 200 OK)"));
                }
                Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || r.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE =>
                {
                    let retry_secs = r
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(2)
                        .clamp(1, 15);
                    let _ = event_tx.try_send(WorkerEvent::Refused {
                        retry_after_secs: retry_secs,
                    });
                    retry_count += 1;
                    sleep(Duration::from_secs(retry_secs)).await;
                }
                Ok(r) => {
                    warn!("Segment {} HTTP status {} unexpected for range request", seg_id, r.status());
                    retry_count += 1;
                    sleep(Duration::from_millis(400)).await;
                }
                Err(e) => {
                    warn!("Segment {} request error: {:#}", seg_id, e);
                    retry_count += 1;
                    sleep(Duration::from_millis(400)).await;
                }
            }
        };

        let mut stream = resp.bytes_stream();
        let mut buf_start = current;
        let mut acc_report_bytes: u64 = 0;
        let mut last_report_time = Instant::now();

        while let Ok(Some(item)) = tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            if stop.load(Ordering::Acquire) {
                break;
            }

            let cur_end = end_atomic.load(Ordering::Acquire);
            if current > cur_end {
                break;
            }

            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    warn!("Segment {} chunk read error: {:#}", seg_id, e);
                    break;
                }
            };

            let remaining = (cur_end - current + 1) as usize;
            let slice = if chunk.len() > remaining {
                &chunk[..remaining]
            } else {
                &chunk[..]
            };

            if slice.is_empty() {
                break;
            }

            // Coalesce into reusable 64KB buffer to slash pwrite64 syscall count, flushing
            // synchronously before advancing atomic counters (`WriteBeforeAccount` invariant).
            if write_buf.len() + slice.len() > WRITE_COALESCE_CAP && !write_buf.is_empty() {
                let latest_end = end_atomic.load(Ordering::Acquire);
                if buf_start <= latest_end {
                    let allowed = ((latest_end - buf_start + 1) as usize).min(write_buf.len());
                    storage.write_at_slice(buf_start, &write_buf[..allowed])?;
                    let flushed = allowed as u64;
                    pos_atomic.store(buf_start + flushed, Ordering::Release);
                    total_dl.fetch_add(flushed, Ordering::Relaxed);
                    acc_report_bytes += flushed;
                }
                write_buf.clear();
                buf_start = current;
            }

            if slice.len() >= WRITE_COALESCE_CAP {
                storage.write_at_slice(current, slice)?;
                let written = slice.len() as u64;
                current += written;
                buf_start = current;
                pos_atomic.store(current, Ordering::Release);
                total_dl.fetch_add(written, Ordering::Relaxed);
                acc_report_bytes += written;
            } else {
                if write_buf.is_empty() {
                    buf_start = current;
                }
                write_buf.extend_from_slice(slice);
                current += slice.len() as u64;
            }

            rate_limiter.throttle(slice.len()).await;

            // Batch channel arrivals (~64KB or 100ms) instead of allocating per TCP frame
            let dt = last_report_time.elapsed().as_secs_f64();
            if acc_report_bytes >= REPORT_BYTES_THRESHOLD || (acc_report_bytes > 0 && dt >= 0.1) {
                let _ = event_tx.try_send(WorkerEvent::Progress {
                    slot_id: seg_id,
                    bytes: acc_report_bytes,
                    dt_secs: dt,
                });
                acc_report_bytes = 0;
                last_report_time = Instant::now();
            }

            // Re-read end_atomic on every chunk: if shrunk mid-flight, stop reading immediately!
            let refreshed_end = end_atomic.load(Ordering::Acquire);
            if current > refreshed_end {
                break;
            }
        }

        // Flush any residual coalesced bytes strictly up to `end_atomic`
        if !write_buf.is_empty() {
            let latest_end = end_atomic.load(Ordering::Acquire);
            if buf_start <= latest_end {
                let allowed = ((latest_end - buf_start + 1) as usize).min(write_buf.len());
                storage.write_at_slice(buf_start, &write_buf[..allowed])?;
                let flushed = allowed as u64;
                current = buf_start + flushed;
                pos_atomic.store(current, Ordering::Release);
                total_dl.fetch_add(flushed, Ordering::Relaxed);
                acc_report_bytes += flushed;
            }
            write_buf.clear();
        }

        if acc_report_bytes > 0 {
            let _ = event_tx.try_send(WorkerEvent::Progress {
                slot_id: seg_id,
                bytes: acc_report_bytes,
                dt_secs: last_report_time.elapsed().as_secs_f64().max(0.01),
            });
        }

        if current > iter_start_pos {
            stall_strikes = 0;
        } else {
            stall_strikes += 1;
            if stall_strikes >= 4 {
                return Err(anyhow!("Segment {} stalled repeatedly without progress", seg_id));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn engine_multisegment_coalesced_pwrite_matches_sha256() {
        // Generate 2 MiB deterministic test payload
        let size = 2 * 1024 * 1024usize;
        let payload: Arc<Vec<u8>> = Arc::new((0..size).map(|i| (i.wrapping_mul(31) ^ (i >> 5)) as u8).collect());
        let expected_hash = Sha256::digest(&*payload);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_payload = payload.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let data = server_payload.clone();
                tokio::spawn(async move {
                    let mut req_buf = [0u8; 4096];
                    loop {
                        let Ok(n) = sock.read(&mut req_buf).await else { return };
                        if n == 0 { return; }
                        let req_str = String::from_utf8_lossy(&req_buf[..n]);
                        let is_head = req_str.starts_with("HEAD ");
                        let range_line = req_str
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim()));

                        if let Some(r_val) = range_line.and_then(|v| v.strip_prefix("bytes=")) {
                            let (s_str, e_str) = r_val.split_once('-').unwrap_or(("0", ""));
                            let start: usize = s_str.parse().unwrap_or(0);
                            let end: usize = if e_str.is_empty() {
                                data.len() - 1
                            } else {
                                e_str.parse::<usize>().unwrap_or(data.len() - 1).min(data.len() - 1)
                            };
                            let slice = &data[start..=end];
                            let hdr = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: keep-alive\r\n\r\n",
                                slice.len(), start, end, data.len()
                            );
                            if sock.write_all(hdr.as_bytes()).await.is_err() { return; }
                            if !is_head && sock.write_all(slice).await.is_err() { return; }
                        } else {
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: keep-alive\r\n\r\n",
                                data.len()
                            );
                            if sock.write_all(hdr.as_bytes()).await.is_err() { return; }
                            if !is_head && sock.write_all(&data).await.is_err() { return; }
                        }
                    }
                });
            }
        });

        let tmp_dir = std::env::temp_dir().join(format!("illusion_hydra_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let save_path = tmp_dir.join("test_2mb.bin");

        let state = AppState {
            tasks: Arc::new(std::sync::RwLock::new(Vec::new())),
            config_dir: tmp_dir.clone(),
            default_download_dir: tmp_dir.clone(),
        };

        let task = DownloadTask::new(
            format!("http://{}/test_2mb.bin", addr),
            "test_2mb.bin".to_string(),
            save_path.clone(),
            None,
            HashMap::new(),
            None,
            None,
        );
        let task_id = task.id.clone();
        state.tasks.write().unwrap().push(task);

        let engine = DownloadEngine::new(8);
        let (_tx, rx) = watch::channel(false);
        engine.run_task(task_id.clone(), state.clone(), rx).await.unwrap();

        let disk_bytes = std::fs::read(&save_path).unwrap();
        assert_eq!(disk_bytes.len(), size);
        let actual_hash = Sha256::digest(&disk_bytes);
        assert_eq!(actual_hash, expected_hash);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn engine_rescues_stalled_chunk_via_hydra_steal() {
        // 1 MiB file across 4 connections where the 1st sub-range request starting at 0 pauses
        // once halfway, forcing Hydra's silence/CUSUM steal scheduler to spawn a helper slot
        // and finish well before a stalled socket ever drains.
        let size = 1024 * 1024usize;
        let payload: Arc<Vec<u8>> = Arc::new((0..size).map(|i| (i.wrapping_mul(17) ^ (i >> 3)) as u8).collect());
        let expected_hash = Sha256::digest(&*payload);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stalled_once = Arc::new(AtomicBool::new(false));

        let server_payload = payload.clone();
        let stalled_flag = stalled_once.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let data = server_payload.clone();
                let flag = stalled_flag.clone();
                tokio::spawn(async move {
                    let mut req_buf = [0u8; 4096];
                    loop {
                        let Ok(n) = sock.read(&mut req_buf).await else { return };
                        if n == 0 { return; }
                        let req_str = String::from_utf8_lossy(&req_buf[..n]);
                        let is_head = req_str.starts_with("HEAD ");
                        let range_line = req_str
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim()));

                        if let Some(r_val) = range_line.and_then(|v| v.strip_prefix("bytes=")) {
                            let (s_str, e_str) = r_val.split_once('-').unwrap_or(("0", ""));
                            let start: usize = s_str.parse().unwrap_or(0);
                            let end: usize = if e_str.is_empty() {
                                data.len() - 1
                            } else {
                                e_str.parse::<usize>().unwrap_or(data.len() - 1).min(data.len() - 1)
                            };
                            let slice = &data[start..=end];
                            let hdr = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: keep-alive\r\n\r\n",
                                slice.len(), start, end, data.len()
                            );
                            if sock.write_all(hdr.as_bytes()).await.is_err() { return; }
                            if !is_head {
                                // Deliberately stall the very first worker on byte range start == 0
                                if start == 0 && !flag.swap(true, Ordering::SeqCst) {
                                    let first_part = slice.len().min(32 * 1024);
                                    let _ = sock.write_all(&slice[..first_part]).await;
                                    sleep(Duration::from_secs(3)).await;
                                    let _ = sock.write_all(&slice[first_part..]).await;
                                } else if sock.write_all(slice).await.is_err() {
                                    return;
                                }
                            }
                        } else {
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: keep-alive\r\n\r\n",
                                data.len()
                            );
                            if sock.write_all(hdr.as_bytes()).await.is_err() { return; }
                            if !is_head && sock.write_all(&data).await.is_err() { return; }
                        }
                    }
                });
            }
        });

        let tmp_dir = std::env::temp_dir().join(format!("illusion_rescue_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let save_path = tmp_dir.join("rescue_1mb.bin");

        let state = AppState {
            tasks: Arc::new(std::sync::RwLock::new(Vec::new())),
            config_dir: tmp_dir.clone(),
            default_download_dir: tmp_dir.clone(),
        };

        let task = DownloadTask::new(
            format!("http://{}/rescue_1mb.bin", addr),
            "rescue_1mb.bin".to_string(),
            save_path.clone(),
            None,
            HashMap::new(),
            None,
            None,
        );
        let task_id = task.id.clone();
        state.tasks.write().unwrap().push(task);

        let engine = DownloadEngine::new(4);
        let (_tx, rx) = watch::channel(false);
        engine.run_task(task_id.clone(), state.clone(), rx).await.unwrap();

        let final_segs = state.tasks.read().unwrap()[0].segments.clone();
        // Because range 0 stalled, Hydra's steal scheduler must have spawned > 4 segments!
        assert!(final_segs.len() > 4, "Expected steal scheduler to split stalled segment, got {} segments", final_segs.len());

        let disk_bytes = std::fs::read(&save_path).unwrap();
        assert_eq!(disk_bytes.len(), size);
        let actual_hash = Sha256::digest(&disk_bytes);
        assert_eq!(actual_hash, expected_hash);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

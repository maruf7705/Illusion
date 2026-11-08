mod ipc;
mod tray;

use eframe::egui::{self, Color32, Margin, Pos2, Rect, Rounding, Stroke, Vec2};
use illusion_core::{
    format_bytes, AppState, DownloadCategory, DownloadEngine, DownloadTask, RateLimiter,
    SegmentStatus, TaskStatus,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

#[derive(Default)]
struct DialogState {
    show_add_url: bool,
    input_text: String, // Multiline support for batch ingestion
    input_filename: String,
    input_save_dir: PathBuf,
}

struct ActiveDownloadWorker {
    cancel_tx: watch::Sender<bool>,
}

#[derive(PartialEq, Clone, Copy)]
enum SpeedPreset {
    Unlimited,
    Kb512,
    Mb1,
    Mb2,
    Mb5,
    Mb10,
}

struct IllusionApp {
    state: AppState,
    selected_category: DownloadCategory,
    selected_task_id: Option<String>,
    dialog: DialogState,
    max_connections: usize,
    speed_preset: SpeedPreset,
    engine: DownloadEngine,
    rate_limiter: RateLimiter,
    active_workers: HashMap<String, ActiveDownloadWorker>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
    pause_all_flag: Arc<AtomicBool>,
    resume_all_flag: Arc<AtomicBool>,
    ipc_rx: mpsc::UnboundedReceiver<String>,
}

impl IllusionApp {
    fn new(cc: &eframe::CreationContext<'_>, rt: Arc<tokio::runtime::Runtime>) -> Self {
        // Configure Hyprland dark theme (Catppuccin Mocha)
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(30, 30, 46); // Catppuccin Base #1e1e2e
        visuals.window_fill = Color32::from_rgb(24, 24, 37); // Crust
        visuals.faint_bg_color = Color32::from_rgb(36, 39, 58);
        visuals.extreme_bg_color = Color32::from_rgb(17, 17, 27);
        visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(36, 39, 58);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(49, 50, 68);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(69, 71, 90);
        visuals.widgets.active.bg_fill = Color32::from_rgb(137, 180, 250); // Sapphire
        visuals.window_rounding = Rounding::same(12.0);
        visuals.widgets.noninteractive.rounding = Rounding::same(8.0);
        visuals.widgets.inactive.rounding = Rounding::same(6.0);
        visuals.widgets.hovered.rounding = Rounding::same(6.0);
        visuals.widgets.active.rounding = Rounding::same(6.0);
        cc.egui_ctx.set_visuals(visuals);

        let state = AppState::load_or_default();
        let default_dir = state.default_download_dir.clone();

        let rate_limiter = RateLimiter::new(0);
        let engine = DownloadEngine::new(8).with_rate_limiter(rate_limiter.clone());

        let pause_all_flag = Arc::new(AtomicBool::new(false));
        let resume_all_flag = Arc::new(AtomicBool::new(false));

        // Spawn StatusNotifierItem Tray for Waybar / Hyprland
        // tray::spawn_tray(rt.clone(), pause_all_flag.clone(), resume_all_flag.clone());

        // Spawn Unix Domain Socket & Local HTTP Bridge IPC Daemon
        let (ipc_tx, ipc_rx) = mpsc::unbounded_channel::<String>();
        ipc::start_ipc_server(rt.clone(), state.clone(), ipc_tx, cc.egui_ctx.clone());

        let mut app = Self {
            state,
            selected_category: DownloadCategory::All,
            selected_task_id: None,
            dialog: DialogState {
                show_add_url: false,
                input_text: String::new(),
                input_filename: String::new(),
                input_save_dir: default_dir,
            },
            max_connections: 8,
            speed_preset: SpeedPreset::Unlimited,
            engine,
            rate_limiter,
            active_workers: HashMap::new(),
            tokio_rt: rt,
            pause_all_flag,
            resume_all_flag,
            ipc_rx,
        };

        // Reconcile tasks on launch (IDM-grade recovery):
        // 1. Any task left in Downloading state without an active worker from a previous session is set to Paused
        {
            let mut list = app.state.tasks.write().unwrap();
            let mut changed = false;
            for t in list.iter_mut() {
                if t.status == TaskStatus::Downloading {
                    t.status = TaskStatus::Paused;
                    t.speed = 0.0;
                    changed = true;
                }
            }
            if changed {
                let state_clone = app.state.clone();
                app.tokio_rt.spawn(async move {
                    let _ = state_clone.save_tasks().await;
                });
            }
        }

        // Auto-resume Queued tasks on launch
        let queued_tasks = {
            let list = app.state.tasks.read().unwrap();
            list.iter()
                .filter(|t| t.status == TaskStatus::Queued)
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        };
        for id in queued_tasks {
            app.start_download(id);
        }

        app
    }

    fn start_download(&mut self, task_id: String) {
        if self.active_workers.contains_key(&task_id) {
            return;
        }

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let engine = self.engine.clone();
        let state = self.state.clone();
        let tid = task_id.clone();

        self.tokio_rt.spawn(async move {
            let _ = engine.run_task(tid, state, cancel_rx).await;
        });

        self.active_workers
            .insert(task_id, ActiveDownloadWorker { cancel_tx });
    }

    fn pause_download(&mut self, task_id: &str) {
        if let Some(worker) = self.active_workers.remove(task_id) {
            let _ = worker.cancel_tx.send(true);
        }
    }

    fn pause_all(&mut self) {
        let ids: Vec<String> = self.active_workers.keys().cloned().collect();
        for id in ids {
            self.pause_download(&id);
        }
    }

    fn resume_all(&mut self) {
        let tasks = self.state.tasks.read().unwrap().clone();
        for task in tasks {
            if task.status == TaskStatus::Paused || task.status == TaskStatus::Queued {
                self.start_download(task.id);
            }
        }
    }

    fn apply_speed_limit(&self) {
        let bytes_per_sec = match self.speed_preset {
            SpeedPreset::Unlimited => 0,
            SpeedPreset::Kb512 => 512 * 1024,
            SpeedPreset::Mb1 => 1024 * 1024,
            SpeedPreset::Mb2 => 2 * 1024 * 1024,
            SpeedPreset::Mb5 => 5 * 1024 * 1024,
            SpeedPreset::Mb10 => 10 * 1024 * 1024,
        };
        self.rate_limiter.set_limit(bytes_per_sec);
    }
}

impl eframe::App for IllusionApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Continuous repaint: 100ms during active downloads, 400ms idle polling for IPC
        let has_active = !self.active_workers.is_empty();
        if has_active {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(Duration::from_millis(400));
        }

        // Process IPC messages from browser native host / HTTP bridge
        while let Ok(task_id) = self.ipc_rx.try_recv() {
            self.selected_task_id = Some(task_id.clone());
            self.start_download(task_id);
        }

        // Process Tray Flags
        if self.pause_all_flag.swap(false, Ordering::Relaxed) {
            self.pause_all();
        }
        if self.resume_all_flag.swap(false, Ordering::Relaxed) {
            self.resume_all();
        }

        // Clean up finished workers
        {
            let tasks = self.state.tasks.read().unwrap().clone();

            self.active_workers.retain(|id, _| {
                if let Some(t) = tasks.iter().find(|t| &t.id == id) {
                    t.status == TaskStatus::Downloading
                } else {
                    false
                }
            });
        }

        // 1. Top Header Toolbar
        egui::TopBottomPanel::top("top_toolbar")
            .frame(egui::Frame::none().fill(Color32::from_rgb(24, 24, 37)).inner_margin(Margin::same(10.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⚡ ILLUSION").strong().color(Color32::from_rgb(137, 180, 250)).size(18.0));
                    ui.separator();

                    if ui.button(egui::RichText::new("＋ Add Download").strong()).clicked() {
                        self.dialog.show_add_url = true;
                        self.dialog.input_text = String::new();
                        self.dialog.input_filename = String::new();

                        // Auto-populate from clipboard if clipboard contains URL
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            if let Ok(text) = clipboard.get_text() {
                                if text.contains("http://") || text.contains("https://") {
                                    self.dialog.input_text = text.trim().to_string();
                                }
                            }
                        }
                    }

                    if ui.button("▶ Resume All").clicked() {
                        self.resume_all();
                    }

                    if ui.button("⏸ Pause All").clicked() {
                        self.pause_all();
                    }

                    ui.separator();

                    let selected_id = self.selected_task_id.clone();
                    if let Some(ref id) = selected_id {
                        let task_opt = self.state.get_task_sync(id);

                        if let Some(ref task) = task_opt {
                            if task.status == TaskStatus::Completed {
                                if ui.button(
                                    egui::RichText::new("▶ Open File")
                                        .strong()
                                        .color(Color32::from_rgb(166, 227, 161)),
                                ).clicked() {
                                    if task.save_path.exists() {
                                        open_media_or_file(&task.save_path);
                                    }
                                }
                            } else if task.status == TaskStatus::Downloading {
                                if ui.button("⏸ Pause").clicked() {
                                    self.pause_download(id);
                                }
                            } else {
                                if ui.button("▶ Resume").clicked() {
                                    self.start_download(id.clone());
                                }
                            }

                            if ui.button("📁 Open Folder").clicked() {
                                if let Some(parent) = task.save_path.parent() {
                                    let _ = open::that(parent);
                                }
                            }

                            if ui.button("🗑 Remove").clicked() {
                                self.pause_download(id);
                                let tid = id.clone();
                                let state = self.state.clone();
                                self.tokio_rt.spawn(async move {
                                    state.remove_task(&tid).await;
                                });
                                self.selected_task_id = None;
                            }
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!("Speed: {}", self.calc_total_speed()));
                        ui.separator();

                        // Speed Limiter Selector
                        ui.label("Throttle:");
                        let old_preset = self.speed_preset;
                        egui::ComboBox::from_id_salt("speed_limit_combo")
                            .selected_text(match self.speed_preset {
                                SpeedPreset::Unlimited => "Unlimited",
                                SpeedPreset::Kb512 => "512 KB/s",
                                SpeedPreset::Mb1 => "1 MB/s",
                                SpeedPreset::Mb2 => "2 MB/s",
                                SpeedPreset::Mb5 => "5 MB/s",
                                SpeedPreset::Mb10 => "10 MB/s",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.speed_preset, SpeedPreset::Unlimited, "Unlimited");
                                ui.selectable_value(&mut self.speed_preset, SpeedPreset::Kb512, "512 KB/s");
                                ui.selectable_value(&mut self.speed_preset, SpeedPreset::Mb1, "1 MB/s");
                                ui.selectable_value(&mut self.speed_preset, SpeedPreset::Mb2, "2 MB/s");
                                ui.selectable_value(&mut self.speed_preset, SpeedPreset::Mb5, "5 MB/s");
                                ui.selectable_value(&mut self.speed_preset, SpeedPreset::Mb10, "10 MB/s");
                            });

                        if self.speed_preset != old_preset {
                            self.apply_speed_limit();
                        }

                        ui.separator();
                        ui.add(egui::Slider::new(&mut self.max_connections, 1..=32).text("Threads"));
                    });
                });
            });

        // 2. Left Sidebar (Categories)
        egui::SidePanel::left("left_sidebar")
            .resizable(false)
            .default_width(180.0)
            .frame(egui::Frame::none().fill(Color32::from_rgb(30, 30, 46)).inner_margin(Margin::same(12.0)))
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("CATEGORIES").size(11.0).color(Color32::from_rgb(166, 173, 200)));
                ui.add_space(8.0);

                let categories = [
                    DownloadCategory::All,
                    DownloadCategory::Compressed,
                    DownloadCategory::Programs,
                    DownloadCategory::Video,
                    DownloadCategory::Music,
                    DownloadCategory::Documents,
                    DownloadCategory::Images,
                    DownloadCategory::Misc,
                ];

                for cat in categories {
                    let is_selected = self.selected_category == cat;
                    let label = format!("{} {}", cat.icon(), cat.display_name());
                    let text = if is_selected {
                        egui::RichText::new(label).strong().color(Color32::from_rgb(137, 180, 250))
                    } else {
                        egui::RichText::new(label).color(Color32::from_rgb(205, 214, 244))
                    };

                    if ui.selectable_label(is_selected, text).clicked() {
                        self.selected_category = cat;
                    }
                }
            });

        // 3. Bottom IDM Segment Visualizer Panel
        if let Some(ref sel_id) = self.selected_task_id.clone() {
            let task_opt = self.state.get_task_sync(sel_id);
            if let Some(task) = task_opt {
                egui::TopBottomPanel::bottom("bottom_segment_panel")
                    .resizable(true)
                    .default_height(160.0)
                    .frame(egui::Frame::none().fill(Color32::from_rgb(24, 24, 37)).inner_margin(Margin::same(12.0)))
                    .show(ctx, |ui| {
                        render_segment_visualizer(ui, &task);
                    });
            }
        }

        // 4. Central Task List Panel
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(30, 30, 46)).inner_margin(Margin::same(12.0)))
            .show(ctx, |ui| {
                let tasks = self.state.tasks.read().unwrap().clone();

                let filtered: Vec<_> = tasks
                    .into_iter()
                    .filter(|t| {
                        if self.selected_category == DownloadCategory::All {
                            true
                        } else {
                            t.category == self.selected_category
                        }
                    })
                    .collect();

                let mut action_select: Option<String> = None;
                let mut action_open_file: Option<PathBuf> = None;
                let mut action_resume: Option<String> = None;
                let mut action_pause: Option<String> = None;

                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("task_grid")
                        .striped(true)
                        .spacing([14.0, 8.0])
                        .show(ui, |ui| {
                            // Headers
                            ui.label(egui::RichText::new("File Name").strong());
                            ui.label(egui::RichText::new("Size").strong());
                            ui.label(egui::RichText::new("Status").strong());
                            ui.label(egui::RichText::new("Progress").strong());
                            ui.label(egui::RichText::new("Speed").strong());
                            ui.label(egui::RichText::new("Time Left").strong());
                            ui.end_row();

                            for task in filtered {
                                let is_selected = self.selected_task_id.as_deref() == Some(&task.id);
                                let name_label = format!("{} {}", task.category.icon(), task.filename);

                                let label_resp = ui.selectable_label(is_selected, name_label);
                                if label_resp.clicked() {
                                    action_select = Some(task.id.clone());
                                }
                                if label_resp.double_clicked() {
                                    action_select = Some(task.id.clone());
                                    if task.status == TaskStatus::Completed && task.save_path.exists() {
                                        action_open_file = Some(task.save_path.clone());
                                    } else if task.status == TaskStatus::Paused {
                                        action_resume = Some(task.id.clone());
                                    } else if task.status == TaskStatus::Downloading {
                                        action_pause = Some(task.id.clone());
                                    }
                                }

                                ui.label(task.formatted_size());

                                let status_color = match task.status {
                                    TaskStatus::Downloading => Color32::from_rgb(137, 180, 250),
                                    TaskStatus::Completed => Color32::from_rgb(166, 227, 161),
                                    TaskStatus::Paused => Color32::from_rgb(249, 226, 175),
                                    TaskStatus::Error => Color32::from_rgb(243, 139, 168),
                                    TaskStatus::Queued => Color32::from_rgb(166, 173, 200),
                                };
                                ui.label(egui::RichText::new(format!("{:?}", task.status)).color(status_color));

                                // Progress bar
                                let pct = task.progress_percent() / 100.0;
                                ui.add(egui::ProgressBar::new(pct).desired_width(140.0));

                                ui.label(task.formatted_speed());
                                ui.label(task.formatted_eta());
                                ui.end_row();
                            }
                        });
                });

                if let Some(id) = action_select {
                    self.selected_task_id = Some(id);
                }
                if let Some(path) = action_open_file {
                    open_media_or_file(&path);
                }
                if let Some(id) = action_resume {
                    self.start_download(id);
                }
                if let Some(id) = action_pause {
                    self.pause_download(&id);
                }
            });

        // 5. Add URL / Multiline Batch Download Modal
        if self.dialog.show_add_url {
            #[derive(PartialEq)]
            enum ModalAction {
                None,
                DownloadNow,
                QueueLater,
                Cancel,
            }
            let mut action = ModalAction::None;

            egui::Window::new("Add Download / Batch Queue")
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .fixed_size([580.0, 340.0])
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Enter download URL(s) - one per line for batch:").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("📋 Paste from Clipboard").clicked() {
                                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                    if let Ok(text) = clipboard.get_text() {
                                        self.dialog.input_text = text.trim().to_string();
                                    }
                                }
                            }
                        });
                    });

                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::multiline(&mut self.dialog.input_text)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY)
                            .hint_text("https://example.com/file1.zip\nhttps://example.com/file2.tar.gz"),
                    );

                    // Count valid URLs in input
                    let urls: Vec<&str> = self
                        .dialog
                        .input_text
                        .lines()
                        .map(|l| l.trim())
                        .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
                        .collect();

                    if urls.len() > 1 {
                        ui.label(
                            egui::RichText::new(format!("📦 Batch Mode: {} URLs detected", urls.len()))
                                .color(Color32::from_rgb(166, 227, 161))
                                .strong(),
                        );
                    }

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("Save To:");
                        ui.label(self.dialog.input_save_dir.to_string_lossy().to_string());
                        if ui.button("Browse...").clicked() {
                            if let Some(folder) = rfd::FileDialog::new()
                                .set_directory(&self.dialog.input_save_dir)
                                .pick_folder()
                            {
                                self.dialog.input_save_dir = folder;
                            }
                        }
                    });

                    if urls.len() <= 1 {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label("File Name (Optional):");
                            ui.text_edit_singleline(&mut self.dialog.input_filename);
                        });
                    }

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        let btn_text = if urls.len() > 1 {
                            format!("▶ Batch Download All ({})", urls.len())
                        } else {
                            "▶ Download Now".to_string()
                        };

                        if ui.button(egui::RichText::new(btn_text).strong()).clicked() {
                            if !urls.is_empty() {
                                action = ModalAction::DownloadNow;
                            }
                        }

                        if ui.button("Queue for Later").clicked() {
                            if !urls.is_empty() {
                                action = ModalAction::QueueLater;
                            }
                        }

                        if ui.button("Cancel").clicked() {
                            action = ModalAction::Cancel;
                        }
                    });
                });

            let urls: Vec<String> = self
                .dialog
                .input_text
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
                .collect();

            if action == ModalAction::DownloadNow || action == ModalAction::QueueLater {
                let save_dir = self.dialog.input_save_dir.clone();
                let custom_name = self.dialog.input_filename.trim().to_string();
                let start_now = action == ModalAction::DownloadNow;

                for (idx, url) in urls.into_iter().enumerate() {
                    let mut filename = if idx == 0 && !custom_name.is_empty() {
                        custom_name.clone()
                    } else {
                        url.split('/').last().unwrap_or("download").to_string()
                    };
                    filename = illusion_core::prober::sanitize_filename(&filename);
                    let save_path = save_dir.join(&filename);

                    let task = DownloadTask::new(
                        url,
                        filename,
                        save_path,
                        None,
                        HashMap::new(),
                        None,
                        Some("Illusion/0.1.0".to_string()),
                    );

                    let task_id = task.id.clone();
                    let state = self.state.clone();
                    self.tokio_rt.spawn(async move {
                        state.add_task(task).await;
                    });

                    if start_now {
                        self.start_download(task_id);
                    }
                }
                self.dialog.show_add_url = false;
            } else if action == ModalAction::Cancel {
                self.dialog.show_add_url = false;
            }
        }
    }
}

impl IllusionApp {
    fn calc_total_speed(&self) -> String {
        let tasks = self.state.tasks.read().unwrap().clone();

        let total: f64 = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Downloading)
            .map(|t| t.speed)
            .sum();

        if total > 0.0 {
            format!("{}/s", format_bytes(total as u64))
        } else {
            "0 B/s".to_string()
        }
    }
}

/// Reliably opens a media or general file using VLC (for audio/video) or system default (xdg-open / open::that)
fn open_media_or_file(path: &Path) {
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        let is_media = ["mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "mp3", "wav", "flac", "m4a", "aac", "ogg"]
            .contains(&ext.as_str());

        if is_media {
            // First try launching VLC directly with decoupled stdio
            if let Ok(mut child) = std::process::Command::new("vlc")
                .arg(&p)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                let _ = child.wait();
                return;
            }
        }

        // Fallback to xdg-open
        if let Ok(mut child) = std::process::Command::new("xdg-open")
            .arg(&p)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            let _ = child.wait();
            return;
        }

        // Final fallback
        let _ = open::that(&p);
    });
}

/// Renders the iconic IDM Multi-Segment Visualizer Bar
fn render_segment_visualizer(ui: &mut egui::Ui, task: &DownloadTask) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("⚡ Active Connection Segments: {}", task.filename)).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(format!("Threads: {}", task.segments.len()));
        });
    });

    ui.add_space(6.0);

    // Segment progress graphic container
    let (rect, _response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), egui::Sense::hover());
    let painter = ui.painter();

    // Background track
    painter.rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(17, 17, 27));
    painter.rect_stroke(rect, Rounding::same(6.0), Stroke::new(1.0_f32, Color32::from_rgb(49, 50, 68)));

    if let Some(total_size) = task.total_size {
        if total_size > 0 && !task.segments.is_empty() {
            let width = rect.width();

            // Segment palette (IDM / Nord / Catppuccin color scheme)
            let colors = [
                Color32::from_rgb(137, 180, 250), // Sapphire
                Color32::from_rgb(166, 227, 161), // Green
                Color32::from_rgb(249, 226, 175), // Yellow
                Color32::from_rgb(245, 194, 231), // Pink
                Color32::from_rgb(148, 226, 213), // Teal
                Color32::from_rgb(250, 179, 135), // Peach
                Color32::from_rgb(203, 166, 247), // Mauve
                Color32::from_rgb(180, 190, 254), // Lavender
            ];

            for (i, seg) in task.segments.iter().enumerate() {
                let color = colors[i % colors.len()];

                let seg_start_x = rect.min.x + (seg.start as f32 / total_size as f32) * width;
                let seg_end_x = rect.min.x + (seg.end as f32 / total_size as f32) * width;
                let seg_cur_x = rect.min.x + (seg.current as f32 / total_size as f32) * width;

                // Downloaded portion
                let dl_width = (seg_cur_x - seg_start_x).max(0.0);
                if dl_width > 0.0 {
                    let seg_rect = Rect::from_min_size(
                        Pos2::new(seg_start_x, rect.min.y + 2.0),
                        Vec2::new(dl_width, rect.height() - 4.0),
                    );
                    painter.rect_filled(seg_rect, Rounding::same(3.0), color);
                }

                // Segment boundary tick
                painter.line_segment(
                    [Pos2::new(seg_end_x, rect.min.y), Pos2::new(seg_end_x, rect.max.y)],
                    Stroke::new(1.0_f32, Color32::from_rgb(69, 71, 90)),
                );
            }
        }
    }

    ui.add_space(8.0);

    // Segment summary text
    ui.horizontal(|ui| {
        for (i, seg) in task.segments.iter().enumerate().take(8) {
            let status_char = match seg.status {
                SegmentStatus::Downloading => "⬇",
                SegmentStatus::Completed => "✓",
                SegmentStatus::Pending => "○",
                SegmentStatus::Failed => "✗",
            };
            ui.label(format!("#{} {} {:.0}%", i + 1, status_char, seg.progress_ratio() * 100.0));
        }
    });
}

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt::init();
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime"),
    );
    let _guard = rt.enter();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Illusion Download Manager")
            .with_inner_size([980.0, 620.0])
            .with_min_inner_size([640.0, 400.0])
            .with_app_id("illusion"),
        ..Default::default()
    };

    let rt_clone = rt.clone();
    eframe::run_native(
        "Illusion",
        native_options,
        Box::new(move |cc| Ok(Box::new(IllusionApp::new(cc, rt_clone)))),
    )
}

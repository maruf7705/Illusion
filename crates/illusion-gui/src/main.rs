use eframe::egui::{self, Color32, Margin, Pos2, Rect, Rounding, Stroke, Vec2};
use illusion_core::{
    format_bytes, AppState, DownloadCategory, DownloadEngine, DownloadTask, SegmentStatus,
    TaskStatus,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Default)]
struct DialogState {
    show_add_url: bool,
    input_url: String,
    input_filename: String,
    input_save_dir: PathBuf,
}

struct ActiveDownloadWorker {
    cancel_tx: watch::Sender<bool>,
}

struct IllusionApp {
    state: AppState,
    selected_category: DownloadCategory,
    selected_task_id: Option<String>,
    dialog: DialogState,
    max_connections: usize,
    engine: DownloadEngine,
    active_workers: HashMap<String, ActiveDownloadWorker>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
}

impl IllusionApp {
    fn new(cc: &eframe::CreationContext<'_>, rt: Arc<tokio::runtime::Runtime>) -> Self {
        // Configure Hyprland dark theme
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

        Self {
            state,
            selected_category: DownloadCategory::All,
            selected_task_id: None,
            dialog: DialogState {
                show_add_url: false,
                input_url: String::new(),
                input_filename: String::new(),
                input_save_dir: default_dir,
            },
            max_connections: 8,
            engine: DownloadEngine::new(8),
            active_workers: HashMap::new(),
            tokio_rt: rt,
        }
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
}

impl eframe::App for IllusionApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Continuous repaint during active downloads for smooth 60fps graphs
        let has_active = !self.active_workers.is_empty();
        if has_active {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        // Clean up finished workers
        {
            let tasks = self.tokio_rt.block_on(async {
                self.state.tasks.read().await.clone()
            });

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

                    if ui.button(egui::RichText::new("＋ Add URL").strong()).clicked() {
                        self.dialog.show_add_url = true;
                        self.dialog.input_url = String::new();
                        self.dialog.input_filename = String::new();
                    }

                    ui.separator();

                    let selected_id = self.selected_task_id.clone();
                    if let Some(ref id) = selected_id {
                        if ui.button("▶ Resume").clicked() {
                            self.start_download(id.clone());
                        }

                        if ui.button("⏸ Pause").clicked() {
                            self.pause_download(id);
                        }

                        if ui.button("🗑 Remove").clicked() {
                            self.pause_download(id);
                            let tid = id.clone();
                            let state = self.state.clone();
                            self.tokio_rt.block_on(async {
                                state.remove_task(&tid).await;
                            });
                            self.selected_task_id = None;
                        }

                        if ui.button("📁 Open Folder").clicked() {
                            let state = self.state.clone();
                            let tid = id.clone();
                            if let Some(task) = self.tokio_rt.block_on(async { state.get_task(&tid).await }) {
                                if let Some(parent) = task.save_path.parent() {
                                    let _ = open::that(parent);
                                }
                            }
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!("Speed: {}", self.calc_total_speed()));
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
            let task_opt = self.tokio_rt.block_on(async { self.state.get_task(sel_id).await });
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
                let tasks = self.tokio_rt.block_on(async {
                    self.state.tasks.read().await.clone()
                });

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

                                if ui.selectable_label(is_selected, name_label).clicked() {
                                    self.selected_task_id = Some(task.id.clone());
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
            });

        // 5. Add URL Dialog Modal
        if self.dialog.show_add_url {
            let mut close = false;
            let mut start_now = false;

            egui::Window::new("Add Download")
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .fixed_size([520.0, 260.0])
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Enter the address (URL) to download:").strong());

                    ui.add_space(4.0);
                    ui.text_edit_singleline(&mut self.dialog.input_url);

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

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("File Name (Optional):");
                        ui.text_edit_singleline(&mut self.dialog.input_filename);
                    });

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        if ui.button(egui::RichText::new("▶ Download Now").strong()).clicked() {
                            if !self.dialog.input_url.trim().is_empty() {
                                start_now = true;
                                close = true;
                            }
                        }

                        if ui.button("Download Later").clicked() {
                            if !self.dialog.input_url.trim().is_empty() {
                                close = true;
                            }
                        }

                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });

            if start_now || (close && !self.dialog.input_url.trim().is_empty()) {
                let url = self.dialog.input_url.trim().to_string();
                let mut filename = self.dialog.input_filename.trim().to_string();
                if filename.is_empty() {
                    filename = url.split('/').last().unwrap_or("download").to_string();
                }
                filename = illusion_core::prober::sanitize_filename(&filename);
                let save_path = self.dialog.input_save_dir.join(&filename);

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
                self.tokio_rt.block_on(async {
                    state.add_task(task).await;
                });

                if start_now {
                    self.start_download(task_id);
                }
            }

            if close {
                self.dialog.show_add_url = false;
            }
        }
    }
}

impl IllusionApp {
    fn calc_total_speed(&self) -> String {
        let tasks = self.tokio_rt.block_on(async {
            self.state.tasks.read().await.clone()
        });

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
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime"),
    );

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Illusion Download Manager")
            .with_inner_size([960.0, 600.0])
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

use ksni::{menu::*, MenuItem, ToolTip, Tray, TrayMethods};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::info;

pub struct IllusionTray {
    pub pause_all_flag: Arc<AtomicBool>,
    pub resume_all_flag: Arc<AtomicBool>,
}

impl Tray for IllusionTray {
    fn id(&self) -> String {
        "illusion".to_string()
    }

    fn title(&self) -> String {
        "Illusion Download Manager".to_string()
    }

    fn icon_name(&self) -> String {
        "illusion".to_string()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Illusion".to_string(),
            description: "High-Performance IDM for Linux & Hyprland".to_string(),
            icon_name: "illusion".to_string(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let pause_flag = self.pause_all_flag.clone();
        let resume_flag = self.resume_all_flag.clone();

        vec![
            StandardItem {
                label: "⚡ Open Illusion".into(),
                activate: Box::new(|_| {
                    let _ = std::process::Command::new("illusion").spawn();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "⏸ Pause All Downloads".into(),
                activate: Box::new(move |_| {
                    pause_flag.store(true, Ordering::Relaxed);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "▶ Resume All Downloads".into(),
                activate: Box::new(move |_| {
                    resume_flag.store(true, Ordering::Relaxed);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "✕ Quit".into(),
                activate: Box::new(|_| {
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub fn spawn_tray(
    rt: Arc<tokio::runtime::Runtime>,
    pause_flag: Arc<AtomicBool>,
    resume_flag: Arc<AtomicBool>,
) {
    let tray = IllusionTray {
        pause_all_flag: pause_flag,
        resume_all_flag: resume_flag,
    };

    rt.spawn(async move {
        match tray.spawn().await {
            Ok(_) => info!("⚡ Registered StatusNotifierItem (SNI) on D-Bus for Waybar/Hyprland"),
            Err(e) => tracing::warn!("StatusNotifierItem tray registration failed: {:#}", e),
        }
    });
}

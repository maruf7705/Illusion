use crate::segment::Segment;
use crate::media::{MediaCandidate, MediaCandidateKind, RequestContext};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Queued,
    Downloading,
    Paused,
    Completed,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadCategory {
    All,
    Compressed,
    Programs,
    Video,
    Music,
    Documents,
    Images,
    Misc,
}

impl DownloadCategory {
    pub fn from_filename(name: &str) -> Self {
        let ext = Path::new(name)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        match ext.as_str() {
            "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "tgz" => {
                DownloadCategory::Compressed
            }
            "exe" | "bin" | "deb" | "rpm" | "pkg" | "appimage" | "dmg" | "iso" | "msi" | "sh" => {
                DownloadCategory::Programs
            }
            "mp4" | "mkv" | "avi" | "webm" | "mov" | "flv" | "wmv" | "ts" | "m4v" => {
                DownloadCategory::Video
            }
            "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" => DownloadCategory::Music,
            "pdf" | "doc" | "docx" | "txt" | "epub" | "ppt" | "pptx" | "xls" | "xlsx" | "odt" => {
                DownloadCategory::Documents
            }
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "ico" => {
                DownloadCategory::Images
            }
            _ => DownloadCategory::Misc,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            DownloadCategory::All => "All Downloads",
            DownloadCategory::Compressed => "Compressed",
            DownloadCategory::Programs => "Programs",
            DownloadCategory::Video => "Video",
            DownloadCategory::Music => "Music",
            DownloadCategory::Documents => "Documents",
            DownloadCategory::Images => "Images",
            DownloadCategory::Misc => "Other",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            DownloadCategory::All => "📦",
            DownloadCategory::Compressed => "🗜️",
            DownloadCategory::Programs => "⚙️",
            DownloadCategory::Video => "🎬",
            DownloadCategory::Music => "🎵",
            DownloadCategory::Documents => "📄",
            DownloadCategory::Images => "🖼️",
            DownloadCategory::Misc => "📁",
        }
    }

    pub fn accent_color(&self) -> (u8, u8, u8) {
        match self {
            DownloadCategory::All => (205, 214, 244),
            DownloadCategory::Compressed => (249, 226, 175),
            DownloadCategory::Programs => (203, 166, 247),
            DownloadCategory::Video => (137, 180, 250),
            DownloadCategory::Music => (245, 194, 231),
            DownloadCategory::Documents => (250, 179, 135),
            DownloadCategory::Images => (166, 227, 161),
            DownloadCategory::Misc => (180, 190, 254),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTask {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub save_path: PathBuf,
    pub total_size: Option<u64>,
    pub downloaded_size: u64,
    pub status: TaskStatus,
    pub error_message: Option<String>,
    #[serde(skip)]
    pub speed: f64, // bytes/sec
    #[serde(skip)]
    pub eta_seconds: Option<u64>,
    pub segments: Vec<Segment>,
    pub category: DownloadCategory,
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub audio_url: Option<String>,
    #[serde(default)]
    pub quality: Option<String>,
    /// The browser-captured request set. Older tasks omit this and continue to
    /// use the legacy URL fields.
    #[serde(default)]
    pub media_candidate: Option<MediaCandidate>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl DownloadTask {
    pub fn new(
        url: String,
        filename: String,
        save_path: PathBuf,
        total_size: Option<u64>,
        headers: HashMap<String, String>,
        cookies: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        let category = DownloadCategory::from_filename(&filename);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            url,
            filename,
            save_path,
            total_size,
            downloaded_size: 0,
            status: TaskStatus::Queued,
            error_message: None,
            speed: 0.0,
            eta_seconds: None,
            segments: Vec::new(),
            category,
            headers,
            cookies,
            user_agent,
            audio_url: None,
            quality: None,
            media_candidate: None,
            created_at: Utc::now(),
            completed_at: None,
        }
    }

    pub fn from_media_candidate(
        candidate: MediaCandidate,
        filename: String,
        save_path: PathBuf,
    ) -> Self {
        let mut task = Self::new(
            candidate.primary.url.clone(),
            filename,
            save_path,
            candidate.expected_size(),
            candidate.primary.normalized_headers(),
            candidate.primary.cookies.clone(),
            candidate.primary.user_agent.clone(),
        );
        task.audio_url = candidate.audio.as_ref().map(|audio| audio.url.clone());
        task.quality = candidate.quality.clone();
        task.media_candidate = Some(candidate);
        task
    }

    pub fn primary_request(&self) -> RequestContext {
        self.media_candidate
            .as_ref()
            .map(|candidate| candidate.primary.clone())
            .unwrap_or_else(|| RequestContext {
                url: self.url.clone(),
                headers: self.headers.clone(),
                cookies: self.cookies.clone(),
                user_agent: self.user_agent.clone(),
                referer: self.headers.get("referer").cloned().or_else(|| self.headers.get("Referer").cloned()),
                ..RequestContext::default()
            })
    }

    pub fn audio_request(&self) -> Option<RequestContext> {
        self.media_candidate
            .as_ref()
            .and_then(|candidate| candidate.audio.clone())
            .or_else(|| {
                self.audio_url.as_ref().map(|url| RequestContext {
                    url: url.clone(),
                    headers: self.headers.clone(),
                    cookies: self.cookies.clone(),
                    user_agent: self.user_agent.clone(),
                    referer: self.headers.get("referer").cloned().or_else(|| self.headers.get("Referer").cloned()),
                    ..RequestContext::default()
                })
            })
    }

    pub fn candidate_kind(&self) -> MediaCandidateKind {
        self.media_candidate
            .as_ref()
            .map(|candidate| candidate.kind)
            .unwrap_or_else(|| if self.audio_url.is_some() { MediaCandidateKind::PairedAudioVideo } else { MediaCandidateKind::Progressive })
    }

    pub fn progress_percent(&self) -> f32 {
        if let Some(total) = self.total_size {
            if total > 0 {
                return (self.downloaded_size as f32 / total as f32 * 100.0).clamp(0.0, 100.0);
            }
        }
        0.0
    }

    pub fn formatted_size(&self) -> String {
        match self.total_size {
            Some(total) => format!("{}/{}", format_bytes(self.downloaded_size), format_bytes(total)),
            None => format_bytes(self.downloaded_size),
        }
    }

    pub fn formatted_speed(&self) -> String {
        if self.status == TaskStatus::Downloading && self.speed > 0.0 {
            format!("{}/s", format_bytes(self.speed as u64))
        } else {
            "-".to_string()
        }
    }

    pub fn formatted_eta(&self) -> String {
        if let Some(eta) = self.eta_seconds {
            if eta < 60 {
                format!("{}s", eta)
            } else if eta < 3600 {
                format!("{}m {}s", eta / 60, eta % 60)
            } else {
                format!("{}h {}m", eta / 3600, (eta % 3600) / 60)
            }
        } else {
            "-".to_string()
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

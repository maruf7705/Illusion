// Illusion Multi-Engine Stream Dispatcher
// Dispatches downloads based on media classification: Direct Range, HLS/DASH, or Audio+Video Mux

use crate::muxer::mux_video_and_audio;
use crate::task::DownloadTask;
use anyhow::Result;
use std::path::Path;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDispatchMode {
    /// Mode A: Standard HTTP 206 Partial Content with fallocate + pwrite + dynamic halving
    DirectByteRange,
    /// Mode B: HLS (.m3u8) or DASH (.mpd) segmented manifest streams
    HlsDashSegmented,
    /// Mode C: Separated Video + Audio adaptive streams muxed via FFmpeg
    SeparatedVideoAudioMux,
}

pub struct StreamEngine;

impl StreamEngine {
    pub fn classify_task(task: &DownloadTask) -> StreamDispatchMode {
        if task.audio_url.is_some() {
            return StreamDispatchMode::SeparatedVideoAudioMux;
        }

        let url_lower = task.url.to_lowercase();
        if url_lower.contains(".m3u8") || url_lower.contains(".mpd") {
            return StreamDispatchMode::HlsDashSegmented;
        }

        StreamDispatchMode::DirectByteRange
    }

    /// Mode C Muxer: Combines separate video and audio tracks after download completion
    pub async fn finalize_separated_mux(
        video_path: &Path,
        audio_path: &Path,
        output_path: &Path,
    ) -> Result<()> {
        info!(
            "⚡ StreamEngine: Muxing video ({:?}) and audio ({:?}) into final destination ({:?})",
            video_path, audio_path, output_path
        );

        mux_video_and_audio(video_path, audio_path, output_path).await?;

        // Cleanup temporary intermediate files
        let _ = tokio::fs::remove_file(video_path).await;
        let _ = tokio::fs::remove_file(audio_path).await;

        Ok(())
    }
}

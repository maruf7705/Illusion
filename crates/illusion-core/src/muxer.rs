use anyhow::{anyhow, Result};
use std::path::Path;
use tokio::process::Command;
use tracing::{info, warn};

/// Multiplexes separate video and audio tracks (e.g. YouTube 1080p/4K adaptive streams)
/// into a unified container using zero-reencoding stream copy (-c copy).
/// If direct stream copy fails (e.g. Opus audio into standard MP4), falls back to
/// copying video stream directly and transrating audio to standard AAC.
pub async fn mux_video_and_audio(
    video_path: &Path,
    audio_path: &Path,
    output_path: &Path,
) -> Result<()> {
    let ext = output_path.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_lowercase();
    let output_temp = output_path.with_extension(format!("muxing_temp.{}", ext));

    info!("⚡ Muxer: Attempting stream-copy muxing for {:?} + {:?} into .{}", video_path, audio_path, ext);

    // Attempt 1: Direct stream copy (instant, 0% CPU overhead, matches official IDM)
    let status = Command::new("ffmpeg")
        .arg("-i")
        .arg(video_path)
        .arg("-i")
        .arg(audio_path)
        .arg("-c")
        .arg("copy")
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("1:a:0")
        .arg("-shortest")
        .arg("-y")
        .arg(&output_temp)
        .output()
        .await;

    let success = match status {
        Ok(ref out) if out.status.success() => true,
        Ok(out) => {
            warn!("Direct stream copy exited with status {:?}. Retrying with AAC audio fallback...", out.status.code());
            false
        }
        Err(e) => {
            warn!("Failed to invoke ffmpeg: {}. Checking raw fallback...", e);
            false
        }
    };

    if !success {
        // Attempt 2: Copy video directly (-c:v copy) and encode audio to standard container audio
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-i")
            .arg(video_path)
            .arg("-i")
            .arg(audio_path)
            .arg("-c:v")
            .arg("copy");

        if ext == "webm" {
            cmd.arg("-c:a").arg("libopus").arg("-b:a").arg("128k");
        } else {
            cmd.arg("-c:a").arg("aac").arg("-b:a").arg("192k");
        }

        let fallback_status = cmd
            .arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("1:a:0")
            .arg("-shortest")
            .arg("-y")
            .arg(&output_temp)
            .output()
            .await?;

        if !fallback_status.status.success() {
            let stderr = String::from_utf8_lossy(&fallback_status.stderr);
            return Err(anyhow!("FFmpeg muxing failed: {}", stderr));
        }
    }

    // Atomically replace target output
    tokio::fs::rename(&output_temp, output_path).await?;

    // Clean up intermediate source streams
    let _ = tokio::fs::remove_file(video_path).await;
    let _ = tokio::fs::remove_file(audio_path).await;

    info!("⚡ Muxer: Successfully assembled final file: {:?}", output_path);
    Ok(())
}

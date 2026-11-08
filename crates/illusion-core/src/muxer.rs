use anyhow::{anyhow, Result};
use std::path::Path;
use tokio::process::Command;

/// Multiplexes separate video and audio tracks (e.g. YouTube 1080p/4K adaptive streams)
/// into a unified container using zero-reencoding stream copy (-c copy).
pub async fn mux_video_and_audio(
    video_path: &Path,
    audio_path: &Path,
    output_path: &Path,
) -> Result<()> {
    let output_temp = output_path.with_extension("muxing_temp.mp4");

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
        .await?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(anyhow!("FFmpeg muxing failed: {}", stderr));
    }

    // Atomically replace target output
    tokio::fs::rename(&output_temp, output_path).await?;

    // Clean up intermediate source streams
    let _ = tokio::fs::remove_file(video_path).await;
    let _ = tokio::fs::remove_file(audio_path).await;

    Ok(())
}

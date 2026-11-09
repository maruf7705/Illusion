use anyhow::{anyhow, Result};
use std::path::Path;
use tokio::process::Command;
use tracing::info;

/// Multiplexes separately captured video and audio using stream copy.  The
/// caller selects a compatible container from observed codecs/MIME types; a
/// mux error remains an error instead of producing a partial video.
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

    match status {
        Ok(ref out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!(
                "FFmpeg stream-copy muxing failed with status {:?}: {}",
                out.status.code(),
                stderr
            ));
        }
        Err(e) => {
            return Err(anyhow!("Failed to invoke FFmpeg for muxing: {}", e));
        }
    }

    verify_media_container(&output_temp).await?;

    // Atomically replace target output
    tokio::fs::rename(&output_temp, output_path).await?;

    // Clean up intermediate source streams
    let _ = tokio::fs::remove_file(video_path).await;
    let _ = tokio::fs::remove_file(audio_path).await;

    info!("⚡ Muxer: Successfully assembled final file: {:?}", output_path);
    Ok(())
}

/// Uses the already required FFmpeg binary to reject a syntactically invalid
/// or unreadable final container before it is marked completed.
pub async fn verify_media_container(path: &Path) -> Result<()> {
    let verification = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-f")
        .arg("null")
        .arg("-")
        .output()
        .await?;
    if !verification.status.success() {
        let stderr = String::from_utf8_lossy(&verification.stderr);
        let _ = tokio::fs::remove_file(path).await;
        return Err(anyhow!("Media container verification failed: {}", stderr));
    }
    Ok(())
}

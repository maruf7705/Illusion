use anyhow::Result;
use illusion_core::{AppState, DownloadEngine, DownloadTask, TaskStatus};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_help();
        return Ok(());
    }

    let state = AppState::load_or_default();

    match args[1].as_str() {
        "add" => {
            if args.len() < 3 {
                eprintln!("Error: Missing URL. Usage: illusion-cli add <URL> [--filename <name>]");
                return Ok(());
            }
            let url = args[2].clone();
            let mut filename = String::new();
            let mut audio_url: Option<String> = None;
            let mut custom_dir: Option<PathBuf> = None;
            let mut conns = 8;

            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--filename" | "-f" => {
                        if i + 1 < args.len() {
                            filename = args[i + 1].clone();
                            i += 1;
                        }
                    }
                    "--audio-url" | "-a" => {
                        if i + 1 < args.len() {
                            audio_url = Some(args[i + 1].clone());
                            i += 1;
                        }
                    }
                    "--dir" | "-d" => {
                        if i + 1 < args.len() {
                            custom_dir = Some(PathBuf::from(&args[i + 1]));
                            i += 1;
                        }
                    }
                    "--conns" | "-c" => {
                        if i + 1 < args.len() {
                            conns = args[i + 1].parse().unwrap_or(8);
                            i += 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }

            let (target_url, target_filename, target_headers, target_ua, target_audio_url, target_size) =
                if illusion_core::is_video_page_url(&url) {
                    println!("🔍 [Illusion] Detected video page URL: {}", url);
                    println!("   Resolving media stream (360p / progressive)...");
                    match illusion_core::resolve_media_url(&url, None).await {
                        Ok(resolved) => {
                            println!("   Found stream: {}", resolved.title);
                            let resolved_filename = if filename.is_empty() || filename == "download" {
                                resolved.filename
                            } else {
                                filename
                            };
                            (
                                resolved.stream_url,
                                resolved_filename,
                                resolved.headers,
                                resolved.user_agent,
                                resolved.audio_url.or(audio_url),
                                resolved.total_size,
                            )
                        }
                        Err(e) => {
                            eprintln!("⚠️  [Illusion] Stream resolution failed: {:#}", e);
                            eprintln!("   Falling back to direct download...");
                            let fname = if filename.is_empty() {
                                url.split('/').last().unwrap_or("download").to_string()
                            } else {
                                filename
                            };
                            (url.clone(), fname, HashMap::new(), None, audio_url, None)
                        }
                    }
                } else {
                    let fname = if filename.is_empty() {
                        url.split('/').last().unwrap_or("download").to_string()
                    } else {
                        filename
                    };
                    (url.clone(), fname, HashMap::new(), None, audio_url, None)
                };

            let clean_filename = illusion_core::prober::sanitize_filename(&target_filename);
            let save_dir = custom_dir.unwrap_or_else(|| state.default_download_dir.clone());
            let save_path = save_dir.join(&clean_filename);

            let mut task = DownloadTask::new(
                target_url.clone(),
                clean_filename.clone(),
                save_path.clone(),
                target_size,
                target_headers,
                None,
                target_ua,
            );
            task.audio_url = target_audio_url;

            let task_id = task.id.clone();
            state.add_task(task).await;

            println!("⚡ [Illusion] Added task: {}", clean_filename);
            println!("   Save Path: {:?}", save_path);
            println!("   Connections: {}", conns);
            println!("   Starting download...\n");

            let engine = DownloadEngine::new(conns);
            let (cancel_tx, cancel_rx) = watch::channel(false);

            // Handle Ctrl-C
            let cancel_tx_ctrlc = cancel_tx.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    println!("\n[Illusion] Pausing download...");
                    let _ = cancel_tx_ctrlc.send(true);
                }
            });

            let task_id_clone = task_id.clone();
            let state_clone = state.clone();
            let monitor_handle = tokio::spawn(async move {
                loop {
                    sleep(Duration::from_millis(500)).await;
                    if let Some(t) = state_clone.get_task(&task_id_clone).await {
                        if t.status == TaskStatus::Completed {
                            println!("\r[100.0%] Finished {}! Total: {}                       ", t.filename, t.formatted_size());
                            break;
                        } else if t.status == TaskStatus::Error {
                            eprintln!("\r[Error] Download failed: {:?}", t.error_message);
                            break;
                        } else if t.status == TaskStatus::Paused {
                            println!("\r[Paused] Download paused at {}                      ", t.formatted_size());
                            break;
                        }

                        let percent = t.progress_percent();
                        let speed = t.formatted_speed();
                        let eta = t.formatted_eta();
                        let active_segs = t.segments.iter().filter(|s| !s.is_finished()).count();

                        print!(
                            "\r[{:>5.1}%] {} | Speed: {:>10} | ETA: {:>6} | Segments: {:>2}   ",
                            percent,
                            t.formatted_size(),
                            speed,
                            eta,
                            active_segs
                        );
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                }
            });

            let _ = engine.run_task(task_id, state, cancel_rx).await;
            let _ = monitor_handle.await;
        }
        "list" => {
            let tasks = state.tasks.read().unwrap();
            if tasks.is_empty() {
                println!("No downloads found in Illusion.");
                return Ok(());
            }

            println!("{:<36} {:<24} {:<12} {:<15} {:<12}", "ID", "FILENAME", "STATUS", "SIZE", "SPEED");
            println!("{}", "-".repeat(95));
            for t in tasks.iter() {
                println!(
                    "{:<36} {:<24} {:<12?} {:<15} {:<12}",
                    &t.id,
                    truncate_str(&t.filename, 22),
                    t.status,
                    t.formatted_size(),
                    t.formatted_speed(),
                );
            }
        }
        "resume" => {
            if args.len() < 3 {
                eprintln!("Error: Missing task ID. Usage: illusion-cli resume <task_id>");
                return Ok(());
            }
            let task_id = args[2].clone();
            let engine = DownloadEngine::new(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            println!("⚡ Resuming task: {}", task_id);
            engine.run_task(task_id, state, cancel_rx).await?;
        }
        "remove" => {
            if args.len() < 3 {
                eprintln!("Error: Missing task ID. Usage: illusion-cli remove <task_id>");
                return Ok(());
            }
            let task_id = args[2].clone();
            state.remove_task(&task_id).await;
            println!("Removed task {}.", task_id);
        }
        _ => {
            print_help();
        }
    }

    Ok(())
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

fn print_help() {
    println!("⚡ Illusion - High Performance IDM Alternative for Linux & Arch");
    println!("\nUsage:");
    println!("  illusion-cli add <URL> [--filename <name>] [--conns <count>]");
    println!("  illusion-cli list");
    println!("  illusion-cli resume <task_id>");
    println!("  illusion-cli remove <task_id>");
}

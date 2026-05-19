/*!
 * Cinnamon Wallpaper - Native Animated Wallpaper for Linux Mint Cinnamon
 * 
 * A high-performance, native application specifically designed for Linux Mint Cinnamon
 * desktop environment. Provides GPU-accelerated animated wallpapers with minimal
 * system resource usage and multi-monitor support.
 */

use anyhow::{Context, Result};
use clap::Parser;
use log::{info, warn, error};
use std::path::PathBuf;
use std::collections::HashMap;

mod desktop;
mod video;
mod window;
mod config;
mod gsettings_wallpaper;
mod gui;
mod instance;

use desktop::CinnamonDesktop;
use video::VideoPlayer;
use window::WallpaperWindow;
use config::{Config, MonitorConfig};
use gsettings_wallpaper::CinnamonWallpaperSettings;

/// Native animated wallpaper application for Linux Mint Cinnamon
#[derive(Parser)]
#[command(
    name = "cinnamon-wallpaper",
    version = "1.0.0",
    about = "Set animated video wallpapers on Linux Mint Cinnamon desktop",
    long_about = "A native, high-performance animated video wallpaper application specifically \
                  designed for Linux Mint Cinnamon desktop environment. Features GPU \
                  acceleration, multi-monitor support, and minimal system resource usage."
)]
struct Args {
    /// Path to video file to use as wallpaper
    #[arg(value_name = "VIDEO_FILE")]
    video_path: Option<PathBuf>,

    /// Monitor to display wallpaper on (0-based index)
    #[arg(short, long, value_name = "INDEX")]
    monitor: Option<usize>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Configuration file path
    #[arg(short, long, value_name = "CONFIG")]
    config: Option<PathBuf>,

    /// List available monitors
    #[arg(long)]
    list_monitors: bool,

    /// Show Cinnamon desktop information
    #[arg(long)]
    desktop_info: bool,

    /// Run in background mode (daemon)
    #[arg(short = 'D', long)]
    daemon: bool,

    /// Stop any running wallpaper
    #[arg(long)]
    stop: bool,

    /// Launch graphical user interface
    #[arg(short, long)]
    gui: bool,
    
    /// Restore the last applied wallpaper (for autostart)
    /// Deprecated: uses config file monitors section now
    #[arg(long)]
    restore_last: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    if args.debug {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    info!("🎬 Cinnamon Wallpaper v{} starting", env!("CARGO_PKG_VERSION"));

    // Check for special flags that don't require instance locking
    if args.stop {
        // TODO: Send stop signal to daemon
        info!("🛑 Stop command received. (Feature not yet implemented)");
        return Ok(()); 
    }

    // Channel for communicating between instance handler and main loop
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tx_clone = tx.clone();

    // Single instance check
    let args_vec: Vec<String> = std::env::args().skip(1).collect();
    // Use ASCII Record Separator (0x1E) as delimiter to handle spaces/quotes safely
    let args_msg = args_vec.join("\x1E");
    
    // Determine instance suffix based on mode
    let instance_suffix = if args.gui || (args.video_path.is_none() && !args.daemon && !args.restore_last) {
        Some("gui")
    } else {
        None // Default for daemon/wallpaper runner
    };

    match instance::acquire_instance_and_maybe_send(&args_msg, instance_suffix) {
        Ok(false) => {
            info!("Another instance is running, forwarded arguments and exiting.");
            return Ok(());
        }
        Ok(true) => {
            // We are primary; register a handler
            instance::register_handler(move |msg| {
                log::info!("[instance] received raw message (len={}): '{:?}'", msg.len(), msg);
                // Parse the message using ASCII Record Separator
                // Filter out empty strings that might result from trailing separators or empty input
                let args_vec: Vec<String> = msg.split('\x1E')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                    
                log::info!("[instance] split args: {:?}", args_vec);
                
                // Add dummy "program name" to front because clap expects it
                let mut full_args = vec!["cinnamon-wallpaper".to_string()];
                full_args.extend(args_vec);
                
                match Args::try_parse_from(full_args) {
                    Ok(parsed_args) => {
                        log::info!("[instance] parsed args: monitor={:?}, video={:?}", parsed_args.monitor, parsed_args.video_path);
                        if let Err(e) = tx_clone.send(parsed_args) {
                            log::error!("Failed to forward args to main loop: {}", e);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to parse received arguments: {}", e);
                    }
                }
            });
        }
        Err(e) => {
            log::warn!("Instance management failed: {}", e);
        }
    }

    // Launch GUI if requested or no video specified (and not daemon mode/restore)
    if (args.gui || (args.video_path.is_none() && !args.daemon && !args.restore_last)) && !args.list_monitors && !args.desktop_info {
        info!("🚀 Launching graphical interface...");
        // TODO: Pass monitor info to GUI?
        let exit_code = gui::launch_gui()
            .context("Failed to launch GUI")?;
        std::process::exit(exit_code.into());
    }

    // Run async runtime
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main(args, rx))
}

async fn async_main(args: Args, rx: tokio::sync::mpsc::UnboundedReceiver<Args>) -> Result<()> {
    // Load configuration
    let mut config = Config::load(args.config.as_deref())
        .context("Failed to load configuration")?;

    // Initialize desktop detection
    let desktop = CinnamonDesktop::new()
        .context("Failed to initialize Cinnamon desktop interface")?;

    // Handle list monitors
    if args.list_monitors {
        return list_monitors(&desktop).await;
    }

    if args.desktop_info {
        return show_desktop_info(&desktop).await;
    }

    // CLI Config Update Logic
    // If user provided a video path, we update the config for the target monitor(s)
    if let Some(video_path) = &args.video_path {
        if !video_path.exists() {
            return Err(anyhow::anyhow!("Video file not found: {}", video_path.display()));
        }

        let abs_path = std::fs::canonicalize(video_path)?;
        
        let target_monitors = if let Some(idx) = args.monitor {
            vec![idx] 
        } else {
            // If no monitor specified, default to 0 (primary) for CLI apply
            vec![0] 
        };

        for monitor_idx in target_monitors {
            info!("💾 Updating config for monitor {}: {}", monitor_idx, abs_path.display());
            let m_config = config.cinnamon.monitors.entry(monitor_idx.to_string())
                .or_insert_with(MonitorConfig::default);
            m_config.video_path = Some(abs_path.clone());
        }
        
        config.save_default()?;
    }

    // Run Mode Logic
    // If daemon mode OR restore_last OR we just set a video: run the wallpapers
    
    // Desktop integration
    let wallpaper_settings = CinnamonWallpaperSettings::new();
    if !config.cinnamon.preserve_native_wallpaper {
        // Attempt to clear Cinnamon wallpaper
        let _ = wallpaper_settings.disable_native_wallpaper_with_solid(config.cinnamon.use_solid_background_for_video);
    } else {
        info!("ℹ️ preserve_native_wallpaper enabled - skipping Cinnamon settings adjustment");
    }

    run_multi_monitor_wallpaper(&desktop, &mut config, rx).await
}

async fn run_multi_monitor_wallpaper(
    desktop: &CinnamonDesktop, 
    config: &mut Config,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Args>
) -> Result<()> {
    let _monitors = desktop.get_monitors()
        .context("Failed to get monitor information")?;

    let mut handles: HashMap<usize, tokio::task::JoinHandle<()>> = HashMap::new();
    
    // Initial startup: spawn for all relevant monitors
    update_wallpapers(desktop, config, &mut handles).await?;

    info!("🚀 Running wallpaper service. Waiting for updates...");

    // Event Loop
    loop {
        tokio::select! {
            // Handle new arguments (dynamic updates)
            Some(new_args) = rx.recv() => {
                info!("🔄 Received update request");
                
                // Parse args and update config
                if let Some(video_path) = new_args.video_path {
                     if video_path.exists() {
                        if let Ok(abs_path) = std::fs::canonicalize(&video_path) {
                            let target_monitors = if let Some(idx) = new_args.monitor {
                                vec![idx] 
                            } else {
                                vec![0] 
                            };

                            for monitor_idx in target_monitors {
                                info!("💾 Updating runtime config for monitor {}: {}", monitor_idx, abs_path.display());
                                let m_config = config.cinnamon.monitors.entry(monitor_idx.to_string())
                                    .or_insert_with(MonitorConfig::default);
                                m_config.video_path = Some(abs_path.clone());
                                
                                // Restart task for this monitor
                                if let Some(handle) = handles.remove(&monitor_idx) {
                                    info!("🔄 Restarting wallpaper task for monitor {}", monitor_idx);
                                    handle.abort();
                                }
                            }
                            // Save global config just in case
                            let _ = config.save_default();
                            
                            // Re-apply wallpapers
                            update_wallpapers(desktop, config, &mut handles).await?;
                        }
                     }
                }
                
                if new_args.stop {
                     info!("🛑 Stop command received");
                     // Abort all
                     for (_, h) in handles.drain() {
                         h.abort();
                     }
                     return Ok(());
                }
            }
            
            // Handle Ctrl+C or other signals if I added signal handling
            _ = tokio::signal::ctrl_c() => {
                 info!("🛑 Shutdown signal received");
                 break;
            }
        }
    }
    
    Ok(())
}

async fn update_wallpapers(
    desktop: &CinnamonDesktop, 
    config: &Config, 
    handles: &mut HashMap<usize, tokio::task::JoinHandle<()>>
) -> Result<()> {
    let monitors = desktop.get_monitors()
        .context("Failed to get monitor information")?;

    for (i, monitor_info) in monitors.iter().enumerate() {
        // If we already have a running handle for this monitor, leave it alone
        if handles.contains_key(&i) {
            continue;
        }
        
        // Check config
        let monitor_key = i.to_string();
        let video_path = if let Some(m_config) = config.cinnamon.monitors.get(&monitor_key) {
            m_config.video_path.clone()
        } else if i == 0 && config.cinnamon.monitors.is_empty() {
             // Fallback/Legacy
             None
        } else {
            None
        };
        
        info!("[DEBUG] Monitor {} config lookup. Map keys: {:?}, Found: {:?}", 
             monitor_key, 
             config.cinnamon.monitors.keys().collect::<Vec<_>>(),
             video_path
        );

        if let Some(path) = video_path {
             if path.exists() {
                info!("🖥️  Spawning wallpaper on monitor {} ({}x{}) with {}", 
                    i, monitor_info.width, monitor_info.height, path.display());
                
                let monitor_info_clone = monitor_info.clone();
                let config_clone = config.clone();
                let path_clone = path.clone();
                let i_clone = i;

                let handle = tokio::spawn(async move {
                    if let Err(e) = run_single_monitor_task(config_clone, monitor_info_clone, path_clone).await {
                        error!("❌ Wallpaper task for monitor {} failed: {}", i_clone, e);
                    }
                });
                
                handles.insert(i, handle);
            }
        }
    }
    Ok(())
}

async fn run_single_monitor_task(
    config: Config,
    monitor_info: desktop::MonitorInfo,
    video_path: PathBuf
) -> Result<()> {
    // We need a separate desktop connection for each thread/window? 
    // `CinnamonDesktop` creates an X connection. `WallpaperWindow` creates another.
    // It should be fine.
    
    // Initialize desktop helper (mainly for querying if needed, but window.rs creates its own connection)
    let desktop = CinnamonDesktop::new()?; 
    
    // Initialize video player
    let mut video_player = VideoPlayer::new(&config, monitor_info.width, monitor_info.height)?;
    
    // Load video
    video_player.load_video(&video_path)?;

    // Create window on specific monitor
    let mut window = WallpaperWindow::new(&desktop, &config, &monitor_info)?;
    
    // Render loop
    // Re-use existing loop logic but adapted
    run_wallpaper_loop(&mut window, &mut video_player).await
}

// Extracted from original main.rs, adapted slightly
async fn run_wallpaper_loop(
    window: &mut WallpaperWindow, 
    video_player: &mut VideoPlayer
) -> Result<()> {
    let mut frame_count = 0;
    
    // Main rendering loop
    loop {
        // Check whether a stop was requested by GUI/IPC
        // This check needs to be global or passed down. For now, individual tasks run indefinitely.
        // if crate::gui::should_stop_requested() {
        //     info!("Stop requested via IPC/GUI flag; exiting wallpaper loop");
        //     break Ok(());
        // }
        // Get next frame from video
        if let Some(frame) = video_player.next_frame().await? {
            // Render frame to window
            window.render_frame(&frame).await?;
            frame_count += 1;
            
            // Periodic icon monitoring (every ~2-4 seconds, assuming 30-60fps)
            if frame_count % 120 == 0 { 
                if let Err(e) = window.monitor_and_fix_icons() {
                    warn!("⚠️  Icon monitoring failed: {e}");
                }
            }
        } else {
            // Video ended, restart if looping is enabled
            video_player.restart().await?;
            frame_count = 0; // Reset frame count on restart
        }

    // When no frame is received next_frame() will return None and we'll loop.
    // Avoid unconditional sleeps here so rendering follows appsink pacing.
    }
}

async fn list_monitors(desktop: &CinnamonDesktop) -> Result<()> {
    let monitors = desktop.get_monitors()
        .context("Failed to get monitor information")?;

    println!("📺 Available Monitors:");
    for (i, monitor) in monitors.iter().enumerate() {
        println!("  {}: {}x{}+{}+{} {}", 
                 i, 
                 monitor.width, 
                 monitor.height, 
                 monitor.x, 
                 monitor.y,
                 if monitor.primary { "(primary)" } else { "" });
    }

    Ok(())
}

async fn show_desktop_info(desktop: &CinnamonDesktop) -> Result<()> {
    println!("🖥️  Desktop Environment Information:");
    println!("  Cinnamon Available: {}", desktop.is_cinnamon_available());
    println!("  Desktop Session: {:?}", std::env::var("DESKTOP_SESSION"));
    println!("  XDG Current Desktop: {:?}", std::env::var("XDG_CURRENT_DESKTOP"));
    println!("  Window Manager: {}", desktop.get_window_manager_name()?);
    
    let monitors = desktop.get_monitors()?;
    println!("  Monitor Count: {}", monitors.len());
    
    Ok(())
}

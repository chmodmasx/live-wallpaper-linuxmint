/*!
 * Configuration management for Cinnamon Wallpaper
 */

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Video playback settings
    pub video: VideoConfig,
    
    /// Window management settings
    pub window: WindowConfig,
    
    /// Performance optimization settings
    pub performance: PerformanceConfig,
    
    /// Cinnamon-specific settings
    pub cinnamon: CinnamonConfig,
}

/// Video playback configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    /// Enable hardware acceleration
    pub hardware_acceleration: bool,
    
    /// Loop video playback
    pub loop_playback: bool,
    
    /// Video scaling quality (1-10, 10 = highest)
    pub scaling_quality: u8,
    
    /// Target FPS (0 = use video's native FPS)
    pub target_fps: u32,
    
    /// Audio enabled (usually false for wallpapers)
    pub audio_enabled: bool,
    /// Maximum number of frames to buffer in appsink (reduces latency vs drops)
    /// Set to a small number (1-5) for low latency, or higher to smooth playback
    pub max_buffer_frames: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            hardware_acceleration: true,
            loop_playback: true,
            scaling_quality: 8,
            target_fps: 0,
            audio_enabled: false,
            max_buffer_frames: 3,
        }
    }
}

/// Window management configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    /// Keep wallpaper below all windows
    pub below_all_windows: bool,
    
    /// Stick to all desktops/workspaces
    pub stick_to_all_desktops: bool,
    
    /// Override window manager decorations
    pub override_decorations: bool,
    
    /// Window opacity (0.0-1.0)
    pub opacity: f32,
    /// Enable periodic force-refresh of Nemo desktop windows (may be noisy)
    pub enable_nemo_refresh: bool,
    /// Minimum interval between Nemo refreshes in milliseconds
    pub nemo_refresh_interval_ms: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            below_all_windows: true,
            stick_to_all_desktops: true,
            override_decorations: true,
            opacity: 1.0,
            enable_nemo_refresh: true,
            nemo_refresh_interval_ms: 2000,
        }
    }
}

/// Performance optimization configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    /// Preferred GPU vendor (nvidia, amd, intel, auto)
    pub gpu_vendor: String,
    
    /// Enable CPU fallback if GPU fails
    pub cpu_fallback: bool,
    
    /// Maximum memory usage for video cache (MB)
    pub max_cache_memory_mb: u32,
    
    /// Enable frame skipping on slow systems
    pub frame_skipping: bool,
    
    /// Reduce quality when system is under load
    pub adaptive_quality: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            gpu_vendor: "auto".to_string(),
            cpu_fallback: true,
            max_cache_memory_mb: 512,
            frame_skipping: false,
            adaptive_quality: true,
        }
    }
}

/// Monitor specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// Path to the video file for this monitor
    pub video_path: Option<PathBuf>,
    
    /// Scaling mode (cover, contain, etc.) - planned for future
    pub scaling_mode: String,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            video_path: None,
            scaling_mode: "cover".to_string(),
        }
    }
}

/// Cinnamon-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CinnamonConfig {
    /// Integrate with Cinnamon's background system
    pub integrate_with_background_system: bool,
    
    /// Respect Cinnamon's desktop icons
    pub respect_desktop_icons: bool,
    
    /// Use Cinnamon's compositor optimizations
    pub use_compositor_optimizations: bool,
    
    /// Automatically adapt to theme changes
    pub adapt_to_theme_changes: bool,
    
    /// If true, do not modify Cinnamon's native wallpaper keys (safe mode)
    pub preserve_native_wallpaper: bool,

    /// If true, when applying a video set Cinnamon to a solid background color
    /// (picture-uri = '' and primary-color used). Default: true
    pub use_solid_background_for_video: bool,
    
    /// Enable autostart of wallpaper on system startup
    pub autostart_enabled: bool,
    
    /// Path to the last applied video wallpaper (for restoration on startup)
    pub last_video_path: Option<PathBuf>,
    
    /// Monitor index for the last applied wallpaper
    pub last_monitor_index: Option<usize>,
    
    /// Per-monitor configuration (monitor index as string -> settings)
    pub monitors: HashMap<String, MonitorConfig>,
}

impl Default for CinnamonConfig {
    fn default() -> Self {
        Self {
            integrate_with_background_system: true,
            respect_desktop_icons: true,
            use_compositor_optimizations: true,
            adapt_to_theme_changes: false,
            preserve_native_wallpaper: false,
            use_solid_background_for_video: true,
            autostart_enabled: true,
            last_video_path: None,
            last_monitor_index: None,
            monitors: HashMap::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            video: VideoConfig::default(),
            window: WindowConfig::default(),
            performance: PerformanceConfig::default(),
            cinnamon: CinnamonConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from file or create default
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        let config_file = if let Some(path) = config_path {
            path.to_path_buf()
        } else {
            Self::default_config_path()?
        };

        if config_file.exists() {
            let content = std::fs::read_to_string(&config_file)
                .with_context(|| format!("Failed to read config file: {}", config_file.display()))?;
            
            let config: Config = toml::from_str(&content)
                .with_context(|| format!("Failed to parse config file: {}", config_file.display()))?;
            
            log::info!("📄 Loaded configuration from: {}", config_file.display());
            Ok(config)
        } else {
            log::info!("📄 Using default configuration");
            let default_config = Self::default();
            
            // Create config directory if it doesn't exist
            if let Some(parent) = config_file.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
            }
            
            // Save default config for future editing
            default_config.save(&config_file)?;
            log::info!("💾 Saved default configuration to: {}", config_file.display());
            
            Ok(default_config)
        }
    }

    /// Save configuration to file
    pub fn save(&self, config_path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .context("Failed to serialize configuration")?;
        
        std::fs::write(config_path, content)
            .with_context(|| format!("Failed to write config file: {}", config_path.display()))?;
        
        Ok(())
    }
    
    /// Save configuration to default path
    pub fn save_default(&self) -> Result<()> {
        let config_path = Self::default_config_path()?;
        
        // Create config directory if it doesn't exist
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
        }
        
        self.save(&config_path)
    }

    /// Get default configuration file path
    pub fn default_config_path() -> Result<PathBuf> {
        let config_dir = if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
            PathBuf::from(xdg_config)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".config")
        } else {
            return Err(anyhow::anyhow!("Could not determine config directory"));
        };

        Ok(config_dir.join("cinnamon-wallpaper").join("config.toml"))
    }
}

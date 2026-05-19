/*!
 * GSettings Wallpaper Management
 * 
 * **NOTE:** This module is intentionally a NO-OP.
 * 
 * Handles Cinnamon's wallpaper settings to allow our application
 * to take full control of the desktop background while Nemo
 * handles only desktop icons.
 * 
 * All methods are disabled to prevent the application from modifying
 * the user's Cinnamon wallpaper settings (gsettings keys), ensuring
 * non-destructive behavior. If Cinnamon integration is needed in the
 * future, these stubs should be implemented using the `gsettings`
 * command-line tool.
 */

use anyhow::Result;
use log::info;

pub struct CinnamonWallpaperSettings {
    _placeholder: (), // We'll use gsettings command line tool
}

impl CinnamonWallpaperSettings {
    /// Create a new wallpaper settings manager
    pub fn new() -> Self {
        Self {
            _placeholder: (),
        }
    }

    /// Save current wallpaper settings before applying video wallpaper
    pub fn save_current_wallpaper(&self) -> Result<()> {
    // NO-OP: Disabled saving the user's wallpaper to avoid touching user settings.
    info!("ℹ️ save_current_wallpaper skipped (no se modifican ni leen claves de GSettings)");
    Ok(())
    }

    /// Disable Cinnamon's native wallpaper with optional solid background behavior
    pub fn disable_native_wallpaper_with_solid(&self, use_solid: bool) -> Result<()> {
        // NO-OP: Do not change Cinnamon wallpaper state under any circumstance
        info!("ℹ️ disable_native_wallpaper_with_solid skipped (use_solid={}): no changes made", use_solid);
        Ok(())
    }
}

/*!
 * Cinnamon desktop environment detection and integration
 */

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as RandrConnectionExt};
use x11rb::protocol::xproto::*;

/// Monitor information
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Cinnamon desktop environment interface
pub struct CinnamonDesktop {
    conn: x11rb::rust_connection::RustConnection,
    root: Window,
}

impl CinnamonDesktop {
    /// Create a new Cinnamon desktop interface
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)
            .context("Failed to connect to X11 server")?;

        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let root = screen.root;

        // Initialize RandR extension for monitor detection
        let randr_version = conn.randr_query_version(1, 4)?;
        let reply = randr_version.reply()
            .context("Failed to query RandR version")?;

        log::debug!("RandR version: {}.{}", reply.major_version, reply.minor_version);

        Ok(Self {
            conn,
            root,
        })
    }

    /// Check if Cinnamon desktop environment is available
    pub fn is_cinnamon_available(&self) -> bool {
        // Check environment variables
        if let Ok(desktop_session) = std::env::var("DESKTOP_SESSION") {
            if desktop_session.to_lowercase().contains("cinnamon") {
                return true;
            }
        }

        if let Ok(xdg_current_desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
            if xdg_current_desktop.to_lowercase().contains("cinnamon") {
                return true;
            }
        }

        // Check for Cinnamon-specific window manager
        if let Ok(wm_name) = self.get_window_manager_name() {
            if wm_name.to_lowercase().contains("mutter") || 
               wm_name.to_lowercase().contains("cinnamon") {
                return true;
            }
        }

        // Check for Cinnamon processes
        self.is_cinnamon_process_running()
    }

    /// Check if Cinnamon processes are running
    fn is_cinnamon_process_running(&self) -> bool {
        let cinnamon_processes = ["cinnamon", "cinnamon-session", "muffin"];
        
        for process in &cinnamon_processes {
            if let Ok(output) = std::process::Command::new("pgrep")
                .arg("-f")
                .arg(process)
                .output()
            {
                if !output.stdout.is_empty() {
                    log::debug!("Found Cinnamon process: {process}");
                    return true;
                }
            }
        }

        false
    }

    /// Get window manager name
    pub fn get_window_manager_name(&self) -> Result<String> {
        // Try to get window manager name from _NET_WM_NAME
        let net_wm_name = self.conn.intern_atom(false, b"_NET_WM_NAME")?
            .reply()
            .context("Failed to intern _NET_WM_NAME atom")?
            .atom;

        let utf8_string = self.conn.intern_atom(false, b"UTF8_STRING")?
            .reply()
            .context("Failed to intern UTF8_STRING atom")?
            .atom;

        // Get the window manager window
        let net_supporting_wm_check = self.conn.intern_atom(false, b"_NET_SUPPORTING_WM_CHECK")?
            .reply()
            .context("Failed to intern _NET_SUPPORTING_WM_CHECK atom")?
            .atom;

        let wm_window_reply = self.conn.get_property(
            false,
            self.root,
            net_supporting_wm_check,
            AtomEnum::WINDOW,
            0,
            1,
        )?.reply()?;

        if let Some(wm_window_data) = wm_window_reply.value.get(0..4) {
            let wm_window = u32::from_le_bytes([
                wm_window_data[0],
                wm_window_data[1], 
                wm_window_data[2],
                wm_window_data[3]
            ]);

            // Get WM name from the WM window
            let name_reply = self.conn.get_property(
                false,
                wm_window,
                net_wm_name,
                utf8_string,
                0,
                1024,
            )?.reply()?;

            if !name_reply.value.is_empty() {
                return Ok(String::from_utf8_lossy(&name_reply.value).to_string());
            }
        }

        Ok("Unknown".to_string())
    }

    /// Get information about all monitors
    pub fn get_monitors(&self) -> Result<Vec<MonitorInfo>> {
        let monitors = self.conn.randr_get_monitors(self.root, true)?
            .reply()
            .context("Failed to get monitor information")?;

        let mut monitor_infos = Vec::new();

        for monitor in monitors.monitors.iter() {
            monitor_infos.push(MonitorInfo {
                x: monitor.x as i32,
                y: monitor.y as i32,
                width: monitor.width as u32,
                height: monitor.height as u32,
                primary: monitor.primary,
            });
        }

        // If no monitors found via RandR, try legacy method
        if monitor_infos.is_empty() {
            monitor_infos = self.get_monitors_legacy()?;
        }

        // Sort monitors by position (left to right, top to bottom)
        monitor_infos.sort_by(|a, b| {
            a.x.cmp(&b.x).then_with(|| a.y.cmp(&b.y))
        });

        Ok(monitor_infos)
    }

    /// Legacy method to get monitor information
    fn get_monitors_legacy(&self) -> Result<Vec<MonitorInfo>> {
        let screen_resources = self.conn.randr_get_screen_resources(self.root)?
            .reply()
            .context("Failed to get screen resources")?;

        let mut monitors = Vec::new();

        for (i, output) in screen_resources.outputs.iter().enumerate() {
            let output_info = self.conn.randr_get_output_info(*output, 0)?
                .reply()
                .context("Failed to get output info")?;

            if output_info.connection == randr::Connection::CONNECTED && output_info.crtc != 0 {
                let crtc_info = self.conn.randr_get_crtc_info(output_info.crtc, 0)?
                    .reply()
                    .context("Failed to get CRTC info")?;

                monitors.push(MonitorInfo {
                    x: crtc_info.x as i32,
                    y: crtc_info.y as i32,
                    width: crtc_info.width as u32,
                    height: crtc_info.height as u32,
                    primary: i == 0, // Assume first connected output is primary
                });
            }
        }

        Ok(monitors)
    }
}

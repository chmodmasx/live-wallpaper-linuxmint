use anyhow::Context;
use std::cell::Cell;
use std::collections::HashMap;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConfigureWindowAux, ConnectionExt as XProtoConnectionExt,
    CreateGCAux, CreateWindowAux, EventMask, ImageFormat, PropMode, StackMode, Window, WindowClass,
    ExposeEvent, MapNotifyEvent, ClipOrdering, EXPOSE_EVENT, MAP_NOTIFY_EVENT
};
use x11rb::protocol::shape;
use x11rb::wrapper::ConnectionExt;
use x11rb::COPY_DEPTH_FROM_PARENT;

use crate::{config::Config, desktop::{CinnamonDesktop, MonitorInfo}, video::VideoFrame};

pub struct WallpaperWindow {
    window: Window,
    connection: x11rb::rust_connection::RustConnection,
    atoms: HashMap<String, Atom>,
    _config: Config,
    // Reusable graphics context to avoid allocating per-frame
    gc: Option<u32>,
    // Monitor dimensions for Nemo refresh events
    screen_width: u16,
    screen_height: u16,
    // Last time we forced a Nemo refresh (to avoid flooding)
    last_nemo_refresh: Cell<std::time::Instant>,
}

impl WallpaperWindow {
    /// Get required X11 atoms for window management
    fn get_required_atoms(conn: &impl RequestConnection) -> anyhow::Result<HashMap<String, Atom>> {
        let mut atoms = HashMap::new();
        
        let atom_names = [
            "_NET_WM_WINDOW_TYPE",
            "_NET_WM_WINDOW_TYPE_DESKTOP", 
            "_NET_WM_WINDOW_TYPE_NORMAL",
            "_NET_WM_STATE",
            "_NET_WM_STATE_BELOW",
            "_NET_WM_STATE_SKIP_TASKBAR",
            "_NET_WM_STATE_SKIP_PAGER", 
            "_NET_WM_STATE_STICKY",
            "_MOTIF_WM_HINTS", 
            "_NET_WM_FULLSCREEN_MONITORS",
        ];
        
        for &name in &atom_names {
            let reply = conn.intern_atom(false, name.as_bytes())?.reply()?;
            atoms.insert(name.to_string(), reply.atom);
            log::debug!("Got atom {}: {}", name, reply.atom);
        }
        
        Ok(atoms)
    }

    pub fn new(desktop: &CinnamonDesktop, config: &Config, monitor: &MonitorInfo) -> anyhow::Result<Self> {
        let desktop_ref = desktop;
        
        // Find monitor index based on geometry
        let monitors = desktop_ref.get_monitors()?;
        let monitor_index = monitors.iter().position(|m| 
            m.x == monitor.x && m.y == monitor.y && m.width == monitor.width && m.height == monitor.height
        ).unwrap_or(0); // Default to 0 if not found (fallback)

        // Create a new connection to X11
        let (conn, screen_num) = x11rb::connect(None)?;
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let root_window = screen.root;

        // Create window
        let window = conn.generate_id()?;
        
        // Use override_redirect = 0 (FALSE) for MANAGED window to fix Z-order
        let win_aux = CreateWindowAux::new()
            .background_pixel(0x000000) // Black background
            .border_pixel(0)
            .override_redirect(0) // Managed window
            .event_mask(EventMask::STRUCTURE_NOTIFY); // Minimal events only

        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            screen.root,
            monitor.x as i16, monitor.y as i16,
            monitor.width as u16,
            monitor.height as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &win_aux,
        )?;

        log::debug!("✅ Window created with ID: 0x{window:x}");

        // Get required atoms
        let atoms = Self::get_required_atoms(&conn)?;

        let instance = Self {
            window,
            connection: conn,
            atoms,
            _config: config.clone(),
            gc: None,
            screen_width: monitor.width as u16,
            screen_height: monitor.height as u16,
            last_nemo_refresh: Cell::new(std::time::Instant::now() - std::time::Duration::from_secs(10)),
        };

        // Configure as wallpaper
        instance.configure_as_wallpaper(root_window)?;
        
        // Set Fullscreen Monitors constraint
        instance.set_fullscreen_monitors(monitor_index)?;

        // Map the window first
        instance.connection.map_window(window)?;
        instance.connection.flush()?;

        // Lower to bottom - critical for managed windows to sit behind icons if they are on the desktop layer too
        instance.connection.configure_window(
            window,
            &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
        )?;
        instance.connection.flush()?;
        log::debug!("✅ Lowered managed window to bottom of stack");

        log::info!("🎬 Wallpaper window created successfully for Monitor {}", monitor_index);
        Ok(instance)
    }

    /// Configure the window to be the true desktop wallpaper
    fn configure_as_wallpaper(&self, _root_window: Window) -> anyhow::Result<()> {
        // Set window type to DESKTOP
        if let Some(&wm_window_type_atom) = self.atoms.get("_NET_WM_WINDOW_TYPE") {
             if let Some(&desktop_atom) = self.atoms.get("_NET_WM_WINDOW_TYPE_DESKTOP") {
                self.connection.change_property32(
                    PropMode::REPLACE,
                    self.window,
                    wm_window_type_atom,
                    AtomEnum::ATOM,
                    &[desktop_atom],
                )?;
                log::debug!("✅ Set window type to DESKTOP");
            }
        }

        // Set state hints (Below, Sticky, Skip Taskbar/Pager)
        if let Some(&wm_state_atom) = self.atoms.get("_NET_WM_STATE") {
            let mut states = Vec::new();
            if let Some(&below) = self.atoms.get("_NET_WM_STATE_BELOW") { states.push(below); }
            if let Some(&skip_taskbar) = self.atoms.get("_NET_WM_STATE_SKIP_TASKBAR") { states.push(skip_taskbar); }
            if let Some(&skip_pager) = self.atoms.get("_NET_WM_STATE_SKIP_PAGER") { states.push(skip_pager); }
            if let Some(&sticky) = self.atoms.get("_NET_WM_STATE_STICKY") { states.push(sticky); }

            if !states.is_empty() {
                self.connection.change_property32(
                    PropMode::REPLACE,
                    self.window,
                    wm_state_atom,
                    AtomEnum::ATOM,
                    &states,
                )?;
                log::debug!("✅ Set window state hints (Below, Sticky, etc.)");
            }
        }

        // Remove decorations (Motif hints)
        self.set_motif_hints()?;

        // Set WM_NORMAL_HINTS to force placement
        // CRITICAL for proper placement of managed windows on multi-monitor setups
        self.set_wm_hints()?;

        // Set class
        let wm_class = "cinnamon-wallpaper-desktop\0CinnamonWallpaperDesktop\0";
        self.connection.change_property8(
            PropMode::REPLACE,
            self.window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            wm_class.as_bytes(),
        )?;
        
        // Configure click-through so desktop icons work
        self.configure_click_through()?;

        Ok(())
    }

    /// Set _NET_WM_FULLSCREEN_MONITORS to constrain the desktop window to a specific monitor
    fn set_fullscreen_monitors(&self, monitor_index: usize) -> anyhow::Result<()> {
        if let Some(&fs_monitors_atom) = self.atoms.get("_NET_WM_FULLSCREEN_MONITORS") {
            // format: top, bottom, left, right (all same index for single monitor)
            let idx = monitor_index as u32;
            let monitors: [u32; 4] = [idx, idx, idx, idx];
            
            self.connection.change_property32(
                PropMode::REPLACE,
                self.window,
                fs_monitors_atom,
                AtomEnum::CARDINAL,
                &monitors,
            )?;
            log::debug!("✅ Set _NET_WM_FULLSCREEN_MONITORS to {}", idx);
        }
        Ok(())
    }

    /// Set WM_NORMAL_HINTS to suggest/force position
    /// Set WM_NORMAL_HINTS to suggest/force position
    fn set_wm_hints(&self) -> anyhow::Result<()> {
        let geom = self.connection.get_geometry(self.window)?.reply()?;
        
        // Construct WM_SIZE_HINTS manually
        // Structure based on ICCCM section 4.1.2.3
        // 0: flags
        // 1: x (obsolete but slot exists)
        // 2: y (obsolete)
        // 3: width (obsolete)
        // 4: height (obsolete)
        // 5: min_width
        // 6: min_height
        // 7: max_width
        // 8: max_height
        // ...
        
        // Flags: USPosition(1) | USSize(2) | PMinSize(16) | PMaxSize(32)
        // We lock the size to the monitor geometry to prevent resizing.
        let flags: u32 = 1 | 2 | 16 | 32; 
        
        let mut hints: Vec<u32> = vec![0; 18];
        hints[0] = flags;
        hints[1] = geom.x as u32; 
        hints[2] = geom.y as u32;
        hints[3] = geom.width as u32;
        hints[4] = geom.height as u32;
        
        // Min Size (same as geometry)
        hints[5] = geom.width as u32;
        hints[6] = geom.height as u32;
        
        // Max Size (same as geometry)
        hints[7] = geom.width as u32;
        hints[8] = geom.height as u32;
        
        // Rest are 0 (increments, aspects, base size, gravity)
        
        self.connection.change_property32(
            PropMode::REPLACE,
            self.window,
            AtomEnum::WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS, // Type is WM_SIZE_HINTS
            &hints,
        )?;
        
        log::debug!("✅ Set WM_NORMAL_HINTS: Fixed Size {}x{} at {},{}", 
             geom.width, geom.height, geom.x, geom.y);

        Ok(())
    }

    /// Set Motif WM hints to disable decorations
    fn set_motif_hints(&self) -> anyhow::Result<()> {
        if let Some(&motif_atom) = self.atoms.get("_MOTIF_WM_HINTS") {
            // Struct:
            // flags: u32 (2 = MWM_HINTS_DECORATIONS)
            // functions: u32
            // decorations: u32 (0 = No decorations)
            // input_mode: i32
            // status: u32
            let hints: [u32; 5] = [2, 0, 0, 0, 0]; 
            
            self.connection.change_property32(
                PropMode::REPLACE,
                self.window,
                motif_atom,
                AtomEnum::ATOM, // Type? Usually it's _MOTIF_WM_HINTS but we can use explicit atom if needed, though AtomEnum::ATOM is often used for generic data types or CARDINAL
                &hints,
            )?;
            log::debug!("✅ Set Motif hints to disable window decorations");
        }
        Ok(())
    }

    /// Configure window to allow clicks to pass through to Nemo desktop
    fn configure_click_through(&self) -> anyhow::Result<()> {
        // Check if Shape extension is available
        let shape_version = self.connection.extension_information(shape::X11_EXTENSION_NAME)?;
        
        if shape_version.is_some() {
            // For DESKTOP windows, we want to allow clicks to pass through
            // so that desktop icons and right-click menus work properly
            shape::rectangles(
                &self.connection,
                shape::SO::SET,      // Operation: SET (replace existing shape)
                shape::SK::INPUT,    // Shape kind: INPUT (input events)
                ClipOrdering::UNSORTED,
                self.window,
                0, 0,               // x_offset, y_offset
                &[],                // Empty rectangle list = no input region
            )?;
            
            log::debug!("✅ Set empty input shape - clicks will pass through for desktop interaction");
        } else {
            log::debug!("⚠️  Shape extension not available - DESKTOP window should still work correctly");
        }
        
        Ok(())
    }

    // Unused method removed to fix warning
    // pub fn setup_persistent_icons...

    /// Monitor icon state and fix if they disappear
    pub fn monitor_and_fix_icons(&self) -> anyhow::Result<()> {
        log::debug!("🔍 Checking desktop icon state...");
        
        // Check if icons are currently visible by checking gsettings
        let output = std::process::Command::new("gsettings")
            .args(["get", "org.nemo.desktop", "show-desktop-icons"])
            .output()
            .context("Failed to check icon state")?;
            
        let current_state = String::from_utf8_lossy(&output.stdout);
        let icons_enabled = current_state.trim() == "true";
        
        log::debug!("📊 Desktop icons state: {} (raw: '{}')", 
                   if icons_enabled { "ENABLED" } else { "DISABLED" }, 
                   current_state.trim());
        
        // If icons are disabled, re-enable them immediately
        if !icons_enabled {
            log::warn!("🔴 Desktop icons were disabled - re-enabling...");
            
            let _output = std::process::Command::new("gsettings")
                .args(["set", "org.nemo.desktop", "show-desktop-icons", "true"])
                .output();
                
            log::info!("✅ Desktop icons gsetting restored");
        }
        
        // Only force a Nemo refresh when we had to re-enable icons (this avoids frequent refreshes).
        if !icons_enabled {
            if self._config.window.enable_nemo_refresh {
                log::debug!("🔄 Forcing Nemo desktop refresh because icons were re-enabled...");
                if let Err(e) = self.force_nemo_desktop_refresh() {
                    log::debug!("⚠️  Nemo refresh failed: {e}");
                } else {
                    log::debug!("✅ Nemo desktop refresh completed");
                }
            } else {
                log::debug!("ℹ️  Nemo refresh disabled by configuration");
            }
        } else {
            // Icons are enabled and OK — do not refresh to avoid unnecessary load
            log::debug!("ℹ️  Nemo icons enabled, skipping refresh");
        }
        
        Ok(())
    }

    /// Force Nemo desktop to refresh by sending expose events to its window
    pub fn force_nemo_desktop_refresh(&self) -> anyhow::Result<()> {
        // Throttle: check last refresh time
        let interval = std::time::Duration::from_millis(self._config.window.nemo_refresh_interval_ms as u64);
        let now = std::time::Instant::now();
        if now.duration_since(self.last_nemo_refresh.get()) < interval {
            log::debug!("⏱️  Skipping Nemo refresh; last refresh was {:?} ago", now.duration_since(self.last_nemo_refresh.get()));
            return Ok(());
        }

        log::info!("🔄 Attempting to force Nemo desktop refresh...");

        let root_window = self.connection.setup().roots[0].root;

        // Query all child windows to find Nemo desktop windows
        if let Ok(reply) = self.connection.query_tree(root_window)?.reply() {
            for &child in reply.children.iter() {
                // Get window properties to check if it's a Nemo desktop window
                if let Ok(class_reply) = self.connection.get_property(
                    false,
                    child,
                    AtomEnum::WM_CLASS,
                    AtomEnum::STRING,
                    0,
                    1024,
                )?.reply() {
                    if let Ok(class_str) = std::str::from_utf8(&class_reply.value) {
                        if class_str.contains("nemo-desktop") {
                            log::debug!("Found Nemo desktop window: 0x{child:x}");

                            // Send Expose event to force repaint
                            self.connection.send_event(
                                false,
                                child,
                                EventMask::EXPOSURE,
                                ExposeEvent {
                                    response_type: EXPOSE_EVENT,
                                    sequence: 0,
                                    window: child,
                                    x: 0,
                                    y: 0,
                                    width: self.screen_width,
                                    height: self.screen_height,
                                    count: 0,
                                },
                            )?;

                            // Also send a MapNotify to the window
                            self.connection.send_event(
                                false,
                                child,
                                EventMask::STRUCTURE_NOTIFY,
                                MapNotifyEvent {
                                    response_type: MAP_NOTIFY_EVENT,
                                    sequence: 0,
                                    event: child,
                                    window: child,
                                    override_redirect: false,
                                },
                            )?;

                            log::debug!("✅ Sent refresh events to Nemo window 0x{child:x}");
                        }
                    }
                }
            }
        }

        self.connection.flush()?;
        log::info!("✅ Nemo desktop refresh events sent");

        // update last refresh timestamp using Cell for safe interior mutability
        self.last_nemo_refresh.set(now);

        Ok(())
    }

    /// Render a video frame to the window
    pub async fn render_frame(&mut self, frame: &VideoFrame) -> anyhow::Result<()> {
        let screen = &self.connection.setup().roots[0];
        
        // Create XImage from video frame data
        let image_data = match frame.format {
            crate::video::PixelFormat::Bgra32 => {
                // BGRA is already in the right format for X11
                frame.data.clone()
            },
            crate::video::PixelFormat::Rgba32 => {
                // Convert RGBA to BGRA by swapping R and B
                let mut bgra_data = Vec::with_capacity(frame.data.len());
                for chunk in frame.data.chunks(4) {
                    if chunk.len() == 4 {
                        bgra_data.push(chunk[2]); // B
                        bgra_data.push(chunk[1]); // G
                        bgra_data.push(chunk[0]); // R
                        bgra_data.push(chunk[3]); // A
                    }
                }
                bgra_data
            },
            _ => {
                log::warn!("Unsupported pixel format for rendering: {:?}", frame.format);
                return Ok(());
            }
        };

        // Create graphics context if not already present
        let gc = if let Some(existing_gc) = self.gc {
            existing_gc
        } else {
            let new_gc = self.connection.generate_id()?;
            self.connection.create_gc(
                new_gc,
                self.window,
                &CreateGCAux::new()
                    .foreground(screen.white_pixel)
                    .background(screen.black_pixel),
            )?;
            // store for reuse
            self.gc = Some(new_gc);
            new_gc
        };

    // Log render timing for diagnostics
    let render_start = std::time::Instant::now();

    // Create and put the image
        // X11 expects data in specific format
        let depth = 24; // RGB depth
        
        // Use put_image to render the frame
        self.connection.put_image(
            ImageFormat::Z_PIXMAP,
            self.window,
            gc,
            frame.width as u16,
            frame.height as u16,
            0, // dst_x
            0, // dst_y
            0, // left_pad
            depth,
            &image_data,
        )?;

    // Flush once per frame (kept) but avoid freeing the GC to reduce overhead
    self.connection.flush()?;

    let render_dur = render_start.elapsed();
    log::debug!("🎨 Frame rendered: {}x{} pixels, format: {:?}, render_ms={}", 
           frame.width, frame.height, frame.format, render_dur.as_millis());
        Ok(())
    }
}

impl Drop for WallpaperWindow {
    fn drop(&mut self) {
        let _ = self.connection.destroy_window(self.window);
        let _ = self.connection.flush();
        log::debug!("🗑️ Wallpaper window destroyed");
    }
}

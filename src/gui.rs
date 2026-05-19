use gtk4::prelude::*;
use gtk4::{glib, Application, ApplicationWindow, Box, Button, Label, Orientation, DropTarget, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION, ScrolledWindow, Grid, Frame, Image, FileFilter, Window, ProgressBar, Picture, Overlay, EventControllerMotion, EventControllerFocus};
use gdk4::{FileList, Display, DragAction};
use gstreamer::prelude::*;
use gstreamer::{Pipeline, Element, State};
use std::path::PathBuf;
use anyhow::Result;
use walkdir::WalkDir;
use dirs;
use std::fs;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::collections::HashSet;
use std::{time::Duration};
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use notify::{Watcher, RecursiveMode, Event, EventKind};
use std::thread;
use std::rc::Rc;
use std::cell::RefCell;

const APP_ID: &str = "com.cinnamonwallpaper.CinnamonWallpaper";

// Ancho objetivo de las tarjetas en modo galería. Ajusta este valor para cambiar
// el punto en el que la cuadrícula pasa de 1→2→3 columnas (breakpoints).
const CARD_WIDTH: i32 = 360;

use crate::desktop::CinnamonDesktop;

// Función para obtener el directorio de la biblioteca de videos
static CONVERTING_FILES: std::sync::LazyLock<Arc<Mutex<HashSet<String>>>> = 
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashSet::new())));

// Variable global para el modo de vista (false = galería, true = lista)
static VIEW_MODE_LIST: std::sync::LazyLock<Arc<Mutex<bool>>> = 
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(false)));

// Variables globales para control del watcher/actualizaciones
static MONITORING_PAUSED: std::sync::LazyLock<Arc<Mutex<bool>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(false)));

static LAST_RESUME_TIME: std::sync::LazyLock<Arc<Mutex<Option<std::time::Instant>>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

static PENDING_UPDATE: std::sync::LazyLock<Arc<Mutex<bool>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(false)));

// Registrar global de SourceId de previews activas para poder detenerlas todas cuando
// se reconstruya la galería o se cambie el modo de vista.
// removed global preview registry (caused ownership issues with SourceId)

// Estructura para rastrear proceso activo de wallpaper
#[derive(Default)]
struct ActiveWallpaper {
    process_id: Option<u32>,
    video_path: Option<PathBuf>,
    is_active: bool,
}

impl ActiveWallpaper {
    fn set_active(&mut self, path: PathBuf, pid: u32) {
        self.process_id = Some(pid);
        self.video_path = Some(path.clone());
        self.is_active = true;
        
        // Persist PID to file
        if let Some(parent) = get_pid_file_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::File::create(get_pid_file_path()) {
            let _ = writeln!(f, "{}", pid);
        }
    }

    fn clear(&mut self) {
        self.process_id = None;
        self.video_path = None;
        self.is_active = false;
        
        // Remove persistence file
        if get_pid_file_path().exists() {
            let _ = std::fs::remove_file(get_pid_file_path());
        }
    }
    
    fn restore(&mut self) {
        let path = get_pid_file_path();
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(pid) = content.trim().parse::<u32>() {
                    // Verify if process is running (signal 0)
                    let check = unsafe { libc::kill(pid as i32, 0) };
                    if check == 0 {
                         self.process_id = Some(pid);
                         self.is_active = true;
                         // Try to load path from config as best-effort for UI
                         if let Ok(cfg) = crate::config::Config::load(None) {
                             self.video_path = cfg.cinnamon.last_video_path;
                         }
                         log::info!("Restored active wallpaper persistence: PID {}", pid);
                    } else {
                        // Stale PID file
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
    }

    fn is_same_video(&self, path: &PathBuf) -> bool {
        if let Some(ref current) = self.video_path {
            current == path
        } else {
            false
        }
    }
}

fn get_pid_file_path() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join("cinnamon-wallpaper.pid")
    } else if let Some(home) = dirs::home_dir() {
        home.join(".local/share/cinnamon-wallpaper/cinnamon-wallpaper.pid")
    } else {
        PathBuf::from("/tmp/cinnamon-wallpaper.pid")
    }
}

static ACTIVE_WALLPAPER: std::sync::LazyLock<Arc<Mutex<ActiveWallpaper>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(ActiveWallpaper::default())));

// Flag used to request that the running wallpaper process stop gracefully.
static STOP_REQUESTED: std::sync::LazyLock<std::sync::Arc<std::sync::atomic::AtomicBool>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));

/// Request the running wallpaper (possibly this process) to stop gracefully.
pub fn request_stop_wallpaper() {
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Query whether a stop was requested.
// pub fn should_stop_requested() -> bool {
//    if let Ok(guard) = STOP_REQUESTED.lock() {
//        *guard
//    } else {
//        false
//    }
// }

// Estructura para manejar el estado de video preview en las cards
#[derive(Debug)]
struct VideoCardState {
    pipeline: Option<Pipeline>,
    video_sink: Option<Element>,
    is_playing: bool,
    preview_textures: Option<Vec<gdk4::Texture>>,
    preview_timeout: Option<glib::SourceId>,
    preview_generating: bool,
    // poster_texture and video_path removed because they were unused
}

impl VideoCardState {
    fn new(_video_path: PathBuf) -> Self {
        Self {
            pipeline: None,
            video_sink: None,
            is_playing: false,
            preview_textures: None,
            preview_timeout: None,
            preview_generating: false,
        }
    }

    fn cleanup(&mut self) {
        if let Some(pipeline) = &self.pipeline {
            let _ = pipeline.set_state(State::Null);
        }
        self.pipeline = None;
        self.video_sink = None;
        self.is_playing = false;
        // Stop any running preview animation
        if let Some(src) = self.preview_timeout.take() {
            src.remove();
        }
        self.preview_textures = None;
    }
}

impl Drop for VideoCardState {
    fn drop(&mut self) {
        self.cleanup();
    }
}

// Función simplificada para crear un pipeline de video preview
// Nota: la integración directa con GStreamer (playbin + paintable sink) fue
// descartada temporalmente porque muchos sistemas no tienen el plugin
// 'gtk4paintablesink' y la inserción de widgets de sink causó errores
// GLib/GObject en tiempo de ejecución. Actualmente usamos un mecanismo de
// preview basado en frames extraídos con ffmpeg. Si en el futuro volvemos a
// intentar una pipeline GStreamer, implementar aquí la creación y retorno
// de (Pipeline, Element).

// Genera N frames de preview usando ffmpeg y devuelve rutas a los archivos generados.
fn generate_preview_frames(video_path: &PathBuf, count: usize) -> Option<Vec<PathBuf>> {
    // If the source video no longer exists (was deleted), bail out early to avoid
    // spawning ffmpeg on a non-existent input and avoid recreating asset folders.
    if !video_path.exists() {
        log::debug!("🔍 generate_preview_frames: video no longer existe: {}", video_path.display());
        return None;
    }
    // Prefer assets folder next to the video file in the library: <video-stem>/previews/
    let preview_dir = get_video_assets_dir(video_path).join("previews");
    if let Err(_) = std::fs::create_dir_all(&preview_dir) {
        // Fallback to global previews dir
        let fallback = get_thumbnail_dir().join("previews");
        if let Err(_) = std::fs::create_dir_all(&fallback) {
            return None;
        }
    }

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    video_path.hash(&mut hasher);
    let base = format!("preview_{:x}", hasher.finish());

    let mut paths = Vec::new();
    // Extract a few frames evenly spaced across the first 8 seconds or video duration
        // We'll attempt to extract 'count' frames evenly spaced using a single ffmpeg invocation
        // by sampling frames in a given time window and writing numbered PNGs.
        // Determine a temporary output pattern path
    // Prefer per-video previews directory if available
    let per_video_pattern = get_video_assets_dir(video_path).join("previews").join(format!("{}_%03d.png", base));
    let global_pattern = get_thumbnail_dir().join("previews").join(format!("{}_%03d.png", base));
    let pattern = if get_video_assets_dir(video_path).exists() { per_video_pattern } else { global_pattern };
        // If all expected files already exist, collect and return them
        let mut all_exist = true;
        for i in 0..count {
            let idx = i + 1; // ffmpeg uses 1-based index for %03d
            let filename = format!("{}_{:03}.png", base, idx);
            let outpath = preview_dir.join(&filename);
            if !outpath.exists() {
                all_exist = false;
                break;
            }
        }
        if all_exist {
            for i in 0..count {
                let idx = i + 1;
                let filename = format!("{}_{:03}.png", base, idx);
                paths.push(preview_dir.join(filename));
            }
            return Some(paths);
        }

        // Extract a contiguous 3s segment at 20 fps to produce smooth previews (3s * 20fps = 60 frames)
        // If caller requested a different count, we'll approximate fps = count / 3.0
        let target_duration_secs = 3.0_f64;
        let fps = (count as f64 / target_duration_secs).max(1.0);
        let result = Command::new("ffmpeg")
            .args(&[
                "-loglevel",
                "error",
                "-ss",
                "1",
                "-t",
                &format!("{:.3}", target_duration_secs),
                "-i",
                &video_path.to_string_lossy(),
                "-vf",
                &format!("fps={:.3},scale=320:-1:force_original_aspect_ratio=decrease:flags=lanczos", fps),
                "-y",
                &pattern.to_string_lossy(),
            ])
            .output();

        match result {
            Ok(output) => {
                if !output.status.success() {
                    log::error!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr));
                    return None;
                }
                // Collect produced files - ffmpeg produces sequential files starting at 1 by default
                for i in 0..count {
                    // ffmpeg pattern uses 1-based indexing by default for %03d
                    let idx = i + 1;
                    let filename = format!("{}_{:03}.png", base, idx);
                    // Check per-video first, then global
                    let per = get_video_assets_dir(video_path).join("previews").join(&filename);
                    let glob = get_thumbnail_dir().join("previews").join(&filename);
                    if per.exists() {
                        paths.push(per);
                    } else if glob.exists() {
                        paths.push(glob);
                    }
                }
                if paths.is_empty() {
                    return None;
                }
            }
            Err(e) => {
                log::error!("Error launching ffmpeg: {}", e);
                return None;
            }
        }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

// Ejecuta generate_preview_frames en un hilo y carga texturas en el main thread cuando estén listas.
fn generate_preview_frames_async(video_path: PathBuf, count: usize, callback: impl Fn(Option<Vec<gdk4::Texture>>) + 'static) {
    use std::sync::mpsc;
    use std::time::Duration;

    // Create a channel to receive the worker result on the main thread
    let (tx, rx) = mpsc::channel::<Option<Vec<PathBuf>>>();

    // Spawn worker thread to compute preview frame paths
    std::thread::spawn(move || {
        let paths = generate_preview_frames(&video_path, count);
        // Ignore send errors (receiver dropped)
        let _ = tx.send(paths);
    });

    // Poll the receiver on the main loop until we get a result.
    // This keeps the callback on the main thread and avoids Send bounds.
    glib::timeout_add_local(Duration::from_millis(50), move || {
        match rx.try_recv() {
            Ok(maybe_paths) => {
                if let Some(paths) = maybe_paths {
                    let textures = load_textures_from_paths(&paths);
                    callback(Some(textures));
                } else {
                    callback(None);
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

// Carga una lista de rutas a Textures (gdk4::Texture)
fn load_textures_from_paths(paths: &[PathBuf]) -> Vec<gdk4::Texture> {
    let mut textures = Vec::new();
    for p in paths {
        if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(p) {
            // Escalar a ancho objetivo consistente con thumbnails para evitar cambios de tamaño
            let target_width = 320;
            let (w, h) = (pixbuf.width(), pixbuf.height());
            let tex = if w > target_width {
                let target_height = if w > 0 { (h * target_width) / w } else { h };
                if let Some(scaled) = pixbuf.scale_simple(target_width, target_height, gdk_pixbuf::InterpType::Bilinear) {
                    gdk4::Texture::for_pixbuf(&scaled)
                } else {
                    gdk4::Texture::for_pixbuf(&pixbuf)
                }
            } else {
                gdk4::Texture::for_pixbuf(&pixbuf)
            };
            textures.push(tex);
        }
    }
    textures
}

// Start animating textures into a Picture at interval_ms. Returns SourceId
fn start_preview_animation(picture: &Picture, textures: Vec<gdk4::Texture>, interval_ms: u32) -> Option<glib::SourceId> {
    if textures.is_empty() {
        return None;
    }

    let picture = picture.clone();
    let mut idx = 0usize;
    let len = textures.len();
    let src = glib::timeout_add_local(std::time::Duration::from_millis(interval_ms as u64), move || {
        let tex = &textures[idx % len];
        picture.set_paintable(Some(tex));
        idx = idx.wrapping_add(1);
        glib::ControlFlow::Continue
    });

    Some(src)
}

// Función simplificada para extraer poster del video
fn extract_video_poster(video_path: &PathBuf) -> Option<gdk4::Texture> {
    // Preferir thumbnail dentro de la carpeta de assets del video (si existe)
    // Ej: <library>/<video-stem>/thumbnails/thumb_{hash}.png
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let asset_dir = get_video_assets_dir(video_path);

    let mut hasher = DefaultHasher::new();
    video_path.hash(&mut hasher);
    let hash = hasher.finish();
    let thumb_name = format!("thumb_{:x}.png", hash);

    let candidate = asset_dir.join("thumbnails").join(&thumb_name);
    if candidate.exists() {
        if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(&candidate) {
            let width = pixbuf.width();
            let height = pixbuf.height();
            let target_width = 320;
            if let Some(scaled) = pixbuf.scale_simple(target_width, if width > 0 { (height * target_width) / width } else { height }, gdk_pixbuf::InterpType::Bilinear) {
                return Some(gdk4::Texture::for_pixbuf(&scaled));
            } else {
                return Some(gdk4::Texture::for_pixbuf(&pixbuf));
            }
        }
    }

    // Fallback: buscar en el directorio global de thumbnails (antiguo comportamiento)
    let global_candidate = get_thumbnail_dir().join(&thumb_name);
    if global_candidate.exists() {
        if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(&global_candidate) {
            let width = pixbuf.width();
            let height = pixbuf.height();
            let target_width = 320;
            if let Some(scaled) = pixbuf.scale_simple(target_width, if width > 0 { (height * target_width) / width } else { height }, gdk_pixbuf::InterpType::Bilinear) {
                return Some(gdk4::Texture::for_pixbuf(&scaled));
            } else {
                return Some(gdk4::Texture::for_pixbuf(&pixbuf));
            }
        }
    }

    None
}

// Función para obtener el directorio de la biblioteca de videos
fn get_video_library_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("cinnamon-wallpaper")
        .join("videos")
}

// Función para obtener el directorio de thumbnails
fn get_thumbnail_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("cinnamon-wallpaper")
        .join("thumbnails")
}

// Función que devuelve la carpeta de assets para un video (ej: <library>/<video-stem>)
fn get_video_assets_dir(video_path: &PathBuf) -> PathBuf {
    // Si el video está en la biblioteca, su carpeta contenedora es su directorio de assets
    let lib = get_video_library_dir();
    if video_path.starts_with(&lib) && video_path.exists() {
        if let Some(parent) = video_path.parent() {
            // El parent ES la carpeta única del video
            return parent.to_path_buf();
        }
    }
    // Si no está en library (externo), devolver la global thumbnails dir para compatibilidad
    get_thumbnail_dir()
}

// Helper que devuelve el texto de estado para la badge (Optimizado/Original)
fn badge_state_text(filename: &str) -> String {
    if filename.contains("_optimizado") {
        "⚡ Optimizado".to_string()
    } else {
        "📀 Original".to_string()
    }
}

// Crea y configura un Label para la badge de estado (Optimizado/Original)
fn create_badge_label(filename: &str) -> gtk4::Label {
    let lbl = gtk4::Label::new(Some(&badge_state_text(filename)));
    lbl.add_css_class("card-badge");
    lbl.set_halign(gtk4::Align::Start);
    lbl.set_hexpand(false);
    lbl.set_margin_bottom(2);
    lbl
}

// Registry helpers that operate on a per-window card_registry
fn register_card_to_registry(registry: &std::rc::Rc<std::cell::RefCell<Vec<std::rc::Rc<std::cell::RefCell<VideoCardState>>>>>, card_state: &std::rc::Rc<std::cell::RefCell<VideoCardState>>) {
    let mut guard = registry.borrow_mut();
    guard.push(card_state.clone());
}

fn stop_other_previews_for_registry(registry: &std::rc::Rc<std::cell::RefCell<Vec<std::rc::Rc<std::cell::RefCell<VideoCardState>>>>>, current_state: &std::rc::Rc<std::cell::RefCell<VideoCardState>>) {
    let guard = registry.borrow_mut();
    for state_rc in guard.iter() {
        if !std::rc::Rc::ptr_eq(state_rc, current_state) {
            let mut s = state_rc.borrow_mut();
            if s.is_playing {
                if let Some(src) = s.preview_timeout.take() {
                    src.remove();
                }
                s.is_playing = false;
                s.preview_textures = None;
            }
        }
    }
}

// Función para copiar un video a la biblioteca
fn copy_video_to_library(source_path: &PathBuf) -> Result<PathBuf> {
    let library_dir = get_video_library_dir();
    if let Err(e) = std::fs::create_dir_all(&library_dir) {
        return Err(anyhow::anyhow!("No se pudo crear el directorio de biblioteca: {}", e));
    }

    let filename = source_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Nombre de archivo inválido"))?;

    // Generar hash para crear carpeta única
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    source_path.hash(&mut hasher);
    let hash = hasher.finish();

    let stem = source_path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| anyhow::anyhow!("Stem inválido"))?;
    // Carpeta única: Nombre_Hash
    let folder_name = format!("{}_{:x}", stem, hash);
    let video_dir = library_dir.join(&folder_name);
    
    if let Err(e) = std::fs::create_dir_all(&video_dir) {
        return Err(anyhow::anyhow!("No se pudo crear la carpeta del video: {}", e));
    }

    let dest_path = video_dir.join(filename);
    if dest_path.exists() {
        return Ok(dest_path);
    }

    std::fs::copy(&source_path, &dest_path)
        .map_err(|e| anyhow::anyhow!("Error copiando archivo: {}", e))?;

    // Migraciones legacy eliminadas para simplificar
    Ok(dest_path)
}

// Funciones para manejar la pausa del monitoreo
fn resume_monitoring() {
    if let Ok(mut paused) = MONITORING_PAUSED.lock() {
        *paused = false;
    }
    
    // Marcar el tiempo de reanudación para implementar debounce
    if let Ok(mut last_resume) = LAST_RESUME_TIME.lock() {
        *last_resume = Some(std::time::Instant::now());
    }
    
    // Marcar que hay una actualización pendiente
    if let Ok(mut pending) = PENDING_UPDATE.lock() {
        *pending = true;
    }
}

fn pause_monitoring() {
    if let Ok(mut paused) = MONITORING_PAUSED.lock() {
        *paused = true;
    }
}

fn is_monitoring_paused() -> bool {
    MONITORING_PAUSED.lock().map(|paused| *paused).unwrap_or(false)
}

fn should_allow_update() -> bool {
    // Si está pausado, no permitir actualizaciones
    if is_monitoring_paused() {
        return false;
    }
    
    // Verificar debounce después de reanudar
    if let Ok(last_resume) = LAST_RESUME_TIME.lock() {
        if let Some(resume_time) = *last_resume {
            // Esperar 5 segundos después de reanudar antes de permitir actualizaciones
            if resume_time.elapsed() < std::time::Duration::from_secs(5) {
                return false;
            }
        }
    }
    
    true
}

fn has_pending_update() -> bool {
    PENDING_UPDATE.lock().map(|pending| *pending).unwrap_or(false)
}

fn clear_pending_update() {
    if let Ok(mut pending) = PENDING_UPDATE.lock() {
        *pending = false;
    }
}

// Funciones para manejar el modo de vista
fn is_list_mode() -> bool {
    VIEW_MODE_LIST.lock().map(|mode| *mode).unwrap_or(false)
}

fn toggle_view_mode() {
    if let Ok(mut mode) = VIEW_MODE_LIST.lock() {
        *mode = !*mode;
    }
}

#[derive(Clone)]
pub struct WallpaperApp {
    video_path: std::rc::Rc<std::cell::RefCell<Option<PathBuf>>>,
}

impl WallpaperApp {
    pub fn new() -> Result<Self> {
        let video_path = std::rc::Rc::new(std::cell::RefCell::new(None));

        Ok(Self {
            video_path,
        })
    }

    pub fn run(&self) -> glib::ExitCode {
        let app = Application::builder()
            .application_id(APP_ID)
            .build();

        let video_path_clone = self.video_path.clone();
        
        app.connect_activate(move |app| {
            let window = ApplicationWindow::builder()
                .application(app)
                .title("Cinnamon Wallpaper")
                .default_width(900)
                .default_height(700)
                .resizable(true)
                .build();

            // Instalar (en modo usuario) el SVG de icono incluido en el repo
            // en ~/.local/share/icons/hicolor/scalable/apps/cinnamon-wallpaper.svg
            if let Ok(exe_path) = std::env::current_exe() {
                if let Some(parent) = exe_path.parent() {
                    // Intentar localizar el svg dentro del repo (target/../../assets/...)
                    let candidate = parent.join("..").join("..").join("assets").join("icons").join("LiveWallpaperIcon.svg");
                    let candidate: Result<std::path::PathBuf, std::io::Error> = candidate.canonicalize().or_else(|_| Ok(parent.join("assets").join("icons").join("LiveWallpaperIcon.svg")));
                    if let Ok(icon_path) = candidate {
                        if icon_path.exists() {
                            if let Some(home) = dirs::home_dir() {
                                let user_icon_dir = home.join(".local").join("share").join("icons").join("hicolor").join("scalable").join("apps");
                                let _ = std::fs::create_dir_all(&user_icon_dir);
                                let dest = user_icon_dir.join("cinnamon-wallpaper.svg");
                                // Copiar si no existe o es distinto
                                let _ = std::fs::copy(&icon_path, &dest);
                                // Llamar a gtk para usar el icon name
                                gtk4::Window::set_default_icon_name("cinnamon-wallpaper");
                            }
                        }
                    }
                }
            }

            let temp_app = WallpaperAppWindow {
                window: window.clone(),
                video_path: video_path_clone.clone(),
                main_container: std::rc::Rc::new(std::cell::RefCell::new(None)),
                gallery_container: std::rc::Rc::new(std::cell::RefCell::new(None)),
                header_count_label: std::rc::Rc::new(std::cell::RefCell::new(None)),
                    card_registry: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            };
            
            temp_app.build_ui();
            window.present();
        });

        app.run_with_args(&[] as &[&str])
    }
}

#[derive(Clone)]
struct WallpaperAppWindow {
    window: ApplicationWindow,
    video_path: std::rc::Rc<std::cell::RefCell<Option<PathBuf>>>,
    main_container: std::rc::Rc<std::cell::RefCell<Option<Box>>>,
    gallery_container: std::rc::Rc<std::cell::RefCell<Option<ScrolledWindow>>>,
    header_count_label: std::rc::Rc<std::cell::RefCell<Option<Label>>>,
    // Registro por-instancia de estados de cards para permitir detener previews cruzadas
    card_registry: std::rc::Rc<std::cell::RefCell<Vec<std::rc::Rc<std::cell::RefCell<VideoCardState>>>>>,
}

impl WallpaperAppWindow {
    fn create_drop_area(&self, parent: &Box) {
        // Limpiar galería anterior si existe
        *self.gallery_container.borrow_mut() = None;
        
    let subtitle = Label::new(Some("🎯 No se encontraron videos. Arrastra un video aquí para comenzar."));
    subtitle.set_markup("<span size='large'>🎯 No se encontraron videos. Arrastra un video aquí para comenzar.</span>");
    subtitle.add_css_class("subtitle");
    // Alinear a la izquierda y ocupar el ancho para quedar bajo el título
    subtitle.set_halign(gtk4::Align::Start);
    subtitle.set_hexpand(true);
    subtitle.set_justify(gtk4::Justification::Left);
    subtitle.set_margin_top(8);
    subtitle.set_margin_bottom(12);
    parent.append(&subtitle);

        let drop_area = Box::new(Orientation::Vertical, 20);
        drop_area.add_css_class("drop-area");
        drop_area.set_halign(gtk4::Align::Fill);
        drop_area.set_valign(gtk4::Align::Center);
        drop_area.set_vexpand(true);

        // Icono y texto de arrastrar y soltar
        let icon_label = Label::new(Some("🎬"));
        icon_label.set_markup("<span size='80000'>🎬</span>");
        drop_area.append(&icon_label);

        let instruction_label = Label::new(Some("Arrastra y suelta un video aquí"));
        instruction_label.set_markup("<span size='x-large' weight='bold'>Arrastra y suelta un video aquí</span>");
        drop_area.append(&instruction_label);

        let format_label = Label::new(Some("Formatos soportados: MP4, AVI, MKV, WEBM, MOV, WMV, FLV, M4V"));
        format_label.set_markup("<span size='medium' color='#888'>Formatos soportados: MP4, AVI, MKV, WEBM, MOV, WMV, FLV, M4V</span>");
        drop_area.append(&format_label);

        // Configurar drag and drop
        let drop_target = DropTarget::new(FileList::static_type(), DragAction::COPY);
        let video_path_clone = self.video_path.clone();
        let self_clone_dt = self.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            let mut any_added = false;
            if let Ok(file_list) = value.get::<FileList>() {
                for file in file_list.files() {
                    if let Some(path) = file.path() {
                        if is_supported_video_format(&path) {
                            match copy_video_to_library(&path) {
                                Ok(library_path) => {
                                    *video_path_clone.borrow_mut() = Some(library_path.clone());
                                    show_success_message(&format!("🎉 Video añadido a la biblioteca: {}", 
                                        library_path.file_name().unwrap_or_default().to_string_lossy()));
                                    any_added = true;
                                        // Generar previews en background inmediatamente después de añadir el video
                                    {
                                        let lib_clone = library_path.clone();
                                        std::thread::spawn(move || {
                                            let _ = generate_preview_frames(&lib_clone, 60);
                                        });
                                    }
                                }
                                Err(e) => {
                                    show_error_dialog(&format!("❌ Error al copiar video a la biblioteca: {}", e));
                                }
                            }
                        } else {
                            show_error_dialog(&format!("❌ Formato no soportado: {}", path.display()));
                        }
                    }
                }
            }
            if any_added {
                // Forzar actualización inmediata
                self_clone_dt.update_gallery();
            }
            any_added
        });
        drop_target.connect_enter(|drop_target, _, _| {
            let widget = drop_target.widget();
            widget.add_css_class("drop-area-hover");
            DragAction::COPY
        });
        drop_target.connect_leave(|drop_target| {
            let widget = drop_target.widget();
            widget.remove_css_class("drop-area-hover");
        });
        drop_area.add_controller(drop_target);

        // Botón alternativo para seleccionar archivo
        let select_button = Button::with_label("📁 Seleccionar archivo");
        select_button.add_css_class("add-btn");
        select_button.set_margin_top(30);
        let video_path_clone2 = self.video_path.clone();
        let self_clone_for_cb = self.clone();
        select_button.connect_clicked(move |_| {
            let inner_self = self_clone_for_cb.clone();
            WallpaperAppWindow::open_file_dialog(video_path_clone2.clone(), Some(std::boxed::Box::new(move |_p| {
                inner_self.update_gallery();
            })));
        });
        drop_area.append(&select_button);
        parent.append(&drop_area);
    }
    pub fn build_ui(&self) {
        // CSS para estilizar la interfaz
        let css_provider = CssProvider::new();
        css_provider.load_from_data(r#"
            .main-container {
                padding: 10px;
            }
            .header {
                margin-bottom: 15px;
            }
            .drop-area {
                border: 2px dashed #cccccc;
                border-radius: 10px;
                background-color: #f9f9f9;
                transition: border-color 300ms ease-in-out, background-color 300ms ease-in-out;
                min-height: 200px;
                margin: 10px;
                padding: 20px;
            }
            /* Quitar fondo blanco que algunos temas aplican al ScrolledWindow/viewport/list */
            scrolledwindow,
            scrolledwindow > viewport,
            scrolledwindow > viewport > list,
            viewport {
                background-color: transparent;
                background-image: none;
                border: none;
                box-shadow: none;
            }
            .drop-area-hover {
                border-color: #4a90e2;
                /* Evitar fondo azul; mantener transparente para que el tema controle el color */
                background-color: transparent;
            }
            /* Tarjeta: ahora sin fondo blanco; la Picture será el fondo real */
            .video-card {
                border: 1px solid #ddd;
                border-radius: 8px;
                margin: 6px;
                padding: 0; /* el contenido se coloca encima del fondo */
                background: transparent;
                transition: border-color 300ms ease-in-out, box-shadow 300ms ease-in-out;
                min-width: 150px;
            }
            .video-card:hover {
                border-color: #4a90e2;
                box-shadow: 0 4px 15px rgba(74, 144, 226, 0.3);
            }
            /* El inspector muestra GtkListBoxRow con la clase `activatable` y el tema
               le aplica background-color: rgb(31,158,222). Aquí lo anulamos con
               selectores más específicos y !important para sobreescribir el tema. */
            listbox row.activatable,
            listbox row.activatable:selected,
            listbox row.activatable:hover,
            listbox row.activatable:backdrop {
                background-color: transparent;
                background-image: none;
                border-color: transparent;
                box-shadow: none;
            }
            /* También anular cualquier fondo en la frame interna que mostramos como tarjeta */
            listbox row.activatable > frame.video-card-list,
            .video-card-list,
            .video-card-list .video-card {
                background-color: transparent;
                background-image: none;
                box-shadow: none;
            }
            /* Selector muy específico que replica la ruta mostrada por GTK Inspector
               (window -> box -> scrolledwindow -> viewport -> list -> row) para
               sobreescribir cualquier regla del tema que pinte las filas de azul. */
            window > box > scrolledwindow > viewport > list > row,
            window > box > scrolledwindow > viewport > list > row.activatable,
            window > box > scrolledwindow > viewport > list > row.activatable:selected,
            window > box > scrolledwindow > viewport > list > row.activatable:hover {
                background: transparent;
                background-color: transparent;
                background-image: none;
                border: none;
                box-shadow: none;
            }
            /* Texto sobre video: colores claros para legibilidad */
            .video-title {
                font-weight: bold;
                margin-bottom: 6px;
                color: #fff;
                font-size: 13px;
                text-shadow: 0 1px 2px rgba(0,0,0,0.6);
            }
            .video-info {
                color: #ddd;
                font-size: 10px;
                margin-bottom: 8px;
                text-shadow: 0 1px 2px rgba(0,0,0,0.6);
            }
            .video-thumbnail {
                border-radius: 6px;
                margin-bottom: 8px;
            }
            .action-button {
                padding: 4px 8px;
                border-radius: 6px;
                font-weight: 500;
                font-size: 11px;
                transition: all 200ms ease-in-out;
            }
            /* Botones de acción con fondo semitransparente similar a badges */
            /* Botones de acción con fondo semitransparente pero menos translúcido */
            .preview-btn {
                background-color: rgba(33,150,243,0.60); /* azul menos translúcido */
                color: #fff;
                border: 1px solid rgba(255,255,255,0.12);
            }
            .convert-btn {
                background-color: rgba(156,39,176,0.60); /* morado menos translúcido */
                color: #fff;
                border: 1px solid rgba(255,255,255,0.12);
            }
            .apply-btn {
                background-color: rgba(76,175,80,0.60); /* verde menos translúcido */
                color: #fff;
                border: 1px solid rgba(255,255,255,0.12);
            }
            .delete-btn {
                background-color: rgba(244,67,54,0.60); /* rojo menos translúcido */
                color: #fff;
                border: 1px solid rgba(255,255,255,0.12);
            }
            /* Hover ligero para botones semitransparentes */
            .action-button:hover {
                transform: translateY(-1px);
                box-shadow: 0 4px 12px rgba(0,0,0,0.25);
                filter: brightness(1.05);
            }
            /* Hover específico: quitar transparencia y mostrar color sólido completo */
            .preview-btn:hover {
                background-color: #2196F3; /* azul sólido */
                opacity: 1;
            }
            .convert-btn:hover {
                background-color: #9C27B0; /* morado sólido */
                opacity: 1;
            }
            .apply-btn:hover {
                background-color: #4CAF50; /* verde sólido */
                opacity: 1;
            }
            .delete-btn:hover {
                background-color: #F44336; /* rojo sólido */
                opacity: 1;
            }
            .add-btn {
                background: #FF9800;
                color: white;
                padding: 8px 16px;
                font-size: 13px;
            }
            .refresh-btn {
                /* Usar el verde sólido #4CAF50 para un aspecto más claro y consistente */
                background-color: #4CAF50;
                color: #fff;
                border: 1px solid rgba(0,0,0,0.08);
            }
            .refresh-btn:hover {
                /* Hover ligeramente más oscuro para indicar interacción */
                background-color: #43A047;
                color: #fff;
                opacity: 1;
            }
            /* Subtítulos (ej. Biblioteca / Total disponibles) dejar heredar color del tema */
            .subtitle {
                /* no establecer color explícito para respetar el tema (claro/oscuro) */
            }
            /* Botón para alternar modo de vista: fondo sólido (sin transparencia) */
            .view-mode-btn {
                background-color: #2196F3; /* azul sólido */
                color: #fff;
                border: 1px solid rgba(0,0,0,0.08);
            }
            /* Botón Detener (stop) debe ser sólido y visible, sin transparencia */
            .stop-btn {
                background-color: #F44336; /* rojo sólido */
                color: #fff;
                border: 1px solid rgba(0,0,0,0.08);
            }
            /* Clase específica para los botones en la barra inferior (solo afecta a esos dos) */
            .bottom-btn {
                padding: 4px 8px;
                font-size: 11px;
                min-height: 34px;
                min-width: 0;
                border-radius: 6px;
            }
            /* Estilos para modo lista: quitar fondo blanco y usar fondo por Picture */
            .video-card-list {
                border: 1px solid #ddd;
                border-radius: 8px;
                margin: 3px 4px;
                padding: 0;
                background: transparent;
                transition: border-color 200ms ease-in-out, box-shadow 200ms ease-in-out;
                /* Altura compacta para modo lista (similar a un item list convencional) */
                min-height: 120px;
            }
            .video-card-list:hover {
                border-color: #4a90e2;
                box-shadow: 0 2px 8px rgba(74, 144, 226, 0.3);
            }
            /* Asegurar que el Overlay y Picture actúen como fondo completo */
            .video-overlay {
                border-radius: 8px;
            }
            .video-background {
                border-radius: 8px;
                /* Rely on widget expand/fill from code; avoid percentage sizes in GTK CSS */
            }
            .video-gradient {
                background: linear-gradient(rgba(0,0,0,0.0), rgba(0,0,0,0.45));
            }
            /* Header: clases para título y contador - usamos CSS/markup consistente
               para asegurar que sus baselines coincidan (misma métrica/line-height). */
            .header-title {
                /* Usar un tamaño fijo y line-height idéntico para alinear baselines */
                font-size: 22px;
                line-height: 22px;
                margin-top: 0;
                margin-bottom: 0;
            }
            .header-count  {
                font-size: 14px;
                line-height: 14px;
                margin-top: 0;
                margin-bottom: 0;
            }
            .header-title {
                font-weight: 700; /* título en negrita */
            }
            /* Badge en la esquina superior izquierda (Original / Optimizado) */
            .card-badge {
                background-color: rgba(0,0,0,0.45);
                color: #fff;
                padding: 6px 10px;
                border-radius: 6px;
                font-weight: 600;
                font-size: 11px;
                box-shadow: 0 2px 6px rgba(0,0,0,0.4);
            }
            /* Información en la esquina superior derecha (Tamaño | Resolución) - estilo igual que badge */
            .card-info-top-right {
                background-color: rgba(0,0,0,0.45);
                color: #fff;
                padding: 6px 10px;
                border-radius: 6px;
                font-weight: 600;
                font-size: 11px;
                box-shadow: 0 2px 6px rgba(0,0,0,0.4);
                text-shadow: 0 1px 2px rgba(0,0,0,0.5);
            }
            /* Si quieres que el botón Detener sea del mismo color naranja, usa .bottom-btn.delete-btn */
        "#);

        if let Some(display) = Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &css_provider,
                STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        // Crear contenedor principal
        let main_box = Box::new(Orientation::Vertical, 0);
        main_box.add_css_class("main-container");
        
        // Almacenar referencia al contenedor principal
        *self.main_container.borrow_mut() = Some(main_box.clone());
        
    // Header: título a la izquierda, contador de videos a la derecha (se actualizará en create_video_gallery)
    let header_box = Box::new(Orientation::Horizontal, 8);
    header_box.set_hexpand(true);
    header_box.set_halign(gtk4::Align::Fill);
    // Centrar verticalmente los elementos dentro del header
    header_box.set_valign(gtk4::Align::Center);

    let title_label = Label::new(Some("🎬 Cinnamon Live Wallpaper"));
    title_label.add_css_class("header-title");
    // Preferir markup solo para el emoji/énfasis si es necesario, pero mantener tamaño/line-height desde CSS
    title_label.set_markup("<span weight='bold'>🎬 Cinnamon Live Wallpaper</span>");
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_hexpand(true);
    title_label.set_valign(gtk4::Align::Center);

    // Contador a la derecha; será actualizado en create_video_gallery
    let count_label = Label::new(Some(""));
    count_label.add_css_class("header-count");
    count_label.set_halign(gtk4::Align::End);
    count_label.set_hexpand(false);
    count_label.set_valign(gtk4::Align::Center);
    header_box.append(&title_label);
    header_box.append(&count_label);

    // Añadir el header directamente (sin overlay de depuración)
    main_box.append(&header_box);
    *self.header_count_label.borrow_mut() = Some(count_label);

        // Inicializar la galería
        self.update_gallery();

        self.window.set_child(Some(&main_box));
        
        // Configurar monitoreo de archivos
        self.setup_file_monitoring();
    }

    fn update_gallery(&self) {
    // Previously attempted to stop global previews here; previews are tracked per-card now.

        let main_box = if let Some(main_container) = self.main_container.borrow().as_ref() {
            main_container.clone()
        } else {
            return;
        };

        // Obtener el header (primer widget) para preservarlo (puede ser Label o un Box personalizado)
        let mut header_widget: Option<gtk4::Widget> = None;
        let widget_child = main_box.first_child();
        if let Some(first_widget) = widget_child {
            header_widget = Some(first_widget.clone());
        }

        // Limpiar todos los widgets del contenedor principal
        while let Some(child) = main_box.last_child() {
            main_box.remove(&child);
        }

        // Restaurar el header (si existía)
        if let Some(header) = header_widget {
            main_box.append(&header);
        }

        // Limpiar referencia a la galería
        *self.gallery_container.borrow_mut() = None;

        // Buscar videos existentes
        let existing_videos = self.find_existing_videos();
        
        if existing_videos.is_empty() {
            // No hay videos: mostrar área de arrastrar y soltar
            self.create_drop_area(&main_box);
        } else {
            // Hay videos: mostrar galería
            self.create_video_gallery(&main_box, existing_videos);
        }
    }

    fn setup_file_monitoring(&self) {
        let library_dir = get_video_library_dir();
        
        // Crear directorio si no existe para poder monitorearlo
        let _ = std::fs::create_dir_all(&library_dir);
        
        // Crear canal para comunicación entre threads
        let (tx, rx) = mpsc::channel();
        
        // Configurar el watcher en un thread separado
        let library_dir_clone = library_dir.clone();
        thread::spawn(move || {
            match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        // Filtrar solo eventos relevantes (crear, eliminar, mover archivos)
                        match event.kind {
                            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) => {
                                // Verificar si algún archivo afectado es un video
                                for path in &event.paths {
                                    if is_supported_video_format(path) {
                                        // Enviar señal de actualización
                                        let _ = tx.send(());
                                        break;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        log::error!("❌ Error en el monitoreo de archivos: {}", e);
                    }
                }
            }) {
                Ok(mut watcher) => {
                    // Iniciar monitoreo del directorio de biblioteca (recursivo para detectar cambios en subcarpetas por video)
                    if let Err(e) = watcher.watch(&library_dir_clone, RecursiveMode::Recursive) {
                        log::error!("❌ No se pudo monitorear el directorio {}: {}", library_dir_clone.display(), e);
                        return;
                    }
                    
                    // Mantener el watcher vivo
                    loop {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
                Err(e) => {
                    log::error!("❌ No se pudo crear el watcher de archivos: {}", e);
                }
            }
        });
        
        // Configurar receptor en el hilo principal de GTK
        let self_clone = self.clone();
        glib::timeout_add_local(Duration::from_millis(1000), move || {
            let mut update_needed = false;
            
            // Consumir TODOS los mensajes pendientes en el canal
            loop {
                match rx.try_recv() {
                    Ok(_) => {
                        // Solo marcar que hay una actualización pendiente, no actualizar inmediatamente
                        update_needed = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        // No hay más mensajes, salir del loop
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // Canal desconectado, terminar timer
                        return glib::ControlFlow::Break;
                    }
                }
            }
            
            // Si había eventos Y está permitido actualizar, hacerlo
            if update_needed && should_allow_update() {
                self_clone.update_gallery();
                show_success_message("🔄 Galería actualizada automáticamente");
                clear_pending_update();
            }
            // Si había eventos pero no se puede actualizar, marcar como pendiente
            else if update_needed && !is_monitoring_paused() {
                if let Ok(mut pending) = PENDING_UPDATE.lock() {
                    *pending = true;
                }
            }
            // Si no había eventos nuevos, verificar actualizaciones pendientes
            else if should_allow_update() && has_pending_update() {
                self_clone.update_gallery();
                show_success_message("🔄 Galería actualizada (cambios detectados)");
                clear_pending_update();
            }
            
            glib::ControlFlow::Continue
        });
    }

    fn find_existing_videos(&self) -> Vec<PathBuf> {
        let mut videos = Vec::new();
        
        // PRIORIDAD 1: Buscar en la biblioteca de videos
        let library_dir = get_video_library_dir();
        if library_dir.exists() {
            // Buscar recursivamente hasta una profundidad razonable (subcarpeta por video)
            // Aumentamos la profundidad a 4 para detectar subcarpetas como `optimized/<output_stem>/`
            for entry in WalkDir::new(&library_dir).max_depth(4).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path().to_path_buf();
                if path.is_file() && is_supported_video_format(&path) {
                    videos.push(path);
                }
            }
        }
        
        // PRIORIDAD 2: Buscar en la carpeta de videos del proyecto (para desarrollo)
        if let Ok(entries) = fs::read_dir("src/video") {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_supported_video_format(&path) {
                    videos.push(path);
                }
            }
        }

        // PRIORIDAD 3: Buscar en la carpeta de assets (para desarrollo)
        if let Ok(entries) = fs::read_dir("assets") {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_supported_video_format(&path) {
                    videos.push(path);
                }
            }
        }

        // Solo buscar en otros directorios si no hay videos en la biblioteca
        if videos.is_empty() {
            // show_success_message("📂 No se encontraron videos en la biblioteca. Buscando en directorios del usuario...");
            
            // if let Some(home_dir) = dirs::home_dir() {
            //     let video_dirs = vec![
            //         home_dir.join("Videos"),
            //         home_dir.join("Vídeos"),
            //         home_dir.join("Desktop"),
            //         home_dir.join("Escritorio"),
            //     ];

            //     for dir in video_dirs {
            //         if dir.exists() {
            //             for entry in WalkDir::new(&dir)
            //                 .max_depth(1)
            //                 .into_iter()
            //                 .filter_map(|e| e.ok())
            //                 .take(10) // Limitar a 10 archivos por directorio
            //             {
            //                 let path = entry.path().to_path_buf();
            //                 if is_supported_video_format(&path) {
            //                     videos.push(path);
            //                 }
            //             }
            //         }
            //     }
            // }
        }

        videos
    }

    fn create_video_gallery(&self, parent: &Box, videos: Vec<PathBuf>) {
    // (Se eliminó el conteo de biblioteca porque no se usa actualmente)
        
    // Mostrar un subtítulo compacto con el conteo total de videos
        // Actualizar el contador del header (si existe)
        if let Some(count_label_ref) = self.header_count_label.borrow().as_ref() {
            let txt = format!("Videos: {}", videos.len());
            count_label_ref.set_text(&txt);
        }

    // No subtítulo adicional; el contador ya aparece en el header a la derecha

    // Nota: el botón de cambio de modo se añade más abajo junto a los controles inferiores

        // Crear área scrollable para la galería
        let scrolled = ScrolledWindow::new();
        scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scrolled.set_min_content_height(400);
        scrolled.set_max_content_height(600);
        scrolled.set_vexpand(true);
        scrolled.set_hexpand(true); // Expandir horizontalmente
        scrolled.set_halign(gtk4::Align::Fill);

        // Permitir arrastrar videos a la galería en cualquier momento
        {
            let drop_target = DropTarget::new(FileList::static_type(), DragAction::COPY);
            let video_path_clone_dt = self.video_path.clone();
            let self_clone_dt = self.clone();
            drop_target.connect_drop(move |_, value, _, _| {
                let mut any_added = false;
                if let Ok(file_list) = value.get::<FileList>() {
                    for file in file_list.files() {
                        if let Some(path) = file.path() {
                            if is_supported_video_format(&path) {
                                match copy_video_to_library(&path) {
                                    Ok(library_path) => {
                                        *video_path_clone_dt.borrow_mut() = Some(library_path.clone());
                                        show_success_message(&format!("🎉 Video añadido a la biblioteca: {}", 
                                            library_path.file_name().unwrap_or_default().to_string_lossy()));
                                        any_added = true;
                                            // Generar previews en background inmediatamente después de añadir el video
                                            {
                                                let lib_clone = library_path.clone();
                                                std::thread::spawn(move || {
                                                    let _ = generate_preview_frames(&lib_clone, 60);
                                                });
                                            }
                                    }
                                    Err(e) => {
                                        show_error_dialog(&format!("❌ Error al copiar video a la biblioteca: {}", e));
                                    }
                                }
                            } else {
                                show_error_dialog(&format!("❌ Formato no soportado: {}", path.display()));
                            }
                        }
                    }
                }
                if any_added {
                    self_clone_dt.update_gallery();
                }
                any_added
            });
            drop_target.connect_enter(|drop_target, _, _| {
                let widget = drop_target.widget();
                widget.add_css_class("drop-area-hover");
                DragAction::COPY
            });
            drop_target.connect_leave(|drop_target| {
                let widget = drop_target.widget();
                widget.remove_css_class("drop-area-hover");
            });
            scrolled.add_controller(drop_target);
        }

    // Almacenar referencia al contenedor de galería
    *self.gallery_container.borrow_mut() = Some(scrolled.clone());

    let is_list = is_list_mode();
    // El ScrolledWindow debe ocupar el espacio disponible para que los controles
    // inferiores se mantengan pegados al pie de la ventana.
    scrolled.set_vexpand(true);
        
            if is_list {
            // Modo Lista: usar ListBox para filas con altura fija y controladas
            let list_box = gtk4::ListBox::new();
            list_box.set_margin_start(8);
            list_box.set_margin_end(8);
            list_box.set_margin_top(8);
            list_box.set_margin_bottom(8);
            // El ListBox se encargará de mantener las filas con la altura que solicitemos
            list_box.set_vexpand(true);

            let video_path_clone = self.video_path.clone();
            for video_path in &videos {
                let card = self.create_video_card_list_mode(video_path, video_path_clone.clone());
                // Envolver el card en un ListBoxRow para asegurar una fila con altura fija
                let row = gtk4::ListBoxRow::new();
                row.set_child(Some(&card));
                // Forzar que la fila y el card no se expandan verticalmente
                row.set_vexpand(false);
                row.set_valign(gtk4::Align::Start);
                card.set_vexpand(false);
                card.set_valign(gtk4::Align::Start);
                list_box.append(&row);
            }

            scrolled.set_child(Some(&list_box));
        } else {
            // Modo Galería: usar FlowBox para organizar los videos (responsive real)
            let flowbox = gtk4::FlowBox::new();
            flowbox.set_valign(gtk4::Align::Start);
            flowbox.set_halign(gtk4::Align::Fill);
            flowbox.set_max_children_per_line(6); // Máximo de columnas
            flowbox.set_min_children_per_line(1); // Mínimo de columnas
            flowbox.set_selection_mode(gtk4::SelectionMode::None);
            flowbox.set_row_spacing(10);
            flowbox.set_column_spacing(10);
            flowbox.set_margin_start(8);
            flowbox.set_margin_end(8);
            flowbox.set_margin_top(8);
            flowbox.set_margin_bottom(8);
            flowbox.set_vexpand(true);
            flowbox.set_hexpand(true);

            let video_path_clone = self.video_path.clone();
            for video_path in &videos {
                let card = self.create_video_card_static(video_path, video_path_clone.clone());
                flowbox.insert(&card, -1);
            }
            
            scrolled.set_child(Some(&flowbox));
        }
        parent.append(&scrolled);

        // ---
        // Nota: create_video_card_static es igual a create_video_card pero sin &self
        // ---

    // Crear una zona inferior que agrupe los controles en una sola fila horizontal
    let bottom_box = Box::new(Orientation::Horizontal, 12);
    bottom_box.set_halign(gtk4::Align::Center);
    bottom_box.set_margin_top(15);
    bottom_box.set_margin_bottom(15);
    bottom_box.add_css_class("bottom-controls");

        // Botón Agregar - misma estructura que el botón Detener
        let add_button = Button::new();
        add_button.set_label("➕ Agregar más videos");
        add_button.add_css_class("action-button");
        add_button.add_css_class("add-btn");
        add_button.add_css_class("bottom-btn");
        add_button.set_sensitive(true);
        add_button.set_height_request(34);
        
        let video_path_clone2 = self.video_path.clone();
        let self_clone_for_add = self.clone();
        add_button.connect_clicked(move |_| {
            let inner_self = self_clone_for_add.clone();
            WallpaperAppWindow::open_file_dialog(video_path_clone2.clone(), Some(std::boxed::Box::new(move |_p| {
                inner_self.update_gallery();
            })));
        });

        // Botón Detener - misma estructura que el botón Agregar
        let stop_restore_button = Button::new();
        stop_restore_button.set_label("Detener");
    stop_restore_button.add_css_class("action-button");
    stop_restore_button.add_css_class("stop-btn");
        stop_restore_button.add_css_class("bottom-btn");
        stop_restore_button.set_sensitive(true);
        stop_restore_button.set_height_request(34);

        // Al hacer clic, sólo terminamos el wallpaper activo (si existe) — no restauramos nada
        stop_restore_button.connect_clicked(move |_| {
            let has_active = {
                let active = ACTIVE_WALLPAPER.lock().unwrap();
                active.process_id.is_some()
            };

            log::debug!("Stop clicked - has_active: {}", has_active);

            if !has_active {
                show_error_dialog("❌ No hay wallpaper activo para detener.");
                return;
            }

            // Terminar cualquier wallpaper activo
            terminate_active_wallpaper();

            show_success_message("✅ Wallpaper detenido exitosamente");
        });

        // Botón para cambiar modo de vista (añadir junto a los controles inferiores)
        let view_mode_button = Button::new();
        let is_list = is_list_mode();
        view_mode_button.set_label(if is_list { "🔲 Modo Galería" } else { "📋 Modo Lista" });
    view_mode_button.add_css_class("action-button");
    view_mode_button.add_css_class("view-mode-btn");
        view_mode_button.set_sensitive(true);
        view_mode_button.set_height_request(34);

        // Handler que alterna el modo y actualiza la etiqueta in-place
        let view_btn_clone = view_mode_button.clone();
        let self_clone_for_mode = self.clone();
        view_mode_button.connect_clicked(move |_| {
            toggle_view_mode();
            // Actualizar etiqueta del propio botón
            let new_mode = is_list_mode();
            view_btn_clone.set_label(if new_mode { "🔲 Modo Galería" } else { "📋 Modo Lista" });
            // Previews se detendrán por limpieza de cada card al destruirse durante la reconstrucción
            // Reconstruir la galería para aplicar el nuevo layout
            self_clone_for_mode.update_gallery();
            show_success_message(&format!("🔄 Cambiado a modo {}", if new_mode { "lista" } else { "galería" }));
        });

        // Añadir botones al bottom_box en orden horizontal
    bottom_box.append(&add_button);
    bottom_box.append(&view_mode_button);
    bottom_box.append(&stop_restore_button);

    // Botón Refrescar: fuerza la actualización manual de la galería
    let refresh_button = Button::with_label("🔁 Refrescar");
    refresh_button.add_css_class("action-button");
    refresh_button.add_css_class("refresh-btn");
    refresh_button.set_height_request(34);
    let self_clone_for_refresh = self.clone();
    refresh_button.connect_clicked(move |_| {
        self_clone_for_refresh.update_gallery();
        show_success_message("🔄 Refresco manual: buscando cambios en la biblioteca");
    });
    bottom_box.append(&refresh_button);

        parent.append(&bottom_box);
    }

    fn create_video_card_static(&self, video_path: &PathBuf, video_path_ref: std::rc::Rc<std::cell::RefCell<Option<PathBuf>>>) -> Frame {

        let frame = Frame::new(None);
        frame.add_css_class("video-card");
    // Mantener un ancho fijo para evitar que el card cambie de tamaño al hacer hover.
    // Usamos la constante CARD_WIDTH para controlar el breakpoint responsivo.
    frame.set_width_request(CARD_WIDTH);
    frame.set_height_request(260);

        // Usar Overlay como contenedor raíz para que el Picture sea el fondo real
        let overlay = Overlay::new();
        overlay.add_css_class("video-overlay");
        overlay.set_hexpand(true);
        overlay.set_vexpand(true);

        // Picture para mostrar poster/video (fondo)
        let picture = Picture::new();
        picture.add_css_class("video-background");
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.set_halign(gtk4::Align::Fill);
        picture.set_valign(gtk4::Align::Fill);
    // Asegurar que la imagen/paintable cubra toda la tarjeta (equivalente a CSS background-size: cover)
    picture.set_content_fit(gtk4::ContentFit::Cover);

        // Cargar poster inicial o thumbnail
        if let Some(poster_texture) = extract_video_poster(video_path) {
            picture.set_paintable(Some(&poster_texture));
        } else {
            // Fallback: usar thumbnail generado
            let thumbnail = create_video_thumbnail(video_path);
            if let Some(paintable) = thumbnail.paintable() {
                picture.set_paintable(Some(&paintable));
            }
        }

    overlay.set_child(Some(&picture));

        // Crear estado para manejo de video
        let card_state = Rc::new(RefCell::new(VideoCardState::new(video_path.clone())));

    // Registrar la card en el registro por-instancia para gestión cruzada de previews
    register_card_to_registry(&self.card_registry, &card_state);

        // Degradé semitransparente para legibilidad del texto superpuesto
        let gradient_box = Box::new(Orientation::Vertical, 0);
        gradient_box.add_css_class("video-gradient");
        gradient_box.set_halign(gtk4::Align::Fill);
        gradient_box.set_valign(gtk4::Align::End);
        gradient_box.set_height_request(40);

        overlay.add_overlay(&gradient_box);

        // Contenido (título, info, botones) como overlay encima del fondo
        let content_box = Box::new(Orientation::Vertical, 10);
        content_box.set_hexpand(true);
        content_box.set_halign(gtk4::Align::Fill);
        content_box.set_valign(gtk4::Align::Fill);
        content_box.set_margin_top(8);
        content_box.set_margin_bottom(8);
        content_box.set_margin_start(8);
        content_box.set_margin_end(8);

        // Título (se añadirá debajo del badge en la esquina superior)
        let filename = video_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("Video sin nombre");

        // Etiqueta: Optimizado o Original
    // Mantener espacio para el título - badges se muestran en las esquinas

        // Info: tamaño y resolución
        let (size_mb, height_label) = match fs::metadata(video_path) {
            Ok(metadata) => {
                let size_mb = metadata.len() / (1024 * 1024);
                let height_label = get_video_height_label(video_path).unwrap_or("?".to_string());
                (size_mb, height_label)
            },
            Err(_) => (0, "?".to_string()),
        };
        // Detectar extensión/format
        let format_label = video_path.extension().and_then(|e| e.to_str()).map(|s| s.to_uppercase()).unwrap_or("?".to_string());
        let info_text = if size_mb > 0 {
            format!("📏 Tamaño: {} MB  |  📐 {}p  |  🎬 {}", size_mb, height_label, format_label)
        } else {
            format!("ℹ️ Información no disponible")
        };
        let info_label = Label::new(Some(&info_text));
        info_label.add_css_class("video-info");
        info_label.set_justify(gtk4::Justification::Center);
    // No añadir info aquí en el content_box; la mostraremos en la esquina superior derecha
    // content_box.append(&info_label);

        // Ahora que hemos calculado `info_text` y `filename`, añadir badge e info en esquinas superiores
        // Contenedor superior izquierdo: badge encima del título
    let top_left_box = Box::new(Orientation::Vertical, 4);
    top_left_box.set_halign(gtk4::Align::Start);
    top_left_box.set_valign(gtk4::Align::Start);
    top_left_box.set_margin_start(8);
    top_left_box.set_margin_top(8);
    top_left_box.set_hexpand(false); // evitar que el contenedor superior se expanda
    // Badge: solo mostrar el estado (Optimizado/Original) - el info_text contiene tamaño/resolución
    let badge_label = create_badge_label(filename);
    top_left_box.append(&badge_label);
        // Título (debajo del badge)
        let title_label = Label::new(Some(filename));
    title_label.add_css_class("video-title");
    title_label.set_wrap(true);
    title_label.set_max_width_chars(30);
    // Alineación a la izquierda en modo galería
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_justify(gtk4::Justification::Left);
        top_left_box.append(&title_label);
        overlay.add_overlay(&top_left_box);

    let info_box_top = Box::new(Orientation::Horizontal, 0);
    info_box_top.set_halign(gtk4::Align::End);
    info_box_top.set_valign(gtk4::Align::Start);
    info_box_top.set_margin_end(8);
    info_box_top.set_margin_top(8);
    info_box_top.set_hexpand(false);
    let info_label_top = Label::new(Some(&info_text));
    info_label_top.add_css_class("card-info-top-right");
    info_label_top.set_halign(gtk4::Align::End);
    info_label_top.set_hexpand(false);
    info_box_top.append(&info_label_top);
    overlay.add_overlay(&info_box_top);

        // Spacer expandible para empujar los botones hacia la parte inferior interna de la tarjeta
        let spacer = Box::new(Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        content_box.append(&spacer);

        // Botones
        let button_box = Box::new(Orientation::Horizontal, 10);
        button_box.add_css_class("button-group");
        button_box.set_halign(gtk4::Align::Center);

        let preview_button = Button::with_label("👁️ Preview");
        preview_button.add_css_class("action-button");
        preview_button.add_css_class("preview-btn");
        let convert_button = Button::with_label("⚙️ Convertir");
        convert_button.add_css_class("action-button");
        convert_button.add_css_class("convert-btn");
    // Usar un icono/check más apropiado para 'Aplicar'
    let apply_button = Button::with_label("✔ Aplicar");
        apply_button.add_css_class("action-button");
        apply_button.add_css_class("apply-btn");

        // Eliminar
        let delete_button = if video_path.starts_with(&get_video_library_dir()) {
            let btn = Button::with_label("🗑️ Eliminar");
            btn.add_css_class("action-button");
            btn.add_css_class("delete-btn");
            let video_path_for_delete = video_path.clone();
            // Determine parent directory for the video (the per-video assets folder)
            let video_dir_for_delete = video_path_for_delete.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| get_video_library_dir());
            btn.connect_clicked(move |_| {
                if video_dir_for_delete.exists() && video_dir_for_delete != get_video_library_dir() {
                    // Nuevo sistema: Cada video tiene su propia carpeta exclusiva, así que es seguro borrarla completa.
                    // Esto borrará video + thumbnails + previews de ese video.
                    match std::fs::remove_dir_all(&video_dir_for_delete) {
                        Ok(_) => {
                            show_success_message(&format!("🗑️ Video eliminado (con assets): {}", video_dir_for_delete.display()));
                        }
                        Err(e) => {
                            show_error_dialog(&format!("❌ Error al eliminar carpeta del video: {}", e));
                        }
                    }
                } else {
                    // Fallback para videos legacy o externos
                    if let Err(e) = std::fs::remove_file(&video_path_for_delete) {
                        show_error_dialog(&format!("❌ Error al eliminar video: {}", e));
                    } else {
                        show_success_message(&format!("🗑️ Video eliminado: {}", 
                            video_path_for_delete.file_name().unwrap_or_default().to_string_lossy()));
                    }
                }
            });
            Some(btn)
        } else {
            None
        };

        let video_path_clone1 = video_path.clone();
        preview_button.connect_clicked(move |_| {
            if let Err(e) = preview_video(&video_path_clone1) {
                show_error_dialog(&format!("Error al mostrar vista previa: {}", e));
            }
        });
        let video_path_clone2 = video_path.clone();
        convert_button.connect_clicked(move |_| {
            show_conversion_dialog(&video_path_clone2);
        });
        let video_path_clone3 = video_path.clone();
        apply_button.connect_clicked(move |_| {
            show_monitor_selection_dialog(video_path_clone3.clone());
        });

        button_box.append(&preview_button);
        button_box.append(&convert_button);
        button_box.append(&apply_button);
        if let Some(delete_btn) = delete_button {
            button_box.append(&delete_btn);
        }

        button_box.set_valign(gtk4::Align::End);
        content_box.append(&button_box);

        // Agregar el content_box como overlay para que aparezca sobre el picture
        overlay.add_overlay(&content_box);

        // Configurar eventos de hover/focus para video preview
        let motion_controller = EventControllerMotion::new();
        let focus_controller = EventControllerFocus::new();
        
    let _overlay_hover = overlay.clone();
    let picture_hover = picture.clone();
    let card_state_hover = card_state.clone();
    let video_path_hover = video_path.clone();
    // Crear clones independientes del registro para cada closure (evita move-use-after-move)
    let registry_for_enter = self.card_registry.clone();
    let registry_for_focus = self.card_registry.clone();
        
        motion_controller.connect_enter(move |_, _, _| {
            log::debug!("🎯 Hover ENTER en card estática: {}", video_path_hover.file_name().unwrap_or_default().to_string_lossy());
            let mut state = card_state_hover.borrow_mut();
            // Antes de iniciar esta preview, detener otras previews activas para evitar múltiples reproducciones
            stop_other_previews_for_registry(&registry_for_enter, &card_state_hover);
            if !state.is_playing {
                // Si ya estamos generando, no iniciar otra tarea
                if state.preview_textures.is_none() && !state.preview_generating {
                    state.preview_generating = true;
                    let pic = picture_hover.clone();
                    let card_state_cb = card_state_hover.clone();
                    let vp = video_path_hover.clone();
                    generate_preview_frames_async(vp, 60, move |maybe_textures| {
                        // Callback en main thread
                        if let Some(textures) = maybe_textures {
                            if !textures.is_empty() {
                                let mut state_cb = card_state_cb.borrow_mut();
                                state_cb.preview_textures = Some(textures.clone());
                                state_cb.preview_generating = false;
                                // Si el usuario sigue con el cursor y no estamos reproduciendo, iniciar
                                if !state_cb.is_playing {
                                    if state_cb.preview_timeout.is_none() {
                                        let sid = start_preview_animation(&pic, textures, 50);
                                        state_cb.preview_timeout = sid;
                                    }
                                    state_cb.is_playing = true;
                                }
                            } else {
                                let mut state_cb = card_state_cb.borrow_mut();
                                state_cb.preview_generating = false;
                            }
                        } else {
                            let mut state_cb = card_state_cb.borrow_mut();
                            state_cb.preview_generating = false;
                            log::debug!("🔄 No se pudieron generar texturas de preview (async)");
                        }
                    });
                } else if let Some(textures) = state.preview_textures.clone() {
                    // Si ya tenemos texturas cargadas, arrancar animación inmediatamente
                    if state.preview_timeout.is_none() {
                        let sid = start_preview_animation(&picture_hover, textures, 41);
                        state.preview_timeout = sid;
                    }
                    state.is_playing = true;
                }
            }
        });

    let _overlay_leave = overlay.clone();
        let picture_leave = picture.clone();
        let card_state_leave = card_state.clone();
        let video_path_leave = video_path.clone();
        
        motion_controller.connect_leave(move |_| {
            log::debug!("🚪 Hover LEAVE en card estática");
            let mut state = card_state_leave.borrow_mut();
            if state.is_playing {
                log::debug!("🔄 Finalizando preview y restaurando poster");
                state.is_playing = false;
                if let Some(src) = state.preview_timeout.take() {
                    src.remove();
                }
                // Volver al poster
                if let Some(poster_texture) = extract_video_poster(&video_path_leave) {
                    picture_leave.set_paintable(Some(&poster_texture));
                } else {
                    let thumbnail = create_video_thumbnail(&video_path_leave);
                    if let Some(paintable) = thumbnail.paintable() {
                        picture_leave.set_paintable(Some(&paintable));
                    }
                }
            }
        });

        // Duplicar eventos para accesibilidad (focus)
        let picture_focus_in = picture.clone();
        let card_state_focus_in = card_state.clone();
        let video_path_focus_in = video_path.clone();
        let registry_for_focus_clone = registry_for_focus.clone();
        
        focus_controller.connect_enter(move |_| {
            let mut state = card_state_focus_in.borrow_mut();
            stop_other_previews_for_registry(&registry_for_focus_clone, &card_state_focus_in);
            if !state.is_playing {
                // Reuse same frame-based preview for focus
                if state.preview_textures.is_none() {
                    if let Some(paths) = generate_preview_frames(&video_path_focus_in, 60) {
                        let textures = load_textures_from_paths(&paths);
                        if !textures.is_empty() {
                            state.preview_textures = Some(textures);
                        }
                    }
                }

                if let Some(textures) = state.preview_textures.clone() {
                        if state.preview_timeout.is_none() {
                        let sid = start_preview_animation(&picture_focus_in, textures, 50);
                        state.preview_timeout = sid;
                    }
                    state.is_playing = true;
                }
            }
        });

        let picture_focus_out = picture.clone();
        let card_state_focus_out = card_state.clone();
        let video_path_focus_out = video_path.clone();
        
        focus_controller.connect_leave(move |_| {
            let mut state = card_state_focus_out.borrow_mut();
            if state.is_playing {
                // Stop animation
                if let Some(src) = state.preview_timeout.take() {
                    src.remove();
                }

                // Volver al poster
                if let Some(poster_texture) = extract_video_poster(&video_path_focus_out) {
                    picture_focus_out.set_paintable(Some(&poster_texture));
                } else {
                    let thumbnail = create_video_thumbnail(&video_path_focus_out);
                    if let Some(paintable) = thumbnail.paintable() {
                        picture_focus_out.set_paintable(Some(&paintable));
                    }
                }

                // Liberar texturas de preview
                state.preview_textures = None;
                state.is_playing = false;
            }
        });

        overlay.add_controller(motion_controller);
        overlay.add_controller(focus_controller);

        // Cleanup automático cuando se destruye la card
        let card_state_cleanup = card_state.clone();
        frame.connect_destroy(move |_| {
            card_state_cleanup.borrow_mut().cleanup();
        });

        // Selección
        let gesture = gtk4::GestureClick::new();
        let video_path_clone4 = video_path.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            *video_path_ref.borrow_mut() = Some(video_path_clone4.clone());
            show_success_message(&format!("✅ Video seleccionado: {}", video_path_clone4.file_name().unwrap_or_default().to_string_lossy()));
        });
        overlay.add_controller(gesture);

        frame.set_child(Some(&overlay));
        frame
    }

    fn create_video_card_list_mode(&self, video_path: &PathBuf, video_path_ref: std::rc::Rc<std::cell::RefCell<Option<PathBuf>>>) -> Frame {
        let frame = Frame::new(None);
        frame.add_css_class("video-card-list");
    // Altura fija y compacta para modo lista (mejor apariencia de "lista").
    // Ajustar este valor si quieres filas más altas/bajas.
    frame.set_height_request(32);
    // No permitir que la tarjeta se expanda verticalmente — así no llenará
    // todo el espacio si hay pocos elementos en la lista.
    frame.set_vexpand(false);
    frame.set_valign(gtk4::Align::Start);

        // Usar Overlay como contenedor raíz para que el Picture sea el fondo real
        let overlay = Overlay::new();
        overlay.add_css_class("video-overlay");
    overlay.set_hexpand(true);
    // Forzar altura fija del overlay para evitar que el contenido interno cambie
    // la altura de la tarjeta en modo lista.
    overlay.set_height_request(32);
    // Evitar que la overlay se expanda verticalmente en modo lista
    // para mantener la altura fija de la tarjeta.
    overlay.set_vexpand(false);

        // Picture para mostrar poster/video (fondo)
        let picture = Picture::new();
        picture.add_css_class("video-background");
    picture.set_hexpand(true);
    // No permitir que la imagen se expanda verticalmente y obligar
    // a que se ajuste dentro de la altura fija de la card.
    picture.set_vexpand(false);
    // Fijar la altura del paintable para que la imagen no estire la tarjeta
    // (la imagen seguirá comportándose como `cover`).
    picture.set_height_request(32);
        picture.set_halign(gtk4::Align::Fill);
        picture.set_valign(gtk4::Align::Fill);
    // Asegurar que la imagen/paintable cubra toda la tarjeta (equivalente a CSS background-size: cover)
    picture.set_content_fit(gtk4::ContentFit::Cover);
        
        // Cargar poster inicial o thumbnail
        if let Some(poster_texture) = extract_video_poster(video_path) {
            picture.set_paintable(Some(&poster_texture));
        } else {
            // Fallback: usar thumbnail generado
            let thumbnail = create_video_thumbnail(video_path);
            if let Some(paintable) = thumbnail.paintable() {
                picture.set_paintable(Some(&paintable));
            }
        }

        overlay.set_child(Some(&picture));

    // Crear estado para manejo de video
    let card_state = Rc::new(RefCell::new(VideoCardState::new(video_path.clone())));
    // Registrar card en el registro por-instancia
    register_card_to_registry(&self.card_registry, &card_state);

        // Contenido en fila encima del fondo (info + botones)
        let content_hbox = Box::new(Orientation::Horizontal, 8);
        content_hbox.set_hexpand(true);
        content_hbox.set_halign(gtk4::Align::Fill);
    // En modo lista colocamos el contenido (título + botones) en la parte inferior
    // Mantener el contenido al final del card pero sin forzar expansión vertical
    content_hbox.set_valign(gtk4::Align::End);
    content_hbox.set_vexpand(false);
    // Forzar altura del content_hbox para que no empuje la card
    content_hbox.set_height_request(32);
    // Márgenes reducidos para evitar que el contenido obligue a crecer la fila
    content_hbox.set_margin_start(6);
    content_hbox.set_margin_end(6);
    content_hbox.set_margin_top(2);
    content_hbox.set_margin_bottom(2);

        // Info box (centro)
        let info_box = Box::new(Orientation::Vertical, 4);
        info_box.set_hexpand(true);
        info_box.set_halign(gtk4::Align::Fill);
    info_box.set_valign(gtk4::Align::Center);
    info_box.set_vexpand(false);

        // Título (se mostrará debajo del badge en la esquina superior)
        let filename = video_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("Video sin nombre");

        // Información compacta en una línea
        let (size_mb, height_label) = match fs::metadata(video_path) {
            Ok(metadata) => {
                let size_mb = metadata.len() / (1024 * 1024);
                let height_label = get_video_height_label(video_path).unwrap_or("?".to_string());
                (size_mb, height_label)
            },
            Err(_) => (0, "?".to_string()),
        };
        
    // En modo lista, mostraremos el estado y la info en las esquinas; aquí mantenemos solo el título
        
    // Información compacta en una línea (reconstruir info_text aquí para usarlo en overlay)
        // Unificar la información de tamaño/resolución con el modo cuadricula
        // Detectar extensión/format
        let format_label = video_path.extension().and_then(|e| e.to_str()).map(|s| s.to_uppercase()).unwrap_or("?".to_string());
        let info_text = if size_mb > 0 {
            format!("📏 Tamaño: {} MB  |  📐 {}p  |  🎬 {}", size_mb, height_label, format_label)
        } else {
            format!("ℹ️ Información no disponible")
        };

        // Contenedor superior izquierdo: badge encima del título (lista)
    let top_left_box = Box::new(Orientation::Vertical, 4);
    top_left_box.set_halign(gtk4::Align::Start);
    top_left_box.set_valign(gtk4::Align::Start);
    top_left_box.set_margin_start(6);
    top_left_box.set_margin_top(6);
    top_left_box.set_hexpand(false);
        let badge_label = create_badge_label(filename);
    top_left_box.append(&badge_label);
    // NOTA: el título se mostrará abajo junto a los botones en modo lista
    let title_label = Label::new(Some(filename));
    title_label.add_css_class("video-title");
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        overlay.add_overlay(&top_left_box);

    let info_box_top = Box::new(Orientation::Horizontal, 0);
    info_box_top.set_halign(gtk4::Align::End);
    info_box_top.set_valign(gtk4::Align::Start);
    info_box_top.set_margin_end(6);
    info_box_top.set_margin_top(6);
    info_box_top.set_hexpand(false);
    let info_label_top = Label::new(Some(&info_text));
    info_label_top.add_css_class("card-info-top-right");
    info_label_top.set_halign(gtk4::Align::End);
    info_label_top.set_hexpand(false);
    info_box_top.append(&info_label_top);
    overlay.add_overlay(&info_box_top);

    // Añadir el título al info_box (que será mostrado en la parte inferior)
    info_box.append(&title_label);
    content_hbox.append(&info_box);

        // Botones de acción (derecha) - compactos
        let button_box = Box::new(Orientation::Horizontal, 6);
        button_box.set_halign(gtk4::Align::End);
        button_box.set_valign(gtk4::Align::Center);

        let preview_button = Button::with_label("👁️");
        preview_button.add_css_class("action-button");
        preview_button.add_css_class("preview-btn");
        preview_button.set_tooltip_text(Some("Vista previa"));
        
        let convert_button = Button::with_label("⚙️");
        convert_button.add_css_class("action-button");
        convert_button.add_css_class("convert-btn");
        convert_button.set_tooltip_text(Some("Convertir"));
        
    let apply_button = Button::with_label("✔");
        apply_button.add_css_class("action-button");
        apply_button.add_css_class("apply-btn");
        apply_button.set_tooltip_text(Some("Aplicar"));

        // Botón eliminar (solo para videos en biblioteca)
        let delete_button = if video_path.starts_with(&get_video_library_dir()) {
            let btn = Button::with_label("🗑️");
            btn.add_css_class("action-button");
            btn.add_css_class("delete-btn");
            btn.set_tooltip_text(Some("Eliminar"));
            let video_path_for_delete = video_path.clone();
            let video_dir_for_delete = video_path_for_delete.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| get_video_library_dir());
            btn.connect_clicked(move |_| {
                if video_dir_for_delete.exists() && video_dir_for_delete != get_video_library_dir() {
                     match std::fs::remove_dir_all(&video_dir_for_delete) {
                        Ok(_) => {
                            show_success_message(&format!("🗑️ Video eliminado (con assets): {}", video_dir_for_delete.display()));
                        }
                        Err(e) => {
                            show_error_dialog(&format!("❌ Error al eliminar carpeta del video: {}", e));
                        }
                    }
                } else {
                    if let Err(e) = std::fs::remove_file(&video_path_for_delete) {
                        show_error_dialog(&format!("❌ Error al eliminar video: {}", e));
                    } else {
                        show_success_message(&format!("🗑️ Video eliminado: {}", 
                            video_path_for_delete.file_name().unwrap_or_default().to_string_lossy()));
                    }
                }
            });
            Some(btn)
        } else {
            None
        };

        // Conectar eventos de botones
        let video_path_clone1 = video_path.clone();
        preview_button.connect_clicked(move |_| {
            if let Err(e) = preview_video(&video_path_clone1) {
                show_error_dialog(&format!("Error al mostrar vista previa: {}", e));
            }
        });
        
        let video_path_clone2 = video_path.clone();
        convert_button.connect_clicked(move |_| {
            show_conversion_dialog(&video_path_clone2);
        });
        
        let video_path_clone3 = video_path.clone();
        apply_button.connect_clicked(move |_| {
            show_monitor_selection_dialog(video_path_clone3.clone());
        });
        button_box.append(&preview_button);
        button_box.append(&convert_button);
        button_box.append(&apply_button);
        if let Some(delete_btn) = delete_button {
            button_box.append(&delete_btn);
        }

        content_hbox.append(&button_box);

        // Agregar contenido como overlay para que aparezca sobre el picture
        overlay.add_overlay(&content_hbox);

        // Configurar eventos de hover/focus para video preview
        let motion_controller = EventControllerMotion::new();
        let focus_controller = EventControllerFocus::new();
        
        let picture_hover = picture.clone();
        let card_state_hover = card_state.clone();
        let video_path_hover = video_path.clone();
        let registry_clone = self.card_registry.clone();
        
        motion_controller.connect_enter(move |_, _, _| {
            let mut state = card_state_hover.borrow_mut();
            // Detener otras previews al entrar (usar el registro por-instancia)
            stop_other_previews_for_registry(&registry_clone, &card_state_hover);
            if !state.is_playing {
                if state.preview_textures.is_none() && !state.preview_generating {
                    state.preview_generating = true;
                    let pic = picture_hover.clone();
                    let card_state_cb = card_state_hover.clone();
                    let vp = video_path_hover.clone();
                    generate_preview_frames_async(vp, 60, move |maybe_textures| {
                        if let Some(textures) = maybe_textures {
                            if !textures.is_empty() {
                                let mut state_cb = card_state_cb.borrow_mut();
                                state_cb.preview_textures = Some(textures.clone());
                                state_cb.preview_generating = false;
                                if state_cb.preview_timeout.is_none() {
                                        let sid = start_preview_animation(&pic, textures, 50);
                                    state_cb.preview_timeout = sid;
                                }
                                state_cb.is_playing = true;
                            } else {
                                let mut state_cb = card_state_cb.borrow_mut();
                                state_cb.preview_generating = false;
                            }
                        } else {
                            let mut state_cb = card_state_cb.borrow_mut();
                            state_cb.preview_generating = false;
                            log::debug!("🔄 No se pudieron generar texturas de preview (async)");
                        }
                    });
                } else if let Some(textures) = state.preview_textures.clone() {
                    if state.preview_timeout.is_none() {
                        let sid = start_preview_animation(&picture_hover, textures, 41);
                        state.preview_timeout = sid;
                    }
                    state.is_playing = true;
                }
            }
        });

        let picture_leave = picture.clone();
        let card_state_leave = card_state.clone();
        let video_path_leave = video_path.clone();
        
        motion_controller.connect_leave(move |_| {
            let mut state = card_state_leave.borrow_mut();
            if state.is_playing {
                if let Some(src) = state.preview_timeout.take() {
                    src.remove();
                }
                if let Some(poster_texture) = extract_video_poster(&video_path_leave) {
                    picture_leave.set_paintable(Some(&poster_texture));
                } else {
                    let thumbnail = create_video_thumbnail(&video_path_leave);
                    if let Some(paintable) = thumbnail.paintable() {
                        picture_leave.set_paintable(Some(&paintable));
                    }
                }
                state.preview_textures = None;
                state.is_playing = false;
            }
        });

        // Duplicar eventos para accesibilidad (focus)
        // NOTA: no iniciamos la preview automáticamente al recibir focus en modo lista,
        // porque el cambio de modo puede producir focus en widgets y provocar que varias
        // previews arranquen a la vez. Solo permitimos que el focus actúe como stop (leave).
        let _picture_focus_in = picture.clone();
        let _card_state_focus_in = card_state.clone();
        let _video_path_focus_in = video_path.clone();
        focus_controller.connect_enter(move |_| {
            // No iniciar preview por focus en modo lista - evitar arranques automáticos
        });

        let picture_focus_out = picture.clone();
        let card_state_focus_out = card_state.clone();
        let video_path_focus_out = video_path.clone();
        
        focus_controller.connect_leave(move |_| {
            let mut state = card_state_focus_out.borrow_mut();
            if state.is_playing {
                if let Some(src) = state.preview_timeout.take() {
                    src.remove();
                }
                if let Some(poster_texture) = extract_video_poster(&video_path_focus_out) {
                    picture_focus_out.set_paintable(Some(&poster_texture));
                } else {
                    let thumbnail = create_video_thumbnail(&video_path_focus_out);
                    if let Some(paintable) = thumbnail.paintable() {
                        picture_focus_out.set_paintable(Some(&paintable));
                    }
                }
                state.preview_textures = None;
                state.is_playing = false;
            }
        });

        overlay.add_controller(motion_controller);
        overlay.add_controller(focus_controller);

        // Cleanup automático cuando se destruye la card
        let card_state_cleanup = card_state.clone();
        frame.connect_destroy(move |_| {
            card_state_cleanup.borrow_mut().cleanup();
        });

        // Selección con clic
        let gesture = gtk4::GestureClick::new();
        let video_path_clone4 = video_path.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            *video_path_ref.borrow_mut() = Some(video_path_clone4.clone());
            show_success_message(&format!("✅ Video seleccionado: {}", video_path_clone4.file_name().unwrap_or_default().to_string_lossy()));
        });
        overlay.add_controller(gesture);

        frame.set_child(Some(&overlay));
        frame
    }

    #[allow(deprecated)]
    fn open_file_dialog(video_path: std::rc::Rc<std::cell::RefCell<Option<PathBuf>>>, on_added: Option<std::boxed::Box<dyn Fn(PathBuf) + 'static>>) {
        // Usar un FileChooserDialog para permitir selección múltiple
        let parent_window: Option<gtk4::Window> = None;
        let dialog = gtk4::FileChooserDialog::new(
            Some("Seleccionar Videos"),
            parent_window.as_ref(),
            gtk4::FileChooserAction::Open,
            &[("Cancelar", gtk4::ResponseType::Cancel), ("Abrir", gtk4::ResponseType::Accept)],
        );
        dialog.set_select_multiple(true);

        // Crear filtro para archivos de video
        let video_filter = FileFilter::new();
        video_filter.set_name(Some("Videos"));
        video_filter.add_pattern("*.mp4");
        video_filter.add_pattern("*.avi");
        video_filter.add_pattern("*.mkv");
        video_filter.add_pattern("*.webm");
        video_filter.add_pattern("*.mov");
        video_filter.add_pattern("*.wmv");
        video_filter.add_pattern("*.flv");
        video_filter.add_pattern("*.m4v");
        video_filter.add_pattern("*.MP4");
        video_filter.add_pattern("*.AVI");
        video_filter.add_pattern("*.MKV");
        video_filter.add_pattern("*.WEBM");
        video_filter.add_pattern("*.MOV");
        video_filter.add_pattern("*.WMV");
        video_filter.add_pattern("*.FLV");
        video_filter.add_pattern("*.M4V");

        // Crear lista de filtros
        let filters = gio::ListStore::new::<FileFilter>();
        filters.append(&video_filter);
        
        let all_filter = FileFilter::new();
        all_filter.set_name(Some("Todos los archivos"));
        all_filter.add_pattern("*");
        filters.append(&all_filter);

    // Establecer filtro por defecto (solo el filtro de videos)
    dialog.set_filter(&video_filter);

        // Establecer directorio inicial (Videos del usuario)
        if let Some(home_dir) = dirs::home_dir() {
            let videos_dir = home_dir.join("Videos");
                if videos_dir.exists() {
                let initial_folder = gio::File::for_path(&videos_dir);
                let _ = dialog.set_current_folder(Some(&initial_folder));
            } else {
                let videos_dir = home_dir.join("Vídeos");
                if videos_dir.exists() {
                    let initial_folder = gio::File::for_path(&videos_dir);
                    let _ = dialog.set_current_folder(Some(&initial_folder));
                }
            }
        }

        // Ejecutar el diálogo y procesar la respuesta
        dialog.connect_response(move |dlg, response| {
            if response == gtk4::ResponseType::Accept {
                // Obtener las selecciones (ListModel)
                let list = dlg.files();
                if let Some(model) = list.downcast_ref::<gio::ListStore>() {
                    let mut any_added = false;
                    for i in 0..model.n_items() {
                        if let Some(item) = model.item(i) {
                            if let Some(gfile) = item.downcast_ref::<gio::File>() {
                                if let Some(path) = gfile.path() {
                                    if is_supported_video_format(&path) {
                                        match copy_video_to_library(&path) {
                                            Ok(library_path) => {
                                                *video_path.borrow_mut() = Some(library_path.clone());
                                                show_success_message(&format!("🎉 Video añadido a la biblioteca: {}", 
                                                    library_path.file_name().unwrap_or_default().to_string_lossy()));
                                                any_added = true;
                                                // Generar previews en background inmediatamente después de añadir el video
                                                {
                                                    let lib_clone = library_path.clone();
                                                    std::thread::spawn(move || {
                                                        // Intentar generar 60 frames de preview; ignorar el resultado
                                                        let _ = generate_preview_frames(&lib_clone, 60);
                                                    });
                                                }
                                                if let Some(cb) = on_added.as_ref() {
                                                    (cb)(library_path.clone());
                                                }
                                            }
                                            Err(e) => {
                                                show_error_dialog(&format!("❌ Error al copiar video a la biblioteca: {}", e));
                                            }
                                        }
                                    } else {
                                        show_error_dialog("❌ Uno de los archivos seleccionados no es un formato de video soportado");
                                    }
                                }
                            }
                        }
                    }
                    if any_added {
                        // El callback on_added fue llamado por cada archivo; si hubiera uno global a llamar, se haría aquí
                    }
                }
            }
            dlg.close();
        });
        dialog.show();
    }
}

// Devuelve la altura del video como string ("480", "720", "1080", etc) usando ffprobe
fn get_video_height_label(video_path: &PathBuf) -> Option<String> {
    let path_str = video_path.to_string_lossy();
    let output = std::process::Command::new("ffprobe")
        .args(&["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=height", "-of", "csv=p=0", &path_str])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let res = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if res.is_empty() {
        None
    } else {
        Some(res)
    }
}

// Funciones auxiliares
fn create_video_thumbnail(video_path: &PathBuf) -> Image {
    // Si el archivo está en conversión, no intentar generar thumbnail ahora
    if let Ok(set) = CONVERTING_FILES.lock() {
        if set.contains(&video_path.to_string_lossy().to_string()) {
            // Devolver thumbnail por defecto temporariamente
            return create_default_thumbnail();
        }
    }

    // Guardar thumbnails dentro de la carpeta de assets por video cuando corresponda
    let assets_dir = get_video_assets_dir(video_path);
    let thumbnail_dir = assets_dir.join("thumbnails");
    if let Err(_) = std::fs::create_dir_all(&thumbnail_dir) {
        // Fallback a directorio global
        let global = get_thumbnail_dir();
        let _ = std::fs::create_dir_all(&global);
    }

    // Generar nombre único para el thumbnail basado en el hash del path
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    video_path.hash(&mut hasher);
    let hash = hasher.finish();
    let thumbnail_filename = format!("thumb_{:x}.png", hash);
    let thumbnail_path = assets_dir.join("thumbnails").join(&thumbnail_filename);

    // Si el thumbnail ya existe en assets del video, cargarlo
    if thumbnail_path.exists() {
        if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(&thumbnail_path) {
            let width = pixbuf.width();
            let height = pixbuf.height();
            let target_width = 320;
            let target_height = if width > 0 { (height * target_width) / width } else { 180 };
            if let Some(scaled) = pixbuf.scale_simple(target_width, target_height, gdk_pixbuf::InterpType::Bilinear) {
                return Image::from_pixbuf(Some(&scaled));
            } else {
                return Image::from_pixbuf(Some(&pixbuf));
            }
        }
    }

    // Fallback: buscar en el directorio global de thumbnails
    let global_thumb = get_thumbnail_dir().join(&thumbnail_filename);
    if global_thumb.exists() {
        if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(&global_thumb) {
            let width = pixbuf.width();
            let height = pixbuf.height();
            let target_width = 320;
            let target_height = if width > 0 { (height * target_width) / width } else { 180 };
            if let Some(scaled) = pixbuf.scale_simple(target_width, target_height, gdk_pixbuf::InterpType::Bilinear) {
                return Image::from_pixbuf(Some(&scaled));
            } else {
                return Image::from_pixbuf(Some(&pixbuf));
            }
        }
    }

    // Generar nuevo thumbnail with ffmpeg (ya lo escala)
    generate_thumbnail_with_ffmpeg(video_path, &thumbnail_path)
}

fn generate_thumbnail_with_ffmpeg(video_path: &PathBuf, output_path: &PathBuf) -> Image {
    // If the source no longer exists, return a default thumbnail immediately.
    if !video_path.exists() {
        log::debug!("🔍 generate_thumbnail_with_ffmpeg: video no existe: {}", video_path.display());
        return create_default_thumbnail();
    }

    let video_path_str = video_path.to_string_lossy();
    let output_path_str = output_path.to_string_lossy();
    
    show_success_message(&format!("🔄 Generando thumbnail para: {}", 
        video_path.file_name().unwrap_or_default().to_string_lossy()));
    
    // Comando ffmpeg optimizado para extraer frame a los 3 segundos del video
    let result = Command::new("ffmpeg")
        .args(&[
            "-loglevel", "error",            // Solo mostrar errores
            "-ss", "00:00:03",               // Seek ANTES del input (más rápido)
            "-i", &video_path_str,           // Input video
            "-vframes", "1",                 // Extract 1 frame
            "-vf", "scale=2048:-1:force_original_aspect_ratio=decrease:flags=lanczos", // Lanczos para mejor calidad a 2048px
            "-vcodec", "png",               // Salida PNG sin pérdida para máxima calidad
            "-y",                            // Overwrite output file
            &output_path_str                 // Output path
        ])
        .output();
    
    match result {
        Ok(output) => {
            if output.status.success() && output_path.exists() {
                // Thumbnail generado exitosamente, cargar imagen y escalarlo a 160px de ancho
                if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(output_path) {
                    let width = pixbuf.width();
                    let height = pixbuf.height();
                    // Cambiar el tamaño objetivo a 320px de ancho para miniaturas de mayor resolución
                    let target_width = 320;
                    let target_height = if width > 0 { (height * target_width) / width } else { 180 };
                    if let Some(scaled) = pixbuf.scale_simple(target_width, target_height, gdk_pixbuf::InterpType::Bilinear) {
                        show_success_message(&format!("✅ Thumbnail generado para: {}", 
                            video_path.file_name().unwrap_or_default().to_string_lossy()));
                        return Image::from_pixbuf(Some(&scaled));
                    } else {
                        // Si falla el escalado, usar el original
                        return Image::from_pixbuf(Some(&pixbuf));
                    }
                } else {
                    show_error_dialog("❌ No se pudo cargar el thumbnail generado");
                }
            } else {
                // ffmpeg falló, mostrar error en stderr si hay
                if !output.stderr.is_empty() {
                    let error_msg = String::from_utf8_lossy(&output.stderr);
                    if !error_msg.trim().is_empty() {
                        show_error_dialog(&format!("❌ Error ffmpeg: {}", error_msg.lines().next().unwrap_or("Error desconocido")));
                    }
                } else {
                    show_error_dialog("❌ ffmpeg falló sin mensaje de error");
                }
            }
        }
        Err(e) => {
            show_error_dialog(&format!("❌ No se pudo ejecutar ffmpeg: {}. ¿Está instalado?", e));
        }
    }
    
    // Si falla la generación, usar thumbnail por defecto
    create_default_thumbnail()
}

fn create_default_thumbnail() -> Image {
    let thumbnail = Image::from_icon_name("video-x-generic");
    thumbnail.set_pixel_size(160);
    thumbnail
}

fn show_conversion_dialog(video_path: &PathBuf) {
    let window = gtk4::Window::builder()
        .title("⚙️ Convertir Video")
        .modal(true)
        .width_request(500)
        .height_request(400)
        .resizable(false)
        .build();

    // Contenedor principal
    let content_box = Box::new(Orientation::Vertical, 20);
    content_box.set_margin_start(30);
    content_box.set_margin_end(30);
    content_box.set_margin_top(20);
    content_box.set_margin_bottom(20);

    // Título y descripción
    let title_label = Label::new(Some("Optimizar video para Live Wallpaper"));
    title_label.set_markup("<span size='large' weight='bold'>⚙️ Optimizar video para Live Wallpaper</span>");
    content_box.append(&title_label);

    let description_label = Label::new(Some("Selecciona la calidad que mejor se adapte a tu sistema:"));
    description_label.set_markup("<span color='#666'>Selecciona la calidad que mejor se adapte a tu sistema:</span>");
    description_label.set_margin_bottom(20);
    content_box.append(&description_label);

    // Información del video actual
    let filename = video_path.file_name().unwrap_or_default().to_string_lossy();
    let info_label = Label::new(Some(&format!("📁 Video: {}", filename)));
    info_label.set_markup(&format!("<span color='#333'>📁 Video: <b>{}</b></span>", filename));
    info_label.set_margin_bottom(10);
    content_box.append(&info_label);

    // Campo de FPS personalizado
    let fps_box = Box::new(Orientation::Horizontal, 10);
    fps_box.set_halign(gtk4::Align::Center);
    fps_box.set_margin_bottom(20);

    let fps_label = Label::new(Some("🎬 FPS deseados:"));
    fps_label.set_markup("<span color='#333'>🎬 FPS deseados:</span>");
    fps_box.append(&fps_label);

    let fps_entry = gtk4::Entry::new();
    fps_entry.set_text("30");
    fps_entry.set_width_request(80);
    fps_entry.set_max_length(3);
    fps_entry.set_placeholder_text(Some("24-60"));
    fps_box.append(&fps_entry);

    let fps_info_label = Label::new(Some("(24-60 recomendado)"));
    fps_info_label.set_markup("<span size='small' color='#888'>(24-60 recomendado)</span>");
    fps_box.append(&fps_info_label);

    content_box.append(&fps_box);

    // Opciones de conversión
    let options_grid = Grid::new();
    options_grid.set_column_spacing(15);
    options_grid.set_row_spacing(15);

    // Crear botones para cada opción
    let options = vec![
        ("📱 480p", "Rendimiento Máximo", "Ideal para sistemas de bajo rendimiento\n• Resolución: 854x480\n• Uso de CPU: Muy bajo"),
        ("💻 720p", "Equilibrio Perfecto", "Balance ideal rendimiento/calidad\n• Resolución: 1280x720\n• Uso de CPU: Moderado"),
        ("🖥️ 1080p", "Calidad Premium", "Máxima calidad visual\n• Resolución: 1920x1080\n• Uso de CPU: Alto"),
        ("🔥 Original", "Optimización Pura", "Mantiene resolución original\n• Solo optimiza compresión\n• Uso de CPU: Variable"),
    ];

    let video_path_clone = video_path.clone();
    for (index, (title, subtitle, description)) in options.iter().enumerate() {
        let option_frame = Frame::new(None);
        option_frame.set_width_request(200);
        option_frame.set_height_request(120);
        
        let option_box = Box::new(Orientation::Vertical, 8);
        option_box.set_margin_start(15);
        option_box.set_margin_end(15);
        option_box.set_margin_top(10);
        option_box.set_margin_bottom(10);

        let title_label = Label::new(Some(title));
        title_label.set_markup(&format!("<span size='large' weight='bold'>{}</span>", title));
        title_label.set_halign(gtk4::Align::Start);
        option_box.append(&title_label);

        let subtitle_label = Label::new(Some(subtitle));
        subtitle_label.set_markup(&format!("<span size='medium' color='#666'>{}</span>", subtitle));
        subtitle_label.set_halign(gtk4::Align::Start);
        option_box.append(&subtitle_label);

        let desc_label = Label::new(Some(description));
        desc_label.set_markup(&format!("<span size='small' color='#888'>{}</span>", description));
        desc_label.set_wrap(true);
        desc_label.set_max_width_chars(25);
        desc_label.set_halign(gtk4::Align::Start);
        desc_label.set_valign(gtk4::Align::Start);
        option_box.append(&desc_label);

        option_frame.set_child(Some(&option_box));

        // Hacer el frame clickeable
        let gesture = gtk4::GestureClick::new();
        let video_path_for_click = video_path_clone.clone();
        let window_for_click = window.clone();
        let quality_index = index;
        let fps_entry_clone = fps_entry.clone();
        
        gesture.connect_pressed(move |_, _, _, _| {
            // Obtener el valor FPS del campo de entrada
            let fps_text = fps_entry_clone.text().to_string();
            let fps = match fps_text.parse::<u32>() {
                Ok(f) if f >= 1 && f <= 120 => f,
                _ => {
                    show_error_dialog("❌ Por favor, introduce un valor FPS válido (1-120)");
                    return;
                }
            };
            
            window_for_click.close();
            convert_video(&video_path_for_click, quality_index, fps);
        });
        
        option_frame.add_controller(gesture);

        let row = index / 2;
        let col = index % 2;
        options_grid.attach(&option_frame, col as i32, row as i32, 1, 1);
    }

    content_box.append(&options_grid);

    // Botón de cancelar
    let cancel_button = Button::with_label("❌ Cancelar");
    cancel_button.set_halign(gtk4::Align::Center);
    cancel_button.set_margin_top(20);
    
    let window_for_cancel = window.clone();
    cancel_button.connect_clicked(move |_| {
        window_for_cancel.close();
    });
    
    content_box.append(&cancel_button);

    window.set_child(Some(&content_box));
    window.present();
}

fn convert_video(video_path: &PathBuf, quality_index: usize, fps: u32) {
    let quality_names = ["480p", "720p", "1080p", "original"];
    let quality_name = quality_names[quality_index];

    // Obtener nombre base y extensión
    let stem = video_path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = video_path.extension().unwrap_or_default().to_string_lossy();

    // Nuevo stem y nombre de archivo para el optimizado


    // Nuevo sistema: Crear una carpeta exclusiva para el video optimizado
    // Formato: Library / <Stem>_<Quality>_<FPS>fps_Opt_<Hash> / <filename>
    
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    video_path.hash(&mut hasher);
    let hash = hasher.finish();

    let output_stem = format!("{}_{}_{fps}fps_optimizado", stem, quality_name);
    let output_filename = format!("{}.{}", output_stem, ext);
    let folder_name = format!("{}_{:x}", output_stem, hash);

    let library_dir = get_video_library_dir();
    let unique_dir = library_dir.join(&folder_name);
    let _ = std::fs::create_dir_all(&unique_dir);
    
    let output_path = unique_dir.join(&output_filename);

    // Verificar si el archivo ya existe
    if output_path.exists() {
        show_success_message(&format!("⚠️ El archivo {} ya existe. Sobrescribiendo...", output_filename));
        start_conversion_process(video_path, &output_path, quality_index, fps);
    } else {
        start_conversion_process(video_path, &output_path, quality_index, fps);
    }
}

// Mensajes para comunicar progreso entre threads
#[derive(Debug, Clone)]
enum ProgressMessage {
    Started,
    Progress { percentage: i32 },
    Status(String),
    Completed(bool), // true = éxito, false = error
}
fn start_conversion_process(input_path: &PathBuf, output_path: &PathBuf, quality_index: usize, fps: u32) {
    // Crear ventana de progreso
    let progress_window = Window::builder()
        .title("⚙️ Convirtiendo Video")
        .modal(true)
        .width_request(500)
        .height_request(200)
        .resizable(false)
        .build();

    let progress_box = Box::new(Orientation::Vertical, 20);
    progress_box.set_margin_start(30);
    progress_box.set_margin_end(30);
    progress_box.set_margin_top(20);
    progress_box.set_margin_bottom(20);

    // Título
    let title_label = Label::new(Some("Conversión en progreso"));
    title_label.set_markup("<span size='large' weight='bold'>⚙️ Conversión en progreso</span>");
    progress_box.append(&title_label);

    // Información del archivo
    let filename = input_path.file_name().unwrap_or_default().to_string_lossy();
    let file_label = Label::new(Some(&format!("📁 {}", filename)));
    file_label.set_markup(&format!("<span color='#666'>📁 {}</span>", filename));
    progress_box.append(&file_label);

    // Barra de progreso
    let progress_bar = ProgressBar::new();
    progress_bar.set_show_text(true);
    progress_bar.set_text(Some("Iniciando conversión..."));
    progress_bar.set_margin_top(10);
    progress_bar.set_margin_bottom(10);
    progress_box.append(&progress_bar);

    // Label de estado
    let status_label = Label::new(Some("Preparando conversión..."));
    status_label.set_markup("<span size='small' color='#888'>Preparando conversión...</span>");
    progress_box.append(&status_label);

    // Botón cancelar (inicialmente deshabilitado hasta implementar cancelación)
    let cancel_button = Button::with_label("❌ Cancelar");
    cancel_button.set_halign(gtk4::Align::Center);
    cancel_button.set_sensitive(false); // Por ahora deshabilitado
    
    // En caso de que se implemente cancelación, asegurar que se reanude el monitoreo
    let progress_window_for_cancel = progress_window.clone();
    cancel_button.connect_clicked(move |_| {
        progress_window_for_cancel.close();
        resume_monitoring();
        show_success_message("🛑 Conversión cancelada - Monitoreo reanudado");
    });
    
    progress_box.append(&cancel_button);

    progress_window.set_child(Some(&progress_box));
    progress_window.present();
    
    // Crear canal de comunicación
    let (tx, rx) = mpsc::channel::<ProgressMessage>();
    
    // Ejecutar conversión en thread separado
    let input_path_clone = input_path.clone();
    let output_path_clone = output_path.clone();
    // Marcar archivo como en conversión
    if let Ok(mut set) = CONVERTING_FILES.lock() {
        set.insert(output_path_clone.to_string_lossy().to_string());
    }
    // Pausar el monitoreo para evitar regenerar miniaturas/actualizar galería hasta completar
    pause_monitoring();
    
    std::thread::spawn(move || {
        let _success = perform_video_conversion_with_progress(
            &input_path_clone, 
            &output_path_clone, 
            quality_index,
            fps,
            tx
        );
        
        // No necesitamos hacer nada más aquí, el receptor maneja todo
    });
    
    // Recibir mensajes de progreso en el hilo principal
    let progress_bar_clone = progress_bar.clone();
    let status_label_clone = status_label.clone();
    let progress_window_clone = progress_window.clone();
    let output_path_for_message = output_path.clone();
    // También clonar el input original para poder copiar su thumbnail al optimizado
    let input_path_for_message = input_path.clone();
    
    glib::timeout_add_local(Duration::from_millis(100), move || {
        match rx.try_recv() {
            Ok(ProgressMessage::Started) => {
                progress_bar_clone.set_fraction(0.0);
                progress_bar_clone.set_text(Some("0%"));
                status_label_clone.set_markup("<span size='small' color='#888'>Analizando video...</span>");
            }
            Ok(ProgressMessage::Progress { percentage }) => {
                let fraction = percentage as f64 / 100.0;
                progress_bar_clone.set_fraction(fraction);
                progress_bar_clone.set_text(Some(&format!("{}%", percentage)));
            }
            Ok(ProgressMessage::Status(status)) => {
                status_label_clone.set_markup(&format!("<span size='small' color='#888'>{}</span>", status));
            }
            Ok(ProgressMessage::Completed(success)) => {
                progress_window_clone.close();
                
                // Antes de reanudar el monitoreo, intentar reutilizar la miniatura del video original
                if success {
                    {
                        use std::collections::hash_map::DefaultHasher;
                        use std::hash::{Hash, Hasher};

                        // Nombre de thumbnail del original
                        let mut hasher = DefaultHasher::new();
                        input_path_for_message.hash(&mut hasher);
                        let orig_hash = hasher.finish();
                        let orig_thumb_filename = format!("thumb_{:x}.png", orig_hash);

                        // Buscar en assets del original
                        let orig_assets = get_video_assets_dir(&input_path_for_message);
                        let orig_thumb_path = orig_assets.join("thumbnails").join(&orig_thumb_filename);

                        // Fallback al directorio global de thumbnails
                        let global_thumb = get_thumbnail_dir().join(&orig_thumb_filename);

                        let source_thumb = if orig_thumb_path.exists() {
                            Some(orig_thumb_path)
                        } else if global_thumb.exists() {
                            Some(global_thumb)
                        } else {
                            None
                        };

                        if let Some(src) = source_thumb {
                            // Preparar destino para el thumbnail del archivo optimizado
                            let dest_assets = get_video_assets_dir(&output_path_for_message);
                            let dest_thumb_dir = dest_assets.join("thumbnails");
                            let _ = std::fs::create_dir_all(&dest_thumb_dir);

                            // Nombre de thumbnail para el archivo optimizado (hash del output)
                            let mut hasher2 = DefaultHasher::new();
                            output_path_for_message.hash(&mut hasher2);
                            let dest_hash = hasher2.finish();
                            let dest_thumb_filename = format!("thumb_{:x}.png", dest_hash);
                            let dest_thumb_path = dest_thumb_dir.join(&dest_thumb_filename);

                            if let Err(e) = std::fs::copy(&src, &dest_thumb_path) {
                                log::warn!("No se pudo copiar thumbnail del original al optimizado: {}", e);
                            } else {
                                log::info!("Thumbnail copiado al optimizado: {}", dest_thumb_path.display());
                            }
                        }
                    }

                        // Iniciar generación de previews en background para el archivo optimizado
                        {
                            let out_clone = output_path_for_message.clone();
                            // Generar 60 frames como hacemos al añadir videos
                            generate_preview_frames_async(out_clone, 60, move |_maybe_textures| {
                                // No necesitamos manejar las texturas aquí; la función se asegura de
                                // escribir los assets en disco para que el watcher los detecte.
                            });
                        }

                    // Desmarcar archivo de conversión y reanudar monitoreo
                    if let Ok(mut set) = CONVERTING_FILES.lock() {
                        set.remove(&output_path_for_message.to_string_lossy().to_string());
                    }
                    resume_monitoring();

                    show_success_message(&format!("✅ Video convertido exitosamente: {}", 
                        output_path_for_message.file_name().unwrap_or_default().to_string_lossy()));
                    show_success_message("▶️ Monitoreo reanudado - Galería actualizada");
                } else {
                    show_error_dialog("❌ Error durante la conversión del video");
                    show_success_message("▶️ Monitoreo reanudado");
                }
                
                return glib::ControlFlow::Break;
            }
            Err(mpsc::TryRecvError::Empty) => {
                // No hay mensajes nuevos, continuar
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                // Canal cerrado, terminar
                progress_window_clone.close();
                
                // Reanudar el monitoreo en caso de error de comunicación
                resume_monitoring();
                show_error_dialog("❌ Error de comunicación durante la conversión");
                show_success_message("▶️ Monitoreo reanudado");
                
                return glib::ControlFlow::Break;
            }
        }
        
        glib::ControlFlow::Continue
    });
}

fn perform_video_conversion_with_progress(
    input_path: &PathBuf, 
    output_path: &PathBuf, 
    quality_index: usize,
    fps: u32,
    tx: mpsc::Sender<ProgressMessage>
) -> bool {
    let input_str = input_path.to_string_lossy();
    let output_str = output_path.to_string_lossy();
    
    // Enviar mensaje de inicio
    let _ = tx.send(ProgressMessage::Started);
    
    // Primero obtener la duración del video para calcular progreso
    let _ = tx.send(ProgressMessage::Status("Analizando video...".to_string()));
    let duration = get_video_duration(input_path);
    
    // Convertir FPS a string para que tenga la vida suficiente
    let fps_str = fps.to_string();
    
    // Configuraciones optimizadas modernas para cada calidad
    let mut ffmpeg_args = vec![
        "-loglevel", "info",  // Cambiar a info para obtener progreso
        "-progress", "pipe:1", // Enviar progreso a stdout
        "-i", &input_str,
        "-an",  // Remover audio (no necesario para wallpapers)
        "-movflags", "+faststart",  // Optimizar para streaming/seeks rápidos
        "-g", "60",  // Keyframes cada 2 segundos para seeks rápidos
        "-pix_fmt", "yuv420p",  // Máxima compatibilidad
        "-tune", "film",  // Optimizado para contenido de video real
        "-x264opts", "ref=3:bframes=3:b-adapt=2:direct=auto:me=umh:subme=8:trellis=2:psy-rd=1.0,0.15",  // Configuración avanzada x264
    ];
    
    // Configuraciones específicas por calidad
    match quality_index {
        0 => {
            // 480p - Rendimiento Máximo con calidad optimizada
            ffmpeg_args.extend(&[
                "-vf", "scale=854:480:force_original_aspect_ratio=decrease:flags=lanczos,pad=854:480:(ow-iw)/2:(oh-ih)/2",
                "-crf", "22",  // Ligeramente mejor calidad
                "-preset", "medium",
                "-c:v", "libx264",
                "-profile:v", "main",  // Perfil más compatible
                "-level", "3.1",  // Level para 480p
                "-r", &fps_str,  // FPS personalizado
            ]);
        }
        1 => {
            // 720p - Equilibrio Perfecto con optimizaciones avanzadas
            ffmpeg_args.extend(&[
                "-vf", "scale=1280:720:force_original_aspect_ratio=decrease:flags=lanczos,pad=1280:720:(ow-iw)/2:(oh-ih)/2",
                "-crf", "20",  // Mejor calidad visual
                "-preset", "slow",  // Mejor compresión sin sacrificar velocidad excesivamente
                "-c:v", "libx264",
                "-profile:v", "high",  // Perfil alto para mejor eficiencia
                "-level", "3.1",  // Level apropiado para 720p
                "-r", &fps_str,  // FPS personalizado
            ]);
        }
        2 => {
            // 1080p - Calidad Premium con configuración profesional
            ffmpeg_args.extend(&[
                "-vf", "scale=1920:1080:force_original_aspect_ratio=decrease:flags=lanczos,pad=1920:1080:(ow-iw)/2:(oh-ih)/2",
                "-crf", "18",  // Calidad casi sin pérdidas
                "-preset", "slower",  // Máxima eficiencia de compresión
                "-c:v", "libx264",
                "-profile:v", "high",  // Perfil alto
                "-level", "4.0",  // Level apropiado para 1080p
                "-aq-mode", "2",  // Quantización adaptativa mejorada
                "-r", &fps_str,  // FPS personalizado
            ]);
        }
        3 => {
            // Mantener Original - Optimización máxima sin cambio de resolución
            ffmpeg_args.extend(&[
                "-crf", "16",  // Calidad visualmente sin pérdidas
                "-preset", "veryslow",  // Máxima eficiencia (toma más tiempo pero mejor resultado)
                "-c:v", "libx264",
                "-profile:v", "high",  // Perfil alto para máxima eficiencia
                "-aq-mode", "3",  // Quantización adaptativa máxima
                "-psy-rd", "1.0,0.15",  // Optimización psico-visual
                "-r", &fps_str,  // FPS personalizado también para original
            ]);
        }
        _ => {
            let _ = tx.send(ProgressMessage::Completed(false));
            return false;
        }
    }
    
    // Agregar archivo de salida y sobrescribir
    ffmpeg_args.extend(&["-y", &output_str]);
    
    // Actualizar estado
    let _ = tx.send(ProgressMessage::Status("Iniciando conversión con ffmpeg...".to_string()));
    
    // Ejecutar ffmpeg con captura de progreso
    match Command::new("ffmpeg")
        .args(&ffmpeg_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn() 
    {
        Ok(mut child) => {
            // Leer progreso desde stdout
            if let Some(stdout) = child.stdout.take() {
                let reader = BufReader::new(stdout);
                
                for line in reader.lines() {
                    if let Ok(line) = line {
                        if line.starts_with("out_time_ms=") {
                            // Extraer tiempo actual en microsegundos
                            if let Some(time_str) = line.strip_prefix("out_time_ms=") {
                                if let Ok(current_time_us) = time_str.parse::<f64>() {
                                    let current_time_s = current_time_us / 1_000_000.0;
                                    
                                    // Calcular progreso
                                    if let Some(total_duration) = duration {
                                        if total_duration > 0.0 {
                                            let progress = (current_time_s / total_duration).min(1.0);
                                            let percentage = (progress * 100.0) as i32;
                                            
                                            // Enviar progreso
                                            let _ = tx.send(ProgressMessage::Progress { 
                                                percentage 
                                            });
                                            
                                            let _ = tx.send(ProgressMessage::Status(format!(
                                                "Procesando: {:.1}s / {:.1}s", current_time_s, total_duration
                                            )));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            
            // Esperar a que termine el proceso
            match child.wait() {
                Ok(status) => {
                    if status.success() {
                        // Conversión exitosa - completar barra de progreso
                        let _ = tx.send(ProgressMessage::Progress { 
                            percentage: 100 
                        });
                        let _ = tx.send(ProgressMessage::Status("✅ Conversión completada".to_string()));
                        
                        // Esperar un momento para que el usuario vea el 100%
                        std::thread::sleep(Duration::from_millis(500));
                        let _ = tx.send(ProgressMessage::Completed(true));
                        true
                    } else {
                        // Error en la conversión
                        let _ = tx.send(ProgressMessage::Status("❌ Error en la conversión".to_string()));
                        let _ = tx.send(ProgressMessage::Completed(false));
                        false
                    }
                }
                Err(e) => {
                    let _ = tx.send(ProgressMessage::Status(format!("❌ Error: {}", e)));
                    let _ = tx.send(ProgressMessage::Completed(false));
                    false
                }
            }
        }
        Err(e) => {
            let _ = tx.send(ProgressMessage::Status(format!("❌ No se pudo ejecutar ffmpeg: {}", e)));
            let _ = tx.send(ProgressMessage::Completed(false));
            false
        }
    }
}

// Función auxiliar para obtener la duración del video
fn get_video_duration(video_path: &PathBuf) -> Option<f64> {
    let path_str = video_path.to_string_lossy();
    
    match Command::new("ffprobe")
        .args(&[
            "-v", "quiet",
            "-print_format", "csv=p=0",
            "-show_entries", "format=duration",
            &path_str
        ])
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                let duration_str = String::from_utf8_lossy(&output.stdout);
                duration_str.trim().parse::<f64>().ok()
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

fn preview_video(path: &PathBuf) -> Result<()> {
    show_success_message(&format!("🎥 Mostrando vista previa de: {}", 
        path.file_name().unwrap_or_default().to_string_lossy()));
    
    // Intentar abrir el video con diferentes reproductores disponibles
    let video_path = path.to_string_lossy().to_string();
    
    // Lista de reproductores de video comunes en Linux
    let players = vec![
        ("mpv", vec!["--geometry=800x600", &video_path]),
        ("vlc", vec![&video_path]),
        ("totem", vec![&video_path]),
        ("xdg-open", vec![&video_path]), // Reproductor por defecto del sistema
    ];
    
    for (player, args) in players {
        match Command::new(player).args(&args).spawn() {
            Ok(_) => {
                show_success_message(&format!("✅ Abriendo preview con {}", player));
                return Ok(());
            }
            Err(_) => {
                // Continuar con el siguiente reproductor si este no está disponible
                continue;
            }
        }
    }
    
    // Si ningún reproductor funciona, mostrar mensaje de error
    show_error_dialog("❌ No se encontró ningún reproductor de video instalado. Instala mpv, vlc o totem.");
    Ok(())
}

fn terminate_active_wallpaper() {
    let mut active_wallpaper = ACTIVE_WALLPAPER.lock().unwrap();

    if let Some(pid) = active_wallpaper.process_id {
        show_success_message(&format!("🔄 Terminando wallpaper anterior (PID: {})", pid));

        let my_pid = std::process::id();

        if pid == my_pid {
            // If this process is the wallpaper runner, request it to stop gracefully.
            request_stop_wallpaper();
        } else {
            // Try to terminate external process gracefully, then force-kill if needed.
            match Command::new("kill").arg("-TERM").arg(pid.to_string()).output() {
                Ok(output) => {
                    if output.status.success() {
                        show_success_message("✅ Wallpaper anterior terminado exitosamente");
                    } else {
                        // Fallback to KILL
                        match Command::new("kill").arg("-KILL").arg(pid.to_string()).output() {
                            Ok(_) => show_success_message("✅ Wallpaper anterior terminado forzosamente"),
                            Err(e) => show_error_dialog(&format!("❌ No se pudo terminar el proceso anterior: {}", e)),
                        }
                    }
                }
                Err(e) => {
                    show_error_dialog(&format!("❌ Error al intentar terminar proceso anterior: {}", e));
                }
            }

            // Wait a bit for external process to exit
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // Clear the tracked active wallpaper state regardless
    active_wallpaper.clear();
}

fn show_monitor_selection_dialog(video_path: PathBuf) {
    // Detectar monitores
    let desktop = match CinnamonDesktop::new() {
        Ok(d) => d,
        Err(_) => {
            // Si falla la detección, asumir monitor 0 por defecto
            apply_wallpaper(&video_path, 0);
            return;
        }
    };
    
    let monitors = match desktop.get_monitors() {
        Ok(m) => m,
        Err(_) => {
            apply_wallpaper(&video_path, 0);
            return;
        }
    };

    if monitors.len() <= 1 {
        // Solo un monitor, aplicar directo
        apply_wallpaper(&video_path, 0);
    } else {
        // Múltiples monitores: Aplicar a todos
        // Iteramos y aplicamos a cada uno.
        for (index, _) in monitors.iter().enumerate() {
            apply_wallpaper(&video_path, index);
            // Pequeña pausa para asegurar orden y evitar race conditions en inicio de procesos
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
}

fn apply_wallpaper(path: &PathBuf, monitor_index: usize) {
    // Verificar si ya está aplicado el mismo video
    {
        let active_wallpaper = ACTIVE_WALLPAPER.lock().unwrap();
        if active_wallpaper.is_same_video(path) {
            show_success_message(&format!("⚠️ El video {} ya está aplicado como wallpaper", 
                path.file_name().unwrap_or_default().to_string_lossy()));
            return;
        }
    }
    
    show_success_message(&format!("🌟 Aplicando wallpaper: {}", 
        path.file_name().unwrap_or_default().to_string_lossy()));
    
    // Terminar cualquier wallpaper activo anterior -- YA NO SE USA
    // Ahora soportamos multi-instancia via IPC, así que no matamos el daemon.
    // terminate_active_wallpaper();
    
    // Guardar la configuración actual del wallpaper antes de aplicar el nuevo
    {
        // Load config to respect preserve_native_wallpaper option
        let mut cfg = match crate::config::Config::load(None) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("No se pudo cargar config, asumiendo comportamiento por defecto: {}", e);
                crate::config::Config::default()
            }
        };

        // Save this video as the last applied wallpaper for autostart
        cfg.cinnamon.last_video_path = Some(path.clone());
        cfg.cinnamon.last_monitor_index = Some(monitor_index);
        
        // Save the updated configuration
        if let Err(e) = cfg.save_default() {
            log::warn!("No se pudo guardar la configuración actualizada: {}", e);
        } else {
            log::info!("✅ Configuración guardada: último video aplicado es {}", path.display());
        }

        if cfg.cinnamon.preserve_native_wallpaper {
            log::info!("preserve_native_wallpaper enabled: skipping save_current_wallpaper in GUI");
        } else {
            let settings = crate::gsettings_wallpaper::CinnamonWallpaperSettings::new();
            match settings.save_current_wallpaper() {
                Ok(()) => log::debug!("Original wallpaper settings saved by GUI before applying new wallpaper."),
                Err(e) => log::warn!("No se pudo guardar la configuración original del wallpaper: {}", e),
            }
        }
    }
    
    // Clonar los valores antes de moverlos al thread
    let video_path_str = path.to_string_lossy().to_string();
    let video_path_clone = path.clone();
    let exe_path = std::env::current_exe().expect("No se pudo obtener la ruta del ejecutable");
    
    show_success_message("🚀 Ejecutando comando de wallpaper...");
    
    // Before spawning the wallpaper process, shut down the listener in this GUI
    // process so the child can bind the instance socket and become primary.
    // YA NO ES NECESARIO: GUI usa socket independiente ("gui") del daemon.
    // let _ = crate::instance::shutdown_listener();

    // Ejecutar el comando exactamente como lo harías en terminal
    // ./target/debug/cinnamon-wallpaper /ruta/al/video.mp4
    std::thread::spawn(move || {
        match Command::new(&exe_path)
            .arg(&video_path_str)
            .arg("--monitor")
            .arg(monitor_index.to_string())
            .spawn()
        {
            Ok(mut child) => {
                // Registrar el nuevo proceso como activo
                let pid = child.id();
                {
                    let mut active_wallpaper = ACTIVE_WALLPAPER.lock().unwrap();
                    active_wallpaper.set_active(video_path_clone.clone(), pid);
                }
                show_success_message(&format!("✅ Proceso de wallpaper iniciado (PID: {})", pid));
                
                // Esperar a que termine el proceso
                match child.wait() {
                    Ok(status) => {
                        // Limpiar el estado cuando el proceso termine
                        {
                            let mut active_wallpaper = ACTIVE_WALLPAPER.lock().unwrap();
                            if active_wallpaper.is_same_video(&video_path_clone) {
                                active_wallpaper.clear();
                            }
                        }
                        
                        if status.success() {
                            show_success_message("✅ Wallpaper terminado exitosamente");
                        } else {
                            use std::os::unix::process::ExitStatusExt;
                            if let Some(signal) = status.signal() {
                                if signal == 15 || signal == 9 {
                                    // SIGTERM or SIGKILL -> Intentional stop
                                     show_success_message("✅ Wallpaper detenido exitosamente");
                                } else {
                                     show_error_dialog(&format!("❌ El proceso de wallpaper terminó con señal: {}", signal));
                                }
                            } else {
                                show_error_dialog("❌ El proceso de wallpaper terminó con error");
                            }
                        }
                    }
                    Err(e) => {
                        show_error_dialog(&format!("❌ Error esperando el proceso: {}", e));
                        // Limpiar el estado en caso de error
                        let mut active_wallpaper = ACTIVE_WALLPAPER.lock().unwrap();
                        active_wallpaper.clear();
                    }
                }
            }
            Err(e) => {
                show_error_dialog(&format!("❌ Error al ejecutar wallpaper: {}", e));
            }
        }
    });
}

fn show_success_message(message: &str) {
    println!("✅ {}", message);
}

fn show_error_dialog(message: &str) {
    log::error!("❌ Error: {}", message);
}

fn is_supported_video_format(path: &PathBuf) -> bool {
    if let Some(extension) = path.extension() {
        if let Some(ext_str) = extension.to_str() {
            let ext_lower = ext_str.to_lowercase();
            matches!(ext_lower.as_str(), "mp4" | "avi" | "mkv" | "webm" | "mov" | "wmv" | "flv" | "m4v")
        } else {
            false
        }
    } else {
        false
    }
}

pub fn launch_gui() -> Result<glib::ExitCode> {
    // Before initializing GTK, allow inheriting active wallpaper state from
    // the environment OR persistence
    inherit_active_from_env();
    
    // Attempt to restore state from PID file if environment didn't provide it
    {
        let mut active = ACTIVE_WALLPAPER.lock().unwrap();
        if active.process_id.is_none() {
            active.restore();
        }
    }

    // Inicializar GTK sin argumentos de línea de comandos
    gtk4::init().map_err(|_| anyhow::anyhow!("Failed to initialize GTK"))?;
    
    let app = WallpaperApp::new()?;
    Ok(app.run())
}

// Public helpers so non-GUI processes (the wallpaper runner) can mark themselves
// as the active wallpaper. This ensures the active state lives in the process
// that is actually running the wallpaper and survives the GUI process exiting.
pub fn mark_active_wallpaper(path: std::path::PathBuf, pid: u32) {
    let mut active = ACTIVE_WALLPAPER.lock().unwrap();
    active.set_active(path, pid);
}

// pub fn clear_active_wallpaper_state() {
//    if let Ok(mut guard) = ACTIVE_WALLPAPER.lock() {
//        guard.clear();
//    }
// }

// pub fn get_active_wallpaper_info() -> Option<(u32, String)> {
//    if let Ok(guard) = ACTIVE_WALLPAPER.lock() {
//        if let Some(pid) = guard.process_id {
//            return Some((pid, guard.file_path.clone().unwrap_or_default()));
//        }
//    }
//    None
// }

/// If the environment contains CW_ACTIVE_PID and CW_ACTIVE_PATH, register
/// that wallpaper as active locally. This is used by spawned GUI processes
/// to inherit the state from the wallpaper runner that spawned them.
pub fn inherit_active_from_env() {
    if let Ok(pid_str) = std::env::var("CW_ACTIVE_PID") {
        if let Ok(pid) = pid_str.parse::<u32>() {
            if let Ok(path_str) = std::env::var("CW_ACTIVE_PATH") {
                let path = std::path::PathBuf::from(path_str);
                mark_active_wallpaper(path, pid);
                log::debug!("Inherited active wallpaper from env: pid={} path={}", pid, std::env::var("CW_ACTIVE_PATH").unwrap_or_default());
            }
        }
    }
}

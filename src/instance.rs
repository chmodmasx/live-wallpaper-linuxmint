use anyhow::Result;
use once_cell::sync::OnceCell;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

type CmdHandler = Box<dyn Fn(String) + Send + 'static>;

// Store the optional handler and the listener so the application can
// shut down the listener when it wants to hand over primary role.
static HANDLER: OnceCell<Arc<Mutex<Option<CmdHandler>>>> = OnceCell::new();
static LISTENER_CELL: OnceCell<Arc<Mutex<Option<UnixListener>>>> = OnceCell::new();

fn socket_path(suffix: Option<&str>) -> PathBuf {
    let filename = if let Some(s) = suffix {
        format!("cinnamon-wallpaper-{}.sock", s)
    } else {
        "cinnamon-wallpaper.sock".to_string()
    };

    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join(filename)
    } else if let Some(home) = dirs::home_dir() {
        home.join(".local/share/cinnamon-wallpaper").join(filename)
    } else {
        PathBuf::from("/tmp").join(filename)
    }
}

/// Try to become the primary instance. If successful returns Ok(true).
/// If another instance exists, sends the message (args_msg) to it and returns Ok(false).
pub fn acquire_instance_and_maybe_send(args_msg: &str, suffix: Option<&str>) -> Result<bool> {
    let path = socket_path(suffix);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // If socket exists but nobody listens, try remove it
    if path.exists() {
        // Try connecting; if succeed then there's a server
        match UnixStream::connect(&path) {
            Ok(mut stream) => {
                let to_send = if args_msg.trim().is_empty() { "--gui" } else { args_msg };
                let _ = stream.write_all(to_send.as_bytes());
                // Close and exit as client
                return Ok(false);
            }
            Err(_) => {
                // Stale socket: remove and continue to bind
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    // Try to bind listener
    match UnixListener::bind(&path) {
        Ok(listener) => {
            // set permissions so only user can access
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));

            // Store the listener so other parts of the app can shut it down if needed
            let list_cell = LISTENER_CELL.get_or_init(|| Arc::new(Mutex::new(None)));
            {
                let mut guard = list_cell.lock().expect("Instance listener mutex poisoned");
                *guard = Some(listener.try_clone().expect("Failed to clone unix listener"));
            }

            // Prepare handler cell
            let handler_cell = HANDLER.get_or_init(|| Arc::new(Mutex::new(None)));
            let handler_clone = handler_cell.clone();

            // Use the cloned listener for the accept loop
            let accept_listener = list_cell.lock().expect("Instance listener mutex poisoned").as_ref().expect("Listener not set").try_clone().expect("Failed to clone listener for accept loop");

            thread::spawn(move || {
                for stream in accept_listener.incoming() {
                    match stream {
                        Ok(mut s) => {
                            let mut buf = String::new();
                            if s.read_to_string(&mut buf).is_ok() {
                                // Invoke handler if present
                                if let Some(h) = &*handler_clone.lock().expect("Instance handler mutex poisoned") {
                                    h(buf.clone());
                                } else {
                                    log::info!("[instance] received message but no handler registered: {}", buf);
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("[instance] listener error: {}", e);
                        }
                    }
                }
            });

            Ok(true)
        }
        Err(e) => {
            // Could not bind; try connect as client and send message
            match UnixStream::connect(&path) {
                Ok(mut stream) => {
                    let to_send = if args_msg.trim().is_empty() { "--gui" } else { args_msg };
                    let _ = stream.write_all(to_send.as_bytes());
                    Ok(false)
                }
                Err(_) => Err(anyhow::anyhow!("Failed to bind or connect to instance socket: {}", e)),
            }
        }
    }
}



/// Register a handler closure that will be called when another instance sends a message.
pub fn register_handler<F: Fn(String) + Send + 'static>(f: F) {
    let cell = HANDLER.get_or_init(|| Arc::new(Mutex::new(None)));
    let mut guard = cell.lock().expect("Instance handler mutex poisoned");
    *guard = Some(Box::new(f));
}

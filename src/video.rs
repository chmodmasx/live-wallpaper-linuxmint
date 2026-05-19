/*!
 * Video player implementation using GStreamer for Cinnamon wallpaper
 */

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::config::Config;

/// Pixel format for video frames
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PixelFormat {
    Rgb24,
    Bgr24,
    Rgba32,
    Bgra32,
}

/// Video frame data
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub format: PixelFormat,
    #[allow(dead_code)]
    pub pts_nanos: Option<u128>,
}

/// Video player state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayerState {
    Stopped,
    Playing,
    EndOfStream,
}

/// Video player for wallpaper playback
pub struct VideoPlayer {
    pipeline: Option<gst::Pipeline>,
    appsink: Option<gst_app::AppSink>,
    config: Config,
    state: Arc<Mutex<PlayerState>>,
    frame_receiver: Option<mpsc::Receiver<VideoFrame>>,
    current_video_path: Option<std::path::PathBuf>,
    screen_width: u32,
    screen_height: u32,
}

impl VideoPlayer {
    /// Create a new video player
    pub fn new(config: &Config, screen_width: u32, screen_height: u32) -> Result<Self> {
        // Initialize GStreamer
        gst::init().context("Failed to initialize GStreamer")?;

        log::info!("🎬 Initializing GStreamer video player");
        log::debug!("Hardware acceleration: {}", config.video.hardware_acceleration);
        log::debug!("Target FPS: {}", config.video.target_fps);
        log::debug!("Screen dimensions: {}x{}", screen_width, screen_height);

        Ok(Self {
            pipeline: None,
            appsink: None,
            config: config.clone(),
            state: Arc::new(Mutex::new(PlayerState::Stopped)),
            frame_receiver: None,
            current_video_path: None,
            screen_width,
            screen_height,
        })
    }

    /// Load a video file for playback
    pub fn load_video<P: AsRef<Path>>(&mut self, video_path: P) -> Result<()> {
        let video_path = video_path.as_ref();
        
        if !video_path.exists() {
            return Err(anyhow::anyhow!("Video file not found: {}", video_path.display()));
        }

        log::info!("📼 Loading video: {}", video_path.display());

        // Stop current playback if any
        self.stop()?;

        // Create GStreamer pipeline
        let pipeline = self.create_pipeline(video_path)?;
        
        // Create appsink for frame extraction
        let appsink = self.setup_appsink(&pipeline)?;

        // Setup frame extraction channel. Use a channel sized from configuration
        // so we can buffer more frames when desired (reduces drops under bursty IO).
        let chan_size = if self.config.video.max_buffer_frames == 0 {
            16usize
        } else {
            // allow a small multiplier to smooth bursts
            (self.config.video.max_buffer_frames as usize).saturating_mul(2).max(4)
        };
        let (frame_sender, frame_receiver) = mpsc::channel(chan_size);
        
        // Setup sample callback
        let frame_sender = Arc::new(Mutex::new(frame_sender));
        let state = Arc::clone(&self.state);
        
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let arrival = std::time::Instant::now();
                    if let Ok(sample) = appsink.pull_sample() {
                        // Log arrival (PTS if present)
                        if let Some(buffer) = sample.buffer() {
                            let pts = buffer.pts().map(|c| c.nseconds());
                            log::debug!("appsink new_sample: arrival_ns={} pts={:?}", arrival.elapsed().as_nanos(), pts);
                        } else {
                            log::debug!("appsink new_sample: arrival, no buffer");
                        }

                        if let Ok(frame) = Self::extract_frame_from_sample(sample) {
                            if let Ok(sender) = frame_sender.lock() {
                                // Non-blocking send to avoid pipeline stalls
                                if sender.try_send(frame).is_err() {
                                    log::debug!("Frame buffer full, dropping frame (diagnostic)");
                                } else {
                                    log::trace!("Frame forwarded to renderer");
                                }
                            }
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .eos(move |_| {
                    log::debug!("Video stream ended");
                    if let Ok(mut state) = state.lock() {
                        *state = PlayerState::EndOfStream;
                    }
                })
                .build(),
        );

        // Store components
        self.pipeline = Some(pipeline);
        self.appsink = Some(appsink);
        self.frame_receiver = Some(frame_receiver);
        self.current_video_path = Some(video_path.to_path_buf());

        // Update state
        {
            let mut state = self.state.lock().expect("PlayerState mutex poisoned");
            *state = PlayerState::Stopped;
        }

        log::info!("✅ Video loaded successfully");
        Ok(())
    }

    /// Create GStreamer pipeline based on configuration
    fn create_pipeline(&self, video_path: &Path) -> Result<gst::Pipeline> {
        // Convert to absolute path and create proper file URI
        let absolute_path = if video_path.is_absolute() {
            video_path.to_path_buf()
        } else {
            std::env::current_dir()?.join(video_path)
        };
        
        let uri = format!("file://{}", absolute_path.display());
        log::debug!("Video URI: {uri}");
        
        // Build pipeline string based on configuration
        let mut pipeline_str = String::new();
        
        // Source - explicitly handle H.264/AAC content
        // Quote the URI to handle paths with spaces
        pipeline_str.push_str(&format!("uridecodebin uri=\"{uri}\" name=decoder"));
        
        // Video processing chain
        pipeline_str.push_str(" ! videoconvert");
        
        // Video scaling and format conversion optimized for fullscreen
        // For lower resolution videos (720p/480p) to fullscreen without additional cost
        let scale_method = if self.config.video.scaling_quality >= 8 { 
            "lanczos" 
        } else if self.config.video.scaling_quality >= 5 { 
            "bilinear" 
        } else { 
            "nearest" // Fastest scaling for minimal CPU cost
        };
        
        // Add fullscreen scaling capability - FORCE to screen dimensions
        pipeline_str.push_str(&format!(
            " ! videoscale method={} add-borders=false ! video/x-raw,width={},height={} ! videoconvert",
            scale_method, self.screen_width, self.screen_height
        ));

        // Frame rate adjustment
        if self.config.video.target_fps > 0 {
            pipeline_str.push_str(&format!(
                " ! videorate ! video/x-raw,framerate={}/1",
                self.config.video.target_fps
            ));
        }

        // Output format - prefer BGRA for X11 compatibility
        pipeline_str.push_str(" ! video/x-raw,format=BGRA");
        
    // AppSink - max-buffers left to appsink property configuration
    pipeline_str.push_str(" ! appsink name=appsink drop=true");

        log::debug!("Pipeline: {pipeline_str}");

        let pipeline = gst::parse::launch(&pipeline_str)
            .context("Failed to create GStreamer pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("Failed to downcast to Pipeline"))?;

        Ok(pipeline)
    }

    /// Setup AppSink for frame extraction
    fn setup_appsink(&self, pipeline: &gst::Pipeline) -> Result<gst_app::AppSink> {
        let appsink = pipeline
            .by_name("appsink")
            .ok_or_else(|| anyhow::anyhow!("Failed to get appsink from pipeline"))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("Failed to downcast to AppSink"))?;

    // Configure appsink: enable sync so appsink follows pipeline clock
    // This paces frame delivery and avoids accelerated playback when reading frames
    appsink.set_property("emit-signals", false);
    appsink.set_property("sync", true); // Pace to pipeline clock to keep correct playback rate
    // Use configured max buffer frames (fall back to 3 if unset)
    let max_buf = if self.config.video.max_buffer_frames == 0 {
        3u32
    } else {
        self.config.video.max_buffer_frames as u32
    };
    appsink.set_property("max-buffers", max_buf);
    appsink.set_property("drop", true); // drop when buffer full to avoid growing latency

        Ok(appsink)
    }

    /// Extract video frame from GStreamer sample
    fn extract_frame_from_sample(sample: gst::Sample) -> Result<VideoFrame> {
        let buffer = sample.buffer().ok_or_else(|| anyhow::anyhow!("No buffer in sample"))?;
        let caps = sample.caps().ok_or_else(|| anyhow::anyhow!("No caps in sample"))?;

        // Extract video info from caps
        let video_info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|_| anyhow::anyhow!("Failed to get video info from caps"))?;

        // Map buffer for reading
        let map = buffer.map_readable()
            .map_err(|_| anyhow::anyhow!("Failed to map buffer"))?;

        // Copy frame data
        let data = map.as_slice().to_vec();

        // Extract PTS (presentation timestamp) if available
        let pts_nanos = match buffer.pts() {
            Some(clock) => Some(clock.nseconds() as u128),
            None => None,
        };

        // Determine pixel format
        let format = match video_info.format() {
            gst_video::VideoFormat::Bgra => PixelFormat::Bgra32,
            gst_video::VideoFormat::Rgba => PixelFormat::Rgba32,
            gst_video::VideoFormat::Rgb => PixelFormat::Rgb24,
            gst_video::VideoFormat::Bgr => PixelFormat::Bgr24,
            _ => {
                log::warn!("Unsupported video format: {:?}, assuming BGRA", video_info.format());
                PixelFormat::Bgra32
            }
        };

        Ok(VideoFrame {
            data,
            width: video_info.width(),
            height: video_info.height(),
            format,
            pts_nanos,
        })
    }

    /// Start video playbook
    pub fn play(&mut self) -> Result<()> {
        if let Some(ref pipeline) = self.pipeline {
            // Set pipeline to playing state with better error handling
            match pipeline.set_state(gst::State::Playing) {
                Ok(success) => {
                    match success {
                        gst::StateChangeSuccess::Success => {
                            log::debug!("Pipeline state changed to Playing successfully");
                        },
                        gst::StateChangeSuccess::Async => {
                            log::debug!("Pipeline state change is async, waiting...");
                            // Wait for the state change to complete
                            let (_state, _pending, _) = pipeline.state(gst::ClockTime::from_seconds(5));
                        },
                        gst::StateChangeSuccess::NoPreroll => {
                            log::debug!("Pipeline requires no preroll");
                        }
                    }
                },
                Err(error) => {
                    // Get more detailed error information
                    if let Some(bus) = pipeline.bus() {
                        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(1)) {
                            if let gst::MessageView::Error(err) = msg.view() {
                                return Err(anyhow::anyhow!("Pipeline error: {} - Debug: {:?}", 
                                    err.error(), err.debug()));
                            }
                        }
                    }
                    return Err(anyhow::anyhow!("Failed to set pipeline to playing state: {}", error));
                }
            }

            {
                let mut state = self.state.lock().expect("PlayerState mutex poisoned");
                *state = PlayerState::Playing;
            }

            log::info!("▶️  Video playback started");
        }
        Ok(())
    }

    /// Stop video playback
    pub fn stop(&mut self) -> Result<()> {
        if let Some(ref pipeline) = self.pipeline {
            pipeline.set_state(gst::State::Null)
                .map_err(|_| anyhow::anyhow!("Failed to set pipeline to null state"))?;

            {
                let mut state = self.state.lock().expect("PlayerState mutex poisoned");
                *state = PlayerState::Stopped;
            }

            log::info!("⏹️  Video playback stopped");
        }
        Ok(())
    }

    /// Get next video frame
    pub async fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
        // Start playback if not already playing
        {
            let state = self.state.lock().expect("PlayerState mutex poisoned");
            if *state == PlayerState::Stopped {
                drop(state);
                self.play()?;
            }
        }

        // Try to receive frame with timeout
        if let Some(ref mut receiver) = self.frame_receiver {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                receiver.recv()
            ).await {
                Ok(Some(frame)) => Ok(Some(frame)),
                Ok(None) => {
                    log::debug!("Frame receiver closed");
                    Ok(None)
                }
                Err(_) => {
                    // Timeout - check if we're at end of stream
                    let state = self.state.lock().expect("PlayerState mutex poisoned");
                    if *state == PlayerState::EndOfStream {
                        Ok(None)
                    } else {
                        // Continue waiting
                        Ok(None)
                    }
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Restart video playback (for looping)
    pub async fn restart(&mut self) -> Result<()> {
        if self.config.video.loop_playback {
            log::debug!("🔄 Restarting video for loop playback");
            
            if let Some(ref pipeline) = self.pipeline {
                // Seek to beginning
                let seek_event = gst::event::Seek::new(
                    1.0,
                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                    gst::SeekType::Set,
                    gst::ClockTime::ZERO,
                    gst::SeekType::None,
                    gst::ClockTime::NONE,
                );

                if pipeline.send_event(seek_event) {
                    {
                        let mut state = self.state.lock().expect("PlayerState mutex poisoned");
                        *state = PlayerState::Playing;
                    }
                    log::debug!("✅ Video restarted successfully");
                } else {
                    log::warn!("Failed to seek to beginning, stopping playback");
                    self.stop()?;
                }
            }
        } else {
            log::info!("Video ended and looping is disabled");
            self.stop()?;
        }

        Ok(())
    }

}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            log::error!("Error stopping video player during drop: {e}");
        }
        
        log::debug!("Video player resources cleaned up");
    }
}

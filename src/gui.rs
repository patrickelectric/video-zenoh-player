use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};
use gstreamer::prelude::*;

/// Tick interval for updating FPS / speed metrics (250 ms → 4 updates/s).
const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// Number of samples kept for charts (240 × 250 ms = 60 s of history).
const HISTORY_LEN: usize = 240;

/// The eframe application state.
pub struct VideoPlayerApp {
    pub rgba_rx: mpsc::Receiver<(Vec<u8>, u32, u32, usize)>,
    pub texture: Option<egui::TextureHandle>,
    pub video_width: u32,
    pub video_height: u32,
    pub frame_count: u64,
    pub last_frame_time: Instant,
    pub pipeline: Arc<Mutex<Option<gstreamer::Element>>>,
    pub topic: String,

    // Metrics sampling
    tick: Instant,
    frames_since_tick: u64,
    bytes_since_tick: u64,

    // FPS
    fps_current: f32,
    fps_history: VecDeque<f32>,

    // Speed (Mbps)
    speed_current: f32,
    speed_history: VecDeque<f32>,
}

impl VideoPlayerApp {
    pub fn new(
        rgba_rx: mpsc::Receiver<(Vec<u8>, u32, u32, usize)>,
        pipeline: Arc<Mutex<Option<gstreamer::Element>>>,
        topic: String,
    ) -> Self {
        Self {
            rgba_rx,
            texture: None,
            video_width: 0,
            video_height: 0,
            frame_count: 0,
            last_frame_time: Instant::now(),
            pipeline,
            topic,
            tick: Instant::now(),
            frames_since_tick: 0,
            bytes_since_tick: 0,
            fps_current: 0.0,
            fps_history: VecDeque::with_capacity(HISTORY_LEN),
            speed_current: 0.0,
            speed_history: VecDeque::with_capacity(HISTORY_LEN),
        }
    }
}

impl Drop for VideoPlayerApp {
    fn drop(&mut self) {
        if let Some(pipeline) = self.pipeline.lock().unwrap().take() {
            let _ = pipeline.set_state(gstreamer::State::Null);
        }
    }
}

impl eframe::App for VideoPlayerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain channel, keep only the latest frame
        let mut latest_frame: Option<(Vec<u8>, u32, u32)> = None;
        while let Ok((data, w, h, h264_size)) = self.rgba_rx.try_recv() {
            self.frame_count += 1;
            self.frames_since_tick += 1;
            self.bytes_since_tick += h264_size as u64;
            latest_frame = Some((data, w, h));
        }

        if let Some((data, w, h)) = latest_frame {
            self.video_width = w;
            self.video_height = h;
            self.last_frame_time = Instant::now();

            let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &data);

            match &mut self.texture {
                Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("video_frame", image, egui::TextureOptions::LINEAR));
                }
            }
        }

        // Update metrics every TICK_INTERVAL (250 ms)
        let elapsed = self.tick.elapsed();
        if elapsed >= TICK_INTERVAL {
            let secs = elapsed.as_secs_f32();

            self.fps_current = self.frames_since_tick as f32 / secs;
            self.speed_current = (self.bytes_since_tick as f32 * 8.0) / (secs * 1_000_000.0); // Mbps

            self.frames_since_tick = 0;
            self.bytes_since_tick = 0;
            self.tick = Instant::now();

            if self.fps_history.len() >= HISTORY_LEN {
                self.fps_history.pop_front();
            }
            self.fps_history.push_back(self.fps_current);

            if self.speed_history.len() >= HISTORY_LEN {
                self.speed_history.pop_front();
            }
            self.speed_history.push_back(self.speed_current);
        }

        // Repaint at ~60 fps
        ctx.request_repaint_after(Duration::from_millis(16));

        // --- Top panel with stats ---
        egui::TopBottomPanel::top("stats_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "Resolution: {}x{}",
                    self.video_width, self.video_height
                ));
                ui.separator();
                ui.label(format!("Frames: {}", self.frame_count));
                ui.separator();
                ui.label(format!("FPS: {:.1}", self.fps_current));
                ui.separator();
                ui.label(format!("Speed: {:.2} Mbps", self.speed_current));
                ui.separator();
                ui.label(format!(
                    "Last frame: {:.1}s ago",
                    self.last_frame_time.elapsed().as_secs_f64()
                ));
            });
        });

        // --- Bottom panel with FPS + speed charts ---
        egui::TopBottomPanel::bottom("charts_panel").show(ctx, |ui| {
            ui.columns(2, |cols| {
                // FPS chart
                cols[0].label("FPS");
                let max_fps = self
                    .fps_history
                    .iter()
                    .copied()
                    .fold(1.0_f32, f32::max)
                    .max(30.0);

                let fps_points: PlotPoints = self
                    .fps_history
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| [i as f64, v as f64])
                    .collect();

                Plot::new("fps_chart")
                    .height(80.0)
                    .include_y(0.0)
                    .include_y(max_fps as f64)
                    .include_x(0.0)
                    .include_x(HISTORY_LEN as f64)
                    .show_axes([false, true])
                    .allow_drag(false)
                    .allow_zoom(false)
                    .allow_scroll(false)
                    .allow_boxed_zoom(false)
                    .show(&mut cols[0], |plot_ui| {
                        plot_ui.line(Line::new("FPS", fps_points));
                    });

                // Speed chart
                cols[1].label("Mbps");
                let max_speed = self
                    .speed_history
                    .iter()
                    .copied()
                    .fold(0.1_f32, f32::max)
                    .max(1.0);

                let speed_points: PlotPoints = self
                    .speed_history
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| [i as f64, v as f64])
                    .collect();

                Plot::new("speed_chart")
                    .height(80.0)
                    .include_y(0.0)
                    .include_y(max_speed as f64)
                    .include_x(0.0)
                    .include_x(HISTORY_LEN as f64)
                    .show_axes([false, true])
                    .allow_drag(false)
                    .allow_zoom(false)
                    .allow_scroll(false)
                    .allow_boxed_zoom(false)
                    .show(&mut cols[1], |plot_ui| {
                        plot_ui.line(Line::new("Mbps", speed_points));
                    });
            });
        });

        // --- Central panel with video ---
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(texture) = &self.texture {
                let avail = ui.available_size();
                let aspect = self.video_width as f32 / self.video_height.max(1) as f32;
                let size = if avail.x / avail.y > aspect {
                    egui::vec2(avail.y * aspect, avail.y)
                } else {
                    egui::vec2(avail.x, avail.x / aspect)
                };

                ui.centered_and_justified(|ui| {
                    ui.image(egui::load::SizedTexture::new(texture.id(), size));
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(format!(
                        "Waiting for video on zenoh topic '{}'...",
                        self.topic
                    ));
                });
            }
        });
    }
}

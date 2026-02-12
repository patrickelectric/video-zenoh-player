use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};
use gstreamer::prelude::*;

/// Number of FPS samples to keep for the chart history.
const FPS_HISTORY_LEN: usize = 120;

/// The eframe application state.
pub struct VideoPlayerApp {
    pub rgba_rx: mpsc::Receiver<(Vec<u8>, u32, u32)>,
    pub texture: Option<egui::TextureHandle>,
    pub video_width: u32,
    pub video_height: u32,
    pub frame_count: u64,
    pub last_frame_time: Instant,
    pub pipeline: Arc<Mutex<Option<gstreamer::Element>>>,
    pub topic: String,

    // FPS tracking
    fps_last_tick: Instant,
    fps_frames_since_tick: u64,
    fps_current: f32,
    fps_history: VecDeque<f32>,
}

impl VideoPlayerApp {
    pub fn new(
        rgba_rx: mpsc::Receiver<(Vec<u8>, u32, u32)>,
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
            fps_last_tick: Instant::now(),
            fps_frames_since_tick: 0,
            fps_current: 0.0,
            fps_history: VecDeque::with_capacity(FPS_HISTORY_LEN),
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
        while let Ok(frame) = self.rgba_rx.try_recv() {
            self.frame_count += 1;
            self.fps_frames_since_tick += 1;
            latest_frame = Some(frame);
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

        // Update FPS once per second
        let elapsed = self.fps_last_tick.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.fps_current = self.fps_frames_since_tick as f32 / elapsed.as_secs_f32();
            self.fps_frames_since_tick = 0;
            self.fps_last_tick = Instant::now();

            if self.fps_history.len() >= FPS_HISTORY_LEN {
                self.fps_history.pop_front();
            }
            self.fps_history.push_back(self.fps_current);
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
                ui.label(format!(
                    "Last frame: {:.1}s ago",
                    self.last_frame_time.elapsed().as_secs_f64()
                ));
            });
        });

        // --- Bottom panel with FPS chart ---
        egui::TopBottomPanel::bottom("fps_panel").show(ctx, |ui| {
            ui.label("FPS over time");
            let max_fps = self
                .fps_history
                .iter()
                .copied()
                .fold(1.0_f32, f32::max)
                .max(30.0);

            let points: PlotPoints = self
                .fps_history
                .iter()
                .enumerate()
                .map(|(i, &fps)| [i as f64, fps as f64])
                .collect();

            let line = Line::new("FPS", points);

            Plot::new("fps_chart")
                .height(80.0)
                .include_y(0.0)
                .include_y(max_fps as f64)
                .include_x(0.0)
                .include_x(FPS_HISTORY_LEN as f64)
                .show_axes([false, true])
                .allow_drag(false)
                .allow_zoom(false)
                .allow_scroll(false)
                .allow_boxed_zoom(false)
                .show(ui, |plot_ui| {
                    plot_ui.line(line);
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

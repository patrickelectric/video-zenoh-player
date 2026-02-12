mod cdr;
mod cli;
mod decoder;
mod gui;
mod zenoh_sub;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use clap::Parser;
use eframe::egui;

fn main() -> eframe::Result {
    let args = cli::Args::parse();
    let topic_display = args.topic.clone();

    // Initialize GStreamer
    gstreamer::init().expect("Failed to initialize GStreamer");

    // --- Channels ---
    let (h264_tx, h264_rx) = mpsc::channel::<Vec<u8>>();
    let (rgba_tx, rgba_rx) = mpsc::sync_channel::<(Vec<u8>, u32, u32)>(2);

    // --- Zenoh subscriber (background thread) ---
    zenoh_sub::spawn(args.endpoint, args.topic, h264_tx);

    // --- GStreamer decode thread (auto-restarts on error) ---
    let pipeline_element: Arc<Mutex<Option<gstreamer::Element>>> = Arc::new(Mutex::new(None));
    let pipeline_for_app = pipeline_element.clone();

    std::thread::spawn(move || {
        decoder::run_loop(h264_rx, rgba_tx, pipeline_element);
    });

    // --- Run the eframe/egui application ---
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Zenoh Video Player",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(gui::VideoPlayerApp::new(
                rgba_rx,
                pipeline_for_app,
                topic_display,
            )))
        }),
    )
}

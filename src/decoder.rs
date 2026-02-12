use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer::prelude::*;

/// Build, run, and auto-restart the GStreamer decode pipeline.
///
/// Each iteration creates a fresh pipeline. On error it tears down and rebuilds.
/// Decoded RGBA frames are sent through `rgba_tx`.
/// The current pipeline reference is stored in `pipeline_holder` so the GUI can
/// shut it down cleanly on exit.
pub fn run_loop(
    h264_rx: mpsc::Receiver<Vec<u8>>,
    rgba_tx: mpsc::SyncSender<(Vec<u8>, u32, u32)>,
    pipeline_holder: Arc<Mutex<Option<gstreamer::Element>>>,
) {

    loop {
        println!("Starting GStreamer decode pipeline...");

        let rgba_tx = rgba_tx.clone();

        // Build the pipeline manually to avoid gst_base_src_loop issues.
        // Use avdec_h264 (software) as primary -- it's more error-tolerant than vtdec.
        // Fall back to vtdec if avdec_h264 is unavailable.
        let pipeline = gstreamer::Pipeline::new();

        let appsrc = gstreamer_app::AppSrc::builder()
            .name("src")
            .is_live(true)
            .format(gstreamer::Format::Time)
            .build();

        let caps = gstreamer::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .build();
        appsrc.set_caps(Some(&caps));

        let h264parse = gstreamer::ElementFactory::make("h264parse")
            .property_from_str("config-interval", "-1")
            .build()
            .expect("h264parse");

        // Try hardware decoders first, fall back to software.
        // Order: VA (modern) → VA-API (legacy) → NVIDIA → VideoToolbox (macOS) → software
        let hw_decoders = [
            ("vah264dec", "VA H.264 (Intel/AMD)"),
            ("vaapih264dec", "VA-API H.264 (Intel/AMD)"),
            ("nvh264dec", "NVIDIA NVDEC H.264"),
            ("vtdec", "VideoToolbox (macOS)"),
        ];
        let (decoder, decoder_name) = hw_decoders
            .iter()
            .find_map(|(name, label)| {
                gstreamer::ElementFactory::make(name)
                    .build()
                    .ok()
                    .map(|d| {
                        println!("  Using hardware decoder: {label} ({name})");
                        (d, *name)
                    })
            })
            .unwrap_or_else(|| {
                let d = gstreamer::ElementFactory::make("avdec_h264")
                    .build()
                    .expect("No H.264 decoder available (tried hw + avdec_h264)");
                println!("  Using software decoder: avdec_h264");
                (d, "avdec_h264")
            });

        let videoscale =
            gstreamer::ElementFactory::make("videoscale").build().expect("videoscale");
        let videoconvert =
            gstreamer::ElementFactory::make("videoconvert").build().expect("videoconvert");

        let capsfilter = gstreamer::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gstreamer::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", gstreamer::IntRange::new(1, 960))
                    .field("height", gstreamer::IntRange::new(1, 540))
                    .build(),
            )
            .build()
            .expect("capsfilter");

        let appsink = gstreamer_app::AppSink::builder()
            .name("sink")
            .max_buffers(1)
            .drop(true)
            .sync(false)
            .build();

        // Add and link all elements
        pipeline
            .add_many([
                appsrc.upcast_ref(),
                &h264parse,
                &decoder,
                &videoscale,
                &videoconvert,
                &capsfilter,
                appsink.upcast_ref(),
            ])
            .expect("Failed to add elements");

        gstreamer::Element::link_many([
            appsrc.upcast_ref(),
            &h264parse,
            &decoder,
            &videoscale,
            &videoconvert,
            &capsfilter,
            appsink.upcast_ref(),
        ])
        .expect("Failed to link elements");

        // Store pipeline reference for clean shutdown
        {
            let mut holder = pipeline_holder.lock().unwrap();
            *holder = Some(pipeline.clone().upcast());
        }

        // Forward decoded RGBA frames from appsink
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink
                        .pull_sample()
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gstreamer::FlowError::Error)?;
                    let info = gstreamer_video::VideoInfo::from_caps(caps)
                        .map_err(|_| gstreamer::FlowError::Error)?;
                    let buffer = sample.buffer().ok_or(gstreamer::FlowError::Error)?;
                    let map = buffer
                        .map_readable()
                        .map_err(|_| gstreamer::FlowError::Error)?;

                    let _ = rgba_tx.try_send((map.as_slice().to_vec(), info.width(), info.height()));
                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline
            .set_state(gstreamer::State::Playing)
            .expect("Failed to start pipeline");

        println!("  Pipeline playing (decoder: {decoder_name})");

        // Watch the bus for errors on a background thread.
        // Sets got_error flag so the main pump loop knows to restart.
        let got_error = Arc::new(Mutex::new(false));
        let got_error_bus = got_error.clone();
        let bus = pipeline.bus().expect("Pipeline has no bus");
        let bus_thread = std::thread::spawn(move || {
            for msg in bus.iter_timed(gstreamer::ClockTime::NONE) {
                use gstreamer::MessageView;
                match msg.view() {
                    MessageView::Error(err) => {
                        eprintln!(
                            "GStreamer ERROR from {:?}: {}",
                            err.src().map(|s| s.path_string()),
                            err.error(),
                        );
                        if let Some(debug) = err.debug() {
                            eprintln!("  debug: {debug}");
                        }
                        *got_error_bus.lock().unwrap() = true;
                        break;
                    }
                    MessageView::Warning(warn) => {
                        eprintln!("GStreamer WARNING: {}", warn.error());
                        if let Some(debug) = warn.debug() {
                            eprintln!("  debug: {debug}");
                        }
                    }
                    MessageView::Eos(..) => {
                        eprintln!("GStreamer: unexpected EOS, restarting...");
                        *got_error_bus.lock().unwrap() = true;
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Push H.264 data directly into appsrc with proper PTS.
        let frame_duration_ns: u64 = 33_333_333; // ~30fps
        let mut pts_ns: u64 = 0;
        let mut pump_count: u64 = 0;

        loop {
            // Check if the bus thread detected an error
            if *got_error.lock().unwrap() {
                break;
            }

            let data = match h264_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(d) => d,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };

            pump_count += 1;
            if pump_count <= 3 || pump_count % 500 == 0 {
                println!("  Pump #{pump_count}: {} bytes", data.len());
            }

            let mut buffer = gstreamer::Buffer::from_slice(data);
            {
                let buf_ref = buffer.get_mut().unwrap();
                buf_ref.set_pts(gstreamer::ClockTime::from_nseconds(pts_ns));
                buf_ref.set_duration(gstreamer::ClockTime::from_nseconds(frame_duration_ns));
            }
            pts_ns += frame_duration_ns;

            if appsrc.push_buffer(buffer).is_err() {
                eprintln!("  appsrc push failed at #{pump_count}");
                break;
            }
        }

        // Tear down
        let _ = pipeline.set_state(gstreamer::State::Null);
        let _ = appsrc.end_of_stream();
        let _ = bus_thread.join();

        // Drain stale H.264 data
        while h264_rx.try_recv().is_ok() {}

        if !*got_error.lock().unwrap() {
            break; // clean shutdown (channel disconnected)
        }

        println!("Restarting pipeline in 500ms...");
        std::thread::sleep(Duration::from_millis(500));
    }
}

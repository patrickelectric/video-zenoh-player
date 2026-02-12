use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use eframe::egui;
use gstreamer::prelude::*;

#[derive(Parser)]
#[command(name = "player", about = "Zenoh video stream player")]
struct Args {
    /// Zenoh endpoint address (e.g. tcp/192.168.31.113:7447)
    #[arg(short = 'e', long, default_value = "tcp/192.168.31.113:7447")]
    endpoint: String,

    /// Zenoh topic to subscribe to
    #[arg(short, long, default_value = "video/RadCam19216831100/stream")]
    topic: String,
}

fn main() -> eframe::Result {
    let args = Args::parse();
    let endpoint = args.endpoint;
    let topic = args.topic.clone();
    let topic_display = args.topic;

    // Initialize GStreamer
    gstreamer::init().expect("Failed to initialize GStreamer");

    // --- Channels ---
    // Compressed H.264 data: zenoh -> GStreamer decode thread
    let (h264_tx, h264_rx) = mpsc::channel::<Vec<u8>>();
    // Decoded RGBA frames: GStreamer -> egui renderer
    let (rgba_tx, rgba_rx) = mpsc::sync_channel::<(Vec<u8>, u32, u32)>(2);

    // --- Zenoh subscriber (background thread with tokio) ---
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        rt.block_on(async {
            let mut config = zenoh::Config::default();
            let endpoints_json = format!(r#"["{}"]"#, endpoint);
            config
                .insert_json5("connect/endpoints", &endpoints_json)
                .unwrap();
            let session = zenoh::open(config)
                .await
                .expect("Failed to open zenoh session");
            let subscriber = session
                .declare_subscriber(&topic)
                .await
                .expect("Failed to declare subscriber");
            println!("Zenoh subscriber active on '{topic}'");

            let mut count: u64 = 0;
            while let Ok(sample) = subscriber.recv_async().await {
                let payload = sample.payload().to_bytes();
                count += 1;

                if payload.is_empty() {
                    continue;
                }

                // Decode CDR-encoded foxglove CompressedVideo
                match decode_cdr_compressed_video(&payload) {
                    Some((data, format)) => {
                        if count % 100 == 1 {
                            println!(
                                "Message #{count}: CDR CompressedVideo format={format}, data={} bytes",
                                data.len()
                            );
                        }
                        if !data.is_empty() {
                            let _ = h264_tx.send(data);
                        }
                    }
                    None => {
                        if count % 100 == 1 {
                            println!(
                                "Message #{count}: CDR decode failed, raw {} bytes",
                                payload.len()
                            );
                        }
                        let _ = h264_tx.send(payload.to_vec());
                    }
                }
            }
        });
    });

    // --- GStreamer decode thread ---
    // Runs the decode pipeline in a loop: if it errors, restart it automatically.
    let pipeline_element: Arc<Mutex<Option<gstreamer::Element>>> = Arc::new(Mutex::new(None));
    let pipeline_for_app = pipeline_element.clone();

    std::thread::spawn(move || {
        gstreamer_decode_loop(h264_rx, rgba_tx, pipeline_element);
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
            Ok(Box::new(VideoPlayerApp {
                rgba_rx,
                texture: None,
                video_width: 0,
                video_height: 0,
                frame_count: 0,
                last_frame_time: Instant::now(),
                pipeline: pipeline_for_app,
                topic: topic_display,
            }))
        }),
    )
}

/// Build, run, and auto-restart the GStreamer decode pipeline.
/// Each iteration creates a fresh pipeline. On error it tears down and rebuilds.
fn gstreamer_decode_loop(
    h264_rx: mpsc::Receiver<Vec<u8>>,
    rgba_tx: mpsc::SyncSender<(Vec<u8>, u32, u32)>,
    pipeline_holder: Arc<Mutex<Option<gstreamer::Element>>>,
) {
    let h264_rx = Arc::new(Mutex::new(h264_rx));

    loop {
        println!("Starting GStreamer decode pipeline...");

        let rgba_tx = rgba_tx.clone();

        // Build the pipeline manually so we can avoid gst_base_src_loop entirely.
        // We create a pipeline, link elements by hand, and push data directly
        // through appsrc in push mode with proper timestamps.
        //
        // Use avdec_h264 (software) as primary -- it's more error-tolerant than vtdec.
        // Fall back to vtdec if avdec_h264 is unavailable.
        let pipeline = gstreamer::Pipeline::new();

        let appsrc = gstreamer_app::AppSrc::builder()
            .name("src")
            .is_live(true)
            .format(gstreamer::Format::Time)
            .build();
        // Set caps after building
        let caps =
            gstreamer::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .build();
        appsrc.set_caps(Some(&caps));

        let h264parse = gstreamer::ElementFactory::make("h264parse")
            .property_from_str("config-interval", "-1")
            .build()
            .expect("h264parse");

        // Try software decoder first (more tolerant), fall back to hardware
        let (decoder, decoder_name) =
            match gstreamer::ElementFactory::make("avdec_h264").build() {
                Ok(d) => {
                    println!("  Using software H.264 decoder (avdec_h264)");
                    (d, "avdec_h264")
                }
                Err(_) => {
                    let d = gstreamer::ElementFactory::make("vtdec")
                        .build()
                        .expect("Neither avdec_h264 nor vtdec available");
                    println!("  Using hardware H.264 decoder (vtdec)");
                    (d, "vtdec")
                }
            };

        let videoscale =
            gstreamer::ElementFactory::make("videoscale").build().expect("videoscale");
        let videoconvert =
            gstreamer::ElementFactory::make("videoconvert").build().expect("videoconvert");

        // Capsfilter to limit output resolution
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

        // Add all elements to the pipeline
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

        // Link: appsrc -> h264parse -> decoder -> videoscale -> videoconvert -> capsfilter -> appsink
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

        // Store pipeline reference so the app can shut it down on exit
        {
            let mut holder = pipeline_holder.lock().unwrap();
            *holder = Some(pipeline.clone().upcast());
        }

        // Appsink callback: forward decoded RGBA frames
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

        // Pump H.264 data into appsrc on a sub-thread.
        // Wait for a keyframe before pushing. Set proper PTS on each buffer.
        let h264_rx_ref = h264_rx.clone();
        let appsrc_clone = appsrc.clone();
        let pump = std::thread::spawn(move || {
            let mut waiting_for_keyframe = true;
            let mut pump_count: u64 = 0;
            // Assume ~30 fps; h264parse will adjust if the stream differs.
            let frame_duration_ns: u64 = 33_333_333; // ~30fps
            let mut pts_ns: u64 = 0;

            loop {
                let data = match h264_rx_ref.lock().unwrap().recv_timeout(Duration::from_secs(1)) {
                    Ok(d) => d,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                };

                if waiting_for_keyframe {
                    if h264_contains_keyframe(&data) {
                        println!("  Found keyframe ({} bytes), starting decode", data.len());
                        waiting_for_keyframe = false;
                    } else {
                        continue;
                    }
                }

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

                if appsrc_clone.push_buffer(buffer).is_err() {
                    eprintln!("  appsrc push failed at #{pump_count}, stopping pump");
                    return;
                }
            }
        });

        // Watch the bus for errors; when one happens, restart
        let bus = pipeline.bus().expect("Pipeline has no bus");
        let mut got_error = false;
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
                    got_error = true;
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
                    got_error = true;
                    break;
                }
                _ => {}
            }
        }

        // Tear down
        let _ = pipeline.set_state(gstreamer::State::Null);
        let _ = appsrc.end_of_stream();
        let _ = pump.join();

        // Drain any stale H.264 data so we start fresh from the next keyframe
        {
            let rx = h264_rx.lock().unwrap();
            while rx.try_recv().is_ok() {}
        }

        if !got_error {
            break; // clean shutdown
        }

        println!("Restarting pipeline in 500ms...");
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The eframe application state.
struct VideoPlayerApp {
    rgba_rx: mpsc::Receiver<(Vec<u8>, u32, u32)>,
    texture: Option<egui::TextureHandle>,
    video_width: u32,
    video_height: u32,
    frame_count: u64,
    last_frame_time: Instant,
    pipeline: Arc<Mutex<Option<gstreamer::Element>>>,
    topic: String,
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
                ui.label(format!(
                    "Last frame: {:.1}s ago",
                    self.last_frame_time.elapsed().as_secs_f64()
                ));
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
                    ui.label(format!("Waiting for video on zenoh topic '{}'...", self.topic));
                });
            }
        });
    }
}

/// Check whether H.264 data contains a keyframe (IDR slice or SPS).
/// Scans for NAL start codes (0x000001 or 0x00000001) and inspects the NAL type.
fn h264_contains_keyframe(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < data.len() {
        // 4-byte start code
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            if i + 4 < data.len() {
                let nal_type = data[i + 4] & 0x1F;
                // 5 = IDR slice, 7 = SPS
                if nal_type == 5 || nal_type == 7 {
                    return true;
                }
            }
            i += 4;
        // 3-byte start code
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if i + 3 < data.len() {
                let nal_type = data[i + 3] & 0x1F;
                if nal_type == 5 || nal_type == 7 {
                    return true;
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Decode a CDR-encoded foxglove CompressedVideo message.
///
/// CDR layout (little-endian):
///   [0..4]   CDR encapsulation header (byte 1: 0x01 = LE)
///   timestamp.sec   : u32
///   timestamp.nsec  : u32
///   frame_id        : CDR string (u32 len including null + bytes)
///   data            : CDR sequence<uint8> (u32 len + bytes)
///   format          : CDR string (u32 len including null + bytes)
///
/// Returns (video_data, format_string) on success.
fn decode_cdr_compressed_video(buf: &[u8]) -> Option<(Vec<u8>, String)> {
    if buf.len() < 4 {
        return None;
    }

    let le = buf[1] == 0x01;
    let mut pos: usize = 4; // skip CDR encapsulation header

    let read_u32 = |pos: &mut usize, buf: &[u8]| -> Option<u32> {
        // align to 4 bytes
        *pos = (*pos + 3) & !3;
        if *pos + 4 > buf.len() {
            return None;
        }
        let val = if le {
            u32::from_le_bytes(buf[*pos..*pos + 4].try_into().ok()?)
        } else {
            u32::from_be_bytes(buf[*pos..*pos + 4].try_into().ok()?)
        };
        *pos += 4;
        Some(val)
    };

    // timestamp.sec, timestamp.nsec
    let _sec = read_u32(&mut pos, buf)?;
    let _nsec = read_u32(&mut pos, buf)?;

    // frame_id (CDR string: u32 len including null terminator, then bytes)
    let frame_id_len = read_u32(&mut pos, buf)? as usize;
    if pos + frame_id_len > buf.len() {
        return None;
    }
    pos += frame_id_len;

    // data (CDR sequence<uint8>: u32 len, then bytes)
    let data_len = read_u32(&mut pos, buf)? as usize;
    if pos + data_len > buf.len() {
        return None;
    }
    let data = buf[pos..pos + data_len].to_vec();
    pos += data_len;

    // format (CDR string)
    let format_len = read_u32(&mut pos, buf)? as usize;
    if format_len == 0 || pos + format_len > buf.len() {
        return None;
    }
    let format_str =
        String::from_utf8_lossy(&buf[pos..pos + format_len - 1]).to_string();

    Some((data, format_str))
}

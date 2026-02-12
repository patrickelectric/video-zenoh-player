use std::sync::mpsc;

use crate::cdr;

/// Spawn a background thread that subscribes to a Zenoh topic and forwards
/// decoded H.264 data through the provided channel.
pub fn spawn(endpoint: String, topic: String, h264_tx: mpsc::Sender<Vec<u8>>) {
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
                match cdr::decode_compressed_video(&payload) {
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
}

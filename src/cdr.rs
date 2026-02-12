use serde::Deserialize;

/// A timestamp, represented as an offset from a user-defined epoch.
/// <https://docs.foxglove.dev/docs/visualization/message-schemas/built-in-types#time>
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Timestamp {
    /// Seconds since epoch.
    pub sec: u32,
    /// Additional nanoseconds since epoch.
    pub nsec: u32,
}

/// A single frame of a compressed video bitstream.
/// <https://docs.foxglove.dev/docs/visualization/message-schemas/compressed-video>
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CompressedVideo {
    /// Timestamp of video frame.
    pub timestamp: Timestamp,
    /// Frame of reference for the video.
    pub frame_id: String,
    /// Compressed video frame data (Annex B formatted for h264/h265).
    pub data: Vec<u8>,
    /// Video format (e.g. "h264", "h265", "vp9", "av1").
    pub format: String,
}

/// Decode a CDR-encoded foxglove CompressedVideo message.
///
/// The buffer must include the 4-byte CDR encapsulation header produced by
/// `cdr::serialize` — `cdr::deserialize` consumes it automatically.
///
/// Returns (video_data, format_string) on success.
pub fn decode_compressed_video(buf: &[u8]) -> Result<(Vec<u8>, String), String> {
    if buf.len() < 4 {
        return Err(format!("payload too short ({} bytes, need ≥4)", buf.len()));
    }

    let msg: CompressedVideo =
        cdr::deserialize(buf).map_err(|e| format!("CDR deserialize: {e}"))?;

    Ok((msg.data, msg.format))
}

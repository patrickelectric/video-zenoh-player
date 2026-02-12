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
/// The first 4 bytes are the CDR encapsulation header:
///   byte 0: reserved (0x00)
///   byte 1: endianness (0x00 = big-endian, 0x01 = little-endian)
///   bytes 2-3: options
///
/// Returns (video_data, format_string) on success.
pub fn decode_compressed_video(buf: &[u8]) -> Option<(Vec<u8>, String)> {
    if buf.len() < 4 {
        return None;
    }

    // Skip the 4-byte CDR encapsulation header
    let payload = &buf[4..];

    let msg: CompressedVideo = cdr::deserialize(payload).ok()?;

    Some((msg.data, msg.format))
}

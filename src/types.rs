use cpal::SampleFormat;

/// Frame header magic (2 bytes) identifying RemoteMic packets.
pub const FRAME_MAGIC: [u8;2] = *b"RM";

/// Sample format numeric codes for wire protocol.
pub const FMT_F32: u8 = 1;
pub const FMT_I16: u8 = 2;
pub const FMT_U16: u8 = 3;

/// Convert CPAL sample format to protocol code.
pub fn sample_format_code(fmt: SampleFormat) -> u8 {
    match fmt {
        SampleFormat::F32 => FMT_F32,
        SampleFormat::I16 => FMT_I16,
        SampleFormat::U16 => FMT_U16,
        _ => FMT_F32,
    }
}

/// Convert protocol code back to CPAL sample format (fallback F32).
pub fn code_to_sample_format(code: u8) -> SampleFormat {
    match code {
        FMT_F32 => SampleFormat::F32,
        FMT_I16 => SampleFormat::I16,
        FMT_U16 => SampleFormat::U16,
        _ => SampleFormat::F32,
    }
}

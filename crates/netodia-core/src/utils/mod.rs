pub mod atomic_file;
pub mod audio_ext;
pub mod event_bridge;
pub mod exe;
pub mod fold;
pub mod missing_artwork;
pub mod play_counts;
pub mod redact;
pub mod self_writes;
pub mod text;
pub mod toast;

use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::tz::Offset;

pub fn now_rfc3339() -> String {
    format_rfc3339(Timestamp::now(), Offset::UTC)
}

/// Preserve the numeric offset and the 0/3/6/9 fractional digits used in stored timestamps.
pub fn format_rfc3339(timestamp: Timestamp, offset: Offset) -> String {
    let nanos = timestamp.subsec_nanosecond().unsigned_abs();
    let precision = match nanos {
        0 => 0,
        n if n.is_multiple_of(1_000_000) => 3,
        n if n.is_multiple_of(1_000) => 6,
        _ => 9,
    };
    format!("{:.precision$}", timestamp.display_with_offset(offset))
}

/// Cross-platform `canonicalize` that avoids the Windows verbatim
/// (`\\?\C:\...`) form. On Windows we route through `dunce::canonicalize`
/// — it strips the `\\?\` prefix whenever the resolved path fits in
/// `MAX_PATH`, so the returned `PathBuf` round-trips through string
/// storage (`folders.path`, `tracks.file_path`) and equality-matches
/// what `std::fs::read_dir` yields. On non-Windows it forwards to
/// `std::fs::canonicalize`. Use this everywhere a canonicalized path
/// is persisted, displayed to the user, or used as a DB key.
#[cfg(windows)]
pub fn canonicalize_path<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

#[cfg(not(windows))]
pub fn canonicalize_path<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;

    #[test]
    fn rfc3339_preserves_stored_timestamp_precision() {
        for (nanos, expected) in [
            (0, "1970-01-01T00:00:00+00:00"),
            (120_000_000, "1970-01-01T00:00:00.120+00:00"),
            (120_340_000, "1970-01-01T00:00:00.120340+00:00"),
            (120_340_560, "1970-01-01T00:00:00.120340560+00:00"),
        ] {
            let stamp = Timestamp::UNIX_EPOCH + SignedDuration::from_nanos(nanos);
            assert_eq!(format_rfc3339(stamp, Offset::UTC), expected);
        }
    }
}

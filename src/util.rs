//! Small helpers with no better home.

use std::path::PathBuf;

/// Decode `%XX` escapes. MPRIS `artUrl` and `xesam:url` are URLs, so a track in
/// `/mnt/music/D12/Devil's Night/` arrives percent-encoded and will not open as
/// a path until it is decoded.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// Convert a `file://` URL to a path. Returns `None` for any other scheme.
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // `file:///path` (empty authority) and `file:/path` both appear in the wild.
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    Some(PathBuf::from(percent_decode(path)))
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("Devil%27s%20Night"), "Devil's Night");
        assert_eq!(percent_decode("plain"), "plain");
        // A stray % must not panic or eat the rest of the string.
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn parses_file_urls() {
        assert_eq!(
            file_url_to_path("file:///mnt/music/D12/Devil%27s%20Night/01.flac"),
            Some(PathBuf::from("/mnt/music/D12/Devil's Night/01.flac"))
        );
        assert_eq!(file_url_to_path("https://example.com/a.jpg"), None);
    }
}

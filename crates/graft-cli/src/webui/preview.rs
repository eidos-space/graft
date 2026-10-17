use super::*;
use std::io::{Seek, SeekFrom};

pub(super) fn mime(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" | "apng" => "image/png",
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "weba" => "audio/webm",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "html" | "htm" => "text/html; charset=utf-8",
        _ => return None,
    })
}

// Hex-encoded UTF-8 keeps resource paths unambiguous, including #, ?, and /.
fn decode_path(encoded: &str) -> Result<String> {
    if encoded.len() > 32768 || !encoded.len().is_multiple_of(2) {
        bail!("invalid preview path");
    }
    let bytes = encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let value = std::str::from_utf8(pair).context("invalid preview path")?;
            u8::from_str_radix(value, 16).context("invalid preview path")
        })
        .collect::<Result<Vec<_>>>()?;
    String::from_utf8(bytes).context("invalid preview path")
}

fn byte_range(header: Option<&str>, size: u64) -> Result<(u64, u64, bool)> {
    let Some(header) = header else {
        return Ok((0, size, false));
    };
    let (start, end) = header
        .strip_prefix("bytes=")
        .context("invalid byte range")?
        .split_once('-')
        .context("invalid byte range")?;
    if size == 0 {
        bail!("unsatisfiable byte range");
    }
    let (start, end) = if start.is_empty() {
        let suffix = end.parse::<u64>().context("invalid byte range")?;
        if suffix == 0 {
            bail!("unsatisfiable byte range");
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        let start = start.parse::<u64>().context("invalid byte range")?;
        let end = if end.is_empty() {
            size - 1
        } else {
            end.parse::<u64>()
                .context("invalid byte range")?
                .min(size - 1)
        };
        (start, end)
    };
    if start >= size || start > end {
        bail!("unsatisfiable byte range");
    }
    Ok((start, end - start + 1, true))
}

pub(super) fn serve(request: Request, viewer: &Viewer, encoded: &str) {
    let file = (|| -> Result<_> {
        let path = decode_path(encoded)?;
        let mime = mime(&path).context("unsupported preview format")?;
        let file = fs::File::open(viewer.safe_path(&path)?)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("not a regular file");
        }
        Ok((file, metadata.len(), mime))
    })();
    let (mut file, size, mime) = match file {
        Ok(file) => file,
        Err(error) => {
            respond(
                request,
                404,
                "text/plain",
                format!("{error:#}").into_bytes(),
            );
            return;
        }
    };
    let (start, length, partial) = match byte_range(header(&request, "Range"), size) {
        Ok(range) => range,
        Err(_) => {
            let response = Response::empty(416).with_header(
                Header::from_bytes("Content-Range", format!("bytes */{size}")).unwrap(),
            );
            let _ = request.respond(response);
            return;
        }
    };
    if let Err(error) = file.seek(SeekFrom::Start(start)) {
        respond(request, 500, "text/plain", error.to_string().into_bytes());
        return;
    }
    let mut response = Response::new(
        StatusCode(if partial { 206 } else { 200 }),
        Vec::new(),
        file.take(length),
        usize::try_from(length).ok(),
        None,
    );
    let policy = if mime.starts_with("text/html") || mime == "image/svg+xml" {
        "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'self'"
    } else {
        "default-src 'none'; frame-ancestors 'self'"
    };
    for (name, value) in [
        ("Content-Type", mime),
        ("Cache-Control", "no-store"),
        ("Accept-Ranges", "bytes"),
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
        ("Content-Security-Policy", policy),
    ] {
        response.add_header(Header::from_bytes(name, value).unwrap());
    }
    if partial {
        response.add_header(
            Header::from_bytes(
                "Content-Range",
                format!("bytes {start}-{}/{size}", start + length - 1),
            )
            .unwrap(),
        );
    }
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_bounded_and_support_seeking() {
        assert_eq!(byte_range(None, 100).unwrap(), (0, 100, false));
        assert_eq!(
            byte_range(Some("bytes=10-19"), 100).unwrap(),
            (10, 10, true)
        );
        assert_eq!(byte_range(Some("bytes=90-"), 100).unwrap(), (90, 10, true));
        assert_eq!(byte_range(Some("bytes=-12"), 100).unwrap(), (88, 12, true));
        assert_eq!(
            byte_range(Some("bytes=90-999"), 100).unwrap(),
            (90, 10, true)
        );
        for value in ["bytes=100-", "bytes=2-1", "bytes=-0", "bytes=0-1,3-4"] {
            assert!(byte_range(Some(value), 100).is_err());
        }
        assert!(byte_range(Some("bytes=0-"), 0).is_err());
    }

    #[test]
    fn preview_paths_round_trip_unicode_and_reject_invalid_encoding() {
        let path = "assets/中文 #?.PNG";
        let encoded = path
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(decode_path(&encoded).unwrap(), path);
        assert_eq!(mime(path), Some("image/png"));
        assert!(decode_path("fff").is_err());
        assert!(decode_path("zz").is_err());
        assert!(decode_path("ff").is_err());
        assert!(mime("secret.sqlite").is_none());
    }
}

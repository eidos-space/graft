use std::ops::Range;

/// One immutable payload request. Transports expand this common inventory into
/// actual body ranges; a plaintext size never stands in for a ciphertext size.
#[derive(Clone, Debug)]
pub struct DownloadRequest {
    pub path: String,
    pub range: Option<Range<u64>>,
    pub length: u64,
}

impl DownloadRequest {
    pub(crate) fn checked_length(&self) -> Option<u64> {
        if let Some(range) = &self.range
            && range.end.checked_sub(range.start) != Some(self.length)
        {
            return None;
        }
        Some(self.length)
    }
}

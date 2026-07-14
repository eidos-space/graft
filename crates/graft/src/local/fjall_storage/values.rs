use std::sync::atomic::{AtomicBool, Ordering};

use crate::core::{
    commit::Commit,
    page::{PAGESIZE, Page},
};
use bilrost::{Message, OwnedMessage};
use bytes::Bytes;

use crate::{
    local::fjall_storage::fjall_repr::{FjallRepr, FjallReprRef},
    volume::Volume,
};

use super::fjall_repr::DecodeErr;

static LEGACY_RELOCATED_PAGE_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

impl FjallReprRef for Page {
    #[inline]
    fn as_slice(&self) -> impl AsRef<[u8]> {
        self
    }

    fn into_slice(self) -> fjall::Slice {
        self.into_bytes().into()
    }
}

impl FjallRepr for Page {
    fn try_from_slice(slice: fjall::Slice) -> Result<Self, DecodeErr> {
        let bytes = Bytes::from(slice);
        if bytes.len() == PAGESIZE.as_usize() {
            return Ok(Page::try_from(bytes)?);
        }

        // lsm-tree <= 3.1.5 could persist relocated LZ4 blobs as uncompressed.
        // Fjall then returns the raw compressed value after recovery. Keep old
        // repositories readable while 3.1.6 prevents new malformed blobs.
        let Ok(recovered) = lz4_flex::decompress(&bytes, PAGESIZE.as_usize()) else {
            return Ok(Page::try_from(bytes)?);
        };
        let page = Page::try_from(Bytes::from(recovered))?;

        if !LEGACY_RELOCATED_PAGE_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                compressed_size = bytes.len(),
                "recovered a page from legacy Fjall blob-relocation metadata"
            );
        }
        Ok(page)
    }
}

macro_rules! impl_fjallrepr_for_bilrost {
    ($($ty:ty),+) => {
        $(
            impl FjallReprRef for $ty {
                #[inline]
                fn as_slice(&self) -> impl AsRef<[u8]> {
                    self.encode_to_bytes()
                }

                #[inline]
                fn into_slice(self) -> fjall::Slice {
                    self.encode_to_bytes().into()
                }
            }

            impl FjallRepr for $ty {
                #[inline]
                fn try_from_slice(slice: fjall::Slice) -> Result<Self, DecodeErr> {
                    Ok(<$ty>::decode(Bytes::from(slice))?)
                }
            }
        )+
    };
}

impl_fjallrepr_for_bilrost!(Volume, Commit);

impl FjallReprRef for () {
    #[inline]
    fn as_slice(&self) -> impl AsRef<[u8]> {
        []
    }

    #[inline]
    fn into_slice(self) -> fjall::Slice
    where
        Self: Sized,
    {
        Bytes::new().into()
    }
}

impl FjallRepr for () {
    fn try_from_slice(slice: fjall::Slice) -> Result<Self, DecodeErr> {
        if slice.is_empty() {
            Ok(())
        } else {
            Err(DecodeErr::NonemptyValue(slice.len()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_log::test;

    use crate::core::VolumeId;
    use crate::core::{LogId, PageCount};
    use crate::lsn;

    use crate::local::fjall_storage::fjall_repr::testutil::{
        test_empty_default, test_invalid, test_roundtrip,
    };

    #[test]
    fn test_page() {
        test_roundtrip(Page::test_filled(123));
        test_roundtrip(Page::EMPTY);
        test_invalid::<Page>(&b"a".repeat(PAGESIZE.as_usize() + 1));
    }

    #[test]
    fn test_page_recovers_legacy_relocated_lz4_value() {
        let expected = Page::test_filled(123);
        let compressed = lz4_flex::compress(expected.as_ref());
        assert_ne!(compressed.len(), PAGESIZE.as_usize());

        let actual = Page::try_from_slice(compressed.into()).unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn test_page_rejects_compressed_non_page_value() {
        let compressed = lz4_flex::compress(b"not a page");
        assert!(Page::try_from_slice(compressed.into()).is_err());
    }

    #[test]
    fn test_volume() {
        test_roundtrip(Volume::new(
            VolumeId::random(),
            LogId::random(),
            LogId::random(),
            None,
            None,
        ));
        test_empty_default::<Volume>();
        test_invalid::<Volume>(&b"abc".repeat(123));
    }

    #[test]
    fn test_commit() {
        test_roundtrip(Commit::new(LogId::random(), lsn!(123), PageCount::new(456)));
        test_empty_default::<Commit>();
        test_invalid::<Commit>(&b"abc".repeat(123));
    }
}

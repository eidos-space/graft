use std::{
    collections::BTreeSet,
    ops::{BitOr, BitOrAssign, Bound, RangeBounds, RangeInclusive},
};

use bytes::Bytes;

use crate::{
    core::{PageCount, PageIdx},
    derive_newtype_proxy,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageSet {
    pages: BTreeSet<PageIdx>,
}

impl PageSet {
    pub const EMPTY: Self = Self { pages: BTreeSet::new() };

    pub fn from_range(range: RangeInclusive<PageIdx>) -> Self {
        Self {
            pages: crate::core::pageidx::PageIdxIter::new(range).collect(),
        }
    }

    pub fn from_pageidx_iter(iter: impl IntoIterator<Item = PageIdx>) -> Self {
        Self { pages: iter.into_iter().collect() }
    }

    pub fn cardinality(&self) -> PageCount {
        PageCount::new(self.pages.len() as u32)
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn first(&self) -> Option<PageIdx> {
        self.pages.first().copied()
    }

    pub fn last(&self) -> Option<PageIdx> {
        self.pages.last().copied()
    }

    pub fn insert(&mut self, pageidx: PageIdx) -> bool {
        self.pages.insert(pageidx)
    }

    pub fn contains(&self, pageidx: PageIdx) -> bool {
        self.pages.contains(&pageidx)
    }

    pub fn contains_all<R: RangeBounds<PageIdx>>(&self, pages: &R) -> bool {
        let Some((start, end)) = concrete_bounds(pages) else {
            return true;
        };
        let expected = u64::from(end.to_u32()) - u64::from(start.to_u32()) + 1;
        self.pages
            .iter()
            .filter(|pageidx| in_bounds(**pageidx, pages))
            .count() as u64
            == expected
    }

    pub fn contains_any<R: RangeBounds<PageIdx>>(&self, pages: &R) -> bool {
        self.pages.iter().any(|pageidx| in_bounds(*pageidx, pages))
    }

    pub fn truncate(&mut self, page_count: PageCount) {
        self.pages.retain(|pageidx| page_count.contains(*pageidx));
    }

    pub fn remove_page_range<R: RangeBounds<PageIdx>>(&mut self, pages: R) {
        self.pages.retain(|pageidx| !in_bounds(*pageidx, &pages));
    }

    pub fn cut(&mut self, rhs: &PageSet) -> PageSet {
        let intersection = self
            .pages
            .intersection(&rhs.pages)
            .copied()
            .collect::<BTreeSet<_>>();
        for pageidx in &intersection {
            self.pages.remove(pageidx);
        }
        PageSet { pages: intersection }
    }

    pub fn difference(&self, rhs: &PageSet) -> PageSet {
        PageSet {
            pages: self.pages.difference(&rhs.pages).copied().collect(),
        }
    }

    pub fn intersection_range(&self, range: RangeInclusive<PageIdx>) -> PageSet {
        PageSet {
            pages: self.pages.range(range).copied().collect(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = PageIdx> + '_ {
        self.pages.iter().copied()
    }
}

impl FromIterator<PageIdx> for PageSet {
    fn from_iter<T: IntoIterator<Item = PageIdx>>(iter: T) -> Self {
        Self::from_pageidx_iter(iter)
    }
}

impl BitOrAssign<Self> for PageSet {
    fn bitor_assign(&mut self, rhs: Self) {
        self.pages.extend(rhs.pages);
    }
}

impl BitOr<Self> for PageSet {
    type Output = Self;

    fn bitor(mut self, rhs: Self) -> Self::Output {
        self |= rhs;
        self
    }
}

derive_newtype_proxy!(
    newtype (PageSet)
    with empty value (PageSet::EMPTY)
    with proxy type (Bytes) and encoding (bilrost::encoding::General)
    with sample value (PageSet::from_pageidx_iter(PageCount::new(9).iter()))
    into_proxy(&self) {
        let mut encoded = Vec::with_capacity(self.pages.len() * 4);
        for pageidx in &self.pages {
            encoded.extend_from_slice(&pageidx.to_u32().to_le_bytes());
        }
        Bytes::from(encoded)
    }
    from_proxy(&mut self, proxy) {
        let mut chunks = proxy.chunks_exact(4);
        let mut pages = BTreeSet::new();
        for chunk in &mut chunks {
            let value = u32::from_le_bytes(chunk.try_into().expect("four-byte chunk"));
            let pageidx = PageIdx::try_new(value)
                .ok_or(bilrost::DecodeErrorKind::InvalidValue)?;
            pages.insert(pageidx);
        }
        if !chunks.remainder().is_empty() {
            return Err(bilrost::DecodeErrorKind::InvalidValue);
        }
        self.pages = pages;
        Ok(())
    }
);

fn concrete_bounds<R: RangeBounds<PageIdx>>(range: &R) -> Option<(PageIdx, PageIdx)> {
    let start = match range.start_bound() {
        Bound::Included(value) => *value,
        Bound::Excluded(value) if *value == PageIdx::LAST => return None,
        Bound::Excluded(value) => value.saturating_next(),
        Bound::Unbounded => PageIdx::FIRST,
    };
    let end = match range.end_bound() {
        Bound::Included(value) => *value,
        Bound::Excluded(value) if *value == PageIdx::FIRST => return None,
        Bound::Excluded(value) => value.saturating_prev(),
        Bound::Unbounded => PageIdx::LAST,
    };
    (start <= end).then_some((start, end))
}

fn in_bounds<R: RangeBounds<PageIdx>>(pageidx: PageIdx, range: &R) -> bool {
    let after_start = match range.start_bound() {
        Bound::Included(start) => pageidx >= *start,
        Bound::Excluded(start) => pageidx > *start,
        Bound::Unbounded => true,
    };
    let before_end = match range.end_bound() {
        Bound::Included(end) => pageidx <= *end,
        Bound::Excluded(end) => pageidx < *end,
        Bound::Unbounded => true,
    };
    after_start && before_end
}

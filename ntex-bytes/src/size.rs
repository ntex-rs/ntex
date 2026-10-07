use crate::stvec;

/// Capacity category used when allocating [`crate::BytePage`] storage.
///
/// Buffers with a page size are returned to a per-thread cache of their
/// category when the last reference is dropped, see
/// [`crate::set_page_cache_size`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum BytePageSize {
    /// A 4 KiB page.
    Size4 = 0,
    /// An 8 KiB page.
    Size8 = 1,
    /// A 16 KiB page.
    #[default]
    Size16 = 2,
    /// A 24 KiB page.
    Size24 = 3,
    /// A 32 KiB page.
    Size32 = 4,
    /// A 48 KiB page.
    Size48 = 5,
    /// A 64 KiB page.
    Size64 = 6,
    /// A 128 KiB page.
    Size128 = 7,
    /// A 256 KiB page.
    Size256 = 8,
    /// No fixed page category.
    ///
    /// Buffers of this category are sized on demand and never returned to
    /// the page cache. A buffer whose allocation is exactly a page size
    /// belongs to that page category, whichever way it was created. It
    /// cannot be used as the page size of [`crate::BytePages`].
    Unset = 9,
}

/// Page categories in increasing order of size, `Unset` excluded.
pub(crate) const PAGE_SIZES: [BytePageSize; 9] = [
    BytePageSize::Size4,
    BytePageSize::Size8,
    BytePageSize::Size16,
    BytePageSize::Size24,
    BytePageSize::Size32,
    BytePageSize::Size48,
    BytePageSize::Size64,
    BytePageSize::Size128,
    BytePageSize::Size256,
];

/// Size of the units of [`CLASS_BY_UNITS`], every page size is a multiple of it.
const PAGE_UNIT_SHIFT: u32 = 12;

/// Page category by allocation size in 4 KiB units, `Unset` for sizes that
/// are not a page size.
const CLASS_BY_UNITS: [BytePageSize; 65] = {
    let mut table = [BytePageSize::Unset; 65];
    let mut i = 0;
    while i < PAGE_SIZES.len() {
        table[PAGE_SIZES[i].alloc_size() >> PAGE_UNIT_SHIFT] = PAGE_SIZES[i];
        i += 1;
    }
    table
};

impl BytePageSize {
    /// Returns the smallest page category with a [`capacity`](Self::capacity)
    /// of at least `capacity` bytes.
    ///
    /// Returns [`BytePageSize::Unset`] if `capacity` is larger than the
    /// capacity of the largest category.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytePageSize;
    ///
    /// assert_eq!(BytePageSize::for_capacity(100), BytePageSize::Size4);
    /// assert_eq!(BytePageSize::for_capacity(5000), BytePageSize::Size8);
    /// assert_eq!(BytePageSize::for_capacity(1024 * 1024), BytePageSize::Unset);
    /// ```
    pub const fn for_capacity(capacity: usize) -> BytePageSize {
        let mut i = 0;
        while i < PAGE_SIZES.len() {
            if capacity <= PAGE_SIZES[i].capacity() {
                return PAGE_SIZES[i];
            }
            i += 1;
        }
        BytePageSize::Unset
    }

    /// Returns the next larger page category.
    ///
    /// The largest category returns [`BytePageSize::Unset`], `Unset` returns
    /// itself.
    #[must_use]
    pub const fn next(self) -> BytePageSize {
        match self {
            BytePageSize::Size4 => BytePageSize::Size8,
            BytePageSize::Size8 => BytePageSize::Size16,
            BytePageSize::Size16 => BytePageSize::Size24,
            BytePageSize::Size24 => BytePageSize::Size32,
            BytePageSize::Size32 => BytePageSize::Size48,
            BytePageSize::Size48 => BytePageSize::Size64,
            BytePageSize::Size64 => BytePageSize::Size128,
            BytePageSize::Size128 => BytePageSize::Size256,
            BytePageSize::Size256 | BytePageSize::Unset => BytePageSize::Unset,
        }
    }

    /// Returns the next smaller page category.
    ///
    /// The smallest category returns itself, [`BytePageSize::Unset`] returns
    /// the largest category.
    #[must_use]
    pub const fn prev(self) -> BytePageSize {
        match self {
            BytePageSize::Size4 | BytePageSize::Size8 => BytePageSize::Size4,
            BytePageSize::Size16 => BytePageSize::Size8,
            BytePageSize::Size24 => BytePageSize::Size16,
            BytePageSize::Size32 => BytePageSize::Size24,
            BytePageSize::Size48 => BytePageSize::Size32,
            BytePageSize::Size64 => BytePageSize::Size48,
            BytePageSize::Size128 => BytePageSize::Size64,
            BytePageSize::Size256 => BytePageSize::Size128,
            BytePageSize::Unset => BytePageSize::Size256,
        }
    }

    /// Returns the page capacity in bytes.
    ///
    /// A page is allocated together with its header, the capacity is the
    /// category size minus the header, so the allocation is exactly the
    /// category size and fits the allocator's size classes.
    pub const fn capacity(self) -> usize {
        self.alloc_size() - stvec::METADATA_SIZE
    }

    const fn alloc_size(self) -> usize {
        match self {
            BytePageSize::Size4 => 4 * 1024,
            BytePageSize::Size8 => 8 * 1024,
            BytePageSize::Size16 => 16 * 1024,
            BytePageSize::Size24 => 24 * 1024,
            BytePageSize::Size32 => 32 * 1024,
            BytePageSize::Size48 => 48 * 1024,
            BytePageSize::Size64 | BytePageSize::Unset => 64 * 1024,
            BytePageSize::Size128 => 128 * 1024,
            BytePageSize::Size256 => 256 * 1024,
        }
    }

    /// Returns the page category of an allocation of `size` bytes, header
    /// included, `Unset` if `size` is not a page size.
    #[inline]
    pub(crate) const fn from_alloc_size(size: usize) -> BytePageSize {
        let units = size >> PAGE_UNIT_SHIFT;
        if size & ((1 << PAGE_UNIT_SHIFT) - 1) != 0 || units >= CLASS_BY_UNITS.len() {
            BytePageSize::Unset
        } else {
            CLASS_BY_UNITS[units]
        }
    }

    /// Returns the recommended write-buffer threshold for this page size.
    ///
    /// This is half of the category size, but at most 16 KiB.
    pub const fn half_capacity(self) -> usize {
        match self {
            BytePageSize::Size4 => 2 * 1024,
            BytePageSize::Size8 => 4 * 1024,
            BytePageSize::Size16 => 8 * 1024,
            BytePageSize::Size24 => 12 * 1024,
            BytePageSize::Size32
            | BytePageSize::Size48
            | BytePageSize::Size64
            | BytePageSize::Size128
            | BytePageSize::Size256
            | BytePageSize::Unset => 16 * 1024,
        }
    }

    /// Returns the low free-capacity threshold for this page size.
    ///
    /// This is 2/32 of the category size: 256 bytes for `Size4`, 1 KiB for
    /// `Size16`, 16 KiB for `Size256` and 4 KiB for [`BytePageSize::Unset`].
    /// [`crate::BytesMut::reserve_more`] grows a buffer once its remaining
    /// capacity falls below it.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytePageSize;
    ///
    /// assert_eq!(BytePageSize::Size4.low(), 256);
    /// assert_eq!(BytePageSize::Size8.low(), 512);
    /// assert_eq!(BytePageSize::Size16.low(), 1024);
    /// assert_eq!(BytePageSize::Unset.low(), 4096);
    /// ```
    pub const fn low(self) -> usize {
        self.alloc_size() >> 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size() {
        const META: usize = stvec::METADATA_SIZE;
        assert_eq!(BytePageSize::Size4.capacity(), 4 * 1024 - META);
        assert_eq!(BytePageSize::Size8.capacity(), 8 * 1024 - META);
        assert_eq!(BytePageSize::Size16.capacity(), 16 * 1024 - META);
        assert_eq!(BytePageSize::Size24.capacity(), 24 * 1024 - META);
        assert_eq!(BytePageSize::Size32.capacity(), 32 * 1024 - META);
        assert_eq!(BytePageSize::Size48.capacity(), 48 * 1024 - META);
        assert_eq!(BytePageSize::Size64.capacity(), 64 * 1024 - META);
        assert_eq!(BytePageSize::Size128.capacity(), 128 * 1024 - META);
        assert_eq!(BytePageSize::Size256.capacity(), 256 * 1024 - META);
        assert_eq!(BytePageSize::Unset.capacity(), 64 * 1024 - META);
        assert_eq!(BytePageSize::Size4.half_capacity(), 2 * 1024);
        assert_eq!(BytePageSize::Size8.half_capacity(), 4 * 1024);
        assert_eq!(BytePageSize::Size16.half_capacity(), 8 * 1024);
        assert_eq!(BytePageSize::Size24.half_capacity(), 12 * 1024);
        assert_eq!(BytePageSize::Size32.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size48.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size64.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size128.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size256.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Unset.half_capacity(), 16 * 1024);
    }

    #[test]
    fn page_size_from_alloc_size() {
        const META: usize = stvec::METADATA_SIZE;
        assert_eq!(META, 16);
        for size in PAGE_SIZES {
            assert_eq!(BytePageSize::from_alloc_size(size.alloc_size()), size);
            assert_eq!(BytePageSize::from_alloc_size(size.capacity() + META), size);
            assert_eq!(
                BytePageSize::from_alloc_size(size.alloc_size() - 1),
                BytePageSize::Unset
            );
            assert_eq!(
                BytePageSize::from_alloc_size(size.alloc_size() + 1),
                BytePageSize::Unset
            );
        }
        for units in [0, 3, 5, 10, 40, 63, 64 + 1, 128 + 1] {
            assert_eq!(
                BytePageSize::from_alloc_size(units * 4096),
                BytePageSize::Unset
            );
        }
        for size in [0, 1, 4095, 512 * 1024, usize::MAX, usize::MAX & !0xFFF] {
            assert_eq!(BytePageSize::from_alloc_size(size), BytePageSize::Unset);
        }
    }

    #[test]
    fn page_size_for_capacity() {
        assert_eq!(BytePageSize::for_capacity(0), BytePageSize::Size4);
        for size in PAGE_SIZES {
            let cap = size.capacity();
            assert_eq!(BytePageSize::for_capacity(cap), size);
            if size != BytePageSize::Size4 {
                assert_eq!(BytePageSize::for_capacity(size.prev().capacity() + 1), size);
            }
        }
        assert_eq!(
            BytePageSize::for_capacity(BytePageSize::Size256.capacity() + 1),
            BytePageSize::Unset
        );
        assert_eq!(BytePageSize::for_capacity(usize::MAX), BytePageSize::Unset);
    }

    #[test]
    fn page_size_next_prev() {
        let mut size = BytePageSize::Size4;
        for expected in &PAGE_SIZES[1..] {
            size = size.next();
            assert_eq!(size, *expected);
        }
        assert_eq!(size.next(), BytePageSize::Unset);
        assert_eq!(BytePageSize::Unset.next(), BytePageSize::Unset);

        let mut size = BytePageSize::Unset;
        for expected in PAGE_SIZES.iter().rev() {
            size = size.prev();
            assert_eq!(size, *expected);
        }
        assert_eq!(size.prev(), BytePageSize::Size4);

        for pair in PAGE_SIZES.windows(2) {
            assert!(pair[0].capacity() < pair[1].capacity());
        }
    }
}

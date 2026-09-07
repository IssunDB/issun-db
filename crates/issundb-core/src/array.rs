//! `Array<T>`: an immutable array that either owns its elements or views a
//! region of a memory-mapped cache file.
//!
//! The CSR snapshot and the property columns are plain arrays every kernel
//! indexes as a slice. Loading them from a cache file into a `Vec` made a
//! second, heap-owned copy of data that already sits in the page cache, and
//! that copy was neither shareable between processes nor evictable under
//! memory pressure. An `Array` dereferences to the same `&[T]` whether it owns
//! a `Vec` or points into a mapped file, so a kernel cannot tell the two apart
//! and the mapped pages stay the kernel's to keep or drop.
//!
//! Mutation goes through [`Array::with_mut`], which copies a mapped view into
//! an owned `Vec` first. That is the copy-on-write the incremental refresh and
//! the column patch rely on: a mapped file is never written through.

use std::ops::Deref;
use std::ptr::NonNull;

/// A read-only memory-mapped cache file, shared by every `Array` that views it.
#[cfg(feature = "lmdb")]
pub(crate) struct MappedFile {
    map: memmap2::Mmap,
}

#[cfg(feature = "lmdb")]
impl MappedFile {
    /// Map `path` read-only.
    ///
    /// Safety of the underlying `Mmap::map` rests on the cache files being
    /// written whole to a temporary name and renamed into place, never
    /// modified through the path a live mapping was opened from: a process
    /// that maps the file keeps the inode it mapped, and the file's bytes do
    /// not change under it. A file truncated by something outside the engine
    /// would fault on access, which is the one failure the checksum cannot
    /// prevent and which no other writer here causes.
    pub(crate) fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        // SAFETY: see the method documentation; the engine is the only writer
        // of these files and replaces them by rename.
        let map = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Self { map })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.map
    }
}

/// Who keeps the elements alive.
enum Owner<T> {
    Vec(Vec<T>),
    #[cfg(feature = "lmdb")]
    Mapped(std::sync::Arc<MappedFile>),
}

/// An immutable array of plain values, owned or mapped. Dereferences to `[T]`.
pub(crate) struct Array<T> {
    /// Start of the elements: the `Vec`'s buffer, or a validated, aligned
    /// position inside the mapped file. Refreshed whenever the owner changes.
    ptr: NonNull<T>,
    len: usize,
    owner: Owner<T>,
}

// SAFETY: the pointer addresses memory the owner keeps alive for as long as
// the `Array` exists, nothing writes through it, and the owners themselves
// (`Vec<T>` and a read-only mapping) are `Send` and `Sync` under the same
// bounds a `Vec<T>` needs.
unsafe impl<T: Send> Send for Array<T> {}
unsafe impl<T: Sync> Sync for Array<T> {}

impl<T> Array<T> {
    fn from_vec(vec: Vec<T>) -> Self {
        // A slice pointer is non-null and aligned even for an empty `Vec`.
        let ptr = NonNull::from(vec.as_slice()).cast::<T>();
        Self {
            ptr,
            len: vec.len(),
            owner: Owner::Vec(vec),
        }
    }

    /// A view of `len` elements starting `offset` bytes into `file`, or `None`
    /// when the region is out of bounds or misaligned for `T`. The file is
    /// held for as long as the view lives; nothing is copied.
    #[cfg(feature = "lmdb")]
    pub(crate) fn mapped(
        file: &std::sync::Arc<MappedFile>,
        offset: usize,
        len: usize,
    ) -> Option<Self>
    where
        T: zerocopy::FromBytes + zerocopy::Immutable + zerocopy::KnownLayout,
    {
        let bytes_len = len.checked_mul(std::mem::size_of::<T>())?;
        let end = offset.checked_add(bytes_len)?;
        let bytes = file.bytes().get(offset..end)?;
        // Validates the alignment and the length; the `&[T]` it yields is not
        // kept, only its address, which the `Arc` keeps valid.
        use zerocopy::FromBytes;
        let slice = <[T]>::ref_from_bytes(bytes).ok()?;
        let ptr = NonNull::new(slice.as_ptr().cast_mut())?;
        Some(Self {
            ptr,
            len,
            owner: Owner::Mapped(std::sync::Arc::clone(file)),
        })
    }

    /// Whether the elements live in a mapped file rather than on the heap.
    #[cfg(all(feature = "lmdb", test))]
    pub(crate) fn is_mapped(&self) -> bool {
        matches!(self.owner, Owner::Mapped(_))
    }

    /// Mutate the elements as a `Vec`, copying a mapped view onto the heap
    /// first. The pointer and length are refreshed afterwards, so a `push` or
    /// a reallocation inside `f` is safe.
    pub(crate) fn with_mut<R>(&mut self, f: impl FnOnce(&mut Vec<T>) -> R) -> R
    where
        T: Clone,
    {
        #[cfg(feature = "lmdb")]
        if let Owner::Mapped(_) = self.owner {
            let copy = self.to_vec();
            self.owner = Owner::Vec(copy);
        }
        let out = match &mut self.owner {
            Owner::Vec(vec) => {
                let out = f(vec);
                self.ptr = NonNull::from(vec.as_slice()).cast::<T>();
                self.len = vec.len();
                out
            }
            #[cfg(feature = "lmdb")]
            Owner::Mapped(_) => unreachable!("a mapped owner was copied above"),
        };
        out
    }
}

impl<T> Deref for Array<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: `ptr` and `len` describe initialized, aligned elements the
        // owner keeps alive and unmodified for the life of `self`; see the
        // struct invariant.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> From<Vec<T>> for Array<T> {
    fn from(vec: Vec<T>) -> Self {
        Self::from_vec(vec)
    }
}

impl<T> Default for Array<T> {
    fn default() -> Self {
        Self::from_vec(Vec::new())
    }
}

impl<T: Clone> Clone for Array<T> {
    fn clone(&self) -> Self {
        match &self.owner {
            Owner::Vec(v) => Self::from_vec(v.clone()),
            #[cfg(feature = "lmdb")]
            Owner::Mapped(file) => Self {
                ptr: self.ptr,
                len: self.len,
                owner: Owner::Mapped(std::sync::Arc::clone(file)),
            },
        }
    }
}

impl<T: PartialEq> PartialEq for Array<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: PartialEq> PartialEq<Vec<T>> for Array<T> {
    fn eq(&self, other: &Vec<T>) -> bool {
        **self == other[..]
    }
}

impl<T: Eq> Eq for Array<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for Array<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> FromIterator<T> for Array<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from_vec(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_array_reads_and_mutates_like_a_vec() {
        let mut a: Array<u32> = vec![3, 1, 2].into();
        assert_eq!(a.len(), 3);
        assert_eq!(a[1], 1);
        assert_eq!(&a[..], &[3, 1, 2]);
        a.with_mut(|v| {
            v.push(9);
            v.sort_unstable();
        });
        assert_eq!(&a[..], &[1, 2, 3, 9]);
        assert_eq!(a, vec![1, 2, 3, 9]);
        let empty: Array<u64> = Array::default();
        assert!(empty.is_empty());
    }

    /// A mapped view reads the file's elements in place, a misaligned or
    /// oversized region is refused, and the first mutation copies onto the heap
    /// without touching the file.
    #[cfg(feature = "lmdb")]
    #[test]
    fn mapped_array_views_the_file_and_copies_on_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("arr.bin");
        let mut bytes = Vec::new();
        for v in [10u64, 20, 30, 40] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();
        let file = std::sync::Arc::new(MappedFile::open(&path).unwrap());

        let mut a: Array<u64> = Array::mapped(&file, 8, 3).expect("aligned in-bounds view");
        assert!(a.is_mapped());
        assert_eq!(&a[..], &[20, 30, 40]);
        assert!(Array::<u64>::mapped(&file, 4, 1).is_none(), "misaligned");
        assert!(Array::<u64>::mapped(&file, 8, 4).is_none(), "past the end");
        assert!(
            Array::<u32>::mapped(&file, 4, 1).is_some(),
            "u32 needs 4-byte alignment"
        );

        let shared = a.clone();
        a.with_mut(|v| v[0] = 7);
        assert!(!a.is_mapped());
        assert_eq!(&a[..], &[7, 30, 40]);
        assert_eq!(&shared[..], &[20, 30, 40], "the mapped view is untouched");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "the file is untouched"
        );
    }
}

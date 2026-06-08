//! Minimal safe wrapper over the SuiteSparse:GraphBLAS operations that IssunDB uses.
//!
//! This crate intentionally exposes only the small slice of GraphBLAS the engine
//! needs: typed sparse matrices and vectors over `i32`, `f32`, and `f64`; building
//! them from element triples; matrix-by-vector multiply (`GrB_mxv`) over a fixed set
//! of predefined semirings; element-wise vector addition (`GrB_Vector_eWiseAdd_Monoid`)
//! over a fixed set of predefined monoids; and the descriptor flags those operations
//! use. Everything maps directly onto predefined `GrB_*` objects, so no semiring,
//! monoid, or binary-operator construction is needed.
//!
//! Built over `issundb-graphblas-sys`, the in-house raw FFI to the Apache-2.0
//! SuiteSparse:GraphBLAS C library (PIC, dynamic OpenMP), so this wrapper links
//! into the binding `cdylib`s.

use std::marker::PhantomData;
use std::os::raw::c_int;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Once};

use issundb_graphblas_sys as gb;

/// Error returned by any wrapped GraphBLAS call that does not return `GrB_SUCCESS`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct GraphblasError(pub String);

type Result<T> = std::result::Result<T, GraphblasError>;

#[inline]
fn check(info: gb::GrB_Info, what: &str) -> Result<()> {
    if info == gb::GrB_Info_GrB_SUCCESS {
        Ok(())
    } else {
        Err(GraphblasError(format!("{what} failed: GrB_Info={info}")))
    }
}

// --- Context ---------------------------------------------------------------

/// Handle proving the process-global GraphBLAS runtime has been initialized.
///
/// GraphBLAS keeps a single process-global context and OpenMP thread pool. It is
/// initialized exactly once (any number of `init_default` calls share it) and is
/// never finalized, matching the engine's existing invariant: re-init after
/// `GrB_finalize` is not allowed by GraphBLAS, so we simply never finalize.
pub struct Context {
    _private: (),
}

static GB_INIT: Once = Once::new();
static GB_INIT_INFO: AtomicI32 = AtomicI32::new(0);

impl Context {
    /// Initialize GraphBLAS in non-blocking mode (idempotent) and return a handle.
    pub fn init_default() -> Result<Arc<Self>> {
        GB_INIT.call_once(|| {
            let info = unsafe { gb::GrB_init(gb::GrB_Mode_GrB_NONBLOCKING as c_int) };
            GB_INIT_INFO.store(info, Ordering::SeqCst);
        });
        check(GB_INIT_INFO.load(Ordering::SeqCst), "GrB_init")?;
        Ok(Arc::new(Self { _private: () }))
    }

    /// Run a raw GraphBLAS call and map a non-success status to an error. Kept for
    /// callers that need to drive a `GxB_*` option not modeled by this crate.
    pub fn call_raw<F: FnMut() -> gb::GrB_Info>(&self, mut f: F) -> Result<()> {
        check(f(), "GraphBLAS call")
    }
}

/// Set the global number of OpenMP threads GraphBLAS may use (`GxB_NTHREADS`).
pub fn set_global_threads(n: i32) -> Result<()> {
    check(
        unsafe { gb::GxB_Global_Option_set(gb::GxB_NTHREADS as c_int, n as c_int) },
        "GxB_Global_Option_set(GxB_NTHREADS)",
    )
}

// --- Element types ---------------------------------------------------------

/// Element types the engine materializes matrices and vectors over. Each method
/// resolves to a predefined `GrB_*` object or the typed `GrB_*_<TYPE>` FFI call.
///
/// The `unsafe` methods are thin dispatch shims to the GraphBLAS C API; they
/// carry the standard FFI contract (valid object handles, correctly sized
/// buffers) and are only ever called from this crate's checked wrappers, so they
/// are not documented individually.
#[allow(clippy::missing_safety_doc)]
pub trait GbType: Copy + Default {
    fn gb_type() -> gb::GrB_Type;
    fn min_plus_semiring() -> gb::GrB_Semiring;
    fn plus_times_semiring() -> gb::GrB_Semiring;
    fn min_second_semiring() -> gb::GrB_Semiring;
    fn min_monoid() -> gb::GrB_Monoid;
    fn plus_monoid() -> gb::GrB_Monoid;
    fn first_binop() -> gb::GrB_BinaryOp;
    fn plus_binop() -> gb::GrB_BinaryOp;

    unsafe fn vec_build(
        w: gb::GrB_Vector,
        i: *const u64,
        x: *const Self,
        n: u64,
        dup: gb::GrB_BinaryOp,
    ) -> gb::GrB_Info;
    unsafe fn vec_set(w: gb::GrB_Vector, x: Self, i: u64) -> gb::GrB_Info;
    unsafe fn vec_extract_element(x: *mut Self, v: gb::GrB_Vector, i: u64) -> gb::GrB_Info;
    unsafe fn vec_extract_tuples(
        i: *mut u64,
        x: *mut Self,
        n: *mut u64,
        v: gb::GrB_Vector,
    ) -> gb::GrB_Info;

    unsafe fn mat_build(
        c: gb::GrB_Matrix,
        i: *const u64,
        j: *const u64,
        x: *const Self,
        n: u64,
        dup: gb::GrB_BinaryOp,
    ) -> gb::GrB_Info;
    unsafe fn mat_set(c: gb::GrB_Matrix, x: Self, i: u64, j: u64) -> gb::GrB_Info;
    unsafe fn mat_extract_tuples(
        i: *mut u64,
        j: *mut u64,
        x: *mut Self,
        n: *mut u64,
        a: gb::GrB_Matrix,
    ) -> gb::GrB_Info;
}

macro_rules! impl_gb_type {
    (
        $t:ty, $gbtype:ident,
        $minplus:ident, $plustimes:ident, $minsecond:ident,
        $minmon:ident, $plusmon:ident, $first:ident, $plusbin:ident,
        $vbuild:ident, $vset:ident, $vextel:ident, $vextup:ident,
        $mbuild:ident, $mset:ident, $mextup:ident
    ) => {
        impl GbType for $t {
            fn gb_type() -> gb::GrB_Type {
                unsafe { gb::$gbtype }
            }
            fn min_plus_semiring() -> gb::GrB_Semiring {
                unsafe { gb::$minplus }
            }
            fn plus_times_semiring() -> gb::GrB_Semiring {
                unsafe { gb::$plustimes }
            }
            fn min_second_semiring() -> gb::GrB_Semiring {
                unsafe { gb::$minsecond }
            }
            fn min_monoid() -> gb::GrB_Monoid {
                unsafe { gb::$minmon }
            }
            fn plus_monoid() -> gb::GrB_Monoid {
                unsafe { gb::$plusmon }
            }
            fn first_binop() -> gb::GrB_BinaryOp {
                unsafe { gb::$first }
            }
            fn plus_binop() -> gb::GrB_BinaryOp {
                unsafe { gb::$plusbin }
            }

            unsafe fn vec_build(
                w: gb::GrB_Vector,
                i: *const u64,
                x: *const Self,
                n: u64,
                dup: gb::GrB_BinaryOp,
            ) -> gb::GrB_Info {
                unsafe { gb::$vbuild(w, i, x, n, dup) }
            }
            unsafe fn vec_set(w: gb::GrB_Vector, x: Self, i: u64) -> gb::GrB_Info {
                unsafe { gb::$vset(w, x, i) }
            }
            unsafe fn vec_extract_element(x: *mut Self, v: gb::GrB_Vector, i: u64) -> gb::GrB_Info {
                unsafe { gb::$vextel(x, v, i) }
            }
            unsafe fn vec_extract_tuples(
                i: *mut u64,
                x: *mut Self,
                n: *mut u64,
                v: gb::GrB_Vector,
            ) -> gb::GrB_Info {
                unsafe { gb::$vextup(i, x, n, v) }
            }

            unsafe fn mat_build(
                c: gb::GrB_Matrix,
                i: *const u64,
                j: *const u64,
                x: *const Self,
                n: u64,
                dup: gb::GrB_BinaryOp,
            ) -> gb::GrB_Info {
                unsafe { gb::$mbuild(c, i, j, x, n, dup) }
            }
            unsafe fn mat_set(c: gb::GrB_Matrix, x: Self, i: u64, j: u64) -> gb::GrB_Info {
                unsafe { gb::$mset(c, x, i, j) }
            }
            unsafe fn mat_extract_tuples(
                i: *mut u64,
                j: *mut u64,
                x: *mut Self,
                n: *mut u64,
                a: gb::GrB_Matrix,
            ) -> gb::GrB_Info {
                unsafe { gb::$mextup(i, j, x, n, a) }
            }
        }
    };
}

impl_gb_type!(
    i32,
    GrB_INT32,
    GrB_MIN_PLUS_SEMIRING_INT32,
    GrB_PLUS_TIMES_SEMIRING_INT32,
    GrB_MIN_SECOND_SEMIRING_INT32,
    GrB_MIN_MONOID_INT32,
    GrB_PLUS_MONOID_INT32,
    GrB_FIRST_INT32,
    GrB_PLUS_INT32,
    GrB_Vector_build_INT32,
    GrB_Vector_setElement_INT32,
    GrB_Vector_extractElement_INT32,
    GrB_Vector_extractTuples_INT32,
    GrB_Matrix_build_INT32,
    GrB_Matrix_setElement_INT32,
    GrB_Matrix_extractTuples_INT32
);
impl_gb_type!(
    f32,
    GrB_FP32,
    GrB_MIN_PLUS_SEMIRING_FP32,
    GrB_PLUS_TIMES_SEMIRING_FP32,
    GrB_MIN_SECOND_SEMIRING_FP32,
    GrB_MIN_MONOID_FP32,
    GrB_PLUS_MONOID_FP32,
    GrB_FIRST_FP32,
    GrB_PLUS_FP32,
    GrB_Vector_build_FP32,
    GrB_Vector_setElement_FP32,
    GrB_Vector_extractElement_FP32,
    GrB_Vector_extractTuples_FP32,
    GrB_Matrix_build_FP32,
    GrB_Matrix_setElement_FP32,
    GrB_Matrix_extractTuples_FP32
);
impl_gb_type!(
    f64,
    GrB_FP64,
    GrB_MIN_PLUS_SEMIRING_FP64,
    GrB_PLUS_TIMES_SEMIRING_FP64,
    GrB_MIN_SECOND_SEMIRING_FP64,
    GrB_MIN_MONOID_FP64,
    GrB_PLUS_MONOID_FP64,
    GrB_FIRST_FP64,
    GrB_PLUS_FP64,
    GrB_Vector_build_FP64,
    GrB_Vector_setElement_FP64,
    GrB_Vector_extractElement_FP64,
    GrB_Vector_extractTuples_FP64,
    GrB_Matrix_build_FP64,
    GrB_Matrix_setElement_FP64,
    GrB_Matrix_extractTuples_FP64
);

// --- Operators -------------------------------------------------------------

/// Duplicate-handling binary operator used when building a collection from a list
/// of element triples that may name the same coordinate more than once.
#[derive(Clone, Copy, Debug)]
pub enum Reducer {
    /// Keep the first value (`GrB_FIRST_*`): used for boolean-union adjacency.
    First,
    /// Sum duplicate values (`GrB_PLUS_*`): used for parallel-edge weights.
    Plus,
}

impl Reducer {
    fn binop<T: GbType>(self) -> gb::GrB_BinaryOp {
        match self {
            Reducer::First => T::first_binop(),
            Reducer::Plus => T::plus_binop(),
        }
    }
}

/// Predefined semiring selector for `mxv`.
#[derive(Clone, Copy, Debug)]
pub enum Semiring {
    MinPlus,
    PlusTimes,
    MinSecond,
}

impl Semiring {
    fn resolve<T: GbType>(self) -> gb::GrB_Semiring {
        match self {
            Semiring::MinPlus => T::min_plus_semiring(),
            Semiring::PlusTimes => T::plus_times_semiring(),
            Semiring::MinSecond => T::min_second_semiring(),
        }
    }
}

/// Predefined monoid selector for element-wise vector addition.
#[derive(Clone, Copy, Debug)]
pub enum Monoid {
    Min,
    Plus,
}

impl Monoid {
    fn resolve<T: GbType>(self) -> gb::GrB_Monoid {
        match self {
            Monoid::Min => T::min_monoid(),
            Monoid::Plus => T::plus_monoid(),
        }
    }
}

/// Operation descriptor flags. Mirrors the four flags the engine sets:
/// replace output, treat the mask as structural, complement the mask, and
/// transpose the matrix (first input).
#[derive(Clone, Copy, Debug, Default)]
pub struct Descriptor {
    pub replace: bool,
    pub mask_structure: bool,
    pub mask_complement: bool,
    pub transpose_first: bool,
}

impl Descriptor {
    /// The null descriptor (all flags false): default GraphBLAS behavior.
    pub const NULL: Descriptor = Descriptor {
        replace: false,
        mask_structure: false,
        mask_complement: false,
        transpose_first: false,
    };

    pub fn new(
        replace: bool,
        mask_structure: bool,
        mask_complement: bool,
        transpose_first: bool,
    ) -> Self {
        Self {
            replace,
            mask_structure,
            mask_complement,
            transpose_first,
        }
    }

    /// Resolve to the matching predefined `GrB_DESC_*` object, or null when no
    /// flag is set. The name suffix order is R, S, C, T0, exactly as GraphBLAS
    /// names its predefined descriptors.
    fn resolve(self) -> gb::GrB_Descriptor {
        let key = (
            self.replace,
            self.mask_structure,
            self.mask_complement,
            self.transpose_first,
        );
        unsafe {
            match key {
                (false, false, false, false) => ptr::null_mut(),
                (false, false, false, true) => gb::GrB_DESC_T0,
                (false, false, true, false) => gb::GrB_DESC_C,
                (false, false, true, true) => gb::GrB_DESC_CT0,
                (false, true, false, false) => gb::GrB_DESC_S,
                (false, true, false, true) => gb::GrB_DESC_ST0,
                (false, true, true, false) => gb::GrB_DESC_SC,
                (false, true, true, true) => gb::GrB_DESC_SCT0,
                (true, false, false, false) => gb::GrB_DESC_R,
                (true, false, false, true) => gb::GrB_DESC_RT0,
                (true, false, true, false) => gb::GrB_DESC_RC,
                (true, false, true, true) => gb::GrB_DESC_RCT0,
                (true, true, false, false) => gb::GrB_DESC_RS,
                (true, true, false, true) => gb::GrB_DESC_RST0,
                (true, true, true, false) => gb::GrB_DESC_RSC,
                (true, true, true, true) => gb::GrB_DESC_RSCT0,
            }
        }
    }
}

// --- Vector ----------------------------------------------------------------

/// A typed GraphBLAS sparse vector.
pub struct Vector<T: GbType> {
    ptr: gb::GrB_Vector,
    _ctx: Arc<Context>,
    _t: PhantomData<T>,
}

// GraphBLAS objects are safe to move and share between threads; the engine
// serializes all mutation through the `Graph` write lock and LMDB, so no two
// threads mutate the same object concurrently.
unsafe impl<T: GbType> Send for Vector<T> {}
unsafe impl<T: GbType> Sync for Vector<T> {}

impl<T: GbType> Vector<T> {
    /// Create an empty vector of length `len`.
    pub fn new(ctx: Arc<Context>, len: usize) -> Result<Self> {
        let mut ptr: gb::GrB_Vector = ptr::null_mut();
        check(
            unsafe { gb::GrB_Vector_new(&mut ptr, T::gb_type(), len as u64) },
            "GrB_Vector_new",
        )?;
        Ok(Self {
            ptr,
            _ctx: ctx,
            _t: PhantomData,
        })
    }

    /// Build a vector of length `len` from `(index, value)` pairs, combining any
    /// duplicate indices with `dup`.
    pub fn from_pairs(
        ctx: Arc<Context>,
        len: usize,
        pairs: &[(usize, T)],
        dup: Reducer,
    ) -> Result<Self> {
        let v = Self::new(ctx, len)?;
        if !pairs.is_empty() {
            let indices: Vec<u64> = pairs.iter().map(|&(i, _)| i as u64).collect();
            let values: Vec<T> = pairs.iter().map(|&(_, x)| x).collect();
            check(
                unsafe {
                    T::vec_build(
                        v.ptr,
                        indices.as_ptr(),
                        values.as_ptr(),
                        pairs.len() as u64,
                        dup.binop::<T>(),
                    )
                },
                "GrB_Vector_build",
            )?;
        }
        Ok(v)
    }

    /// Set a single element.
    pub fn set(&mut self, index: usize, value: T) -> Result<()> {
        check(
            unsafe { T::vec_set(self.ptr, value, index as u64) },
            "GrB_Vector_setElement",
        )
    }

    /// Number of stored (explicit) elements.
    pub fn nvals(&self) -> Result<usize> {
        let mut n: u64 = 0;
        check(
            unsafe { gb::GrB_Vector_nvals(&mut n, self.ptr) },
            "GrB_Vector_nvals",
        )?;
        Ok(n as usize)
    }

    /// Indices of all stored elements, in ascending order.
    pub fn indices(&self) -> Result<Vec<usize>> {
        let mut n = self.nvals()? as u64;
        let mut indices = vec![0u64; n as usize];
        check(
            unsafe {
                T::vec_extract_tuples(indices.as_mut_ptr(), ptr::null_mut(), &mut n, self.ptr)
            },
            "GrB_Vector_extractTuples",
        )?;
        indices.truncate(n as usize);
        Ok(indices.into_iter().map(|i| i as usize).collect())
    }

    /// Value at `index`, or `T::default()` when no element is stored there.
    pub fn get_or_default(&self, index: usize) -> Result<T> {
        let mut value = T::default();
        let info = unsafe { T::vec_extract_element(&mut value, self.ptr, index as u64) };
        if info == gb::GrB_Info_GrB_NO_VALUE {
            Ok(T::default())
        } else {
            check(info, "GrB_Vector_extractElement")?;
            Ok(value)
        }
    }
}

impl<T: GbType> Drop for Vector<T> {
    fn drop(&mut self) {
        unsafe {
            let _ = gb::GrB_Vector_free(&mut self.ptr);
        }
    }
}

// --- Matrix ----------------------------------------------------------------

/// A typed GraphBLAS sparse matrix.
pub struct Matrix<T: GbType> {
    ptr: gb::GrB_Matrix,
    _ctx: Arc<Context>,
    _t: PhantomData<T>,
}

unsafe impl<T: GbType> Send for Matrix<T> {}
unsafe impl<T: GbType> Sync for Matrix<T> {}

impl<T: GbType> Matrix<T> {
    /// Create an empty `nrows` x `ncols` matrix.
    pub fn new(ctx: Arc<Context>, nrows: usize, ncols: usize) -> Result<Self> {
        let mut ptr: gb::GrB_Matrix = ptr::null_mut();
        check(
            unsafe { gb::GrB_Matrix_new(&mut ptr, T::gb_type(), nrows as u64, ncols as u64) },
            "GrB_Matrix_new",
        )?;
        Ok(Self {
            ptr,
            _ctx: ctx,
            _t: PhantomData,
        })
    }

    /// Build an `nrows` x `ncols` matrix from `(row, col, value)` triples,
    /// combining any duplicate coordinates with `dup`.
    pub fn from_triples(
        ctx: Arc<Context>,
        nrows: usize,
        ncols: usize,
        triples: &[(usize, usize, T)],
        dup: Reducer,
    ) -> Result<Self> {
        let m = Self::new(ctx, nrows, ncols)?;
        if !triples.is_empty() {
            let rows: Vec<u64> = triples.iter().map(|&(r, _, _)| r as u64).collect();
            let cols: Vec<u64> = triples.iter().map(|&(_, c, _)| c as u64).collect();
            let vals: Vec<T> = triples.iter().map(|&(_, _, v)| v).collect();
            check(
                unsafe {
                    T::mat_build(
                        m.ptr,
                        rows.as_ptr(),
                        cols.as_ptr(),
                        vals.as_ptr(),
                        triples.len() as u64,
                        dup.binop::<T>(),
                    )
                },
                "GrB_Matrix_build",
            )?;
        }
        Ok(m)
    }

    /// Resize to `nrows` x `ncols`.
    pub fn resize(&mut self, nrows: usize, ncols: usize) -> Result<()> {
        check(
            unsafe { gb::GrB_Matrix_resize(self.ptr, nrows as u64, ncols as u64) },
            "GrB_Matrix_resize",
        )
    }

    /// Set a single element.
    pub fn set(&mut self, row: usize, col: usize, value: T) -> Result<()> {
        check(
            unsafe { T::mat_set(self.ptr, value, row as u64, col as u64) },
            "GrB_Matrix_setElement",
        )
    }

    /// Drop a single element if present.
    pub fn drop_element(&mut self, row: usize, col: usize) -> Result<()> {
        check(
            unsafe { gb::GrB_Matrix_removeElement(self.ptr, row as u64, col as u64) },
            "GrB_Matrix_removeElement",
        )
    }

    /// Number of stored (explicit) elements.
    pub fn nvals(&self) -> Result<usize> {
        let mut n: u64 = 0;
        check(
            unsafe { gb::GrB_Matrix_nvals(&mut n, self.ptr) },
            "GrB_Matrix_nvals",
        )?;
        Ok(n as usize)
    }

    /// All stored elements as `(row, col, value)` triples.
    pub fn triples(&self) -> Result<Vec<(usize, usize, T)>> {
        let mut n = self.nvals()? as u64;
        let mut rows = vec![0u64; n as usize];
        let mut cols = vec![0u64; n as usize];
        let mut vals = vec![T::default(); n as usize];
        check(
            unsafe {
                T::mat_extract_tuples(
                    rows.as_mut_ptr(),
                    cols.as_mut_ptr(),
                    vals.as_mut_ptr(),
                    &mut n,
                    self.ptr,
                )
            },
            "GrB_Matrix_extractTuples",
        )?;
        let n = n as usize;
        Ok((0..n)
            .map(|k| (rows[k] as usize, cols[k] as usize, vals[k]))
            .collect())
    }
}

impl<T: GbType> Drop for Matrix<T> {
    fn drop(&mut self) {
        unsafe {
            let _ = gb::GrB_Matrix_free(&mut self.ptr);
        }
    }
}

// --- Operations ------------------------------------------------------------

/// `out<mask> = A *.semiring u` (matrix-by-vector multiply). The accumulator is
/// always assignment (no accumulation). Pass `None` for the mask to apply no mask.
pub fn mxv<T: GbType>(
    out: &mut Vector<T>,
    mask: Option<&Vector<T>>,
    semiring: Semiring,
    matrix: &Matrix<T>,
    vector: &Vector<T>,
    desc: Descriptor,
) -> Result<()> {
    let mask_ptr = mask.map(|m| m.ptr).unwrap_or(ptr::null_mut());
    check(
        unsafe {
            gb::GrB_mxv(
                out.ptr,
                mask_ptr,
                ptr::null_mut(),
                semiring.resolve::<T>(),
                matrix.ptr,
                vector.ptr,
                desc.resolve(),
            )
        },
        "GrB_mxv",
    )
}

/// `out<mask> = a (+monoid) b` element-wise (set union). The accumulator is always
/// assignment. Pass `None` for the mask to apply no mask.
pub fn ewise_add<T: GbType>(
    out: &mut Vector<T>,
    mask: Option<&Vector<T>>,
    monoid: Monoid,
    a: &Vector<T>,
    b: &Vector<T>,
    desc: Descriptor,
) -> Result<()> {
    let mask_ptr = mask.map(|m| m.ptr).unwrap_or(ptr::null_mut());
    check(
        unsafe {
            gb::GrB_Vector_eWiseAdd_Monoid(
                out.ptr,
                mask_ptr,
                ptr::null_mut(),
                monoid.resolve::<T>(),
                a.ptr,
                b.ptr,
                desc.resolve(),
            )
        },
        "GrB_Vector_eWiseAdd_Monoid",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bfs_one_step_min_plus() {
        // Path 0 -> 1 -> 2. One MinPlus step from a source vector at node 0 should
        // reach node 1 with distance 1.
        let ctx = Context::init_default().unwrap();
        let a =
            Matrix::<i32>::from_triples(ctx.clone(), 3, 3, &[(0, 1, 1), (1, 2, 1)], Reducer::First)
                .unwrap();
        let mut frontier = Vector::<i32>::new(ctx.clone(), 3).unwrap();
        frontier.set(0, 0).unwrap();
        let mut next = Vector::<i32>::new(ctx.clone(), 3).unwrap();
        // out = A^T *.(min,plus) frontier, transposing so columns feed rows.
        mxv(
            &mut next,
            None,
            Semiring::MinPlus,
            &a,
            &frontier,
            Descriptor::new(false, false, false, true),
        )
        .unwrap();
        assert_eq!(next.indices().unwrap(), vec![1]);
        assert_eq!(next.get_or_default(1).unwrap(), 1);
    }

    #[test]
    fn ewise_add_min_merges() {
        let ctx = Context::init_default().unwrap();
        let a =
            Vector::<i32>::from_pairs(ctx.clone(), 4, &[(0, 5), (1, 2)], Reducer::First).unwrap();
        let b =
            Vector::<i32>::from_pairs(ctx.clone(), 4, &[(1, 9), (3, 1)], Reducer::First).unwrap();
        let mut out = Vector::<i32>::new(ctx.clone(), 4).unwrap();
        ewise_add(&mut out, None, Monoid::Min, &a, &b, Descriptor::NULL).unwrap();
        assert_eq!(out.indices().unwrap(), vec![0, 1, 3]);
        assert_eq!(out.get_or_default(1).unwrap(), 2); // min(2, 9)
    }

    #[test]
    fn matrix_triples_round_trip() {
        let ctx = Context::init_default().unwrap();
        let m = Matrix::<f64>::from_triples(
            ctx.clone(),
            2,
            2,
            &[(0, 1, 2.5), (1, 0, 4.0)],
            Reducer::Plus,
        )
        .unwrap();
        let mut t = m.triples().unwrap();
        t.sort_by_key(|&(r, c, _)| (r, c));
        assert_eq!(t, vec![(0, 1, 2.5), (1, 0, 4.0)]);
    }
}

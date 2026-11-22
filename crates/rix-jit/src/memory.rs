//! W^X Executable Code-Page Allocator (`mmap` RW -> write -> `mprotect` RX -> `munmap` on `Drop`).
//!
//! Reference: Spec 05 §4.3 & Issue #19.
//! Every executable page is strictly Write-XOR-Execute: mapped `PROT_READ | PROT_WRITE`
//! during emission, transitioned to `PROT_READ | PROT_EXEC` before any entry pointer is
//! exposed, and deterministically reclaimed via `libc::munmap` when [`CodePage`] drops
//! (zero `Box::into_raw` memory leaks).

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use thiserror::Error;

/// Global diagnostic counters verifying every mapped code page is unmapped on drop.
static LIVE_CODE_PAGES: AtomicUsize = AtomicUsize::new(0);
static LIVE_CODE_BYTES: AtomicUsize = AtomicUsize::new(0);
static TOTAL_UNMAPPED_PAGES: AtomicU64 = AtomicU64::new(0);

/// Allocation or W^X protection failure in [`CodePage`].
#[derive(Debug, Error)]
pub enum CodeAllocError {
    #[error("Cannot allocate empty JIT code buffer")]
    EmptyBuffer,
    #[error("mmap failed with OS error {0}")]
    MmapFailed(i32),
    #[error("mprotect(PROT_READ | PROT_EXEC) failed with OS error {0}")]
    MprotectFailed(i32),
}

/// Returns the number of currently mapped executable JIT code pages in this process.
#[must_use]
pub fn live_code_pages() -> usize {
    LIVE_CODE_PAGES.load(Ordering::SeqCst)
}

/// Returns the total bytes currently mapped across all live [`CodePage`] allocations.
#[must_use]
pub fn live_code_bytes() -> usize {
    LIVE_CODE_BYTES.load(Ordering::SeqCst)
}

/// Returns the cumulative count of `munmap` reclamations performed on drop.
#[must_use]
pub fn total_unmapped_pages() -> u64 {
    TOTAL_UNMAPPED_PAGES.load(Ordering::SeqCst)
}

/// Owning handle to a page-aligned W^X executable machine-code buffer.
/// Automatically unmaps its OS pages via `libc::munmap` on [`Drop`].
pub struct CodePage {
    ptr: NonNull<u8>,
    mapped_len: usize,
    code_len: usize,
}

// SAFETY: Finalized `CodePage` memory is immutable RX machine code with no interior mutability.
unsafe impl Send for CodePage {}
// SAFETY: Concurrent reads/execution of immutable RX machine code across threads are safe.
unsafe impl Sync for CodePage {}

impl CodePage {
    /// Allocates page-aligned RW memory via `mmap`, copies `machine_code`, and flips
    /// protection strictly to RX (`PROT_READ | PROT_EXEC`) before returning.
    pub fn from_machine_code(machine_code: &[u8]) -> Result<Self, CodeAllocError> {
        if machine_code.is_empty() {
            return Err(CodeAllocError::EmptyBuffer);
        }
        let page_size = os_page_size();
        let mapped_len = machine_code.len().div_ceil(page_size) * page_size;

        // SAFETY: Anonymous private RW mapping with valid non-zero page-aligned `mapped_len`.
        // Null address lets the kernel choose a safe mapping address.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mapped_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED || raw.is_null() {
            let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
            return Err(CodeAllocError::MmapFailed(err));
        }

        let ptr = NonNull::new(raw.cast::<u8>()).ok_or(CodeAllocError::MmapFailed(-1))?;

        // SAFETY: `ptr` points to a fresh, exclusively owned, RW mapping of `mapped_len >= machine_code.len()`
        // bytes that cannot overlap `machine_code`.
        unsafe {
            std::ptr::copy_nonoverlapping(machine_code.as_ptr(), ptr.as_ptr(), machine_code.len());
        }

        // Flush instruction cache on non-coherent architectures (e.g. aarch64) before marking RX.
        flush_icache(ptr.as_ptr(), machine_code.len());

        // SAFETY: `ptr` is page-aligned and `mapped_len` is a multiple of the OS page size.
        // Removing `PROT_WRITE` and granting `PROT_EXEC` enforces strict W^X.
        let rc = unsafe {
            libc::mprotect(
                ptr.as_ptr().cast::<libc::c_void>(),
                mapped_len,
                libc::PROT_READ | libc::PROT_EXEC,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
            // SAFETY: Mapping succeeded above; clean up immediately so we never leak on error.
            unsafe {
                let _ = libc::munmap(ptr.as_ptr().cast::<libc::c_void>(), mapped_len);
            }
            return Err(CodeAllocError::MprotectFailed(err));
        }

        LIVE_CODE_PAGES.fetch_add(1, Ordering::SeqCst);
        LIVE_CODE_BYTES.fetch_add(mapped_len, Ordering::SeqCst);

        Ok(Self {
            ptr,
            mapped_len,
            code_len: machine_code.len(),
        })
    }

    /// Returns the raw executable entry address (valid for the lifetime of `self`).
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    #[inline]
    #[must_use]
    pub const fn code_len(&self) -> usize {
        self.code_len
    }

    #[inline]
    #[must_use]
    pub const fn mapped_len(&self) -> usize {
        self.mapped_len
    }
}

impl Drop for CodePage {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` and `self.mapped_len` came directly from `libc::mmap` in
        // `from_machine_code`, and `Drop` runs exactly once when no references remain.
        unsafe {
            let _ = libc::munmap(self.ptr.as_ptr().cast::<libc::c_void>(), self.mapped_len);
        }
        LIVE_CODE_PAGES.fetch_sub(1, Ordering::SeqCst);
        LIVE_CODE_BYTES.fetch_sub(self.mapped_len, Ordering::SeqCst);
        TOTAL_UNMAPPED_PAGES.fetch_add(1, Ordering::SeqCst);
    }
}

fn os_page_size() -> usize {
    // SAFETY: `sysconf(_SC_PAGESIZE)` is thread-safe and has no memory-safety preconditions.
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz <= 0 {
        4096
    } else {
        sz as usize
    }
}

#[cfg(target_arch = "aarch64")]
fn flush_icache(ptr: *const u8, len: usize) {
    unsafe extern "C" {
        fn __clear_cache(beg: *mut libc::c_void, end: *mut libc::c_void);
    }
    // SAFETY: `[ptr, ptr + len)` lies strictly within the mapped code page.
    unsafe {
        __clear_cache(ptr as *mut libc::c_void, ptr.add(len) as *mut libc::c_void);
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn flush_icache(_ptr: *const u8, _len: usize) {}

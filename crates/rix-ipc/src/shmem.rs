use crate::error::{IpcError, Result};
use std::ffi::CStr;
use std::io;
use std::mem::{align_of, size_of};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

pub const MAX_SHMEM_BLOB_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_TRIPLE_BUFFER_SLOT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_FDS_PER_MESSAGE: usize = 8;

const TRIPLE_BUFFER_MAGIC: u32 = 0x5249_5854; // "RIXT"
const TRIPLE_BUFFER_VERSION: u32 = 1;
const HEADER_SIZE_ALIGNED: usize = 64;
const SLOT_COUNT: usize = 3;
const SLOT_MASK: u8 = 0x03;
const DIRTY_BIT: u8 = 0x04;
const REQUIRED_BLOB_SEALS: libc::c_int =
    libc::F_SEAL_WRITE | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
const REQUIRED_TRIPLE_SEALS: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
const CMSG_BUF_BYTES: usize = 128;

#[repr(C)]
union AlignedCmsgBuf {
    _align: libc::cmsghdr,
    bytes: [u8; CMSG_BUF_BYTES],
}

#[derive(Debug)]
struct MappedRegion {
    ptr: NonNull<u8>,
    len: usize,
}

impl MappedRegion {
    fn map(fd: BorrowedFd<'_>, len: usize, prot: libc::c_int) -> Result<Self> {
        if len == 0 {
            return Err(IpcError::ShmemInvalidSize {
                size: 0,
                max: MAX_SHMEM_BLOB_BYTES,
            });
        }
        // SAFETY: `fd` is a valid open file descriptor borrowed for this call, `len` > 0, and
        // `MAP_SHARED` with null address lets the kernel choose a valid page-aligned mapping.
        let raw = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                prot,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error().into());
        }
        let ptr = NonNull::new(raw.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap succeeded but returned null pointer"))?;
        Ok(Self { ptr, len })
    }

    fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr().cast_const()
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for MappedRegion {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` and `self.len` were returned together by `libc::mmap` and remain
        // valid until this sole owner unmaps the region here.
        unsafe {
            let _ = libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

fn create_memfd(name: &CStr, size: usize) -> Result<OwnedFd> {
    let off = libc::off_t::try_from(size).map_err(|_| IpcError::ShmemInvalidSize {
        size,
        max: MAX_SHMEM_BLOB_BYTES,
    })?;
    // SAFETY: `name` is a valid NUL-terminated static C string and flags are standard memfd constants.
    let raw_fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: `raw_fd >= 0` is a newly created file descriptor owned exclusively by this function.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    // SAFETY: `fd` is an open memfd owned here and `off` fits in `libc::off_t`.
    let rc = unsafe { libc::ftruncate(fd.as_raw_fd(), off) };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(fd)
}

fn fstat_size(fd: BorrowedFd<'_>) -> Result<usize> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fd` is a valid borrowed descriptor and `st` points to writable stack storage.
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: `fstat` succeeded and initialized `st`.
    let st = unsafe { st.assume_init() };
    usize::try_from(st.st_size).map_err(|_| IpcError::ShmemInvalidSize {
        size: 0,
        max: MAX_SHMEM_BLOB_BYTES,
    })
}

fn get_seals(fd: BorrowedFd<'_>) -> Result<libc::c_int> {
    // SAFETY: `fd` is a valid borrowed file descriptor; `F_GET_SEALS` takes no extra args.
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(seals)
}

fn add_seals(fd: BorrowedFd<'_>, seals: libc::c_int) -> Result<()> {
    // SAFETY: `fd` is a valid borrowed memfd and `seals` is a bitmask of `F_SEAL_*` flags.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[derive(Debug)]
pub struct SealedShmemBlob {
    fd: OwnedFd,
    region: MappedRegion,
}

// SAFETY: Once constructed, a `SealedShmemBlob` has read-only `PROT_READ` mapping backed by a
// `F_SEAL_WRITE | F_SEAL_SHRINK | F_SEAL_GROW`-sealed memfd whose bytes are completely immutable.
unsafe impl Send for SealedShmemBlob {}
// SAFETY: Sharing `&SealedShmemBlob` only permits immutable `&[u8]` reads and `dup`ing the sealed FD.
unsafe impl Sync for SealedShmemBlob {}

impl SealedShmemBlob {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.is_empty() || data.len() > MAX_SHMEM_BLOB_BYTES {
            return Err(IpcError::ShmemInvalidSize {
                size: data.len(),
                max: MAX_SHMEM_BLOB_BYTES,
            });
        }

        let fd = create_memfd(c"rix-shmem", data.len())?;
        {
            let mut rw =
                MappedRegion::map(fd.as_fd(), data.len(), libc::PROT_READ | libc::PROT_WRITE)?;
            // SAFETY: `data.len()` matches both `data` and `rw.len`; regions are non-overlapping
            // because `rw` is a brand-new anonymous memfd mapping.
            unsafe {
                ptr::copy_nonoverlapping(data.as_ptr(), rw.as_mut_ptr(), data.len());
            }
        }
        // `rw` is unmapped before sealing so `F_SEAL_WRITE` succeeds cleanly on Linux.
        add_seals(fd.as_fd(), REQUIRED_BLOB_SEALS | libc::F_SEAL_SEAL)?;

        let region = MappedRegion::map(fd.as_fd(), data.len(), libc::PROT_READ)?;
        Ok(Self { fd, region })
    }

    pub fn from_owned_fd(fd: OwnedFd) -> Result<Self> {
        let seals = get_seals(fd.as_fd())
            .map_err(|_| IpcError::ShmemSealMissing("descriptor does not support memfd sealing"))?;
        if (seals & REQUIRED_BLOB_SEALS) != REQUIRED_BLOB_SEALS {
            return Err(IpcError::ShmemSealMissing(
                "sealed blob requires F_SEAL_WRITE | F_SEAL_SHRINK | F_SEAL_GROW",
            ));
        }

        let size = fstat_size(fd.as_fd())?;
        if size == 0 || size > MAX_SHMEM_BLOB_BYTES {
            return Err(IpcError::ShmemInvalidSize {
                size,
                max: MAX_SHMEM_BLOB_BYTES,
            });
        }

        let region = MappedRegion::map(fd.as_fd(), size, libc::PROT_READ)?;
        Ok(Self { fd, region })
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `self.region` maps `self.region.len` readable bytes sealed against mutation or shrinking.
        unsafe { std::slice::from_raw_parts(self.region.as_ptr(), self.region.len) }
    }

    pub fn len(&self) -> usize {
        self.region.len
    }

    pub fn is_empty(&self) -> bool {
        self.region.len == 0
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn try_clone_fd(&self) -> Result<OwnedFd> {
        Ok(self.fd.as_fd().try_clone_to_owned()?)
    }
}

#[repr(C, align(64))]
struct TripleBufferHeader {
    magic: u32,
    version: u32,
    slot_capacity: u32,
    _pad0: u32,
    shared_state: AtomicU8,
    _pad1: [u8; 3],
    slot_lengths: [AtomicU32; SLOT_COUNT],
    generation: AtomicU64,
}

const _: () = assert!(size_of::<TripleBufferHeader>() <= HEADER_SIZE_ALIGNED);

#[derive(Debug)]
pub struct TripleBufferShmem {
    fd: OwnedFd,
    region: MappedRegion,
    slot_capacity: usize,
    slot_stride: usize,
}

// SAFETY: `TripleBufferShmem` owns a shared-memory mapping and FD that are safe to transfer across
// threads before splitting into single-producer / single-consumer endpoints.
unsafe impl Send for TripleBufferShmem {}

impl TripleBufferShmem {
    pub fn create(slot_capacity: usize) -> Result<Self> {
        if slot_capacity == 0 || slot_capacity > MAX_TRIPLE_BUFFER_SLOT_BYTES {
            return Err(IpcError::ShmemInvalidSize {
                size: slot_capacity,
                max: MAX_TRIPLE_BUFFER_SLOT_BYTES,
            });
        }
        let slot_stride =
            slot_capacity
                .checked_next_multiple_of(64)
                .ok_or(IpcError::ShmemInvalidSize {
                    size: slot_capacity,
                    max: MAX_TRIPLE_BUFFER_SLOT_BYTES,
                })?;
        let total_len = slot_stride
            .checked_mul(SLOT_COUNT)
            .and_then(|s| s.checked_add(HEADER_SIZE_ALIGNED))
            .ok_or(IpcError::ShmemInvalidSize {
                size: slot_capacity,
                max: MAX_TRIPLE_BUFFER_SLOT_BYTES,
            })?;

        let fd = create_memfd(c"rix-triplebuf", total_len)?;
        add_seals(fd.as_fd(), REQUIRED_TRIPLE_SEALS)?;
        let mut region =
            MappedRegion::map(fd.as_fd(), total_len, libc::PROT_READ | libc::PROT_WRITE)?;

        if !(region.as_ptr() as usize).is_multiple_of(align_of::<TripleBufferHeader>()) {
            return Err(IpcError::ShmemCorruptHeader("unaligned mapping"));
        }

        let header = TripleBufferHeader {
            magic: TRIPLE_BUFFER_MAGIC,
            version: TRIPLE_BUFFER_VERSION,
            slot_capacity: u32::try_from(slot_capacity).map_err(|_| {
                IpcError::ShmemInvalidSize {
                    size: slot_capacity,
                    max: MAX_TRIPLE_BUFFER_SLOT_BYTES,
                }
            })?,
            _pad0: 0,
            // Producer starts owning slot 0, shared spare holds slot 1 (clean), consumer owns slot 2.
            shared_state: AtomicU8::new(1),
            _pad1: [0; 3],
            slot_lengths: [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)],
            generation: AtomicU64::new(0),
        };
        // SAFETY: `region` is freshly created, page-aligned, >= `HEADER_SIZE_ALIGNED` bytes, and unaliased.
        unsafe {
            ptr::write(region.as_mut_ptr().cast::<TripleBufferHeader>(), header);
        }

        Ok(Self {
            fd,
            region,
            slot_capacity,
            slot_stride,
        })
    }

    pub fn slot_capacity(&self) -> usize {
        self.slot_capacity
    }

    pub fn into_pair(self) -> Result<(TripleBufferProducer, TripleBufferConsumer)> {
        let consumer_fd = self.fd.as_fd().try_clone_to_owned()?;
        let producer = TripleBufferProducer::from_created(self);
        let consumer = TripleBufferConsumer::from_owned_fd(consumer_fd)?;
        Ok((producer, consumer))
    }
}

#[derive(Debug)]
pub struct TripleBufferProducer {
    fd: OwnedFd,
    region: MappedRegion,
    slot_capacity: usize,
    slot_stride: usize,
    back_idx: usize,
}

// SAFETY: `TripleBufferProducer` is `!Sync` and requires `&mut self` for writes; moving sole
// producer ownership between threads/tasks is sound via atomic synchronization in `TripleBufferHeader`.
unsafe impl Send for TripleBufferProducer {}

impl TripleBufferProducer {
    pub fn from_created(shmem: TripleBufferShmem) -> Self {
        Self {
            fd: shmem.fd,
            region: shmem.region,
            slot_capacity: shmem.slot_capacity,
            slot_stride: shmem.slot_stride,
            back_idx: 0,
        }
    }

    fn header(&self) -> &TripleBufferHeader {
        // SAFETY: `self.region` was validated/initialized with an aligned `TripleBufferHeader` at offset 0.
        unsafe { &*self.region.as_ptr().cast::<TripleBufferHeader>() }
    }

    fn slot_mut(&mut self, idx: usize) -> &mut [u8] {
        let offset = HEADER_SIZE_ALIGNED + idx * self.slot_stride;
        // SAFETY: `idx < 3` is checked or invariant-maintained and `offset + self.slot_capacity <= self.region.len`.
        // `back_idx` is exclusively owned by this single producer (`&mut self`).
        unsafe {
            std::slice::from_raw_parts_mut(self.region.as_mut_ptr().add(offset), self.slot_capacity)
        }
    }

    pub fn write_frame(&mut self, data: &[u8]) -> Result<u64> {
        if data.len() > self.slot_capacity {
            return Err(IpcError::FrameTooLarge {
                size: data.len(),
                max: self.slot_capacity,
            });
        }
        let len_u32 = u32::try_from(data.len()).map_err(|_| IpcError::FrameTooLarge {
            size: data.len(),
            max: self.slot_capacity,
        })?;

        let idx = self.back_idx.min(SLOT_COUNT - 1);
        if !data.is_empty() {
            self.slot_mut(idx)[..data.len()].copy_from_slice(data);
        }

        let hdr = self.header();
        hdr.slot_lengths[idx].store(len_u32, Ordering::Release);
        let generation = hdr
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let prev = hdr
            .shared_state
            .swap((idx as u8) | DIRTY_BIT, Ordering::AcqRel);

        let next = usize::from(prev & SLOT_MASK);
        if next < SLOT_COUNT {
            self.back_idx = next;
        }
        Ok(generation)
    }

    pub fn slot_capacity(&self) -> usize {
        self.slot_capacity
    }

    pub fn generation(&self) -> u64 {
        self.header().generation.load(Ordering::Relaxed)
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn try_clone_fd(&self) -> Result<OwnedFd> {
        Ok(self.fd.as_fd().try_clone_to_owned()?)
    }
}

#[derive(Debug)]
pub struct TripleBufferConsumer {
    _fd: OwnedFd,
    region: MappedRegion,
    slot_capacity: usize,
    slot_stride: usize,
    front_idx: usize,
    front_len: usize,
}

// SAFETY: `TripleBufferConsumer` is `!Sync` and requires `&mut self` to swap buffers; moving sole
// consumer ownership across threads/tasks is sound.
unsafe impl Send for TripleBufferConsumer {}

impl TripleBufferConsumer {
    pub fn from_owned_fd(fd: OwnedFd) -> Result<Self> {
        let seals = get_seals(fd.as_fd())
            .map_err(|_| IpcError::ShmemSealMissing("descriptor does not support memfd sealing"))?;
        if (seals & REQUIRED_TRIPLE_SEALS) != REQUIRED_TRIPLE_SEALS {
            return Err(IpcError::ShmemSealMissing(
                "triple buffer requires F_SEAL_SHRINK | F_SEAL_GROW",
            ));
        }

        let file_size = fstat_size(fd.as_fd())?;
        if file_size < HEADER_SIZE_ALIGNED {
            return Err(IpcError::ShmemCorruptHeader(
                "backing size smaller than header",
            ));
        }

        let region = MappedRegion::map(fd.as_fd(), file_size, libc::PROT_READ | libc::PROT_WRITE)?;
        if !(region.as_ptr() as usize).is_multiple_of(align_of::<TripleBufferHeader>()) {
            return Err(IpcError::ShmemCorruptHeader("unaligned mapping"));
        }

        // SAFETY: `region` maps at least `HEADER_SIZE_ALIGNED` page-aligned bytes, sealed against shrink.
        let (magic, version, raw_cap) = unsafe {
            let hdr = &*region.as_ptr().cast::<TripleBufferHeader>();
            (hdr.magic, hdr.version, hdr.slot_capacity as usize)
        };

        if magic != TRIPLE_BUFFER_MAGIC {
            return Err(IpcError::ShmemCorruptHeader("invalid magic"));
        }
        if version != TRIPLE_BUFFER_VERSION {
            return Err(IpcError::ShmemCorruptHeader("unsupported version"));
        }
        if raw_cap == 0 || raw_cap > MAX_TRIPLE_BUFFER_SLOT_BYTES {
            return Err(IpcError::ShmemCorruptHeader("slot capacity out of bounds"));
        }

        let slot_stride = raw_cap
            .checked_next_multiple_of(64)
            .ok_or(IpcError::ShmemCorruptHeader("slot stride overflow"))?;
        let expected_total = slot_stride
            .checked_mul(SLOT_COUNT)
            .and_then(|s| s.checked_add(HEADER_SIZE_ALIGNED))
            .ok_or(IpcError::ShmemCorruptHeader("total length overflow"))?;

        if expected_total > file_size {
            return Err(IpcError::ShmemCorruptHeader("backing file truncated"));
        }

        Ok(Self {
            _fd: fd,
            region,
            slot_capacity: raw_cap,
            slot_stride,
            front_idx: 2,
            front_len: 0,
        })
    }

    fn header(&self) -> &TripleBufferHeader {
        // SAFETY: Header alignment and bounds were verified in `from_owned_fd`.
        unsafe { &*self.region.as_ptr().cast::<TripleBufferHeader>() }
    }

    fn slot_slice(&self, idx: usize) -> &[u8] {
        let offset = HEADER_SIZE_ALIGNED + idx * self.slot_stride;
        // SAFETY: `idx < 3`, `offset + self.slot_capacity <= self.region.len`, and `front_idx`
        // is exclusively owned by this consumer until swapped away.
        unsafe { std::slice::from_raw_parts(self.region.as_ptr().add(offset), self.slot_capacity) }
    }

    pub fn try_acquire(&mut self) -> Option<&[u8]> {
        let (next_idx, next_len) = {
            let hdr = self.header();
            if (hdr.shared_state.load(Ordering::Acquire) & DIRTY_BIT) == 0 {
                return None;
            }
            let prev = hdr
                .shared_state
                .swap(self.front_idx as u8, Ordering::AcqRel);
            let candidate = usize::from(prev & SLOT_MASK);
            let next_idx = if candidate < SLOT_COUNT {
                candidate
            } else {
                self.front_idx
            };
            let next_len = (hdr.slot_lengths[next_idx].load(Ordering::Acquire) as usize)
                .min(self.slot_capacity);
            (next_idx, next_len)
        };
        self.front_idx = next_idx;
        self.front_len = next_len;
        Some(&self.slot_slice(self.front_idx)[..self.front_len])
    }

    pub fn current_frame(&self) -> &[u8] {
        &self.slot_slice(self.front_idx)[..self.front_len]
    }

    pub fn slot_capacity(&self) -> usize {
        self.slot_capacity
    }

    pub fn generation(&self) -> u64 {
        self.header().generation.load(Ordering::Relaxed)
    }
}

pub(crate) fn send_with_fds_raw(
    sock: BorrowedFd<'_>,
    data: &[u8],
    fds: &[BorrowedFd<'_>],
) -> io::Result<usize> {
    if data.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SCM_RIGHTS requires at least one payload byte",
        ));
    }
    if fds.len() > MAX_FDS_PER_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many file descriptors for single IPC message",
        ));
    }

    let mut iov = libc::iovec {
        iov_base: data.as_ptr().cast_mut().cast(),
        iov_len: data.len(),
    };

    // SAFETY: Zeroed `msghdr` initializes any platform padding fields safely before field setup.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;

    let mut cmsg_storage = AlignedCmsgBuf {
        bytes: [0u8; CMSG_BUF_BYTES],
    };

    if !fds.is_empty() {
        let fd_bytes = std::mem::size_of_val(fds);
        // SAFETY: `CMSG_SPACE` computes control-buffer length purely from byte size.
        let space = unsafe { libc::CMSG_SPACE(fd_bytes as libc::c_uint) } as usize;
        if space > CMSG_BUF_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control buffer too small for SCM_RIGHTS",
            ));
        }
        // SAFETY: Aligned `cmsg_storage` is set onto `msg` and initialized via `CMSG_FIRSTHDR`.
        unsafe {
            msg.msg_control = cmsg_storage.bytes.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                return Err(io::Error::other("CMSG_FIRSTHDR returned null"));
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as libc::c_uint) as _;

            let data_ptr = libc::CMSG_DATA(cmsg);
            for (idx, fd) in fds.iter().enumerate() {
                let raw = fd.as_raw_fd();
                ptr::write_unaligned(data_ptr.cast::<RawFd>().add(idx), raw);
            }
        }
    }

    // SAFETY: `sock` is a valid connected Unix socket FD and `msg` points to valid stack buffers.
    let sent = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(sent as usize)
    }
}

pub(crate) fn recv_with_fds_raw(
    sock: BorrowedFd<'_>,
    buf: &mut [u8],
) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cmsg_storage = AlignedCmsgBuf {
        bytes: [0u8; CMSG_BUF_BYTES],
    };

    // SAFETY: Zeroed `msghdr` with aligned control storage of size `CMSG_BUF_BYTES`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    // SAFETY: `cmsg_storage.bytes` is aligned to `cmsghdr` and owned exclusively on stack.
    unsafe {
        msg.msg_control = cmsg_storage.bytes.as_mut_ptr().cast();
        msg.msg_controllen = CMSG_BUF_BYTES as _;
    }

    // SAFETY: `sock` is a valid Unix socket FD and `iov`/`msg_control` are valid writable buffers.
    let received = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut fds = Vec::new();
    // SAFETY: Parse kernel-populated `cmsghdr` entries inside `msg.msg_control` and wrap every
    // received `RawFd` immediately in `OwnedFd` so all descriptors close cleanly even on error.
    unsafe {
        let base_len = libc::CMSG_LEN(0) as usize;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_bytes = ((*cmsg).cmsg_len as usize).saturating_sub(base_len);
                let fd_count = data_bytes / size_of::<RawFd>();
                let data_ptr = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for idx in 0..fd_count {
                    let raw = ptr::read_unaligned(data_ptr.add(idx));
                    if raw >= 0 {
                        fds.push(OwnedFd::from_raw_fd(raw));
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    if (msg.msg_flags & libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SCM_RIGHTS control message was truncated (MSG_CTRUNC)",
        ));
    }

    Ok((received as usize, fds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsealed_memfd_is_rejected_for_blob_and_triple_buffer() {
        let unsealed_blob_fd = create_memfd(c"unsealed-blob", 64).expect("memfd");
        assert!(matches!(
            SealedShmemBlob::from_owned_fd(unsealed_blob_fd),
            Err(IpcError::ShmemSealMissing(_))
        ));

        let unsealed_tb_fd = create_memfd(c"unsealed-tb", 4096).expect("memfd");
        assert!(matches!(
            TripleBufferConsumer::from_owned_fd(unsealed_tb_fd),
            Err(IpcError::ShmemSealMissing(_))
        ));
    }

    #[test]
    fn zero_or_oversized_shmem_requests_fail_cleanly() {
        assert!(matches!(
            SealedShmemBlob::from_bytes(&[]),
            Err(IpcError::ShmemInvalidSize { size: 0, .. })
        ));
        assert!(matches!(
            TripleBufferShmem::create(0),
            Err(IpcError::ShmemInvalidSize { size: 0, .. })
        ));
        assert!(matches!(
            TripleBufferShmem::create(MAX_TRIPLE_BUFFER_SLOT_BYTES + 1),
            Err(IpcError::ShmemInvalidSize { .. })
        ));
    }
}

#![cfg(unix)]

use rix_ipc::{
    unix_pair, CompositorToContent, ContentToCompositor, IpcError, PaintStats, PipelineId,
    ScrollTreeUpdate, SealedShmemBlob, ShmemId, ShmemRef, TripleBufferConsumer, TripleBufferShmem,
};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};

fn open_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("read /proc/self/fd")
        .count()
}

#[test]
fn sealed_blob_roundtrip_and_write_denied() {
    let payload: Vec<u8> = (0..=255).cycle().take(65_536).collect();
    let blob = SealedShmemBlob::from_bytes(&payload).expect("create sealed blob");
    assert_eq!(blob.len(), payload.len());
    assert!(!blob.is_empty());
    assert_eq!(blob.as_slice(), payload.as_slice());

    // Attempting to write to the cloned FD via a File handle must fail (F_SEAL_WRITE).
    let cloned = blob.try_clone_fd().expect("clone fd");
    let mut file = File::from(cloned);
    let err = file.write(b"tamper").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EPERM));
    assert_eq!(blob.as_slice(), payload.as_slice());
}

#[test]
fn unsealed_fd_is_rejected() {
    let mut tmp = tempfile::tempfile().expect("tempfile");
    tmp.write_all(b"unsealed payload").expect("write");
    let raw = OwnedFd::from(tmp);
    let err = SealedShmemBlob::from_owned_fd(raw).unwrap_err();
    assert!(matches!(err, IpcError::ShmemSealMissing(_)));
}

#[tokio::test]
async fn scm_rights_transfers_sealed_blob_and_triple_buffer_over_uds() {
    let (mut content, mut compositor) =
        unix_pair::<ContentToCompositor, CompositorToContent>().expect("pair");

    let payload = vec![0x5Au8; 128 * 1024];
    let blob = SealedShmemBlob::from_bytes(&payload).expect("blob");
    let frame_msg = ContentToCompositor::SubmitFrame {
        pipeline: PipelineId(7),
        epoch: 3,
        display_list: ShmemRef {
            id: ShmemId(1),
            offset: 0,
            len: payload.len() as u32,
        },
        scroll_tree: ScrollTreeUpdate::default(),
        stats: PaintStats::default(),
    };

    let (mut prod, _) = TripleBufferShmem::create(256)
        .expect("triple")
        .into_pair()
        .expect("pair");
    prod.write_frame(b"initial video frame").expect("write");
    let tb_fd = prod.try_clone_fd().expect("clone tb fd");
    let blob_fd = blob.try_clone_fd().expect("clone blob fd");

    content
        .send_with_fds(&frame_msg, &[blob_fd.as_fd(), tb_fd.as_fd()])
        .await
        .expect("send_with_fds");

    let (received_msg, mut fds) = compositor
        .recv_with_fds()
        .await
        .expect("recv_with_fds")
        .expect("some");
    assert_eq!(received_msg, frame_msg);
    assert_eq!(fds.len(), 2);

    let mut recv_tb =
        TripleBufferConsumer::from_owned_fd(fds.pop().expect("tb fd")).expect("consumer");
    let recv_blob =
        SealedShmemBlob::from_owned_fd(fds.pop().expect("blob fd")).expect("sealed blob");
    assert_eq!(recv_blob.as_slice(), payload.as_slice());
    assert_eq!(
        recv_tb.try_acquire(),
        Some(b"initial video frame".as_slice())
    );
}

#[test]
fn triple_buffer_10000_handoffs_stress_zero_torn_reads() {
    const SLOT_WORDS: usize = 512;
    const SLOT_BYTES: usize = SLOT_WORDS * 8;
    const FRAMES: u64 = 10_000;

    let shmem = TripleBufferShmem::create(SLOT_BYTES).expect("create triple buffer");
    let (mut producer, mut consumer) = shmem.into_pair().expect("split pair");
    assert_eq!(consumer.current_frame(), &[] as &[u8]);

    let prod_handle = std::thread::spawn(move || {
        let mut frame = vec![0u8; SLOT_BYTES];
        for seq in 1..=FRAMES {
            for chunk in frame.as_chunks_mut::<8>().0 {
                chunk.copy_from_slice(&seq.to_le_bytes());
            }
            let gen = producer.write_frame(&frame).expect("write_frame");
            assert_eq!(gen, seq);
        }
    });

    let mut last_seq = 0u64;
    let mut observed_updates = 0u64;
    while last_seq < FRAMES {
        if let Some(frame) = consumer.try_acquire() {
            assert_eq!(frame.len(), SLOT_BYTES);
            let expected_seq = u64::from_le_bytes(frame[0..8].try_into().expect("8 bytes"));
            assert!(
                expected_seq > last_seq,
                "seq must strictly advance: {expected_seq} <= {last_seq}"
            );
            for chunk in frame.as_chunks::<8>().0 {
                let word = u64::from_le_bytes(*chunk);
                assert_eq!(word, expected_seq, "torn read detected!");
            }
            last_seq = expected_seq;
            observed_updates += 1;
        } else {
            std::thread::yield_now();
        }
    }

    prod_handle.join().expect("producer joined");
    assert_eq!(last_seq, FRAMES);
    assert!(observed_updates >= 1);
    assert_eq!(consumer.generation(), FRAMES);
}

#[test]
fn no_fd_leaks_across_500_cycles() {
    use std::os::fd::AsRawFd;

    // Run 500 cycles (2,000+ FD lifetimes). Even a 1-FD leak per cycle drives `peak_raw_fd`
    // up by >= 500, catching leaks reliably even with sibling tests running concurrently.
    let first_raw_fd = {
        let blob = SealedShmemBlob::from_bytes(b"warmup").expect("warmup blob");
        blob.as_fd().as_raw_fd()
    };

    let before = open_fd_count();
    let mut peak_raw_fd = first_raw_fd;

    for i in 0..500usize {
        let byte = (i % 251) as u8;
        let data = vec![byte; 4096];
        let blob = SealedShmemBlob::from_bytes(&data).expect("blob");
        peak_raw_fd = peak_raw_fd.max(blob.as_fd().as_raw_fd());

        let cloned = blob.try_clone_fd().expect("clone blob");
        peak_raw_fd = peak_raw_fd.max(cloned.as_raw_fd());
        let reopened = SealedShmemBlob::from_owned_fd(cloned).expect("reopen");
        assert_eq!(reopened.as_slice(), data.as_slice());

        let tb = TripleBufferShmem::create(512).expect("tb");
        let (mut prod, mut cons) = tb.into_pair().expect("pair");
        peak_raw_fd = peak_raw_fd.max(prod.as_fd().as_raw_fd());
        prod.write_frame(&[byte; 64]).expect("write");
        assert_eq!(cons.try_acquire(), Some([byte; 64].as_slice()));
    }

    let after = open_fd_count();
    assert!(
        peak_raw_fd - first_raw_fd < 64,
        "FD numbers drifted across 500 cycles: first={first_raw_fd}, peak={peak_raw_fd}"
    );
    assert!(
        after <= before + 16,
        "open FD count grew unexpectedly: before={before}, after={after}"
    );
}

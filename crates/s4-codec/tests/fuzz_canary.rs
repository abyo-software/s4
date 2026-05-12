//! Fuzz CI canary — fuzz infrastructure 自体が壊れていないことを証明する。
//!
//! ## なぜ必要か
//!
//! ユーザの不安: 「fuzz で何かあれば CI が fail する? 本当に?」。
//! → silently skipped な test が混入して fuzz が動いていない状態を防ぐ。
//!
//! このファイルの test は **必ず数回 invariant を assert する**。proptest
//! framework が起動しなければ test 自体が「実行されていない」状態になり、
//! 開発者が気づく。
//!
//! ## 含む test の意図
//!
//! 1. `canary_proptest_does_run`: proptest が確実に実行されたことを print で示す
//! 2. `canary_known_invariant_holds`: 既知の真な不変式 (write_frame の出力長 =
//!    header + payload) を 1024 cases × proptest で確認。これが落ちたら
//!    `write_frame` 実装が破壊された signal
//! 3. `canary_zstd_bomb_protection_active`: cpu_zstd の bomb hardening が
//!    enabled であることを explicit に verify。hardening を誰かが revert したら
//!    fuzz canary が fail する

use bytes::{Bytes, BytesMut};
use proptest::prelude::*;
use s4_codec::cpu_zstd::CpuZstd;
use s4_codec::multipart::{FRAME_HEADER_BYTES, FrameHeader, write_frame};
use s4_codec::{ChunkManifest, Codec, CodecKind};

#[test]
fn canary_proptest_does_run() {
    // proptest が起動したか可視化する。AtomicUsize で closure 越しに count。
    use std::sync::atomic::{AtomicUsize, Ordering};
    let counter = AtomicUsize::new(0);
    proptest!(|(_x in 0u8..255)| {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let n = counter.load(Ordering::Relaxed);
    assert!(n >= 100, "proptest must execute many cases, got {n}");
    eprintln!("CANARY: proptest executed {n} cases");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// `write_frame` の出力 byte 数 = `FRAME_HEADER_BYTES + payload.len()` 不変式。
    /// もし誰かが header size を変えたり padding を混入したらこれが落ちる。
    /// = "fuzz infrastructure が動いていれば必ず caught される" canary。
    #[test]
    fn canary_known_invariant_holds(
        payload in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let mut buf = BytesMut::new();
        write_frame(
            &mut buf,
            FrameHeader {
                codec: CodecKind::CpuZstd,
                original_size: 0,
                compressed_size: payload.len() as u64,
                crc32c: 0,
            },
            &payload,
        );
        prop_assert_eq!(buf.len(), FRAME_HEADER_BYTES + payload.len());
    }
}

#[tokio::test]
async fn canary_zstd_bomb_protection_active() {
    // cpu_zstd の bomb hardening が有効であることを直接検証。
    // hardening (Decoder + take(limit)) が剥がれると下記 assert が落ちる。
    let codec = CpuZstd::default();
    // 1 KB の実データ → zstd 圧縮
    let real = vec![b'x'; 1024];
    let compressed = zstd::stream::encode_all(real.as_slice(), 3).unwrap();
    // manifest が嘘 (10 GB と主張) → bomb scenario
    let bomb_manifest = ChunkManifest {
        codec: CodecKind::CpuZstd,
        original_size: 10_000_000_000,
        compressed_size: compressed.len() as u64,
        crc32c: 0,
    };
    let result = codec
        .decompress(Bytes::from(compressed), &bomb_manifest)
        .await;
    assert!(
        result.is_err(),
        "bomb hardening must reject mismatched manifest, OOM-protect server"
    );
}

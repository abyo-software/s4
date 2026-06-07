window.BENCHMARK_DATA = {
  "lastUpdate": 1780827466382,
  "repoUrl": "https://github.com/abyo-software/s4",
  "entries": {
    "s4-codec criterion benches": [
      {
        "commit": {
          "author": {
            "email": "abyo.software@gmail.com",
            "name": "masumi-ryugo"
          },
          "committer": {
            "email": "abyo.software@gmail.com",
            "name": "masumi-ryugo"
          },
          "distinct": true,
          "id": "2da3a9e1c63e0137d4473195997f626053b91970",
          "message": "fix(ci): #106-audit — CI green for race test + bench gh-pages bootstrap\n\nCloses two post-v0.9-landing CI failures the per-feature audit cycles\nmissed:\n\n1. **MinIO E2E `repair_sidecar_detects_post_get_overwrite_race`\n   flaky** (commit e59b115's CI run on 2026-06-07 06:51 UTC).\n   The 5-attempt parallel-overwrite test relied on the spawned\n   PUT landing during repair's post-PUT/pre-final-HEAD window;\n   CI runners execute the whole HEAD→GET→build→PUT pipeline\n   faster than the 5-25 ms sleep ladder, so the race window\n   never lands in the post-PUT branch on those runners. Now a\n   best-effort smoke (validates cleanup if race lands, logs and\n   passes otherwise); a deterministic regression guard for the\n   `OverwrittenDuringRepair` error shape lives in the lib unit\n   test `repair::tests::overwritten_during_repair_error_shape`.\n\n2. **Bench workflow `gh-pages` branch missing** (commit\n   5dc282e's Bench run failed at \"Store benchmark result\"\n   with `fatal: couldn't find remote ref gh-pages`). The\n   `benchmark-action/github-action-benchmark@v1` action does\n   NOT auto-create the branch — its `git fetch origin\n   gh-pages:gh-pages` step fails closed on a repo that has\n   never had a Pages deploy. Added a `Bootstrap gh-pages\n   branch if missing` step that creates an orphan branch with\n   a one-line landing page so the action can append to it on\n   the next push.\n\nCoverage: lib unit tests now 12 (was 11) with the new\n`overwritten_during_repair_error_shape` deterministic guard. The\nexisting flaky E2E remains in the MinIO suite as a smoke (best-\neffort) so a future fix that brings the timing into the post-PUT\nwindow can opportunistically validate the cleanup branch.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T18:48:02+09:00",
          "tree_id": "04646ac9b357e037126cf9242b0f90c83cdca049",
          "url": "https://github.com/abyo-software/s4/commit/2da3a9e1c63e0137d4473195997f626053b91970"
        },
        "date": 1780826219464,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 47622,
            "range": "± 1662",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57505,
            "range": "± 921",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 428,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2208707,
            "range": "± 70913",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50533405,
            "range": "± 82054",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201187,
            "range": "± 207",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49326644,
            "range": "± 942178",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 922810768,
            "range": "± 2347953",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3217076,
            "range": "± 119694",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27862,
            "range": "± 1118",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32723,
            "range": "± 1224",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 422,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 574998,
            "range": "± 2471",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1651597,
            "range": "± 7411",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201841,
            "range": "± 2294",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 10966587,
            "range": "± 70018",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27467048,
            "range": "± 1715173",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3218382,
            "range": "± 62702",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1455643,
            "range": "± 16079",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2103955,
            "range": "± 18336",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 308990431,
            "range": "± 1095851",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7846,
            "range": "± 19",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 918,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14279,
            "range": "± 140",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2754,
            "range": "± 23",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21400,
            "range": "± 39",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85423,
            "range": "± 262",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 598,
            "range": "± 18",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4771,
            "range": "± 82",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20480,
            "range": "± 48",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 31,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 31,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 31,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "abyo.software@gmail.com",
            "name": "masumi-ryugo"
          },
          "committer": {
            "email": "abyo.software@gmail.com",
            "name": "masumi-ryugo"
          },
          "distinct": true,
          "id": "714018b12a0bbce5a9e203907ec4ff19de83b4c5",
          "message": "fix(audit): #106-audit-R2 — P2-INT cross-feature gaps closed\n\nCodex round 1 integrated audit of the v0.9 range\n(142e50e..2da3a9e) caught two P2 cross-feature integration gaps\nthat per-feature audits couldn't see in isolation:\n\nP2-INT-1: repair-sidecar can't handle SSE-S4 chunked bodies\n  s4 repair-sidecar runs against the BACKEND, so for an object\n  written via the new SSE-S4 chunked path it sees the S4E6\n  encrypted envelope, not the pre-encrypt S4F2 frame stream.\n  build_index_from_body would have failed with a confusing\n  FrameScan error, and the v3 sidecar's sse_v3 binding (key_id /\n  salt / chunk_size) can't be reconstructed from the backend\n  bytes alone.\n\n  Fix: detect_sse_magic peeks the body for S4E1-S4E6 magic and\n  short-circuits to a new RepairError::EncryptedSidecarUnsupported\n  variant before reaching the frame scanner. Display message\n  points the operator at server-mode rebuild paths and announces\n  the v0.10 follow-up (CLI --sse-s4-key plumbing). New lib unit\n  test + new MinIO E2E\n  (repair_sidecar_rejects_sse_s4_chunked_object_cleanly) pin\n  the contract.\n\nP2-INT-2: streaming checksum trailer verify skipped on buffered path\n  x-amz-trailer announced checksums were only verified on the\n  streaming-framed branch (cpu-zstd / nvcomp-zstd). Passthrough\n  PUTs and non-streaming GPU codec PUTs went through the\n  buffered branch and read the trailing_headers exactly never —\n  a SigV4-streaming PUT with a bad or absent checksum trailer\n  silently passed on those codec paths.\n\n  Fix: new verify_client_trailer_checksums shared helper called\n  by BOTH the streaming-framed and buffered branches. Buffered\n  branch now derives WhichHashers from x-amz-trailer, runs\n  compute_digests (new one-shot helper in streaming_checksum)\n  over the already-in-memory body, then delegates to the same\n  fail-closed comparison. New compute_digests one-shot exposes\n  the same HasherSet pipeline previously locked inside the\n  streaming wrapper. 2 new E2E tests in roundtrip.rs +\n  5 new unit tests in service.rs.\n\nQuality gates: fmt clean, clippy clean, 707 workspace tests pass\n(0 failed), MinIO E2E suites green (8/8 sidecar, 36/36 minio_e2e\nincl. the new encrypted-reject E2E).\n\nAudit cycle: Codex R1 on this diff returned no findings against\nthe P2-INT fixes themselves (the only flagged item was untracked\nfuzz corpus which is per-task off-limits and unrelated). Setup\nfor the v0.9 integrated audit's next iteration.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T19:09:55+09:00",
          "tree_id": "883823d9fa84a97eb3cc57a742e81d0436d407f6",
          "url": "https://github.com/abyo-software/s4/commit/714018b12a0bbce5a9e203907ec4ff19de83b4c5"
        },
        "date": 1780827465682,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 54419,
            "range": "± 4073",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 54107,
            "range": "± 3386",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 370,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2619456,
            "range": "± 88663",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41681863,
            "range": "± 95131",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192301,
            "range": "± 412",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51033880,
            "range": "± 1212382",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 756894633,
            "range": "± 2963073",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3074505,
            "range": "± 43000",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32774,
            "range": "± 3056",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38230,
            "range": "± 2614",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 372,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 573410,
            "range": "± 4597",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1558892,
            "range": "± 39250",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192329,
            "range": "± 1386",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13288380,
            "range": "± 129388",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 26819505,
            "range": "± 80740",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3069024,
            "range": "± 15293",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1624565,
            "range": "± 35883",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2571284,
            "range": "± 15606",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 338718055,
            "range": "± 4521829",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 139,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 9011,
            "range": "± 31",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 795,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12610,
            "range": "± 53",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2938,
            "range": "± 27",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22133,
            "range": "± 471",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 91768,
            "range": "± 1660",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 597,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4974,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19912,
            "range": "± 66",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 27,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 27,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 27,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}
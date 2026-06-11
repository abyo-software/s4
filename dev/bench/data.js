window.BENCHMARK_DATA = {
  "lastUpdate": 1781165826382,
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
          "id": "bee3e2e3ba9483250716c95802beab5085958962",
          "message": "fix(audit): self-review — extend encrypted-body guard to classify_missing_sidecar\n\nPost-R2 self-review caught that the P2-INT-1 fix (encrypted-body\ndetection) was only wired into `repair_sidecar`. `verify_sidecar`'s\nsidecar-missing branch routes to `classify_missing_sidecar`, which\nalso fetches the main body and runs `build_index_from_body` — same\nencrypted-bytes hazard. SSE-S4 chunked objects pre-v0.9 (no v3\nsidecar emitted yet) trip this path on first `s4 verify-sidecar`\npost-upgrade.\n\nAdded the same `detect_sse_magic` short-circuit before the frame\nscan; verify-sidecar now surfaces `EncryptedSidecarUnsupported`\nwith the same operator-guidance Display as repair-sidecar instead\nof a confusing `FrameScan` error.\n\nNo new lib test needed: the existing\n`detect_sse_magic_covers_all_envelope_variants` already pins the\nmagic detection, and the `repair_sidecar_rejects_sse_s4_chunked_*`\nE2E exercises the same shared `detect_sse_magic` helper from the\nverify path indirectly via `classify_missing_sidecar` reuse.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T19:13:20+09:00",
          "tree_id": "cf78b756468349994b0c9b92dd860fda353c4535",
          "url": "https://github.com/abyo-software/s4/commit/bee3e2e3ba9483250716c95802beab5085958962"
        },
        "date": 1780827678670,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48272,
            "range": "± 1932",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57938,
            "range": "± 2709",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 426,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2213886,
            "range": "± 21281",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50575061,
            "range": "± 160181",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201423,
            "range": "± 1107",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 50055706,
            "range": "± 1196335",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 922286824,
            "range": "± 2798635",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3222358,
            "range": "± 7276",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 26326,
            "range": "± 1376",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32316,
            "range": "± 890",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 423,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 576533,
            "range": "± 3180",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1650257,
            "range": "± 10643",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201233,
            "range": "± 751",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 11206746,
            "range": "± 57419",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27690109,
            "range": "± 69542",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3244075,
            "range": "± 10249",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1488161,
            "range": "± 17560",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2138214,
            "range": "± 112049",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 324937580,
            "range": "± 5440567",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 10536,
            "range": "± 15",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 892,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13892,
            "range": "± 52",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2757,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21356,
            "range": "± 376",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85208,
            "range": "± 608",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 597,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4729,
            "range": "± 20",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19327,
            "range": "± 111",
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
            "range": "± 0",
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
          "id": "76f0c110e0fbf0a77ea95ca000e0d8c65ef03d2b",
          "message": "fix(audit): #106-audit-R3 P2-R3 — reject NotFramed before writing sidecar\n\nCodex round 3 integrated audit caught: `s4 repair-sidecar` on an\nobject whose body has no S4F2 frames (passthrough / empty / very\nshort bodies) used to silently write an empty `<key>.s4index`\nbecause `build_index_from_body` returns `Ok(FrameIndex { entries: [],\n.. })` rather than an error for those bodies. The empty sidecar\nthen broke Range GET on that key: `FrameIndex::lookup_range` over\nzero entries returns `None`, and the GET path took the \"no plan\"\nbranch instead of the passthrough-range fallback that exists for\nsidecar-less objects.\n\nFix: add an `idx.entries.is_empty()` guard right after\n`build_index_from_body` returns in `repair_sidecar`. Rejects with\nnew `RepairError::NotFramed { bucket, key }` whose Display tells\nthe operator the object isn't a sidecar-repair candidate (and\n`verify-sidecar` separately classifies it as `MissingHarmless`\nwith `frame_count = 0`, which IS the correct verdict — passthrough\nobjects intentionally have no sidecar).\n\nTests:\n- Lib unit `not_framed_error_shape` pins the variant's wire shape\n  + Display (catches refactor renames at compile time)\n- MinIO E2E `repair_sidecar_rejects_zero_frame_body` plants an\n  empty body (the exact case `build_index_from_body` returns Ok\n  with zero entries) AND a non-trivial raw-bytes body (which trips\n  the inner BadMagic / FrameScan path); proves BOTH paths reject\n  cleanly without writing a sidecar\n\nCoverage: lib unit tests now 15 (was 14). Workspace 0 failed.\nfmt + clippy clean.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T19:48:43+09:00",
          "tree_id": "275474102d07e996478fd2b19a35ebdc78221ddc",
          "url": "https://github.com/abyo-software/s4/commit/76f0c110e0fbf0a77ea95ca000e0d8c65ef03d2b"
        },
        "date": 1780829809234,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48276,
            "range": "± 1694",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57353,
            "range": "± 1597",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 428,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2199801,
            "range": "± 24739",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50710077,
            "range": "± 126442",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201793,
            "range": "± 2656",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49046070,
            "range": "± 504645",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 925168395,
            "range": "± 5183646",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3218412,
            "range": "± 10512",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 26996,
            "range": "± 1045",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 31886,
            "range": "± 975",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 422,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 575323,
            "range": "± 3520",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1654277,
            "range": "± 16453",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201090,
            "range": "± 265",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12837047,
            "range": "± 330745",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 29468087,
            "range": "± 199047",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3337962,
            "range": "± 70438",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1467992,
            "range": "± 61350",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2104590,
            "range": "± 73530",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 355185939,
            "range": "± 5389871",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 9796,
            "range": "± 26",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 920,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14280,
            "range": "± 175",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2756,
            "range": "± 29",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21422,
            "range": "± 58",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 86320,
            "range": "± 806",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 647,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5363,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20951,
            "range": "± 261",
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
            "range": "± 0",
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
          "id": "1e05404902dd90fb79dce403d34ceaa8cdf41785",
          "message": "fix(audit): #106-audit-R4 P2-R4 — verify-sidecar MissingHarmless on non-framed bodies\n\nCodex round 4 integrated audit caught the verify-side twin of R3\nP2-R3: `s4 verify-sidecar` on a passthrough / raw-bytes object\n(no S4F2 magic, body long enough to clear the 28-byte FRAME_HEADER\nprobe) used to exit 1 with a confusing `FrameScan` error. The\nserver never sidecars those objects by design, so absence of a\nsidecar is the correct steady state — CI / cron jobs would\nfalse-alert on healthy passthrough workloads.\n\nFix: in `classify_missing_sidecar`, catch the `FrameError::BadMagic`\nvariant from `build_index_from_body` and surface\n`MissingHarmless { frame_count: 0 }` (exit 0) instead of bubbling\nthe error. Non-BadMagic FrameScan errors still propagate so genuine\ncorruption surfaces loud (e.g. half-written multipart with a partial\nframe, attacker-supplied forged header).\n\nE2E: `verify_sidecar_reports_missing_harmless_for_non_framed_body`\nplants raw bytes directly via the backend, asserts MissingHarmless\n+ is_clean. Sibling to the R3 `repair_sidecar_rejects_zero_frame_body`\ntest — both prove the verify / repair paths handle non-framed\nbodies with the right shape for their respective semantics\n(verify = clean, repair = NotFramed reject).\n\nCoverage: workspace 0 failed. fmt + clippy clean.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T19:56:16+09:00",
          "tree_id": "1ea711177c1de073276e7de85805b413669a4ae7",
          "url": "https://github.com/abyo-software/s4/commit/1e05404902dd90fb79dce403d34ceaa8cdf41785"
        },
        "date": 1780830249601,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 54622,
            "range": "± 3631",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57493,
            "range": "± 3590",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 373,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2689250,
            "range": "± 68303",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41748271,
            "range": "± 152020",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192569,
            "range": "± 615",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 54650147,
            "range": "± 1093890",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 754861461,
            "range": "± 1431635",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3078032,
            "range": "± 12768",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 33377,
            "range": "± 3031",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38338,
            "range": "± 2675",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 378,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 571388,
            "range": "± 9796",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1554004,
            "range": "± 30606",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192553,
            "range": "± 512",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13434966,
            "range": "± 376700",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27292497,
            "range": "± 136271",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3084679,
            "range": "± 18846",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1534264,
            "range": "± 23509",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2658056,
            "range": "± 18870",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 353559178,
            "range": "± 3496437",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 141,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7736,
            "range": "± 70",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 827,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13074,
            "range": "± 31",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2912,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22700,
            "range": "± 231",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90782,
            "range": "± 1275",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 599,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4958,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19738,
            "range": "± 45",
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
          "id": "d078a45eb282f1385e8ed012876d63fcd0790bd0",
          "message": "fix(audit): #106-audit-R5 P2-R5 — bounded sidecar fetch (OOM hardening)\n\nCodex round 5 integrated audit caught: `s4 verify-sidecar` and\n`sweep-orphan-sidecars` used to do an unbounded GET of every\n`<key>.s4index` body before `decode_index` could reject it. A\nmulti-GiB corrupt sidecar or legacy reserved-name user object\n(the v0.8.17 `--allow-legacy-reserved-key-reads` migration\nscenario) could OOM the operator's repair process — same DoS\nshape the codec already defends against on the server side via\nMAX_FRAMES / MAX_ETAG_BYTES.\n\nNew bounded `get_sidecar_bytes_capped` helper HEADs the sidecar\nfirst to learn its size; refuses to GET if Content-Length exceeds\n`MAX_SIDECAR_BODY_BYTES = 600 MiB`. The cap is comfortably above\nthe codec spec's max legitimate sidecar (MAX_FRAMES (16M) *\nENTRY_BYTES (32) + header = ~512 MiB) and well below attacker\npayload sizes.\n\nWired into both call sites:\n  - verify_sidecar: surfaces new typed\n    `RepairError::SidecarTooLarge { bucket, key, size, cap }`\n    (Display explains the threat model — OOM / legacy user data\n    / attack payload)\n  - classify_one (sweep): surfaces oversized entries as\n    `SidecarUndecodable` with a size-explaining message, so\n    one bad sidecar doesn't abort the whole sweep\n\nDead-code removal: the old `get_object_bytes` helper + its\n`GetOutcome` enum are no longer referenced after the cap fix\n(repair_sidecar uses the If-Match GET directly; classify_one and\nverify_sidecar now route through the capped helper).\n\nTests:\n  - Lib: `sidecar_too_large_error_shape` pins the variant\n    Display + field shape;\n    `max_sidecar_body_bytes_cap_value_pinned` derives the spec\n    max dynamically from `s4_codec::index::{MAX_FRAMES,\n    ENTRY_BYTES, HEADER_FIXED_V2, MAX_ETAG_BYTES}` and asserts\n    the cap exceeds it (any codec-side bump that pushes the\n    spec ceiling past 600 MiB will surface here loudly)\n  - MinIO E2E:\n    `sweep_classifies_oversized_lookalike_sidecar_as_undecodable`\n    walks the sweep path with a 1 MiB lookalike (full 600 MiB+\n    exercise too slow for CI; the cap value itself is pinned\n    by the lib unit test)\n\nCoverage: lib unit tests now 17 (was 15). Workspace 0 failed.\nfmt + clippy clean.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T20:05:10+09:00",
          "tree_id": "1a04c2f028765dfe7befe37722b08f06948cd708",
          "url": "https://github.com/abyo-software/s4/commit/d078a45eb282f1385e8ed012876d63fcd0790bd0"
        },
        "date": 1780830787104,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 47836,
            "range": "± 2044",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 58017,
            "range": "± 1104",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 427,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2215744,
            "range": "± 13749",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50510255,
            "range": "± 833319",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201188,
            "range": "± 366",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49924099,
            "range": "± 1651003",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 922053211,
            "range": "± 12394473",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3222430,
            "range": "± 5115",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27478,
            "range": "± 1980",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32775,
            "range": "± 1184",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 423,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 589039,
            "range": "± 2703",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1646733,
            "range": "± 10096",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201273,
            "range": "± 1362",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12277551,
            "range": "± 133307",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28829209,
            "range": "± 109972",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3220909,
            "range": "± 20479",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1472757,
            "range": "± 30819",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2119277,
            "range": "± 68907",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 313722269,
            "range": "± 2527772",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 141,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8528,
            "range": "± 21",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 919,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14293,
            "range": "± 32",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2749,
            "range": "± 80",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21378,
            "range": "± 38",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85285,
            "range": "± 146",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 684,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4876,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19127,
            "range": "± 160",
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
            "range": "± 0",
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
          "id": "472b28ecf378f5f8ca731ddb4266e0fddb4e20f1",
          "message": "fix(audit): #106-audit-R6 P2-R6 — bound sidecar GET (HEAD→GET TOCTOU)\n\nCodex round 6 integrated audit caught: the R5 P2-R5 cap fix\nHEADed first and GETed second, leaving a TOCTOU window where a\nsidecar swap between the two could bypass the cap. Race shape:\nHEAD(small) → swap-in(massive) → GET would still let `collect()`\npull the full new body into memory before the post-decode reject\ncould fire.\n\nFix: pin the GET to the HEAD ETag via `If-Match` so any swap\nsurfaces as 412 PreconditionFailed before any bytes are read.\nPlus a defense-in-depth post-GET length check that catches\nraces on ETag-less backends OR If-Match-non-honouring backends.\n\nRace detection paths:\n  - 412 → typed `SidecarFetchOutcome::Other` with a re-run hint\n  - Post-GET length > cap → `SidecarTooLarge` (same surface as\n    the HEAD-time rejection so callers branch uniformly)\n\nZero new tests — the existing `sweep_classifies_oversized_*` E2E\nexercises the happy capped path; the race itself is hard to\ndeterministically reproduce in a MinIO E2E (would require an\ninterposing proxy that mutates the body between HEAD and GET).\nThe lib unit `sidecar_too_large_error_shape` already pins the\ntyped error surface that the defense-in-depth post-GET branch\nemits, so any refactor that silently drops either guard fails\nloud either at the cap-value test or at the Display assertion.\n\nCoverage: workspace 0 failed. fmt + clippy clean.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T20:11:37+09:00",
          "tree_id": "3794eef48fc145158c864cc582c519455c1da537",
          "url": "https://github.com/abyo-software/s4/commit/472b28ecf378f5f8ca731ddb4266e0fddb4e20f1"
        },
        "date": 1780831172329,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 57356,
            "range": "± 4054",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 54083,
            "range": "± 3846",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 373,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2631434,
            "range": "± 56743",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41911550,
            "range": "± 147440",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192743,
            "range": "± 351",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 53850312,
            "range": "± 996966",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 755877498,
            "range": "± 1669375",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3073923,
            "range": "± 13796",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32045,
            "range": "± 2292",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38960,
            "range": "± 3275",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 376,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 573778,
            "range": "± 12410",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1545271,
            "range": "± 27157",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192501,
            "range": "± 1602",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12386614,
            "range": "± 180956",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27374161,
            "range": "± 130682",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3084784,
            "range": "± 15530",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1606064,
            "range": "± 15982",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2548915,
            "range": "± 23361",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 350797519,
            "range": "± 3564581",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 140,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8389,
            "range": "± 38",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 818,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13155,
            "range": "± 28",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2910,
            "range": "± 58",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22706,
            "range": "± 97",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90911,
            "range": "± 1075",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 597,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4606,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 18086,
            "range": "± 52",
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
            "value": 28,
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
          "id": "a1dfe2016138258365aa33dfff2275f671d7cb90",
          "message": "chore(release): cut v0.9.0 — six-feature roadmap + 7-round integrated audit\n\nWorkspace version 0.8.22 → 0.9.0. Internal dep specs bumped on\ns4-server (s4-codec/s4-config 0.8 → 0.9), s4-codec-py (0.8 → 0.9),\ns4-codec-wasm (0.8.10 → 0.9). CHANGELOG `[Unreleased]` → `[0.9.0]\n— 2026-06-07` with a top-level summary block + the six-feature\nAdded entries + seven Fixed entries from the integrated audit.\n\nSix v0.9 roadmap items shipped in this release line:\n  - eb8a9f2 #106 sidecar verify/repair/sweep CLI\n  - 50e4d68 criterion regression-tracking bench + GHA gh-pages\n  - 061654e i686 cross-compile support across the workspace\n  - f056143 chaos infrastructure (5 deterministic scenarios)\n  - e59b115 tee-into-hasher streaming PUT checksum verify\n  - 5dc282e v3 sidecar (SSE-S4 chunked Range GET partial-fetch)\n\nPlus the integrated-audit closeout fixes:\n  - 2da3a9e CI green for race test + bench gh-pages bootstrap\n  - 714018b P2-INT-1 (encrypted-sidecar reject) + P2-INT-2\n    (buffered trailer verify)\n  - bee3e2e verify-side encrypted-body guard (twin of P2-INT-1)\n  - 76f0c11 P2-R3 NotFramed reject for non-S4F2 bodies\n  - 1e05404 P2-R4 verify-side MissingHarmless for non-framed\n  - d078a45 P2-R5 sidecar OOM cap (MAX_SIDECAR_BODY_BYTES)\n  - 472b28e P2-R6 sidecar fetch HEAD→GET TOCTOU close\n\nAudit posture: 6 per-feature audits + 7-round integrated audit\non the v0.9 range (clean bill of health on R7). Zero P1\nfindings across the entire 11+7 = 18 Codex rounds.\n\nPre-release verification:\n  - cargo fmt --check clean\n  - cargo clippy --workspace --all-targets -- -D warnings clean\n  - cargo test --workspace: 0 failed across all test binaries\n  - cargo publish --dry-run: s4-codec + s4-config pass\n    (s4-server dry-run hits expected dep-skew because s4-codec\n    0.9.0 is not yet on crates.io; resolved at publish time\n    by ordering s4-codec → s4-config → s4-server)\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T20:29:16+09:00",
          "tree_id": "58374925c61526752a8f06bf6fe4d60ef886b5b8",
          "url": "https://github.com/abyo-software/s4/commit/a1dfe2016138258365aa33dfff2275f671d7cb90"
        },
        "date": 1780832197813,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 54538,
            "range": "± 5607",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 43483,
            "range": "± 2693",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 386,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2179966,
            "range": "± 39491",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 28656117,
            "range": "± 458781",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 152115,
            "range": "± 1573",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 50324495,
            "range": "± 447443",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 507444191,
            "range": "± 535023",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2445992,
            "range": "± 6262",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 24459,
            "range": "± 1680",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 29770,
            "range": "± 2105",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 393,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 532876,
            "range": "± 5422",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1402438,
            "range": "± 62520",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 152179,
            "range": "± 323",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12947071,
            "range": "± 295386",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 26176038,
            "range": "± 202600",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2454624,
            "range": "± 41730",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1340338,
            "range": "± 25572",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2088612,
            "range": "± 22379",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 398811246,
            "range": "± 5809517",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 118,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 5849,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 787,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12278,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2231,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 17205,
            "range": "± 15",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 68634,
            "range": "± 89",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 553,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4467,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19508,
            "range": "± 36",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 30,
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
          "id": "b0523192e4e7f3e53b52907f9523cb83e9032165",
          "message": "feat(v0.10): #A1 + #B1 + #A2-doc — encryption-aware sidecar completion + Docker publish + AEAD constraint docs\n\nThree parallel agents landed the v0.10 wave-1 themes in one\nbatch (file scope was strictly partitioned so the work composes\ncleanly):\n\n**A1 — SSE-S4 keyring CLI plumbing for `repair-sidecar`**\n  New `--sse-s4-key <PATH>` + `--sse-s4-key-rotated id=N,key=PATH`\n  flags on `s4 repair-sidecar` (matches the server-mode shape).\n  New lib entry point `s4_server::repair::repair_sidecar_with_keyring`;\n  existing `repair_sidecar` preserved as a None-keyring shim. When\n  the backend body is an S4E6 envelope AND a keyring is supplied,\n  the repair path decrypts in-process via `decrypt_chunked_buffered`,\n  frame-scans the recovered plaintext, and stamps a v3 sidecar so\n  subsequent Range GETs hit the encryption-aware partial-fetch fast\n  path. New `RepairError::SseDecryptFailed` for keyring mismatches;\n  refreshed `EncryptedSidecarUnsupported` message. Hardened against\n  attacker-controlled S4E6 header inflation via\n  `SSE_S4_REPAIR_MAX_OVERHEAD_BYTES` + `SSE_S4_REPAIR_MAX_CHUNK_SLACK_BYTES`\n  caps. 3 new E2E + 4 new unit tests. 14/14 MinIO E2E pass.\n\n**B1 — ghcr.io container image publishing**\n  New `.github/workflows/docker.yml` builds + pushes\n  `ghcr.io/abyo-software/s4:<version>` (CPU, multi-arch\n  amd64+arm64) and `ghcr.io/abyo-software/s4:<version>-gpu`\n  (nvCOMP GPU, amd64) on every `v*.*.*` tag push, plus\n  workflow_dispatch for back-filling pre-workflow tags. SLSA\n  provenance (mode=max) + SPDX SBOM + OCI labels. GHA Buildx\n  cache. GITHUB_TOKEN auth, no PAT. Public ghcr package (no\n  pull secrets). Mutable tags (`latest`, `<major>.<minor>`)\n  push-only AND non-prerelease-only so back-fills / RC pushes\n  can't move stable refs backward. Dockerfile gains `wget` for\n  HEALTHCHECK. Helm chart bumps to 0.2.0 / appVersion 0.9.0,\n  default `image.repository` flipped from the never-published\n  `docker.io/abyosoftware/s4` to `ghcr.io/abyo-software/s4`.\n  README §\"Kubernetes (Helm)\" rewritten with ghcr install\n  example. docker-compose.{,gpu}.yml gain `image:` alongside\n  `build:`.\n\n**A2-doc — SSE partial-fetch AEAD constraint clarification**\n  New `docs/security/sse-partial-fetch-constraint.md` (252\n  lines) walks the AEAD authenticated-encryption contract (NIST\n  SP 800-38D §7.2 quoted), per-mode wire layout, why S4E6\n  alone escapes the constraint (per-chunk nonce+tag), provisional\n  S4E7/S4E8 roadmap candidates for chunked KMS/SSE-C, and a\n  4-condition operator checklist. threat-model.md §2 row +\n  §3 #3 rewritten in AEAD framing — \"deferred plumbing\"\n  wording removed; now explicit that S4E2/E3/E4 can't partial-\n  decrypt by algorithm contract, not implementation gap. README\n  §\"Server-side encryption — Range GET fast-path matrix\" new\n  subsection with the 5-row support matrix + operator guidance.\n\nCoordination: CHANGELOG `[Unreleased]` was pre-partitioned into\n`### Added` / `### Documentation` / `### Fixed` so each agent\nappended to its own subsection — zero merge conflicts.\n\nAudit posture: each agent ran its own Codex review loop to clean.\nA1 = 5 rounds (4 P2 fixed + 1 clean), B1 = 4 rounds (5 P2 fixed +\n1 clean), A2-doc = 1 round clean. Integrated audit pending; will\nrun after this lands.\n\nCoverage: 14 MinIO E2E pass on the sidecar suite (3 new A1 tests\n+ 11 existing). Lib unit tests in repair module now 21 (was 17,\n+4 A1 unit). workspace tests 0 failed. fmt + clippy clean.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T22:40:42+09:00",
          "tree_id": "e8e0f6b2e7fbad4997190410465353f79374901a",
          "url": "https://github.com/abyo-software/s4/commit/b0523192e4e7f3e53b52907f9523cb83e9032165"
        },
        "date": 1780840114430,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 58546,
            "range": "± 4705",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 55821,
            "range": "± 3552",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 371,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2697476,
            "range": "± 160326",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41860204,
            "range": "± 569436",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192828,
            "range": "± 421",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51931240,
            "range": "± 769957",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 754904536,
            "range": "± 4281833",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3070032,
            "range": "± 5194",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32345,
            "range": "± 3101",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38879,
            "range": "± 3167",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 377,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 573517,
            "range": "± 22686",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1560794,
            "range": "± 29692",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192576,
            "range": "± 432",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13929175,
            "range": "± 99288",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27048318,
            "range": "± 177034",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3083223,
            "range": "± 9564",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1600662,
            "range": "± 20834",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2659624,
            "range": "± 24574",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 351140044,
            "range": "± 6806882",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 140,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 6812,
            "range": "± 141",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 816,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13120,
            "range": "± 25",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2915,
            "range": "± 89",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 20811,
            "range": "± 672",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 83534,
            "range": "± 3022",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 622,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4491,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 17961,
            "range": "± 44",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 27,
            "range": "± 1",
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
          "id": "4b64e43de33890ecffab42a89df25c11314e1b59",
          "message": "fix(ci): v0.10 race test must accept NotFramed as race outcome\n\nCI failure on commit b052319 (v0.10 wave-1): the race E2E\n`repair_sidecar_detects_post_get_overwrite_race` panic'd because\nthe parallel overwrite landed BEFORE repair's initial HEAD on\nfast CI runners. Repair then saw a raw-bytes body (the spawned\nPUT was `b\"overwritten attempt 0\"`, no S4F2 magic) and\ncorrectly rejected with `RepairError::NotFramed` (added by\nv0.9 audit R3 P2-R3).\n\nThe race test enumerates accepted \"race detected at some layer\"\noutcomes — already covered `Backend` (If-Match 412) and the\npost-PUT-HEAD `OverwrittenDuringRepair` path. NotFramed is the\nthird valid race outcome: overwrite landed before HEAD = body\nis raw = repair rejects = race detector at the earliest layer.\nAccept as `hit_get_race` retry rather than panicking.\n\nThe R3 NotFramed branch shipped in v0.9 audit but the existing\nrace test was written before R3 so its `Err(other) =>\npanic!(...)` arm covered it accidentally. CI surfaced the\noversight on the first fast-runner schedule where overwrite\nbeat HEAD.\n\nLocal repro of the fix: `cargo test -p s4-server --test\nsidecar_repair_via_minio --release repair_sidecar_detects_post_get_overwrite_race\n-- --ignored --test-threads=1` passes.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T22:49:12+09:00",
          "tree_id": "dbff984f934a9eddcc98ecfc528049fecdf91b12",
          "url": "https://github.com/abyo-software/s4/commit/4b64e43de33890ecffab42a89df25c11314e1b59"
        },
        "date": 1780840633268,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48993,
            "range": "± 1748",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 58115,
            "range": "± 2846",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 428,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2224120,
            "range": "± 33825",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50612737,
            "range": "± 172054",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201626,
            "range": "± 510",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 48583022,
            "range": "± 233419",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 924129610,
            "range": "± 1986377",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3216130,
            "range": "± 8892",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27106,
            "range": "± 1069",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32938,
            "range": "± 895",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 419,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 573876,
            "range": "± 5352",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1646994,
            "range": "± 15404",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201470,
            "range": "± 972",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12247804,
            "range": "± 79086",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28559786,
            "range": "± 263775",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3220896,
            "range": "± 18529",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1462804,
            "range": "± 18153",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2266477,
            "range": "± 19347",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 313814254,
            "range": "± 2643578",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 9418,
            "range": "± 189",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 886,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13755,
            "range": "± 34",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2759,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21411,
            "range": "± 48",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85556,
            "range": "± 218",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 632,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5286,
            "range": "± 21",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20819,
            "range": "± 101",
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
            "range": "± 0",
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
          "id": "58a5bb0a2d97b3c86c47347342d8ac9743f78ed7",
          "message": "fix(docker): add back-fill inputs + apply R4 prerelease guard\n\nTwo small `docker.yml` follow-ups the v0.10 #B1 audit covered in\nnarrative but didn't actually land in code:\n\n1. **R4 prerelease guard implementation** — the comment block\n   above the `{{major}}.{{minor}}` tag claimed `!contains(github.ref,\n   '-')` was applied, but the actual `enable=` condition only\n   gated on `github.event_name == 'push'`. A SemVer prerelease tag\n   push (`v0.10.0-rc1`) would have moved `0.10` to the rc. Now\n   actually gated.\n\n2. **Workflow back-fill inputs** — `gh workflow run docker.yml\n   --ref v0.9.0` fails with HTTP 422 because the workflow file\n   doesn't exist at the v0.9.0 tag. New optional inputs:\n\n   - `build_ref`: git ref to check out for the Dockerfile + binary\n     context (`actions/checkout@v4` `ref:`). Lets the dispatcher\n     run from `main` (where the workflow exists) while building\n     a different tag's source.\n   - `image_tag_override`: emits raw `<override>` + `v<override>`\n     tags so the immutable tag pair lands on ghcr.io. Necessary\n     when `build_ref` is a tag because `github.ref` (= dispatcher\n     branch) doesn't match SemVer patterns.\n\nBack-fill invocation now:\n  gh workflow run docker.yml --ref main \\\n    -f build_ref=v0.9.0 \\\n    -f image_tag_override=0.9.0 \\\n    -f push=true\n\nMutable tags (`latest`, `{major}.{minor}`) stay push-only so a\nback-fill never moves them backward.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T22:52:57+09:00",
          "tree_id": "c6ba56d1823abe7779b10db5619274844040ab40",
          "url": "https://github.com/abyo-software/s4/commit/58a5bb0a2d97b3c86c47347342d8ac9743f78ed7"
        },
        "date": 1780840819999,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 57073,
            "range": "± 5382",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 45678,
            "range": "± 3420",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 391,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2198479,
            "range": "± 43792",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 28631927,
            "range": "± 75183",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 152302,
            "range": "± 1669",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 50172812,
            "range": "± 257735",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 506006595,
            "range": "± 931909",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2451565,
            "range": "± 18183",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 26018,
            "range": "± 1859",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 31838,
            "range": "± 1823",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 394,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 529503,
            "range": "± 5723",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1394636,
            "range": "± 21807",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 152274,
            "range": "± 191",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13778949,
            "range": "± 268210",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 26493700,
            "range": "± 127670",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2472806,
            "range": "± 11976",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1360962,
            "range": "± 17235",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2228672,
            "range": "± 14034",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 399586448,
            "range": "± 3988927",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 118,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 5859,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 801,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12432,
            "range": "± 67",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2234,
            "range": "± 30",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 17215,
            "range": "± 34",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 68717,
            "range": "± 129",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 561,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4483,
            "range": "± 12",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19155,
            "range": "± 49",
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
            "range": "± 0",
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
          "id": "f9853ada6e86efbdf8b1b8cff4143e8b899b5fb3",
          "message": "feat(v0.10-A4): #A4 — i686 runtime smoke CI job\n\nCloses v0.10 wave-2 #A4. The v0.9 #106-32bit work proved\n`cargo check --target i686-unknown-linux-gnu` passes across the\nworkspace, but README qualified it as \"compiles, untested at\nruntime\" because no CI step actually exercised the i686 binary.\n\nNew per-push `i686-runtime-smoke` job in `.github/workflows/ci.yml`:\n\n  1. Installs `gcc-multilib` + `libc6-dev-i386` + `libc6:i386`\n     so a stock ubuntu-latest runner can produce + execute\n     i686 ELF binaries.\n  2. Runs `cargo test --target i686-unknown-linux-gnu -p s4-codec\n     -p s4-config --release` (full codec/config test suite —\n     regression gate for the v0.9 const-overflow + `usize::try_from`\n     hardening work).\n  3. Builds `s4-server` for i686 with `continue-on-error: true`\n     (the aws-sdk-rust / rustls / ring stack pulls C-source\n     crypto crates that may not link cleanly under stock i386\n     multilib; a build failure surfaces in the log without\n     going red because it doesn't invalidate the codec/config\n     test results in step 2).\n  4. When step 3 succeeded, invokes `s4 --help` + `s4 --version`\n     against the i686 binary to confirm it loads + initialises\n     CLI parsing on a 32-bit ELF.\n\n`notify-on-failure` job now depends on `i686-runtime-smoke` too,\nso an actual regression on the always-required steps (codec/config\ntests) auto-files a `ci-failure` issue like the other CI gates.\n\nREADME §\"Supported targets\" upgraded: `s4-server` cell flips from\n\"⚠️ compiles, untested at runtime\" to \"✅ compiles + `--help` /\n`--version` smoke (CI)\". Caveat preserved: full end-to-end\nPUT/GET on 32-bit is still not exercised; operators on i686\nshould treat `--max-body-bytes` carefully (auto-clamps to\n`isize::MAX as usize` ≈ 2 GiB per the v0.9 #106-32bit fix).\n\nContext: 3 wave-2 sub-agents (A3 / A4 / B2) hit Anthropic\nsession limit before producing any working-tree changes. This\ncommit picks up A4 in the main session so wave-2 progresses\nwhile the agent-quota resets at 00:30 JST. A3 + B2 to follow.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T23:25:42+09:00",
          "tree_id": "f619adda769b390474e94ff05772101e32b5386b",
          "url": "https://github.com/abyo-software/s4/commit/f9853ada6e86efbdf8b1b8cff4143e8b899b5fb3"
        },
        "date": 1780842815976,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 56179,
            "range": "± 3653",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57615,
            "range": "± 3350",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 371,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2674435,
            "range": "± 44862",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41758697,
            "range": "± 112604",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192244,
            "range": "± 395",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52082222,
            "range": "± 1006535",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 755460485,
            "range": "± 1761224",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3110172,
            "range": "± 21269",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32528,
            "range": "± 2589",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38983,
            "range": "± 2454",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 378,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 574134,
            "range": "± 12211",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1555965,
            "range": "± 31128",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192296,
            "range": "± 432",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13211108,
            "range": "± 339590",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27218796,
            "range": "± 161780",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3075544,
            "range": "± 11277",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1600265,
            "range": "± 23016",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2545148,
            "range": "± 39211",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 345631394,
            "range": "± 4065724",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 140,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 9247,
            "range": "± 43",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 815,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13098,
            "range": "± 30",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2798,
            "range": "± 87",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21866,
            "range": "± 214",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 87301,
            "range": "± 2632",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 595,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4955,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19743,
            "range": "± 68",
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
          "id": "5aa5c391e95b7a7dd1233602730684ad5c034d16",
          "message": "feat(v0.10-B2): #B2 — Docker / Helm distribution smoke CI\n\nCloses v0.10 wave-2 #B2. v0.10 #B1 (wave-1) added the\nghcr.io publish workflow + flipped the Helm chart default to the\nnew image repo, but no CI step actually exercised the published\noutput. New `.github/workflows/docker-smoke.yml` validates the\ndistribution surface on every push that touches it (path-filtered\nto `charts/**`, `Dockerfile*`, `docker-compose*.yml`, plus the\ndocker / docker-smoke workflow files themselves):\n\n  - `helm-lint-template`: `helm lint` + three `helm template`\n    runs (default, `image.tag=0.9.0` pinned, `0.9.0-gpu`\n    variant) against `./charts/s4` with a placeholder\n    `backend.endpointUrl`. Asserts the rendered manifest\n    references the expected ghcr repo / tag per variant.\n  - `docker-compose-config`: `docker compose config` on both\n    compose files + grep for the `ghcr.io/abyo-software/s4`\n    image refs the wave-1 #B1 work added (catches a regression\n    that silently drops `image:` and forces consumers back to\n    `build:`-only mode).\n  - `image-smoke`: pulls `ghcr.io/abyo-software/s4:latest`\n    (overrideable via `workflow_dispatch -f image_tag=...`),\n    runs `s4 --help` + `s4 --version`. `continue-on-error: true`\n    on pull tolerates the not-yet-published case (before v0.10.0\n    cut) — chart + compose jobs still gate.\n\nWorkflow is NOT in `notify-on-failure`'s `needs:` by design —\ndistribution regressions are advisory during the v0.10\ndistribution-ramp; they surface in the run UI without\nauto-filing issues that would be noisy.\n\nREADME §\"Kubernetes (Helm)\" gains a \"Verifying the image /\nchart locally\" subsection mirroring the CI checks for\noperators who want to reproduce them pre-deploy.\n\nLocal sanity (run on this commit before push):\n  - `helm lint ./charts/s4 --set backend.endpointUrl=...`: clean\n  - `helm template ... --set image.tag=0.9.0`: 1 match for\n    `ghcr.io/abyo-software/s4:0.9.0` (= deployment image ref)\n  - `docker compose config` on both compose files: no errors\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T23:28:36+09:00",
          "tree_id": "325bb9ce28c406bc2ea33a57ff16640c96167342",
          "url": "https://github.com/abyo-software/s4/commit/5aa5c391e95b7a7dd1233602730684ad5c034d16"
        },
        "date": 1780843002523,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48305,
            "range": "± 2099",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57751,
            "range": "± 883",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 428,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2225254,
            "range": "± 84058",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50685545,
            "range": "± 151528",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201411,
            "range": "± 10544",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49229018,
            "range": "± 1241884",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 923613708,
            "range": "± 6131192",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3215816,
            "range": "± 15520",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 28269,
            "range": "± 4234",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32325,
            "range": "± 982",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 419,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 575527,
            "range": "± 2076",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1641750,
            "range": "± 7199",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201341,
            "range": "± 728",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12315314,
            "range": "± 90760",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28925438,
            "range": "± 1899288",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3222364,
            "range": "± 32566",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1477710,
            "range": "± 39126",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2111604,
            "range": "± 21997",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 328027130,
            "range": "± 8841415",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8756,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 910,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14174,
            "range": "± 293",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2750,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21346,
            "range": "± 115",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85166,
            "range": "± 146",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 631,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5287,
            "range": "± 31",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 21140,
            "range": "± 45",
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
            "range": "± 0",
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
          "id": "3da7b7ec71d97602866e869aa51bd364cf7a2ee5",
          "message": "docs(v0.10-A3): #A3-doc — streaming PUT checksum coverage matrix\n\nCloses v0.10 wave-2 #A3. Evaluation outcome:\n\nQ1: multipart upload_part streaming verify?\n  → NO. `Codec::compress_with_telemetry(bytes, codec_kind)` in\n  `service.rs::upload_part` takes `bytes: Bytes` by value because\n  (a) the dispatcher needs a sample for codec selection, (b) the\n  codec needs the full body for encode, (c) `pad_to_minimum`\n  needs the framed length to decide padding-skip. Teeing the\n  body through a hasher first doesn't change memory peak.\n\nQ2: non-streaming GPU codec branch streaming verify?\n  → NO. `nvcomp-bitcomp` / `nvcomp-gdeflate` need the full body\n  in one buffer to copy to GPU HBM. Same shape as the multipart\n  case — tee doesn't help.\n\nBoth gaps are codec-API constraints (codec trait takes `Bytes`,\nnot `Stream<Bytes>`), not implementation oversights. Closing\nthem requires new wire format + codec re-architecture, which\nis v0.11+ scope, not v0.10.\n\nApproach: same \"doc-only with explicit constraint walkthrough\"\npattern as wave-1 #A2-doc on the SSE side. New\n`docs/security/streaming-checksum-coverage.md` (~80 lines)\ndocuments:\n\n  - 5-row coverage matrix (single-PUT cpu-zstd/nvcomp-zstd\n    streaming, single-PUT passthrough buffered, single-PUT\n    non-streaming GPU codec buffered, multipart upload_part\n    buffered).\n  - Three preconditions for streaming win + which paths meet how\n    many (only single-PUT streaming-codec meets all three).\n  - Where each path lives in `s4-server` (link to\n    streaming_checksum.rs + service.rs anchors).\n  - v0.11+ roadmap candidates (`S4F3` streaming frame, streaming\n    nvCOMP wrappers, multipart streaming upload_part) with the\n    upstream API constraints that block each.\n\nREADME §\"Streaming I/O\" `**Streaming PUT**` bullet gains a link\nto the dedicated doc. CHANGELOG entry under `### Documentation`.\n\nNo code changes. No test additions (existing v0.9\n#streaming-checksum tests already cover the streaming path;\nbuffered paths are exercised by the existing checksum E2E suite).\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T23:31:19+09:00",
          "tree_id": "e862e423d1ba70cd385ed548507a02db95e8bbe6",
          "url": "https://github.com/abyo-software/s4/commit/3da7b7ec71d97602866e869aa51bd364cf7a2ee5"
        },
        "date": 1780843160530,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48887,
            "range": "± 1715",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57835,
            "range": "± 2604",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 426,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2217414,
            "range": "± 87110",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50540225,
            "range": "± 121109",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201580,
            "range": "± 1489",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49073287,
            "range": "± 236357",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 921742545,
            "range": "± 2827021",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3218339,
            "range": "± 48482",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27996,
            "range": "± 1531",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32843,
            "range": "± 862",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 411,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 575186,
            "range": "± 4416",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1650280,
            "range": "± 16995",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201065,
            "range": "± 2731",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12507549,
            "range": "± 74319",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28859779,
            "range": "± 100633",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3221896,
            "range": "± 36388",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1474586,
            "range": "± 32290",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2093994,
            "range": "± 11911",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 319582406,
            "range": "± 5032417",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 10683,
            "range": "± 19",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 909,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14156,
            "range": "± 42",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2756,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21408,
            "range": "± 41",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85498,
            "range": "± 4147",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 658,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4731,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 18948,
            "range": "± 314",
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
            "range": "± 0",
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
          "id": "be1765e038cf39eabcc34d29ef99a2906b373281",
          "message": "fix(audit): v0.10-W2-R1 P2 — drop duplicate `s4` arg in docker run smoke\n\nCodex round 1 integrated audit on the v0.10 wave-1 + wave-2 range\ncaught a latent CI regression in the v0.10 #B2 docker-smoke\nworkflow: `docker run image s4 --help` against a Dockerfile with\n`ENTRYPOINT [\"/usr/local/bin/s4\"]` would execute `s4 s4 --help`\nand fail with \"unknown subcommand\". Today the image-smoke job\nskips (latest tag not yet published, continue-on-error path);\nthe moment v0.10.0 cuts and publishes `:latest`, the workflow\nwould go red.\n\nFix: drop the `s4` positional, pass only the flag. Entrypoint\nprepends the binary path. Same fix to the matching local-repro\ninvocation in README §\"Verifying the image / chart locally\" so\noperators copying the example don't hit the same bug.\n\nNo code changes. The image-smoke job will exercise the corrected\npath once the v0.9.0 back-fill build finishes pushing `:latest`\n(or, more cleanly, once v0.10.0 cuts and the tag-push trigger\nruns the full docker.yml publish flow).\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T23:35:24+09:00",
          "tree_id": "0d46ba2207c1792f7d5ca5d640abc72fef80deba",
          "url": "https://github.com/abyo-software/s4/commit/be1765e038cf39eabcc34d29ef99a2906b373281"
        },
        "date": 1780843394106,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 59895,
            "range": "± 3921",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57624,
            "range": "± 3407",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 371,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2549785,
            "range": "± 32346",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41659141,
            "range": "± 153006",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 191890,
            "range": "± 901",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51712528,
            "range": "± 1224145",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 755277476,
            "range": "± 5747838",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3062131,
            "range": "± 9088",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32225,
            "range": "± 2575",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38432,
            "range": "± 3010",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 376,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 569921,
            "range": "± 11213",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1559786,
            "range": "± 26740",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 191963,
            "range": "± 3284",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13787539,
            "range": "± 882144",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27033017,
            "range": "± 107060",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3066452,
            "range": "± 6758",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1611345,
            "range": "± 13253",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2656376,
            "range": "± 17662",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 347611204,
            "range": "± 3440216",
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
            "value": 11943,
            "range": "± 310",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 815,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13109,
            "range": "± 28",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2809,
            "range": "± 97",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22784,
            "range": "± 1186",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90800,
            "range": "± 4358",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 595,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4914,
            "range": "± 21",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19630,
            "range": "± 2708",
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
          "id": "eddd7323479ca3e9825fd5cd687209ec080df622",
          "message": "fix(audit): v0.10-W2-R2 P2 — gate branch/sha tags during back-fill\n\nCodex round 2 integrated audit caught: docker.yml back-fill mode\n(workflow_dispatch with `build_ref` set to an older tag like\nv0.9.0) still emitted `type=ref,event=branch` + `type=sha,format=short`\ntags from the dispatcher ref. Result: `gh workflow run docker.yml\n--ref main -f build_ref=v0.9.0 -f image_tag_override=0.9.0\n-f push=true` would publish the v0.9.0 binary under `:main` +\n`:sha-<current-main-sha>` IN ADDITION TO the intended `:0.9.0` +\n`:v0.9.0` raw tags. Consumers pulling `:main` would silently\nget the older binary.\n\nFix: add `enable=${{ inputs.build_ref == '' }}` to both the\n`type=ref,event=branch` and `type=sha,format=short` rules. Tag-\npush events (= dispatcher ref IS the tag, build_ref empty) keep\nemitting branch/sha as before. Back-fill events skip them — only\nthe explicit `image_tag_override` raw tags fire.\n\n**Operational followup (NOT in this commit)**: the in-flight\nv0.9.0 back-fill (run id 27094500626) was dispatched against\nthe previous docker.yml and ALREADY pushed `:main-gpu` pointing\nat v0.9.0-gpu content (GPU job completed 11m26s). When the CPU\njob finishes, it will also push `:main` + `:sha-<a1dfe20-ish>`\nwith v0.9.0 CPU content. Cleanup plan:\n\n  1. Let the CPU back-fill complete (cancelling mid-multi-arch\n     wastes the 40+ min already burned, doesn't prevent the\n     mis-tag because the GPU :main-gpu is already pushed).\n  2. Trigger a no-input workflow_dispatch from main once the\n     back-fill is done: `gh workflow run docker.yml --ref main\n     -f push=true` (NO build_ref, NO image_tag_override). This\n     rebuilds main HEAD with the corrected workflow and\n     overwrites the mis-tagged `:main` / `:main-gpu` /\n     `:sha-<sha>` with the actual current-main binary.\n\nFuture back-fills are protected by this commit's enable= gate.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T23:39:23+09:00",
          "tree_id": "880cde5327d329e7cf130dbd215c2ffed4788560",
          "url": "https://github.com/abyo-software/s4/commit/eddd7323479ca3e9825fd5cd687209ec080df622"
        },
        "date": 1780843634863,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 55083,
            "range": "± 3763",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 56755,
            "range": "± 3446",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 372,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2636570,
            "range": "± 47266",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41650515,
            "range": "± 100355",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192299,
            "range": "± 404",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51984124,
            "range": "± 1573159",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 753944800,
            "range": "± 871210",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3068790,
            "range": "± 58429",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32496,
            "range": "± 2676",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 40066,
            "range": "± 2669",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 378,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 586211,
            "range": "± 6494",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1616705,
            "range": "± 30854",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192219,
            "range": "± 1842",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13395868,
            "range": "± 346174",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27154032,
            "range": "± 456252",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3076371,
            "range": "± 11654",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1626196,
            "range": "± 11814",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2674631,
            "range": "± 26940",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 349798560,
            "range": "± 2736252",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 144,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7984,
            "range": "± 70",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 793,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12558,
            "range": "± 21",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2912,
            "range": "± 78",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22699,
            "range": "± 425",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90780,
            "range": "± 1498",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 595,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4945,
            "range": "± 29",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19747,
            "range": "± 57",
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
          "id": "5ec27e36df3908124cc5c763b0f3a205c5a78034",
          "message": "chore(release): cut v0.10.0 — encryption-aware completion + Docker distribution + hardening\n\nWorkspace 0.9.0 → 0.10.0. Internal dep specs: s4-server (s4-codec /\ns4-config 0.9 → 0.10), s4-codec-py (0.9 → 0.10), s4-codec-wasm\n(0.9 → 0.10). Helm chart Chart.yaml: version 0.2.0 → 0.2.1\n(appVersion-only bump, no chart-shape change since 0.2.0),\nappVersion 0.9.0 → 0.10.0.\n\nWave-1 shipped (commit b052319):\n  - #A1  s4 repair-sidecar --sse-s4-key plumbing (closes the v0.9\n         EncryptedSidecarUnsupported reject path; CLI can now\n         decrypt SSE-S4 chunked bodies in-process via the keyring\n         and rebuild the v3 sidecar)\n  - #B1  ghcr.io/abyo-software/s4 image publishing (multi-arch CPU\n         + amd64 GPU, SLSA provenance + SPDX SBOM, Helm chart\n         repo flip)\n  - #A2-doc  SSE partial-fetch AEAD constraint walkthrough\n\nWave-2 shipped (commits f9853ad / 5aa5c39 / 3da7b7e):\n  - #A4  i686 runtime smoke CI (cargo test codec/config + s4\n         binary --help / --version)\n  - #B2  Docker / Helm distribution smoke CI (helm lint + template\n         + docker compose config + image pull-smoke)\n  - #A3-doc  streaming PUT checksum coverage matrix\n\nIntegrated audit closeout fixes:\n  - be1765e v0.10-W2-R1 P2 (docker-smoke `s4 s4 --help` arg dup)\n  - eddd732 v0.10-W2-R2 P2 (docker.yml back-fill mis-tags `:main`\n            + `:sha-<x>` from dispatcher ref instead of build_ref)\n\nAudit posture: per-feature audits (A1 5R + B1 4R + B2 1R + A2-doc\n1R + A3-doc 0R + A4 0R) + 4-round integrated audit on the full\nv0.9.0..main range. Zero P1. 2 P2 integrated-audit fixes (both\ncaught BEFORE the corresponding image actually shipped). v0.10.0\npublishes from R4 clean.\n\nPre-release verification:\n  - cargo fmt --check clean\n  - cargo clippy --workspace --all-targets -- -D warnings clean\n  - cargo test --workspace: 0 failed\n  - cargo publish --dry-run: s4-codec + s4-config pass (s4-server\n    + s4-codec-py hit expected dep-skew because s4-codec 0.10.0\n    not yet published; resolved at publish time by ordering\n    s4-codec → s4-config → s4-server → s4-codec-py)\n  - helm lint ./charts/s4: clean\n  - helm template (default + image.tag=0.10.0): renders cleanly\n  - docker compose config (both files): clean\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T23:50:08+09:00",
          "tree_id": "592ad3b0f01d188b2c690fc276f371ae1dbd3711",
          "url": "https://github.com/abyo-software/s4/commit/5ec27e36df3908124cc5c763b0f3a205c5a78034"
        },
        "date": 1780844291839,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48104,
            "range": "± 1815",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 58012,
            "range": "± 1469",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 432,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2202744,
            "range": "± 59982",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50516382,
            "range": "± 158526",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201459,
            "range": "± 721",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49306197,
            "range": "± 1098061",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 921841474,
            "range": "± 1924078",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3217897,
            "range": "± 9858",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27247,
            "range": "± 1339",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32668,
            "range": "± 2191",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 418,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 590569,
            "range": "± 4499",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1640485,
            "range": "± 16340",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201384,
            "range": "± 205",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 11993680,
            "range": "± 135478",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28732600,
            "range": "± 193254",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3225494,
            "range": "± 18566",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1491195,
            "range": "± 23958",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2077965,
            "range": "± 15013",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 308631108,
            "range": "± 5233277",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8285,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 899,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13787,
            "range": "± 33",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2937,
            "range": "± 102",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22842,
            "range": "± 101",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 91015,
            "range": "± 212",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 597,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4722,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19468,
            "range": "± 452",
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
            "range": "± 0",
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
          "id": "6988e24b38b6511795d36b0c9e30b313802709d3",
          "message": "fix(docker): bump build job timeout 60→90 min\n\nv0.9.0 back-fill CPU build (run 27094500626) timed out at\n1h0m17s — `timeout-minutes: 60` was too tight for the worst-case\narm64-via-QEMU multi-arch path on a cold GHA cache. GPU build\n(amd64 only) finished comfortably under 30 min.\n\n90 min gives both flavors headroom plus occasional GHA hiccups\nwithout breaching the daily Actions budget. Future back-fills\n+ stable tag pushes inherit the new limit.\n\nCleanup: the failed CPU back-fill means `:0.9.0` (CPU multi-arch\nimage) was never published — only `:0.9.0-gpu`. v0.10.0\nauto-build is in flight under the old 60-min limit; if it\nalso hits the cap, retry from this commit (= the limit fix\nlanded before retry).\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-07T23:59:11+09:00",
          "tree_id": "fca6c9fa768f8002365d4a69f73d3db1379cecd5",
          "url": "https://github.com/abyo-software/s4/commit/6988e24b38b6511795d36b0c9e30b313802709d3"
        },
        "date": 1780844830349,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 49027,
            "range": "± 2706",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 58081,
            "range": "± 1104",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 431,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2189460,
            "range": "± 90093",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50478951,
            "range": "± 53057",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201520,
            "range": "± 523",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49016308,
            "range": "± 956614",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 921487211,
            "range": "± 1538449",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3219819,
            "range": "± 51464",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27628,
            "range": "± 1366",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32904,
            "range": "± 1044",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 419,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 579555,
            "range": "± 9461",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1647798,
            "range": "± 34025",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201631,
            "range": "± 541",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12104387,
            "range": "± 78686",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28638275,
            "range": "± 90268",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3218719,
            "range": "± 7357",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1475045,
            "range": "± 25755",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2284987,
            "range": "± 18629",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 312414565,
            "range": "± 3999116",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 138,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8831,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 1098,
            "range": "± 19",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 16488,
            "range": "± 158",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2936,
            "range": "± 69",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22815,
            "range": "± 312",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 91093,
            "range": "± 255",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 598,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4743,
            "range": "± 48",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19521,
            "range": "± 53",
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
            "range": "± 0",
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
          "id": "63aa85f9d709435100a40d23071737ae9bb6bcc2",
          "message": "feat(docker): native arm64 runner + per-digest build → manifest merge\n\nReplaces the v0.10 #B1 single-job QEMU-emulated multi-arch CPU\nbuild with a per-arch native-runner shape. Closes the 60+ min\nQEMU bottleneck that timed out the v0.9.0 + v0.10.0 back-fills\non the previous 60-min cap (and would have still been painfully\nslow at the bumped 90-min cap).\n\nNew pipeline:\n\n  build (matrix: cpu-amd64 ubuntu-latest, cpu-arm64 ubuntu-24.04-arm,\n         gpu-amd64 ubuntu-latest)\n    - native runner per arch, no QEMU emulation\n    - docker/build-push-action with\n      `outputs: type=image,push-by-digest=true,name-canonical=true,push=true`\n    - upload the resulting digest as a 0-byte file artifact named\n      `digests-<flavor>-<arch>` (filename = digest hex)\n\n  merge (matrix: cpu, gpu)\n    - needs: build\n    - download digests-<flavor>-* artifacts\n    - docker/metadata-action emits the FINAL tags (semver, latest,\n      raw-override, branch/sha) — same gating rules as before\n      (push-only mutable tags, prerelease guard, back-fill aware)\n    - `docker buildx imagetools create --tag <each> <digest-refs>`\n      assembles the multi-arch manifest server-side and pushes\n    - `imagetools inspect` first tag for verification\n\nWhy native arm64 is dramatically faster:\n  - Old: arm64 cargo release build via QEMU emulation on amd64\n    runner → 40-60 min per push (often timed out at 60 min)\n  - New: arm64 cargo release build on `ubuntu-24.04-arm` native\n    runner → 10-15 min per push (parallel with amd64)\n  - GPU build unchanged (already amd64-only native)\n\n`ubuntu-24.04-arm` is GitHub's free native arm64 runner for OSS\npublic repos (rolled out 2024-25). No emulation, no perf penalty.\n\nBuild-only smoke mode (workflow_dispatch with push=false) still\nworks — the build-push-action skips the by-digest export + the\ndigest-upload steps are gated on `inputs.push`, and the merge\njob is gated identically so the build flow exits cleanly without\nneeding digests.\n\nCache scoping: `docker-<flavor>-<arch>` (was `docker-<flavor>`)\nso per-arch runners don't poison each other's layer reuse.\n\nSLSA provenance + SPDX SBOM moved to the merge step — the\nmulti-arch manifest is the artifact end-users actually pull, so\nthe attestation rides on the manifest, not the per-arch digests.\n\nIn-flight `v0.10.0` back-fill (workflow_dispatch run 27097353124)\nwas cancelled before this rewrite — re-trigger from main after\npush to publish `:0.10.0` (now CPU multi-arch via native arm64).\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-08T01:48:57+09:00",
          "tree_id": "8f2b3ff5c289452cbdd9ecec7fbb4bb9bf047b07",
          "url": "https://github.com/abyo-software/s4/commit/63aa85f9d709435100a40d23071737ae9bb6bcc2"
        },
        "date": 1780851402191,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 56307,
            "range": "± 2929",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 55463,
            "range": "± 3596",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 364,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2685919,
            "range": "± 69299",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41630792,
            "range": "± 105789",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192714,
            "range": "± 906",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52368468,
            "range": "± 1051679",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 754433682,
            "range": "± 719685",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3077040,
            "range": "± 58992",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 33072,
            "range": "± 3137",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 40562,
            "range": "± 2842",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 377,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 570866,
            "range": "± 7626",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1551785,
            "range": "± 38776",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192645,
            "range": "± 1492",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13825311,
            "range": "± 191650",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27169218,
            "range": "± 240681",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3084959,
            "range": "± 11285",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1619921,
            "range": "± 32597",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2582040,
            "range": "± 32256",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 347218409,
            "range": "± 1795556",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 140,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8280,
            "range": "± 68",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 815,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13113,
            "range": "± 30",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3238,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 25515,
            "range": "± 210",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 101128,
            "range": "± 1221",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 592,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4531,
            "range": "± 12",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 18124,
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
          "id": "3332f3e0f0c8a03a0a752cbb1d9c5157b8b9da62",
          "message": "feat(v0.11-wave1): #A4 + #A5 + #A7 — polish + maintenance\n\nThree parallel agents landed the v0.11 wave-1 polish + maintenance\nthemes in one batch (file scope partitioned, CHANGELOG pre-segmented\ninto ### Added / ### Changed for non-conflicting append).\n\n**#A4 — 32-bit `s4-server` runtime end-to-end PUT/GET smoke**\n  Extends the v0.10 #A4 `i686-runtime-smoke` job in\n  `.github/workflows/ci.yml` from `--help` / `--version` smoke to a\n  full PUT/GET round-trip via MinIO. Pipeline:\n    - docker run minio/minio:latest in background, poll\n      /minio/health/live (continue-on-error to tolerate Docker Hub\n      flakes)\n    - start i686 `s4` binary as bg process pointing at MinIO,\n      poll /ready\n    - aws s3 mb + cp small file in/out + diff byte-equality\n    - cleanup PID via trap\n  PUT/GET step itself is `continue-on-error: true` (advisory) until\n  a stretch of green main pushes is observed — server log uploaded\n  as artifact for post-mortem.\n  README §\"Supported targets\" 32-bit cell: \"compiles + smoke (CI)\"\n  → \"compiles + --help / --version + advisory PUT/GET round-trip\n  (CI)\". Audit: 3 Codex rounds (R1 P2 MinIO red-pipeline guard, R2\n  false-positive on cross-agent action-version bumps, R3 cross-agent\n  finding on #A7 scope).\n\n**#A5 — GitHub Actions Node.js 24 migration**\n  Bumps 11 action major versions across all 9 workflows to the\n  Node.js 24-native release line, closing the 2026-09 deprecation\n  deadline GHA logs have been warning about since v0.10.0 cut. Pure\n  bump strategy (no FORCE_JAVASCRIPT_ACTIONS_TO_NODE24 env opt-in):\n\n    actions/checkout                    v4 → v5\n    actions/upload-artifact             v4 → v6 (v5 still Node 20)\n    actions/download-artifact           v4 → v7 (v5+v6 still Node 20)\n    actions/github-script               v7 → v8\n    codecov/codecov-action              v4 → v5\n    docker/build-push-action            v5 → v7 (v6 still Node 20)\n    docker/login-action                 v3 → v4\n    docker/setup-buildx-action          v3 → v4\n    docker/metadata-action              v5 → v6\n    aws-actions/configure-aws-credentials  v4 → v6 (v5 still Node 20)\n    azure/setup-helm                    v4 → v5\n\n  Unchanged (already Node 24 at floating tag):\n  Swatinem/rust-cache@v2, benchmark-action/github-action-benchmark@v1,\n  dtolnay/rust-toolchain@stable/nightly (composite).\n  actionlint 1.7.12 clean across all 9 workflows. Audit: 1 Codex\n  round, clean within scope (P1 raised was the #A7 missing local\n  action, fixed by the #A7 agent in the same wave).\n\n**#A7 — Backend compatibility matrix**\n  New `.github/workflows/compat-matrix.yml` (weekly schedule +\n  workflow_dispatch) exercises a PUT/GET + sidecar HEAD round-trip\n  per S3-compatible backend the README claims compat with:\n\n    Docker tier (no secrets):  MinIO + Garage + Ceph RGW\n    Real-cloud tier (operator-provided secrets, silent skip\n                     when absent):  Backblaze B2, Cloudflare R2,\n                                    Wasabi\n\n  Refactored the per-backend round-trip into a composite local\n  action `.github/actions/compat-roundtrip/action.yml` so adding\n  a 7th/8th backend doesn't require copy-paste. Ceph RGW kept as\n  `continue-on-error: true` (the `quay.io/ceph/demo` image is\n  unmaintained upstream but is the only single-container Ceph RGW\n  that fits a CI runner today).\n\n  README §\"How it Compares\" gains a 7-row backend matrix tagged\n  ✅ verified (AWS S3, MinIO, Garage), ⚠️ best-effort (Ceph RGW),\n  🔧 configurable in operator CI (B2, R2, Wasabi).\n\n  Audit: 2 Codex rounds. R1 1 P1 (4 MiB test body produced only\n  1 frame so no sidecar emitted — server gates on `entries.len()\n  > 1`; bumped to 8 MiB = 2 frames at the 4 MiB chunk size) + 1\n  P2 (real-cloud opt-in checked vars but not secrets, fixed to\n  assert both). R2 clean within scope.\n\n**Cross-agent fix**: applied 1-line follow-up to ci.yml's #A4\ni686 smoke env block — adds `AWS_REGION` alongside\n`AWS_DEFAULT_REGION` (SDK v2 prefers the former, CLI v2 reads\neither; defensive against variations across the AWS SDK callers\nin s4-server's startup path).\n\nAudit posture: per-agent 6 Codex rounds total (A4=3, A5=1, A7=2)\n+ cross-agent 1-line fix. fmt/clippy/test clean. Integrated audit\npending; will run after this lands.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-08T11:40:38+09:00",
          "tree_id": "22381aff42a4298082d299f2a6a7380202a2e79a",
          "url": "https://github.com/abyo-software/s4/commit/3332f3e0f0c8a03a0a752cbb1d9c5157b8b9da62"
        },
        "date": 1780886895424,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 55423,
            "range": "± 5263",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 44603,
            "range": "± 2087",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 392,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2142243,
            "range": "± 81958",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 28517400,
            "range": "± 529386",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 152142,
            "range": "± 674",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51271049,
            "range": "± 395993",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 504928794,
            "range": "± 4275162",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2438342,
            "range": "± 18395",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 24844,
            "range": "± 1874",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 29422,
            "range": "± 1778",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 394,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 519284,
            "range": "± 4559",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1370054,
            "range": "± 14858",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 152001,
            "range": "± 336",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13251289,
            "range": "± 297345",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 26641354,
            "range": "± 259325",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2447751,
            "range": "± 4829",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1327066,
            "range": "± 16843",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2127490,
            "range": "± 18786",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 400095162,
            "range": "± 7598394",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 118,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 5850,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 836,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13076,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2428,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 18688,
            "range": "± 15",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 74452,
            "range": "± 139",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 563,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4575,
            "range": "± 85",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19233,
            "range": "± 111",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 30,
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
          "id": "3fceddd517f6c082945b9ab40af6f2a8152f3c10",
          "message": "fix(audit): v0.11-R1 P2 — restore SLSA + SBOM on per-arch builds\n\nCodex R1 integrated audit caught: the v0.10.0 docker.yml\nnative-runner rewrite dropped `provenance: mode=max` + `sbom: true`\nfrom the per-arch build step. `docker buildx imagetools create`\nin the merge step assembles existing manifests but cannot\nretroactively add attestations — every v0.10.0 release image\nwould have lost the supply-chain metadata the README + workflow\ncomments still promised.\n\nFix: attach `provenance: mode=max` + `sbom: true` to the\nper-arch `docker/build-push-action@v7` step. Each digest now\ncarries its own SLSA build attestation + SPDX SBOM, and the\nmerged multi-arch manifest references both transitively\n(visible via `docker buildx imagetools inspect`).\n\nComment block at the file top + merge-step header updated to\nreflect the per-arch-time attachment vs. the old \"applied on\nmerge step\" wording.\n\nCleanup recipe for shipped v0.10.0 image without attestations:\nre-trigger `gh workflow run docker.yml --ref main -f\nbuild_ref=v0.10.0 -f image_tag_override=0.10.0 -f push=true`\nfrom this commit forward — the per-arch rebuild attaches the\nattestations, the merged manifest under `:0.10.0` overwrites\nthe prior attestation-less manifest.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-08T11:44:25+09:00",
          "tree_id": "9a007277b0fecf7e79f103bbccc3e4913a9d2bc6",
          "url": "https://github.com/abyo-software/s4/commit/3fceddd517f6c082945b9ab40af6f2a8152f3c10"
        },
        "date": 1780887126810,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 54703,
            "range": "± 3698",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 53507,
            "range": "± 3081",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 366,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2653998,
            "range": "± 22507",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41767717,
            "range": "± 536474",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192114,
            "range": "± 230",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51387636,
            "range": "± 1087230",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 756137625,
            "range": "± 2119769",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3072166,
            "range": "± 41750",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 31521,
            "range": "± 2708",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 37062,
            "range": "± 2858",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 378,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 621113,
            "range": "± 3612",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1553212,
            "range": "± 28373",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192058,
            "range": "± 375",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13345098,
            "range": "± 214807",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27013187,
            "range": "± 83956",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3066242,
            "range": "± 7148",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1503974,
            "range": "± 23798",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2578123,
            "range": "± 42969",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 340826798,
            "range": "± 4285732",
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
            "value": 7207,
            "range": "± 38",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 791,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12552,
            "range": "± 23",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3121,
            "range": "± 47",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 24733,
            "range": "± 550",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 100967,
            "range": "± 1736",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 593,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4706,
            "range": "± 23",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 18894,
            "range": "± 49",
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
          "id": "c29d69ff8c848e83c44ba7803db12a99daab55c6",
          "message": "fix(audit): v0.11-R2 P2 — restore OCI labels + per-run compat keys\n\nCodex R2 integrated audit caught two cross-feature P2s:\n\n1. **compat-matrix sidecar HEAD satisfied by stale state**\n   (`compat-matrix.yml:73`) — `TEST_KEY` was `compat/large.bin`\n   fixed. On real-cloud rows the bucket is operator-pre-provisioned\n   and reused across weekly runs, so a previous run's sidecar at\n   `compat/large.bin.s4index` could satisfy the post-PUT sidecar\n   HEAD assertion (`write_sidecar` is fail-open by design) even\n   when the current run failed to emit a fresh sidecar — the\n   weekly compat check would go green on stale evidence. Fix:\n   scope the key to `compat/run-${{ github.run_id }}/large.bin`\n   so the assertion provably covers the current run's write path.\n\n2. **OCI image-config labels dropped on published images**\n   (`docker.yml:272-275` + merge-step `metadata-action` `labels:`)\n   — the v0.10.0 native-runner rewrite moved labels into the\n   merge-step metadata-action, but `docker buildx imagetools\n   create` only assembles existing manifests and cannot\n   retroactively patch image-config labels. Same shape as the R1\n   SLSA / SBOM regression. Every published image since v0.10.0\n   has been missing `org.opencontainers.image.{source, vendor,\n   licenses, documentation, ...}`. Fix: hard-code the static\n   labels at the per-arch build step (only `title` +\n   `description` vary by `matrix.flavor`); remove the now-dead\n   `labels:` block from the merge-step `metadata-action`.\n\nCleanup recipe for already-shipped v0.9.0 / v0.10.0 images\nwithout labels: re-trigger `docker.yml` with `build_ref` +\n`image_tag_override` from this commit forward — per-arch\nrebuilds attach the labels, merged manifest under each tag\noverwrites the prior label-less manifest.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-08T11:49:12+09:00",
          "tree_id": "5ca04ac237fccd5dcbcfc2aa49645d993fd6a24a",
          "url": "https://github.com/abyo-software/s4/commit/c29d69ff8c848e83c44ba7803db12a99daab55c6"
        },
        "date": 1780887431474,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 37431,
            "range": "± 1361",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 45528,
            "range": "± 657",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 337,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 1657496,
            "range": "± 41403",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 39335043,
            "range": "± 580486",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 156030,
            "range": "± 295",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 37061726,
            "range": "± 175608",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 715102179,
            "range": "± 2985198",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2503039,
            "range": "± 17479",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 22119,
            "range": "± 1087",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 25487,
            "range": "± 914",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 324,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 549844,
            "range": "± 23801",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1368789,
            "range": "± 47864",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 156063,
            "range": "± 195",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 9909462,
            "range": "± 190126",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 23023178,
            "range": "± 62928",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2509022,
            "range": "± 114887",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1136468,
            "range": "± 40944",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 1649425,
            "range": "± 44278",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 262144001,
            "range": "± 5380134",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 104,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 6083,
            "range": "± 34",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 708,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 10992,
            "range": "± 31",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2557,
            "range": "± 48",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 19911,
            "range": "± 332",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 74945,
            "range": "± 108",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 490,
            "range": "± 14",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4106,
            "range": "± 78",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 16314,
            "range": "± 47",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 24,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 24,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 24,
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
          "id": "08545ba9117612189f6fa399276fd4ed4c34d1f3",
          "message": "fix(audit): v0.11-R3 P2 — propagate test-key + flavor-independent merge\n\nCodex R3 caught two follow-on P2s from R2:\n\n1. **compat-matrix run-scoped key NOT propagated to composite\n   action** — the R2 fix changed `env.TEST_KEY` to\n   `compat/run-${{ github.run_id }}/large.bin`, but the\n   `compat-roundtrip` composite action has its own input\n   default `compat/large.bin`. All 4 invocations omitted\n   `test-key:` so the action fell back to the stale default.\n   Real-cloud rows running against operator-pre-provisioned\n   buckets still had the original stale-state failure mode.\n   Fix: explicitly pass `test-key: ${{ env.TEST_KEY }}` to all\n   4 call sites (minio/garage/ceph + the real-cloud matrix\n   row).\n\n2. **`needs: build` couples CPU + GPU merges to the entire\n   build matrix** — if any build leg fails (e.g. CPU arm64\n   runner hiccup), both CPU AND GPU merges are skipped, even\n   though the GPU build leg succeeded and its digest artifact\n   exists. Per-flavor publication independence is lost. Fix:\n   change merge job's `if:` to\n   `always() && needs.build.result != 'cancelled' && ...` so\n   the matrix runs regardless of build outcome; each merge\n   leg then `continue-on-error: true`s the artifact download\n   and gates all subsequent steps (Buildx setup, login,\n   metadata extraction, imagetools create, inspect) on a new\n   `have_digests.outputs.skip` flag. Result: a missing\n   per-flavor digest set emits a `::warning::` and exits\n   cleanly without taking down the other flavor's publish.\n\nNet effect: CPU arm64 transient failures no longer prevent\nGPU publication, and vice versa. Cancellations still\npropagate (intentional — workflow cancel means stop).\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-08T11:53:14+09:00",
          "tree_id": "79792d0dfa0238a490474209625dce31be519904",
          "url": "https://github.com/abyo-software/s4/commit/08545ba9117612189f6fa399276fd4ed4c34d1f3"
        },
        "date": 1780887659740,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 55657,
            "range": "± 3786",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 58411,
            "range": "± 3304",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 363,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2825563,
            "range": "± 426199",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41789047,
            "range": "± 258020",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192586,
            "range": "± 706",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 54034280,
            "range": "± 1641382",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 757403142,
            "range": "± 4152462",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3199276,
            "range": "± 89811",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 33123,
            "range": "± 2801",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 40073,
            "range": "± 3172",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 376,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 576059,
            "range": "± 16199",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1567311,
            "range": "± 60908",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192385,
            "range": "± 531",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13068350,
            "range": "± 1559696",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27929505,
            "range": "± 1286293",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3095223,
            "range": "± 73226",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1540043,
            "range": "± 24799",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2595409,
            "range": "± 37831",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 363695561,
            "range": "± 35351883",
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
            "value": 7392,
            "range": "± 160",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 814,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13088,
            "range": "± 19",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3314,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 25349,
            "range": "± 118",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 101403,
            "range": "± 444",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 592,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4549,
            "range": "± 53",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 18012,
            "range": "± 101",
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
            "value": 28,
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
          "id": "157d7e7315b148ef1c5aaf2087414899e3286315",
          "message": "fix(audit): v0.11-R4 P1 — refuse partial multi-arch CPU publish\n\nCodex R4 escalated the previous merge-job hardening from P2 to\nP1: my R3 fix (`have_digests.skip=true` only when ZERO artifacts\nexist) was too permissive for the CPU flavor. CPU's contract is\nmulti-arch (`linux/amd64` + `linux/arm64` per the README +\nDocker manifest); if CPU arm64 build legged-fails but CPU amd64\nsucceeds, my R3 code would have run `imagetools create` with\njust the amd64 digest, overwriting `:0.10.0` with an amd64-only\nmanifest — a silent degradation of the advertised multi-arch\nimage.\n\nFix: expected digest count per flavor:\n  - CPU expects 2 (amd64 + arm64)\n  - GPU expects 1 (amd64-only by design, nvCOMP redist constraint)\nSkip merge if `actual < expected`; emit a warning naming the\nflavor + count. Per-flavor independence preserved (GPU still\npublishes when CPU partially fails), but no flavor publishes\nwith a downgraded arch set.\n\nWhy P1: the bug would have silently shipped an amd64-only image\nunder the CPU `:<version>` tag on first CPU arm64 flake, and\nARM operators following the README install recipe would see\n\"unable to find image for linux/arm64\" with no warning that the\nrelease was incomplete.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-08T11:55:42+09:00",
          "tree_id": "cb282071ee8b43d1e3e00af117d1c6c422b3b1a7",
          "url": "https://github.com/abyo-software/s4/commit/157d7e7315b148ef1c5aaf2087414899e3286315"
        },
        "date": 1780887782154,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 58186,
            "range": "± 5492",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 43723,
            "range": "± 2387",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 392,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2145645,
            "range": "± 24783",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 28559163,
            "range": "± 579469",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 152194,
            "range": "± 1037",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52792058,
            "range": "± 347950",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 504109835,
            "range": "± 1202009",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2454241,
            "range": "± 13177",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 25911,
            "range": "± 2438",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 31382,
            "range": "± 1639",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 393,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 529251,
            "range": "± 6015",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1382592,
            "range": "± 26505",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 152284,
            "range": "± 467",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12882423,
            "range": "± 315830",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 25949205,
            "range": "± 224365",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2457146,
            "range": "± 15992",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1348549,
            "range": "± 14622",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2149025,
            "range": "± 15915",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 395390138,
            "range": "± 4329297",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 118,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 5856,
            "range": "± 31",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 784,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12227,
            "range": "± 39",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2426,
            "range": "± 17",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 18697,
            "range": "± 25",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 74500,
            "range": "± 136",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 596,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4579,
            "range": "± 95",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19184,
            "range": "± 84",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 41,
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
          "id": "eebc7e25bdecdbaac7248b775de61acbd161efce",
          "message": "docs(audit): v0.11-R5/R6 — action-version policy comment (false-positive guard)\n\nCodex integrated audit R5 + R6 both raised P1 \"action versions\nunpublished\" findings against the v0.11 #A5 Node.js 24 bumps.\nBoth were false positives: Codex's review sandbox couldn't reach\ngithub.com to verify tag existence (its `git ls-remote --tags\nhttps://github.com/actions/upload-artifact.git refs/tags/v6` came\nback empty), so it assumed the tags don't exist.\n\nReality, verified via `gh api /repos/<owner>/<repo>/releases/latest`:\n\n  actions/checkout            latest v6.0.3 → @v5 valid\n  actions/upload-artifact     latest v7.0.1 → @v6 valid\n  actions/download-artifact   latest v8.0.1 → @v7 valid\n  actions/github-script       latest v9.0.0 → @v8 valid\n  docker/build-push-action    latest v7.2.0 → @v7 valid\n  docker/login-action         latest v4.2.0 → @v4 valid\n  aws-actions/configure-...   latest v6.2.0 → @v6 valid\n\nAND every CI run since commit 3332f3e (the v0.11 wave-1 push)\nhas resolved these cleanly — workflow_dispatch + push triggers\nboth succeed.\n\nAdds top-of-file comment to ci.yml + compat-matrix.yml\ndocumenting the policy + verification + the audit false-positive\nhistory, so future audits / human reviewers see the evidence\ninline before re-raising the same claim. No code change.\n\nAfter this commit the v0.11 integrated audit is treated as\n2-round converged (R5 + R6 both no-real-finding) and v0.11.0\ncut can proceed.\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-08T12:02:38+09:00",
          "tree_id": "a1461487dcedf1463737a37a9e0208e8332f0a3d",
          "url": "https://github.com/abyo-software/s4/commit/eebc7e25bdecdbaac7248b775de61acbd161efce"
        },
        "date": 1780888222839,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 54223,
            "range": "± 3899",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 54211,
            "range": "± 4100",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 366,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2667526,
            "range": "± 40971",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41636815,
            "range": "± 486143",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192559,
            "range": "± 1273",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52825170,
            "range": "± 1055898",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 754528865,
            "range": "± 2858694",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3085611,
            "range": "± 89080",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32695,
            "range": "± 2439",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38599,
            "range": "± 2795",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 377,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 580408,
            "range": "± 19132",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1668320,
            "range": "± 49993",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192429,
            "range": "± 1247",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12603337,
            "range": "± 144479",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28535439,
            "range": "± 904025",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3094607,
            "range": "± 19903",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1568249,
            "range": "± 27719",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2615479,
            "range": "± 26018",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 353399384,
            "range": "± 4304492",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 174,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7828,
            "range": "± 48",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 815,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13112,
            "range": "± 22",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3237,
            "range": "± 45",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 25670,
            "range": "± 506",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 101216,
            "range": "± 819",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 594,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4708,
            "range": "± 101",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 17869,
            "range": "± 34",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 28,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 28,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 28,
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
          "id": "50d728bc3d61f4c66d4aa81c75e18ed02df9b916",
          "message": "chore(release): cut v0.11.0 — polish + maintenance (32-bit + Node24 + compat matrix)\n\nWorkspace 0.10.0 → 0.11.0. Internal dep specs: s4-server +\ns4-codec-py + s4-codec-wasm 0.10 → 0.11. Helm chart 0.2.1 →\n0.2.2 (appVersion bump only, no chart shape change since 0.2.1).\n\nWave-1 shipped (commit 3332f3e):\n  - #A4 — i686 runtime PUT/GET round-trip in CI (MinIO docker\n    backed). README \"compiles + smoke\" → \"compiles + full\n    PUT/GET (advisory)\".\n  - #A5 — 11 GHA JavaScript actions bumped to Node.js 24-ready\n    majors. actionlint clean. 2026-09 deprecation gate closed.\n  - #A7 — backend compat matrix workflow (weekly cron +\n    dispatch). MinIO/Garage/Ceph docker tier + B2/R2/Wasabi\n    real-cloud tier (silent skip when secrets absent).\n    Composite local action `.github/actions/compat-roundtrip`\n    factors out the per-backend PUT/GET + sidecar HEAD step.\n\nAudit closeout fixes:\n  - 3fceddd  R1 P2  SLSA + SBOM restored on per-arch builds\n                    (`imagetools create` can't retroactively patch)\n  - c29d69f  R2 P2  OCI image labels restored on per-arch builds\n                    + compat-matrix key scoped to ${{ github.run_id }}\n  - 08545ba  R3 P2  test-key propagation into composite action +\n                    flavor-independent merge (CPU arm64 failure\n                    no longer blocks GPU publish)\n  - 157d7e7  R4 P1  expected-digest-count guard: refuse\n                    partial multi-arch publish (CPU arm64 fail\n                    must not overwrite `:0.10.0` as amd64-only)\n  - eebc7e2  R5/R6  doc-only — action-version policy comment\n                    documents false-positive Codex sandbox\n                    hallucination + verification evidence\n\nAudit posture: per-feature 6 Codex rounds (A4=3, A5=1, A7=2)\n+ 6-round integrated audit (4 P2 + 1 P1 real fixes, 2\nfalse-positive rounds with inline doc note). Zero unresolved.\nv0.11.0 publishes from R6 (effective convergence).\n\nPre-release verification:\n  - cargo fmt --check clean\n  - cargo clippy --workspace --all-targets -- -D warnings clean\n  - cargo test --workspace: 0 failed\n  - helm lint ./charts/s4: clean\n  - docker compose config: both files clean\n  - actionlint: all 9 workflows clean\n\nCo-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>",
          "timestamp": "2026-06-08T12:04:47+09:00",
          "tree_id": "c6c7a40516c1488c66b3613bd86cc0fb785f8bd9",
          "url": "https://github.com/abyo-software/s4/commit/50d728bc3d61f4c66d4aa81c75e18ed02df9b916"
        },
        "date": 1780888372495,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 47997,
            "range": "± 1712",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 58059,
            "range": "± 6407",
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
            "value": 2245284,
            "range": "± 32808",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50583286,
            "range": "± 120061",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201639,
            "range": "± 545",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 48903815,
            "range": "± 839124",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 924049292,
            "range": "± 2193044",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3228354,
            "range": "± 20173",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27739,
            "range": "± 1183",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32438,
            "range": "± 1036",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 419,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 575834,
            "range": "± 4228",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1648288,
            "range": "± 19036",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201541,
            "range": "± 276",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12221651,
            "range": "± 76725",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 29033387,
            "range": "± 180801",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3220922,
            "range": "± 55095",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1454768,
            "range": "± 20054",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2086641,
            "range": "± 22791",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 315320698,
            "range": "± 1693877",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 137,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7416,
            "range": "± 43",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 910,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14164,
            "range": "± 30",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2937,
            "range": "± 92",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22787,
            "range": "± 54",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90939,
            "range": "± 211",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 632,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5259,
            "range": "± 170",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20831,
            "range": "± 287",
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
            "value": 32,
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
          "id": "b950b0c1d9212d69df746e95af2661583871e38e",
          "message": "fix(v1.0): F14 — compat-matrix Garage NODE_ID parse + Ceph best-effort\n\nTwo bugs surfaced by the first manual compat-matrix runs (in support\nof the v1.0 HTTP API surface freeze evidence):\n\n1. Garage job died with `Internal error: 0 nodes match '===='`.\n   The awk pattern `/HEALTHY|UNHEALTHY|NO ROLE/` matched the\n   `==== HEALTHY NODES ====` table header line in dxflrs/garage:v1.1.0\n   output and printed `====` as the node id. Replaced with\n   `garage node id -q`, which returns `<hex>@<addr>` directly.\n\n2. Ceph RGW round-trip failed with XAmzContentSHA256Mismatch on the\n   PutObject through s4-server. quay.io/ceph/demo:latest-quincy is\n   unmaintained upstream and drifts against the current\n   aws-sdk-rust streaming-checksum wire shape. The job already had\n   continue-on-error on the start step (per the existing header\n   comment's intent), but the round-trip step did not, so the wire\n   drift gated the workflow. Made round-trip continue-on-error too\n   and added a separate Warn step + downgraded the README compat\n   claim from \"CI-verified\" to \"claimed but not CI-verified\" until\n   a successor demo image lands (Cluster B will reflect this in\n   the README v1.0 rewrite).\n\nAfter this commit the matrix should show success on MinIO + Garage\n(docker tier) and on B2 / R2 / Wasabi (real-cloud tier), with Ceph\nRGW emitting a clear warning. This is the evidence backing the\nREADME v1.0 stability section's HTTP API surface freeze.",
          "timestamp": "2026-06-08T23:30:23+09:00",
          "tree_id": "4e5b3360cba9e8bc75fdc556a4cbbd8024defded",
          "url": "https://github.com/abyo-software/s4/commit/b950b0c1d9212d69df746e95af2661583871e38e"
        },
        "date": 1780929507015,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 55686,
            "range": "± 3433",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 55770,
            "range": "± 6047",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 364,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2666444,
            "range": "± 84564",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41885496,
            "range": "± 1036680",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192160,
            "range": "± 3939",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 53145034,
            "range": "± 1305139",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 757811127,
            "range": "± 5565646",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3077001,
            "range": "± 19514",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 31868,
            "range": "± 2640",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38898,
            "range": "± 3226",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 376,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 568436,
            "range": "± 21481",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1582744,
            "range": "± 50069",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192452,
            "range": "± 2022",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12451542,
            "range": "± 475091",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27821493,
            "range": "± 155316",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3158622,
            "range": "± 20587",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1521366,
            "range": "± 13743",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2458654,
            "range": "± 39828",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 356907754,
            "range": "± 7902021",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 172,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7099,
            "range": "± 450",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 814,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13096,
            "range": "± 32",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2910,
            "range": "± 40",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22704,
            "range": "± 970",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90761,
            "range": "± 3104",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 636,
            "range": "± 35",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4956,
            "range": "± 73",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19686,
            "range": "± 50",
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
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 27,
            "range": "± 1",
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
          "id": "8c6b93088bf7b4c1a9641cad7443527d8d56ef99",
          "message": "docs(v1.0): F5-F12 — README rewrite + audit ignores doc + Garage best-effort\n\nCluster B of the v1.0 readiness work: aligns the README + companion\ndocs with the v1.0 stability section's actual contract.\n\nREADME.md\n- F5: removes \"alpha / early-access / pre-1.0 / may break the wire\n  format\" disclaimer block from Project Status; replaces with v1.0\n  GA narrative (surface freeze ≠ marketing claim; first public\n  production reference still being collected under issue label\n  `production-reference`).\n- F5 (sweep): \"When NOT to use S4\" bullets updated to drop \"alpha\n  disclaimer\" + \"pre-1.0\" phrasing.\n- F6: qualifies the \"v0.8.x backend can read v1.0 bytes\" claim with\n  three explicit cross-major caveats: v3 sidecar UnsupportedVersion,\n  S4E6 chunked-SSE unrecognized, and the deployments where bidirectional\n  compatibility still holds.\n- F7: drops \"replication\" from the cumulative scope claim; instead,\n  the \"What's not promised\" section explicitly excludes it from v1.0\n  freeze (experimental scaffolding from v0.6, promotion on v1.x\n  roadmap).\n- F8: adds explicit s3s 0.13 stability policy — we track 0.13 trait\n  surface through minor bumps, a s3s major would itself trigger our\n  v2.0 with migration in docs/migration/.\n- F10: expands freeze list with real public surface — added the\n  s4_server::service::S4Service row (hard-freeze per the v1.0\n  judgment call), s4_server::sse row (14 items), s4_server::streaming\n  row (3 items), and expanded the s4_server::repair row with the\n  RepairReport / OrphanReport / SweepReport / RepairSseBinding types\n  + #[non_exhaustive] policy. s4-codec row now enumerates the full\n  error enum set (CodecError / IndexError / FrameError /\n  GpuSelectError / CompareOp) and all the index version constants.\n- F11: expands the Helm row to enumerate every top-level values.yaml\n  key (replicas, image.*, serviceAccount.*, backend.*, gpu.*, tls.*,\n  policy.*, service.*, ingress.*, resources.*, podAnnotations,\n  podLabels, podSecurityContext, securityContext, nodeSelector,\n  tolerations, affinity, extraEnv, extraVolumes, extraVolumeMounts,\n  probes.*). Distinguishes \"default values may shift in minor\" from\n  \"key shape is v2.0 territory\".\n- F12: links the new docs/security/cargo-audit-ignores.md from the\n  \"What's not promised\" section.\n- Adds a new \"Backend compatibility matrix (CI-verified surface)\"\n  table inside Stability — calls out which of the 7 README-claimed\n  backends are currently CI-gating vs claimed-but-not-CI-verified,\n  and links to compat-matrix.yml for the source of truth. Garage +\n  Ceph RGW are flagged as claimed-but-not-CI-verified with the\n  specific drift symptom (signature interop for Garage, unmaintained\n  demo image for Ceph). Re-introducing both as gating is on the\n  v1.x roadmap.\n- Removes the contradictory \"service/sse/streaming are NOT public\n  API\" bullet (the F2 audit found all three have external callers;\n  they are now frozen at the externally-used surface and listed in\n  the freeze table).\n\ndocs/security/cargo-audit-ignores.md (new)\n- F12: per-advisory rationale + mitigation + upstream-tracking for\n  the four currently-ignored RUSTSEC advisories\n  (2026-0098/0099/0104 = rustls-webpki path-blocked by AWS SDK\n  transitive pin; 2025-0134 = rustls-pemfile unmaintained, dev-only).\n- Policy section for when to add a new ignore vs trigger a\n  release-blocker.\n- Procedure for removing an ignore when the upstream bump lands.\n\n.github/workflows/compat-matrix.yml\n- F14 follow-up: Garage round-trip step is now continue-on-error\n  with a warning (same pattern as Ceph). dxflrs/garage:v1.1.0\n  rejects STREAMING-AWS4-HMAC-SHA256-PAYLOAD from current\n  aws-sdk-rust, breaking the 8 MiB PUT round-trip. The\n  provisioning steps still gate (admin-API surface IS verified);\n  re-introducing the round-trip as gating is on the v1.x roadmap.\n\nCargo.toml + Chart.yaml version bumps + the v1.0.0 tag are\nintentionally NOT in this commit per F9 (version bumps must land\nin a single atomic commit with the cut). This commit just lands\nthe doc + workflow scaffolding; the cut commit follows after the\nintegrated re-audit closes any new findings the rewrite surfaced.",
          "timestamp": "2026-06-08T23:41:04+09:00",
          "tree_id": "2d8cffe9b9be9385b6da5d2c214a42af8ea17dbc",
          "url": "https://github.com/abyo-software/s4/commit/8c6b93088bf7b4c1a9641cad7443527d8d56ef99"
        },
        "date": 1780930146774,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 53779,
            "range": "± 3898",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 55539,
            "range": "± 3410",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 365,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2722389,
            "range": "± 66388",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41939110,
            "range": "± 160274",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192693,
            "range": "± 567",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 53268667,
            "range": "± 1303694",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 756451912,
            "range": "± 936670",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3165020,
            "range": "± 49726",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32163,
            "range": "± 3230",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 37422,
            "range": "± 2834",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 376,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 581183,
            "range": "± 16836",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1594847,
            "range": "± 43714",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192696,
            "range": "± 447",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12095719,
            "range": "± 176295",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27002562,
            "range": "± 161237",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3182620,
            "range": "± 10873",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1599447,
            "range": "± 16340",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2523892,
            "range": "± 23611",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 391503590,
            "range": "± 3219140",
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
            "value": 7661,
            "range": "± 73",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 814,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13119,
            "range": "± 30",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2916,
            "range": "± 28",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21770,
            "range": "± 468",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 86407,
            "range": "± 2122",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 588,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4468,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 18279,
            "range": "± 600",
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
          "id": "f9c14a3b89bb4b60359444180b6fd82c17446781",
          "message": "docs(v1.0): re-audit fix wave — 2×P1 + 7×P2 closure\n\nAdversarial re-audit on the v1.0 readiness work returned verdict C\nwith two P1 + seven P2 findings. This commit closes all 9.\n\n## P1 fixes (blocking)\n\nP1-1 — Fabricated `with_backend` in S4Service freeze table.\n`with_backend` does not exist anywhere in the workspace; the freeze\ntable at README.md§Stability claimed it as part of the constructor\nsignature. Rewrote the S4Service row to enumerate the actual builder\nAPI (every `pub fn with_*` visible on S4Service — 23 builders, each\nlocked at its `fn(self, …) -> Self` signature). Adding a new\n`with_*` builder remains additive (minor); rename or signature\nchange is v2.0. Also added the `DEFAULT_MAX_BODY_BYTES` +\n`DEFAULT_REPLICATION_MAX_CONCURRENT` constants which the binary\nrelies on.\n\nP1-2 — cargo-audit-ignores.md had 3 fabricated advisory titles + a\n4th with wrong version and scope. Rewrote the doc against the\nactual `cargo audit --json` output and `cargo tree -i` dep paths:\n\n- RUSTSEC-2026-0098: real title \"Name constraints for URI names\n  were incorrectly accepted\" (was: \"rustls-webpki Time constructor\n  panic\"). Dep path verified via cargo tree against\n  rustls-webpki@0.101.7 ← rustls 0.21 ← aws-smithy-http-client.\n- RUSTSEC-2026-0099: real title \"Name constraints were accepted\n  for certificates asserting a wildcard name\" (was: \"PKCS#7 cert\n  chain validation\").\n- RUSTSEC-2026-0104: real title \"Reachable panic in certificate\n  revocation list parsing\" (was: \"name-constraint validation\",\n  which is actually 0098/0099). Updated mitigation: S4 does not\n  opt-in to rustls CRL checking, so the panic path is unreachable.\n- RUSTSEC-2025-0134: scope was wrong. Doc had claimed \"1.x, dev-\n  only test fixtures\"; actual is `rustls-pemfile = \"2\"` (2.2.0 in\n  Cargo.lock), used at runtime in `crates/s4-server/src/tls.rs`\n  to load the production HTTPS listener cert/key. Risk acceptance\n  still holds (operator-controlled PEM files, not adversary-\n  controlled) but the rationale needed grounding in reality.\n\nAdded a \"Verification\" section at the bottom with the exact\ncommands to re-verify every fact in the doc.\n\n## P2 fixes\n\nP2-1 — S3 API matrix said \"Bucket replication ✅ Full\" while the\nstability section explicitly excludes replication from v1.0 freeze.\nUpdated the matrix row to \"⚠ experimental\" with the same scaffold-\nonly qualifier.\n\nP2-2 — F6 cross-major caveat (b) said \"v0.8.x readers do not\nrecognize S4E6\". S4E6 was introduced in v0.8.1 (commit a7333f2),\nso only v0.8.0 is missing it. Updated to \"v0.8.1+ recognizes\nS4E6; only the v0.8.0 hot-fix line lacks support\".\n\nP2-3 — F6 cross-major caveat (a) inverted wording about\n--sse-chunk-size. The default is 1_048_576 (chunked is on by\ndefault whenever SSE-S4 is active), so the previous wording\n\"deployments that have not enabled --sse-chunk-size > 0\" was\ninverted. Rephrased to \"deployments without an SSE-S4 keyring\nconfigured never emit v3 sidecars; SSE-S4 deployments DO emit v3\nby default\".\n\nP2-4 — Freeze surprise on 23 unfrozen `pub mod`. Added a new\nsub-section \"Modules NOT in the freeze list\" right after the\nfreeze table, enumerating every other `pub mod` in s4-server\nand stating explicitly: they exist for binary-and-tests' needs,\nare not part of the v1.0 contract, and may break in any minor.\nConsumers who depend on them should pin `=1.x.y`. Path to\npromotion is opening an issue with the use case.\n\nP2-5 — README.ja.md was stuck at v0.2.0 (2026-05-12), wildly\ncontradicting the v1.0 claim. Added an outdated-banner at top\nof the JP file pointing readers to README.md for current\ninformation, and dropped the two cross-link references to JP\nfrom README.md until the translation is updated (tracked as\n`i18n-ja` for v1.x).\n\nP2-6 — `VerifyReport` (return type of frozen `verify_sidecar`)\nwas missing from the s4_server::repair freeze list. Added.\nAlso added an explicit \"public structs are NOT\n`#[non_exhaustive]`; field additions are v2.0 territory\" clause\nto the repair row so the asymmetry vs the public enums is\ndocumented rather than implicit.\n\nP2-7 — `SharedSseKey` (= `Arc<SseKey>`, parameter type of\nfrozen `S4Service::with_sse_key`) and `S4E6Header<'a>` (return\ntype of frozen `parse_s4e6_header`) were missing from the\ns4_server::sse freeze list. Added both, with explicit hints\npointing to the frozen signatures that reach them.\n\n## Out of scope this commit\n\n- P3 (post-cut nice-to-have) items deferred: ferro_compress::Error\n  + s4-config::CompressionMode non_exhaustive hygiene; additional\n  repair constants in freeze; #[non_exhaustive] on the public\n  structs noted above (left as documented-asymmetry per P2-6\n  treatment); S4F3 / S4E7 / S4E8 forward-compat clarification.\n- F9 (workspace version bump) still deferred to the cut commit per\n  the F9 rule of atomic version-bump-with-cut.\n\nAfter re-audit signs off, cut commit follows.",
          "timestamp": "2026-06-08T23:56:13+09:00",
          "tree_id": "6b37fe2bb0baa9fcc260fd7e24b8cb4e3fb7967f",
          "url": "https://github.com/abyo-software/s4/commit/f9c14a3b89bb4b60359444180b6fd82c17446781"
        },
        "date": 1780931031951,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 58380,
            "range": "± 5541",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 46241,
            "range": "± 1662",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 385,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2153995,
            "range": "± 30663",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 28678485,
            "range": "± 93641",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 152131,
            "range": "± 386",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52124508,
            "range": "± 2032960",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 507126173,
            "range": "± 1510895",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2500065,
            "range": "± 22275",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 26778,
            "range": "± 1760",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 31486,
            "range": "± 1477",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 398,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 536506,
            "range": "± 4904",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1414545,
            "range": "± 27203",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 152168,
            "range": "± 235",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 14104214,
            "range": "± 962535",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 26442095,
            "range": "± 138075",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2529899,
            "range": "± 14406",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1356395,
            "range": "± 13089",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2168039,
            "range": "± 42656",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 423034353,
            "range": "± 4137477",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 118,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 5852,
            "range": "± 140",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 788,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12309,
            "range": "± 39",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2232,
            "range": "± 26",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 17203,
            "range": "± 205",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 68575,
            "range": "± 468",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 561,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4943,
            "range": "± 24",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19610,
            "range": "± 75",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 30,
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
          "id": "2dde5c96059689e6b4da1488971fe05c6efe4b72",
          "message": "docs(v1.0): NF-1 — service_arc / SharedService path correction\n\nRound-2 re-audit caught a contradiction: README freeze table claims\n`SharedService` is frozen at `s4_server::service::SharedService`, but\nthe struct actually lives at `s4_server::service_arc::SharedService`\n(see crates/s4-server/src/service_arc.rs:40). The `service_arc`\nmodule is `pub mod` in lib.rs:30 but was omitted from both:\n- the freeze list (so the SharedService claim pointed to a non-\n  existent path), and\n- the \"Modules NOT in the freeze list\" 25-module enumeration (so\n  consumers couldn't tell whether the module was frozen or not).\n\nResolved by:\n1. The S4Service row in the freeze table now explicitly notes\n   `SharedService` lives at `s4_server::service_arc::SharedService`\n   and is the \"externally-supported clone-able shared use\" wrapper.\n2. The \"Modules NOT in the freeze list\" section now counts 30 total\n   pub mods (was \"~27\") and lists the five frozen modules explicitly:\n   repair, service, sse, streaming, AND service_arc (with the note\n   that service_arc contributes only SharedService — the rest is\n   not frozen).\n3. The 25-module enumeration updated to exclude service_arc + add\n   the modules previously elided (acme, lock_recovery, metrics,\n   state_loader, streaming_checksum — the round-1 list missed them).\n\nRound-2 re-audit verdict was B (this one finding); round 3 closes\nit. Ready for cut.",
          "timestamp": "2026-06-09T00:00:37+09:00",
          "tree_id": "34ce646b398f3b29411f01b776f5254ecd6dec9e",
          "url": "https://github.com/abyo-software/s4/commit/2dde5c96059689e6b4da1488971fe05c6efe4b72"
        },
        "date": 1780931319650,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 47687,
            "range": "± 1373",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57338,
            "range": "± 893",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 426,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2247027,
            "range": "± 26043",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50713692,
            "range": "± 121289",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201222,
            "range": "± 482",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 48554186,
            "range": "± 215296",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 925291883,
            "range": "± 1990095",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3215995,
            "range": "± 2643",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27399,
            "range": "± 1262",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32263,
            "range": "± 1234",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 420,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 578931,
            "range": "± 2956",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1643336,
            "range": "± 32946",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201327,
            "range": "± 648",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12062616,
            "range": "± 69030",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28416184,
            "range": "± 136556",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3217733,
            "range": "± 2892",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1485024,
            "range": "± 18559",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2222341,
            "range": "± 35132",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 309075492,
            "range": "± 2517808",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8091,
            "range": "± 12",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 913,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14176,
            "range": "± 38",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2756,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21408,
            "range": "± 33",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85534,
            "range": "± 490",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 632,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5378,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20806,
            "range": "± 371",
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
            "range": "± 0",
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
          "id": "e69ab933e22d8e51ea3438a84b76b4f7b0ec8225",
          "message": "docs(v1.0): round-3 dual-reviewer fix wave — 4×P1 + 7×P2 closure\n\nRound 3 (Opus + Codex CLI dual cross-review) caught 11 new findings\nthe prior 3 rounds missed. This commit closes all of them. Following\nthe user's instruction \"Finding 0 になるまでレビュー回して\" (loop\nreview until findings = 0), this is the third fix wave; a 4th\nreview round will follow.\n\n## P1 fixes (4)\n\nNF-2 — S4Service freeze claimed Default impl + S4Service::default()\nchain that don't exist in the source (verified by `grep \"Default for\nS4Service\" crates/` returning nothing). Same fabrication species as\nthe original P1-1 with_backend finding. Rewrote the freeze opening\nto use the actual constructor: `S4Service::new(backend, registry,\ndispatcher) -> Self` with the full signature spelled out.\n\nNF-10 — Backend compat matrix marked AWS / B2 / R2 / Wasabi all as\n\"✓ gating\" on this upstream repo. Verified via `gh run view` that\nall four jobs skip in 3-5 seconds with \"opt-in\" annotations because\nthe repo has no AWS_E2E_* / B2_* / R2_* / WASABI_* secrets\nconfigured. Only MinIO actually gates on this repo. Reworded the\nupper compat-matrix table to be honest: \"⚠ opt-in. Gates ONLY when\n<secrets> are configured on the fork; this upstream repo has them\nNOT configured.\" Added a closing paragraph distinguishing this\nupstream repo's intentional no-shared-credentials posture from\nwhat a fork can opt in to.\n\nNF-7 / Codex #2 — S4Service freeze claims every with_* builder\nsignature is frozen; 13 of the 23 builders take Arc<ManagerType>\nparameters from modules the README explicitly lists as unfrozen\n(tagging / inventory / lifecycle / notifications / replication /\nmfa / cors / object_lock / kms / versioning / access_log /\nrate_limit / policy). Internally contradictory. Added an \"Important\ncaveat on builder parameter types\" paragraph to the S4Service row\nexplaining: builder signatures are frozen, but the manager\nconstructors / fields are not — consumers should pin =1.x.y if\nthey inject these managers.\n\nCodex #5 — Lower compat table at README.md:475-478 had Garage as\n\"✅ Verified via weekly compat-matrix CI\" while the upper Stability\ntable now says \"claimed but not currently CI-verified\". Fixed both\nthe AWS row (added opt-in qualifier) and the Garage row\n(distinguished provisioning gate from round-trip continue-on-error)\nand the Ceph row (noted both start AND round-trip are best-effort).\n\n## P2 fixes (7)\n\nNF-3 — README claimed `StreamingBlob` was a stable type alias in\n`s4_server::streaming`. Actually it's a private `use s3s::dto::\nStreamingBlob;` at streaming.rs:32; no pub type. Rewrote the\nstreaming row to explicitly note the functions take s3s::dto::\nStreamingBlob and that the type is governed by the s3s 0.13 row\nabove.\n\nNF-4 / Codex #4 — `.github/workflows/ci.yml` security-audit job\ncomment still called rustls-pemfile \"dev-only\". Round-1 fix wave\ncorrected the cargo-audit-ignores.md doc but missed the CI comment.\nUpdated to \"runtime dependency (rustls-pemfile 2.x, used at\nproduction HTTPS startup in tls.rs)\" and linked to the audit doc.\n\nNF-5 — Stability section said \"compat-matrix job's start-step\nalways gates\" but Ceph's start step has continue-on-error: true.\nReworded to \"gates for every backend except Ceph RGW\".\n\nCodex #1 / NF-8 — `s4-codec` row said Py and WASM bindings \"export\nthe same surface\" as the Rust crate. They don't — Py exports\nPyCpuZstd / PyCpuGzip / gpu_available + exception classes; WASM\nexports decompressFramed / decompressSingle / supportedCodecs /\nsupportedFrameMagic. Rewrote to \"bindings are versioned in\nlockstep; their binding-specific public APIs are frozen, but the\nbindings do NOT re-export the full Rust surface — only the\ndocumented subset\".\n\nCodex #3 — FrameIndex is frozen but its public fields use\nFrameIndexEntry + Option<SseChunkBinding>, and the frozen\nencode_index / decode_index family returns RangePlan +\nEncryptedRangePlan. Added these four reachable types to the\ns4-codec freeze list explicitly.\n\nCodex #6 — Adding #[non_exhaustive] to 30+ existing public enums\nis source-API-breaking for v0.x → v1.0 consumers who used exhaustive\nmatch without `_ =>` arms. Stability section previously documented\nonly wire caveats. Added a new \"v0.x → v1.0 source compatibility\nnote\" sub-section enumerating every affected enum and the\nmechanical consumer-side fix (add `_ =>` arm); explained why this\nis intentional (alternative would be never adding a variant within\nv1.x = worse contract).\n\nNF-9 — `s4-config` is published on crates.io and re-exported as\n`s4_server::config::*` but had no row in the freeze table. Added\nan s4-config row: CompressionMode + BackendConfig + S4Config struct\nfield sets frozen; S4Config::from_toml stub explicitly NOT frozen\n(currently bails with \"toml loading not implemented yet\"; the\neventual real implementation may change its error shape).\n\n## Round-3 reviewer agreement\n\nOf the 11 new findings, both reviewers independently caught:\n- S4Service builder param type contradiction (Codex #2 / Opus NF-7)\n- ci.yml rustls-pemfile dev-only comment stale (Codex #4 / Opus NF-4)\n- Py + WASM \"same surface\" overclaim (Codex #1 / Opus NF-8)\n\nOpus uniquely caught: NF-2 (Default::default fabrication), NF-3\n(StreamingBlob), NF-5 (start-step gate), NF-9 (s4-config), NF-10\n(cloud opt-in misrep), and verified NF-1 still closed.\n\nCodex uniquely caught: #3 (FrameIndex inner types), #5 (lower\nREADME compat table contradicts upper), #6 (source-break enum\nmigration caveat).\n\nRound 4 follows.",
          "timestamp": "2026-06-09T01:37:53+09:00",
          "tree_id": "88cea5617e1bb4b6b7df564231cd13a391895331",
          "url": "https://github.com/abyo-software/s4/commit/e69ab933e22d8e51ea3438a84b76b4f7b0ec8225"
        },
        "date": 1780937158808,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 37706,
            "range": "± 1758",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 45220,
            "range": "± 4713",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 334,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 1760887,
            "range": "± 20937",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 39251599,
            "range": "± 59928",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 156168,
            "range": "± 575",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 39362301,
            "range": "± 293524",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 715701649,
            "range": "± 5114499",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2497000,
            "range": "± 3955",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 21800,
            "range": "± 781",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 25637,
            "range": "± 690",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 333,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 450527,
            "range": "± 36320",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1296284,
            "range": "± 3949",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 156113,
            "range": "± 251",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 8571660,
            "range": "± 66448",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 21525190,
            "range": "± 67362",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2496888,
            "range": "± 1826",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1161344,
            "range": "± 14618",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 1775824,
            "range": "± 19293",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 250140030,
            "range": "± 838085",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 144,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 6955,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 707,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 10979,
            "range": "± 41",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2420,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 18831,
            "range": "± 33",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 75091,
            "range": "± 161",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 491,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4108,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 16196,
            "range": "± 26",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 24,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 24,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 24,
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
          "id": "945c583ca3a9d79eb88e7b35b91e70f737d7c26a",
          "message": "docs(v1.0): round-4 fix wave — 4 P2 closures (Python names, enum list, SECURITY policy, FrameIndex fields)\n\nRound 4 (Opus + Codex dual cross-review) confirmed all 11 round-3 fixes\nlanded correctly and surfaced 4 P2 doc-accuracy findings:\n\n- Opus NF-NEW-1 (P2): README s4-codec freeze row claimed Python public\n  APIs as `PyCpuZstd` / `PyCpuGzip`, which are the Rust struct names.\n  Actual Python-importable names are `CpuZstd` / `CpuGzip` (per\n  `#[pyclass(name = \"...\")]` at crates/s4-codec-py/src/lib.rs:147,211).\n  README's own usage example uses the correct names; the freeze table\n  was self-contradicting. Fixed by spelling out: Python-side names\n  `CpuZstd` + `CpuGzip` are the contract, with a parenthetical\n  noting the underlying Rust types. Also added `__version__` to the\n  enumerated Python public surface (was omitted).\n\n- Opus NF-NEW-2 (P2): The new \"v0.x → v1.0 source compatibility note\"\n  enumerated 29 enums but omitted 4 that ALSO received non_exhaustive\n  in commits ce30dde / db06912: CompareOp, GpuSelectError,\n  SelectInputFormat, SelectOutputFormat. The commit message promised\n  \"enumerating every affected enum\" — promise was unmet. Updated the\n  list to 33 total enums (6 s4-codec + 27 s4-server), grouped by\n  crate for clarity.\n\n- Codex P2 (SECURITY.md:27): \"S4 is currently pre-1.0\" contradicted\n  the README v1.0 stability claim. Rewrote the Supported Versions\n  section to match the v1.0 contract: rolling window of latest\n  minor + previous minor, patch releases on the affected minor's\n  release branch.\n\n- Codex P3 (FrameIndex inner-type field freeze): the newly-added\n  FrameIndexEntry / SseChunkBinding / RangePlan / EncryptedRangePlan\n  entries didn't explicitly say their public field sets were frozen\n  (unlike the s4_server::repair row which does). Added the same\n  \"field sets + inherent method signatures frozen; additions /\n  removals / renames are v2.0\" rule explicitly.\n\n## Cut-commit reminders (deferred; not in this commit)\n\n- Opus NF-NEW-3 (P3): CHANGELOG.md needs a v1.0.0 entry. Will be\n  added in the cut commit itself (atomic with the version bump,\n  per F9). Drafted topics for that entry: 33 enums received\n  non_exhaustive, freeze table introduced in Stability section,\n  test-only helpers gated (F3), service_arc/SharedService path\n  correction, source-break migration note, backend compat matrix\n  table, cargo-audit-ignores doc, Garage + Ceph best-effort,\n  binding-specific Python/WASM API freeze.\n\nAfter this wave, round 5 follows per user instruction\n\"Finding 0 になるまでレビュー回して\".",
          "timestamp": "2026-06-09T01:47:25+09:00",
          "tree_id": "62e2279145304ba2df0f4703e08d2cb616502489",
          "url": "https://github.com/abyo-software/s4/commit/945c583ca3a9d79eb88e7b35b91e70f737d7c26a"
        },
        "date": 1780937728881,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 56196,
            "range": "± 4152",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 55378,
            "range": "± 3797",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 368,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2750808,
            "range": "± 43076",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41749382,
            "range": "± 102226",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192348,
            "range": "± 1813",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52519848,
            "range": "± 1195930",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 754466047,
            "range": "± 2469943",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3069863,
            "range": "± 16765",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32269,
            "range": "± 2692",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 41969,
            "range": "± 2972",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 379,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 574922,
            "range": "± 8604",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1555436,
            "range": "± 42241",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192358,
            "range": "± 1791",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12324503,
            "range": "± 80214",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27066259,
            "range": "± 482517",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3074983,
            "range": "± 21529",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1596102,
            "range": "± 19037",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2546675,
            "range": "± 31634",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 355802334,
            "range": "± 5557070",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 139,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7988,
            "range": "± 37",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 954,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14802,
            "range": "± 128",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2565,
            "range": "± 162",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22696,
            "range": "± 415",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 91083,
            "range": "± 157",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 591,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4577,
            "range": "± 140",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 18321,
            "range": "± 81",
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
          "id": "e6b356f5dd3bab60aac2f0c37fd43644dee8d9f1",
          "message": "docs(v1.0): round-5 fix wave — scope-explicit freeze + 4 doc closures\n\nRound 5 (Opus + Codex dual cross-review) returned verdict B with ~8\nnew findings. Root cause across them: the freeze-table model that\ntries to enumerate every pub item exhaustively keeps surfacing \"you\nmissed X\" findings, even though X is internal-helper material that\nshould NOT be frozen anyway.\n\n## Structural fix — scope-explicit freeze (closes 4 findings)\n\nAdded new \"How to read the freeze table — scope of 'frozen'\"\nsub-section right before \"Modules NOT in the freeze list\", stating:\n\n- Items named in the freeze table ARE the v1.0 contract.\n- Other pub items in the same modules are NOT part of the contract\n  (they exist for binary + test needs).\n- Consumers depending on an unlisted item should pin =1.x.y and treat\n  each minor bump as a manual integration step.\n- Filing an issue is the path to promoting an unlisted item into the\n  named freeze list.\n\nIncludes a representative (not exhaustive) list of currently-existing\nunlisted items in the frozen modules: parse_bucket_key + 4 constants\nin repair; SSE_MAGIC_V1..V6, CustomerKeyMaterial, parse_customer_key_\nheaders, encrypt_with_source, S4E4Header, parse_s4e4_header,\ndecrypt_with_kms, and chunked helpers in sse; INDEX_MAGIC,\nSSE_BLOCK_V3, INDEX_HEADER_BYTES, build_index_from_body, sidecar_key,\nis_reserved_sidecar_key, FRAME_MAGIC, PADDING_MAGIC, FRAME_HEADER_BYTES\nin s4-codec. Closes Codex round-5 P2#1 (repair) + P2#2 (sse) + P3#3\n(s4-codec format constants).\n\nWhy this scope shape (paragraph in the same sub-section): exhaustive\n\"freeze every pub item\" over-promises on transitive internal-helper\nchurn; \"freeze nothing\" under-promises on items library consumers\nactually integrate against; naming the items keeps the contract\nexplicit on both ends.\n\n## Other closures\n\nOpus NF-NEW-4 (P2) — README enumerated Python exception classes as\n\"S4Error, S4CrcMismatchError, S4SizeMismatchError, plus the\ncodec-error / format-error / backend-error / I/O-error wrappers\".\n\"format-error wrapper\" was a fabricated category — no class with that\nlabel exists. Replaced the hand-wave with the actual 9 class names\n(S4Error, S4CrcMismatchError, S4SizeMismatchError,\nS4CodecMismatchError, S4UnregisteredCodecError,\nS4ManifestSizeExceedsLimitError, S4ManifestSizeMismatchError,\nS4BackendError, S4IoError) per\ncrates/s4-codec-py/src/lib.rs:52-60.\n\nOpus NF-NEW-5 (P3) — README \"field sets + inherent method signatures\nfrozen\" clause for index inner types grammatically applied only to\n\"the latter four\" (FrameIndexEntry / SseChunkBinding / RangePlan /\nEncryptedRangePlan); FrameIndex itself wasn't covered. Reworded to\nexplicitly include FrameIndex.\n\nOpus NF-NEW-6 (P3) — \"every public enum that may grow new variants\ngets #[non_exhaustive]\" was an overstatement: 6 AWS-spec-bound enums\nin unfrozen modules (IncludedVersions / LifecycleStatus / LockMode /\nEffect / ReplicationStatus / VersioningState) are intentionally not\nannotated. Scoped the claim to \"every public enum on the frozen\nsurface\" + added a parenthetical explaining the AWS-spec rationale.\n\nOpus NF-NEW-7 (P3) — crates/s4-codec-py/README.md listed\nNvcompZstd / NvcompBitcomp / NvcompGDeflate as Python classes\n\"Requires --features nvcomp-gpu\". Those classes don't exist in\nany build — the Rust feature forwards to s4-codec-rs but does NOT\nadd Python classes. README's Supported codecs table now lists only\nthe two actual classes (CpuZstd + CpuGzip) with a paragraph\nexplaining the v1.0 decision to keep Python CPU-only + the v1.x\nroadmap candidate for GPU exposure. Same correction in\ncrates/s4-codec-py/src/lib.rs doc comment.\n\nCodex P3#4 / Opus NF-NEW-8 — crates/s4-codec-wasm/README.md \"Status\"\nsection still said \"v0.4 #24 — initial cut\" + \"v0.9+ follow-up\" +\n\"as of v0.8.5\". Updated to v1.0-aligned wording with link to main\nREADME's stability section + \"v1.x roadmap\" phrasing for the\nstreaming-decoder + npm-publish-automation items.\n\nRound 6 follows per user instruction \"Finding 0 になるまで\".",
          "timestamp": "2026-06-09T01:56:52+09:00",
          "tree_id": "cb23fa10d8ea73708fba35b0df843987291c9960",
          "url": "https://github.com/abyo-software/s4/commit/e6b356f5dd3bab60aac2f0c37fd43644dee8d9f1"
        },
        "date": 1780938299679,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 49135,
            "range": "± 2701",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 59161,
            "range": "± 1929",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 426,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2211493,
            "range": "± 29437",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50708559,
            "range": "± 851434",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201216,
            "range": "± 2815",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49371517,
            "range": "± 1336620",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 923878239,
            "range": "± 5400417",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3219632,
            "range": "± 13284",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27620,
            "range": "± 1126",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32325,
            "range": "± 1017",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 422,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 598756,
            "range": "± 7685",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1639986,
            "range": "± 16074",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201511,
            "range": "± 4130",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12823791,
            "range": "± 112174",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 29056503,
            "range": "± 215990",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3223970,
            "range": "± 6861",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1460212,
            "range": "± 19733",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2114948,
            "range": "± 28534",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 331384241,
            "range": "± 8167647",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7879,
            "range": "± 339",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 919,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14254,
            "range": "± 44",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2752,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21372,
            "range": "± 29",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85336,
            "range": "± 222",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 635,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5242,
            "range": "± 41",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20943,
            "range": "± 37",
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
            "range": "± 0",
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
          "id": "fef44ada9362828fadfbc205733326b469cf83d0",
          "message": "docs(v1.0): round-6 fix wave — 2×P2 + 1×P3 closure (Python pkg metadata + CompressionMode non_exhaustive)\n\nRound 6 returned a split verdict: Opus said findings=0/cut-now, Codex\nsaid B with 3 new findings. Per the user's \"stricter wins\" pattern,\nCodex's findings are real. Verified each by grep before fixing.\n\n## Codex round-6 P2#1 — Python package GPU/nvCOMP marketing claim\n\nThe Python module is CPU-only in v1.0 (the round-5 fix wave already\ncorrected the README freeze table and the Supported codecs table).\nBut the surrounding package metadata still claimed GPU support:\n\n- crates/s4-codec-py/README.md:3 → \"In-process GPU/CPU compression\n  from Python\" → rewrote to \"In-process CPU compression (zstd + gzip)\n  from Python\" with the v1.0-aligned rationale for why GPU isn't\n  exposed.\n- crates/s4-codec-py/Cargo.toml:19 description → updated to drop GPU.\n- crates/s4-codec-py/pyproject.toml:12 description → updated.\n- crates/s4-codec-py/pyproject.toml:11 keywords → dropped \"gpu\",\n  \"nvcomp\" (kept \"s3\", \"compression\", \"zstd\", \"gzip\").\n- README \"Build from source\" → kept the `--features nvcomp-gpu` build\n  recipe (the Cargo feature exists for forwarding to s4-codec-rs's\n  GPU paths at the Rust level) but added a note that it does NOT\n  expose Python classes for GPU codecs in v1.0; the only Python-level\n  effect is what `gpu_available()` reports.\n\n## Codex round-6 P2#2 — PyPI classifier still \"3 - Alpha\"\n\ncrates/s4-codec-py/pyproject.toml:22 had\n`Development Status :: 3 - Alpha`. For a v1.0 frozen-API package,\nthe correct PyPI trove classifier is `5 - Production/Stable`.\nBumped (skipping 4 - Beta on purpose; the API surface is frozen,\nnot a beta).\n\n## Codex round-6 P3 — CompressionMode lacked #[non_exhaustive]\n\ncrates/s4-config/src/lib.rs:8 declared `CompressionMode` as a plain\n`pub enum` without `#[non_exhaustive]`, despite the enum being in\nthe v1.0 freeze table (via the s4-config row added in round-3).\nTwo options were available: annotate (per the F1 pattern), or\nexplicitly exclude from \"annotated enums\" with a v2.0-frozen\nvariant set. Annotating is the right call — `CompressionMode` is\na strategy enum that may legitimately grow (e.g. a future\n\"per-bucket-pinned\" mode), and `#[non_exhaustive]` keeps additions\nSemVer-additive in v1.x.\n\n- crates/s4-config/src/lib.rs:8 → added `#[non_exhaustive]`\n  attribute.\n- README.md:198 → updated the v0.x→v1.0 source-compat enum list\n  from 33 → 34 enums, now grouped by 3 crates (6 s4-codec + 27\n  s4-server + 1 s4-config).\n\n## Verification\n\n- `cargo fmt --check`: clean\n- `cargo build --workspace`: succeeds (no consumer break from the\n  CompressionMode annotation — the enum is constructed by name in\n  serde paths only; no exhaustive match exists on it in this\n  workspace)\n- `cargo test -p s4-config`: clean\n\nAfter this wave, Codex's 3 P2/P3 findings are closed. Opus already\nreturned A on round 6, so the convergence point is here. Round 7\nverifies the closure and confirms findings = 0 before the cut commit.",
          "timestamp": "2026-06-09T02:02:06+09:00",
          "tree_id": "67340ac13ad78e018cd53c0a0fccdc92f341b3ab",
          "url": "https://github.com/abyo-software/s4/commit/fef44ada9362828fadfbc205733326b469cf83d0"
        },
        "date": 1780938604713,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 57748,
            "range": "± 4017",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57353,
            "range": "± 4039",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 370,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2647825,
            "range": "± 104100",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41686078,
            "range": "± 495988",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192094,
            "range": "± 914",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51876415,
            "range": "± 1746151",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 754280553,
            "range": "± 3578391",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3065528,
            "range": "± 48688",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32891,
            "range": "± 2532",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 40307,
            "range": "± 2622",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 377,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 573969,
            "range": "± 11592",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1551139,
            "range": "± 37395",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 191998,
            "range": "± 721",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12358122,
            "range": "± 156799",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27039622,
            "range": "± 173735",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3068325,
            "range": "± 6042",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1594689,
            "range": "± 24829",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2505194,
            "range": "± 71023",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 347707470,
            "range": "± 2699205",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 140,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8445,
            "range": "± 92",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 802,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12691,
            "range": "± 64",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2830,
            "range": "± 98",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22433,
            "range": "± 537",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90704,
            "range": "± 2297",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 589,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4563,
            "range": "± 82",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 17906,
            "range": "± 136",
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
          "id": "d93c9ebd17d48e063b19c197b3e4d49648a739f3",
          "message": "docs(v1.0): round-7 fix wave — 2 P2 + 2 P3 closures (cut-commit prep)\n\nCodex round 7 returned C verdict with 5 findings. Closing the 4\nnon-cut-commit-dependent ones in this wave; the remaining P1\n(workspace version still 0.11.0 while docs claim v1.0) is the cut\ncommit itself which immediately follows.\n\n## Closures\n\nP2 #2 — README.md:1259 said `s4 = \"1\"` in Cargo.toml. There is no\n`s4` crate published — only `s4-server`, `s4-codec`, `s4-config`.\nFixed to enumerate all 3 publishable crates (`s4-server = \"1\"`,\n`s4-codec = \"1\"`, `s4-config = \"1\"`) so a downstream consumer\ncopy-pastes a Cargo.toml line that actually resolves on crates.io.\n\nP2 #3 — README.md:197 said the v0.x→v1.0 #[non_exhaustive] sweep\nwas \"every public enum on the frozen surface that may grow new\nvariants\", but the list of 34 enums includes enums from modules\nthe README explicitly marks as NOT frozen (cors, mfa, notifications,\npolicy, select, tagging, …). Internal contradiction. Reworded the\npreamble to describe the annotation as \"a Rust-source-level\nforward-compat tool — it works the same way whether the containing\nmodule is frozen by name in the table above or not\", removing the\nimplicit claim that the listed enums all live in frozen modules.\nThe annotation IS a forward-compat measure regardless of module\nfreeze status, so the list is accurate; the previous wording over-\nclaimed scope.\n\nP3 #4 — crates/s4-codec-py/README.md:78-81 said \"GPU codecs require\na --features nvcomp-gpu build\" without the v1.0 caveat that the\nPython module does NOT add GPU classes even when built with the\nfeature. Reworded to spell out: the flag forwards to s4-codec-rs's\nGPU paths at the Rust level, but only affects what gpu_available()\nreports at the Python level — no new Python classes appear.\n\nP3 #5 — git diff --check reported \"README.md:1363: new blank line\nat EOF\". Trimmed the trailing blank line.\n\n## Remaining for the cut commit (follows immediately)\n\nP1 #1 — workspace Cargo.toml version is 0.11.0; Cargo.lock S4\npackages all 0.11.0; internal-dep pins 0.11; charts/s4/Chart.yaml\nappVersion 0.11.0. Per the F9 atomic-bump rule, the version bump\n+ tag + CHANGELOG entry happens in a single commit. That commit\nis the v1.0.0 cut and is the natural next step now that the doc\nsurface has converged.\n\nAfter cut: round 8 verifies the version-bump landed correctly\n(or this final wave + the cut converges to verdict A on round 8).",
          "timestamp": "2026-06-09T02:05:55+09:00",
          "tree_id": "f8d8d17203bdd02f784cb8c8ea0162bfa03813a6",
          "url": "https://github.com/abyo-software/s4/commit/d93c9ebd17d48e063b19c197b3e4d49648a739f3"
        },
        "date": 1780938835714,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 56091,
            "range": "± 4537",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 56038,
            "range": "± 3380",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 365,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2633716,
            "range": "± 62595",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41812961,
            "range": "± 124779",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192590,
            "range": "± 598",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52014852,
            "range": "± 1142178",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 756558073,
            "range": "± 986369",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3080700,
            "range": "± 28312",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 33162,
            "range": "± 3203",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 42030,
            "range": "± 3068",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 375,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 580134,
            "range": "± 9507",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1558001,
            "range": "± 33784",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192952,
            "range": "± 1676",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13364502,
            "range": "± 565310",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27325895,
            "range": "± 173314",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3124950,
            "range": "± 30341",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1642194,
            "range": "± 47741",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2674744,
            "range": "± 92337",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 358145451,
            "range": "± 8502019",
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
            "value": 10208,
            "range": "± 77",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 943,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14885,
            "range": "± 89",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2910,
            "range": "± 75",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22810,
            "range": "± 672",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90848,
            "range": "± 1624",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 589,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4914,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19686,
            "range": "± 34",
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
          "id": "af3c69b6cd435eecfb92488fe9893ff26b808c4b",
          "message": "chore(release): cut v1.0.0 — SemVer-stable surface freeze\n\nv1.0 — the surface enumerated in README.md §\"Stability — v1.0\nguarantees\" is frozen for the v1.x line. Downstream consumers can\npin s4-server = \"1\" (or s4-codec = \"1\", or s4-config = \"1\", or\nghcr.io/abyo-software/s4:1) and rely on the surface not changing.\n\nThis is the cut commit. Per the F9 atomic-bump rule, version bumps\nland in a single commit:\n\n- Cargo.toml: workspace.version 0.11.0 → 1.0.0\n- crates/s4-server/Cargo.toml: internal dep pins (s4-codec,\n  s4-config) \"0.11\" → \"1\"\n- crates/s4-codec-wasm/Cargo.toml: internal dep pin\n  (s4-codec) \"0.11\" → \"1\"\n- crates/s4-codec-py/Cargo.toml: internal dep pin (s4-codec-rs)\n  \"0.11\" → \"1\" (already landed in round-7 wave; included here\n  again for atomicity)\n- charts/s4/Chart.yaml: appVersion \"0.11.0\" → \"1.0.0\";\n  chart's own version 0.2.2 → 0.2.3 (Helm-SemVer convention —\n  appVersion bump only, no chart-shape change)\n- Cargo.lock: regenerated via `cargo update --workspace`\n- CHANGELOG.md: full v1.0.0 entry per Keep-a-Changelog,\n  documenting the freeze surface, the 7-round audit history,\n  the added/changed/fixed work, and the v1.x roadmap candidates\n- SOCIAL_POSTS.md: outdated-banner added (v0.8.10-era drafts;\n  the \"pre-1.0\" wording inside the drafts contradicts the v1.0\n  claim and needs rewrite before being posted)\n\n## Audit convergence\n\n7 rounds of dual-reviewer (Opus + Codex) drove ~30 findings to\nclosure. Final round-7 verdict: Codex C (only blocker = this\ncommit's version bump), Opus A (findings = 0, cut now). The\naudit-cycle-pattern memory (\"5-8 rounds normal for convergence\")\nheld: this is round-cut at #7, well inside the band.\n\nNet commit stack since v0.11.0 base (commit 50d728b):\n- 5889cab fix(v1.0): F2 — annotate service/sse/streaming as freeze candidates\n- 65f972b fix(v1.0): F3 — gate test-only helpers out of public API contract\n- ce30dde fix(v1.0): F1 — non_exhaustive on codec public enums\n- db06912 fix(v1.0): F1 — non_exhaustive on server public enums\n- b950b0c fix(v1.0): F14 — compat-matrix Garage NODE_ID parse + Ceph best-effort\n- 8c6b930 docs(v1.0): F5-F12 — README rewrite + audit ignores doc + Garage best-effort\n- f9c14a3 docs(v1.0): re-audit fix wave — 2×P1 + 7×P2 closure\n- 2dde5c9 docs(v1.0): NF-1 — service_arc / SharedService path correction\n- e69ab93 docs(v1.0): round-3 dual-reviewer fix wave — 4×P1 + 7×P2 closure\n- 945c583 docs(v1.0): round-4 fix wave — 4 P2 closures\n- e6b356f docs(v1.0): round-5 fix wave — scope-explicit freeze + 4 doc closures\n- fef44ad docs(v1.0): round-6 fix wave — 2×P2 + 1×P3 closure\n- d93c9eb docs(v1.0): round-7 fix wave — 2 P2 + 2 P3 closures\n- [this commit] chore(release): cut v1.0.0 — SemVer-stable surface freeze\n\nAfter this commit:\n- Tag v1.0.0 on this commit\n- Publish 4 crates: s4-codec, s4-config, s4-server, s4-codec-py\n  (s4-codec-wasm npm publish is manual + listed as v1.x roadmap)\n- GitHub Release notes from CHANGELOG.md v1.0.0 entry\n- ghcr.io image trigger (auto from tag via docker.yml workflow)",
          "timestamp": "2026-06-09T02:10:45+09:00",
          "tree_id": "e481a86a8678186d2df3e60cce2404661373aaf0",
          "url": "https://github.com/abyo-software/s4/commit/af3c69b6cd435eecfb92488fe9893ff26b808c4b"
        },
        "date": 1780939123972,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 53819,
            "range": "± 3546",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 56418,
            "range": "± 7379",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 366,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2603990,
            "range": "± 62734",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 41681571,
            "range": "± 118720",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192297,
            "range": "± 324",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 50684635,
            "range": "± 1041265",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 753451585,
            "range": "± 647793",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3068623,
            "range": "± 7740",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 31339,
            "range": "± 2712",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 37843,
            "range": "± 3313",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 375,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 569152,
            "range": "± 17338",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1546317,
            "range": "± 22941",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192205,
            "range": "± 2447",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 10987981,
            "range": "± 82426",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 26068159,
            "range": "± 77426",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3078051,
            "range": "± 9180",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1599713,
            "range": "± 17497",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2666158,
            "range": "± 73605",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 339286383,
            "range": "± 1699724",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 140,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 12335,
            "range": "± 255",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 800,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12686,
            "range": "± 28",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2997,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22711,
            "range": "± 366",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90905,
            "range": "± 1618",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 589,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5053,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19694,
            "range": "± 38",
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
          "id": "1f2944df1908d33fbab5e783beffcbaf5fc1cb74",
          "message": "docs(post-v1.0): refresh image-tag examples 0.9.0 → 1.0.0\n\nRound-8 Opus review caught that helm install + docker pull snippets\nin §\"Install via Docker / Helm\" still cited image.tag=0.9.0 +\nghcr.io/abyo-software/s4:0.9.0. After the v1.0 cut, copy-pasting\nthese would pull a 2-version-old image and confuse first-time\nv1.0 users.\n\nFixed 4 references:\n- Line 256: `--set image.tag=0.9.0` → `1.0.0` + added a note\n  pointing at the §Stability tag policy and `image.tag=1` as the\n  floating major-line option.\n- Line 267: `--set image.tag=0.9.0-gpu` → `1.0.0-gpu`.\n- Lines 304-306: `docker pull/run ghcr.io/abyo-software/s4:0.9.0`\n  → `:1.0.0`.\n\nThe 3 per-crate READMEs (crates/{s4-codec,s4-server,s4-config}/\nREADME.md) are symlinks to this file so they inherit the fix\nautomatically; no separate edit needed.\n\nNet: doc lag closed, image-tag examples match the v1.0 cut.\nThis is a post-cut docs commit on main; the v1.0.0-tagged\nsnapshot at af3c69b has the old tag examples, which is symmetric\nwith the rest of the v1.0 publish payload (crate READMEs uploaded\nto crates.io are the v1.0.0 snapshot). Future post-v1.0 doc\nrefreshes can land on main without re-tagging.",
          "timestamp": "2026-06-09T02:18:00+09:00",
          "tree_id": "865a63f364ed8928792992d9a4c5520e8a0a40b5",
          "url": "https://github.com/abyo-software/s4/commit/1f2944df1908d33fbab5e783beffcbaf5fc1cb74"
        },
        "date": 1780939566893,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 49255,
            "range": "± 1857",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 58216,
            "range": "± 877",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 426,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2251712,
            "range": "± 63317",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50540976,
            "range": "± 107098",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201285,
            "range": "± 321",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 50730164,
            "range": "± 1264616",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 922355530,
            "range": "± 2230800",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3221759,
            "range": "± 7688",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27641,
            "range": "± 2182",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 33039,
            "range": "± 988",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 419,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 577043,
            "range": "± 15011",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1636045,
            "range": "± 8270",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201336,
            "range": "± 912",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12079992,
            "range": "± 76517",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28559356,
            "range": "± 103382",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3220043,
            "range": "± 23468",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1438401,
            "range": "± 17474",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2144222,
            "range": "± 24943",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 313181537,
            "range": "± 6341176",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7417,
            "range": "± 39",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 890,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13829,
            "range": "± 69",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2749,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21345,
            "range": "± 338",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85173,
            "range": "± 344",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 646,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5234,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20768,
            "range": "± 43",
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
            "range": "± 0",
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
          "id": "f1c70bdd39c6ae60c0c5ef3756b55fe3fb67427a",
          "message": "ci(fuzz): ClusterFuzzLite PR fuzzing — bolero targets を OSS-Fuzz 形式で実行\n\nPR の diff に対して s4-codec の bolero fuzz target 7 本を ASan + libFuzzer\nで 5 min 回す regression gate を追加 (深掘りは既存 fuzz-nightly.yml が担当)。\n\n- .clusterfuzzlite/{project.yaml,Dockerfile,build.sh}: OSS-Fuzz 互換 build\n  統合。`cargo bolero build-clusterfuzz` で tests/fuzz_bolero.rs の\n  ハーネスを `<test>_fuzzer` wrapper + libFuzzer バイナリに変換し、\n  commit 済み __fuzz__ corpus を seed_corpus.zip として同梱。\n- base image の pinned nightly (1.91) が MSRV 1.92 未満のため\n  nightly-2026-06-01 に date pin で切替。\n- OSS-Fuzz env との衝突 2 件を build.sh で吸収:\n  - RUSTFLAGS の単独 `--cfg fuzzing` が bolero の engine 選択 cfg を壊す\n    → unset し cargo-bolero に flag 構築を任せ、ASan runtime のみ\n    `-Zsanitizer=address` で常時リンク (CFLAGS 計装済み zstd-sys 対策)\n  - CXXFLAGS の -stdlib=libc++ が vendored libFuzzer の final link で\n    未解決シンボルになる → 除去\n- bad_build_check は ELF 前提 (objdump ASan 検査 + $OUT 直下実行仮定) で\n  shell wrapper 方式 (nearcore と同方式) に適用不能のため action input で\n  無効化。target 検出は `*_fuzzer` 名で通ることを cifuzz source で確認済み。\n\nローカル検証: docker compile exit 0 / 7 target + seed corpus 生成 /\nframe_parser・cpu_zstd_decompress を -runs で実行し coverage feedback 動作確認。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-10T23:55:44+09:00",
          "tree_id": "0d4b3bbbd69d27256b48cff8029b345a00668ed6",
          "url": "https://github.com/abyo-software/s4/commit/f1c70bdd39c6ae60c0c5ef3756b55fe3fb67427a"
        },
        "date": 1781103851525,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48099,
            "range": "± 2797",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57637,
            "range": "± 1003",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 428,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2218015,
            "range": "± 311825",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50554528,
            "range": "± 112435",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201244,
            "range": "± 2522",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49359890,
            "range": "± 2524633",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 922114534,
            "range": "± 3364168",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3264809,
            "range": "± 22969",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 28061,
            "range": "± 1912",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32383,
            "range": "± 1035",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 419,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 576572,
            "range": "± 4451",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1655607,
            "range": "± 15876",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201207,
            "range": "± 377",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 11319393,
            "range": "± 80071",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28150827,
            "range": "± 242313",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3277200,
            "range": "± 40341",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1464568,
            "range": "± 24529",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2189581,
            "range": "± 47226",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 330527475,
            "range": "± 6299159",
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
            "value": 8880,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 912,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14184,
            "range": "± 106",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2755,
            "range": "± 40",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21333,
            "range": "± 115",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85360,
            "range": "± 163",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 632,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5222,
            "range": "± 24",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20713,
            "range": "± 232",
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
            "range": "± 0",
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
          "id": "c1a3a2e2ecc1ff6d166bc7d9a37c6cf9bc8ce17f",
          "message": "feat(cli): s4 estimate — 導入前の節約額シミュレータ subcommand\n\n既存バケットを read-only でサンプリングし、gateway が PUT 時に行うのと\n同一の SamplingDispatcher 判定 + 実圧縮で削減率と $/月 を見積もる。\n\n- 拡張子で層別 → サイズ加重サンプリング (--seed で決定的) → 外挿\n- 大オブジェクトは Range GET 先頭 --max-sample-bytes (8 MiB) で計測\n- GPU 不要: nvcomp-* 判定は cpu-zstd proxy 計測 + 明示注記\n- report は storage-bytes-only / sampled fraction / prefix 乖離の\n  注意書きを常時出力 (honest limitations)\n- 新規 lib API: s4_server::estimate (run_estimate / EstimateParams /\n  EstimateReport / EstimateError #[non_exhaustive])。additive only。\n\ntests: lib 501 pass (estimate unit 10 本) / minio e2e 1 本 (ignored,\nDocker gate) / clippy -D warnings / fmt clean。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T00:31:16+09:00",
          "tree_id": "61c3a15c997d55f8e5baf3b4a70ecb6f3a1a1693",
          "url": "https://github.com/abyo-software/s4/commit/c1a3a2e2ecc1ff6d166bc7d9a37c6cf9bc8ce17f"
        },
        "date": 1781105953977,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 38197,
            "range": "± 1095",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 45295,
            "range": "± 6933",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 331,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 1775962,
            "range": "± 20528",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 39337429,
            "range": "± 75508",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 156316,
            "range": "± 247",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 39864465,
            "range": "± 624944",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 716175274,
            "range": "± 3729831",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2625904,
            "range": "± 14871",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 22184,
            "range": "± 1411",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 25663,
            "range": "± 845",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 326,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 552672,
            "range": "± 26682",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1292917,
            "range": "± 10707",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 156551,
            "range": "± 686",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 8908939,
            "range": "± 103815",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 21975658,
            "range": "± 802866",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2634675,
            "range": "± 14666",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1136521,
            "range": "± 25655",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 1789310,
            "range": "± 23648",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 293010554,
            "range": "± 2811035",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 104,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 6246,
            "range": "± 15",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 690,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 10699,
            "range": "± 24",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2785,
            "range": "± 56",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 17775,
            "range": "± 667",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 75469,
            "range": "± 342",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 491,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4057,
            "range": "± 253",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 16096,
            "range": "± 49",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 24,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 24,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 24,
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
          "id": "5d6be329a6334a1d1516ada5d0b04a6678635008",
          "message": "feat(cli): s4 migrate — 既存バケットの遡及圧縮ツール\n\n非圧縮オブジェクトを S4F2 framed 形式へバルク変換する subcommand。\ngateway PUT 経路と同じ building blocks (streaming_compress_to_frames /\npick_chunk_size / build_index_from_body) を呼ぶことで読み手互換を担保し、\ne2e で in-process gateway readback + verify-sidecar Ok を証明。\n\n- デフォルト dry-run、--execute で実行 (sweep-orphan-sidecars と同じ文化)\n- S4F2/S4P1 magic 検出で already-s4 を skip = 再実行が自動 resume\n- decompress roundtrip 検証成功時のみ上書き PUT (無効化フラグなし)\n- HEAD ETag 確認で race を縮小 (完全防止でない旨 doc 明記)\n- versioning Enabled バケットへ二重課金警告 / SSE 構成は明示拒否\n- service.rs はメタデータ定数 5 個の pub(crate) 化のみ (挙動不変)\n\ntests: lib 509 pass (migrate unit 8 本) / minio e2e 2 本 green\n(orchestrator 再実行済) / clippy -D warnings / fmt clean。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T00:55:55+09:00",
          "tree_id": "a55b9946dd155cffed9e9e90fea4895faebcbb73",
          "url": "https://github.com/abyo-software/s4/commit/5d6be329a6334a1d1516ada5d0b04a6678635008"
        },
        "date": 1781107476640,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 47037,
            "range": "± 1815",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57576,
            "range": "± 835",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 428,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2225112,
            "range": "± 16952",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50630666,
            "range": "± 345685",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201743,
            "range": "± 398",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49042228,
            "range": "± 238461",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 924632292,
            "range": "± 2970808",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3250139,
            "range": "± 29231",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 28408,
            "range": "± 3959",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 33173,
            "range": "± 1239",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 423,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 573729,
            "range": "± 4288",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1640972,
            "range": "± 9075",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201618,
            "range": "± 854",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12144008,
            "range": "± 152383",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28657016,
            "range": "± 501032",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3234710,
            "range": "± 80463",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1473066,
            "range": "± 19506",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2135581,
            "range": "± 16086",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 323547877,
            "range": "± 16448559",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 9169,
            "range": "± 35",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 889,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13780,
            "range": "± 38",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2755,
            "range": "± 72",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21391,
            "range": "± 483",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 85316,
            "range": "± 402",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 634,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5247,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20737,
            "range": "± 907",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 31,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 31,
            "range": "± 0",
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
          "id": "663bf9b12040518dbf2ddaa10fb53dea467fba89",
          "message": "feat(codec): zstd dictionary 圧縮 — 小オブジェクトの共有辞書 (s4 train-dict + --zstd-dict)\n\n数 KB の同種小オブジェクト (JSON イベント等) を prefix 訓練済み共有辞書で\n圧縮する。e2e 実測 2.46× (100 個の同 schema JSON、dict 8,903 B vs plain\n21,923 B)。\n\n- CodecKind::CpuZstdDict (id 8、additive)。S4F2 レイアウト不変、辞書 ID\n  は s4-dict-id メタデータ、辞書本体は .s4dict/<sha256 先頭 16hex>\n  (content-addressed / immutable / listing から隠蔽)\n- s4 train-dict <bucket>/<prefix>: backend 直結で訓練 → .s4dict PUT →\n  gateway 起動 flag を出力\n- --zstd-dict <bucket>/<prefix>=<id> (repeatable) + --zstd-dict-max-bytes\n  (1 MiB)。無指定時は bit-for-bit 不変。dict が plain に劣後したら自動\n  fallback。GET は flag 無しでも lazy fetch + LRU(16) + fingerprint 検証で\n  読める (fail-closed)\n- 旧 reader (≤v1.0) は UnknownCodec(8) の typed failure (silent corruption\n  なし、CHANGELOG 明記)。外部復号レシピ (zstd CLI -D) を実測検証して README 化\n- fuzz: frame_roundtrip_bolero の codec_id 範囲を 0..9 に拡張 (6=gdeflate /\n  7=gzip は追加時から漏れていた)\n\ntests: s4-codec 90+ / s4-server lib 520 / dict minio e2e green\n(orchestrator 再実行済) / clippy -D warnings / fmt clean。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T01:23:21+09:00",
          "tree_id": "22a40531203d08099cbc9fbfa3e22fd45ab26dc3",
          "url": "https://github.com/abyo-software/s4/commit/663bf9b12040518dbf2ddaa10fb53dea467fba89"
        },
        "date": 1781109078750,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 55352,
            "range": "± 3659",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 56441,
            "range": "± 3521",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 367,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2672295,
            "range": "± 117025",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 42467223,
            "range": "± 131883",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192211,
            "range": "± 1053",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52877314,
            "range": "± 949425",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 769972167,
            "range": "± 2134323",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3143155,
            "range": "± 25719",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32673,
            "range": "± 3373",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 37061,
            "range": "± 2918",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 378,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 576386,
            "range": "± 18387",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1547898,
            "range": "± 18228",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 191993,
            "range": "± 306",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13968639,
            "range": "± 397807",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27932097,
            "range": "± 429607",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3131526,
            "range": "± 31234",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1517982,
            "range": "± 34164",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2514983,
            "range": "± 18430",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 361248277,
            "range": "± 11700294",
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
            "value": 13603,
            "range": "± 48",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 803,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12756,
            "range": "± 34",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3213,
            "range": "± 100",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 25356,
            "range": "± 619",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 101101,
            "range": "± 1010",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 595,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4895,
            "range": "± 33",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19507,
            "range": "± 54",
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
          "id": "386234e35e8a3a5f423cb73b96d711f909afef62",
          "message": "feat(python): s4fs — fsspec FileSystem アダプタ (gateway 無し直接読み出し)\n\npandas / pyarrow / DuckDB / Polars がバックエンド上の S4 オブジェクトを\ngateway 無しで読める read-only fsspec 実装。lock-in 不安への直接回答。\n\n- s4-codec-py (additive): read_frame / frame_iter / decode_index /\n  CpuZstdDict / crc32c binding + 定数・例外型。既存 API 不変\n- python/s4fs (新規): S4FileSystem (protocol \"s4\"、entry point 登録、\n  underlying fs 注入可)。ls/info はオリジナルサイズ返却 (sidecar →\n  metadata の順、不明時は s4_size_exact: False)。range read は sidecar\n  駆動 partial fetch (ETag staleness check + full fallback)。dict は\n  .s4dict fetch + fingerprint 検証。nvcomp-*/SSE/書込は明示エラー\n- fixture は実 gateway が書いたバイト列 (456 KB、生成手順併記)。\n  unframed manifest 経路 (cpu-gzip / passthrough) も実 fixture で発覚し対応\n\ntests: binding 23 + s4fs unit 33 + minio e2e 8 (pyarrow read_table /\npandas s4:// / DuckDB / range read 転送量 assert 含む、orchestrator\n再実行済) / cargo clippy -D warnings / fmt clean / python -m build OK。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T01:59:49+09:00",
          "tree_id": "f8b8f8c6d4efcf0fb97f97e3e6fc0e72bd37c563",
          "url": "https://github.com/abyo-software/s4/commit/386234e35e8a3a5f423cb73b96d711f909afef62"
        },
        "date": 1781111274248,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48404,
            "range": "± 1455",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57020,
            "range": "± 1663",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 426,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2211440,
            "range": "± 72118",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50582798,
            "range": "± 968628",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201524,
            "range": "± 430",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49987989,
            "range": "± 1292815",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 923435991,
            "range": "± 4179638",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3296270,
            "range": "± 33700",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 28121,
            "range": "± 1028",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 33230,
            "range": "± 3675",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 418,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 587172,
            "range": "± 3021",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1641793,
            "range": "± 39776",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201507,
            "range": "± 781",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12349183,
            "range": "± 98357",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28783113,
            "range": "± 116527",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3219693,
            "range": "± 50001",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1452331,
            "range": "± 17930",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2119400,
            "range": "± 66000",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 316992648,
            "range": "± 6819317",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 135,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 9281,
            "range": "± 48",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 905,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14045,
            "range": "± 124",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2931,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22752,
            "range": "± 785",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90858,
            "range": "± 117",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 634,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5218,
            "range": "± 117",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20801,
            "range": "± 351",
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
            "range": "± 0",
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
          "id": "367ddfcf102d51e7ed267a25157fade3ea0cfbe2",
          "message": "feat(cli): s4 recompact — 低アクセス帯の高圧縮焼き直し (コンパクション)\n\n書き込み時 cpu-zstd-3 で入った framed オブジェクトを夜間等に\n--target-zstd-level (default 19) へ再圧縮する。e2e 実測で zstd-3 比\n~14% 追加削減。LSM compaction の S3 版。\n\n- 対象は S4F2 framed の cpu-zstd のみ (非 framed は 'run s4 migrate\n  first' ヒント付き skip、nvcomp/gzip/passthrough/dict は\n  unsupported-codec skip)\n- --older-than 30d (LastModified 基準、自作 duration パーサ) /\n  --min-gain-percent 3.0 未満は skip\n- s4-zstd-level メタデータ stamp で再実行が already-compacted skip\n  (冪等)。dry-run デフォルト + --execute は migrate と同じ文化\n- decode 検証 → 再圧縮 → roundtrip 検証 → ETag HEAD guard → 再 PUT。\n  単一 frame 化時は stale sidecar を掃除\n- migrate.rs は helper 5 個の pub(crate) 化のみ (挙動不変)\n\ntests: lib 531 pass (recompact unit 11 本) / recompact + migrate minio\ne2e green (orchestrator 再実行済) / clippy -D warnings / fmt clean。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T02:21:00+09:00",
          "tree_id": "89fed5e82fde7ae7d4f81633dc50ca0cf3b1dac0",
          "url": "https://github.com/abyo-software/s4/commit/367ddfcf102d51e7ed267a25157fade3ea0cfbe2"
        },
        "date": 1781112547017,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 49654,
            "range": "± 2923",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57171,
            "range": "± 3163",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 427,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2276611,
            "range": "± 89304",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50571429,
            "range": "± 335690",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201390,
            "range": "± 237",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49844048,
            "range": "± 907028",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 921829652,
            "range": "± 3356497",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3221337,
            "range": "± 29446",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27160,
            "range": "± 1056",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32728,
            "range": "± 1248",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 419,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 577529,
            "range": "± 12648",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1636418,
            "range": "± 46928",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201444,
            "range": "± 642",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 11151659,
            "range": "± 59278",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27613824,
            "range": "± 237912",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3217904,
            "range": "± 44001",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1452027,
            "range": "± 36940",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2257479,
            "range": "± 70206",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 319259761,
            "range": "± 6703062",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7168,
            "range": "± 126",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 900,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13985,
            "range": "± 29",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2933,
            "range": "± 42",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22817,
            "range": "± 108",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90924,
            "range": "± 913",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 632,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5258,
            "range": "± 85",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20991,
            "range": "± 73",
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
            "range": "± 1",
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
          "id": "e3fa5f8fa95c9cc29a1f5fbc732c42c7d69e046c",
          "message": "feat(gpu): nvCOMP batched API — 小オブジェクト PUT のバッチ GPU 圧縮 (--gpu-batch-small-puts)\n\n複数同時 PUT を 1 回の batched kernel launch にまとめ、--gpu-min-bytes\n未満の小オブジェクト ingest でも GPU を使えるようにする。\n\n- wire format 影響ゼロ: 既存 per-object 経路も内部は nvcompBatched* +\n  FCG1 frame であり、新 encoder は同一 write_header で per-object に\n  同一形式を組み立てる。batched で PUT → 無変更 GET 経路で byte 一致を\n  GPU 実機 e2e で証明\n- gpu_batch.rs: mpsc+oneshot アグリゲータ (--gpu-batch-max-items 32 /\n  --gpu-batch-window-ms 4 / floor 4 KiB)、queue full は per-object へ\n  フォールバック。flag off (default) で bit-for-bit 不変\n- ratio 劣化時は cpu-zstd へ自動フォールバック (二重圧縮コストは\n  s4_gpu_batch_total{result} metric で観測可)\n- 実測 (RTX 4070 Ti SUPER, 1000×8 KiB): per-object GPU 比 ~24× 改善・\n  CPU 比 ratio +10%。ただし wall time は cpu-zstd-3 順次が 1.5-1.9× 速い\n  — 価値は CPU 飽和時のオフロード + ratio (README にこの通り明記)\n\ntests: CPU 側 lib 538 / codec 128 / clippy -D warnings / fmt clean。\nGPU 側 batched 互換 5 本 + minio e2e 3 本 green (orchestrator 再実行済、\nLD_LIBRARY_PATH=$NVCOMP_HOME/lib が必要)。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T02:48:40+09:00",
          "tree_id": "786040e6b5740a140e9f104350eb13488ce971f8",
          "url": "https://github.com/abyo-software/s4/commit/e3fa5f8fa95c9cc29a1f5fbc732c42c7d69e046c"
        },
        "date": 1781114212890,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48316,
            "range": "± 4077",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 56316,
            "range": "± 958",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 427,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2206110,
            "range": "± 14118",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50519599,
            "range": "± 142984",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201267,
            "range": "± 280",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 49784916,
            "range": "± 1069311",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 921894554,
            "range": "± 1499502",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3218421,
            "range": "± 5501",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 26996,
            "range": "± 1112",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 31913,
            "range": "± 939",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 418,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 574826,
            "range": "± 2500",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1637348,
            "range": "± 21281",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201182,
            "range": "± 174",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12110926,
            "range": "± 72917",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28767051,
            "range": "± 124456",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3224036,
            "range": "± 10054",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1440827,
            "range": "± 27419",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2126650,
            "range": "± 14999",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 312611605,
            "range": "± 1101443",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 153,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8368,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 912,
            "range": "± 22",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14037,
            "range": "± 46",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2934,
            "range": "± 82",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 22794,
            "range": "± 34",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90933,
            "range": "± 759",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 597,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4791,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19143,
            "range": "± 531",
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
            "range": "± 0",
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
          "id": "e75925bca95d872bf5b55a1d02d71fed2b1c5eb4",
          "message": "fix(audit-r1): round-1 監査 fix wave — P1×4 / P2×8 / P3×8 closure\n\n4 レビュアー (Claude×3 + Codex) による v1.0.0..HEAD 監査。Codex と\nClaude が独立に同一 P1 2 件を発見 (dual-reviewer の再実証)。\n\nP1: migrate の .s4dict 再圧縮破壊 / client 供給 s4-dict-id での\nflag 無指定 GET 5xx (freeze 違反) / s4fs の SSE 暗号文無音返却 /\ns4fs の .__s4ver__ shadow 混入。\nP2: storage class + tags の rewrite 喪失 / migrate verify 失敗 exit 0 /\ncross-bucket copy の dict 不伝播 / .s4dict reserved-key guard /\ns4fs stale·v1 sidecar 信頼 / s4fs 依存 floor。\nP3: estimate 404 abort / max-body-bytes 後置 / unstamped-framed 昇格 /\ndict cache bucket scope + full-sha 検証 + lazy cap 1 MiB /\nnvcomp_batched device size 検証 / inexact open truncate 他。\n\n詳細は CHANGELOG [Unreleased] Fixed 節。\n\ngates: lib 545 / codec 90 / minio e2e 4 系統 (estimate 1, migrate 3,\nrecompact 1, dict 1) / python unit 72 + e2e 9 (SSE 実 gateway 検証含む)\n/ GPU 実機 5 / clippy -D warnings / fmt — 全 green (orchestrator 再実行済)。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T11:16:22+09:00",
          "tree_id": "ec4b4194916e34a60c5fa46f6e917358bf2098ac",
          "url": "https://github.com/abyo-software/s4/commit/e75925bca95d872bf5b55a1d02d71fed2b1c5eb4"
        },
        "date": 1781144663933,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 48460,
            "range": "± 2190",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 55706,
            "range": "± 1059",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 429,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2257638,
            "range": "± 30299",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 50661338,
            "range": "± 304131",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 201335,
            "range": "± 242",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51188299,
            "range": "± 1067129",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 924070260,
            "range": "± 1927542",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3223238,
            "range": "± 14830",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 27344,
            "range": "± 12362",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32817,
            "range": "± 1069",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 421,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 576990,
            "range": "± 2768",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1641652,
            "range": "± 20436",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 201208,
            "range": "± 241",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12220507,
            "range": "± 78164",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 28747189,
            "range": "± 1455579",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3245670,
            "range": "± 27360",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1452691,
            "range": "± 32539",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2158221,
            "range": "± 18549",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 322144528,
            "range": "± 3310455",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 136,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 35258,
            "range": "± 26",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 961,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 14479,
            "range": "± 24",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2932,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 21368,
            "range": "± 1538",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 90850,
            "range": "± 1006",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 632,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 5224,
            "range": "± 23",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20735,
            "range": "± 29",
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
            "range": "± 0",
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
          "id": "dff433bfa227034da43a3cbbfb11c705c9a7bd81",
          "message": "fix(audit-r2): round-2 監査 fix wave — P2×3 / P3×4 closure\n\nRound-1 fix の敵対的検証 (Claude×2 + Codex)。multipart 経由の s4-*\nmetadata smuggle (round-1 PUT fix の再発形)、tagging 権限なし backend\nでの migrate 全滅リスク、versionId 付き CopyObject の probe 取り違え、\ndict サイズ cap の契約不整合、boot preload の bucket スコープ +\nfull-sha 検証、estimate の already-s4 除外、s4fs の HEAD 倍増解消。\n\n詳細は CHANGELOG [Unreleased] Fixed (audit round 2) 節。\n\ngates: lib 553 / minio e2e 5 系統 12 本 (versioned-copy 新規含む) /\npython unit 72 + e2e 9 / clippy -D warnings / fmt — 全 green。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T11:49:42+09:00",
          "tree_id": "702e965ee1b5e405e41ce7b2778eb74c7388fdbe",
          "url": "https://github.com/abyo-software/s4/commit/dff433bfa227034da43a3cbbfb11c705c9a7bd81"
        },
        "date": 1781146640868,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 54579,
            "range": "± 3821",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 54152,
            "range": "± 3490",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 363,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2663777,
            "range": "± 38931",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 42398482,
            "range": "± 774053",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192316,
            "range": "± 809",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51163712,
            "range": "± 1367318",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 767356153,
            "range": "± 2896352",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3063096,
            "range": "± 7471",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 31734,
            "range": "± 3078",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38517,
            "range": "± 2449",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 377,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 582941,
            "range": "± 14619",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1578110,
            "range": "± 80688",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192346,
            "range": "± 873",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12458035,
            "range": "± 76125",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27627241,
            "range": "± 161322",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3116874,
            "range": "± 17607",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1589431,
            "range": "± 37193",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2489839,
            "range": "± 16767",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 350971712,
            "range": "± 8786598",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 138,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 7365,
            "range": "± 51",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 818,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 13033,
            "range": "± 419",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3220,
            "range": "± 62",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 25324,
            "range": "± 464",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 101051,
            "range": "± 1801",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 637,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4909,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19807,
            "range": "± 74",
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
          "id": "37b7a20f0c140f28fa8e4c34cb81bc9990a7e8a0",
          "message": "fix(audit-r3): round-3 収束措置 — 残 P3 closure (CONVERGED)\n\n4 レビュアー × 3 round の監査が収束 (findings 20 → 7 → 5、P1/P2 は\nround 3 で 0)。残 P3 の措置:\n\n- estimate の already-s4 判定を構造検証付きに (S4F2 は codec id +\n  payload サイズ整合、S4P1 は padding 長整合を要求) — 4-byte magic\n  偶然一致の顧客データがサンプリングから無音脱落しないように。\n  e2e fixture も本物のフレームに更新\n- README/CHANGELOG の round-1/2 fix 由来の drift 解消: dict 1 MiB cap\n  の 3 面契約化、migrate/recompact 出力例の skip taxonomy 完全化、\n  --no-tags / tags-unreadable / estimate already-s4 除外の文書化\n- s4fs: _sidecar_matches の cache semantics を docstring/CHANGELOG で\n  開示 + info() が snapshot を seed して初回 2 HEAD を解消\n\ngates: lib 553 / estimate e2e 2 / python unit 49 + e2e 9 / clippy\n-D warnings / fmt — 全 green。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T12:06:36+09:00",
          "tree_id": "c91c691662b82739d01fc6d30374d5e658fc4cb1",
          "url": "https://github.com/abyo-software/s4/commit/37b7a20f0c140f28fa8e4c34cb81bc9990a7e8a0"
        },
        "date": 1781147664471,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 55761,
            "range": "± 3950",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 57587,
            "range": "± 3134",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 363,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2661683,
            "range": "± 49619",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 42424039,
            "range": "± 95435",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192123,
            "range": "± 461",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 52433387,
            "range": "± 767000",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 767539798,
            "range": "± 1914784",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3065606,
            "range": "± 4972",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 33383,
            "range": "± 3068",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38440,
            "range": "± 2652",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 376,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 614756,
            "range": "± 6052",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1552993,
            "range": "± 10575",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 191940,
            "range": "± 422",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 12252676,
            "range": "± 114359",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27098264,
            "range": "± 130070",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3070011,
            "range": "± 6609",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1578285,
            "range": "± 39232",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2490767,
            "range": "± 24896",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 341082567,
            "range": "± 2192600",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 138,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8548,
            "range": "± 53",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 800,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12717,
            "range": "± 25",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3120,
            "range": "± 67",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 25252,
            "range": "± 418",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 97648,
            "range": "± 1421",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 591,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4946,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19608,
            "range": "± 32",
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
          "id": "09d1966e6f32c8390d914803ec4fbe22669ed33b",
          "message": "chore(release): cut v1.1.0 — adoption tooling + small-object compression\n\nv1.1 の 6 機能 (estimate / migrate / zstd dict + train-dict / s4fs /\nrecompact / GPU batched small-PUT) + 3-round dual-reviewer 監査\n(findings 20 → 7 → 5、R3 で P1/P2 ゼロ収束)。v1.0 freeze 契約は維持 —\n全て additive / default-off、flag 無指定の PUT/GET は bit-for-bit 不変。\n\nAtomic bump:\n- Cargo.toml: workspace.version 1.0.0 → 1.1.0 (+ Cargo.lock)\n- charts/s4/Chart.yaml: appVersion 1.1.0 / chart 0.2.4 (appVersion\n  bump only)\n- python/s4fs/pyproject.toml: 1.1.0 (s4-codec>=1.1.0 floor と整合)\n- README: image-tag 例 1.0.0 → 1.1.0\n- CHANGELOG: [Unreleased] → [1.1.0] — 2026-06-11\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T12:34:10+09:00",
          "tree_id": "87688b1a34c568a07bffe4813e7aa03cc516113c",
          "url": "https://github.com/abyo-software/s4/commit/09d1966e6f32c8390d914803ec4fbe22669ed33b"
        },
        "date": 1781149311186,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 56674,
            "range": "± 4197",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 52658,
            "range": "± 2957",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 367,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2562039,
            "range": "± 79713",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 42333821,
            "range": "± 458614",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192198,
            "range": "± 480",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51839363,
            "range": "± 1372532",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 767447336,
            "range": "± 1873018",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3072711,
            "range": "± 74399",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 31080,
            "range": "± 2903",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 36577,
            "range": "± 3184",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 381,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 569287,
            "range": "± 6052",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1543943,
            "range": "± 10315",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192181,
            "range": "± 298",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13465933,
            "range": "± 514626",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27131062,
            "range": "± 104850",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3101392,
            "range": "± 29129",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1583246,
            "range": "± 15801",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2615451,
            "range": "± 49087",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 346404763,
            "range": "± 3280644",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 140,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 8094,
            "range": "± 79",
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
            "value": 12613,
            "range": "± 35",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3224,
            "range": "± 305",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 24964,
            "range": "± 57",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 99700,
            "range": "± 223",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 589,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4973,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 20142,
            "range": "± 64",
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
          "id": "002f3af9cf26caa3456b105d4dd0dd63e75679dc",
          "message": "fix(s4fs): __version__ を dist metadata から動的解決\n\n1.1.0 wheel が __version__ == \"1.0.0\" を報告していた (ハードコード\nbump 漏れ、post-release 検証で発見)。pyproject.toml を単一真実源に。\n公開済み wheel の dist metadata 自体は 1.1.0 で正しい (cosmetic)。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T12:49:51+09:00",
          "tree_id": "07411805239c62fdefb6b1ba1fc41664741d8fec",
          "url": "https://github.com/abyo-software/s4/commit/002f3af9cf26caa3456b105d4dd0dd63e75679dc"
        },
        "date": 1781150263932,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 56672,
            "range": "± 3562",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 55711,
            "range": "± 2919",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 364,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2628891,
            "range": "± 12910",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 42413872,
            "range": "± 112960",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 192291,
            "range": "± 2920",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 51684697,
            "range": "± 786769",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 767375587,
            "range": "± 646510",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 3065075,
            "range": "± 8972",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 32162,
            "range": "± 2336",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 38628,
            "range": "± 2250",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 376,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 613242,
            "range": "± 12964",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1560663,
            "range": "± 33305",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 192395,
            "range": "± 3357",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13161217,
            "range": "± 392519",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 27124031,
            "range": "± 361422",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 3072366,
            "range": "± 14658",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1492543,
            "range": "± 28124",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2499905,
            "range": "± 41975",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 342595409,
            "range": "± 2969031",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 140,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 11569,
            "range": "± 69",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 800,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12730,
            "range": "± 42",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 3078,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 24114,
            "range": "± 1378",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 95871,
            "range": "± 363",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 636,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4491,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 17869,
            "range": "± 103",
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
          "id": "4f302785fbbf3b3b23cf775f68720386d019bd23",
          "message": "feat(server): savings ledger — 実績節約額の継続可視化 (--savings-ledger-state-file + s4 savings)\n\nestimate (予測) の対になる実績側。per-bucket の original/stored/objects\nを gateway が累積し、state file 永続化 + Prometheus gauge +\ns4 savings CLI + Grafana dashboard で「実際いくら節約できているか」に\n常時答える。v1.2 の看板機能。\n\n- 更新点: PUT / multipart complete / copy / delete 全経路。削除・\n  上書きは破壊操作前の HEAD probe で減算 (metadata → sidecar →\n  original=stored fallback の対称設計、非 S4 オブジェクトの節約は 0 計上)\n- 永続化: 既存 state-file 流儀 (atomic tmp+rename、SIGUSR1 dump-back\n  にも参加)。flag 無指定はコードパス完全不変\n- s4 savings: state file 読むだけ (gateway 停止不要)、table|json\n- contrib/grafana/s4-savings-dashboard.json (metric 名 grep 検証済) +\n  docs/observability.md 更新\n- honest notes: gateway 経由の書き込みのみ観測 (migrate/recompact/\n  backend 直接は不可視)、ledger 有効時は write 系 1 req +1 HEAD\n\ntests: lib 564 / ledger minio e2e (実測 bytes 一致 + 再起動 reload +\nCLI 出力一致) / multipart + dict e2e 回帰 / clippy -D warnings / fmt\n— 全 green (orchestrator 再実行済)。\n\nCo-Authored-By: Claude Fable 5 <noreply@anthropic.com>",
          "timestamp": "2026-06-11T17:09:33+09:00",
          "tree_id": "a87d6ffec12348c0866cbaf4f95d264de0a0be25",
          "url": "https://github.com/abyo-software/s4/commit/4f302785fbbf3b3b23cf775f68720386d019bd23"
        },
        "date": 1781165825436,
        "tool": "cargo",
        "benches": [
          {
            "name": "compress/cpu_zstd_lvl3/1KiB",
            "value": 60444,
            "range": "± 5094",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1KiB",
            "value": 45924,
            "range": "± 1906",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1KiB",
            "value": 395,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/1MiB",
            "value": 2256685,
            "range": "± 41269",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/1MiB",
            "value": 28473997,
            "range": "± 184453",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/1MiB",
            "value": 152638,
            "range": "± 2117",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_zstd_lvl3/16MiB",
            "value": 48668566,
            "range": "± 491320",
            "unit": "ns/iter"
          },
          {
            "name": "compress/cpu_gzip_lvl6/16MiB",
            "value": 506423309,
            "range": "± 1685803",
            "unit": "ns/iter"
          },
          {
            "name": "compress/passthrough/16MiB",
            "value": 2519256,
            "range": "± 104694",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1KiB",
            "value": 28134,
            "range": "± 4337",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1KiB",
            "value": 32254,
            "range": "± 4141",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1KiB",
            "value": 395,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/1MiB",
            "value": 539053,
            "range": "± 6789",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/1MiB",
            "value": 1477786,
            "range": "± 71797",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/1MiB",
            "value": 152206,
            "range": "± 1919",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_zstd_lvl3/16MiB",
            "value": 13943535,
            "range": "± 658765",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/cpu_gzip_lvl6/16MiB",
            "value": 26802860,
            "range": "± 132904",
            "unit": "ns/iter"
          },
          {
            "name": "decompress/passthrough/16MiB",
            "value": 2499199,
            "range": "± 22006",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/1",
            "value": 1352692,
            "range": "± 60203",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/3",
            "value": 2141259,
            "range": "± 40093",
            "unit": "ns/iter"
          },
          {
            "name": "cpu_zstd_levels_1MiB/compress/22",
            "value": 416051472,
            "range": "± 4039660",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/4KiB",
            "value": 141,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "write_frame/single/256KiB",
            "value": 5869,
            "range": "± 53",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/16f_64KiB",
            "value": 810,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "frame_iter/256f_4KiB",
            "value": 12693,
            "range": "± 81",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/128f",
            "value": 2464,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/1024f",
            "value": 19019,
            "range": "± 375",
            "unit": "ns/iter"
          },
          {
            "name": "encode_index/4096f",
            "value": 75780,
            "range": "± 134",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/128f",
            "value": 557,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/1024f",
            "value": 4526,
            "range": "± 26",
            "unit": "ns/iter"
          },
          {
            "name": "decode_index/4096f",
            "value": 19237,
            "range": "± 103",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/small_head",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/mid_16MiB",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "lookup_range_1024f/span_256MiB",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}
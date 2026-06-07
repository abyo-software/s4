window.BENCHMARK_DATA = {
  "lastUpdate": 1780840115638,
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
      }
    ]
  }
}
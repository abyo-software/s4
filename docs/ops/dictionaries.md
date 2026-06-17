# Shared zstd dictionaries for small objects

### Shared zstd dictionaries for small objects (`s4 train-dict` + `--zstd-dict`)

Single-digit-KiB objects (JSON events, per-line log PUTs, small API
payloads) barely compress with plain zstd — the window never sees
redundancy *across* objects. A **shared dictionary** trained on a sample
of similar objects moves that redundancy out of band; each object then
compresses against the dictionary. Three steps:

```bash
# 1. Train from existing small objects (backend-direct tool, like migrate).
#    Writes the dictionary to `.s4dict/<dict-id>` inside the bucket and
#    prints the gateway flag.
s4 train-dict mybucket/events/ --endpoint-url https://s3.example.com
#   → --zstd-dict 'mybucket/events/=0123456789abcdef'

# 2. Start the gateway with the printed mapping (repeatable per prefix).
s4 --endpoint-url https://s3.example.com \
   --zstd-dict 'mybucket/events/=0123456789abcdef'

# 3. Confirm the effect: codec label `cpu-zstd-dict` in the access log /
#    `s4_requests_total{codec="cpu-zstd-dict"}`, and backend object sizes.
```

Measured effect (minio E2E `dict_minio.rs`, 100 × ~300-byte JSON events
of identical schema): **8 903 bytes stored with the dictionary vs
21 923 bytes with plain cpu-zstd — 2.46× smaller (40 % of the dict-less
size)**. The win scales with how homogeneous the objects are; on
heterogeneous prefixes the dictionary won't beat plain zstd, and the
gateway then **falls back to plain cpu-zstd automatically** (both are
compressed and compared per PUT — affordable because the path is capped
at `--zstd-dict-max-bytes`, default 1 MiB).

Mechanics and operational notes:

- **When the dict path applies**: dispatcher picked `cpu-zstd` + key
  longest-prefix-matches a configured `<bucket>/<prefix>` + declared
  `Content-Length` ≤ `--zstd-dict-max-bytes`. Everything else — and
  *every* PUT when no `--zstd-dict` flag is set — is bit-for-bit
  unchanged. Multipart uploads and chunked uploads without a
  Content-Length never take the dict path.
- **Wire format is additive**: the object is a normal single-frame S4F2
  body whose frame carries the new codec id 8 (`cpu-zstd-dict`); the
  dictionary id travels in the `s4-dict-id` object-metadata key. The
  S4F2 layout itself is unchanged.
- **Pre-v1.1 readers** (older gateway / `s4-codec` builds) fail a GET of
  a dict-compressed object with the existing *unknown codec id* error —
  a clean, typed failure, not silent corruption. Roll gateways forward
  before enabling the flag if you run mixed fleets.
- **Dropping the flag doesn't strand data**: a gateway booted without
  `--zstd-dict` lazily fetches `.s4dict/<id>` from the object's bucket
  on first GET (fingerprint-verified, small LRU cache; failures surface
  as 5xx + `s4_dict_fetch_total{result="err"}`).
- **`.s4dict/<dict-id>`** is hidden from gateway listings, named by the
  SHA-256 prefix of its bytes (content-addressed, immutable; re-training
  the same corpus is idempotent).
- **No lock-in**: the stored payload is a **stock zstd frame** and the
  dictionary object is **raw zstd dictionary bytes**. Decode without any
  S4 software (the E2E pins this recipe against the real `zstd` CLI):

  ```bash
  # strip the 28-byte S4F2 frame header, then:
  aws s3 cp s3://mybucket/.s4dict/0123456789abcdef dict.bin
  zstd -D dict.bin -d payload.zst -o original.json
  # python: zstandard.ZstdDecompressor(dict_data=ZstdCompressionDict(dict.bin))
  ```

- **Dictionaries are bucket-local.** GET resolves `.s4dict/<id>` from
  the *object's own bucket*. Cross-bucket CopyObject through the
  gateway propagates the dictionary to the destination bucket
  automatically (content-addressed, idempotent); **cross-region
  replication (experimental) does not** — place the dictionary in the
  replica bucket yourself or its dict-compressed replicas fail GET
  with a typed 5xx. `.s4dict/` keys are write-protected through the
  gateway (`InvalidObjectName` on PUT/DELETE, reads allowed); manage
  dictionaries with `s4 train-dict` against the backend. `train-dict`
  also stamps the full digest as `s4-dict-sha256` metadata, which the
  lazy-fetch path verifies when present (pre-existing dictionaries
  without the stamp fall back to the 16-hex prefix check). Dictionary
  size is one 1 MiB contract enforced at all three surfaces:
  `train-dict --max-dict-bytes` rejects above-cap requests, boot-time
  `--zstd-dict` preload refuses an above-cap dictionary, and the
  flag-less lazy fetch refuses it too — so a dictionary that works with
  the flag can never become unreadable without it.
- **Reserved metadata namespace**: the gateway strips client-supplied
  `x-amz-meta-s4-*` keys on PUT — they are S4's manifest namespace and
  forging them (e.g. a stray `s4-dict-id`) must not change GET behavior.
- **Scope-outs (follow-ups)**: `s4-codec-wasm` doesn't decode
  `cpu-zstd-dict` natively yet (`s4-codec-py` does, via the
  `CpuZstdDict` binding — s4fs uses it). Multipart uploads are out of
  scope **by design**, not as a follow-up: parts never consult the
  dictionary store, and S3's 5 MiB minimum part size sits far above the
  small-object ceiling (`--zstd-dict-max-bytes`, default 1 MiB) the
  feature targets — the two size ranges never intersect. Re-training
  for schema drift no longer needs a restart — see the next section.

#### Operating dictionaries (`s4 dict-status` + `--zstd-dict-map` + SIGHUP)

Day-2 operations for the feature above: drift monitoring and
restart-less rotation.

- **Per-prefix health metrics**: the dict PUT branch exports
  `s4_dict_put_total{prefix,outcome="win"|"loss"}` and
  `s4_dict_put_bytes_total{prefix,kind="original"|"dict"|"plain"}` —
  both compression results are measured per PUT anyway, so the byte
  counters are exact whether the dictionary won or lost. Cardinality is
  bounded by the configured prefix count; without dict configuration
  the series are never registered. The gateway also self-monitors: when
  a prefix's rolling win rate over its last 100 dict-path PUTs drops
  below 0.5, it WARNs (at most once per prefix per hour) that the
  dictionary looks stale. SIGHUP map reloads are counted as
  `s4_dict_reload_total{result="ok"|"err"}`.
- **`s4 dict-status --metrics-url <URL>`** scrapes `/metrics` (plain
  HTTP GET, no auth headers, 10 s timeout — front an authenticated
  metrics endpoint yourself if you need one) and reports per-prefix win
  rate / effective compression ratio / lazy fetch errors; any prefix
  below `--warn-win-rate` (default 0.5) gets a warning and the command
  exits 1, so a cron job catches drift unattended (`--format json` for
  machines; a failed scrape also exits 1 — distinguish via stderr).
  Note the counters are **cumulative since gateway start**: right after
  a rotation fixes a stale dictionary, the prefix keeps reporting STALE
  until new wins outweigh the accumulated losses, and a prefix removed
  from the map keeps its last series until the gateway restarts. Measured output (minio E2E
  `dict_ops_minio.rs`: 30 matching JSON PUTs under `events/`, then
  random bodies under a deliberately mismatched `rand/` mapping):

  ```console
  $ s4 dict-status --metrics-url http://127.0.0.1:8014/metrics
  PREFIX                                      WIN   LOSS  WIN-RATE   ORIGINAL-BYTES     DICT-BYTES  DICT-RATIO
  dictops/events/                              30      0    100.0%             7440           1689       22.7%
  dictops/rand/                                 0     16      0.0%             6400           6608      103.2%  STALE
  lazy dict fetches: ok=0 err=0
  WARN prefix "dictops/rand/": win rate 0.00 over 16 dict-path PUT(s) is below 0.50 — dictionary may be stale; consider retraining (s4 train-dict)
  $ echo $?
  1
  ```

- **Restart-less rotation** (`--zstd-dict-map <FILE>` + SIGHUP): the
  TOML file is the reloadable twin of repeated `--zstd-dict` flags —
  same validation, same boot-time fetch + fingerprint verification,
  same 1 MiB dictionary cap (a prefix configured in both places is a
  boot error):

  ```toml
  # dict-map.toml
  [mappings]
  "mybucket/events/" = "0123456789abcdef"
  ```

  ```bash
  s4 --endpoint-url https://s3.example.com --zstd-dict-map dict-map.toml
  # rotate without a restart:
  s4 train-dict mybucket/events/ --endpoint-url https://s3.example.com  # → new dict-id
  $EDITOR dict-map.toml                                                 # point the prefix at it
  kill -HUP <pid>             # fetch + verify + atomic store swap
  ```

  A failed reload (unreadable file, bad TOML, missing `.s4dict/`
  object, fingerprint mismatch) keeps the **current** mappings live —
  ERROR log + `s4_dict_reload_total{result="err"}`, never a
  half-applied swap. In-flight requests finish on the generation they
  started with. Without `--zstd-dict-map`, SIGHUP does not touch
  dictionary configuration (the TLS cert reload on SIGHUP is
  independent and unchanged).

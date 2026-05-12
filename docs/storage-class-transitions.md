# Storage-class transitions and the S4 sidecar

S4 stores every compressed object as **two** S3 objects:

1. The main object at `<key>` — the `S4F2`-framed compressed payload.
2. A sidecar at `<key>.s4index` — a tiny index that lets Range GET
   fetch only the compressed bytes that cover the user-visible range,
   instead of the full object.

Both are plain S3 objects in the *same* bucket. From S3's point of
view they are unrelated keys; from S4's point of view they are a unit.
**Bucket lifecycle rules must move them together.** If S3 transitions
the main object to Glacier but leaves the sidecar in Standard (or vice
versa), a Range GET will silently degrade or fail:

- Main in Glacier + sidecar in Standard → S4 reads the sidecar fine,
  then the partial-fetch Range GET against the main object returns
  `InvalidObjectState` (Glacier requires restore).
- Main in Standard + sidecar in Glacier → S4 cannot read the sidecar,
  falls back to a full-object GET (correct but defeats the Range
  optimisation entirely; latency and egress regress sharply).
- Main in Standard-IA + sidecar in Standard → mostly works, but you
  pay IA retrieval on the data plane and Standard storage on the
  sidecar; billing surprises and no operational benefit.

The fix is a lifecycle filter that matches **both** keys, or that
matches everything (which trivially includes both). The trap is a
filter such as `"Filter": {"ObjectSizeGreaterThan": 131072}` that
moves only large objects: the sidecar is almost always under that
threshold and gets left behind.

---

## Pattern A — transition every object in the bucket to IA after 30 days (recommended default)

The simplest and safest configuration. `"Filter": {}` matches every
object, so the main payload **and** its `.s4index` sidecar transition
together.

```json
{
  "Rules": [
    {
      "ID": "s4-all-to-ia-30d",
      "Status": "Enabled",
      "Filter": {},
      "Transitions": [
        {
          "Days": 30,
          "StorageClass": "STANDARD_IA"
        }
      ]
    }
  ]
}
```

Apply with:

```bash
aws s3api put-bucket-lifecycle-configuration \
  --bucket my-s4-bucket \
  --lifecycle-configuration file://lifecycle-all-ia.json
```

This pattern is correct by construction: every key in the bucket —
main object, sidecar, multipart upload temp keys, anything — moves on
the same schedule.

---

## Pattern B — transition only a key prefix to Glacier after 60 days

If you want to scope the rule to a logical dataset (e.g. `foo/*`),
you must also transition `foo/*.s4index`. The standard
`Filter.Prefix` keyword is a *prefix* match, so a single prefix
covers both: `foo/bar.parquet` and `foo/bar.parquet.s4index` both
start with `foo/`.

```json
{
  "Rules": [
    {
      "ID": "s4-foo-to-glacier-60d",
      "Status": "Enabled",
      "Filter": {
        "Prefix": "foo/"
      },
      "Transitions": [
        {
          "Days": 60,
          "StorageClass": "GLACIER"
        }
      ]
    }
  ]
}
```

This works because the sidecar lives at `foo/bar.parquet.s4index` —
inside the `foo/` prefix — not in some sibling location.

---

## Anti-pattern — suffix-scoped rule that splits the pair

**Do NOT** write a rule that targets only the sidecar suffix, or only
the main suffix, without a paired rule for the other half:

```json
{
  "Rules": [
    {
      "ID": "BROKEN-only-sidecars-to-ia",
      "Status": "Enabled",
      "Filter": {
        "And": {
          "Prefix": "",
          "Tags": [],
          "ObjectSizeLessThan": 131072
        }
      },
      "Transitions": [{ "Days": 30, "StorageClass": "STANDARD_IA" }]
    }
  ]
}
```

The size filter above happens to catch `.s4index` files (they are
typically a few hundred bytes to tens of KiB) and *not* the main
payload — so after 30 days every sidecar lives in Standard-IA while
every main object stays in Standard. Range GET still works but you
pay IA retrieval on every sidecar fetch (which is the hot path). The
same trap applies to any filter that distinguishes main vs sidecar:
suffix matchers, size buckets, tag scopes, etc.

If you genuinely need different policies (e.g. main → Glacier,
sidecar stays warm so Range GET still works without a restore), write
**two explicit rules** and review them as a pair:

```json
{
  "Rules": [
    {
      "ID": "s4-main-to-glacier-90d",
      "Status": "Enabled",
      "Filter": { "Prefix": "" },
      "Transitions": [{ "Days": 90, "StorageClass": "GLACIER" }]
    },
    {
      "ID": "s4-sidecars-stay-warm",
      "Status": "Enabled",
      "Filter": { "Prefix": "" },
      "Transitions": []
    }
  ]
}
```

S3 picks the most-specific matching rule; the suffix `.s4index` of the
sidecar must be added back via tag-based filtering or by uploading
sidecars with a distinctive prefix. **This is advanced configuration
and we recommend Pattern A or B unless you have measured the cost
delta.**

---

## Verification recipe

After a transition has fired (check with
`aws s3api list-objects-v2 --bucket … --prefix foo/`), confirm both
files share a storage class:

```bash
aws s3api head-object --bucket my-s4-bucket --key foo/bar.parquet \
  --query 'StorageClass' --output text
aws s3api head-object --bucket my-s4-bucket --key foo/bar.parquet.s4index \
  --query 'StorageClass' --output text
```

Both calls should print the **same** value (`STANDARD_IA`, `GLACIER`,
`DEEP_ARCHIVE`, etc.). If they differ, your lifecycle filter is
splitting the pair — go back and audit it against Patterns A / B
above.

For a bucket-wide audit:

```bash
aws s3api list-objects-v2 --bucket my-s4-bucket \
  --query 'Contents[].[Key,StorageClass]' --output text \
  | awk '{
      if ($1 ~ /\.s4index$/) {
        sub(/\.s4index$/, "", $1); sidecar[$1]=$2
      } else {
        main[$1]=$2
      }
    }
    END {
      for (k in main) {
        if (k in sidecar && main[k] != sidecar[k]) {
          printf "DRIFT %s: main=%s sidecar=%s\n", k, main[k], sidecar[k]
        }
      }
    }'
```

Any line printed by the `awk` block is a key whose main payload and
sidecar drifted into different storage classes — fix the lifecycle
config and either wait for the next transition or run a one-off
`copy_object` to realign.

---

## Restore semantics (Glacier / Deep Archive)

If your lifecycle moves objects to `GLACIER` or `DEEP_ARCHIVE`, a
Range GET against an un-restored object returns `InvalidObjectState`
from the backend. S4 surfaces this verbatim. The recommended
workflow is:

1. Issue `restore-object` against `<key>` **and** `<key>.s4index`
   together.
2. Wait for the restore to complete (`x-amz-restore: ongoing-request="false"`).
3. Re-issue the Range GET — S4 will fetch the sidecar (now warm),
   compute the compressed-byte range, and Range-GET the main object.

Restoring only the main object without the sidecar leaves S4 in
full-read fallback (still correct, but slower and more expensive).
Restoring only the sidecar wastes the restore — the main object is
still cold. Always restore the pair.

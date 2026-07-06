# AWS Marketplace metering (RegisterUsage)

For teams running the **container** listing, **the Marketplace image and
the free ghcr.io image are the same binary** — the only difference is that
the Marketplace deployment passes `--marketplace-product-code <CODE>` (via
the chart's `marketplace.productCode` value), which makes each pod
register itself with the AWS Marketplace Metering Service at boot.

How it works (v1.3, opt-in):

- At startup — before the backend S3 client is even built — the gateway
  calls the Marketplace `RegisterUsage` API once with your product code.
  Success confirms your subscription and starts **per-pod, per-hour**
  metering on the AWS side; charges appear on your regular AWS invoice
  (AWS measures pod runtime automatically after the one-shot call — the
  gateway makes no further metering calls).
- Any final failure — not subscribed (`CustomerNotEntitled`), wrong code,
  or running outside ECS / EKS / Fargate (`PlatformNotSupported` — plain
  `docker run` and bare EC2 cannot meter) — aborts boot with a non-zero
  exit and a typed error, so a non-entitled pod crash-loops instead of
  serving. Only throttling / internal-service errors are retried
  (exponential backoff, 3 retries, per AWS guidance).
- The boot outcome is also exposed as the
  `s4_marketplace_register_usage_total{result}` counter (see
  `s4_server::marketplace` for the fail-closed semantics and the honest
  scope of response-signature checking).

Customer setup on EKS (IRSA):

1. Create an IAM role for the S4 service account whose policy allows
   `aws-marketplace:RegisterUsage` (see
   [charts/s4/README.md §AWS Marketplace](../../charts/s4/README.md#aws-marketplace-paid-container)
   for the JSON policy document).
2. Install with the product code from your Marketplace fulfillment page:

   ```bash
   helm install s4 ./charts/s4 \
     --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com \
     --set backend.region=us-east-1 \
     --set marketplace.productCode=<YOUR_PRODUCT_CODE> \
     --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::<ACCOUNT_ID>:role/s4-marketplace
   ```

Without `marketplace.productCode` (the default), no Marketplace code runs
at all and the gateway behaves bit-for-bit like the free OSS distribution
it is.

## Custom-dimension metering (`MeterUsage`) and metered savings

Products whose pricing defines a **custom ("externally metered")
dimension** (catalog `Dimensions[].Types` = `ExternallyMetered`) take the
`MeterUsage` route instead: pass `--marketplace-usage-dimension <KEY>`
(the dimension's API identifier, e.g. `PID1` — not the display name)
together with `--marketplace-product-code`. Boot performs a `DryRun`
`MeterUsage` entitlement check (fail-closed), then a background loop
sends one record per pod per hour (fail-open with ≤6 h backfill; see
`s4_server::marketplace` for the retry/backlog semantics and the
`s4_marketplace_meter_usage_total{result}` counter). The IAM policy
needs `aws-marketplace:MeterUsage`.

**Metered savings (v1.5, opt-in)** — `--marketplace-metered-savings`
switches the hourly quantity from the constant `1` pod-hour to the
storage the gateway is currently avoiding on the backend, in integer
GiB (`original_bytes − stored_bytes` from the savings ledger — the same
counters behind `s4 savings`). Requirements and semantics:

- Requires `--marketplace-usage-dimension` (a dimension priced per
  GiB-saved-hour) and `--savings-ledger-state-file` (the measurement
  source; the flag refuses to parse without it rather than silently
  metering 0 forever).
- It is a *stock* measure ("rent on savings"), captured once per hour;
  a backfilled hour bills the quantity captured at that hour, not the
  value at retry time.
- Failure modes err in the **customer's favor**: a lost/reset ledger
  (ephemeral pod storage) under-meters until the counters rebuild —
  give the ledger a persistent volume for accurate metering. Sub-GiB
  savings floor to 0.
- **Single metering gateway only.** Run exactly one replica with this
  flag: every metering replica bills the full stock its ledger reports,
  and AWS does not dedup `MeterUsage` across callers — N metering
  replicas means N× billing (the gateway WARNs about this at metering
  start). Multi-replica fleets should stay on per-pod hourly pricing
  until the fleet-aggregation follow-up lands.
- Design rationale, pricing arithmetic, and the listing-side rollout
  plan: [metered-savings-design.md](metered-savings-design.md). The
  listing change (new dimension) is a separate, seller-approved step —
  shipping this flag does not alter any live listing.

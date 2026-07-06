# Design: value-based Marketplace metering — `GBSavedHours` dimension

Status: **code shipped** (2026-07-06) — `--marketplace-metered-savings`,
off by default, inert without the flag; operator docs in
[metering.md](metering.md). The listing-side dimension is NOT submitted
to AWS Marketplace until explicitly approved by the seller (rollout
§ below).

## Problem

Today's paid listings price by runtime: per-instance-type hourly (AMIs)
or per-pod hourly (`RegisterUsage` / flat `MeterUsage` custom dimension,
see [metering.md](metering.md)). Runtime pricing under-charges heavy
users and — more importantly for adoption — over-charges small ones: a
team saving 40 GB pays the same as a team saving 400 TB. A usage
dimension that tracks the *value delivered* (bytes removed from the
backend bill) aligns price with savings and lowers the entry barrier.

## Dimension semantics: GB-saved-hours ("rent on savings")

Each hour, each gateway meters the **currently avoided storage** it is
responsible for, in integer GB:

```
quantity(hour) = floor((original_bytes - stored_bytes) / GiB)
```

taken from the savings ledger's global totals
(`s4_server::ledger::LedgerSnapshot::global_totals`, the same counters
behind `s4 savings`). This is a *stock* measure, like S3's own
GB-month pricing, prorated hourly — NOT an incremental "new GB saved
this hour" flow measure.

Why stock, not flow:

- **Restart-safe without double-billing.** A flow (delta) meter needs a
  durable last-metered checkpoint; a crash between meter and checkpoint
  either double-bills (seller error, unacceptable) or drops an hour.
  The stock meter is memoryless: every hour states "S4 is currently
  avoiding N GB", and a missed hour simply isn't billed (safe
  direction: customer's favor).
- **Value-aligned.** The customer's S3 bill benefit is also a stock
  (fewer GB-months stored). Price per GB-saved-hour can be set as a
  fraction of S3 Standard (`$0.023/GB-month ÷ 730 h ≈ $0.0000315/GB-h`);
  e.g. a 30% take is ~`$0.00001/GB-h` — the customer always keeps the
  majority of the savings.
- **Single metering gateway (v1.5 constraint).** ~~Multi-gateway
  additive~~ — withdrawn after review (2026-07-06): per-instance
  ledgers do sum for writes, but cross-pod DELETEs drift the per-pod
  stocks (the deleting pod's ledger never held the object), and a
  *shared* ledger file would bill the same stock once per replica (AWS
  does not dedup `MeterUsage` across callers). v1.5 therefore requires
  **exactly one metering replica** (the flag docs + a boot WARN state
  this); the primary target is the single-instance AMI deployment.
  Fleet-accurate aggregation (backend-scan derivation or a leader
  lease) is the documented follow-up before recommending the flag on
  multi-replica fleets.

### Known honest caveats

- **Ledger loss under-meters.** The ledger persists to the local state
  path; a pod restart with ephemeral storage resets it, and the metered
  quantity restarts from the bytes processed since. This errs in the
  customer's favor. Deployments that want accurate metering should give
  the ledger a persistent volume (AMI/EBS deployments get this free).
  `s4 maintain`-driven re-derivation of the ledger from a backend scan
  is the follow-up that closes this gap.
- **Deleted objects.** The ledger already applies deletion deltas
  (`apply_delta`), so avoided-GB shrinks when compressed objects are
  deleted. Objects deleted while the gateway was down are missed until
  a ledger re-derivation (same follow-up as above).
- **`MeterUsage` quantity is `i32` GB.** Saturate at `i32::MAX`
  (~2.1 exabytes avoided) — cap, don't wrap.

## Code shape (wave 2)

Reuses the existing `MeterUsage` route end to end — no new AWS calls,
no new crates:

1. New flag `--marketplace-metered-savings` (bool, default **off**),
   valid only together with `--marketplace-product-code` and
   `--marketplace-usage-dimension`. Off = today's behavior
   (quantity = 1 pod-hour), bit-for-bit.
2. The hourly loop in `main.rs` (search `meter_one_hour(`) computes the
   quantity from the ledger snapshot instead of the constant `1`.
   `meter_one_hour` already takes `quantity: i32`; no signature change.
3. The boot-time `DryRun` entitlement check is unchanged.
4. Log line gains the metered quantity; metrics: reuse
   `s4_marketplace_meter_usage_total{result}`. No new gauge — the
   quantity is derivable from the existing ledger gauges
   (`s4_ledger_original_bytes − s4_ledger_stored_bytes`), and the
   per-record value is in the success log line.
5. Tests: quantity derivation from a ledger snapshot (incl. zero /
   negative-drift → 0, saturation), flag-gating (off ⇒ constant 1),
   and an hourly-loop test with a scripted client asserting the sent
   quantities.

## Listing attempt result (2026-07-06): blocked by platform rule

Submission was approved and attempted (change sets
`aym10q5u2n4aepsnay66ztd0e`, `97gvwbhics2hsdn7i38s7todp` on
`prod-nimrbd77e4xfs` + `offer-quy32knls4zlk`): AWS rejects
`AddDimensions` with **"New dimension cannot be added to product having
hourly pricing"**. The live container product is registered under the
hourly-with-annual pricing model (its `Hours`/PID1 dimension is
`ExternallyMetered` only because container hourly meters via
`MeterUsage`), and that model's dimension set is fixed — this is a
model-level rule, so the seller portal cannot do it either. Two
validation details learned: dimension `Description` ≤ 90 chars;
`ChangeSetName` must match `^[\w\s+=.:@-]{1,100}$`.

Remaining paths (decision pending):
1. **New dedicated container listing** under the custom-metering model
   with `GBSavedHours` — clean but costs a full portal creation flow
   (repos must be created BY the portal, see
   `feedback-marketplace-repo-order`), AWS review, and splits the
   product's Marketplace presence.
2. **Hold until demand** — the gateway flag ships regardless; create
   the dedicated listing when a real customer asks for value-based
   pricing.
3. **Seller-support case** asking whether an existing hourly container
   product can be converted / gain a usage dimension (days of latency,
   likely "no").

## Listing shape (superseded by the above — kept for the dedicated-listing path)

- New custom dimension, catalog type `ExternallyMetered`, key e.g.
  `GBSavedHours`, unit-priced as a fraction of S3 Standard GB-month
  (see above; final price is a business decision).
- Rollout as a **new offer/pricing option alongside** the existing
  hourly pricing, not a replacement — existing subscribers see no
  change. AWS Marketplace pricing changes go through seller-portal
  review; competitor-reference and pricing-floor rules apply.
- The container listing's IAM policy documentation gains
  `aws-marketplace:MeterUsage` (already required for the existing
  custom-dimension route).

## Rollout order

1. Ship the code path (flag off by default) in a regular release; it is
   inert without the flag.
2. Validate on a Limited/test listing with a scripted subscriber
   account: confirm hourly records in the seller report match the
   ledger (`s4 savings`) within the hour granularity.
3. Only then submit the public listing change set (seller `as` profile,
   explicit approval per repo policy).

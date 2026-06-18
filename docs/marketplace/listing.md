# AWS Marketplace listing — source of truth

Status (verified 2026-06-19 via Marketplace Catalog API, seller account
393886308285) — the earlier "draft blocked on seller public profile /
AccessDeniedException" state is **resolved**; both products now exist as
catalog entities:

- **GPU AMI** — `prod-l5my73chs43y6` ("S4 - Squished S3: GPU S3 Compression
  Gateway (EC2 AMI)"): **Public / live** →
  https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i
- **Container** — `prod-nimrbd77e4xfs` ("S4 - Squished S3: Transparent S3
  Compression Gateway"): **Limited visibility / live via direct URL**
  (subscribable at
  https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e — not yet in
  public catalog search). The change set to broaden it,
  `fzadtfuuq65wav865rp2g9us`, has been in `PREPARING` since 2026-06-16; poll
  with `aws --profile as marketplace-catalog describe-change-set --catalog
  AWSMarketplace --change-set-id fzadtfuuq65wav865rp2g9us`.
- **Cleanup**: a stale duplicate container draft `prod-xhip4gvojbuuy`
  (Visibility: Draft) should be removed.

All claims below are grep-verified against the README / code at the
commit that last touched this file. Do not add numbers that are not
measured in the repo.

## Product (ContainerProduct@1.0)

- **ProductTitle**: `S4 - Squished S3: Transparent S3 Compression Gateway`
- **Sku**: `s4-gateway`
- **ShortDescription**:
  > Drop-in S3-compatible gateway that transparently compresses every
  > object (CPU zstd / GPU nvCOMP), cutting S3 storage bytes 50-80% for
  > compressible data with zero application changes. Includes
  > pre-deployment savings estimation and measured-savings reporting.
- **LongDescription**:
  > S4 sits between your applications (boto3, aws-sdk, Spark, Trino,
  > DuckDB - anything S3) and your S3 bucket, and transparently
  > compresses each object with a codec chosen per payload: CPU zstd for
  > text/logs, GPU (NVIDIA nvCOMP Bitcomp/zstd) for integer/columnar
  > data, passthrough for already-compressed inputs.
  >
  > No application changes: same S3 wire protocol, same SigV4 auth, same
  > SDK calls - just change the endpoint URL. Range GETs stay fast via a
  > sidecar frame index (Parquet/ORC reader compatible). The on-backend
  > format is open: stop the gateway and objects remain readable with
  > the Apache-2.0 s4-codec CLI, the s4-codec Python package, or the
  > s4fs fsspec adapter (pandas / pyarrow / DuckDB read without the
  > gateway).
  >
  > Operate it with the built-in day-2 tooling: `s4 estimate` projects
  > savings on your existing bucket before you deploy; `s4 migrate`
  > retro-compresses existing objects; `s4 recompact` re-compresses cold
  > data at higher levels; `s4 maintain` runs policy-driven maintenance;
  > the savings ledger + `s4 savings` report the measured storage bytes
  > and $ actually saved in production. Prometheus metrics and a Grafana
  > dashboard are included.
  >
  > This listing is billed per pod per hour. The same software is
  > available free under Apache-2.0 (ghcr.io image); the Marketplace
  > build adds metered billing through your AWS bill and is the
  > supported procurement path for enterprises.
- **Highlights** (3):
  1. `50-80% fewer S3 storage bytes for compressible data (logs, JSON, columnar) - measured, with per-bucket savings reporting built in`
  2. `Zero application changes: S3 wire-compatible endpoint (SigV4, multipart, Range GET, SSE) - point your SDK at S4 and keep working`
  3. `Open format, no lock-in: objects stay readable without the gateway via Apache-2.0 CLI / Python / fsspec tooling`
- **Categories**: `["Storage"]`
- **SearchKeywords**: `["S3", "compression", "storage cost reduction", "object storage", "zstd", "GPU", "gateway"]`
- **SupportDescription**:
  > Community support via GitHub issues
  > (https://github.com/abyo-software/s4/issues). Marketplace
  > subscribers: email abyo.software@gmail.com - best-effort response
  > within 2 business days. Documentation: README + ops runbook +
  > threat model in the repository.
- **AdditionalResources**:
  - `{"Text": "Documentation (README)", "Url": "https://github.com/abyo-software/s4#readme"}`
  - `{"Text": "Operations runbook", "Url": "https://github.com/abyo-software/s4/blob/main/docs/ops/runbook.md"}`
- **LogoUrl**: `assets/marketplace/s4-logo-square.png` (640x640, 1:1,
  white background, abyo brand mark + gradient wordmark). Upload via AMMP
  at publish (gives an awsmp-logos S3 URL). 2:1 wide + transparent
  variants alongside it.

## Delivery (Helm delivery option)

- Container image: `<marketplace-ecr>/abyo-software/s4:<version>`
  (mirror of `ghcr.io/abyo-software/s4`, built with the
  `--marketplace-product-code` flag wired via Helm)
- Helm chart: `<marketplace-ecr>/abyo-software/s4-helm:<chart-version>`
  (OCI push of `charts/s4`, chart >= 0.3.0 which carries
  `marketplace.productCode`)
- UsageInstructions (draft):
  1. Create an IAM role for the service account (IRSA) allowing
     `aws-marketplace:RegisterUsage` (policy JSON in chart README).
  2. `helm install s4 oci://<marketplace-ecr>/abyo-software/s4-helm \
       --version <chart-version> \
       --set backend.endpointUrl=https://s3.<region>.amazonaws.com \
       --set marketplace.productCode=<injected by Marketplace> \
       --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=<role-arn>`
  3. Point S3 clients at the S4 service endpoint.
- `MarketplaceServiceAccountName`: `s4`
- OverrideParameters: `marketplace.productCode` ← `$AWSMP_PRODUCT_CODE`
  equivalent (exact variable per AddDeliveryOptions schema at submit
  time).

## Pricing

- Model: **hourly, per pod** (RegisterUsage integration shipped in
  commit ae93271; fail-closed on entitlement failure).
- Dimension: per-pod/hour. Price point: operator decision at submit
  time (comparable infra tools list at roughly $0.05-0.30/pod/hour).
- Paid listing prerequisites on the seller account: completed tax
  interview + banking deposit information (Management Portal).

## Submission checklist (Catalog API, us-east-1)

1. [BLOCKED→user] Seller registration + public profile in AMMP.
2. `CreateProduct` (ContainerProduct@1.0) — draft.
3. `UpdateInformation` with the copy above.
4. `AddRepositories` — `s4` (image) + `s4-helm` (chart).
5. `docker pull ghcr.io/.../s4:<v>` → tag → push to marketplace ECR;
   `helm package` → OCI push (must pass Marketplace CVE scan).
6. `AddDeliveryOptions` (Helm delivery, usage instructions above).
7. Pricing/offer terms (hourly dimension) — needs tax+banking complete.
8. `ReleaseProduct` → AWS review → public.

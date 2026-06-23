# AWS Marketplace listing — source of truth

Status (verified 2026-06-23 via Marketplace Catalog API, seller account
393886308285) — **both products Public / live**:

- **GPU AMI** — `prod-l5my73chs43y6` ("S4 - Squished S3: GPU S3 Compression
  Gateway (EC2 AMI)"): **Public / live, v1.4.0 latest** →
  https://aws.amazon.com/marketplace/pp/prodview-yvesl7lunql6i
- **Container** — `prod-nimrbd77e4xfs` ("S4 - Squished S3: Transparent S3
  Compression Gateway"): **Public / live, v1.4.0 latest** →
  https://aws.amazon.com/marketplace/pp/prodview-kwpxxoeciis7e
- **CPU AMI** — `prod-4opohg7jaqo24` ("S4 - Squished S3: CPU S3 Compression
  Gateway (EC2 AMI)"): created 2026-06-23, **Limited / live for targeted
  buyers**, `$0.05/instance-hour` on t3 / t3a / m6i / m7i / c6i / c7i
  families (44 instance types, x86_64 only). Public-availability change
  set `95wlqe3za88a3c3gc8zzt4dlf` submitted same day and awaiting AWS
  review (~1-2 weeks). The GPU AMI listing remains the right choice for
  integer/columnar workloads at high throughput; the CPU AMI listing
  exists so the AMI flavor matches the gateway's CPU sales pitch for
  buyers who don't want GPU drivers or who run on general-purpose /
  compute families.
- **Cleanup**: a stale duplicate container draft `prod-xhip4gvojbuuy`
  (Visibility: Draft) is harmless — Catalog API rejects `DeleteResources`
  for `ContainerProduct@1.0`, so it can only be purged by AWS
  Marketplace Support.

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
     `aws-marketplace:MeterUsage` (the live product uses the custom-
     dimension MeterUsage route — see `crates/s4-server/src/marketplace.rs`
     and `docs/marketplace/metering.md`; chart README has the policy
     JSON).
  2. `helm install s4 oci://<marketplace-ecr>/abyo-software/s4-helm \
       --version <chart-version> \
       --set backend.endpointUrl=https://s3.<region>.amazonaws.com \
       --set marketplace.productCode=<injected by Marketplace> \
       --set marketplace.usageDimension=PID1 \
       --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=<role-arn>`
  3. Point S3 clients at the S4 service endpoint.
- `MarketplaceServiceAccountName`: `s4`
- OverrideParameters: `marketplace.productCode` ← `$AWSMP_PRODUCT_CODE`
  equivalent (exact variable per AddDeliveryOptions schema at submit
  time).

## Pricing

- Model: **hourly, per pod** (custom-dimension MeterUsage route since
  chart 0.3.4 / s4 v1.2.2; one MeterUsage record per pod-hour on dimension
  `PID1`; fail-closed on entitlement DryRun at boot).
- Dimension: per-pod/hour. Price point: operator decision at submit
  time (comparable infra tools list at roughly $0.05-0.30/pod/hour).
- Paid listing prerequisites on the seller account: completed tax
  interview + banking deposit information (Management Portal).

## Submission checklist (Catalog API, us-east-1)

Initial submission (done — `prod-nimrbd77e4xfs` / `prod-l5my73chs43y6`
are both Public/live as of 2026-06-23):

1. [done] Seller registration + public profile in AMMP.
2. [done] `CreateProduct` (ContainerProduct@1.0 / AmiProduct@1.0).
3. [done] `UpdateInformation` with the copy above.
4. [done] `AddRepositories` — `s4` (image) + `s4-helm` (chart) [Container].
5. [done] Image + chart push to Marketplace ECR (must pass CVE scan).
6. [done] `AddDeliveryOptions` (Helm delivery / Ami delivery).
7. [done] Pricing/offer terms (hourly dimension, tax+banking complete).
8. [done] `ReleaseProduct` + `UpdateVisibility` → Public.

Per-release update (each new s4 vX.Y.Z):

1. Marketplace ECR push of `s4:<X.Y.Z>-1` (clean multi-arch, attestation
   excluded via `imagetools create` on the per-platform manifest digests
   from `docker manifest inspect ghcr.io/abyo-software/s4:<X.Y.Z>`).
2. Build marketplace chart variant (rename to `s4-helm`, bake
   `image.repository` / `image.tag` to marketplace ECR, set
   `backend.endpointUrl` default so `helm template` renders cleanly,
   `serviceAccount.create=false`, `marketplace.productCode` +
   `marketplace.usageDimension=PID1`) and `helm push` to
   `oci://<marketplace-ecr>/abyo-software` as `s4-helm:<chart-version>`.
3. GPU AMI: launch a CPU `t3.large` from the latest DL Base GPU AMI
   (`/aws/service/deeplearning/ami/x86_64/base-oss-nvidia-driver-gpu-amazon-linux-2023/latest/ami-id`),
   `docker pull ghcr.io/abyo-software/s4:<X.Y.Z>-gpu`, install the
   systemd unit + `/etc/s4/s4.env` (uses `S4_BACKEND_ENDPOINT`,
   `AWS_REGION`, `S4_HOST=0.0.0.0`, `S4_PORT=8014`,
   `S4_CODEC=nvcomp-zstd`, `S4_DISPATCHER=sampling`,
   `S4_LOG_FORMAT=json`), install the operator README at
   `/home/ec2-user/README-S4.txt`, harden (drop authorized_keys,
   regenerate SSH host keys on next boot, clear logs / bash history /
   cloud-init state), stop the instance, and `create-image` with
   `--no-reboot`. **EC2 keypair fingerprint pre-flight is mandatory.**
3b. CPU AMI (same image, no `-gpu` suffix): launch a `t3.medium` from
    the latest stock Amazon Linux 2023 AMI
    (`/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64`),
    `dnf install -y docker`, then `docker pull
    ghcr.io/abyo-software/s4:<X.Y.Z>`. Same systemd unit / env file as
    the GPU AMI but with `S4_CODEC=cpu-zstd` and without `--gpus all`
    on the `docker run`. Same hardening / snapshot path. 20 GB root
    volume is enough (CPU image is ~150 MB vs ~2.5 GB for GPU).
4. `AddDeliveryOptions` for the Container listing (Helm delivery, image
   + chart URIs from steps 1-2), the GPU AMI listing (AmiDeliveryOption
   pointing at the new GPU AMI id), and the CPU AMI listing
   (AmiDeliveryOption pointing at the new CPU AMI id). Each kicks off a
   CVE / AMI scan (~40 min Container, ~60 min AMI). All text fields
   must be ASCII-clean (verify with
   `LC_ALL=C grep -P '[^\x09\x0a\x0d\x20-\x7e]'`).
5. Once the new version is `SUCCEEDED`, restrict the prior public
   version with `RestrictDeliveryOptions` so only the latest is
   subscribable.
6. Terminate the AMI-build EC2 instance + delete the ephemeral SG
   immediately. Deregister/snapshot-delete any intermediate AMIs.

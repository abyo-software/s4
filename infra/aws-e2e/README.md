# AWS E2E test infrastructure

This Terraform module provisions the AWS resources needed for the
[`aws-e2e.yml`](../../.github/workflows/aws-e2e.yml) GitHub Actions
workflow:

| Resource | Purpose |
|---|---|
| Dedicated S3 bucket | Test artifacts (auto-deleted after 1 day) |
| GitHub OIDC provider | Federated identity, no long-lived access keys |
| IAM role scoped to the test bucket | Assumable only by `abyo-software/s4` workflow runs |

The bucket has a 1-day expiration lifecycle rule and incomplete-multipart-upload
cleanup, so cost is bounded to <$1/month under nightly cadence.

## Quick start (recommended)

One command does everything — Terraform apply + push the three outputs
into the repo's GitHub Actions Variables:

```bash
./scripts/bootstrap-aws-e2e.sh
```

The script is idempotent: re-run it whenever Terraform state or AWS
credentials change and it will refresh the GitHub variables to match.

Prerequisites the script verifies up-front (and errors out cleanly if
any are missing):
- AWS credentials configured (`aws sts get-caller-identity` must succeed)
- GitHub CLI authenticated with `repo` scope (`gh auth status` must succeed)
- `terraform` >= 1.5, `aws`, `gh`, `jq` on `PATH`

Optional env-var overrides forwarded to Terraform:

| Var | Default | Purpose |
|---|---|---|
| `AWS_REGION` | `us-east-1` | AWS region |
| `BUCKET_PREFIX` | `s4-aws-e2e` | S3 bucket name prefix |
| `GITHUB_REPO` | `abyo-software/s4` | OIDC `sub:` filter (which repo can assume the role) |
| `GH_REPO` | `abyo-software/s4` | Repo to push the Variables to (defaults to `GITHUB_REPO`) |

When the script finishes it prints a summary block with the three
variable names + values that were set.

## Manual setup (fallback)

If the bootstrap script can't run for some reason (e.g. you only have
Terraform and want to set the GitHub variables by hand), the equivalent
manual steps:

You need:
- AWS account + credentials with permissions to create S3 buckets, IAM roles,
  and (if not already present) the GitHub OIDC provider
- Terraform >= 1.5

```bash
cd infra/aws-e2e
terraform init
terraform apply

# Note the outputs:
# bucket_name = "s4-aws-e2e-xxxxxxxx"
# role_arn    = "arn:aws:iam::123456789012:role/s4-aws-e2e-github-actions"
# region      = "us-east-1"
```

Then go to **GitHub → Settings → Secrets and variables → Actions →
Variables tab** (NOT Secrets — these are not sensitive) and add three
repository variables:

| Variable name | Value |
|---|---|
| `AWS_E2E_BUCKET` | the `bucket_name` output |
| `AWS_E2E_ROLE_ARN` | the `role_arn` output |
| `AWS_E2E_REGION` | the `region` output |

The workflow checks for these variables before running and fails with a
clear message if any are missing.

### AWS KMS E2E (opt-in)

Set additionally for the KMS E2E workflow
([`.github/workflows/aws-kms-e2e.yml`](../../.github/workflows/aws-kms-e2e.yml),
v0.8.1 #60). When any of these are unset the workflow no-ops with a
`::notice::` (matching the `aws-e2e.yml` gating pattern):

| Variable name | Value |
|---|---|
| `AWS_KMS_REGION` | e.g. `us-east-1` |
| `AWS_KMS_ROLE_ARN` | IAM role with `kms:GenerateDataKey` + `kms:Decrypt` on the test key |
| `AWS_KMS_TEST_KEY_ID` | alias or KMS key id (alias `alias/s4-test` recommended) |

The role's trust policy should mirror the S3 E2E role (GitHub OIDC,
`sub: repo:abyo-software/s4:*`); the inline policy needs only
`kms:GenerateDataKey` and `kms:Decrypt` scoped to the single test key
ARN. Provisioning is currently manual — a Terraform module to mirror
`infra/aws-e2e/main.tf` for KMS is tracked separately.

## Running the workflow

The workflow runs:
- **Nightly** at `04:00 UTC` (cron schedule)
- **On-demand** via `workflow_dispatch` (Actions tab → "AWS E2E" →
  "Run workflow")
- **On PRs** that carry the `aws-e2e` label (gated to avoid running on
  every PR — only when reviewer adds the label)

## Why this design

- **OIDC, not long-lived keys**: the IAM role's trust policy uses
  GitHub's OIDC issuer + `sub: repo:abyo-software/s4:*`, so only this
  repo's workflows can assume the role. No access keys committed
  anywhere.
- **Least-privilege**: the role's inline policy only grants S3 ops
  on the dedicated test bucket — not on any other bucket in the AWS
  account.
- **Auto-cleanup**: 1-day expiration lifecycle keeps storage cost
  bounded even if a test run is interrupted mid-cleanup.

## Tearing down

```bash
cd infra/aws-e2e
terraform destroy
```

`force_destroy = true` on the bucket lets `destroy` succeed even if
objects remain (typically they're already gone via the lifecycle rule).

## Cost estimate

Under nightly cadence with ~100 MB of test artifacts per run:
- Storage (auto-expired in 24h): negligible (<$0.01/month)
- Requests (PUT/GET/DELETE): a few thousand per run × 30 nights = <$0.10/month
- Data transfer: 0 (workflow runs in AWS-region runners → S3 in same region)

**Total: under $1/month** in steady state.

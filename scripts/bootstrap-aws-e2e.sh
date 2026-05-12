#!/usr/bin/env bash
# One-command bootstrap for the S4 AWS E2E test infrastructure.
#
# Provisions (or updates) the Terraform-managed S3 bucket + IAM/OIDC role
# under `infra/aws-e2e/` and pushes the three outputs into the
# `abyo-software/s4` GitHub Actions Variables so the `aws-e2e.yml`
# workflow can run unattended.
#
# Idempotent: re-running against an already-applied stack just refreshes
# the outputs and overwrites the GitHub variables with whatever
# Terraform currently reports.
#
# Usage:
#   ./scripts/bootstrap-aws-e2e.sh
#
# Optional env vars (all forwarded to terraform via -var):
#   AWS_REGION       — AWS region for the bucket / role (default: us-east-1)
#   BUCKET_PREFIX    — name prefix for the S3 bucket  (default: s4-aws-e2e)
#   GITHUB_REPO      — `<owner>/<repo>` for the OIDC trust policy
#                      (default: abyo-software/s4)
#   GH_REPO          — repo to push the Variables to (default: same as GITHUB_REPO)
#
# Prerequisites the script verifies up-front:
#   - `terraform` >= 1.5 on PATH
#   - `aws` CLI on PATH with valid credentials
#     (`aws sts get-caller-identity` must succeed)
#   - `gh` CLI on PATH and authenticated
#     (`gh auth status` must succeed; needs `repo` scope to set Variables)

set -euo pipefail

# --- 0. resolve repo root regardless of cwd ---
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
TF_DIR="$REPO_ROOT/infra/aws-e2e"

GH_REPO_DEFAULT="abyo-software/s4"
GH_REPO="${GH_REPO:-${GITHUB_REPO:-$GH_REPO_DEFAULT}}"

err() { printf 'ERROR: %s\n' "$*" >&2; }
log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

# --- 1. preflight: required binaries ---
for bin in terraform aws gh jq; do
    if ! command -v "$bin" >/dev/null 2>&1; then
        err "required binary not found on PATH: $bin"
        case "$bin" in
            terraform) err "  install: https://developer.hashicorp.com/terraform/downloads" ;;
            aws)       err "  install: https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html" ;;
            gh)        err "  install: https://cli.github.com/" ;;
            jq)        err "  install: 'apt install jq' / 'brew install jq'" ;;
        esac
        exit 1
    fi
done

# --- 2. preflight: AWS credentials ---
log "checking AWS credentials..."
if ! aws sts get-caller-identity >/dev/null 2>&1; then
    err "AWS credentials are not configured (or expired)."
    err "  run 'aws sts get-caller-identity' to diagnose; configure via"
    err "  'aws configure', 'aws sso login', or appropriate AWS_* env vars."
    exit 1
fi
AWS_ACCT="$(aws sts get-caller-identity --query Account --output text)"
AWS_USER_ARN="$(aws sts get-caller-identity --query Arn --output text)"
log "  AWS account: $AWS_ACCT  (caller: $AWS_USER_ARN)"

# --- 3. preflight: gh auth ---
log "checking gh auth..."
if ! gh auth status >/dev/null 2>&1; then
    err "GitHub CLI is not authenticated."
    err "  run 'gh auth login' first (needs repo scope to set Actions Variables)."
    exit 1
fi
log "  gh authenticated; target repo = $GH_REPO"

# --- 4. terraform init + apply (idempotent) ---
TF_VAR_ARGS=()
if [ -n "${AWS_REGION:-}" ];    then TF_VAR_ARGS+=(-var "aws_region=$AWS_REGION"); fi
if [ -n "${BUCKET_PREFIX:-}" ]; then TF_VAR_ARGS+=(-var "bucket_prefix=$BUCKET_PREFIX"); fi
if [ -n "${GITHUB_REPO:-}" ];   then TF_VAR_ARGS+=(-var "github_repo=$GITHUB_REPO"); fi

log "terraform init  ($TF_DIR)"
terraform -chdir="$TF_DIR" init -input=false

log "terraform apply -auto-approve"
if [ "${#TF_VAR_ARGS[@]}" -gt 0 ]; then
    terraform -chdir="$TF_DIR" apply -auto-approve -input=false "${TF_VAR_ARGS[@]}"
else
    terraform -chdir="$TF_DIR" apply -auto-approve -input=false
fi

# --- 5. capture outputs ---
log "reading terraform outputs..."
TF_JSON="$(terraform -chdir="$TF_DIR" output -json)"

BUCKET_NAME="$(printf '%s' "$TF_JSON" | jq -r '.bucket_name.value // empty')"
ROLE_ARN="$(printf '%s' "$TF_JSON"   | jq -r '.role_arn.value    // empty')"
REGION="$(printf '%s' "$TF_JSON"     | jq -r '.region.value      // empty')"

if [ -z "$BUCKET_NAME" ] || [ -z "$ROLE_ARN" ] || [ -z "$REGION" ]; then
    err "one or more terraform outputs are empty:"
    err "  bucket_name='$BUCKET_NAME'  role_arn='$ROLE_ARN'  region='$REGION'"
    err "  (check 'terraform -chdir=$TF_DIR output' manually)"
    exit 1
fi

# --- 6. push GitHub Actions Variables (overwrites if already set) ---
set_var() {
    local name="$1"
    local value="$2"
    log "gh variable set $name  ->  $GH_REPO"
    # `gh variable set` is upsert: creates if missing, updates if present.
    gh variable set "$name" --body "$value" --repo "$GH_REPO"
}

set_var AWS_E2E_BUCKET   "$BUCKET_NAME"
set_var AWS_E2E_ROLE_ARN "$ROLE_ARN"
set_var AWS_E2E_REGION   "$REGION"

# --- 7. summary ---
cat <<EOF

==========================================================================
S4 AWS-E2E bootstrap complete.

GitHub Actions Variables set on $GH_REPO:

  AWS_E2E_BUCKET   = $BUCKET_NAME
  AWS_E2E_ROLE_ARN = $ROLE_ARN
  AWS_E2E_REGION   = $REGION

The aws-e2e.yml workflow is now ready to run:
  gh workflow run aws-e2e.yml --repo $GH_REPO

Tear down later (when no longer needed) with:
  terraform -chdir=$TF_DIR destroy
==========================================================================
EOF

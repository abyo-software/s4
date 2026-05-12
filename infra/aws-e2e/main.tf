# Terraform for the S4 AWS E2E test infrastructure.
#
# Provisions:
#   - An S3 bucket dedicated to the nightly aws-e2e GitHub Actions workflow
#   - A 1-day expiration lifecycle rule (auto-cleanup, cost cap)
#   - The GitHub OIDC provider (idempotent — safe if already exists in the account)
#   - An IAM role assumable only by GitHub Actions runs from
#     `abyo-software/s4` with permission scoped to that single bucket
#
# Apply once per AWS account:
#
#     cd infra/aws-e2e
#     terraform init
#     terraform apply
#
# Outputs `bucket_name` and `role_arn` — set them as GitHub repository
# variables (settings -> secrets and variables -> actions -> Variables tab):
#
#     AWS_E2E_BUCKET    = <bucket_name from terraform output>
#     AWS_E2E_ROLE_ARN  = <role_arn from terraform output>
#     AWS_E2E_REGION    = us-east-1   (or your chosen region)
#
# These are *variables*, not secrets — the role ARN and bucket name are not
# sensitive (the OIDC trust policy is what enforces access control).

terraform {
  required_version = ">= 1.5"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.50"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

resource "random_id" "suffix" {
  byte_length = 4
}

# ---------- S3 bucket ----------

resource "aws_s3_bucket" "e2e" {
  bucket = "${var.bucket_prefix}-${random_id.suffix.hex}"

  # Force-destroy lets `terraform destroy` succeed even if objects remain
  # (CI test artifacts older than 1 day will already be gone via lifecycle).
  force_destroy = true

  tags = {
    Project = "s4"
    Purpose = "github-actions-aws-e2e"
    Managed = "terraform"
  }
}

resource "aws_s3_bucket_public_access_block" "e2e" {
  bucket                  = aws_s3_bucket.e2e.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_lifecycle_configuration" "e2e" {
  bucket = aws_s3_bucket.e2e.id

  rule {
    id     = "delete-test-artifacts-after-1-day"
    status = "Enabled"

    filter {}

    expiration {
      days = 1
    }

    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }
}

# ---------- GitHub OIDC provider ----------
# Idempotent across AWS accounts: re-using an existing provider is OK because
# the thumbprint and URL are fixed. If you already have one configured in
# this account, comment this resource out and reference its ARN below.

data "tls_certificate" "github_oidc" {
  url = "https://token.actions.githubusercontent.com"
}

resource "aws_iam_openid_connect_provider" "github" {
  url             = "https://token.actions.githubusercontent.com"
  client_id_list  = ["sts.amazonaws.com"]
  thumbprint_list = [data.tls_certificate.github_oidc.certificates[0].sha1_fingerprint]
}

# ---------- IAM role for the GitHub Actions runs ----------

data "aws_caller_identity" "current" {}

resource "aws_iam_role" "github_actions" {
  name = "${var.bucket_prefix}-github-actions"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Principal = {
        Federated = aws_iam_openid_connect_provider.github.arn
      }
      Action = "sts:AssumeRoleWithWebIdentity"
      Condition = {
        StringEquals = {
          "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com"
        }
        # Limit assumption to the specific repo. Branches/PRs/tags from this
        # repo can all assume the role; the inline policy below limits what
        # they can DO once assumed.
        StringLike = {
          "token.actions.githubusercontent.com:sub" = "repo:${var.github_repo}:*"
        }
      }
    }]
  })

  tags = {
    Project = "s4"
    Managed = "terraform"
  }
}

resource "aws_iam_role_policy" "github_actions_e2e" {
  name = "s4-aws-e2e"
  role = aws_iam_role.github_actions.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        # Bucket-level operations on the test bucket only.
        Effect = "Allow"
        Action = [
          "s3:ListBucket",
          "s3:GetBucketLocation",
          "s3:ListBucketMultipartUploads",
        ]
        Resource = aws_s3_bucket.e2e.arn
      },
      {
        # Object-level operations on the test bucket only.
        Effect = "Allow"
        Action = [
          "s3:GetObject",
          "s3:PutObject",
          "s3:DeleteObject",
          "s3:AbortMultipartUpload",
          "s3:ListMultipartUploadParts",
          "s3:GetObjectAttributes",
        ]
        Resource = "${aws_s3_bucket.e2e.arn}/*"
      }
    ]
  })
}

# ---------- Outputs ----------

output "bucket_name" {
  value       = aws_s3_bucket.e2e.id
  description = "Set this as the AWS_E2E_BUCKET GitHub Actions variable."
}

output "role_arn" {
  value       = aws_iam_role.github_actions.arn
  description = "Set this as the AWS_E2E_ROLE_ARN GitHub Actions variable."
}

output "region" {
  value       = var.aws_region
  description = "Set this as the AWS_E2E_REGION GitHub Actions variable."
}

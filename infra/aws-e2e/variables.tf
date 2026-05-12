variable "aws_region" {
  description = "AWS region for the test bucket. Pick a region close to your GitHub Actions runners (us-east-1 works well for Linux runners)."
  type        = string
  default     = "us-east-1"
}

variable "bucket_prefix" {
  description = "Prefix for the test bucket name. A random 4-byte hex suffix is appended for global uniqueness."
  type        = string
  default     = "s4-aws-e2e"
}

variable "github_repo" {
  description = "GitHub repo (org/name) allowed to assume the IAM role via OIDC."
  type        = string
  default     = "abyo-software/s4"
}

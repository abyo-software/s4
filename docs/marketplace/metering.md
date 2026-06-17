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

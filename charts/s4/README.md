# s4 — Helm chart

Drop-in S3-compatible storage gateway with GPU-accelerated transparent
compression. This chart deploys [S4](https://github.com/abyo-software/s4) as a
stateless `Deployment` + `Service` on any Kubernetes 1.24+ cluster.

> **Status: MVP (chart 0.1.0).** The chart itself is intended to follow Helm
> best practices (labels, helpers, NOTES, ingress hook, GPU node selector,
> readOnlyRootFilesystem). Official images are published to
> [`ghcr.io/abyo-software/s4`](https://github.com/abyo-software/s4/pkgs/container/s4)
> on every `v*.*.*` git tag — see [Image](#image) below.

## TL;DR

```bash
helm install s4 ./charts/s4 \
  --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com \
  --set backend.region=us-east-1
```

That brings up 2 replicas of the gateway listening on port 8014 inside the
cluster (ClusterIP). To reach it from your laptop:

```bash
kubectl port-forward svc/s4 8014:8014
aws --endpoint-url http://localhost:8014 s3 mb s3://demo
aws --endpoint-url http://localhost:8014 s3 cp some.log s3://demo/some.log
```

## Image

Official images are published to
[`ghcr.io/abyo-software/s4`](https://github.com/abyo-software/s4/pkgs/container/s4)
on every `v*.*.*` release tag by
[`.github/workflows/docker.yml`](../../.github/workflows/docker.yml). The
package is public — `helm install` works with no `imagePullSecrets`.

Two flavors share the same repository under different tag suffixes:

| Flavor | Tag                                          | Platforms                    |
|--------|----------------------------------------------|------------------------------|
| CPU    | `ghcr.io/abyo-software/s4:<version>`         | `linux/amd64`, `linux/arm64` |
| GPU    | `ghcr.io/abyo-software/s4:<version>-gpu`     | `linux/amd64` only           |

`<version>` is the bare semver (e.g. `0.9.0`); `v0.9.0` and the `latest`
moving tag are also published.

For local testing without ghcr.io, build and side-load:

```bash
# from the repo root
docker build -t ghcr.io/abyo-software/s4:0.9.0 .

# kind:
kind load docker-image ghcr.io/abyo-software/s4:0.9.0

# minikube:
minikube image load ghcr.io/abyo-software/s4:0.9.0

# k3d:
k3d image import ghcr.io/abyo-software/s4:0.9.0
```

Or push to your own registry and override:

```bash
helm install s4 ./charts/s4 \
  --set image.repository=ghcr.io/myorg/s4 \
  --set image.tag=0.9.0 \
  --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com
```

## GPU (nvCOMP) deployments

The GPU image (built from `Dockerfile.gpu`) ships as the `-gpu` tag on the
same `ghcr.io/abyo-software/s4` repository. Enable GPU scheduling and pick
an nvCOMP codec:

```bash
helm install s4 ./charts/s4 \
  --set image.tag=0.9.0-gpu \
  --set codec=nvcomp-zstd \
  --set gpu.enabled=true \
  --set gpu.count=1 \
  --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com
```

The chart will request `nvidia.com/gpu` on the container, set the
`nvidia.com/gpu.present=true` node selector, and (optionally) set
`runtimeClassName: nvidia` if you pass `--set gpu.runtimeClassName=nvidia`.

## TLS

Three ways to terminate HTTPS at the listener (see also the upstream
README's "HTTPS" section):

1. **External (cert-manager) Secret** — recommended in production:
   ```bash
   --set tls.enabled=true \
   --set tls.existingSecret=s4-tls
   ```
2. **Inline PEM** (good for first-run / dev):
   ```bash
   --set tls.enabled=true \
   --set-file tls.cert=./server.crt \
   --set-file tls.key=./server.key
   ```
3. **Plain HTTP** (default) — let an Ingress / service mesh terminate.

## AWS Marketplace (paid container)

Subscribers to the paid AWS Marketplace listing run the **same binary** as
the free ghcr.io image — the only difference is that the Marketplace
deployment sets `marketplace.productCode`, which makes each pod prove its
entitlement and meter usage at boot (charges appear on your regular AWS
invoice). A non-entitled pod (not subscribed, wrong code, unsupported
platform) exits non-zero before it ever serves a request.

There are **two metering routes**, chosen by whether you also set
`marketplace.usageDimension`. Pick the one that matches how your product's
pricing dimension is configured (`describe-entity` →
`Dimensions[].Types`):

| Product dimension type | Set | Metering API |
|---|---|---|
| none / `Metered` (per-pod hour, AWS auto-meters) | `productCode` only | `RegisterUsage` once at boot |
| `ExternallyMetered` (custom dimension you price per unit) | `productCode` **and** `usageDimension` | `MeterUsage` DryRun at boot + one record per pod per hour |

Using the wrong route is silently unbillable: `RegisterUsage` never emits a
record against an `ExternallyMetered` dimension, and AWS will reject the
listing with *"all metered dimensions must be registered at the metering
service."*

Requirements (both routes):

1. **Run on Amazon EKS / ECS / Fargate.** This is a container listing, so
   deploy the metered build there. The `RegisterUsage` hourly route in
   particular refuses any other platform (plain `docker run`, bare EC2) with
   `PlatformNotSupportedException`, and the pod logs a clear error and exits.
2. **IRSA with the metering permission.** Create an IAM role for the
   service account. Grant `aws-marketplace:RegisterUsage` for the hourly
   route, `aws-marketplace:MeterUsage` for the custom-dimension route, or
   both if unsure:

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       {
         "Effect": "Allow",
         "Action": [
           "aws-marketplace:RegisterUsage",
           "aws-marketplace:MeterUsage"
         ],
         "Resource": "*"
       }
     ]
   }
   ```

3. Install with the product code from your Marketplace fulfillment page.

   Hourly (RegisterUsage) route:

   ```bash
   helm install s4 ./charts/s4 \
     --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com \
     --set backend.region=us-east-1 \
     --set marketplace.productCode=<YOUR_PRODUCT_CODE> \
     --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::<ACCOUNT_ID>:role/s4-marketplace
   ```

   Custom-dimension (MeterUsage) route — add the dimension API name:

   ```bash
   helm install s4 ./charts/s4 \
     --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com \
     --set backend.region=us-east-1 \
     --set marketplace.productCode=<YOUR_PRODUCT_CODE> \
     --set marketplace.usageDimension=Hours \
     --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::<ACCOUNT_ID>:role/s4-marketplace
   ```

With `marketplace.productCode` left at its default `""`, no metering flag is
rendered at all and the deployment is the unmetered free OSS gateway.

## Bucket policy

Pass an AWS-style JSON policy:

```bash
--set-file policy.json=./bucket-policy.json
```

The chart renders a ConfigMap, mounts it at `/etc/s4/policy/policy.json`,
and starts the binary with `--policy /etc/s4/policy/policy.json`. Pod-level
checksum annotations roll the deployment when the policy text changes.

## Values

| Key | Default | Description |
|---|---|---|
| `replicas` | `2` | Number of gateway replicas (S4 is stateless). |
| `image.repository` | `ghcr.io/abyo-software/s4` | Image repo. Published from `.github/workflows/docker.yml` on each `v*.*.*` tag (CPU multi-arch + `-gpu` amd64). |
| `image.tag` | `""` (defaults to `.Chart.AppVersion`) | Image tag. |
| `image.pullPolicy` | `IfNotPresent` | Standard Kubernetes image pull policy. |
| `backend.endpointUrl` | `""` (**REQUIRED**) | Real S3 endpoint the gateway forwards to. |
| `backend.region` | `""` | AWS region (set as `AWS_REGION` env). |
| `marketplace.productCode` | `""` | AWS Marketplace paid-container product code. Empty = free OSS deployment (no metering code runs). See [AWS Marketplace](#aws-marketplace-paid-container). |
| `marketplace.usageDimension` | `""` | AWS Marketplace custom (`ExternallyMetered`) dimension API name. Empty = RegisterUsage hourly route. Set (with `productCode`) to use the MeterUsage route. See [AWS Marketplace](#aws-marketplace-paid-container). |
| `codec` | `cpu-zstd` | Default codec: `cpu-zstd` / `nvcomp-zstd` / `nvcomp-bitcomp` / `nvcomp-gdeflate` / `identity`. |
| `dispatcher` | `sampling` | `always` (pin to `codec`) or `sampling` (auto-select per object). |
| `gpu.enabled` | `false` | Request `nvidia.com/gpu` and apply GPU node selector. Requires the GPU build of the image. |
| `gpu.count` | `1` | GPUs per pod. |
| `tls.enabled` | `false` | Terminate HTTPS on the listener. |
| `tls.existingSecret` | `""` | Use an existing `kubernetes.io/tls` secret (cert-manager friendly). |
| `policy.json` | `""` | Inline AWS-style bucket-policy JSON. |
| `service.type` | `ClusterIP` | Service type. |
| `service.port` | `8014` | Service / container port. |
| `resources.requests` | `cpu=500m, memory=512Mi` | Container resource requests. |
| `resources.limits` | `cpu=2, memory=2Gi` | Container resource limits. |
| `ingress.enabled` | `false` | Render an `Ingress` object. |
| `logFormat` | `json` | `pretty` or `json`. |
| `otlpEndpoint` | `""` | OTLP gRPC endpoint for tracing. |

See [`values.yaml`](values.yaml) for the complete schema (probes, security
context, extraEnv, extraVolumes, etc).

## Tested on

The chart is written to follow Helm best practices and renders cleanly under
`helm template` / `helm lint`. It is intended to run on:

- minikube (1.30+)
- k3d / k3s
- kind
- managed clusters (EKS / GKE / AKS) — for production EKS, prefer IRSA
  rather than embedded credentials; pass an annotation via
  `serviceAccount.annotations`.

## Uninstalling

```bash
helm uninstall s4
```

The chart owns no PVCs (S4 is stateless, all state lives in the backing S3
bucket), so this is a clean removal.

## Upstream

- Source: <https://github.com/abyo-software/s4>
- Issues: <https://github.com/abyo-software/s4/issues>
- License: Apache-2.0

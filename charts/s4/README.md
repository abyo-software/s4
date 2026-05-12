# s4 — Helm chart

Drop-in S3-compatible storage gateway with GPU-accelerated transparent
compression. This chart deploys [S4](https://github.com/abyo-software/s4) as a
stateless `Deployment` + `Service` on any Kubernetes 1.24+ cluster.

> **Status: MVP (chart 0.1.0).** The chart itself is intended to follow Helm
> best practices (labels, helpers, NOTES, ingress hook, GPU node selector,
> readOnlyRootFilesystem). The official `docker.io/abyosoftware/s4:0.3.0`
> image is **not yet published** — see [Image](#image) below for how to
> bootstrap with a locally built image.

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

As of chart 0.1.0 the official Docker Hub image is **not yet published**.
For local testing, build and side-load:

```bash
# from the repo root
docker build -t abyosoftware/s4:0.3.0 .

# kind:
kind load docker-image abyosoftware/s4:0.3.0

# minikube:
minikube image load abyosoftware/s4:0.3.0

# k3d:
k3d image import abyosoftware/s4:0.3.0
```

Or push to your own registry and override:

```bash
helm install s4 ./charts/s4 \
  --set image.repository=ghcr.io/myorg/s4 \
  --set image.tag=0.3.0 \
  --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com
```

## GPU (nvCOMP) deployments

Build the GPU image (see `Dockerfile.gpu` in the repo root), then enable GPU
scheduling and pick an nvCOMP codec:

```bash
helm install s4 ./charts/s4 \
  --set image.repository=ghcr.io/myorg/s4-gpu \
  --set image.tag=0.3.0 \
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
| `image.repository` | `docker.io/abyosoftware/s4` | Image repo. **Not yet published — see Image above.** |
| `image.tag` | `""` (defaults to `.Chart.AppVersion`) | Image tag. |
| `image.pullPolicy` | `IfNotPresent` | Standard Kubernetes image pull policy. |
| `backend.endpointUrl` | `""` (**REQUIRED**) | Real S3 endpoint the gateway forwards to. |
| `backend.region` | `""` | AWS region (set as `AWS_REGION` env). |
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

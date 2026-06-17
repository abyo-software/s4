# Deploying S4 on Kubernetes (Helm)

### Kubernetes (Helm)

Official container images are published to GitHub Container Registry on every
`v*.*.*` release tag — `ghcr.io/abyo-software/s4:<version>` (CPU, multi-arch
amd64 + arm64) and `ghcr.io/abyo-software/s4:<version>-gpu` (nvCOMP GPU build,
amd64). The package is public; no `imagePullSecrets` needed.

```bash
helm install s4 ./charts/s4 \
  --set image.tag=1.0.0 \
  --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com \
  --set backend.region=us-east-1
kubectl port-forward svc/s4 8014:8014
```

(Use `image.tag=1` for the floating major-line tag that auto-rolls forward
across v1.x minors; the per-version, per-minor, and floating-major tag
rules are defined in [§Stability](stability.md).)

For the GPU image, override `image.tag` with the `-gpu` suffix and turn on
GPU scheduling:

```bash
helm install s4 ./charts/s4 \
  --set image.tag=1.0.0-gpu \
  --set codec=nvcomp-zstd \
  --set gpu.enabled=true \
  --set backend.endpointUrl=https://s3.us-east-1.amazonaws.com
```

The chart in [`charts/s4/`](../charts/s4/) ships a stateless Deployment + Service
(ClusterIP, port 8014), optional GPU node selector (`gpu.enabled=true` for
nvCOMP), inline or cert-manager TLS, and bucket-policy ConfigMap. See
[charts/s4/README.md](../charts/s4/README.md) for the full values table and
[.github/workflows/docker.yml](../.github/workflows/docker.yml) for the image
build / publish pipeline.

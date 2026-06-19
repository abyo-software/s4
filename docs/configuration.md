# Configuration

| CLI flag | Default | Description |
|---|---|---|
| `--endpoint-url` | (required) | Backend S3 endpoint (e.g. `https://s3.us-east-1.amazonaws.com`) |
| `--host` | `127.0.0.1` | Bind host |
| `--port` | `8014` | Bind port |
| `--domain` | (none) | Virtual-hosted-style requests domain |
| `--codec` | `cpu-zstd` | Default codec: `passthrough`, `cpu-zstd`, `nvcomp-zstd`, `nvcomp-bitcomp` |
| `--zstd-level` | `3` | CPU zstd compression level (1–22) |
| `--dispatcher` | `sampling` | `always` (use `--codec`) or `sampling` (entropy + magic byte) |
| `--log-format` | `pretty` | `pretty` (terminal) or `json` (CloudWatch / fluent-bit) |
| `--otlp-endpoint` | (none) | OpenTelemetry OTLP gRPC endpoint |
| `--service-name` | `s4` | OTel resource `service.name` |
| `--tls-cert` | (none) | TLS server certificate (PEM). Together with `--tls-key`, terminates HTTPS on the listener. Hot-reload via `SIGHUP` (v0.3) |
| `--tls-key` | (none) | TLS server private key (PEM, PKCS#8 or RSA) |
| `--acme` | (none) | Comma-separated domains for ACME (Let's Encrypt) auto-cert via TLS-ALPN-01. Mutually exclusive with `--tls-cert` (v0.3) |
| `--acme-contact` | (none) | Contact email for ACME account (required when `--acme` is set) |
| `--acme-cache-dir` | `~/.s4/acme/` | Cert + account cache directory (so restarts don't trigger fresh enrollments and exhaust LE rate limits) |
| `--acme-staging` | (off) | Use the LE staging directory (no rate limits; cert is not browser-trusted). Recommended for first-run |
| `--policy` | (none) | AWS-style bucket policy JSON. When set, every PUT/GET/DELETE/List request is evaluated before backend dispatch |
| `--read-timeout-seconds` | `30` | Per-connection wall-clock cap covering header reads, body reads, **and request handling/compression** (slowloris guard). `0` disables it (not recommended). Because compression time counts toward the cap, a single PUT that needs longer than 30s — e.g. compressing a large body at a **high `--zstd-level` (19–22) on the PUT path** — will trip it; for that case prefer the latency-friendly default level and `s4 recompact` cold data instead (see the [Elasticsearch frozen-tier use case](use-cases/elasticsearch-frozen-tier.md)) |
| `--max-concurrent-connections` | `1024` | Hard cap on in-flight HTTP connections; when the cap is reached the listener waits before accepting another connection until one drains |

AWS credentials are read from the standard AWS chain (`AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY` / `AWS_PROFILE` / IAM role on EC2).

### HTTPS

S4 can terminate TLS itself — no fronting reverse proxy required:

```bash
s4 --endpoint-url https://s3.us-east-1.amazonaws.com \
   --host 0.0.0.0 --port 8443 \
   --tls-cert /etc/ssl/s4.crt --tls-key /etc/ssl/s4.key
aws --endpoint-url https://localhost:8443 s3 ls
```

Backed by `tokio-rustls` + `ring`. ALPN advertises `h2` then `http/1.1`, so
HTTP/2 is negotiated automatically with capable clients. Without these
flags, S4 serves plain HTTP (the default).

**Cert hot-reload (v0.3)**: rotate `--tls-cert` / `--tls-key` files on disk
and `kill -HUP <pid>` to swap the active cert without dropping any
in-flight connections. Re-read failures keep the previous cert in effect
so a bad deploy never causes a listener outage.

**ACME / Let's Encrypt (v0.3)**: for public deployments, fetch and renew
certs automatically with `--acme`:

```bash
s4 --endpoint-url https://s3.us-east-1.amazonaws.com \
   --host 0.0.0.0 --port 443 \
   --acme s4.example.com,api.example.com \
   --acme-contact ops@example.com \
   --acme-staging   # remove for production after first-run validation
```

Uses TLS-ALPN-01 challenge handled inline on the listening port — no
separate port-80 listener required. Background renewal at the standard
~60-day interval; `s4_acme_renewal_total{result}` Prometheus counter
+ `s4_acme_cert_expiry_seconds` gauge for monitoring.

### Bucket policy enforcement

Pass an AWS-style bucket policy JSON to `--policy` to gate requests at the
gateway:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {"Sid": "ReadOnly",  "Effect": "Allow", "Action": ["s3:GetObject", "s3:ListBucket"],
     "Resource": ["arn:aws:s3:::my-bucket", "arn:aws:s3:::my-bucket/*"]},
    {"Sid": "DenyDelete", "Effect": "Deny",  "Action": "s3:DeleteObject",
     "Resource": "arn:aws:s3:::my-bucket/*"}
  ]
}
```

Supported subset (v0.3):

- **`Effect` / `Action` / `Resource` / `Principal`** (v0.2 baseline): SigV4
  access-key match for Principal, glob matching for Resource.
- **`Condition` clauses** (v0.3 #13): `IpAddress` / `NotIpAddress` (CIDR),
  `StringEquals` / `StringNotEquals` / `StringLike` / `StringNotLike`,
  `DateGreaterThan` / `DateLessThan`, `Bool`. Supports the well-known
  AWS context keys `aws:SourceIp` (taken from the `X-Forwarded-For`
  header — set this at your reverse proxy / load balancer),
  `aws:UserAgent`, `aws:CurrentTime`, `aws:SecureTransport` (true when
  the listener is `--tls-cert` or `--acme`).

Decision order is the standard AWS one: **explicit Deny > explicit Allow >
implicit Deny**. Conditions are AND-combined within a Statement. Denials
are exposed as the `s4_policy_denials_total{action,bucket}` Prometheus
counter.

For STS / AssumeRole chains and cross-account delegation (still out of
scope), front S4 with an IAM-aware proxy and use this flag for the
in-gateway last-mile checks.

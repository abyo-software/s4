# Security overview

S4 is a TLS-terminating S3-compatible proxy. The boundaries you should
think about:

- **Authentication scope**: S4 verifies SigV4 / SigV4a on incoming
  requests using credentials operators configure (`--credentials FILE`
  or `--sigv4a-credentials DIR`). The S4 server then turns around and
  speaks to the backend bucket using **its own** AWS credentials
  (`AWS_ACCESS_KEY_ID` etc. from the standard SDK chain). Client
  identity is **not** delegated to the backend; the backend sees S4 as
  one principal regardless of which incoming client made the request.
  If you need per-client backend identity, run one S4 instance per
  client and use distinct backend credentials.
- **TLS termination**: S4 terminates TLS at its own listener
  (`--tls-cert` / `--tls-key`, or ACME via `--acme`). The connection
  to the backend uses the SDK's own TLS (rustls with the system root
  CA store). If your security model requires end-to-end TLS without
  intermediate decryption, S4 is the wrong shape — use a different
  proxy or run S4 colocated with the backend so the second TLS hop
  doesn't leave the same host.
- **Bucket policy enforcement at the S4 layer**: when `--bucket-policy
  FILE` is set, S4 evaluates AWS-style JSON Allow / Deny rules
  **before** forwarding to the backend. The backend's own bucket
  policy still applies on top. Two policies in series; both must
  permit. We do **not** parse every IAM Condition operator — see
  [`crates/s4-server/src/policy.rs`](../../crates/s4-server/src/policy.rs)
  for the supported subset.
- **Body-size limits / request smuggling**: hyper limits enforced
  (`--max-header-bytes`, default 64 KiB; `--max-concurrent-connections`,
  default 1024; `--read-timeout-seconds`, default 30s — see v0.8.5
  #84). HTTP/2 is **off by default** (`--http2` to opt in); the S3 API
  is HTTP/1.1 in practice and h2 adds DoS surface (stream-multiplexing
  abuse) that doesn't pay off for our workload.
- **Tenant isolation**: S4 is **single-tenant by design** — one S4
  instance per security boundary. We do not enforce cross-bucket
  isolation at the S4 layer beyond what the backend's IAM enforces.
  Multi-tenant deployments should run one S4 instance per tenant with
  separate backend credentials.
- **Non-goals**: S4 is not an IDS / WAF, does not log request bodies
  (only headers + length), does not implement S3's `ObjectACL`
  Grant-by-CanonicalUser semantics beyond canned ACLs, does not
  proxy IAM API calls.

For incident reporting see [SECURITY.md](../../SECURITY.md).

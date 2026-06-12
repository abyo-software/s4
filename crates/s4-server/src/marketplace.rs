//! v1.3: AWS Marketplace paid-container metering — `RegisterUsage` at boot.
//!
//! AWS Marketplace **container** products with hourly pricing must call the
//! AWS Marketplace Metering Service `RegisterUsage` API once at container
//! startup. The single successful call both (a) verifies the customer's
//! entitlement to the product and (b) starts the per-pod / per-task hourly
//! metering clock on the AWS side — no further calls are required for the
//! lifetime of the pod (AWS measures runtime automatically after the
//! one-shot registration).
//!
//! S4 wires this behind `--marketplace-product-code <CODE>`:
//!
//! * **flag absent (default)** — this module is never invoked; behavior is
//!   bit-for-bit identical to every earlier release (v1.0 freeze contract).
//! * **flag present** — [`register_usage`] runs early in the boot sequence
//!   (before the backend S3 client is built). Success logs the product code
//!   and continues boot; ANY final failure aborts boot with a typed
//!   [`MarketplaceError`] (fail-closed: a paid container that cannot prove
//!   entitlement must not serve, per the AWS Marketplace integration
//!   requirements).
//!
//! ## Region
//!
//! `RegisterUsage` must be called in the **same region the ECS task / EKS
//! pod runs in** — the service rejects cross-region calls with
//! `InvalidRegionException` precisely to stop containers from hardcoding a
//! region (see the AWS API reference for `RegisterUsage`). We therefore let
//! the SDK's default provider chain resolve the region (ECS / EKS inject
//! `AWS_REGION` / `AWS_DEFAULT_REGION` into the container environment, and
//! IMDS is the fallback) and deliberately do NOT pin `us-east-1` and do NOT
//! reuse the gateway's `--endpoint-url` (that flag points at the backend
//! S3, not at the metering service).
//!
//! ## Platforms
//!
//! AWS only supports `RegisterUsage` from Amazon ECS, Amazon EKS, and
//! Fargate. A plain `docker run` on a laptop or a directly-launched EC2
//! instance gets `PlatformNotSupportedException` — by design, that is a
//! fatal boot error here (see [`MeteringCallError::PlatformNotSupported`]
//! for the operator-facing message). Use the free ghcr.io image (same
//! binary, just without this flag) outside ECS / EKS.
//!
//! ## Retry policy
//!
//! Per the AWS integration guidance, only `ThrottlingException` and
//! `InternalServiceErrorException` are retried (exponential backoff,
//! [`RetryPolicy::DEFAULT_MAX_RETRIES`] retries). Everything else —
//! `CustomerNotEntitledException`, `PlatformNotSupportedException`,
//! `InvalidProductCodeException`, … — fails immediately: retrying a
//! non-entitled customer or an unsupported platform cannot succeed.
//!
//! ## Signature verification — honest scope statement
//!
//! A successful `RegisterUsage` response carries a `signature` field: a JWS
//! (RS256) over the request fields + nonce, verifiable against the AWS
//! Marketplace public key identified by `PublicKeyVersion` (`= 1`, see
//! [`PUBLIC_KEY_VERSION`]). **Full cryptographic verification is NOT
//! implemented here**: the PublicKeyVersion=1 public key is issued
//! per-product through the AWS Marketplace Management Portal only after the
//! container product listing exists, and fabricating a placeholder key (or
//! skipping the trust root and "verifying" against a key baked from
//! nothing) would be security theater. What IS implemented:
//!
//! * presence check — a success response without a `signature` is treated
//!   as a failed registration ([`MarketplaceError::MissingSignature`]);
//! * a future hook — once the listing exists and AWS hands us the public
//!   key, RS256 JWS verification slots into [`register_usage`] at the
//!   marked call site without changing the public surface.
//!
//! Note the call being accepted by AWS *is* the entitlement check (the
//! metering service itself rejects non-entitled customers); the signature
//! exists so a paranoid workload can prove the response wasn't spoofed by
//! a man-in-the-middle inside the VPC. Presence-check-only is the honest
//! pre-listing baseline, not a substitute for verification.

use async_trait::async_trait;
use std::time::Duration;

/// `PublicKeyVersion` sent on every `RegisterUsage` call. AWS currently
/// defines version 1; bump only when AWS rotates the Marketplace metering
/// public key (the response's `PublicKeyRotationTimestamp` signals an
/// expired version).
pub const PUBLIC_KEY_VERSION: i32 = 1;

/// Classified outcome of one `RegisterUsage` attempt, decoupled from the
/// AWS SDK error types so the retry loop (and its unit tests) can run
/// against a mock [`MeteringClient`].
///
/// v1.3 stability: `#[non_exhaustive]` — new metering failure modes may be
/// added in minor releases. Downstream callers must include a `_ =>` arm
/// when matching on this enum.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum MeteringCallError {
    /// `CustomerNotEntitledException` — the AWS account running the pod
    /// has no valid subscription to this product. Fatal, never retried.
    #[error(
        "customer is not entitled to this AWS Marketplace product \
         (no valid subscription for the product code): {0}"
    )]
    CustomerNotEntitled(String),
    /// `PlatformNotSupportedException` — the container is not running on
    /// Amazon ECS / Amazon EKS / Fargate. Fatal, never retried.
    #[error(
        "AWS Marketplace metering is only supported on Amazon ECS, Amazon EKS, \
         and AWS Fargate — plain `docker run` and directly-launched EC2 \
         instances cannot call RegisterUsage. Deploy this Marketplace product \
         on ECS / EKS, or use the free ghcr.io image (same binary) without \
         --marketplace-product-code: {0}"
    )]
    PlatformNotSupported(String),
    /// `ThrottlingException` — retried with exponential backoff.
    #[error("AWS Marketplace metering throttled the RegisterUsage call: {0}")]
    Throttling(String),
    /// `InternalServiceErrorException` — retried with exponential backoff
    /// (the AWS docs explicitly say "Retry your request" for this one).
    #[error("AWS Marketplace metering internal service error: {0}")]
    InternalServiceError(String),
    /// `InvalidProductCodeException` — the supplied code does not match
    /// any published product. Fatal (a typo cannot be retried away).
    #[error(
        "the product code passed to --marketplace-product-code does not match \
         the product code of any published AWS Marketplace product: {0}"
    )]
    InvalidProductCode(String),
    /// Everything else: `DisabledApiException`, `InvalidRegionException`,
    /// `InvalidPublicKeyVersionException`, credential / connector / build
    /// failures, unmodeled service errors. Fatal — the boot loop must not
    /// spin on errors with no documented retry semantics (Kubernetes /
    /// ECS restart policy is the coarse-grained retry for genuinely
    /// transient network failures).
    #[error("AWS Marketplace RegisterUsage failed: {0}")]
    Other(String),
}

impl MeteringCallError {
    /// `true` only for the two error classes the AWS integration guidance
    /// designates as retryable (`ThrottlingException` /
    /// `InternalServiceErrorException`).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Throttling(_) | Self::InternalServiceError(_))
    }
}

/// Final outcome of the boot-time registration (after retries). Returned
/// to `main()`, which treats `Err` as a fatal boot error (process exits
/// non-zero before the listener ever binds).
///
/// v1.3 stability: `#[non_exhaustive]` — new failure modes may be added in
/// minor releases. Downstream callers must include a `_ =>` arm when
/// matching on this enum.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MarketplaceError {
    /// A non-retryable error (entitlement / platform / product-code / …).
    #[error(
        "AWS Marketplace RegisterUsage failed for product code {product_code} \
         (fatal, not retryable — refusing to start): {source}"
    )]
    Fatal {
        product_code: String,
        #[source]
        source: MeteringCallError,
    },
    /// Throttling / internal-service errors persisted through the whole
    /// backoff budget.
    #[error(
        "AWS Marketplace RegisterUsage for product code {product_code} still \
         failing after {attempts} attempts (exponential backoff exhausted — \
         refusing to start): {source}"
    )]
    RetriesExhausted {
        product_code: String,
        attempts: u32,
        #[source]
        source: MeteringCallError,
    },
    /// The service accepted the call but returned no `signature`. See the
    /// module docs — presence of the JWS is the minimum integrity bar we
    /// can enforce pre-listing, so its absence is treated as a failed
    /// registration rather than silently trusted.
    #[error(
        "AWS Marketplace RegisterUsage for product code {product_code} \
         returned success without a response signature — refusing to treat \
         the registration as valid"
    )]
    MissingSignature { product_code: String },
}

/// Subset of the `RegisterUsage` response the boot sequence cares about.
#[derive(Debug, Clone)]
pub struct RegisterUsageResponse {
    /// The JWS (RS256) over the request fields + nonce. `None` only on a
    /// malformed / spoofed response — see [`MarketplaceError::MissingSignature`].
    pub signature: Option<String>,
}

/// One `RegisterUsage` round-trip, abstracted so the retry / fail-closed
/// boot logic is unit-testable with a mock (the real AWS API only works
/// from inside an ECS task / EKS pod with `aws-marketplace:RegisterUsage`
/// IAM permission — there is no local emulator).
#[async_trait]
pub trait MeteringClient: Send + Sync {
    async fn register_usage(
        &self,
        product_code: &str,
        public_key_version: i32,
        nonce: &str,
    ) -> Result<RegisterUsageResponse, MeteringCallError>;
}

/// Production [`MeteringClient`] backed by `aws-sdk-marketplacemetering`.
#[derive(Debug, Clone)]
pub struct SdkMeteringClient {
    client: aws_sdk_marketplacemetering::Client,
}

impl SdkMeteringClient {
    /// Build from a resolved SDK config. The caller must pass a config
    /// WITHOUT the gateway's `--endpoint-url` override (that points at the
    /// backend S3) — `main()` loads a separate default-chain config for
    /// this client so region + endpoint resolve to the container's own
    /// region, as the metering service requires (see module docs).
    pub fn new(conf: &aws_config::SdkConfig) -> Self {
        Self {
            client: aws_sdk_marketplacemetering::Client::new(conf),
        }
    }
}

#[async_trait]
impl MeteringClient for SdkMeteringClient {
    async fn register_usage(
        &self,
        product_code: &str,
        public_key_version: i32,
        nonce: &str,
    ) -> Result<RegisterUsageResponse, MeteringCallError> {
        match self
            .client
            .register_usage()
            .product_code(product_code)
            .public_key_version(public_key_version)
            .nonce(nonce)
            .send()
            .await
        {
            Ok(out) => Ok(RegisterUsageResponse {
                signature: out.signature,
            }),
            // `into_service_error()` folds non-service failures (connector,
            // timeout, response-parse) into the `Unhandled` variant, which
            // `classify_sdk_error` maps to the fatal `Other` class.
            Err(sdk_err) => Err(classify_sdk_error(&sdk_err.into_service_error())),
        }
    }
}

/// Map the SDK's modeled `RegisterUsageError` onto our retry-classified
/// [`MeteringCallError`]. Pure + sync so the classification table is
/// directly unit-testable with builder-constructed exception values.
pub fn classify_sdk_error(
    err: &aws_sdk_marketplacemetering::operation::register_usage::RegisterUsageError,
) -> MeteringCallError {
    use aws_sdk_marketplacemetering::operation::register_usage::RegisterUsageError as E;
    // Modeled exceptions carry an optional message; normalize the absent
    // case so log lines never print a bare `None`.
    fn msg(m: Option<&str>) -> String {
        m.unwrap_or("(no message from service)").to_owned()
    }
    match err {
        E::CustomerNotEntitledException(e) => {
            MeteringCallError::CustomerNotEntitled(msg(e.message()))
        }
        E::PlatformNotSupportedException(e) => {
            MeteringCallError::PlatformNotSupported(msg(e.message()))
        }
        E::ThrottlingException(e) => MeteringCallError::Throttling(msg(e.message())),
        E::InternalServiceErrorException(e) => {
            MeteringCallError::InternalServiceError(msg(e.message()))
        }
        E::InvalidProductCodeException(e) => {
            MeteringCallError::InvalidProductCode(msg(e.message()))
        }
        // DisabledApi / InvalidRegion / InvalidPublicKeyVersion / Unhandled
        // (+ any variant AWS adds — RegisterUsageError is #[non_exhaustive]):
        // all fatal, all carry their own Display.
        other => MeteringCallError::Other(other.to_string()),
    }
}

/// Backoff knobs for the retryable error classes. Injectable so unit tests
/// run with zero delay; production uses [`RetryPolicy::default`].
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Retries AFTER the initial attempt (total attempts = max_retries + 1).
    pub max_retries: u32,
    /// First retry waits this long; each subsequent retry doubles it.
    pub base_delay: Duration,
}

impl RetryPolicy {
    /// 3 retries — the AWS integration guidance's recommended budget for
    /// `ThrottlingException` / `InternalServiceErrorException`.
    pub const DEFAULT_MAX_RETRIES: u32 = 3;
    /// 1 s base ⇒ 1 s / 2 s / 4 s waits (≤ 7 s added to a failing boot).
    pub const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(1);

    /// Delay before retry number `retry_index` (0-based): `base × 2^index`.
    /// No jitter — this is a one-shot boot call per pod, not a fleet-wide
    /// synchronized retry storm (pods start at operator-controlled times).
    pub fn delay_for(&self, retry_index: u32) -> Duration {
        // Cap the shift so a pathological max_retries cannot overflow.
        self.base_delay.saturating_mul(1u32 << retry_index.min(16))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: Self::DEFAULT_MAX_RETRIES,
            base_delay: Self::DEFAULT_BASE_DELAY,
        }
    }
}

/// Proof of a completed boot-time registration.
#[derive(Debug, Clone)]
pub struct RegisteredUsage {
    /// The JWS from the response (presence already enforced; full RS256
    /// verification is the documented future hook — see module docs).
    pub signature: String,
    /// Total attempts the registration took (1 = first try succeeded).
    pub attempts: u32,
}

/// Boot-time `RegisterUsage` with the fail-closed + retry semantics from
/// the module docs. Called from `main()` when `--marketplace-product-code`
/// is set; `Err` aborts boot.
///
/// Bumps `s4_marketplace_register_usage_total{result="ok"|"err"}` once per
/// final outcome (attempt-level retries are visible in the WARN logs, not
/// the counter — one boot = one sample keeps the metric trivially
/// alertable).
pub async fn register_usage(
    client: &dyn MeteringClient,
    product_code: &str,
    policy: RetryPolicy,
) -> Result<RegisteredUsage, MarketplaceError> {
    let outcome = register_usage_inner(client, product_code, policy).await;
    let result = if outcome.is_ok() { "ok" } else { "err" };
    crate::metrics::record_marketplace_register_usage(result);
    outcome
}

async fn register_usage_inner(
    client: &dyn MeteringClient,
    product_code: &str,
    policy: RetryPolicy,
) -> Result<RegisteredUsage, MarketplaceError> {
    // Fresh nonce per boot — lets a (future) signature verifier bind the
    // JWS to this exact registration and rules out replayed responses.
    let nonce = uuid::Uuid::new_v4().to_string();
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        match client
            .register_usage(product_code, PUBLIC_KEY_VERSION, &nonce)
            .await
        {
            Ok(response) => {
                // Future hook: RS256 JWS verification against the AWS
                // Marketplace PublicKeyVersion=1 public key goes here once
                // the product listing exists and AWS issues the key (see
                // the module-level "Signature verification" section for
                // why presence-only is the honest pre-listing baseline).
                let Some(signature) = response.signature else {
                    return Err(MarketplaceError::MissingSignature {
                        product_code: product_code.to_owned(),
                    });
                };
                return Ok(RegisteredUsage {
                    signature,
                    attempts,
                });
            }
            Err(err) if err.is_retryable() => {
                let retry_index = attempts - 1; // 0-based
                if retry_index >= policy.max_retries {
                    return Err(MarketplaceError::RetriesExhausted {
                        product_code: product_code.to_owned(),
                        attempts,
                        source: err,
                    });
                }
                let delay = policy.delay_for(retry_index);
                tracing::warn!(
                    product_code,
                    attempt = attempts,
                    max_attempts = policy.max_retries + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %err,
                    "AWS Marketplace RegisterUsage failed with a retryable error — backing off"
                );
                tokio::time::sleep(delay).await;
            }
            Err(err) => {
                return Err(MarketplaceError::Fatal {
                    product_code: product_code.to_owned(),
                    source: err,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Scripted mock: pops one result per call, panic-free bookkeeping of
    /// how many attempts the retry loop actually made.
    struct ScriptedClient {
        script: Mutex<Vec<Result<RegisterUsageResponse, MeteringCallError>>>,
        calls: Mutex<u32>,
    }

    impl ScriptedClient {
        fn new(script: Vec<Result<RegisterUsageResponse, MeteringCallError>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().expect("calls lock")
        }
    }

    #[async_trait]
    impl MeteringClient for ScriptedClient {
        async fn register_usage(
            &self,
            product_code: &str,
            public_key_version: i32,
            nonce: &str,
        ) -> Result<RegisterUsageResponse, MeteringCallError> {
            assert_eq!(product_code, "prod-test123", "product code passthrough");
            assert_eq!(public_key_version, PUBLIC_KEY_VERSION);
            assert!(!nonce.is_empty(), "nonce must be generated per boot");
            *self.calls.lock().expect("calls lock") += 1;
            let mut script = self.script.lock().expect("script lock");
            assert!(!script.is_empty(), "mock called more times than scripted");
            script.remove(0)
        }
    }

    fn ok_response() -> Result<RegisterUsageResponse, MeteringCallError> {
        Ok(RegisterUsageResponse {
            signature: Some("eyJhbGciOiJQUzI1NiJ9.payload.sig".to_owned()),
        })
    }

    fn zero_delay() -> RetryPolicy {
        RetryPolicy {
            max_retries: RetryPolicy::DEFAULT_MAX_RETRIES,
            base_delay: Duration::ZERO,
        }
    }

    // ---- error classification: retryable vs fatal --------------------

    #[test]
    fn retryable_classification_matches_aws_guidance() {
        let retryable = [
            MeteringCallError::Throttling("x".into()),
            MeteringCallError::InternalServiceError("x".into()),
        ];
        let fatal = [
            MeteringCallError::CustomerNotEntitled("x".into()),
            MeteringCallError::PlatformNotSupported("x".into()),
            MeteringCallError::InvalidProductCode("x".into()),
            MeteringCallError::Other("x".into()),
        ];
        for e in &retryable {
            assert!(e.is_retryable(), "{e:?} must be retryable");
        }
        for e in &fatal {
            assert!(!e.is_retryable(), "{e:?} must be fatal");
        }
    }

    /// One classification case: a builder-constructed SDK error and the
    /// predicate the classified [`MeteringCallError`] must satisfy.
    type ClassifyCase = (
        aws_sdk_marketplacemetering::operation::register_usage::RegisterUsageError,
        fn(&MeteringCallError) -> bool,
    );

    #[test]
    fn classify_sdk_error_maps_modeled_exceptions() {
        use aws_sdk_marketplacemetering::operation::register_usage::RegisterUsageError as E;
        use aws_sdk_marketplacemetering::types::error as sdk_err;

        let cases: Vec<ClassifyCase> = vec![
            (
                E::CustomerNotEntitledException(
                    sdk_err::CustomerNotEntitledException::builder()
                        .message("no subscription")
                        .build(),
                ),
                |c| matches!(c, MeteringCallError::CustomerNotEntitled(m) if m == "no subscription"),
            ),
            (
                E::PlatformNotSupportedException(
                    sdk_err::PlatformNotSupportedException::builder().build(),
                ),
                |c| {
                    matches!(c, MeteringCallError::PlatformNotSupported(m)
                        if m == "(no message from service)")
                },
            ),
            (
                E::ThrottlingException(sdk_err::ThrottlingException::builder().build()),
                |c| matches!(c, MeteringCallError::Throttling(_)),
            ),
            (
                E::InternalServiceErrorException(
                    sdk_err::InternalServiceErrorException::builder().build(),
                ),
                |c| matches!(c, MeteringCallError::InternalServiceError(_)),
            ),
            (
                E::InvalidProductCodeException(
                    sdk_err::InvalidProductCodeException::builder().build(),
                ),
                |c| matches!(c, MeteringCallError::InvalidProductCode(_)),
            ),
            (
                E::InvalidRegionException(sdk_err::InvalidRegionException::builder().build()),
                |c| matches!(c, MeteringCallError::Other(_)),
            ),
            (
                E::DisabledApiException(sdk_err::DisabledApiException::builder().build()),
                |c| matches!(c, MeteringCallError::Other(_)),
            ),
        ];
        for (sdk, check) in cases {
            let classified = classify_sdk_error(&sdk);
            assert!(
                check(&classified),
                "misclassified: {sdk:?} -> {classified:?}"
            );
        }
    }

    // ---- backoff schedule ---------------------------------------------

    #[test]
    fn backoff_is_exponential_from_base() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay_for(0), Duration::from_secs(1));
        assert_eq!(policy.delay_for(1), Duration::from_secs(2));
        assert_eq!(policy.delay_for(2), Duration::from_secs(4));
        // Shift cap: huge indices saturate instead of overflowing.
        assert!(policy.delay_for(40) >= policy.delay_for(16));
    }

    // ---- boot sequence: success → continue ----------------------------

    #[tokio::test]
    async fn first_try_success_registers_and_continues() {
        // Touch the shared test recorder so the counter bump is observable
        // (and so this test exercises the same recorder path as the rest
        // of the crate's metrics tests).
        let handle = crate::metrics::test_metrics_handle();
        let client = ScriptedClient::new(vec![ok_response()]);
        let registered = register_usage(&client, "prod-test123", zero_delay())
            .await
            .expect("entitled customer must boot");
        assert_eq!(registered.attempts, 1);
        assert_eq!(client.calls(), 1);
        assert!(registered.signature.contains('.'), "JWS passthrough");
        let rendered = handle.render();
        assert!(
            rendered.contains("s4_marketplace_register_usage_total"),
            "counter must be registered after a final outcome:\n{rendered}"
        );
    }

    // ---- boot sequence: retryable errors recover ----------------------

    #[tokio::test]
    async fn throttling_retries_then_succeeds() {
        let client = ScriptedClient::new(vec![
            Err(MeteringCallError::Throttling("slow down".into())),
            Err(MeteringCallError::InternalServiceError("oops".into())),
            ok_response(),
        ]);
        let registered = register_usage(&client, "prod-test123", zero_delay())
            .await
            .expect("recovers within the retry budget");
        assert_eq!(registered.attempts, 3);
        assert_eq!(client.calls(), 3);
    }

    #[tokio::test]
    async fn retry_budget_exhaustion_refuses_boot() {
        // 1 initial + 3 retries = 4 attempts, all throttled.
        let client = ScriptedClient::new(vec![
            Err(MeteringCallError::Throttling("1".into())),
            Err(MeteringCallError::Throttling("2".into())),
            Err(MeteringCallError::Throttling("3".into())),
            Err(MeteringCallError::Throttling("4".into())),
        ]);
        let err = register_usage(&client, "prod-test123", zero_delay())
            .await
            .expect_err("exhausted budget must refuse boot");
        assert_eq!(client.calls(), 4);
        match err {
            MarketplaceError::RetriesExhausted {
                product_code,
                attempts,
                source,
            } => {
                assert_eq!(product_code, "prod-test123");
                assert_eq!(attempts, 4);
                assert!(source.is_retryable());
            }
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
    }

    // ---- boot sequence: fatal errors refuse immediately ---------------

    #[tokio::test]
    async fn customer_not_entitled_refuses_boot_without_retry() {
        let client = ScriptedClient::new(vec![Err(MeteringCallError::CustomerNotEntitled(
            "no subscription".into(),
        ))]);
        let err = register_usage(&client, "prod-test123", zero_delay())
            .await
            .expect_err("non-entitled customer must not boot");
        assert_eq!(client.calls(), 1, "fatal errors must not be retried");
        assert!(matches!(err, MarketplaceError::Fatal { .. }));
        assert!(err.to_string().contains("refusing to start"));
    }

    #[tokio::test]
    async fn platform_not_supported_message_names_supported_platforms() {
        let client = ScriptedClient::new(vec![Err(MeteringCallError::PlatformNotSupported(
            "not ECS".into(),
        ))]);
        let err = register_usage(&client, "prod-test123", zero_delay())
            .await
            .expect_err("plain docker / EC2 must not boot with the flag");
        assert_eq!(client.calls(), 1);
        let msg = err.to_string();
        for needle in ["ECS", "EKS", "Fargate", "ghcr.io"] {
            assert!(msg.contains(needle), "missing `{needle}` in: {msg}");
        }
    }

    #[tokio::test]
    async fn missing_signature_refuses_boot() {
        let client = ScriptedClient::new(vec![Ok(RegisterUsageResponse { signature: None })]);
        let err = register_usage(&client, "prod-test123", zero_delay())
            .await
            .expect_err("signature presence is the minimum integrity bar");
        assert!(matches!(err, MarketplaceError::MissingSignature { .. }));
    }
}

//! v1.3: AWS Marketplace paid-container metering.
//!
//! ## Two metering routes (they are mutually exclusive)
//!
//! AWS Marketplace **container** products meter usage one of two ways, and
//! a product is configured for exactly one of them at listing time:
//!
//! 1. **`RegisterUsage` — per-pod/per-task hourly, AWS auto-meters.** Used
//!    when the product has *no custom dimension* (the dimension would not
//!    appear, or appears with catalog type `Metered`). One call at boot
//!    both verifies entitlement and starts AWS's hourly metering clock; the
//!    software makes no further metering calls. Selected by setting
//!    `--marketplace-product-code` and leaving `--marketplace-usage-dimension`
//!    unset. See [`register_usage`].
//!
//! 2. **`MeterUsage` — custom ("externally metered") dimension, the seller
//!    meters hourly.** Used when the product's pricing defines a custom
//!    dimension (catalog type `ExternallyMetered`). AWS does **not** meter
//!    automatically: the software must (a) call `MeterUsage` with
//!    `DryRun=true` at boot to confirm entitlement, and (b) send one
//!    `MeterUsage` record per pod per hour against that dimension for the
//!    lifetime of the pod. Selected by setting BOTH
//!    `--marketplace-product-code` and `--marketplace-usage-dimension <NAME>`
//!    (the dimension's API name, e.g. `Hours`). See
//!    [`meter_usage_entitlement_check`] (boot, fail-closed) and
//!    [`meter_one_hour`] (the hourly loop, fail-open).
//!
//! Picking the wrong route is silently broken: `RegisterUsage` never emits
//! a record against an `ExternallyMetered` dimension, so AWS rejects the
//! listing with "all metered dimensions must be registered at the metering
//! service". Confirm the route with the dimension's `Types` in the product
//! entity (`describe-entity` → `Dimensions[].Types`) before deploying.
//!
//! ## `RegisterUsage` route
//!
//! AWS Marketplace **container** products with per-pod hourly pricing call
//! the AWS Marketplace Metering Service `RegisterUsage` API once at
//! container startup. The single successful call both (a) verifies the
//! customer's entitlement to the product and (b) starts the per-pod /
//! per-task hourly metering clock on the AWS side — no further calls are
//! required for the lifetime of the pod (AWS measures runtime automatically
//! after the one-shot registration).
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
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

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
    /// MeterUsage route: the boot-time DryRun entitlement check failed with
    /// a non-retryable error (not entitled / invalid dimension / invalid
    /// product code / region / …). Fail-closed: the gateway refuses to
    /// start.
    #[error(
        "AWS Marketplace MeterUsage DryRun entitlement check failed for \
         product code {product_code} dimension {dimension} (fatal, not \
         retryable — refusing to start): {source}"
    )]
    MeterUsageFatal {
        product_code: String,
        dimension: String,
        #[source]
        source: MeterUsageCallError,
    },
    /// MeterUsage route: the boot-time DryRun entitlement check kept hitting
    /// Throttling / InternalServiceError through the whole backoff budget.
    #[error(
        "AWS Marketplace MeterUsage DryRun entitlement check for product \
         code {product_code} dimension {dimension} still failing after \
         {attempts} attempts (exponential backoff exhausted — refusing to \
         start): {source}"
    )]
    MeterUsageRetriesExhausted {
        product_code: String,
        dimension: String,
        attempts: u32,
        #[source]
        source: MeterUsageCallError,
    },
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

// ============================================================================
// MeterUsage route — custom ("externally metered") hourly metering
// ============================================================================
//
// See the module-level "Two metering routes" section. This route is used
// when the Marketplace product defines a custom dimension (catalog type
// `ExternallyMetered`). Unlike RegisterUsage, AWS does not meter
// automatically: the gateway proves entitlement once at boot (DryRun, fail-
// closed) and then sends one record per pod per hour for the dimension.

/// Classified outcome of one `MeterUsage` attempt, decoupled from the AWS
/// SDK error types so the boot retry loop and the hourly loop (and their
/// unit tests) can run against a mock [`MeterUsageClient`].
///
/// v1.3 stability: `#[non_exhaustive]` — new metering failure modes may be
/// added in minor releases. Downstream callers must include a `_ =>` arm
/// when matching on this enum.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum MeterUsageCallError {
    /// `CustomerNotEntitledException` — the AWS account running the pod has
    /// no valid subscription. Fatal at boot, never retried. (Per AWS, this
    /// is only thrown on the first call; a mid-run unsubscribe does not stop
    /// an already-running pod from being metered.)
    #[error(
        "customer is not entitled to this AWS Marketplace product \
         (no valid subscription for the product code): {0}"
    )]
    CustomerNotEntitled(String),
    /// `DuplicateRequestException` — a record for this {dimension,
    /// timestamp-hour} was already emitted by this resource. Benign for the
    /// hourly loop (the hour is already metered); impossible on a DryRun.
    #[error("AWS Marketplace MeterUsage: this pod-hour was already metered: {0}")]
    DuplicateRequest(String),
    /// `InvalidUsageDimensionException` — the dimension name passed does not
    /// match any dimension defined on the product. Fatal: a wrong
    /// `--marketplace-usage-dimension` cannot be retried away.
    #[error(
        "the dimension passed to --marketplace-usage-dimension does not \
         match any UsageDimension defined on this AWS Marketplace product \
         (check the dimension API name in the product's pricing): {0}"
    )]
    InvalidUsageDimension(String),
    /// `InvalidProductCodeException` — the product code matches no published
    /// product. Fatal.
    #[error(
        "the product code passed to --marketplace-product-code does not \
         match the product code of any published AWS Marketplace product: {0}"
    )]
    InvalidProductCode(String),
    /// `UnauthorizedException` — a DryRun call whose IAM identity lacks the
    /// `aws-marketplace:MeterUsage` permission (this is the DryRun "not
    /// permitted" signal; the permitted signal is the `DryRunOperation`
    /// code, which the SDK client translates to `Ok`). Fatal: an unsigned /
    /// under-privileged pod must not serve a paid product.
    #[error(
        "the pod's IAM identity is not authorized to call AWS Marketplace \
         MeterUsage — grant aws-marketplace:MeterUsage via IRSA (EKS) or the \
         task role (ECS): {0}"
    )]
    Unauthorized(String),
    /// `TimestampOutOfBoundsException` — the record timestamp is outside the
    /// accepted window (AWS accepts up to 6 h in the past). Fatal at boot; in
    /// the hourly loop it surfaces as a failed hour (logged + retried, then
    /// dropped once it ages past the window).
    #[error("AWS Marketplace MeterUsage timestamp out of allowed range: {0}")]
    TimestampOutOfBounds(String),
    /// `ThrottlingException` — retried with exponential backoff.
    #[error("AWS Marketplace metering throttled the MeterUsage call: {0}")]
    Throttling(String),
    /// `InternalServiceErrorException` — retried with exponential backoff
    /// (the AWS docs explicitly say "Retry your request" for this one).
    #[error("AWS Marketplace metering internal service error: {0}")]
    InternalServiceError(String),
    /// The call did not return within the per-call timeout (a hung
    /// connection with no SDK-level deadline). Only produced by
    /// [`meter_one_hour`]; the hourly loop retains the hour and backfills it
    /// on a later tick, and the timeout keeps a stuck send from blocking the
    /// loop past its tick interval (or delaying shutdown).
    #[error("AWS Marketplace MeterUsage call timed out: {0}")]
    Timeout(String),
    /// Everything else: `InvalidEndpointRegionException`,
    /// `InvalidTagException`, `InvalidUsageAllocationsException`,
    /// `IdempotencyConflictException`, credential / connector / build
    /// failures, unmodeled service errors. Fatal at boot.
    #[error("AWS Marketplace MeterUsage failed: {0}")]
    Other(String),
}

impl MeterUsageCallError {
    /// `true` only for the two error classes AWS designates as retryable
    /// (`ThrottlingException` / `InternalServiceErrorException`).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Throttling(_) | Self::InternalServiceError(_))
    }
}

/// `true` iff a failed `MeterUsage` call was actually a *permitted* DryRun.
///
/// AWS does not return a normal `Ok` for `DryRun=true`: a permitted DryRun
/// comes back as the `DryRunOperation` error code, and an unpermitted one as
/// `UnauthorizedException` (see the `MeterUsage` API reference, `DryRun`
/// parameter). Neither is in the operation's modeled error set, so both
/// arrive as the SDK `Unhandled` variant — we must inspect the error *code*
/// to tell "DryRun succeeded" from a real failure.
pub fn is_dry_run_success(dry_run: bool, error_code: Option<&str>) -> bool {
    dry_run && error_code == Some("DryRunOperation")
}

/// Map an *unmodeled* `MeterUsage` error (a common error not in the
/// operation's modeled set, surfaced by the SDK as `Unhandled`) onto our
/// classification by its error code. `UnauthorizedException` (the DryRun
/// "not permitted" signal) gets a clear IAM message; everything else is
/// `Other` (fatal at boot). Pure + sync so it is directly unit-testable
/// without constructing a sealed `Unhandled`.
pub fn classify_meter_usage_unmodeled(code: Option<&str>, display: String) -> MeterUsageCallError {
    match code {
        Some("UnauthorizedException") => MeterUsageCallError::Unauthorized(display),
        _ => MeterUsageCallError::Other(display),
    }
}

/// Map the SDK's modeled `MeterUsageError` onto our retry-classified
/// [`MeterUsageCallError`]. Pure + sync so the classification table is
/// directly unit-testable with builder-constructed exception values.
pub fn classify_meter_usage_sdk_error(
    err: &aws_sdk_marketplacemetering::operation::meter_usage::MeterUsageError,
) -> MeterUsageCallError {
    use aws_sdk_marketplacemetering::error::ProvideErrorMetadata as _;
    use aws_sdk_marketplacemetering::operation::meter_usage::MeterUsageError as E;
    fn msg(m: Option<&str>) -> String {
        m.unwrap_or("(no message from service)").to_owned()
    }
    match err {
        E::CustomerNotEntitledException(e) => {
            MeterUsageCallError::CustomerNotEntitled(msg(e.message()))
        }
        E::DuplicateRequestException(e) => MeterUsageCallError::DuplicateRequest(msg(e.message())),
        E::InvalidUsageDimensionException(e) => {
            MeterUsageCallError::InvalidUsageDimension(msg(e.message()))
        }
        E::InvalidProductCodeException(e) => {
            MeterUsageCallError::InvalidProductCode(msg(e.message()))
        }
        E::TimestampOutOfBoundsException(e) => {
            MeterUsageCallError::TimestampOutOfBounds(msg(e.message()))
        }
        E::ThrottlingException(e) => MeterUsageCallError::Throttling(msg(e.message())),
        E::InternalServiceErrorException(e) => {
            MeterUsageCallError::InternalServiceError(msg(e.message()))
        }
        // InvalidEndpointRegion / InvalidTag / InvalidUsageAllocations /
        // IdempotencyConflict / Unhandled (+ any variant AWS adds —
        // MeterUsageError is #[non_exhaustive]). The DryRun signals
        // (UnauthorizedException, and DryRunOperation which the SDK client
        // intercepts before classification) land here as `Unhandled`, so we
        // dispatch on the error code.
        other => classify_meter_usage_unmodeled(other.code(), other.to_string()),
    }
}

/// Subset of the `MeterUsage` response the caller cares about.
#[derive(Debug, Clone)]
pub struct MeterUsageResponse {
    /// The metering record id AWS assigns to an accepted (non-DryRun)
    /// record. `None` for a DryRun (which records nothing).
    pub metering_record_id: Option<String>,
}

/// One `MeterUsage` round-trip, abstracted so the boot fail-closed logic
/// and the hourly fail-open loop are unit-testable with a mock (the real
/// AWS API only works from inside an ECS task / EKS pod with
/// `aws-marketplace:MeterUsage` IAM permission — there is no local
/// emulator).
#[async_trait]
pub trait MeterUsageClient: Send + Sync {
    async fn meter_usage(
        &self,
        product_code: &str,
        dimension: &str,
        quantity: i32,
        timestamp: SystemTime,
        dry_run: bool,
    ) -> Result<MeterUsageResponse, MeterUsageCallError>;
}

#[async_trait]
impl MeterUsageClient for SdkMeteringClient {
    async fn meter_usage(
        &self,
        product_code: &str,
        dimension: &str,
        quantity: i32,
        timestamp: SystemTime,
        dry_run: bool,
    ) -> Result<MeterUsageResponse, MeterUsageCallError> {
        let ts = aws_sdk_marketplacemetering::primitives::DateTime::from(timestamp);
        match self
            .client
            .meter_usage()
            .product_code(product_code)
            .timestamp(ts)
            .usage_dimension(dimension)
            .usage_quantity(quantity)
            .dry_run(dry_run)
            .send()
            .await
        {
            Ok(out) => Ok(MeterUsageResponse {
                metering_record_id: out.metering_record_id().map(str::to_owned),
            }),
            Err(sdk_err) => {
                use aws_sdk_marketplacemetering::error::ProvideErrorMetadata as _;
                let svc = sdk_err.into_service_error();
                // A permitted DryRun is reported as the `DryRunOperation`
                // error code, NOT a normal Ok — translate it to success so
                // the boot entitlement check (and only it ever sets
                // dry_run=true) sees a clean pass instead of aborting boot.
                if is_dry_run_success(dry_run, svc.code()) {
                    return Ok(MeterUsageResponse {
                        metering_record_id: None,
                    });
                }
                Err(classify_meter_usage_sdk_error(&svc))
            }
        }
    }
}

/// Boot-time `MeterUsage` DryRun entitlement check for the custom-metering
/// route. Mirrors [`register_usage`]'s fail-closed + retry semantics:
/// `Err` aborts boot (a paid container that cannot prove entitlement must
/// not serve). Bumps `s4_marketplace_meter_usage_total{result="entitlement_ok"
/// |"entitlement_err"}` once per final outcome.
///
/// A permitted DryRun is signaled by AWS with the `DryRunOperation` error
/// code, which [`SdkMeteringClient::meter_usage`] translates to `Ok` — so a
/// clean entitlement check returns `Ok` here. `CustomerNotEntitled`,
/// `Unauthorized` (missing `aws-marketplace:MeterUsage` IAM), invalid
/// product code, and invalid dimension are all fatal. DryRun records
/// nothing, so it never bills; if AWS reports a duplicate we treat it as
/// proof of entitlement and continue.
pub async fn meter_usage_entitlement_check(
    client: &dyn MeterUsageClient,
    product_code: &str,
    dimension: &str,
    policy: RetryPolicy,
) -> Result<(), MarketplaceError> {
    let outcome = meter_usage_entitlement_inner(client, product_code, dimension, policy).await;
    let result = if outcome.is_ok() {
        "entitlement_ok"
    } else {
        "entitlement_err"
    };
    crate::metrics::record_marketplace_meter_usage(result);
    outcome
}

async fn meter_usage_entitlement_inner(
    client: &dyn MeterUsageClient,
    product_code: &str,
    dimension: &str,
    policy: RetryPolicy,
) -> Result<(), MarketplaceError> {
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        // DryRun=true: validate entitlement + dimension only. Quantity 1 is
        // a valid placeholder; nothing is billed.
        match client
            .meter_usage(product_code, dimension, 1, SystemTime::now(), true)
            .await
        {
            Ok(_) => return Ok(()),
            // A duplicate on a DryRun is effectively impossible, but if it
            // happens it still proves the customer is entitled — succeed.
            Err(MeterUsageCallError::DuplicateRequest(_)) => return Ok(()),
            Err(err) if err.is_retryable() => {
                let retry_index = attempts - 1; // 0-based
                if retry_index >= policy.max_retries {
                    return Err(MarketplaceError::MeterUsageRetriesExhausted {
                        product_code: product_code.to_owned(),
                        dimension: dimension.to_owned(),
                        attempts,
                        source: err,
                    });
                }
                let delay = policy.delay_for(retry_index);
                tracing::warn!(
                    product_code,
                    dimension,
                    attempt = attempts,
                    max_attempts = policy.max_retries + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %err,
                    "AWS Marketplace MeterUsage DryRun failed with a retryable error — backing off"
                );
                tokio::time::sleep(delay).await;
            }
            Err(err) => {
                return Err(MarketplaceError::MeterUsageFatal {
                    product_code: product_code.to_owned(),
                    dimension: dimension.to_owned(),
                    source: err,
                });
            }
        }
    }
}

/// Outcome of one hourly [`meter_one_hour`] send, for the caller to log.
///
/// v1.3 stability: `#[non_exhaustive]`.
#[derive(Debug)]
#[non_exhaustive]
pub enum MeterOutcome {
    /// A real per-pod-hour record was accepted.
    Metered { record_id: Option<String> },
    /// This pod-hour was already metered (`DuplicateRequestException`) —
    /// benign, no action needed.
    AlreadyMetered,
    /// The call failed. The hourly loop is **fail-open**: it logs + counts
    /// this and keeps serving (entitlement was enforced at boot), retrying
    /// on the next hourly tick.
    Failed(MeterUsageCallError),
}

/// Send one real (DryRun=false) `MeterUsage` record of `quantity` units for
/// the clock hour containing `now`, against `dimension`. Records
/// `s4_marketplace_meter_usage_total{result=...}` and returns the classified
/// [`MeterOutcome`] for the caller to log. Never returns `Err` / never
/// panics — a metering hiccup must never take down a paying customer's data
/// plane.
///
/// The call is bounded by `timeout`: a hung connection (no SDK deadline)
/// surfaces as [`MeterUsageCallError::Timeout`] / [`MeterOutcome::Failed`]
/// instead of blocking the hourly loop past its tick interval (which would
/// otherwise miss subsequent hour buckets) or stalling shutdown.
///
/// `now` is injected for testability (and for backfilling a past hour);
/// production passes `SystemTime::now()` for the current hour or a retained
/// past hour from the backlog.
///
/// **Billing unit:** one unit means "this pod was active during that clock
/// hour" — whole-hour granularity, *not* prorated to the second the way
/// `RegisterUsage` is. A pod that lives five minutes still meters one unit
/// for that hour.
///
/// **Idempotency / duplicates (per the `MeterUsage` API reference):** AWS
/// rounds the timestamp down to the hour and enforces once-per-hour metering
/// *per ECS task / EKS pod*. Re-sending the same {dimension, hour} with the
/// *same* quantity is idempotent — AWS returns the original `MeteringRecordId`
/// and does not double-charge — which makes backfill retries safe. Only a
/// *different* quantity for the same {dimension, hour} yields
/// `DuplicateRequestException` ([`MeterOutcome::AlreadyMetered`]). Because the
/// rule is per-pod, it does **not** dedup across pod restarts: a restarted
/// pod is a new pod and can be metered again for the same hour (an inherent
/// property of per-pod-hour custom metering, not something this code can
/// prevent without external aggregation).
pub async fn meter_one_hour(
    client: &dyn MeterUsageClient,
    product_code: &str,
    dimension: &str,
    quantity: i32,
    now: SystemTime,
    timeout: Duration,
) -> MeterOutcome {
    let call = client.meter_usage(product_code, dimension, quantity, now, false);
    let outcome = match tokio::time::timeout(timeout, call).await {
        Ok(Ok(resp)) => MeterOutcome::Metered {
            record_id: resp.metering_record_id,
        },
        Ok(Err(MeterUsageCallError::DuplicateRequest(_))) => MeterOutcome::AlreadyMetered,
        Ok(Err(err)) => MeterOutcome::Failed(err),
        Err(_elapsed) => MeterOutcome::Failed(MeterUsageCallError::Timeout(format!(
            "no response within {}s",
            timeout.as_secs()
        ))),
    };
    let result = match &outcome {
        MeterOutcome::Metered { .. } => "ok",
        MeterOutcome::AlreadyMetered => "duplicate",
        MeterOutcome::Failed(_) => "err",
    };
    crate::metrics::record_marketplace_meter_usage(result);
    outcome
}

/// Per-call timeout for an hourly [`meter_one_hour`] send. Far below the
/// 1-hour tick interval so a hung connection can never delay the loop past
/// its next tick (which would miss an hour bucket) or stall shutdown; far
/// above any healthy round-trip so it never trips in normal operation.
pub const METER_USAGE_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum age of a pod-hour we will still try to meter. AWS rejects
/// `MeterUsage` records "more than six hours after events occur"; we keep a
/// 10-minute safety margin so a record never ages out between the staleness
/// check and the `send()`.
pub const MAX_METER_BACKLOG: Duration = Duration::from_secs(6 * 3600 - 600);

/// Drop pod-hour timestamps from the front of `pending` that are older than
/// `max_age` (AWS will reject them, so retrying is pointless). Returns how
/// many were dropped — the caller MUST log a non-zero count: those are
/// pod-hours that will never be billed (a sustained metering outage), and
/// silently discarding billable usage would be a revenue bug hiding as a
/// no-op.
///
/// `pending` is kept oldest-first (the hourly loop pushes `now` to the back),
/// so this only ever pops from the front. A timestamp in the future relative
/// to `now` (clock skew) is treated as "recent" and kept.
pub fn drop_stale_pending(
    pending: &mut VecDeque<SystemTime>,
    now: SystemTime,
    max_age: Duration,
) -> usize {
    let mut dropped = 0;
    while let Some(&front) = pending.front() {
        match now.duration_since(front) {
            Ok(age) if age > max_age => {
                pending.pop_front();
                dropped += 1;
            }
            // Front is young enough, or is in the future (skew) — stop.
            _ => break,
        }
    }
    dropped
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

    /// A per-call timeout long enough that an immediately-resolving mock
    /// never trips it (the timeout path is exercised separately).
    fn far_timeout() -> Duration {
        Duration::from_secs(30)
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

    // ====================================================================
    // MeterUsage route
    // ====================================================================

    /// Scripted MeterUsage mock: pops one result per call and records every
    /// call's (quantity, dry_run) so tests can assert the boot DryRun vs the
    /// hourly real-record distinction.
    struct ScriptedMeterClient {
        script: Mutex<Vec<Result<MeterUsageResponse, MeterUsageCallError>>>,
        calls: Mutex<Vec<(i32, bool)>>,
    }

    impl ScriptedMeterClient {
        fn new(script: Vec<Result<MeterUsageResponse, MeterUsageCallError>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(i32, bool)> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    #[async_trait]
    impl MeterUsageClient for ScriptedMeterClient {
        async fn meter_usage(
            &self,
            product_code: &str,
            dimension: &str,
            quantity: i32,
            _timestamp: SystemTime,
            dry_run: bool,
        ) -> Result<MeterUsageResponse, MeterUsageCallError> {
            assert_eq!(product_code, "prod-test123", "product code passthrough");
            assert_eq!(dimension, "Hours", "dimension passthrough");
            self.calls
                .lock()
                .expect("calls lock")
                .push((quantity, dry_run));
            let mut script = self.script.lock().expect("script lock");
            assert!(!script.is_empty(), "mock called more times than scripted");
            script.remove(0)
        }
    }

    fn ok_meter() -> Result<MeterUsageResponse, MeterUsageCallError> {
        Ok(MeterUsageResponse {
            metering_record_id: Some("rec-abc123".to_owned()),
        })
    }

    #[test]
    fn meter_usage_retryable_classification() {
        assert!(MeterUsageCallError::Throttling("x".into()).is_retryable());
        assert!(MeterUsageCallError::InternalServiceError("x".into()).is_retryable());
        for e in [
            MeterUsageCallError::CustomerNotEntitled("x".into()),
            MeterUsageCallError::DuplicateRequest("x".into()),
            MeterUsageCallError::InvalidUsageDimension("x".into()),
            MeterUsageCallError::InvalidProductCode("x".into()),
            MeterUsageCallError::TimestampOutOfBounds("x".into()),
            MeterUsageCallError::Other("x".into()),
        ] {
            assert!(!e.is_retryable(), "{e:?} must be fatal");
        }
    }

    /// One MeterUsage classification case (aliased to keep clippy's
    /// `type_complexity` lint happy, mirroring `ClassifyCase` above).
    type MeterClassifyCase = (
        aws_sdk_marketplacemetering::operation::meter_usage::MeterUsageError,
        fn(&MeterUsageCallError) -> bool,
    );

    #[test]
    fn classify_meter_usage_sdk_error_maps_modeled_exceptions() {
        use aws_sdk_marketplacemetering::operation::meter_usage::MeterUsageError as E;
        use aws_sdk_marketplacemetering::types::error as sdk_err;

        let cases: Vec<MeterClassifyCase> = vec![
            (
                E::CustomerNotEntitledException(
                    sdk_err::CustomerNotEntitledException::builder()
                        .message("no sub")
                        .build(),
                ),
                |c| matches!(c, MeterUsageCallError::CustomerNotEntitled(m) if m == "no sub"),
            ),
            (
                E::DuplicateRequestException(sdk_err::DuplicateRequestException::builder().build()),
                |c| matches!(c, MeterUsageCallError::DuplicateRequest(_)),
            ),
            (
                E::InvalidUsageDimensionException(
                    sdk_err::InvalidUsageDimensionException::builder().build(),
                ),
                |c| matches!(c, MeterUsageCallError::InvalidUsageDimension(_)),
            ),
            (
                E::InvalidProductCodeException(
                    sdk_err::InvalidProductCodeException::builder().build(),
                ),
                |c| matches!(c, MeterUsageCallError::InvalidProductCode(_)),
            ),
            (
                E::TimestampOutOfBoundsException(
                    sdk_err::TimestampOutOfBoundsException::builder().build(),
                ),
                |c| matches!(c, MeterUsageCallError::TimestampOutOfBounds(_)),
            ),
            (
                E::ThrottlingException(sdk_err::ThrottlingException::builder().build()),
                |c| matches!(c, MeterUsageCallError::Throttling(_)),
            ),
            (
                E::InternalServiceErrorException(
                    sdk_err::InternalServiceErrorException::builder().build(),
                ),
                |c| matches!(c, MeterUsageCallError::InternalServiceError(_)),
            ),
            (
                // Catch-all → Other (region mismatch is fatal at boot).
                E::InvalidEndpointRegionException(
                    sdk_err::InvalidEndpointRegionException::builder().build(),
                ),
                |c| matches!(c, MeterUsageCallError::Other(_)),
            ),
        ];
        for (sdk, check) in cases {
            let classified = classify_meter_usage_sdk_error(&sdk);
            assert!(
                check(&classified),
                "misclassified: {sdk:?} -> {classified:?}"
            );
        }
    }

    #[tokio::test]
    async fn meter_entitlement_dryrun_succeeds() {
        let handle = crate::metrics::test_metrics_handle();
        let client = ScriptedMeterClient::new(vec![ok_meter()]);
        meter_usage_entitlement_check(&client, "prod-test123", "Hours", zero_delay())
            .await
            .expect("entitled customer must boot");
        // Exactly one call, and it MUST be a DryRun (we never bill at boot).
        assert_eq!(client.calls(), vec![(1, true)]);
        assert!(
            handle.render().contains("s4_marketplace_meter_usage_total"),
            "meter_usage counter must be registered after the boot check"
        );
    }

    #[tokio::test]
    async fn meter_entitlement_dryrun_retries_then_succeeds() {
        let client = ScriptedMeterClient::new(vec![
            Err(MeterUsageCallError::Throttling("slow".into())),
            Err(MeterUsageCallError::InternalServiceError("oops".into())),
            ok_meter(),
        ]);
        meter_usage_entitlement_check(&client, "prod-test123", "Hours", zero_delay())
            .await
            .expect("recovers within the retry budget");
        assert_eq!(client.calls().len(), 3);
        assert!(client.calls().iter().all(|&(_, dry)| dry), "all DryRun");
    }

    #[tokio::test]
    async fn meter_entitlement_not_entitled_refuses_boot() {
        let client = ScriptedMeterClient::new(vec![Err(MeterUsageCallError::CustomerNotEntitled(
            "no sub".into(),
        ))]);
        let err = meter_usage_entitlement_check(&client, "prod-test123", "Hours", zero_delay())
            .await
            .expect_err("non-entitled customer must not boot");
        assert_eq!(client.calls().len(), 1, "fatal errors must not be retried");
        assert!(matches!(err, MarketplaceError::MeterUsageFatal { .. }));
        assert!(err.to_string().contains("refusing to start"));
    }

    #[tokio::test]
    async fn meter_entitlement_invalid_dimension_refuses_boot() {
        let client = ScriptedMeterClient::new(vec![Err(
            MeterUsageCallError::InvalidUsageDimension("bad dim".into()),
        )]);
        let err = meter_usage_entitlement_check(&client, "prod-test123", "Hours", zero_delay())
            .await
            .expect_err("a wrong dimension name must abort boot");
        assert!(matches!(err, MarketplaceError::MeterUsageFatal { .. }));
        assert!(err.to_string().contains("--marketplace-usage-dimension"));
    }

    #[tokio::test]
    async fn meter_entitlement_duplicate_is_treated_as_entitled() {
        let client = ScriptedMeterClient::new(vec![Err(MeterUsageCallError::DuplicateRequest(
            "already".into(),
        ))]);
        meter_usage_entitlement_check(&client, "prod-test123", "Hours", zero_delay())
            .await
            .expect("a duplicate still proves entitlement");
    }

    #[tokio::test]
    async fn meter_entitlement_retry_budget_exhaustion_refuses_boot() {
        let client = ScriptedMeterClient::new(vec![
            Err(MeterUsageCallError::Throttling("1".into())),
            Err(MeterUsageCallError::Throttling("2".into())),
            Err(MeterUsageCallError::Throttling("3".into())),
            Err(MeterUsageCallError::Throttling("4".into())),
        ]);
        let err = meter_usage_entitlement_check(&client, "prod-test123", "Hours", zero_delay())
            .await
            .expect_err("exhausted budget must refuse boot");
        assert_eq!(client.calls().len(), 4);
        match err {
            MarketplaceError::MeterUsageRetriesExhausted {
                dimension,
                attempts,
                ..
            } => {
                assert_eq!(dimension, "Hours");
                assert_eq!(attempts, 4);
            }
            other => panic!("expected MeterUsageRetriesExhausted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn meter_one_hour_sends_one_real_record() {
        let client = ScriptedMeterClient::new(vec![ok_meter()]);
        let outcome = meter_one_hour(
            &client,
            "prod-test123",
            "Hours",
            1,
            SystemTime::UNIX_EPOCH,
            far_timeout(),
        )
        .await;
        // The hourly loop sends a REAL (non-DryRun) record of quantity 1.
        assert_eq!(client.calls(), vec![(1, false)]);
        match outcome {
            MeterOutcome::Metered { record_id } => {
                assert_eq!(record_id.as_deref(), Some("rec-abc123"));
            }
            other => panic!("expected Metered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn meter_one_hour_duplicate_is_already_metered() {
        let client = ScriptedMeterClient::new(vec![Err(MeterUsageCallError::DuplicateRequest(
            "this hour".into(),
        ))]);
        let outcome = meter_one_hour(
            &client,
            "prod-test123",
            "Hours",
            1,
            SystemTime::UNIX_EPOCH,
            far_timeout(),
        )
        .await;
        assert!(matches!(outcome, MeterOutcome::AlreadyMetered));
    }

    #[tokio::test]
    async fn meter_one_hour_failure_is_fail_open() {
        // A transient failure must surface as Failed (the loop keeps serving)
        // rather than panicking or returning an Err the caller must handle.
        let client =
            ScriptedMeterClient::new(vec![Err(MeterUsageCallError::Throttling("later".into()))]);
        let outcome = meter_one_hour(
            &client,
            "prod-test123",
            "Hours",
            1,
            SystemTime::UNIX_EPOCH,
            far_timeout(),
        )
        .await;
        assert!(matches!(
            outcome,
            MeterOutcome::Failed(MeterUsageCallError::Throttling(_))
        ));
    }

    /// A `meter_usage` that never returns must surface as `Failed(Timeout)`
    /// (fail-open) rather than hang the hourly loop / block shutdown.
    struct HangingMeterClient;

    #[async_trait]
    impl MeterUsageClient for HangingMeterClient {
        async fn meter_usage(
            &self,
            _product_code: &str,
            _dimension: &str,
            _quantity: i32,
            _timestamp: SystemTime,
            _dry_run: bool,
        ) -> Result<MeterUsageResponse, MeterUsageCallError> {
            // Never readies within the test's (zero) timeout budget.
            tokio::time::sleep(Duration::from_secs(3600)).await;
            ok_meter()
        }
    }

    #[tokio::test]
    async fn meter_one_hour_times_out_fail_open() {
        // timeout = ZERO: the hung call is pending on first poll, so the
        // timeout fires immediately (the test does not actually wait an hour).
        let outcome = meter_one_hour(
            &HangingMeterClient,
            "prod-test123",
            "Hours",
            1,
            SystemTime::UNIX_EPOCH,
            Duration::ZERO,
        )
        .await;
        assert!(matches!(
            outcome,
            MeterOutcome::Failed(MeterUsageCallError::Timeout(_))
        ));
    }

    // ---- DryRun success sentinel + unmodeled-error classification --------

    #[test]
    fn dry_run_operation_code_is_success_only_for_dry_run() {
        // Permitted DryRun comes back as the `DryRunOperation` error code,
        // which must be read as success — but ONLY when we actually asked for
        // a DryRun (a real call returning that code would be nonsense).
        assert!(is_dry_run_success(true, Some("DryRunOperation")));
        assert!(!is_dry_run_success(false, Some("DryRunOperation")));
        assert!(!is_dry_run_success(true, Some("UnauthorizedException")));
        assert!(!is_dry_run_success(
            true,
            Some("CustomerNotEntitledException")
        ));
        assert!(!is_dry_run_success(true, None));
    }

    #[test]
    fn unmodeled_unauthorized_maps_to_fatal_iam_error() {
        // UnauthorizedException is the DryRun "no IAM permission" signal and
        // is NOT in the modeled MeterUsageError set — it must still classify
        // as a clear, fatal (non-retryable) error, not a generic Other.
        let unauthorized =
            classify_meter_usage_unmodeled(Some("UnauthorizedException"), "denied".into());
        assert!(matches!(unauthorized, MeterUsageCallError::Unauthorized(_)));
        assert!(!unauthorized.is_retryable());
        assert!(
            unauthorized
                .to_string()
                .contains("aws-marketplace:MeterUsage")
        );

        // Anything else (incl. a stray DryRunOperation reaching classify on a
        // non-DryRun path) is Other / fatal.
        assert!(matches!(
            classify_meter_usage_unmodeled(Some("DryRunOperation"), "x".into()),
            MeterUsageCallError::Other(_)
        ));
        assert!(matches!(
            classify_meter_usage_unmodeled(None, "x".into()),
            MeterUsageCallError::Other(_)
        ));
    }

    #[tokio::test]
    async fn meter_entitlement_unauthorized_refuses_boot() {
        let client = ScriptedMeterClient::new(vec![Err(MeterUsageCallError::Unauthorized(
            "no perm".into(),
        ))]);
        let err = meter_usage_entitlement_check(&client, "prod-test123", "Hours", zero_delay())
            .await
            .expect_err("missing IAM permission must abort boot");
        assert_eq!(client.calls().len(), 1, "Unauthorized is not retryable");
        assert!(matches!(err, MarketplaceError::MeterUsageFatal { .. }));
    }

    // ---- backfill staleness window --------------------------------------

    #[test]
    fn drop_stale_pending_drops_only_too_old_front_entries() {
        let base = SystemTime::UNIX_EPOCH;
        let hour = Duration::from_secs(3600);
        let mut pending: VecDeque<SystemTime> = VecDeque::new();
        // Hours at t=0,1,2,3,4,5,6,7 relative to base.
        for h in 0..8u32 {
            pending.push_back(base + hour * h);
        }
        // "now" = t=7h. With MAX_METER_BACKLOG ~5h50m, entries at 0h and 1h
        // (ages 7h, 6h) are stale; 2h (age 5h) is within the window.
        let now = base + hour * 7;
        let dropped = drop_stale_pending(&mut pending, now, MAX_METER_BACKLOG);
        assert_eq!(dropped, 2, "the 7h-old and 6h-old hours must be dropped");
        assert_eq!(
            *pending.front().expect("queue not empty"),
            base + hour * 2,
            "oldest surviving entry is the 5h-old hour"
        );
        assert_eq!(pending.len(), 6);
    }

    #[test]
    fn drop_stale_pending_keeps_future_and_recent_entries() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 3600);
        let mut pending: VecDeque<SystemTime> = VecDeque::new();
        pending.push_back(base); // exactly now
        pending.push_back(base + Duration::from_secs(60)); // 1 min in the future (skew)
        let dropped = drop_stale_pending(&mut pending, base, MAX_METER_BACKLOG);
        assert_eq!(dropped, 0, "nothing recent/future may be dropped");
        assert_eq!(pending.len(), 2);
    }
}

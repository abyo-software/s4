use super::S3Auth;

use crate::auth::SecretKey;
use crate::error::S3Result;

use std::collections::HashMap;

/// A simple authentication provider
#[derive(Debug, Default)]
pub struct SimpleAuth {
    /// key map
    map: HashMap<String, SecretKey>,
}

impl SimpleAuth {
    /// Constructs a new `SimpleAuth`
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    #[must_use]
    pub fn from_single(access_key: impl Into<String>, secret_key: impl Into<SecretKey>) -> Self {
        let access_key = access_key.into();
        let secret_key = secret_key.into();
        let map = [(access_key, secret_key)].into_iter().collect();
        Self { map }
    }

    /// register a pair of keys
    pub fn register(&mut self, access_key: String, secret_key: SecretKey) -> Option<SecretKey> {
        self.map.insert(access_key, secret_key)
    }

    /// lookup a secret key
    #[must_use]
    pub fn lookup(&self, access_key: &str) -> Option<&SecretKey> {
        self.map.get(access_key)
    }
}

#[async_trait::async_trait]
impl S3Auth for SimpleAuth {
    async fn get_secret_key(&self, access_key: &str) -> S3Result<SecretKey> {
        match self.lookup(access_key) {
            None => Err(s3_error!(NotSignedUp, "Your account is not signed up")),
            Some(s) => Ok(s.clone()),
        }
    }
}

/// v0.13.0-s4.1: An [`S3Auth`] provider that returns a dummy secret for every
/// access key.
///
/// Designed for a trusted-network S3 compatibility gateway that pairs this
/// provider with
/// [`S3ServiceBuilder::set_skip_signature_verification`](crate::service::S3ServiceBuilder::set_skip_signature_verification).
/// The returned secret is never compared against the client signature when
/// NoVerify mode is on, so its value is irrelevant — but it MUST be returned
/// to keep the signature-context plumbing happy (streaming-body decoding etc.
/// expects an `Ok` secret).
///
/// **Never combine with the default access-control without enabling NoVerify
/// mode — the gateway would happily accept any forged signature.** Either
/// pair with `set_skip_signature_verification(true)` (gateway re-signs to
/// the real backend with its own credentials) or with a real
/// [`S3Access`](crate::access::S3Access) policy.
#[derive(Debug, Default)]
pub struct AcceptAnyAuth;

impl AcceptAnyAuth {
    /// Constructs a new `AcceptAnyAuth`
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl S3Auth for AcceptAnyAuth {
    async fn get_secret_key(&self, _access_key: &str) -> S3Result<SecretKey> {
        Ok(SecretKey::from(
            "s4-no-verify-mode-fake-secret-do-not-trust",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_empty() {
        let auth = SimpleAuth::new();
        assert!(auth.lookup("anything").is_none());
    }

    #[test]
    fn from_single() {
        let auth = SimpleAuth::from_single("AKID", "secret");
        assert!(auth.lookup("AKID").is_some());
        assert_eq!(auth.lookup("AKID").unwrap().expose(), "secret");
        assert!(auth.lookup("other").is_none());
    }

    #[test]
    fn register_and_lookup() {
        let mut auth = SimpleAuth::new();
        let prev = auth.register("key1".to_owned(), SecretKey::from("sec1"));
        assert!(prev.is_none());
        assert_eq!(auth.lookup("key1").unwrap().expose(), "sec1");
    }

    #[test]
    fn register_replaces() {
        let mut auth = SimpleAuth::from_single("key", "old");
        let prev = auth.register("key".to_owned(), SecretKey::from("new"));
        assert!(prev.is_some());
        assert_eq!(auth.lookup("key").unwrap().expose(), "new");
    }

    #[test]
    fn default_is_empty() {
        let auth = SimpleAuth::default();
        assert!(auth.lookup("x").is_none());
    }

    #[test]
    fn debug_impl() {
        let auth = SimpleAuth::from_single("AKID", "secret");
        let debug = format!("{auth:?}");
        assert!(debug.contains("SimpleAuth"));
    }

    #[tokio::test]
    async fn get_secret_key_found() {
        let auth = SimpleAuth::from_single("AKID", "secret");
        let result = auth.get_secret_key("AKID").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().expose(), "secret");
    }

    #[tokio::test]
    async fn get_secret_key_not_found() {
        let auth = SimpleAuth::from_single("AKID", "secret");
        let result = auth.get_secret_key("UNKNOWN").await;
        assert!(result.is_err());
    }

    // v0.13.0-s4.1 NoVerify mode regression tests.
    //
    // The v1.4.0 Marketplace AMI E2E found the gateway rejecting every
    // external client because `SimpleAuth::from_single` only matched one
    // AKID (the gateway's IRSA/instance-role STS access key). NoVerify
    // mode + `AcceptAnyAuth` is the v1.4.1 fix.
    #[tokio::test]
    async fn accept_any_auth_returns_ok_for_any_key() {
        let auth = AcceptAnyAuth::new();
        for akid in [
            "AKIAIOSFODNN7EXAMPLE",
            "ASIA01234567890ABCDE",
            "GOOG1XYZ",
            "",
            "AKID-with-strange-bytes!@#",
        ] {
            let r = auth.get_secret_key(akid).await;
            assert!(r.is_ok(), "AcceptAnyAuth must accept {akid:?}");
            // Returned secret is a fixed marker, never a real credential.
            assert_eq!(
                r.unwrap().expose(),
                "s4-no-verify-mode-fake-secret-do-not-trust",
                "fake-secret marker is required so an operator grep can prove the gateway is in NoVerify mode"
            );
        }
    }

    #[tokio::test]
    async fn accept_any_auth_default_matches_new() {
        let a = AcceptAnyAuth::new();
        let b = AcceptAnyAuth;
        assert_eq!(
            a.get_secret_key("X").await.unwrap().expose(),
            b.get_secret_key("Y").await.unwrap().expose()
        );
    }
}

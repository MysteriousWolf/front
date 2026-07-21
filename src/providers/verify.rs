//! Candidate API key verification: a shared, network-free classifier used by
//! the per-provider probe (`EumetnetProvider::verify_api_key`).
//!
//! Keeping the classification itself pure — a status code in, an outcome out
//! — is what makes it unit-testable without a live network. The async probe
//! on each provider is just: build an authed request with the *candidate*
//! key → send → feed the resulting status (or `None` on network error /
//! timeout) into [`classify_verify_status`].

/// Outcome of checking a candidate API key against its live service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Service accepted the key (2xx to an authed request).
    Valid,
    /// Service rejected the key (HTTP 401 or 403).
    Invalid,
    /// Network error, timeout, or any other status (including 5xx and 429)
    /// — cannot confirm either way. A rate-limited response must land here,
    /// never `Invalid`: a valid key that got throttled is not a bad key.
    Unreachable,
}

/// Classify an HTTP status code (`None` = network error/timeout) into a
/// [`VerifyOutcome`]. Pure — no I/O — so it is testable without a live
/// network.
pub fn classify_verify_status(status: Option<u16>) -> VerifyOutcome {
    match status {
        Some(code) if (200..300).contains(&code) => VerifyOutcome::Valid,
        Some(401 | 403) => VerifyOutcome::Invalid,
        _ => VerifyOutcome::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_status_is_valid() {
        assert_eq!(classify_verify_status(Some(200)), VerifyOutcome::Valid);
        assert_eq!(classify_verify_status(Some(204)), VerifyOutcome::Valid);
    }

    #[test]
    fn unauthorized_and_forbidden_are_invalid() {
        assert_eq!(classify_verify_status(Some(401)), VerifyOutcome::Invalid);
        assert_eq!(classify_verify_status(Some(403)), VerifyOutcome::Invalid);
    }

    #[test]
    fn network_error_is_unreachable() {
        assert_eq!(classify_verify_status(None), VerifyOutcome::Unreachable);
    }

    #[test]
    fn server_error_is_unreachable() {
        assert_eq!(
            classify_verify_status(Some(500)),
            VerifyOutcome::Unreachable
        );
    }

    /// Rate limiting must never be mistaken for a bad key (spec Risks row 5):
    /// a valid key that got throttled must not be reported as `Invalid`.
    #[test]
    fn rate_limited_is_unreachable_not_invalid() {
        assert_eq!(
            classify_verify_status(Some(429)),
            VerifyOutcome::Unreachable
        );
    }
}

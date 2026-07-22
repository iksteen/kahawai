//! Enrollment code derivation (SEC-2/SEC-3).
//!
//! The code commits to the CSR (and thus the satellite's public key): both the
//! satellite (printing it) and the hub (verifying the admin's input) derive it
//! from the CSR DER, so a substituted CSR can never match the printed code.

use data_encoding::BASE32_NOPAD;
use sha2::{Digest, Sha256};

/// `base32(SHA-256(csr_der))[0..8]`, formatted `XXXX-XXXX`.
pub fn enrollment_code(csr_der: &[u8]) -> String {
    let digest = Sha256::digest(csr_der);
    let b32 = BASE32_NOPAD.encode(&digest);
    format!("{}-{}", &b32[0..4], &b32[4..8])
}

/// Compare an admin-entered code against a CSR, tolerating case and a
/// missing/misplaced dash. Exact match on the normalized form only (SEC-3).
pub fn code_matches(entered: &str, csr_der: &[u8]) -> bool {
    let normalize = |s: &str| s.chars().filter(|c| *c != '-').collect::<String>().to_ascii_uppercase();
    normalize(entered) == normalize(&enrollment_code(csr_der))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_stable_and_formatted() {
        let code = enrollment_code(b"some-csr-der");
        assert_eq!(code.len(), 9);
        assert_eq!(code.as_bytes()[4], b'-');
        assert_eq!(code, enrollment_code(b"some-csr-der"));
    }

    #[test]
    fn different_csr_different_code() {
        assert_ne!(enrollment_code(b"csr-a"), enrollment_code(b"csr-b"));
    }

    #[test]
    fn matching_tolerates_case_and_dash() {
        let code = enrollment_code(b"csr");
        assert!(code_matches(&code, b"csr"));
        assert!(code_matches(&code.to_lowercase(), b"csr"));
        assert!(code_matches(&code.replace('-', ""), b"csr"));
        assert!(!code_matches(&code, b"other-csr"));
    }
}

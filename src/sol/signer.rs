use std::fmt;

use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignerError {
    // Deliberately carries no detail: the parser's message could echo key material.
    #[error(
        "SOL_PRIVATE_KEY is not a valid Solana keypair (expected base58 of 64 bytes, or a \
         solana-keygen JSON array)"
    )]
    InvalidPrivateKey,
}

/// Bot wallet keypair. Debug only ever exposes the public key; signing goes through
/// `keypair()`.
pub struct SolSigner {
    keypair: Keypair,
}

impl SolSigner {
    /// Accepts the two common export formats: base58 of the 64-byte keypair (Phantom,
    /// Solflare) or the `solana-keygen` JSON array `[12,34,...]`. The 64 bytes must be a
    /// consistent secret+public pair.
    pub fn from_secret(raw: &str) -> Result<Self, SignerError> {
        let raw = raw.trim();
        let bytes: Vec<u8> = if raw.starts_with('[') {
            serde_json::from_str(raw).map_err(|_| SignerError::InvalidPrivateKey)?
        } else {
            bs58::decode(raw)
                .into_vec()
                .map_err(|_| SignerError::InvalidPrivateKey)?
        };
        let keypair =
            Keypair::try_from(bytes.as_slice()).map_err(|_| SignerError::InvalidPrivateKey)?;
        Ok(Self { keypair })
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }
}

impl fmt::Debug for SolSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SolSigner")
            .field("pubkey", &self.pubkey())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_base58_and_json_and_hides_secret() {
        let kp = Keypair::new();
        let b58 = kp.to_base58_string();
        let s = SolSigner::from_secret(&b58).unwrap();
        assert_eq!(s.pubkey(), kp.pubkey());
        assert!(!format!("{s:?}").contains(&b58[..20]));

        let json = serde_json::to_string(&kp.to_bytes().to_vec()).unwrap();
        assert_eq!(SolSigner::from_secret(&json).unwrap().pubkey(), kp.pubkey());
    }

    #[test]
    fn rejects_garbage_and_inconsistent_pairs_without_leaking() {
        let mut bytes = Keypair::new().to_bytes();
        bytes[40] ^= 1; // public half no longer matches the secret
        let bad = bs58::encode(bytes).into_string();
        for raw in [bad.as_str(), "", "0OIl", "[1,2,3]", "deadbeef"] {
            let err = SolSigner::from_secret(raw).unwrap_err();
            assert_eq!(err, SignerError::InvalidPrivateKey);
            assert!(raw.is_empty() || !err.to_string().contains(raw));
        }
    }
}

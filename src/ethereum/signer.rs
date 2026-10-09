use std::fmt;

use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignerError {
    // Deliberately carries no detail: the parser's message could echo key material.
    #[error("PRIVATE_KEY is not a valid secp256k1 private key (expected 32-byte hex)")]
    InvalidPrivateKey,
}

/// Bot wallet signer. Debug/Display only ever expose the address, never the key.
#[derive(Clone)]
pub struct BotSigner {
    inner: PrivateKeySigner,
}

impl BotSigner {
    pub fn from_hex(raw: &str) -> Result<Self, SignerError> {
        let raw = raw.trim();
        let hex = raw.strip_prefix("0x").unwrap_or(raw);
        if hex.len() != 64 {
            return Err(SignerError::InvalidPrivateKey);
        }
        let inner: PrivateKeySigner = hex.parse().map_err(|_| SignerError::InvalidPrivateKey)?;
        Ok(Self { inner })
    }

    pub fn address(&self) -> Address {
        self.inner.address()
    }

    /// Signing handle for tx building; the key itself never leaves this type.
    pub fn wallet(&self) -> EthereumWallet {
        EthereumWallet::from(self.inner.clone())
    }
}

impl fmt::Debug for BotSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BotSigner")
            .field("address", &self.address())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Well-known Anvil/Hardhat dev account #0 — public test key, holds no real funds.
    const ANVIL_KEY_0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const ANVIL_KEY_0_HEX: &str =
        "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn parses_key_and_derives_address() {
        let signer = BotSigner::from_hex(ANVIL_KEY_0).unwrap();
        let expected: Address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            .parse()
            .unwrap();
        assert_eq!(signer.address(), expected);
    }

    #[test]
    fn accepts_key_without_prefix() {
        assert!(BotSigner::from_hex(ANVIL_KEY_0_HEX).is_ok());
    }

    #[test]
    fn rejects_invalid_keys() {
        for bad in [
            "",
            "0x",
            "0x1234",
            "not-hex",
            &format!("0x{}", "0".repeat(64)),
        ] {
            assert_eq!(
                BotSigner::from_hex(bad).unwrap_err(),
                SignerError::InvalidPrivateKey,
                "input: {bad}"
            );
        }
    }

    #[test]
    fn debug_never_contains_key() {
        let signer = BotSigner::from_hex(ANVIL_KEY_0).unwrap();
        let dbg = format!("{signer:?}");
        assert!(!dbg.to_lowercase().contains(ANVIL_KEY_0_HEX));
        assert!(dbg.contains("address"));
    }
}

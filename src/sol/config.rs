//! `sol-copy-trade` configuration. Every variable is prefixed `SOL_` so one `.env` can serve
//! both binaries without one chain's settings (above all `SOL_DRY_RUN`) leaking into the
//! other. Only `TELEGRAM_*` and `LOG_LEVEL` are shared.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use solana_pubkey::Pubkey;

use crate::common::env::{
    BPS_DENOMINATOR, EnvError, check_scheme, invalid, parse_bool, parse_positive_usize,
    parse_ratio_bps, telegram_from,
};
use crate::common::telegram::TelegramConfig;
use crate::sol::models::VenueKind;
use crate::sol::signer::{SignerError, SolSigner};

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

#[derive(Debug, thiserror::Error)]
pub enum SolConfigError {
    #[error(transparent)]
    Env(#[from] EnvError),
    #[error(transparent)]
    Signer(#[from] SignerError),
}

/// BUY sizing. SELL always sells `SOL_SELL_RATIO` of the bot's own balance (the bot's
/// position is unrelated to the source's), same as the ETH bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolStrategy {
    /// Spend a fixed amount of lamports per copied buy.
    FixedSol { lamports: u64 },
    /// Spend `ratio` of what the source spent.
    Ratio { bps: u32 },
}

/// Holds a secret (e.g. the gRPC `x-token`): `Debug` never prints it.
#[derive(Clone)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug)]
pub struct SolConfig {
    /// JSON-RPC (HTTP): account reads, simulation, sending.
    pub rpc_url: String,
    /// Yellowstone (Geyser) gRPC endpoint.
    pub grpc_url: String,
    pub grpc_x_token: Option<Secret>,
    pub signer: SolSigner,
    pub copy_wallets: Vec<Pubkey>,
    pub strategy: SolStrategy,
    pub sell_ratio_bps: u32,
    pub max_trade_lamports: u64,
    pub max_slippage_bps: u32,
    /// Compute-unit price (priority fee), in micro-lamports per CU.
    pub priority_fee_micro_lamports: u64,
    /// SOL always kept for tx fees and account rent (ATA, volume accumulator).
    pub fee_reserve_lamports: u64,
    pub dry_run: bool,
    pub venues: Vec<VenueKind>,
    pub token_blacklist: Vec<Pubkey>,
    pub trade_cooldown: Duration,
    pub channel_capacity: usize,
    pub executor_workers: usize,
    pub telegram: Option<TelegramConfig>,
}

impl SolConfig {
    pub fn from_env() -> Result<Self, SolConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// `lookup` is injected so tests never mutate process-global env.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, SolConfigError> {
        let get = |key: &'static str| -> Option<String> {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let required = |key: &'static str| get(key).ok_or(EnvError::Missing(key));
        let or_default = |key: &'static str, default: &str| get(key).unwrap_or(default.into());

        let rpc_url = required("SOL_RPC_URL")?;
        check_scheme("SOL_RPC_URL", &rpc_url, &["http://", "https://"])?;
        let grpc_url = required("SOL_GRPC_URL")?;
        check_scheme("SOL_GRPC_URL", &grpc_url, &["http://", "https://"])?;
        let grpc_x_token = get("SOL_GRPC_X_TOKEN").map(Secret);

        // The raw key String is dropped at the end of this statement.
        let signer = SolSigner::from_secret(&required("SOL_PRIVATE_KEY")?)?;

        let copy_wallets = parse_pubkey_list("SOL_COPY_WALLETS", &required("SOL_COPY_WALLETS")?)?;
        if copy_wallets.is_empty() {
            return Err(invalid("SOL_COPY_WALLETS", "must contain at least one address").into());
        }
        if copy_wallets.contains(&signer.pubkey()) {
            return Err(invalid(
                "SOL_COPY_WALLETS",
                "contains the bot's own address (would copy its own trades in a loop)",
            )
            .into());
        }

        let max_trade_lamports =
            parse_sol("SOL_MAX_TRADE_SOL", &or_default("SOL_MAX_TRADE_SOL", "1"))?;
        let strategy = match or_default("SOL_COPY_STRATEGY", "FIXED_SOL")
            .to_ascii_uppercase()
            .as_str()
        {
            "FIXED_SOL" => {
                let lamports = parse_sol(
                    "SOL_FIXED_TRADE_SOL",
                    &or_default("SOL_FIXED_TRADE_SOL", "0.1"),
                )?;
                if lamports > max_trade_lamports {
                    return Err(
                        invalid("SOL_FIXED_TRADE_SOL", "must be <= SOL_MAX_TRADE_SOL").into(),
                    );
                }
                SolStrategy::FixedSol { lamports }
            }
            "RATIO" => SolStrategy::Ratio {
                bps: parse_ratio_bps("SOL_BUY_RATIO", &or_default("SOL_BUY_RATIO", "0.10"))?,
            },
            other => {
                return Err(invalid(
                    "SOL_COPY_STRATEGY",
                    format!("expected `FIXED_SOL` or `RATIO`, got `{other}`"),
                )
                .into());
            }
        };
        let sell_ratio_bps =
            parse_ratio_bps("SOL_SELL_RATIO", &or_default("SOL_SELL_RATIO", "1.0"))?;
        if sell_ratio_bps > BPS_DENOMINATOR {
            return Err(invalid(
                "SOL_SELL_RATIO",
                "must be <= 1.0 (cannot sell more than balance)",
            )
            .into());
        }

        let max_slippage_bps = or_default("SOL_MAX_SLIPPAGE_BPS", "500")
            .parse::<u32>()
            .map_err(|e| invalid("SOL_MAX_SLIPPAGE_BPS", e.to_string()))?;
        if max_slippage_bps == 0 || max_slippage_bps >= BPS_DENOMINATOR {
            return Err(invalid("SOL_MAX_SLIPPAGE_BPS", "must be in 1..10000").into());
        }
        let priority_fee_micro_lamports = or_default("SOL_PRIORITY_FEE_MICROLAMPORTS", "200000")
            .parse::<u64>()
            .map_err(|e| invalid("SOL_PRIORITY_FEE_MICROLAMPORTS", e.to_string()))?;
        let fee_reserve_lamports = parse_sol_allow_zero(
            "SOL_FEE_RESERVE_SOL",
            &or_default("SOL_FEE_RESERVE_SOL", "0.01"),
        )?;

        let dry_run = parse_bool("SOL_DRY_RUN", &or_default("SOL_DRY_RUN", "true"))?;

        let venues = parse_venues(&or_default("SOL_VENUES", "PUMP_FUN,PUMP_SWAP"))?;
        let token_blacklist = match get("SOL_TOKEN_BLACKLIST") {
            Some(raw) => parse_pubkey_list("SOL_TOKEN_BLACKLIST", &raw)?,
            None => Vec::new(),
        };
        let trade_cooldown = Duration::from_secs(
            or_default("SOL_TRADE_COOLDOWN_SECS", "30")
                .parse::<u64>()
                .map_err(|e| invalid("SOL_TRADE_COOLDOWN_SECS", e.to_string()))?,
        );
        let channel_capacity = parse_positive_usize(
            "SOL_CHANNEL_CAPACITY",
            &or_default("SOL_CHANNEL_CAPACITY", "1024"),
        )?;
        let executor_workers = parse_positive_usize(
            "SOL_EXECUTOR_WORKERS",
            &or_default("SOL_EXECUTOR_WORKERS", "4"),
        )?;
        let telegram = telegram_from(get)?;

        Ok(Self {
            rpc_url,
            grpc_url,
            grpc_x_token,
            signer,
            copy_wallets,
            strategy,
            sell_ratio_bps,
            max_trade_lamports,
            max_slippage_bps,
            priority_fee_micro_lamports,
            fee_reserve_lamports,
            dry_run,
            venues,
            token_blacklist,
            trade_cooldown,
            channel_capacity,
            executor_workers,
            telegram,
        })
    }
}

/// Comma-separated base58 pubkeys. Duplicates are collapsed (first occurrence wins).
pub fn parse_pubkey_list(key: &'static str, raw: &str) -> Result<Vec<Pubkey>, EnvError> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (idx, part) in raw.split(',').map(str::trim).enumerate() {
        if part.is_empty() {
            continue;
        }
        let pk = Pubkey::from_str(part)
            .map_err(|_| invalid(key, format!("entry #{idx} `{part}`: not a base58 pubkey")))?;
        if pk == Pubkey::default() {
            return Err(invalid(
                key,
                format!("entry #{idx}: default pubkey is not allowed"),
            ));
        }
        if seen.insert(pk) {
            out.push(pk);
        }
    }
    Ok(out)
}

/// Decimal SOL (up to 9 decimals) -> lamports, exact. Must be > 0.
pub fn parse_sol(key: &'static str, raw: &str) -> Result<u64, EnvError> {
    let lamports = parse_sol_allow_zero(key, raw)?;
    if lamports == 0 {
        return Err(invalid(key, "must be > 0"));
    }
    Ok(lamports)
}

fn parse_sol_allow_zero(key: &'static str, raw: &str) -> Result<u64, EnvError> {
    let (int_part, frac_part) = raw.split_once('.').unwrap_or((raw, ""));
    if int_part.is_empty() && frac_part.is_empty()
        || !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(invalid(
            key,
            format!("`{raw}` is not a non-negative decimal"),
        ));
    }
    if frac_part.len() > 9 {
        return Err(invalid(key, "at most 9 decimal places (1 lamport)"));
    }
    let int: u64 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse()
            .map_err(|_| invalid(key, format!("`{raw}` is out of range")))?
    };
    let frac: u64 = format!("{frac_part:0<9}")
        .parse()
        .map_err(|_| invalid(key, format!("`{raw}` is not a decimal")))?;
    int.checked_mul(LAMPORTS_PER_SOL)
        .and_then(|v| v.checked_add(frac))
        .ok_or_else(|| invalid(key, format!("`{raw}` is out of range")))
}

fn parse_venues(raw: &str) -> Result<Vec<VenueKind>, EnvError> {
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let v: VenueKind = part.parse().map_err(|e| invalid("SOL_VENUES", e))?;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    if out.is_empty() {
        return Err(invalid("SOL_VENUES", "must contain at least one venue"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use solana_keypair::Keypair;
    use solana_signer::Signer;

    use super::*;

    const W1: &str = "omegoMAe1AMY5MFKQQr3JwXVy8F4eCvmBAfcpo8XAfq";
    const W2: &str = "CvVX4fgTZiapGy5vcGgFBLY8kxY37Uahnwg3tjpJU9i4";

    fn base_env(key: &Keypair) -> HashMap<&'static str, String> {
        HashMap::from([
            ("SOL_RPC_URL", "https://rpc.example/KEY".to_string()),
            ("SOL_GRPC_URL", "https://grpc.example:443".to_string()),
            ("SOL_PRIVATE_KEY", key.to_base58_string()),
            ("SOL_COPY_WALLETS", format!("{W1},{W2}")),
        ])
    }

    fn load(env: &HashMap<&'static str, String>) -> Result<SolConfig, SolConfigError> {
        SolConfig::from_lookup(|k| env.get(k).cloned())
    }

    fn assert_invalid(env: &HashMap<&'static str, String>, expected: &str) {
        match load(env) {
            Err(SolConfigError::Env(EnvError::Invalid { key, .. })) => assert_eq!(key, expected),
            other => panic!("expected Invalid({expected}), got {other:?}"),
        }
    }

    #[test]
    fn loads_with_safe_defaults() {
        let kp = Keypair::new();
        let cfg = load(&base_env(&kp)).unwrap();
        assert!(cfg.dry_run, "SOL_DRY_RUN must default to true");
        assert_eq!(cfg.signer.pubkey(), kp.pubkey());
        assert_eq!(cfg.copy_wallets.len(), 2);
        assert_eq!(
            cfg.strategy,
            SolStrategy::FixedSol {
                lamports: LAMPORTS_PER_SOL / 10
            }
        );
        assert_eq!(cfg.max_trade_lamports, LAMPORTS_PER_SOL);
        assert_eq!(cfg.sell_ratio_bps, 10_000);
        assert_eq!(cfg.max_slippage_bps, 500);
        assert_eq!(cfg.fee_reserve_lamports, 10_000_000);
        assert_eq!(cfg.venues, vec![VenueKind::PumpFun, VenueKind::PumpSwap]);
        assert!(cfg.grpc_x_token.is_none() && cfg.telegram.is_none());
    }

    #[test]
    fn eth_dry_run_does_not_affect_sol() {
        let kp = Keypair::new();
        let mut env = base_env(&kp);
        env.insert("DRY_RUN", "false".into());
        assert!(load(&env).unwrap().dry_run);
        env.insert("SOL_DRY_RUN", "flase".into());
        assert_invalid(&env, "SOL_DRY_RUN");
        env.insert("SOL_DRY_RUN", "false".into());
        assert!(!load(&env).unwrap().dry_run);
    }

    #[test]
    fn debug_hides_private_key_and_grpc_token() {
        let kp = Keypair::new();
        let mut env = base_env(&kp);
        env.insert("SOL_GRPC_X_TOKEN", "SECRET-TOKEN".into());
        let dbg = format!("{:?}", load(&env).unwrap());
        assert!(!dbg.contains("SECRET") && !dbg.contains(&kp.to_base58_string()[..16]));
    }

    #[test]
    fn rejects_bot_in_copy_wallets_and_bad_entries() {
        let kp = Keypair::new();
        let mut env = base_env(&kp);
        env.insert("SOL_COPY_WALLETS", format!("{W1},{}", kp.pubkey()));
        assert_invalid(&env, "SOL_COPY_WALLETS");
        for bad in ["0x1111", "11111111111111111111111111111111", " , "] {
            env.insert("SOL_COPY_WALLETS", bad.into());
            assert_invalid(&env, "SOL_COPY_WALLETS");
        }
        for key in [
            "SOL_RPC_URL",
            "SOL_GRPC_URL",
            "SOL_PRIVATE_KEY",
            "SOL_COPY_WALLETS",
        ] {
            let mut env = base_env(&kp);
            env.remove(key);
            assert!(
                matches!(load(&env), Err(SolConfigError::Env(EnvError::Missing(k))) if k == key)
            );
        }
    }

    #[test]
    fn strategy_and_amount_validation() {
        let kp = Keypair::new();
        let mut env = base_env(&kp);
        env.insert("SOL_COPY_STRATEGY", "ratio".into());
        env.insert("SOL_BUY_RATIO", "0.25".into());
        assert_eq!(
            load(&env).unwrap().strategy,
            SolStrategy::Ratio { bps: 2_500 }
        );

        let mut env = base_env(&kp);
        env.insert("SOL_FIXED_TRADE_SOL", "2".into());
        assert_invalid(&env, "SOL_FIXED_TRADE_SOL");
        for (key, bad) in [
            ("SOL_COPY_STRATEGY", "FIXED_ETH"),
            ("SOL_SELL_RATIO", "1.5"),
            ("SOL_MAX_SLIPPAGE_BPS", "0"),
            ("SOL_MAX_TRADE_SOL", "0"),
            ("SOL_VENUES", "RAYDIUM"),
            ("SOL_GRPC_URL", "wss://grpc.example"),
        ] {
            let mut env = base_env(&kp);
            env.insert(key, bad.into());
            assert_invalid(&env, key);
        }
    }

    #[test]
    fn sol_amounts_are_exact() {
        assert_eq!(parse_sol("K", "0.1"), Ok(100_000_000));
        assert_eq!(parse_sol("K", "1.000000001"), Ok(1_000_000_001));
        assert_eq!(parse_sol("K", ".5"), Ok(500_000_000));
        for bad in ["0", "0.0000000001", "-1", "1e9", "."] {
            assert!(parse_sol("K", bad).is_err(), "should reject `{bad}`");
        }
        assert_eq!(parse_sol_allow_zero("K", "0"), Ok(0));
    }
}

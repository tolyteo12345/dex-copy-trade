use std::collections::HashSet;
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::utils::{parse_ether, parse_units};
use alloy::primitives::{Address, U256};

use crate::ethereum::signer::{BotSigner, SignerError};
use crate::models::Dex;
use crate::notify::TelegramConfig;

pub const BPS_DENOMINATOR: u32 = 10_000;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required env var {0}")]
    Missing(&'static str),
    #[error("invalid value for {key}: {reason}")]
    Invalid { key: &'static str, reason: String },
    #[error(transparent)]
    Signer(#[from] SignerError),
}

fn invalid(key: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key,
        reason: reason.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    Pending,
    Confirmed,
}

impl FromStr for CopyMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pending" => Ok(CopyMode::Pending),
            "confirmed" => Ok(CopyMode::Confirmed),
            other => Err(format!("expected `pending` or `confirmed`, got `{other}`")),
        }
    }
}

/// What to do when base fee + tip exceeds MAX_GAS_GWEI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GasCapMode {
    Skip,
    /// Re-check every block up to `max_wait`, then skip.
    Wait {
        max_wait: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyKind {
    FixedEth,
    FixedUsd,
    Ratio,
    Proportional,
}

impl FromStr for StrategyKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "FIXED_ETH" => Ok(StrategyKind::FixedEth),
            "FIXED_USD" => Ok(StrategyKind::FixedUsd),
            "RATIO" => Ok(StrategyKind::Ratio),
            "PROPORTIONAL" => Ok(StrategyKind::Proportional),
            other => Err(format!("unknown strategy `{other}`")),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    pub chain_id: u64,
    pub ws_url: String,
    pub http_url: String,
    pub signer: BotSigner,
    pub copy_wallets: Vec<Address>,
    pub copy_mode: CopyMode,
    pub strategy: StrategyKind,
    pub fixed_trade_wei: U256,
    /// USD with 6 decimals (FIXED_USD strategy).
    pub fixed_trade_usd_micro: U256,
    pub buy_ratio_bps: u32,
    pub sell_ratio_bps: u32,
    pub max_trade_wei: U256,
    pub max_slippage_bps: u32,
    pub max_gas_wei: u128,
    pub gas_cap_mode: GasCapMode,
    pub dry_run: bool,
    pub supported_dexes: Vec<Dex>,
    pub token_blacklist: Vec<Address>,
    /// Min time between two copies of the same (token, side). Zero disables.
    pub trade_cooldown: Duration,
    pub channel_capacity: usize,
    pub executor_workers: usize,
    /// `None` = notifications off (both TELEGRAM_* vars unset).
    pub telegram: Option<TelegramConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// `lookup` is injected so tests never mutate process-global env.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |key: &'static str| -> Option<String> {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let required = |key: &'static str| get(key).ok_or(ConfigError::Missing(key));
        let or_default = |key: &'static str, default: &str| get(key).unwrap_or(default.into());

        let chain_id = or_default("CHAIN_ID", "1")
            .parse::<u64>()
            .map_err(|e| invalid("CHAIN_ID", e.to_string()))?;

        let ws_url = required("ETH_WS_URL")?;
        check_scheme("ETH_WS_URL", &ws_url, &["ws://", "wss://"])?;
        let http_url = required("ETH_HTTP_URL")?;
        check_scheme("ETH_HTTP_URL", &http_url, &["http://", "https://"])?;

        // The raw key String is dropped at the end of this statement.
        let signer = BotSigner::from_hex(&required("PRIVATE_KEY")?)?;

        let copy_wallets = parse_wallets(&required("COPY_WALLETS")?)?;
        if copy_wallets.contains(&signer.address()) {
            return Err(invalid(
                "COPY_WALLETS",
                "contains the bot's own address (would copy its own trades in a loop)",
            ));
        }

        let copy_mode = or_default("COPY_MODE", "pending")
            .parse()
            .map_err(|e| invalid("COPY_MODE", e))?;
        let strategy = or_default("COPY_STRATEGY", "FIXED_ETH")
            .parse()
            .map_err(|e| invalid("COPY_STRATEGY", e))?;

        let fixed_trade_wei = parse_eth("FIXED_TRADE_ETH", &or_default("FIXED_TRADE_ETH", "0.05"))?;
        let max_trade_wei = parse_eth("MAX_TRADE_ETH", &or_default("MAX_TRADE_ETH", "0.5"))?;
        if fixed_trade_wei.is_zero() {
            return Err(invalid("FIXED_TRADE_ETH", "must be > 0"));
        }
        if max_trade_wei.is_zero() {
            return Err(invalid("MAX_TRADE_ETH", "must be > 0"));
        }
        if fixed_trade_wei > max_trade_wei {
            return Err(invalid("FIXED_TRADE_ETH", "must be <= MAX_TRADE_ETH"));
        }

        let fixed_trade_usd_micro: U256 = parse_units(&or_default("FIXED_TRADE_USD", "100"), 6)
            .map_err(|e| invalid("FIXED_TRADE_USD", e.to_string()))?
            .into();
        if fixed_trade_usd_micro.is_zero() {
            return Err(invalid("FIXED_TRADE_USD", "must be > 0"));
        }

        let buy_ratio_bps = parse_ratio_bps("BUY_RATIO", &or_default("BUY_RATIO", "0.10"))?;
        let sell_ratio_bps = parse_ratio_bps("SELL_RATIO", &or_default("SELL_RATIO", "1.0"))?;
        if sell_ratio_bps > BPS_DENOMINATOR {
            return Err(invalid(
                "SELL_RATIO",
                "must be <= 1.0 (cannot sell more than balance)",
            ));
        }

        let max_slippage_bps = or_default("MAX_SLIPPAGE_BPS", "300")
            .parse::<u32>()
            .map_err(|e| invalid("MAX_SLIPPAGE_BPS", e.to_string()))?;
        if max_slippage_bps == 0 || max_slippage_bps >= BPS_DENOMINATOR {
            return Err(invalid("MAX_SLIPPAGE_BPS", "must be in 1..10000"));
        }

        let max_gas_wei = parse_gwei("MAX_GAS_GWEI", &or_default("MAX_GAS_GWEI", "100"))?;
        let gas_cap_mode = match or_default("GAS_CAP_MODE", "skip")
            .to_ascii_lowercase()
            .as_str()
        {
            "skip" => GasCapMode::Skip,
            "wait" => {
                let secs = or_default("GAS_WAIT_MAX_SECS", "60")
                    .parse::<u64>()
                    .map_err(|e| invalid("GAS_WAIT_MAX_SECS", e.to_string()))?;
                if secs == 0 {
                    return Err(invalid("GAS_WAIT_MAX_SECS", "must be > 0 in wait mode"));
                }
                GasCapMode::Wait {
                    max_wait: Duration::from_secs(secs),
                }
            }
            other => {
                return Err(invalid(
                    "GAS_CAP_MODE",
                    format!("expected `skip` or `wait`, got `{other}`"),
                ));
            }
        };

        let dry_run = parse_bool("DRY_RUN", &or_default("DRY_RUN", "true"))?;

        let supported_dexes = parse_dexes(&or_default(
            "SUPPORTED_DEXES",
            "UNISWAP_V2,UNISWAP_V3,UNISWAP_UNIVERSAL_ROUTER,UNISWAP_V4",
        ))?;

        let token_blacklist = match get("TOKEN_BLACKLIST") {
            Some(raw) => parse_address_list("TOKEN_BLACKLIST", &raw)?,
            None => Vec::new(),
        };
        let trade_cooldown = Duration::from_secs(
            or_default("TRADE_COOLDOWN_SECS", "30")
                .parse::<u64>()
                .map_err(|e| invalid("TRADE_COOLDOWN_SECS", e.to_string()))?,
        );

        let channel_capacity =
            parse_positive_usize("CHANNEL_CAPACITY", &or_default("CHANNEL_CAPACITY", "1024"))?;
        let executor_workers =
            parse_positive_usize("EXECUTOR_WORKERS", &or_default("EXECUTOR_WORKERS", "4"))?;

        let telegram = match (get("TELEGRAM_BOT_TOKEN"), get("TELEGRAM_CHAT_ID")) {
            (None, None) => None,
            (Some(_), None) => return Err(ConfigError::Missing("TELEGRAM_CHAT_ID")),
            (None, Some(_)) => return Err(ConfigError::Missing("TELEGRAM_BOT_TOKEN")),
            (Some(bot_token), Some(chat_id)) => {
                // Shape check only (`<bot id>:<secret>`); the value never enters the error.
                let valid = bot_token.split_once(':').is_some_and(|(id, secret)| {
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) && !secret.is_empty()
                });
                if !valid {
                    return Err(invalid(
                        "TELEGRAM_BOT_TOKEN",
                        "expected `<bot id>:<secret>` as issued by @BotFather",
                    ));
                }
                Some(TelegramConfig { bot_token, chat_id })
            }
        };

        Ok(Self {
            chain_id,
            ws_url,
            http_url,
            signer,
            copy_wallets,
            copy_mode,
            strategy,
            fixed_trade_wei,
            fixed_trade_usd_micro,
            buy_ratio_bps,
            sell_ratio_bps,
            max_trade_wei,
            max_slippage_bps,
            max_gas_wei,
            gas_cap_mode,
            dry_run,
            supported_dexes,
            token_blacklist,
            trade_cooldown,
            channel_capacity,
            executor_workers,
            telegram,
        })
    }
}

fn check_scheme(key: &'static str, url: &str, schemes: &[&str]) -> Result<(), ConfigError> {
    if schemes.iter().any(|s| url.starts_with(s)) {
        Ok(())
    } else {
        Err(invalid(key, format!("must start with one of {schemes:?}")))
    }
}

/// Parses a comma-separated address list. Duplicates are collapsed (first occurrence wins).
pub fn parse_address_list(key: &'static str, raw: &str) -> Result<Vec<Address>, ConfigError> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (idx, part) in raw.split(',').map(str::trim).enumerate() {
        if part.is_empty() {
            continue;
        }
        let addr = parse_address(part)
            .map_err(|reason| invalid(key, format!("entry #{idx} `{part}`: {reason}")))?;
        if seen.insert(addr) {
            out.push(addr);
        }
    }
    Ok(out)
}

pub fn parse_wallets(raw: &str) -> Result<Vec<Address>, ConfigError> {
    let out = parse_address_list("COPY_WALLETS", raw)?;
    if out.is_empty() {
        return Err(invalid("COPY_WALLETS", "must contain at least one address"));
    }
    Ok(out)
}

/// Strict address parse: `0x` + 40 hex, non-zero; mixed-case input must carry a valid
/// EIP-55 checksum (all-lower / all-upper are accepted as "no checksum").
pub fn parse_address(s: &str) -> Result<Address, String> {
    let hex = s
        .strip_prefix("0x")
        .ok_or_else(|| "missing 0x prefix".to_string())?;
    if hex.len() != 40 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("expected 0x followed by 40 hex chars".into());
    }
    let has_lower = hex.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = hex.chars().any(|c| c.is_ascii_uppercase());
    let addr = if has_lower && has_upper {
        Address::parse_checksummed(s, None).map_err(|_| "invalid EIP-55 checksum".to_string())?
    } else {
        s.parse::<Address>().map_err(|e| e.to_string())?
    };
    if addr == Address::ZERO {
        return Err("zero address is not allowed".into());
    }
    Ok(addr)
}

fn parse_eth(key: &'static str, raw: &str) -> Result<U256, ConfigError> {
    parse_ether(raw).map_err(|e| invalid(key, e.to_string()))
}

fn parse_gwei(key: &'static str, raw: &str) -> Result<u128, ConfigError> {
    let wei: U256 = parse_units(raw, "gwei")
        .map_err(|e| invalid(key, e.to_string()))?
        .into();
    if wei.is_zero() {
        return Err(invalid(key, "must be > 0"));
    }
    u128::try_from(wei).map_err(|_| invalid(key, "value too large"))
}

/// Decimal ratio (e.g. `0.1`, `1.0`) -> basis points. Max 4 decimal places so the
/// conversion is exact; ratio must be > 0.
pub fn parse_ratio_bps(key: &'static str, raw: &str) -> Result<u32, ConfigError> {
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
    if frac_part.len() > 4 {
        return Err(invalid(key, "at most 4 decimal places supported"));
    }
    let int: u32 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse()
            .map_err(|_| invalid(key, format!("`{raw}` is out of range")))?
    };
    let frac: u32 = format!("{frac_part:0<4}")
        .parse()
        .map_err(|_| invalid(key, format!("`{raw}` is not a decimal")))?;
    let bps = int
        .checked_mul(BPS_DENOMINATOR)
        .and_then(|v| v.checked_add(frac))
        .ok_or_else(|| invalid(key, format!("`{raw}` is out of range")))?;
    if bps == 0 {
        return Err(invalid(key, "must be > 0"));
    }
    Ok(bps)
}

/// Only exact `true`/`false` are accepted: a typo must never silently disable DRY_RUN.
fn parse_bool(key: &'static str, raw: &str) -> Result<bool, ConfigError> {
    match raw.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid(
            key,
            format!("expected `true` or `false`, got `{raw}`"),
        )),
    }
}

fn parse_dexes(raw: &str) -> Result<Vec<Dex>, ConfigError> {
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let dex: Dex = part.parse().map_err(|e| invalid("SUPPORTED_DEXES", e))?;
        if !out.contains(&dex) {
            out.push(dex);
        }
    }
    if out.is_empty() {
        return Err(invalid("SUPPORTED_DEXES", "must contain at least one dex"));
    }
    Ok(out)
}

fn parse_positive_usize(key: &'static str, raw: &str) -> Result<usize, ConfigError> {
    match raw.parse::<usize>() {
        Ok(0) => Err(invalid(key, "must be > 0")),
        Ok(v) => Ok(v),
        Err(e) => Err(invalid(key, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Public Anvil dev key #0 (address 0xf39F...2266) — test-only, no real funds.
    const TEST_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const W1: &str = "0x1111111111111111111111111111111111111111";
    const W2: &str = "0x2222222222222222222222222222222222222222";

    fn base_env() -> HashMap<&'static str, String> {
        HashMap::from([
            ("ETH_WS_URL", "wss://node.example/ws".to_string()),
            ("ETH_HTTP_URL", "https://node.example".to_string()),
            ("PRIVATE_KEY", TEST_KEY.to_string()),
            ("COPY_WALLETS", format!("{W1},{W2}")),
        ])
    }

    fn load(env: &HashMap<&'static str, String>) -> Result<Config, ConfigError> {
        Config::from_lookup(|k| env.get(k).cloned())
    }

    fn assert_invalid(env: &HashMap<&'static str, String>, expected_key: &str) {
        match load(env) {
            Err(ConfigError::Invalid { key, .. }) => assert_eq!(key, expected_key),
            other => panic!("expected Invalid({expected_key}), got {other:?}"),
        }
    }

    #[test]
    fn loads_with_safe_defaults() {
        let cfg = load(&base_env()).unwrap();
        assert_eq!(cfg.chain_id, 1);
        assert!(cfg.dry_run, "DRY_RUN must default to true");
        assert_eq!(cfg.copy_mode, CopyMode::Pending);
        assert_eq!(cfg.strategy, StrategyKind::FixedEth);
        assert_eq!(cfg.fixed_trade_wei, U256::from(50_000_000_000_000_000u64));
        assert_eq!(cfg.max_trade_wei, U256::from(500_000_000_000_000_000u64));
        assert_eq!(cfg.buy_ratio_bps, 1_000);
        assert_eq!(cfg.sell_ratio_bps, 10_000);
        assert_eq!(cfg.max_slippage_bps, 300);
        assert_eq!(cfg.max_gas_wei, 100_000_000_000);
        assert_eq!(cfg.supported_dexes.len(), 4);
    }

    #[test]
    fn copy_wallets_parsing_trims_dedups_and_skips_empty() {
        let raw = format!(" {W1} ,{W2},, {W1} ,");
        let wallets = parse_wallets(&raw).unwrap();
        assert_eq!(
            wallets,
            vec![W1.parse::<Address>().unwrap(), W2.parse().unwrap()]
        );
    }

    #[test]
    fn copy_wallets_rejects_bad_entries() {
        for bad in [
            "",
            " , ",
            "0x123",
            "1111111111111111111111111111111111111111",
            "0xZZ11111111111111111111111111111111111111",
            "0x0000000000000000000000000000000000000000",
        ] {
            assert!(parse_wallets(bad).is_err(), "should reject `{bad}`");
        }
    }

    #[test]
    fn address_checksum_is_enforced_for_mixed_case() {
        assert!(parse_address("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266").is_ok());
        assert!(parse_address("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266").is_ok());
        assert!(parse_address("0xF39FD6E51AAD88F6F4CE6AB8827279CFFFB92266").is_ok());
        // flipped case of one char -> bad checksum
        assert!(parse_address("0xF39Fd6e51aad88F6F4ce6aB8827279cffFb92266").is_err());
    }

    #[test]
    fn rejects_bot_wallet_in_copy_wallets() {
        let mut env = base_env();
        env.insert(
            "COPY_WALLETS",
            format!("{W1},0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
        );
        assert_invalid(&env, "COPY_WALLETS");
    }

    #[test]
    fn missing_required_vars() {
        for key in ["ETH_WS_URL", "ETH_HTTP_URL", "PRIVATE_KEY", "COPY_WALLETS"] {
            let mut env = base_env();
            env.remove(key);
            match load(&env) {
                Err(ConfigError::Missing(k)) => assert_eq!(k, key),
                other => panic!("expected Missing({key}), got {other:?}"),
            }
        }
    }

    #[test]
    fn invalid_private_key_fails_without_leaking_it() {
        let mut env = base_env();
        let bad = "0xdeadbeefdeadbeefdeadbeefdeadbeef";
        env.insert("PRIVATE_KEY", bad.to_string());
        let err = load(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Signer(SignerError::InvalidPrivateKey)
        ));
        assert!(!err.to_string().contains("deadbeef"));
        assert!(!format!("{err:?}").contains("deadbeef"));
    }

    #[test]
    fn config_debug_does_not_leak_private_key() {
        let cfg = load(&base_env()).unwrap();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(&TEST_KEY[2..]));
    }

    #[test]
    fn dry_run_rejects_typos() {
        let mut env = base_env();
        env.insert("DRY_RUN", "flase".into());
        assert_invalid(&env, "DRY_RUN");
        env.insert("DRY_RUN", "FALSE".into());
        assert!(!load(&env).unwrap().dry_run);
    }

    #[test]
    fn url_schemes_are_validated() {
        let mut env = base_env();
        env.insert("ETH_WS_URL", "https://node.example".into());
        assert_invalid(&env, "ETH_WS_URL");
        let mut env = base_env();
        env.insert("ETH_HTTP_URL", "wss://node.example".into());
        assert_invalid(&env, "ETH_HTTP_URL");
    }

    #[test]
    fn ratio_parsing() {
        assert_eq!(parse_ratio_bps("R", "0.1").unwrap(), 1_000);
        assert_eq!(parse_ratio_bps("R", "0.10").unwrap(), 1_000);
        assert_eq!(parse_ratio_bps("R", "1").unwrap(), 10_000);
        assert_eq!(parse_ratio_bps("R", "1.0").unwrap(), 10_000);
        assert_eq!(parse_ratio_bps("R", ".5").unwrap(), 5_000);
        assert_eq!(parse_ratio_bps("R", "0.0001").unwrap(), 1);
        for bad in ["0", "0.0", "-0.1", "abc", "0.00001", ".", "1e-2"] {
            assert!(parse_ratio_bps("R", bad).is_err(), "should reject `{bad}`");
        }
    }

    #[test]
    fn sell_ratio_above_one_rejected() {
        let mut env = base_env();
        env.insert("SELL_RATIO", "1.5".into());
        assert_invalid(&env, "SELL_RATIO");
    }

    #[test]
    fn fixed_trade_cannot_exceed_max_trade() {
        let mut env = base_env();
        env.insert("FIXED_TRADE_ETH", "1".into());
        env.insert("MAX_TRADE_ETH", "0.5".into());
        assert_invalid(&env, "FIXED_TRADE_ETH");
    }

    #[test]
    fn slippage_bounds() {
        for bad in ["0", "10000", "-1", "x"] {
            let mut env = base_env();
            env.insert("MAX_SLIPPAGE_BPS", bad.into());
            assert_invalid(&env, "MAX_SLIPPAGE_BPS");
        }
    }

    #[test]
    fn gas_cap_mode() {
        assert_eq!(load(&base_env()).unwrap().gas_cap_mode, GasCapMode::Skip);
        let mut env = base_env();
        env.insert("GAS_CAP_MODE", "Wait".into());
        env.insert("GAS_WAIT_MAX_SECS", "90".into());
        assert_eq!(
            load(&env).unwrap().gas_cap_mode,
            GasCapMode::Wait {
                max_wait: Duration::from_secs(90)
            }
        );
        env.insert("GAS_WAIT_MAX_SECS", "0".into());
        assert_invalid(&env, "GAS_WAIT_MAX_SECS");
        env.insert("GAS_CAP_MODE", "later".into());
        assert_invalid(&env, "GAS_CAP_MODE");
    }

    #[test]
    fn phase3_settings() {
        let cfg = load(&base_env()).unwrap();
        assert_eq!(cfg.fixed_trade_usd_micro, U256::from(100_000_000u64));
        assert!(cfg.token_blacklist.is_empty());
        assert_eq!(cfg.trade_cooldown, Duration::from_secs(30));

        let mut env = base_env();
        env.insert("FIXED_TRADE_USD", "12.5".into());
        env.insert("TOKEN_BLACKLIST", format!("{W1}, {W1},{W2}"));
        env.insert("TRADE_COOLDOWN_SECS", "0".into());
        let cfg = load(&env).unwrap();
        assert_eq!(cfg.fixed_trade_usd_micro, U256::from(12_500_000u64));
        assert_eq!(cfg.token_blacklist.len(), 2);
        assert_eq!(cfg.trade_cooldown, Duration::ZERO);

        for (key, bad) in [
            ("FIXED_TRADE_USD", "0"),
            ("TOKEN_BLACKLIST", "0x123"),
            ("TRADE_COOLDOWN_SECS", "-1"),
        ] {
            let mut env = base_env();
            env.insert(key, bad.into());
            assert_invalid(&env, key);
        }
    }

    #[test]
    fn telegram_settings() {
        assert!(load(&base_env()).unwrap().telegram.is_none());

        let token = "123456:SECRET-abc";
        let mut env = base_env();
        env.insert("TELEGRAM_BOT_TOKEN", token.into());
        match load(&env) {
            Err(ConfigError::Missing(k)) => assert_eq!(k, "TELEGRAM_CHAT_ID"),
            other => panic!("expected Missing, got {other:?}"),
        }
        env.insert("TELEGRAM_CHAT_ID", "-1001234".into());
        let cfg = load(&env).unwrap();
        let tg = cfg.telegram.as_ref().unwrap();
        assert_eq!(
            (tg.bot_token.as_str(), tg.chat_id.as_str()),
            (token, "-1001234")
        );
        assert!(!format!("{cfg:?}").contains("SECRET"));

        env.insert("TELEGRAM_BOT_TOKEN", "SECRET-no-colon".into());
        let err = load(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "TELEGRAM_BOT_TOKEN",
                ..
            }
        ));
        assert!(!err.to_string().contains("SECRET"));
    }

    #[test]
    fn gas_and_enums() {
        let mut env = base_env();
        env.insert("MAX_GAS_GWEI", "1.5".into());
        env.insert("COPY_MODE", "Confirmed".into());
        env.insert("COPY_STRATEGY", "ratio".into());
        env.insert("SUPPORTED_DEXES", "uniswap_v2, UNISWAP_V2".into());
        let cfg = load(&env).unwrap();
        assert_eq!(cfg.max_gas_wei, 1_500_000_000);
        assert_eq!(cfg.copy_mode, CopyMode::Confirmed);
        assert_eq!(cfg.strategy, StrategyKind::Ratio);
        assert_eq!(cfg.supported_dexes, vec![Dex::UniswapV2]);

        env.insert("SUPPORTED_DEXES", "UNISWAP_V2,SUSHI".into());
        assert_invalid(&env, "SUPPORTED_DEXES");
    }
}

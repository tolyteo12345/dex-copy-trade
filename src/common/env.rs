//! Env-var parsing helpers shared by both binaries' configs.

use crate::common::telegram::TelegramConfig;

pub const BPS_DENOMINATOR: u32 = 10_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvError {
    #[error("missing required env var {0}")]
    Missing(&'static str),
    #[error("invalid value for {key}: {reason}")]
    Invalid { key: &'static str, reason: String },
}

pub fn invalid(key: &'static str, reason: impl Into<String>) -> EnvError {
    EnvError::Invalid {
        key,
        reason: reason.into(),
    }
}

pub fn check_scheme(key: &'static str, url: &str, schemes: &[&str]) -> Result<(), EnvError> {
    if schemes.iter().any(|s| url.starts_with(s)) {
        Ok(())
    } else {
        Err(invalid(key, format!("must start with one of {schemes:?}")))
    }
}

/// Decimal ratio (e.g. `0.1`, `1.0`) -> basis points. Max 4 decimal places so the
/// conversion is exact; ratio must be > 0.
pub fn parse_ratio_bps(key: &'static str, raw: &str) -> Result<u32, EnvError> {
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
pub fn parse_bool(key: &'static str, raw: &str) -> Result<bool, EnvError> {
    match raw.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid(
            key,
            format!("expected `true` or `false`, got `{raw}`"),
        )),
    }
}

pub fn parse_positive_usize(key: &'static str, raw: &str) -> Result<usize, EnvError> {
    match raw.parse::<usize>() {
        Ok(0) => Err(invalid(key, "must be > 0")),
        Ok(v) => Ok(v),
        Err(e) => Err(invalid(key, e.to_string())),
    }
}

/// `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID`: both unset = notifications off. `get` must
/// return trimmed, non-empty values only.
pub fn telegram_from(
    get: impl Fn(&'static str) -> Option<String>,
) -> Result<Option<TelegramConfig>, EnvError> {
    match (get("TELEGRAM_BOT_TOKEN"), get("TELEGRAM_CHAT_ID")) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(EnvError::Missing("TELEGRAM_CHAT_ID")),
        (None, Some(_)) => Err(EnvError::Missing("TELEGRAM_BOT_TOKEN")),
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
            Ok(Some(TelegramConfig { bot_token, chat_id }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn bool_is_strict() {
        assert_eq!(parse_bool("B", "TRUE"), Ok(true));
        assert_eq!(parse_bool("B", "false"), Ok(false));
        assert!(parse_bool("B", "flase").is_err());
        assert!(parse_bool("B", "1").is_err());
    }
}

//! pump.fun programs: addresses, PDAs, account layouts, events and the instructions the bot
//! builds. Layouts follow the official IDLs (github.com/pump-fun/pump-public-docs, `idl/`).
//!
//! The bot always trades with the newest fixed-account instructions: bonding curve
//! `buy_exact_quote_in_v3` / `sell_v3` and PumpSwap `buy_exact_quote_in_v2` / `sell_v2`
//! (17 accounts, no optional/remaining accounts). Sources may use any variant or a router:
//! they are recognised by the events every variant emits, not by their instructions.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use crate::sol::models::TradeSide;

pub const PUMP_PROGRAM: Pubkey =
    Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
pub const PUMP_AMM_PROGRAM: Pubkey =
    Pubkey::from_str_const("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const PUMP_FEES_PROGRAM: Pubkey =
    Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
/// Pump Fees `fee_config` PDAs (`[b"fee_config", program_id]`), checked by tests.
pub const PUMP_FEE_CONFIG: Pubkey =
    Pubkey::from_str_const("8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt");
pub const PUMP_AMM_FEE_CONFIG: Pubkey =
    Pubkey::from_str_const("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx");
pub const PUMP_AMM_GLOBAL_CONFIG: Pubkey =
    Pubkey::from_str_const("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");

pub const WSOL_MINT: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");
pub const TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ATA_PROGRAM: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM_PROGRAM: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
pub const COMPUTE_BUDGET_PROGRAM: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

/// Buyback fee recipient wallets (docs/FEE_RECIPIENTS.md). v3/v2 trades take any one.
pub const BUYBACK_FEE_RECIPIENTS: [Pubkey; 8] = [
    Pubkey::from_str_const("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
    Pubkey::from_str_const("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7"),
    Pubkey::from_str_const("GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL"),
    Pubkey::from_str_const("3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR"),
    Pubkey::from_str_const("5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6"),
    Pubkey::from_str_const("EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL"),
    Pubkey::from_str_const("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD"),
    Pubkey::from_str_const("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW"),
];

const IX_CURVE_BUY_EXACT_QUOTE_IN_V3: [u8; 8] = [225, 247, 80, 30, 213, 179, 132, 136];
const IX_CURVE_SELL_V3: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];
const IX_AMM_BUY_EXACT_QUOTE_IN_V2: [u8; 8] = [194, 171, 28, 70, 104, 77, 91, 47];
const IX_AMM_SELL_V2: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];

/// Anchor `emit_cpi!` prefix: `sha256("anchor:event")[..8]`.
const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
const EV_CURVE_TRADE: [u8; 8] = [189, 219, 127, 211, 78, 230, 97, 238];
const EV_AMM_BUY: [u8; 8] = [103, 244, 82, 31, 44, 245, 119, 119];
const EV_AMM_SELL: [u8; 8] = [62, 47, 55, 10, 165, 3, 220, 42];

const ACC_BONDING_CURVE: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];
const ACC_POOL: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("{0}: wrong account discriminator")]
    Discriminator(&'static str),
    #[error("{0}: account data too short")]
    TooShort(&'static str),
}

/// On-chain accounts the executor needs to build the bot's swap, resolved by the decision
/// stage. `pool` is `None` for the bonding curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Market {
    pub mint: Pubkey,
    /// SPL Token for older coins, Token-2022 for `create_v2` coins.
    pub base_token_program: Pubkey,
    pub pool: Option<PoolVaults>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolVaults {
    pub pool: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
}

// ---------------------------------------------------------------- PDAs / addresses

pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

pub fn bonding_curve(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PUMP_PROGRAM).0
}

fn pump_global() -> Pubkey {
    Pubkey::find_program_address(&[b"global"], &PUMP_PROGRAM).0
}

fn event_authority(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program).0
}

fn user_volume_accumulator(user: &Pubkey, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], program).0
}

/// Any of the 8 recipients is valid; spreading by mint avoids every bot trade write-locking
/// the same account (which would serialise them with each other in the scheduler).
pub fn buyback_wallet(mint: &Pubkey) -> Pubkey {
    BUYBACK_FEE_RECIPIENTS[usize::from(mint.as_ref()[0]) % BUYBACK_FEE_RECIPIENTS.len()]
}

// ---------------------------------------------------------------- account layouts

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurveState {
    pub complete: bool,
    pub is_cashback_coin: bool,
    /// `Pubkey::default()` for SOL-paired coins.
    pub quote_mint: Pubkey,
}

impl BondingCurveState {
    pub fn is_sol_paired(&self) -> bool {
        self.quote_mint == Pubkey::default() || self.quote_mint == WSOL_MINT
    }
}

/// Older curves have a shorter layout (fields were appended); missing trailing fields take
/// their default, which is also what the program assumes for them.
pub fn parse_bonding_curve(data: &[u8]) -> Result<BondingCurveState, LayoutError> {
    const NAME: &str = "bonding curve";
    if data.get(..8) != Some(&ACC_BONDING_CURVE[..]) {
        return Err(LayoutError::Discriminator(NAME));
    }
    // disc(8) + 5 × u64 reserves/supply, then `complete` at 48.
    let complete = *data.get(48).ok_or(LayoutError::TooShort(NAME))? != 0;
    // creator(32) @49, is_mayhem_mode @81, is_cashback_coin @82, quote_mint @83..115.
    let is_cashback_coin = data.get(82).is_some_and(|b| *b != 0);
    let quote_mint = data.get(83..115).map(pubkey_at).unwrap_or_default();
    Ok(BondingCurveState {
        complete,
        is_cashback_coin,
        quote_mint,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub is_cashback_coin: bool,
}

pub fn parse_pool(data: &[u8]) -> Result<PoolState, LayoutError> {
    const NAME: &str = "pool";
    if data.get(..8) != Some(&ACC_POOL[..]) {
        return Err(LayoutError::Discriminator(NAME));
    }
    // disc(8) bump(1) index(2) creator(32) → base_mint @43, quote_mint @75, lp_mint @107,
    // base vault @139, quote vault @171, lp_supply @203, coin_creator @211,
    // is_mayhem_mode @243, is_cashback_coin @244.
    let key = |at: usize| {
        data.get(at..at + 32)
            .map(pubkey_at)
            .ok_or(LayoutError::TooShort(NAME))
    };
    Ok(PoolState {
        base_mint: key(43)?,
        quote_mint: key(75)?,
        base_vault: key(139)?,
        quote_vault: key(171)?,
        is_cashback_coin: data.get(244).is_some_and(|b| *b != 0),
    })
}

/// SPL Token and Token-2022 accounts share the base layout: mint(32) owner(32) amount(u64).
pub fn token_account_amount(data: &[u8]) -> Option<u64> {
    data.get(64..72).map(le_u64)
}

// ---------------------------------------------------------------- events

/// A pump trade as reported by the program itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpEvent {
    /// Bonding-curve `TradeEvent`. `sol_amount` is the curve's quote amount before fees.
    Curve {
        mint: Pubkey,
        sol_amount: u64,
        token_amount: u64,
        is_buy: bool,
        user: Pubkey,
    },
    /// PumpSwap `BuyEvent` / `SellEvent`. `quote_amount` is what the user paid (buy) or
    /// received (sell), fees included. The event has no mint: the base mint is resolved
    /// from `user_base_token_account`.
    Amm {
        side: TradeSide,
        pool: Pubkey,
        user: Pubkey,
        user_base_token_account: Pubkey,
        base_amount: u64,
        quote_amount: u64,
    },
}

/// Parses an `emit_cpi!` self-invocation of a pump program. Only the leading fields are
/// read: pump appends new fields at the end of its events, so this prefix stays stable.
pub fn parse_event(program: &Pubkey, data: &[u8]) -> Option<PumpEvent> {
    if data.get(..8)? != EVENT_IX_TAG {
        return None;
    }
    let disc: [u8; 8] = data.get(8..16)?.try_into().ok()?;
    let body = data.get(16..)?;
    if *program == PUMP_PROGRAM && disc == EV_CURVE_TRADE {
        // mint(32) sol_amount(u64) token_amount(u64) is_buy(bool) user(32)
        return Some(PumpEvent::Curve {
            mint: pubkey_at(body.get(0..32)?),
            sol_amount: le_u64(body.get(32..40)?),
            token_amount: le_u64(body.get(40..48)?),
            is_buy: *body.get(48)? != 0,
            user: pubkey_at(body.get(49..81)?),
        });
    }
    if *program == PUMP_AMM_PROGRAM && (disc == EV_AMM_BUY || disc == EV_AMM_SELL) {
        // timestamp(i64) base_amount(u64) ... 11 more u64 ... user_quote_amount @104,
        // pool @112, user @144, user_base_token_account @176. Same offsets in both events.
        return Some(PumpEvent::Amm {
            side: if disc == EV_AMM_BUY {
                TradeSide::Buy
            } else {
                TradeSide::Sell
            },
            base_amount: le_u64(body.get(8..16)?),
            quote_amount: le_u64(body.get(104..112)?),
            pool: pubkey_at(body.get(112..144)?),
            user: pubkey_at(body.get(144..176)?),
            user_base_token_account: pubkey_at(body.get(176..208)?),
        });
    }
    None
}

// ---------------------------------------------------------------- instructions

pub fn compute_unit_limit(units: u32) -> Instruction {
    let mut data = vec![2];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(COMPUTE_BUDGET_PROGRAM, &data, vec![])
}

pub fn compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![3];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction::new_with_bytes(COMPUTE_BUDGET_PROGRAM, &data, vec![])
}

fn create_ata_idempotent(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    program: &Pubkey,
) -> Instruction {
    Instruction::new_with_bytes(
        ATA_PROGRAM,
        &[1],
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(owner, mint, program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(*program, false),
        ],
    )
}

fn system_transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = vec![2, 0, 0, 0];
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction::new_with_bytes(
        SYSTEM_PROGRAM,
        &data,
        vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
    )
}

fn sync_native(account: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        TOKEN_PROGRAM,
        &[17],
        vec![AccountMeta::new(*account, false)],
    )
}

fn close_account(account: &Pubkey, destination: &Pubkey, owner: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        TOKEN_PROGRAM,
        &[9],
        vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
    )
}

fn trade_data(disc: [u8; 8], a: u64, b: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&a.to_le_bytes());
    data.extend_from_slice(&b.to_le_bytes());
    data
}

/// The 17 accounts shared by the v3 curve trades (docs/instructions/TRADE_V3.md), for a
/// SOL-paired coin: the quote accounts are the wrapped-SOL addresses, which the program
/// only seed-checks and never touches.
fn curve_v3_accounts(user: &Pubkey, m: &Market) -> Vec<AccountMeta> {
    let curve = bonding_curve(&m.mint);
    vec![
        AccountMeta::new_readonly(pump_global(), false),
        AccountMeta::new_readonly(m.mint, false),
        AccountMeta::new_readonly(WSOL_MINT, false),
        AccountMeta::new_readonly(m.base_token_program, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        AccountMeta::new(curve, false),
        AccountMeta::new(ata(&curve, &m.mint, &m.base_token_program), false),
        AccountMeta::new(ata(&curve, &WSOL_MINT, &TOKEN_PROGRAM), false),
        AccountMeta::new(*user, true),
        AccountMeta::new(ata(user, &m.mint, &m.base_token_program), false),
        AccountMeta::new(ata(user, &WSOL_MINT, &TOKEN_PROGRAM), false),
        AccountMeta::new(user_volume_accumulator(user, &PUMP_PROGRAM), false),
        AccountMeta::new_readonly(PUMP_FEE_CONFIG, false),
        // SOL-paired: the buyback wallet itself (not its token account).
        AccountMeta::new(buyback_wallet(&m.mint), false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(event_authority(&PUMP_PROGRAM), false),
        AccountMeta::new_readonly(PUMP_PROGRAM, false),
    ]
}

/// The 17 accounts shared by the v2 PumpSwap trades (docs/instructions/PUMP_SWAP_TRADE_V2.md)
/// for a canonical pool (base = coin, quote = WSOL).
fn amm_v2_accounts(user: &Pubkey, m: &Market, p: &PoolVaults) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(p.pool, false),
        AccountMeta::new(*user, true),
        AccountMeta::new_readonly(PUMP_AMM_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(m.mint, false),
        AccountMeta::new_readonly(WSOL_MINT, false),
        AccountMeta::new(ata(user, &m.mint, &m.base_token_program), false),
        AccountMeta::new(ata(user, &WSOL_MINT, &TOKEN_PROGRAM), false),
        AccountMeta::new(p.base_vault, false),
        AccountMeta::new(p.quote_vault, false),
        AccountMeta::new_readonly(m.base_token_program, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        AccountMeta::new(user_volume_accumulator(user, &PUMP_AMM_PROGRAM), false),
        AccountMeta::new_readonly(PUMP_AMM_FEE_CONFIG, false),
        // PumpSwap wants the buyback wallet's WSOL token account (always exists).
        AccountMeta::new(
            ata(&buyback_wallet(&m.mint), &WSOL_MINT, &TOKEN_PROGRAM),
            false,
        ),
        AccountMeta::new_readonly(event_authority(&PUMP_AMM_PROGRAM), false),
        AccountMeta::new_readonly(PUMP_AMM_PROGRAM, false),
    ]
}

/// Every instruction of the bot's swap except the compute-budget ones. `min_out` is the
/// only value that differs between the simulated and the sent transaction.
///
/// BUY spends exactly `amount_in` lamports; SELL sells exactly `amount_in` base units. SOL
/// is paid and received natively: PumpSwap legs wrap into the bot's WSOL ATA and close it
/// at the end (returning its rent and any leftover).
pub fn swap_instructions(
    user: &Pubkey,
    side: TradeSide,
    m: &Market,
    amount_in: u64,
    min_out: u64,
) -> Vec<Instruction> {
    let wsol = ata(user, &WSOL_MINT, &TOKEN_PROGRAM);
    match (m.pool, side) {
        (None, TradeSide::Buy) => vec![
            // v3 trades require the user's base account to exist already.
            create_ata_idempotent(user, user, &m.mint, &m.base_token_program),
            Instruction::new_with_bytes(
                PUMP_PROGRAM,
                &trade_data(IX_CURVE_BUY_EXACT_QUOTE_IN_V3, amount_in, min_out),
                curve_v3_accounts(user, m),
            ),
        ],
        (None, TradeSide::Sell) => vec![Instruction::new_with_bytes(
            PUMP_PROGRAM,
            &trade_data(IX_CURVE_SELL_V3, amount_in, min_out),
            curve_v3_accounts(user, m),
        )],
        (Some(p), TradeSide::Buy) => vec![
            create_ata_idempotent(user, user, &WSOL_MINT, &TOKEN_PROGRAM),
            system_transfer(user, &wsol, amount_in),
            sync_native(&wsol),
            create_ata_idempotent(user, user, &m.mint, &m.base_token_program),
            Instruction::new_with_bytes(
                PUMP_AMM_PROGRAM,
                &trade_data(IX_AMM_BUY_EXACT_QUOTE_IN_V2, amount_in, min_out),
                amm_v2_accounts(user, m, &p),
            ),
            close_account(&wsol, user, user),
        ],
        (Some(p), TradeSide::Sell) => vec![
            create_ata_idempotent(user, user, &WSOL_MINT, &TOKEN_PROGRAM),
            Instruction::new_with_bytes(
                PUMP_AMM_PROGRAM,
                &trade_data(IX_AMM_SELL_V2, amount_in, min_out),
                amm_v2_accounts(user, m, &p),
            ),
            close_account(&wsol, user, user),
        ],
    }
}

fn pubkey_at(bytes: &[u8]) -> Pubkey {
    let mut a = [0u8; 32];
    a.copy_from_slice(bytes);
    Pubkey::new_from_array(a)
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(bytes);
    u64::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINT: Pubkey = Pubkey::from_str_const("68eGdrUBX87sMvJ7SV7rJvA4P53F2ntf45WT1C4Dpump");
    const USER: Pubkey = Pubkey::from_str_const("omegoMAe1AMY5MFKQQr3JwXVy8F4eCvmBAfcpo8XAfq");

    fn curve_market() -> Market {
        Market {
            mint: MINT,
            base_token_program: TOKEN_2022_PROGRAM,
            pool: None,
        }
    }

    fn pool_market() -> Market {
        Market {
            pool: Some(PoolVaults {
                pool: Pubkey::new_unique(),
                base_vault: Pubkey::new_unique(),
                quote_vault: Pubkey::new_unique(),
            }),
            ..curve_market()
        }
    }

    #[test]
    fn documented_pdas_match_derivation() {
        let fee_config = |program: &Pubkey| {
            Pubkey::find_program_address(&[b"fee_config", program.as_ref()], &PUMP_FEES_PROGRAM).0
        };
        assert_eq!(fee_config(&PUMP_PROGRAM), PUMP_FEE_CONFIG);
        assert_eq!(fee_config(&PUMP_AMM_PROGRAM), PUMP_AMM_FEE_CONFIG);
        assert_eq!(
            Pubkey::find_program_address(&[b"global_config"], &PUMP_AMM_PROGRAM).0,
            PUMP_AMM_GLOBAL_CONFIG
        );
        assert_eq!(
            pump_global(),
            Pubkey::from_str_const("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf")
        );
        assert_eq!(
            event_authority(&PUMP_PROGRAM),
            Pubkey::from_str_const("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1")
        );
        assert_eq!(
            event_authority(&PUMP_AMM_PROGRAM),
            Pubkey::from_str_const("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR")
        );
    }

    #[test]
    fn curve_buy_spends_exact_sol_and_creates_base_ata_first() {
        let ixs = swap_instructions(&USER, TradeSide::Buy, &curve_market(), 1_000, 42);
        assert_eq!(ixs.len(), 2);
        assert_eq!(ixs[0].program_id, ATA_PROGRAM);
        assert_eq!(
            ixs[0].accounts[1].pubkey,
            ata(&USER, &MINT, &TOKEN_2022_PROGRAM)
        );
        let trade = &ixs[1];
        assert_eq!(trade.program_id, PUMP_PROGRAM);
        assert_eq!(trade.accounts.len(), 17);
        assert_eq!(
            trade.data,
            trade_data(IX_CURVE_BUY_EXACT_QUOTE_IN_V3, 1_000, 42)
        );
        let signers: Vec<_> = trade.accounts.iter().filter(|a| a.is_signer).collect();
        assert_eq!(signers.len(), 1);
        assert_eq!(signers[0].pubkey, USER);
    }

    #[test]
    fn amm_legs_wrap_and_unwrap_sol() {
        let m = pool_market();
        let wsol = ata(&USER, &WSOL_MINT, &TOKEN_PROGRAM);
        let buy = swap_instructions(&USER, TradeSide::Buy, &m, 5_000, 7);
        let programs: Vec<_> = buy.iter().map(|i| i.program_id).collect();
        assert_eq!(
            programs,
            [
                ATA_PROGRAM,
                SYSTEM_PROGRAM,
                TOKEN_PROGRAM,
                ATA_PROGRAM,
                PUMP_AMM_PROGRAM,
                TOKEN_PROGRAM
            ]
        );
        assert_eq!(buy[1].accounts[1].pubkey, wsol);
        assert_eq!(buy[1].data[4..], 5_000u64.to_le_bytes());
        assert_eq!(buy[5].data, [9], "last instruction closes the WSOL account");
        assert_eq!(
            buy[4].data,
            trade_data(IX_AMM_BUY_EXACT_QUOTE_IN_V2, 5_000, 7)
        );

        let sell = swap_instructions(&USER, TradeSide::Sell, &m, 9, 3);
        assert_eq!(sell.len(), 3);
        assert_eq!(sell[1].data, trade_data(IX_AMM_SELL_V2, 9, 3));
        assert_eq!(sell[1].accounts[6].pubkey, wsol);
    }

    #[test]
    fn bonding_curve_layout_tolerates_old_short_accounts() {
        let mut data = vec![0u8; 151];
        data[..8].copy_from_slice(&ACC_BONDING_CURVE);
        data[48] = 1;
        data[82] = 1;
        data[83..115].copy_from_slice(WSOL_MINT.as_ref());
        let s = parse_bonding_curve(&data).unwrap();
        assert!(s.complete && s.is_cashback_coin && s.is_sol_paired());

        let mut usdc = data.clone();
        usdc[83..115].copy_from_slice(Pubkey::new_unique().as_ref());
        assert!(!parse_bonding_curve(&usdc).unwrap().is_sol_paired());

        let old = parse_bonding_curve(&data[..81]).unwrap();
        assert!(old.complete && !old.is_cashback_coin && old.is_sol_paired());

        assert_eq!(
            parse_bonding_curve(&data[..40]),
            Err(LayoutError::TooShort("bonding curve"))
        );
        data[0] ^= 1;
        assert!(matches!(
            parse_bonding_curve(&data),
            Err(LayoutError::Discriminator(_))
        ));
    }

    #[test]
    fn pool_layout_offsets() {
        let (base, quote, bv, qv) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut data = vec![0u8; 300];
        data[..8].copy_from_slice(&ACC_POOL);
        data[43..75].copy_from_slice(base.as_ref());
        data[75..107].copy_from_slice(quote.as_ref());
        data[139..171].copy_from_slice(bv.as_ref());
        data[171..203].copy_from_slice(qv.as_ref());
        let p = parse_pool(&data).unwrap();
        assert_eq!(
            (
                p.base_mint,
                p.quote_mint,
                p.base_vault,
                p.quote_vault,
                p.is_cashback_coin
            ),
            (base, quote, bv, qv, false)
        );
        assert!(parse_pool(&data[..190]).is_err());
    }

    #[test]
    fn non_event_data_is_ignored() {
        assert_eq!(parse_event(&PUMP_PROGRAM, &[1, 2, 3]), None);
        let mut data = EVENT_IX_TAG.to_vec();
        data.extend_from_slice(&EV_CURVE_TRADE);
        assert_eq!(parse_event(&PUMP_PROGRAM, &data), None, "truncated body");
        data.extend_from_slice(&[0u8; 81]);
        assert_eq!(parse_event(&PUMP_AMM_PROGRAM, &data), None, "wrong program");
        assert!(parse_event(&PUMP_PROGRAM, &data).is_some());
    }
}

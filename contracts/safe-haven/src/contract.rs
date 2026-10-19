// ============================================================
//  SAFE-HAVEN — Soroban Smart Contract
//  Stellar Blockchain | Soroban SDK v22
// ============================================================

use soroban_sdk::{contract, contractimpl, token, Address, Env, String, Vec};

use crate::{
    constants::{
        MAX_BATCH_SIZE, MAX_DEPOSIT_AMOUNT, MAX_LOCK_DURATION_SECS, MIN_LOCK_DURATION_SECS,
        MIN_LOCK_LEDGERS, STAKER_PENALTY_BPS, FEE_RECIPIENT_PENALTY_BPS,
    },
    errors::VaultError,
    events, storage,
    types::{
        DepositType, MultiTokenVaultEntry, TokenDeposit, VaultEntry, LedgerVaultEntry, Page,
        STORAGE_VERSION, MAX_TOKENS_PER_DEPOSIT, InsuranceClaim, ClaimStatus,
    },
};

/// Minimum compound frequency: must be at least 60 seconds if non-zero.
const MIN_COMPOUND_FREQUENCY_SECS: u64 = 60;

/// Annual interest rate used for compound accrual: 5% expressed as basis points (500).
/// In production this could be made configurable, but per the issue scope it is fixed.
const ANNUAL_INTEREST_BPS: u128 = 500;

/// Seconds in a year (non-leap) used for pro-rata interest calculations.
const SECS_PER_YEAR: u128 = 31_536_000;

#[contract]
pub struct SafeHaven;

// ----------------------------------------------------------------
//  Internal helpers
// ----------------------------------------------------------------

/// Compute the accrued balance for `entry` as of `now`.
///
/// Uses compound interest applied period-by-period:
///   balance_after_one_period = balance + balance × ANNUAL_INTEREST_BPS × freq_secs
///                                                    ─────────────────────────────
///                                                       SECS_PER_YEAR × 10_000
///
/// Crucially, the multiplication is done **before** the division so that integer
/// truncation does not zero-out the per-period rate for sub-year frequencies.
/// This guarantees that any `amount ≥ (SECS_PER_YEAR × 10_000) / ANNUAL_INTEREST_BPS`
/// (≈ 6.3 M for 5% annual rate) will accrue at least 1 unit per period.
///
/// Returns the new balance (>= original amount).
fn compute_accrued_amount(amount: i128, entry_freq: u64, last_accrual: u64, now: u64) -> i128 {
    if entry_freq == 0 || now <= last_accrual {
        return amount;
    }

    let elapsed = now.saturating_sub(last_accrual);
    let periods = elapsed / entry_freq; // integer division — only complete periods

    if periods == 0 {
        return amount;
    }

    // Denominator: SECS_PER_YEAR × 10_000 (fixed for 5% p.a. expressed in bps)
    let denominator: u128 = SECS_PER_YEAR.saturating_mul(10_000);
    let freq_u128 = entry_freq as u128;

    // Apply compounding iteratively.  Capped to guard against enormous period counts.
    let mut balance = amount;
    let periods_capped = periods.min(2_628_000); // 5 years ÷ 60 s/period
    for _ in 0..periods_capped {
        // interest = balance × ANNUAL_INTEREST_BPS × freq / (SECS_PER_YEAR × 10_000)
        // Multiplying balance × numerator first avoids truncating the small rate.
        let interest = (balance as u128)
            .saturating_mul(ANNUAL_INTEREST_BPS)
            .saturating_mul(freq_u128)
            / denominator;
        balance = balance.saturating_add(interest as i128);
    }
    balance
}

/// Check the withdrawal whitelist for `(depositor, deposit_id)`.
/// Returns `Ok(())` if:
///   - no whitelist is configured (None), OR
///   - the whitelist is empty, OR
///   - `recipient` is in the whitelist.
/// Returns `Err(RecipientNotWhitelisted)` otherwise.
fn check_whitelist(
    env: &Env,
    depositor: &Address,
    deposit_id: u32,
    recipient: &Address,
) -> Result<(), VaultError> {
    if let Some(wl) = storage::get_withdrawal_whitelist(env, depositor, deposit_id) {
        if wl.is_empty() {
            return Ok(());
        }
        for addr in wl.iter() {
            if addr == *recipient {
                return Ok(());
            }
        }
        return Err(VaultError::RecipientNotWhitelisted);
    }
    Ok(())
}

#[contractimpl]
impl SafeHaven {
    // ----------------------------------------------------------------
    //  Initialization
    // ----------------------------------------------------------------

    pub fn initialize(
        env: Env,
        admin: Address,
        fee_recipient: Address,
        max_deposit: Option<i128>,
        max_lock_secs: Option<u64>,
    ) -> Result<(), VaultError> {
        admin.require_auth();

        if storage::is_initialized(&env) {
            return Err(VaultError::AlreadyInitialized);
        }

        storage::set_admin(&env, &admin);
        storage::set_initialized(&env);
        storage::set_fee_recipient(&env, &fee_recipient);

        if let Some(v) = max_deposit {
            if v <= 0 {
                return Err(VaultError::InvalidAmount);
            }
            storage::set_max_deposit(&env, v);
        }

        if let Some(v) = max_lock_secs {
            if v == 0 {
                return Err(VaultError::LockDurationTooLong);
            }
            storage::set_max_lock_secs(&env, v);
        }

        let effective_max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
        let effective_max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
        events::contract_initialized(
            &env,
            &admin,
            &fee_recipient,
            effective_max_deposit,
            effective_max_lock,
        );

        Ok(())
    }

    // ----------------------------------------------------------------
    //  Core: Single-token Deposit
    // ----------------------------------------------------------------

    pub fn deposit(
        env: Env,
        depositor: Address,
        token: Address,
        amount: i128,
        unlock_time: u64,
        penalty_bps: u32,
    ) -> Result<u32, VaultError> {
        depositor.require_auth();

        if storage::is_paused(&env) {
            return Err(VaultError::ContractPaused);
        }

        if amount <= 0 {
            return Err(VaultError::InvalidAmount);
        }

        let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
        if amount > max_deposit {
            return Err(VaultError::AmountTooLarge);
        }

        if penalty_bps > 10_000 {
            return Err(VaultError::InvalidPenaltyBps);
        }

        if penalty_bps > 0 && storage::get_fee_recipient(&env).is_none() {
            return Err(VaultError::MissingFeeRecipient);
        }

        let now = env.ledger().timestamp();
        if unlock_time <= now {
            return Err(VaultError::UnlockTimeNotInFuture);
        }

        let max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
        let lock_duration: u64 = unlock_time.saturating_sub(now);
        if lock_duration > max_lock {
            return Err(VaultError::LockDurationTooLong);
        }
        if lock_duration < MIN_LOCK_DURATION_SECS {
            return Err(VaultError::LockDurationTooShort);
        }

        let deposit_id = storage::next_deposit_id(&env, &depositor);

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&depositor, &env.current_contract_address(), &amount);

        let entry = VaultEntry {
            token: token.clone(),
            amount,
            unlock_time,
            depositor: depositor.clone(),
            penalty_bps,
            compound_frequency_secs: 0,
            last_accrual_timestamp: now,
        };

        storage::set_deposit(&env, &depositor, deposit_id, &entry);
        storage::add_depositor(&env, &depositor);
        events::deposit(&env, &depositor, &token, amount, unlock_time, deposit_id);

        Ok(deposit_id)
    }

    pub fn batch_deposit(
        env: Env,
        depositor: Address,
        deposits: Vec<DepositRequest>,
    ) -> Result<Vec<u32>, VaultError> {
        depositor.require_auth();

        if deposits.len() > MAX_BATCH_SIZE {
            return Err(VaultError::BatchTooLarge);
        }

        let mut deposit_ids = Vec::new(&env);
        for request in deposits.iter() {
            if storage::is_paused(&env) || request.amount <= 0 {
                return Err(if storage::is_paused(&env) {
                    VaultError::ContractPaused
                } else {
                    VaultError::InvalidAmount
                });
            }
            let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
            if request.amount > max_deposit {
                return Err(VaultError::AmountTooLarge);
            }
            if request.penalty_bps > 10_000 {
                return Err(VaultError::InvalidPenaltyBps);
            }
            if request.penalty_bps > 0 && storage::get_fee_recipient(&env).is_none() {
                return Err(VaultError::MissingFeeRecipient);
            }
            let now = env.ledger().timestamp();
            if request.unlock_time <= now {
                return Err(VaultError::UnlockTimeNotInFuture);
            }
            let lock_duration = request.unlock_time.saturating_sub(now);
            let max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
            if lock_duration > max_lock {
                return Err(VaultError::LockDurationTooLong);
            }
            if lock_duration < MIN_LOCK_DURATION_SECS {
                return Err(VaultError::LockDurationTooShort);
            }
        }

        for request in deposits.iter() {
            let deposit_id = storage::next_deposit_id(&env, &depositor);
            token::Client::new(&env, &request.token).transfer(
                &depositor,
                &env.current_contract_address(),
                &request.amount,
            );
            let entry = VaultEntry {
                token: request.token.clone(),
                amount: request.amount,
                unlock_time: request.unlock_time,
                depositor: depositor.clone(),
                penalty_bps: request.penalty_bps,
            };
            storage::set_deposit(&env, &depositor, deposit_id, &entry);
            storage::add_depositor(&env, &depositor);
            events::deposit(
                &env,
                &depositor,
                &request.token,
                request.amount,
                request.unlock_time,
                deposit_id,
            );
            deposit_ids.push_back(deposit_id);
        }
        Ok(deposit_ids)
    }

    pub fn deposit_for(
        env: Env,
        payer: Address,
        depositor: Address,
        token: Address,
        amount: i128,
        unlock_time: u64,
        penalty_bps: u32,
    ) -> Result<u32, VaultError> {
        payer.require_auth();

        if storage::is_paused(&env) {
            return Err(VaultError::ContractPaused);
        }

        if amount <= 0 {
            return Err(VaultError::InvalidAmount);
        }

        let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
        if amount > max_deposit {
            return Err(VaultError::AmountTooLarge);
        }

        if penalty_bps > 10_000 {
            return Err(VaultError::InvalidPenaltyBps);
        }

        if penalty_bps > 0 && storage::get_fee_recipient(&env).is_none() {
            return Err(VaultError::MissingFeeRecipient);
        }

        let now = env.ledger().timestamp();
        if unlock_time <= now {
            return Err(VaultError::UnlockTimeNotInFuture);
        }

        let max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
        let lock_duration: u64 = unlock_time.saturating_sub(now);
        if lock_duration > max_lock {
            return Err(VaultError::LockDurationTooLong);
        }
        if lock_duration < MIN_LOCK_DURATION_SECS {
            return Err(VaultError::LockDurationTooShort);
        }

        let deposit_id = storage::next_deposit_id(&env, &depositor);

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&payer, &env.current_contract_address(), &amount);

        let entry = VaultEntry {
            token: token.clone(),
            amount,
            unlock_time,
            depositor: depositor.clone(),
            penalty_bps,
            compound_frequency_secs: 0,
            last_accrual_timestamp: now,
        };

        storage::set_deposit(&env, &depositor, deposit_id, &entry);
        storage::add_depositor(&env, &depositor);
        events::deposit(&env, &depositor, &token, amount, unlock_time, deposit_id);

        Ok(deposit_id)
    }

    pub fn deposit_with_delay(
        env: Env,
        depositor: Address,
        token: Address,
        amount: i128,
        unlock_time: u64,
        penalty_bps: u32,
        withdrawal_delay_secs: u64,
    ) -> Result<u32, VaultError> {
        depositor.require_auth();

        if storage::is_paused(&env) {
            return Err(VaultError::ContractPaused);
        }
        if amount <= 0 {
            return Err(VaultError::InvalidAmount);
        }
        let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
        if amount > max_deposit {
            return Err(VaultError::AmountTooLarge);
        }
        if penalty_bps > 10_000 {
            return Err(VaultError::InvalidPenaltyBps);
        }
        if penalty_bps > 0 && storage::get_fee_recipient(&env).is_none() {
            return Err(VaultError::MissingFeeRecipient);
        }

        let now = env.ledger().timestamp();
        if unlock_time <= now {
            return Err(VaultError::UnlockTimeNotInFuture);
        }
        let lock_duration = unlock_time.saturating_sub(now);
        let max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
        if lock_duration > max_lock {
            return Err(VaultError::LockDurationTooLong);
        }
        if lock_duration < MIN_LOCK_DURATION_SECS {
            return Err(VaultError::LockDurationTooShort);
        }

        let deposit_id = storage::next_deposit_id(&env, &depositor);
        token::Client::new(&env, &token).transfer(
            &depositor,
            &env.current_contract_address(),
            &amount,
        );

        let entry = VaultEntry {
            token: token.clone(),
            amount,
            unlock_time,
            depositor: depositor.clone(),
            penalty_bps,
            withdrawal_delay_secs,
        };
        storage::set_deposit(&env, &depositor, deposit_id, &entry);
        storage::add_depositor(&env, &depositor);
        events::deposit(&env, &depositor, &token, amount, unlock_time, deposit_id);

        Ok(deposit_id)
    }

    // ----------------------------------------------------------------
    //  Core: Deposit by Ledger Sequence
    // ----------------------------------------------------------------

    pub fn deposit_by_ledger(
        env: Env,
        depositor: Address,
        token: Address,
        amount: i128,
        unlock_ledger: u32,
        penalty_bps: u32,
    ) -> Result<u32, VaultError> {
        depositor.require_auth();

        if storage::is_paused(&env) {
            return Err(VaultError::ContractPaused);
        }

        if amount <= 0 {
            return Err(VaultError::InvalidAmount);
        }

        let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
        if amount > max_deposit {
            return Err(VaultError::AmountTooLarge);
        }

        if penalty_bps > 10_000 {
            return Err(VaultError::InvalidPenaltyBps);
        }

        if penalty_bps > 0 && storage::get_fee_recipient(&env).is_none() {
            return Err(VaultError::MissingFeeRecipient);
        }

        let current_ledger = env.ledger().sequence();
        if unlock_ledger <= current_ledger {
            return Err(VaultError::UnlockTimeNotInFuture);
        }

        let ledger_gap = unlock_ledger.saturating_sub(current_ledger);
        if ledger_gap < MIN_LOCK_LEDGERS {
            return Err(VaultError::LockDurationTooShort);
        }

        let deposit_id = storage::next_deposit_id(&env, &depositor);

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&depositor, &env.current_contract_address(), &amount);

        let entry = LedgerVaultEntry {
            token: token.clone(),
            amount,
            unlock_ledger,
            depositor: depositor.clone(),
            penalty_bps,
        };

        storage::set_deposit_by_ledger(&env, &depositor, deposit_id, &entry);
        storage::add_depositor(&env, &depositor);
        events::deposit_by_ledger(&env, &depositor, &token, amount, unlock_ledger, deposit_id);

        Ok(deposit_id)
    }

    // ----------------------------------------------------------------
    //  #330 — Multi-token Deposit
    // ----------------------------------------------------------------

    /// Deposit multiple tokens in a single vault entry.
    ///
    /// - `tokens_and_amounts`: list of `(token, amount)` pairs, length 1–MAX_TOKENS_PER_DEPOSIT.
    /// - Each individual `amount` is validated (> 0, ≤ max_deposit).
    /// - All tokens are transferred from `depositor` to the contract atomically.
    /// - Returns the new deposit ID (shared counter with single-token deposits).
    pub fn multi_deposit(
        env: Env,
        depositor: Address,
        tokens_and_amounts: Vec<TokenDeposit>,
        unlock_time: u64,
        penalty_bps: u32,
    ) -> Result<u32, VaultError> {
        depositor.require_auth();

        if storage::is_paused(&env) {
            return Err(VaultError::ContractPaused);
        }

        let count = tokens_and_amounts.len();
        if count == 0 {
            return Err(VaultError::EmptyTokenList);
        }
        if count > MAX_TOKENS_PER_DEPOSIT {
            return Err(VaultError::TooManyTokens);
        }

        if penalty_bps > 10_000 {
            return Err(VaultError::InvalidPenaltyBps);
        }

        if penalty_bps > 0 && storage::get_fee_recipient(&env).is_none() {
            return Err(VaultError::MissingFeeRecipient);
        }

        let now = env.ledger().timestamp();
        if unlock_time <= now {
            return Err(VaultError::UnlockTimeNotInFuture);
        }

        let max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
        let lock_duration: u64 = unlock_time.saturating_sub(now);
        if lock_duration > max_lock {
            return Err(VaultError::LockDurationTooLong);
        }
        if lock_duration < MIN_LOCK_DURATION_SECS {
            return Err(VaultError::LockDurationTooShort);
        }

        let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);

        // Validate all amounts before transferring anything (checks before effects).
        for td in tokens_and_amounts.iter() {
            if td.amount <= 0 {
                return Err(VaultError::InvalidAmount);
            }
            if td.amount > max_deposit {
                return Err(VaultError::AmountTooLarge);
            }
        }

        let deposit_id = storage::next_deposit_id(&env, &depositor);
        let contract = env.current_contract_address();

        // Transfer all tokens into the contract.
        for td in tokens_and_amounts.iter() {
            let token_client = token::Client::new(&env, &td.token);
            token_client.transfer(&depositor, &contract, &td.amount);
        }

        let entry = MultiTokenVaultEntry {
            tokens: tokens_and_amounts.clone(),
            unlock_time,
            depositor: depositor.clone(),
            penalty_bps,
            compound_frequency_secs: 0,
            last_accrual_timestamp: now,
        };

        storage::set_multi_deposit(&env, &depositor, deposit_id, &entry);
        storage::add_depositor(&env, &depositor);
        events::multi_deposit(&env, &depositor, count, unlock_time, deposit_id);

        Ok(deposit_id)
    }

    /// Query a multi-token vault entry.  Returns `None` if no multi-token deposit
    /// exists at the given `(depositor, deposit_id)`.
    pub fn get_multi_vault(
        env: Env,
        depositor: Address,
        deposit_id: u32,
    ) -> Option<MultiTokenVaultEntry> {
        storage::get_multi_deposit_readonly(&env, &depositor, deposit_id)
    }

    // ----------------------------------------------------------------
    //  #331 — Withdrawal Whitelist
    // ----------------------------------------------------------------

    /// Set (or replace) the withdrawal whitelist for an existing deposit.
    /// Only the depositor themselves may call this.
    ///
    /// - An empty `addresses` vec means "no restriction" (anyone may receive).
    /// - Calling this again replaces the previous whitelist entirely.
    pub fn set_withdrawal_whitelist(
        env: Env,
        depositor: Address,
        deposit_id: u32,
        addresses: Vec<Address>,
    ) -> Result<(), VaultError> {
        depositor.require_auth();

        // Verify the deposit exists (timestamp-based, ledger-based, or multi-token).
        let deposit_exists =
            storage::get_deposit_readonly(&env, &depositor, deposit_id).is_some()
                || storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id).is_some()
                || storage::get_multi_deposit_readonly(&env, &depositor, deposit_id).is_some();

        if !deposit_exists {
            return Err(VaultError::NoDepositFound);
        }

        storage::set_withdrawal_whitelist(&env, &depositor, deposit_id, &addresses);
        events::whitelist_set(&env, &depositor, deposit_id, &addresses);

        Ok(())
    }

    /// Return the current withdrawal whitelist for `(depositor, deposit_id)`.
    /// Returns `None` if no whitelist has been configured (= no restriction).
    pub fn get_withdrawal_whitelist(
        env: Env,
        depositor: Address,
        deposit_id: u32,
    ) -> Option<Vec<Address>> {
        storage::get_withdrawal_whitelist(&env, &depositor, deposit_id)
    }

    // ----------------------------------------------------------------
    //  #332 — Compound Interest Deposit
    // ----------------------------------------------------------------

    /// Like `deposit` but with an explicit compound-interest frequency.
    ///
    /// - `compound_frequency_secs`: interval between compounding events in seconds.
    ///   Must be ≥ 60 if non-zero; 0 disables compounding (equivalent to plain deposit).
    /// - Interest rate is fixed at `ANNUAL_INTEREST_BPS` (500 bps = 5% p.a.).
    pub fn deposit_with_compound_interest(
        env: Env,
        depositor: Address,
        token: Address,
        amount: i128,
        unlock_time: u64,
        penalty_bps: u32,
        compound_frequency_secs: u64,
    ) -> Result<u32, VaultError> {
        depositor.require_auth();

        if storage::is_paused(&env) {
            return Err(VaultError::ContractPaused);
        }

        if amount <= 0 {
            return Err(VaultError::InvalidAmount);
        }

        let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
        if amount > max_deposit {
            return Err(VaultError::AmountTooLarge);
        }

        if penalty_bps > 10_000 {
            return Err(VaultError::InvalidPenaltyBps);
        }

        if penalty_bps > 0 && storage::get_fee_recipient(&env).is_none() {
            return Err(VaultError::MissingFeeRecipient);
        }

        // Validate compound frequency: 0 = disabled; otherwise ≥ 60s.
        if compound_frequency_secs != 0 && compound_frequency_secs < MIN_COMPOUND_FREQUENCY_SECS {
            return Err(VaultError::InvalidCompoundFrequency);
        }

        let now = env.ledger().timestamp();
        if unlock_time <= now {
            return Err(VaultError::UnlockTimeNotInFuture);
        }

        let max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
        let lock_duration: u64 = unlock_time.saturating_sub(now);
        if lock_duration > max_lock {
            return Err(VaultError::LockDurationTooLong);
        }
        if lock_duration < MIN_LOCK_DURATION_SECS {
            return Err(VaultError::LockDurationTooShort);
        }

        let deposit_id = storage::next_deposit_id(&env, &depositor);

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&depositor, &env.current_contract_address(), &amount);

        let entry = VaultEntry {
            token: token.clone(),
            amount,
            unlock_time,
            depositor: depositor.clone(),
            penalty_bps,
            compound_frequency_secs,
            last_accrual_timestamp: now,
        };

        storage::set_deposit(&env, &depositor, deposit_id, &entry);
        storage::add_depositor(&env, &depositor);
        events::deposit(&env, &depositor, &token, amount, unlock_time, deposit_id);

        Ok(deposit_id)
    }

    /// Trigger compound-interest accrual for a deposit.
    ///
    /// Accrues all complete compound periods elapsed since `last_accrual_timestamp`
    /// and updates the stored `amount` and `last_accrual_timestamp` in-place.
    /// If fewer than one period has elapsed, this is a no-op (returns `Ok(false)`).
    ///
    /// Returns `Ok(true)` if accrual happened, `Ok(false)` if nothing changed,
    /// `Err(NoDepositFound)` if no deposit exists.
    pub fn update_accrual(
        env: Env,
        depositor: Address,
        deposit_id: u32,
    ) -> Result<bool, VaultError> {
        // No auth required — anyone can trigger accrual (it only benefits the depositor).

        let mut entry = storage::get_deposit(&env, &depositor, deposit_id)
            .ok_or(VaultError::NoDepositFound)?;

        if entry.compound_frequency_secs == 0 {
            // Compounding not enabled for this deposit.
            return Ok(false);
        }

        let now = env.ledger().timestamp();
        let new_amount =
            compute_accrued_amount(entry.amount, entry.compound_frequency_secs, entry.last_accrual_timestamp, now);

        if new_amount == entry.amount {
            return Ok(false);
        }

        let old_amount = entry.amount;

        // Advance last_accrual_timestamp by the number of complete periods that were applied.
        let elapsed = now.saturating_sub(entry.last_accrual_timestamp);
        let periods = elapsed / entry.compound_frequency_secs;
        entry.last_accrual_timestamp = entry
            .last_accrual_timestamp
            .saturating_add(periods.saturating_mul(entry.compound_frequency_secs));
        entry.amount = new_amount;

        storage::set_deposit(&env, &depositor, deposit_id, &entry);
        events::interest_accrued(&env, &depositor, deposit_id, old_amount, new_amount);

        Ok(true)
    }

    /// Returns the current balance of a deposit including any unaccrued compound
    /// interest (i.e. the value `withdraw` would release if called right now).
    ///
    /// This is a read-only query — it does NOT update storage.
    pub fn get_current_balance(
        env: Env,
        depositor: Address,
        deposit_id: u32,
    ) -> Option<i128> {
        let entry = storage::get_deposit_readonly(&env, &depositor, deposit_id)?;
        let now = env.ledger().timestamp();
        let balance = compute_accrued_amount(
            entry.amount,
            entry.compound_frequency_secs,
            entry.last_accrual_timestamp,
            now,
        );
        Some(balance)
    }

    // ----------------------------------------------------------------
    //  Core: Cancel Deposit (early exit with penalty)
    // ----------------------------------------------------------------

    pub fn cancel_deposit(env: Env, depositor: Address, deposit_id: u32) -> Result<(), VaultError> {
        depositor.require_auth();

        // Try timestamp-based deposit first (accrue interest before calculating refund).
        if let Some(mut entry) = storage::get_deposit(&env, &depositor, deposit_id) {
            let now = env.ledger().timestamp();
            if now >= entry.unlock_time {
                return Err(VaultError::VaultAlreadyUnlocked);
            }

            // Accrue interest before computing refund (so penalty is taken from current balance).
            if entry.compound_frequency_secs > 0 {
                let new_amount = compute_accrued_amount(
                    entry.amount,
                    entry.compound_frequency_secs,
                    entry.last_accrual_timestamp,
                    now,
                );
                entry.amount = new_amount;
            }

            storage::remove_deposit(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let token_client = token::Client::new(&env, &entry.token);
            let contract = env.current_contract_address();

            let penalty: i128 = (entry.amount * entry.penalty_bps as i128) / 10_000;
            let refund = entry.amount.saturating_sub(penalty);

            // Split penalty: fee_recipient gets FEE_RECIPIENT_PENALTY_BPS, stakers get STAKER_PENALTY_BPS
            let fee_recipient_share: i128 = (penalty * FEE_RECIPIENT_PENALTY_BPS as i128) / 10_000;
            let stakers_share: i128 = penalty - fee_recipient_share;

            if penalty > 0 {
                let fee_recipient =
                    storage::get_fee_recipient(&env).ok_or(VaultError::MissingFeeRecipient)?;
                if fee_recipient_share > 0 {
                    token_client.transfer(&contract, &fee_recipient, &fee_recipient_share);
                }
                // Add stakers_share to rewards pool
                if stakers_share > 0 {
                    let current_pool = storage::get_rewards_pool(&env);
                    storage::set_rewards_pool(&env, current_pool + stakers_share);
                }
            }
            if refund > 0 {
                token_client.transfer(&contract, &depositor, &refund);
            }

            events::penalty_split(&env, &depositor, penalty, fee_recipient_share, stakers_share, deposit_id);
            events::deposit_cancelled(&env, &depositor, &entry.token, entry.amount, penalty, deposit_id);
            return Ok(());
        }

        // Try ledger-based deposit.
        if let Some(entry) = storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id) {
            let current_ledger = env.ledger().sequence();
            if current_ledger >= entry.unlock_ledger {
                return Err(VaultError::VaultAlreadyUnlocked);
            }

            storage::remove_deposit_by_ledger(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let token_client = token::Client::new(&env, &entry.token);
            let contract = env.current_contract_address();

            let penalty: i128 = (entry.amount * entry.penalty_bps as i128) / 10_000;
            let refund = entry.amount.saturating_sub(penalty);

            // Split penalty: fee_recipient gets FEE_RECIPIENT_PENALTY_BPS, stakers get STAKER_PENALTY_BPS
            let fee_recipient_share: i128 = (penalty * FEE_RECIPIENT_PENALTY_BPS as i128) / 10_000;
            let stakers_share: i128 = penalty - fee_recipient_share;

            if penalty > 0 {
                let fee_recipient =
                    storage::get_fee_recipient(&env).ok_or(VaultError::MissingFeeRecipient)?;
                if fee_recipient_share > 0 {
                    token_client.transfer(&contract, &fee_recipient, &fee_recipient_share);
                }
                // Add stakers_share to rewards pool
                if stakers_share > 0 {
                    let current_pool = storage::get_rewards_pool(&env);
                    storage::set_rewards_pool(&env, current_pool + stakers_share);
                }
            }
            if refund > 0 {
                token_client.transfer(&contract, &depositor, &refund);
            }

            events::penalty_split(&env, &depositor, penalty, fee_recipient_share, stakers_share, deposit_id);
            events::deposit_cancelled(&env, &depositor, &entry.token, entry.amount, penalty, deposit_id);
            return Ok(());
        }

        // Try multi-token deposit.
        if let Some(entry) = storage::get_multi_deposit_readonly(&env, &depositor, deposit_id) {
            let now = env.ledger().timestamp();
            if now >= entry.unlock_time {
                return Err(VaultError::VaultAlreadyUnlocked);
            }
            let withdrawal_time = entry.unlock_time.saturating_add(entry.withdrawal_delay_secs);
            if now < withdrawal_time {
                return Err(VaultError::WithdrawalDelayActive);
            }

            storage::remove_multi_deposit(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let contract = env.current_contract_address();
            for td in entry.tokens.iter() {
                let penalty: i128 = (td.amount * entry.penalty_bps as i128) / 10_000;
                let refund = td.amount.saturating_sub(penalty);

                let token_client = token::Client::new(&env, &td.token);
                if penalty > 0 {
                    let fee_recipient =
                        storage::get_fee_recipient(&env).ok_or(VaultError::MissingFeeRecipient)?;
                    token_client.transfer(&contract, &fee_recipient, &penalty);
                }
                if refund > 0 {
                    token_client.transfer(&contract, &depositor, &refund);
                }
                events::deposit_cancelled(&env, &depositor, &td.token, td.amount, penalty, deposit_id);
            }
            return Ok(());
        }

        Err(VaultError::NoDepositFound)
    }

    // ----------------------------------------------------------------
    //  Staker Registry Functions
    // ----------------------------------------------------------------

    /// Register a staker with a stake amount. Updates their stake if already registered.
    pub fn register_staker(env: Env, staker: Address, amount: i128) -> Result<(), VaultError> {
        staker.require_auth();

        if amount <= 0 {
            return Err(VaultError::InvalidStakeAmount);
        }

        // Get the current total staked
        let current_total = storage::get_total_staked(&env);
        let old_stake = storage::get_staker(&env, &staker).unwrap_or(0);

        // Update staker's stake amount
        storage::set_staker(&env, &staker, amount);

        // Update total staked
        let new_total = current_total - old_stake + amount;
        storage::set_total_staked(&env, new_total);

        // Add to staker list if first-time registration
        if old_stake == 0 {
            storage::add_staker_to_list(&env, &staker);
        }

        events::staker_registered(&env, &staker, amount);
        Ok(())
    }

    /// Claim rewards for the caller. Calculates their share based on stake proportion.
    pub fn claim_staker_rewards(env: Env, staker: Address) -> Result<(), VaultError> {
        staker.require_auth();

        // Get staker's stake amount
        let stake_amount = storage::get_staker(&env, &staker)
            .ok_or(VaultError::StakerNotFound)?;

        if stake_amount <= 0 {
            return Err(VaultError::InvalidStakeAmount);
        }

        // Get current rewards pool and total staked
        let rewards_pool = storage::get_rewards_pool(&env);
        let total_staked = storage::get_total_staked(&env);

        if rewards_pool <= 0 {
            return Err(VaultError::NoRewardsToClaim);
        }

        if total_staked <= 0 {
            return Err(VaultError::NoRewardsToClaim);
        }

        // Calculate staker's proportional share
        // reward = (stake_amount / total_staked) * rewards_pool
        let reward = (stake_amount * rewards_pool) / total_staked;

        if reward <= 0 {
            return Err(VaultError::NoRewardsToClaim);
        }

        // Track total claimed
        let already_claimed = storage::get_staker_rewards_claimed(&env, &staker);
        let new_claimed = already_claimed + reward;
        storage::set_staker_rewards_claimed(&env, &staker, new_claimed);

        // Reduce rewards pool
        let new_pool = rewards_pool - reward;
        storage::set_rewards_pool(&env, new_pool);

        events::rewards_claimed(&env, &staker, reward);
        Ok(())
    }

    // ----------------------------------------------------------------
    //  Core: Withdraw
    // ----------------------------------------------------------------

    pub fn withdraw(env: Env, depositor: Address, deposit_id: u32) -> Result<(), VaultError> {
        depositor.require_auth();

        // Try timestamp-based deposit first.
        if let Some(mut entry) = storage::get_deposit_readonly(&env, &depositor, deposit_id) {
            let now = env.ledger().timestamp();
            if now < entry.unlock_time {
                return Err(VaultError::FundsStillLocked);
            }

            // Accrue any outstanding interest before computing the final payout.
            if entry.compound_frequency_secs > 0 {
                entry.amount = compute_accrued_amount(
                    entry.amount,
                    entry.compound_frequency_secs,
                    entry.last_accrual_timestamp,
                    now,
                );
            }

            storage::remove_deposit(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let token_client = token::Client::new(&env, &entry.token);
            token_client.transfer(&env.current_contract_address(), &depositor, &entry.amount);

            // Increment withdrawal count and clean up old epochs
            let current_epoch = storage::get_current_epoch(&env);
            storage::increment_withdrawal_count(&env, &depositor, current_epoch);
            storage::cleanup_old_epochs(&env, &depositor, current_epoch);

            events::withdraw(&env, &depositor, &entry.token, entry.amount, deposit_id);
            return Ok(());
        }

        // Try ledger-based deposit.
        if let Some(entry) = storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id) {
            let current_ledger = env.ledger().sequence();
            if current_ledger < entry.unlock_ledger {
                return Err(VaultError::FundsStillLocked);
            }

            storage::remove_deposit_by_ledger(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let token_client = token::Client::new(&env, &entry.token);
            token_client.transfer(&env.current_contract_address(), &depositor, &entry.amount);

            // Increment withdrawal count and clean up old epochs
            let current_epoch = storage::get_current_epoch(&env);
            storage::increment_withdrawal_count(&env, &depositor, current_epoch);
            storage::cleanup_old_epochs(&env, &depositor, current_epoch);

            events::withdraw(&env, &depositor, &entry.token, entry.amount, deposit_id);
            return Ok(());
        }

        // Try multi-token deposit.
        if let Some(entry) = storage::get_multi_deposit_readonly(&env, &depositor, deposit_id) {
            let now = env.ledger().timestamp();
            if now < entry.unlock_time {
                return Err(VaultError::FundsStillLocked);
            }

            storage::remove_multi_deposit(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let contract = env.current_contract_address();
            let token_count = entry.tokens.len();
            for td in entry.tokens.iter() {
                let token_client = token::Client::new(&env, &td.token);
                token_client.transfer(&contract, &depositor, &td.amount);
                events::withdraw(&env, &depositor, &td.token, td.amount, deposit_id);
            }
            events::multi_withdraw(&env, &depositor, &depositor, deposit_id, token_count);
            return Ok(());
        }

        Err(VaultError::NoDepositFound)
    }

    /// Withdraw to a specific recipient address.
    ///
    /// If a whitelist has been set for this deposit, `recipient` must be in it.
    pub fn withdraw_to(
        env: Env,
        depositor: Address,
        deposit_id: u32,
        recipient: Address,
    ) -> Result<(), VaultError> {
        depositor.require_auth();

        // Try timestamp-based deposit first.
        if let Some(mut entry) = storage::get_deposit_readonly(&env, &depositor, deposit_id) {
            let now = env.ledger().timestamp();
            if now < entry.unlock_time {
                return Err(VaultError::FundsStillLocked);
            }
            let withdrawal_time = entry.unlock_time.saturating_add(entry.withdrawal_delay_secs);
            if now < withdrawal_time {
                return Err(VaultError::WithdrawalDelayActive);
            }

            // #331: enforce whitelist.
            check_whitelist(&env, &depositor, deposit_id, &recipient)?;

            // Accrue interest before payout.
            if entry.compound_frequency_secs > 0 {
                entry.amount = compute_accrued_amount(
                    entry.amount,
                    entry.compound_frequency_secs,
                    entry.last_accrual_timestamp,
                    now,
                );
            }

            storage::remove_deposit(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let token_client = token::Client::new(&env, &entry.token);
            token_client.transfer(&env.current_contract_address(), &recipient, &entry.amount);

            // Increment withdrawal count and clean up old epochs
            let current_epoch = storage::get_current_epoch(&env);
            storage::increment_withdrawal_count(&env, &depositor, current_epoch);
            storage::cleanup_old_epochs(&env, &depositor, current_epoch);

            events::withdraw_to(&env, &depositor, &recipient, &entry.token, entry.amount);
            return Ok(());
        }

        // Try ledger-based deposit.
        if let Some(entry) = storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id) {
            let current_ledger = env.ledger().sequence();
            if current_ledger < entry.unlock_ledger {
                return Err(VaultError::FundsStillLocked);
            }

            // #331: enforce whitelist.
            check_whitelist(&env, &depositor, deposit_id, &recipient)?;

            storage::remove_deposit_by_ledger(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let token_client = token::Client::new(&env, &entry.token);
            token_client.transfer(&env.current_contract_address(), &recipient, &entry.amount);

            // Increment withdrawal count and clean up old epochs
            let current_epoch = storage::get_current_epoch(&env);
            storage::increment_withdrawal_count(&env, &depositor, current_epoch);
            storage::cleanup_old_epochs(&env, &depositor, current_epoch);

            events::withdraw_to(&env, &depositor, &recipient, &entry.token, entry.amount);
            return Ok(());
        }

        // Try multi-token deposit.
        if let Some(entry) = storage::get_multi_deposit_readonly(&env, &depositor, deposit_id) {
            let now = env.ledger().timestamp();
            if now < entry.unlock_time {
                return Err(VaultError::FundsStillLocked);
            }

            // #331: enforce whitelist.
            check_whitelist(&env, &depositor, deposit_id, &recipient)?;

            storage::remove_multi_deposit(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let contract = env.current_contract_address();
            let token_count = entry.tokens.len();
            for td in entry.tokens.iter() {
                let token_client = token::Client::new(&env, &td.token);
                token_client.transfer(&contract, &recipient, &td.amount);
                events::withdraw_to(&env, &depositor, &recipient, &td.token, td.amount);
            }
            events::multi_withdraw(&env, &depositor, &recipient, deposit_id, token_count);
            return Ok(());
        }

        Err(VaultError::NoDepositFound)
    }

    // ----------------------------------------------------------------
    //  Admin: Emergency Withdrawal
    // ----------------------------------------------------------------

    pub fn emergency_withdraw(
        env: Env,
        admin: Address,
        depositor: Address,
        deposit_id: u32,
    ) -> Result<(), VaultError> {
        admin.require_auth();
        storage::require_admin(&env, &admin)?;

        // Try timestamp-based deposit first.
        if let Some(entry) = storage::get_deposit_readonly(&env, &depositor, deposit_id) {
            storage::remove_deposit(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let token_client = token::Client::new(&env, &entry.token);
            token_client.transfer(&env.current_contract_address(), &depositor, &entry.amount);

            events::emergency_withdraw(
                &env,
                &admin,
                &depositor,
                &entry.token,
                entry.amount,
                deposit_id,
            );
            return Ok(());
        }

        // Try ledger-based deposit.
        if let Some(entry) = storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id) {
            storage::remove_deposit_by_ledger(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let token_client = token::Client::new(&env, &entry.token);
            token_client.transfer(&env.current_contract_address(), &depositor, &entry.amount);

            events::emergency_withdraw(
                &env,
                &admin,
                &depositor,
                &entry.token,
                entry.amount,
                deposit_id,
            );
            return Ok(());
        }

        // Try multi-token deposit.
        if let Some(entry) = storage::get_multi_deposit_readonly(&env, &depositor, deposit_id) {
            storage::remove_multi_deposit(&env, &depositor, deposit_id);
            storage::remove_withdrawal_whitelist(&env, &depositor, deposit_id);
            if storage::get_deposit_ids(&env, &depositor).len() == 0 {
                storage::remove_depositor(&env, &depositor);
            }

            let contract = env.current_contract_address();
            for td in entry.tokens.iter() {
                let token_client = token::Client::new(&env, &td.token);
                token_client.transfer(&contract, &depositor, &td.amount);
                events::emergency_withdraw(&env, &admin, &depositor, &td.token, td.amount, deposit_id);
            }
            return Ok(());
        }

        Err(VaultError::NoDepositFound)
    }

    // ----------------------------------------------------------------
    //  Archival: Archive and Delete Deposits
    // ----------------------------------------------------------------

    /// Archive a completed (withdrawn or cancelled) timestamp-based deposit for record-keeping.
    /// The depositor manually archives a deposit entry that has been withdrawn or cancelled,
    /// storing it with the current archive timestamp.
    ///
    /// # Preconditions
    /// - Deposit must NOT exist in active storage (must have been withdrawn or cancelled already)
    /// - Caller must provide the original deposit entry data
    ///
    /// # Parameters
    /// - `depositor`: The original deposit owner
    /// - `deposit_id`: The ID of the deposit to archive
    /// - `token`: Token address (must match the original deposit)
    /// - `amount`: Amount (must match the original deposit)
    /// - `unlock_time`: Unlock time (must match the original deposit)
    /// - `penalty_bps`: Penalty basis points (must match the original deposit)
    ///
    /// # Returns
    /// - `Ok(())` on success
    /// - `Err(VaultError::NoDepositFound)` if active deposit still exists
    /// - `Err(VaultError::NoArchivedDepositFound)` if archived deposit already exists
    pub fn archive_deposit(
        env: Env,
        depositor: Address,
        deposit_id: u32,
        token: Address,
        amount: i128,
        unlock_time: u64,
        penalty_bps: u32,
    ) -> Result<(), VaultError> {
        depositor.require_auth();

        // Verify the deposit is NOT in active storage
        if storage::get_deposit_readonly(&env, &depositor, deposit_id).is_some() {
            return Err(VaultError::NoDepositFound);
        }

        if storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id).is_some() {
            return Err(VaultError::NoDepositFound);
        }

        // Verify not already archived
        if storage::get_archived_deposit_readonly(&env, &depositor, deposit_id).is_some() {
            return Err(VaultError::NoArchivedDepositFound);
        }

        let now = env.ledger().timestamp();
        let entry = VaultEntry {
            token: token.clone(),
            amount,
            unlock_time,
            depositor: depositor.clone(),
            penalty_bps,
        };

        storage::set_archived_deposit(&env, &depositor, deposit_id, &entry, now);
        events::deposit_archived(&env, &depositor, &token, amount, deposit_id);
        Ok(())
    }

    /// Archive a completed (withdrawn or cancelled) ledger-based deposit for record-keeping.
    /// The depositor manually archives a ledger-based deposit entry that has been withdrawn or cancelled,
    /// storing it with the current archive timestamp.
    ///
    /// # Preconditions
    /// - Deposit must NOT exist in active storage (must have been withdrawn or cancelled already)
    /// - Caller must provide the original deposit entry data
    ///
    /// # Parameters
    /// - `depositor`: The original deposit owner
    /// - `deposit_id`: The ID of the deposit to archive
    /// - `token`: Token address (must match the original deposit)
    /// - `amount`: Amount (must match the original deposit)
    /// - `unlock_ledger`: Unlock ledger (must match the original deposit)
    /// - `penalty_bps`: Penalty basis points (must match the original deposit)
    ///
    /// # Returns
    /// - `Ok(())` on success
    /// - `Err(VaultError::NoDepositFound)` if active deposit still exists
    /// - `Err(VaultError::NoArchivedDepositFound)` if archived deposit already exists
    pub fn archive_deposit_by_ledger(
        env: Env,
        depositor: Address,
        deposit_id: u32,
        token: Address,
        amount: i128,
        unlock_ledger: u32,
        penalty_bps: u32,
    ) -> Result<(), VaultError> {
        depositor.require_auth();

        // Verify the deposit is NOT in active storage
        if storage::get_deposit_readonly(&env, &depositor, deposit_id).is_some() {
            return Err(VaultError::NoDepositFound);
        }

        if storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id).is_some() {
            return Err(VaultError::NoDepositFound);
        }

        // Verify not already archived
        if storage::get_archived_deposit_by_ledger_readonly(&env, &depositor, deposit_id).is_some() {
            return Err(VaultError::NoArchivedDepositFound);
        }

        let now = env.ledger().timestamp();
        let entry = LedgerVaultEntry {
            token: token.clone(),
            amount,
            unlock_ledger,
            depositor: depositor.clone(),
            penalty_bps,
        };

        storage::set_archived_deposit_by_ledger(&env, &depositor, deposit_id, &entry, now);
        events::deposit_archived_by_ledger(&env, &depositor, &token, amount, deposit_id);
        Ok(())
    }

    /// Delete an archived deposit that is old enough (>= 1 year).
    /// Only deposits archived more than 1 year ago can be deleted.
    ///
    /// # Preconditions
    /// - Archived deposit must exist
    /// - Deposit must be at least 1 year old
    ///
    /// # Parameters
    /// - `depositor`: The original deposit owner
    /// - `deposit_id`: The ID of the archived deposit to delete
    ///
    /// # Returns
    /// - `Ok(())` on success (deposit deleted)
    /// - `Err(VaultError::NoArchivedDepositFound)` if no archived deposit exists
    /// - `Err(VaultError::ArchivedDepositTooYoung)` if deposit is less than 1 year old
    pub fn delete_archived_deposit(
        env: Env,
        depositor: Address,
        deposit_id: u32,
    ) -> Result<(), VaultError> {
        depositor.require_auth();

        let now = env.ledger().timestamp();

        // Check timestamp-based archived deposit
        if let Some(archived) = storage::get_archived_deposit_readonly(&env, &depositor, deposit_id) {
            let age = now.saturating_sub(archived.archive_timestamp);
            if age < ARCHIVED_DEPOSIT_MIN_AGE_SECS {
                return Err(VaultError::ArchivedDepositTooYoung);
            }
            storage::remove_archived_deposit(&env, &depositor, deposit_id);
            return Ok(());
        }

        // Check ledger-based archived deposit
        if let Some(archived) = storage::get_archived_deposit_by_ledger_readonly(&env, &depositor, deposit_id) {
            let age = now.saturating_sub(archived.archive_timestamp);
            if age < ARCHIVED_DEPOSIT_MIN_AGE_SECS {
                return Err(VaultError::ArchivedDepositTooYoung);
            }
            storage::remove_archived_deposit_by_ledger(&env, &depositor, deposit_id);
            return Ok(());
        }

        Err(VaultError::NoArchivedDepositFound)
    }

    // ----------------------------------------------------------------
    //  Admin: Pause / Unpause
    // ----------------------------------------------------------------

    pub fn pause(env: Env, admin: Address) -> Result<(), VaultError> {
        admin.require_auth();
        let stored_admin = storage::get_admin(&env).ok_or(VaultError::Unauthorized)?;
        if admin != stored_admin {
            return Err(VaultError::Unauthorized);
        }
        storage::set_paused(&env, true);
        events::paused(&env, &admin);
        Ok(())
    }

    pub fn unpause(env: Env, admin: Address) -> Result<(), VaultError> {
        admin.require_auth();
        let stored_admin = storage::get_admin(&env).ok_or(VaultError::Unauthorized)?;
        if admin != stored_admin {
            return Err(VaultError::Unauthorized);
        }
        storage::set_paused(&env, false);
        events::unpaused(&env, &admin);
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        storage::is_paused(&env)
    }

    // ----------------------------------------------------------------
    //  Admin: Two-Step Admin Transfer
    // ----------------------------------------------------------------

    pub fn transfer_admin(env: Env, admin: Address, new_admin: Address) -> Result<(), VaultError> {
        admin.require_auth();
        let stored_admin = storage::get_admin(&env).ok_or(VaultError::Unauthorized)?;
        if admin != stored_admin {
            return Err(VaultError::Unauthorized);
        }

        if new_admin == stored_admin {
            return Err(VaultError::InvalidAdmin);
        }

        storage::set_pending_admin(&env, &new_admin);
        events::admin_transfer_initiated(&env, &admin, &new_admin);
        Ok(())
    }

    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), VaultError> {
        new_admin.require_auth();

        let pending_admin = storage::get_pending_admin(&env).ok_or(VaultError::Unauthorized)?;
        if new_admin != pending_admin {
            return Err(VaultError::Unauthorized);
        }

        storage::set_admin(&env, &new_admin);
        storage::remove_pending_admin(&env);
        events::admin_transfer_accepted(&env, &new_admin);
        Ok(())
    }

    pub fn cancel_transfer_admin(env: Env, admin: Address) -> Result<(), VaultError> {
        admin.require_auth();

        let stored_admin = storage::get_admin(&env).ok_or(VaultError::Unauthorized)?;
        if admin != stored_admin {
            return Err(VaultError::Unauthorized);
        }

        if let Some(pending) = storage::get_pending_admin(&env) {
            storage::remove_pending_admin(&env);
            events::admin_transfer_cancelled(&env, &admin, &pending);
        }
        Ok(())
    }

    pub fn renounce_admin(env: Env, admin: Address) -> Result<(), VaultError> {
        admin.require_auth();

        let stored_admin = storage::get_admin(&env).ok_or(VaultError::Unauthorized)?;
        if admin != stored_admin {
            return Err(VaultError::Unauthorized);
        }

        storage::remove_admin(&env);
        storage::remove_pending_admin(&env);
        events::admin_renounced(&env, &admin);
        Ok(())
    }

    // ----------------------------------------------------------------
    //  Read-only Queries
    // ----------------------------------------------------------------

    pub fn get_vault(env: Env, depositor: Address, deposit_id: u32) -> Option<VaultEntry> {
        storage::get_deposit_readonly(&env, &depositor, deposit_id)
    }

    pub fn get_ledger_vault(env: Env, depositor: Address, deposit_id: u32) -> Option<LedgerVaultEntry> {
        storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id)
    }

    /// Returns whether a deposit is timestamp-based, ledger-based, or multi-token.
    /// Returns `None` if no deposit exists at the given `(depositor, deposit_id)`.
    pub fn get_deposit_type(env: Env, depositor: Address, deposit_id: u32) -> Option<DepositType> {
        if storage::get_deposit_readonly(&env, &depositor, deposit_id).is_some() {
            return Some(DepositType::TimeBased);
        }
        if storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id).is_some() {
            return Some(DepositType::LedgerBased);
        }
        if storage::get_multi_deposit_readonly(&env, &depositor, deposit_id).is_some() {
            return Some(DepositType::MultiToken);
        }
        None
    }

    /// Fetch a single deposit entry for each `(depositor, deposit_id)` pair.
    /// Clamped to `MAX_BATCH_SIZE` (25) entries per call.
    pub fn get_vault_batch(
        env: Env,
        depositors: Vec<Address>,
        deposit_id: u32,
    ) -> Vec<Option<VaultEntry>> {
        let limit = if depositors.len() > MAX_BATCH_SIZE {
            MAX_BATCH_SIZE
        } else {
            depositors.len()
        };
        let mut results = Vec::new(&env);
        for i in 0..limit {
            if let Some(depositor) = depositors.get(i) {
                let entry = storage::get_deposit_readonly(&env, &depositor, deposit_id);
                results.push_back(entry);
            }
        }
        results
    }

    /// Fetch multiple deposits for a single depositor in one RPC call.
    /// Limit: up to 25 deposit IDs per call.
    /// Returns `Vec` of `(deposit_id, Option<VaultEntry>)` pairs.
    pub fn get_deposit_batch(
        env: Env,
        depositor: Address,
        deposit_ids: Vec<u32>,
    ) -> Vec<(u32, Option<VaultEntry>)> {
        let limit = if deposit_ids.len() > MAX_BATCH_SIZE {
            MAX_BATCH_SIZE
        } else {
            deposit_ids.len()
        };
        let mut results = Vec::new(&env);
        for i in 0..limit {
            if let Some(id) = deposit_ids.get(i) {
                let entry = storage::get_deposit_readonly(&env, &depositor, id);
                results.push_back((id, entry));
            }
        }
        results
    }

    pub fn get_deposit_ids(env: Env, depositor: Address) -> Vec<u32> {
        storage::get_deposit_ids(&env, &depositor)
    }

    pub fn get_time(env: Env) -> u64 {
        env.ledger().timestamp()
    }

    /// Returns time remaining for a deposit.
    /// For timestamp-based deposits: exact seconds remaining.
    /// For ledger-based deposits: estimated seconds (remaining_ledgers × 5).
    /// For multi-token deposits: exact seconds remaining.
    /// Returns 0 when unlocked or not found.
    pub fn time_remaining(env: Env, depositor: Address, deposit_id: u32) -> u64 {
        // Timestamp-based path.
        if let Some(entry) = storage::get_deposit_readonly(&env, &depositor, deposit_id) {
            let now = env.ledger().timestamp();
            return entry.unlock_time.saturating_sub(now);
        }

        // Ledger-based path.
        if let Some(entry) = storage::get_deposit_by_ledger_readonly(&env, &depositor, deposit_id) {
            let current = env.ledger().sequence();
            if current >= entry.unlock_ledger {
                return 0;
            }
            let remaining_ledgers = (entry.unlock_ledger - current) as u64;
            return remaining_ledgers.saturating_mul(storage::LEDGER_SECONDS);
        }

        // Multi-token path.
        if let Some(entry) = storage::get_multi_deposit_readonly(&env, &depositor, deposit_id) {
            let now = env.ledger().timestamp();
            return entry.unlock_time.saturating_sub(now);
        }

        0
    }

    pub fn time_to_withdrawal(env: Env, depositor: Address, deposit_id: u32) -> u64 {
        if let Some(entry) = storage::get_deposit_readonly(&env, &depositor, deposit_id) {
            return entry
                .unlock_time
                .saturating_add(entry.withdrawal_delay_secs)
                .saturating_sub(env.ledger().timestamp());
        }
        0
    }

    pub fn get_accrued_interest(env: Env, depositor: Address, deposit_id: u32) -> i128 {
        storage::get_deposit_readonly(&env, &depositor, deposit_id)
            .and_then(|entry| calculate_interest(&entry, env.ledger().timestamp()).ok())
            .unwrap_or(0)
    }

    pub fn get_admin(env: Env) -> Option<Address> {
        storage::get_admin(&env)
    }

    pub fn get_pending_admin(env: Env) -> Option<Address> {
        storage::get_pending_admin(&env)
    }

    pub fn get_constants(env: Env) -> (i128, u64) {
        let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
        let max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
        (max_deposit, max_lock)
    }

    pub fn get_fee_recipient(env: Env) -> Option<Address> {
        storage::get_fee_recipient(&env)
    }

    pub fn get_depositor_count(env: Env) -> u32 {
        storage::get_depositor_count(&env)
    }

    pub fn get_depositors(env: Env, offset: u32, limit: u32) -> Page {
        let (items, total_count) = storage::get_depositors_page(&env, offset, limit);
        Page { items, total_count }
    }

    pub fn is_initialized(env: Env) -> bool {
        storage::is_initialized(&env)
    }

    pub fn version(_env: Env) -> soroban_sdk::String {
        soroban_sdk::String::from_slice(&_env, env!("CARGO_PKG_VERSION"))
    }

    /// Admin-readable query: returns the cumulative emergency withdrawal amount
    /// for a specific ledger sequence number.
    /// 
    /// This allows admins to audit emergency withdrawal activity and ensure
    /// the per-ledger limit is being respected. Returns 0 if no emergency
    /// withdrawals have occurred in that ledger.
    pub fn get_emergency_withdrawal_total(env: Env, ledger: u32) -> i128 {
        storage::get_emergency_withdrawal_per_ledger(&env, ledger)
    }

    /// Returns the current balance of the staker rewards pool.
    /// 
    /// The rewards pool accumulates 70% of penalties from early deposit cancellations.
    /// Stakers can claim their proportional share based on their registered stake.
    /// 
    /// This is a read-only query that does not modify contract state.
    pub fn get_rewards_pool_total(env: Env) -> i128 {
        storage::get_rewards_pool(&env)
    }

    // ----------------------------------------------------------------
    //  Read-only: Paginated flat deposits view
    // ----------------------------------------------------------------

    pub fn get_deposits_page(
        env: Env,
        offset: u32,
        limit: u32,
    ) -> Vec<(Address, u32, VaultEntry)> {
        let mut results: Vec<(Address, u32, VaultEntry)> = Vec::new(&env);
        let mut global_index: u32 = 0;
        let end_at = offset.saturating_add(limit);

        let depositor_list = storage::get_all_depositors_raw(&env);
        for depositor in depositor_list.iter() {
            if global_index >= end_at {
                break;
            }
            if !storage::depositor_is_active(&env, &depositor) {
                continue;
            }
            let ids = storage::get_deposit_ids(&env, &depositor);
            for id in ids.iter() {
                if global_index >= end_at {
                    break;
                }
                if let Some(entry) = storage::get_deposit_readonly(&env, &depositor, id) {
                    if global_index >= offset {
                        results.push_back((depositor.clone(), id, entry));
                    }
                    global_index = global_index.saturating_add(1);
                }
            }
        }
        results
    }

    // ----------------------------------------------------------------
    //  Admin: Storage migration
    // ----------------------------------------------------------------

    pub fn get_storage_version(env: Env) -> Option<u32> {
        storage::get_storage_version(&env)
    }

    pub fn migrate(env: Env, admin: Address) -> Result<bool, VaultError> {
        admin.require_auth();
        storage::require_admin(&env, &admin)?;

        let current_version = storage::get_storage_version(&env).unwrap_or(0);

        if current_version >= STORAGE_VERSION {
            return Ok(false);
        }

        storage::set_storage_version(&env, STORAGE_VERSION);
        Ok(true)
    }

    // ----------------------------------------------------------------
    //  Issue #333: Recurring deposit subscriptions
    // ----------------------------------------------------------------

    /// Creates a recurring deposit subscription.
    ///
    /// The depositor commits to having `total_count` individual deposits of
    /// `amount` tokens created at intervals of `interval_secs` seconds.  The
    /// first execution is immediately due (caller can invoke `execute_subscription`
    /// right after creation, or wait `interval_secs`).
    ///
    /// # Parameters
    /// - `depositor`         — account that will fund every deposit tick; must sign.
    /// - `token`             — SAC token to lock.
    /// - `amount`            — amount per deposit tick (> 0, ≤ `max_deposit`).
    /// - `interval_secs`     — seconds between ticks (> 0).
    /// - `total_count`       — total number of ticks (> 0).
    /// - `lock_duration_secs`— lock duration per individual deposit (≥ `MIN_LOCK_DURATION_SECS`).
    /// - `penalty_bps`       — early-exit penalty for each produced deposit (0–10000).
    ///
    /// # Returns
    /// The new `subscription_id`.
    pub fn create_subscription(
        env: Env,
        depositor: Address,
        token: Address,
        amount: i128,
        interval_secs: u64,
        total_count: u32,
        lock_duration_secs: u64,
        penalty_bps: u32,
    ) -> Result<u32, VaultError> {
        depositor.require_auth();

        if storage::is_paused(&env) {
            return Err(VaultError::ContractPaused);
        }

        // Validate params
        if amount <= 0 {
            return Err(VaultError::InvalidAmount);
        }
        let max_deposit = storage::get_max_deposit(&env).unwrap_or(MAX_DEPOSIT_AMOUNT);
        if amount > max_deposit {
            return Err(VaultError::AmountTooLarge);
        }
        if interval_secs == 0 || total_count == 0 {
            return Err(VaultError::InvalidSubscriptionParams);
        }
        if lock_duration_secs < MIN_LOCK_DURATION_SECS {
            return Err(VaultError::LockDurationTooShort);
        }
        let max_lock = storage::get_max_lock_secs(&env).unwrap_or(MAX_LOCK_DURATION_SECS);
        if lock_duration_secs > max_lock {
            return Err(VaultError::LockDurationTooLong);
        }
        if penalty_bps > 10_000 {
            return Err(VaultError::InvalidPenaltyBps);
        }
        if penalty_bps > 0 && storage::get_fee_recipient(&env).is_none() {
            return Err(VaultError::MissingFeeRecipient);
        }

        let sub_id = storage::next_subscription_id(&env, &depositor);
        let now = env.ledger().timestamp();

        let sub = RecurringDeposit {
            depositor: depositor.clone(),
            token: token.clone(),
            amount,
            interval_secs,
            total_count,
            executed_count: 0,
            lock_duration_secs,
            penalty_bps,
            // First execution is due immediately.
            next_execution_time: now,
            cancelled: false,
        };

        storage::set_subscription(&env, &depositor, sub_id, &sub);
        events::subscription_created(
            &env,
            &depositor,
            &token,
            sub_id,
            amount,
            interval_secs,
            total_count,
        );

        Ok(sub_id)
    }

    /// Cancels an active subscription.
    ///
    /// Only the depositor who created the subscription may cancel it.
    /// Already-executed deposits are unaffected — they remain locked until
    /// their individual `unlock_time`.
    pub fn cancel_subscription(
        env: Env,
        depositor: Address,
        sub_id: u32,
    ) -> Result<(), VaultError> {
        depositor.require_auth();

        let mut sub = storage::get_subscription(&env, &depositor, sub_id)
            .ok_or(VaultError::NoSubscriptionFound)?;

        if sub.cancelled {
            return Err(VaultError::SubscriptionCancelled);
        }
        if sub.executed_count >= sub.total_count {
            return Err(VaultError::SubscriptionCompleted);
        }

        sub.cancelled = true;
        storage::set_subscription(&env, &depositor, sub_id, &sub);

        events::subscription_cancelled(&env, &depositor, sub_id, sub.executed_count);
        Ok(())
    }

    /// Executes the next tick of a recurring deposit subscription.
    ///
    /// This is a *permissionless* function — anyone may call it as long as the
    /// subscription is active and the interval has elapsed.  This mirrors the
    /// "oracle-triggered" model described in issue #333: an off-chain keeper
    /// (or the depositor themselves) triggers execution at the right time.
    ///
    /// On each call the contract:
    /// 1. Validates the subscription is not cancelled / completed.
    /// 2. Checks `now >= next_execution_time`.
    /// 3. Transfers `amount` tokens from `depositor` to the contract.
    /// 4. Creates a `VaultEntry` locked for `lock_duration_secs` from `now`.
    /// 5. Updates `executed_count` and `next_execution_time`.
    ///
    /// # Returns
    /// The new `deposit_id` created for this tick.
    pub fn execute_subscription(
        env: Env,
        depositor: Address,
        sub_id: u32,
    ) -> Result<u32, VaultError> {
        // No auth required — permissionless execution by any caller (keeper/oracle).

        if storage::is_paused(&env) {
            return Err(VaultError::ContractPaused);
        }

        let mut sub = storage::get_subscription(&env, &depositor, sub_id)
            .ok_or(VaultError::NoSubscriptionFound)?;

        if sub.cancelled {
            return Err(VaultError::SubscriptionCancelled);
        }
        if sub.executed_count >= sub.total_count {
            return Err(VaultError::SubscriptionCompleted);
        }

        let now = env.ledger().timestamp();
        if now < sub.next_execution_time {
            return Err(VaultError::SubscriptionNotDue);
        }

        // Transfer tokens from depositor into the contract.
        let token_client = token::Client::new(&env, &sub.token);
        token_client.transfer(&sub.depositor, &env.current_contract_address(), &sub.amount);

        // Create a locked VaultEntry for this tick.
        let unlock_time = now.saturating_add(sub.lock_duration_secs);
        let deposit_id = storage::next_deposit_id(&env, &depositor);
        let entry = VaultEntry {
            token: sub.token.clone(),
            amount: sub.amount,
            unlock_time,
            depositor: depositor.clone(),
            penalty_bps: sub.penalty_bps,
        };
        storage::set_deposit(&env, &depositor, deposit_id, &entry);
        storage::add_depositor(&env, &depositor);

        // Advance subscription state.
        sub.executed_count = sub.executed_count.saturating_add(1);
        sub.next_execution_time = now.saturating_add(sub.interval_secs);
        storage::set_subscription(&env, &depositor, sub_id, &sub);

        events::subscription_executed(
            &env,
            &depositor,
            &sub.token,
            sub_id,
            deposit_id,
            sub.executed_count,
        );
        events::deposit(&env, &depositor, &sub.token, sub.amount, unlock_time, deposit_id);

        Ok(deposit_id)
    }

    /// Returns the `RecurringDeposit` struct for the given `(depositor, sub_id)`,
    /// or `None` if not found.
    pub fn get_subscription(
        env: Env,
        depositor: Address,
        sub_id: u32,
    ) -> Option<RecurringDeposit> {
        storage::get_subscription_readonly(&env, &depositor, sub_id)
    }

    /// Returns all subscription IDs ever created for `depositor` (includes
    /// cancelled and completed ones — filter by `cancelled` / `executed_count`).
    pub fn get_subscription_ids(env: Env, depositor: Address) -> Vec<u32> {
        storage::get_subscription_ids(&env, &depositor)
    }

    // ----------------------------------------------------------------
    //  Issue #334: Deposit insurance pool
    // ----------------------------------------------------------------

    /// Returns the current insurance pool balance for `token`.
    pub fn get_insurance_pool_balance(env: Env, token: Address) -> i128 {
        storage::get_insurance_pool_balance(&env, &token)
    }

    /// Files an insurance claim against the pool for a specific token.
    ///
    /// The claimant provides free-form `incident_evidence` (e.g. a description
    /// of what happened or an off-chain transaction hash).  The admin reviews
    /// and either approves or denies via `approve_claim` / `deny_claim`.
    ///
    /// There is no restriction on who can file a claim — any address may submit
    /// evidence.  Restricting claims to depositors only would require iterating
    /// every deposit, which is budget-unsafe.
    ///
    /// # Returns
    /// A new `claim_id`.
    pub fn claim_insurance(
        env: Env,
        claimant: Address,
        token: Address,
        amount_requested: i128,
        incident_evidence: String,
    ) -> Result<u32, VaultError> {
        claimant.require_auth();

        if amount_requested <= 0 {
            return Err(VaultError::InvalidAmount);
        }

        let claim_id = storage::next_claim_id(&env);
        let claim = InsuranceClaim {
            claim_id,
            claimant: claimant.clone(),
            token: token.clone(),
            amount_requested,
            incident_evidence,
            status: ClaimStatus::Pending,
        };

        storage::set_claim(&env, claim_id, &claim);
        events::insurance_claim_filed(&env, &claimant, &token, claim_id, amount_requested);

        Ok(claim_id)
    }

    /// Admin: approve an insurance claim and disburse funds from the pool.
    ///
    /// The full `amount_requested` is paid out from the pool balance for
    /// `claim.token`.  Returns `InsufficientInsurancePool` if the pool
    /// balance is lower than the requested amount.
    ///
    /// # Returns
    /// The amount disbursed.
    pub fn approve_claim(env: Env, admin: Address, claim_id: u32) -> Result<i128, VaultError> {
        admin.require_auth();
        storage::require_admin(&env, &admin)?;

        let mut claim = storage::get_claim(&env, claim_id).ok_or(VaultError::NoClaimFound)?;

        match claim.status {
            ClaimStatus::Approved | ClaimStatus::Denied => {
                return Err(VaultError::ClaimAlreadyResolved)
            }
            ClaimStatus::Pending => {}
        }

        // Deduct from pool balance first (checks-effects-interactions).
        storage::deduct_insurance_pool_balance(&env, &claim.token, claim.amount_requested)?;

        // Update claim status.
        claim.status = ClaimStatus::Approved;
        storage::set_claim(&env, claim_id, &claim);

        // Transfer tokens from contract to claimant.
        let token_client = token::Client::new(&env, &claim.token);
        token_client.transfer(
            &env.current_contract_address(),
            &claim.claimant,
            &claim.amount_requested,
        );

        events::insurance_claim_approved(
            &env,
            &admin,
            &claim.claimant,
            claim_id,
            claim.amount_requested,
        );

        Ok(claim.amount_requested)
    }

    /// Admin: deny an insurance claim.  No funds are moved.
    pub fn deny_claim(env: Env, admin: Address, claim_id: u32) -> Result<(), VaultError> {
        admin.require_auth();
        storage::require_admin(&env, &admin)?;

        let mut claim = storage::get_claim(&env, claim_id).ok_or(VaultError::NoClaimFound)?;

        match claim.status {
            ClaimStatus::Approved | ClaimStatus::Denied => {
                return Err(VaultError::ClaimAlreadyResolved)
            }
            ClaimStatus::Pending => {}
        }

        claim.status = ClaimStatus::Denied;
        storage::set_claim(&env, claim_id, &claim);

        events::insurance_claim_denied(&env, &admin, claim_id);
        Ok(())
    }

    /// Returns the `InsuranceClaim` for `claim_id`, or `None` if not found.
    pub fn get_claim(env: Env, claim_id: u32) -> Option<InsuranceClaim> {
        storage::get_claim_readonly(&env, claim_id)
    }
}

use soroban_sdk::{contracttype, Address, Vec};

pub const MAX_DEPOSIT_AMOUNT: i128 = 1_000_000_000_000_000;
pub const MAX_LOCK_DURATION_SECS: u64 = 157_788_000;
pub const MIN_LOCK_DURATION_SECS: u64 = 60;

/// Maximum number of tokens allowed in a single multi-token deposit (issue #330).
pub const MAX_TOKENS_PER_DEPOSIT: u32 = 5;

/// Current storage schema version. Bump this constant when the on-chain
/// layout of a `contracttype` struct changes so `migrate()` can detect
/// and upgrade stale entries.
pub const STORAGE_VERSION: u32 = 1;

/// Fraction of the penalty fee reserved for the insurance pool (5 = 5%).
pub const INSURANCE_POOL_BPS: u32 = 500; // 5% in basis points

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepositRequest {
    pub token: Address,
    pub amount: i128,
    pub unlock_time: u64,
    pub penalty_bps: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VaultKey {
    Deposit(Address, u32),
    DepositByLedger(Address, u32),
    /// Multi-token deposit entry (issue #330).
    MultiDeposit(Address, u32),
    /// Withdrawal whitelist for a deposit (issue #331).
    WithdrawalWhitelist(Address, u32),
    DepositCounter(Address),
    /// Stores a `Vec<u32>` of active deposit IDs for a depositor (both timestamp- and
    /// ledger-based). Maintained alongside the counter so `get_deposit_ids` is O(1).
    ActiveDepositIds(Address),
    Admin,
    PendingAdmin,
    Initialized,
    DepositorList,
    /// Boolean existence flag per depositor — O(1) duplicate check in `add_depositor`.
    DepositorFlag(Address),
    /// Set-once flag recording that an address was appended to `DepositorList`.
    /// Never deleted, so re-deposits don't create duplicate list entries even
    /// after the corresponding `DepositorFlag` has been cleared by `remove_depositor`.
    DepositorInList(Address),
    FeeRecipient,
    MaxDeposit,
    MaxLockSecs,
    Paused,
    /// Persists the schema version written by the last `migrate()` call (or 1
    /// for contracts that were initialized before versioning was introduced).
    StorageVersion,
    /// Staker entry: maps staker address to their stake amount
    Staker(Address),
    /// List of all registered stakers
    StakerList,
    /// Flag to track if a staker is in the StakerList (prevents duplicates)
    StakerInList(Address),
    /// Total amount staked by all stakers
    TotalStaked,
    /// Rewards pool for stakers (accumulated from penalties)
    RewardsPool,
    /// Rewards claimed by a staker (track cumulative for auditing)
    StakerRewardsClaimed(Address),
    /// Insurance claim entry (issue #334)
    InsuranceClaim(u32),
    /// Counter for insurance claim IDs (issue #334)
    ClaimCounter,
    /// Insurance pool balance per token (issue #334)
    InsurancePool(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultEntry {
    pub token: Address,
    pub amount: i128,
    pub unlock_time: u64,
    pub depositor: Address,
    pub penalty_bps: u32,
    /// Compound interest accrual frequency in seconds (0 = no compounding). (issue #332)
    pub compound_frequency_secs: u64,
    /// Timestamp of last compound accrual (issue #332).
    pub last_accrual_timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerVaultEntry {
    pub token: Address,
    pub amount: i128,
    pub unlock_ledger: u32,
    pub depositor: Address,
    pub penalty_bps: u32,
}

/// A single token+amount pair used in multi-token deposits (issue #330).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenDeposit {
    pub token: Address,
    pub amount: i128,
}

/// Vault entry that holds multiple token deposits (issue #330).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultiTokenVaultEntry {
    /// Each element is a (token, amount) pair. Length ≤ MAX_TOKENS_PER_DEPOSIT.
    pub tokens: Vec<TokenDeposit>,
    pub unlock_time: u64,
    pub depositor: Address,
    pub penalty_bps: u32,
    /// Compound interest accrual frequency in seconds (0 = no compounding). (issue #332)
    pub compound_frequency_secs: u64,
    /// Timestamp of last compound accrual (issue #332).
    pub last_accrual_timestamp: u64,
}

/// The deposit type discriminant returned by `get_deposit_type`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DepositType {
    /// Timestamp-based (`VaultEntry`) single-token deposit.
    TimeBased,
    /// Ledger-sequence-based (`LedgerVaultEntry`) deposit.
    LedgerBased,
    /// Multi-token timestamp-based deposit (`MultiTokenVaultEntry`). (issue #330)
    MultiToken,
}

/// Paginated query result for depositor addresses.
/// (Soroban `#[contracttype]` does not support generics.)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page {
    /// The items in this page
    pub items: soroban_sdk::Vec<Address>,
    /// Total number of active items across all pages
    pub total_count: u32,
}

/// Staker entry: tracks stake amount and optionally last claim timestamp
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakerEntry {
    pub staker: Address,
    pub stake_amount: i128,
}

/// Status of an insurance claim (for issue #334)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimStatus {
    Pending,
    Approved,
    Denied,
}

/// Insurance claim record (for issue #334)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InsuranceClaim {
    pub claim_id: u32,
    pub claimant: Address,
    pub token: Address,
    pub amount_requested: i128,
    pub incident_evidence: String,
    pub status: ClaimStatus,
}

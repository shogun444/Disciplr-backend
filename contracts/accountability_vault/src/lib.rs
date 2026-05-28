#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, log, symbol_short, Address, Env,
    String, Vec,
};

// ─── Storage keys ────────────────────────────────────────────────────────────
//
// "Vault"    — instance storage key for the Vault / VaultV1 struct.
// "VaultVer" — instance storage key for the schema version sentinel (u32).
//              Written by initialize() and migrate(); read by migrate() to
//              detect the stored schema without touching the vault struct
//              (Soroban XDR decoding is field-count strict — trying to decode
//              a VaultV1 slot as Vault panics at the boundary).

// ─── Error codes ─────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum VaultError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidAmount = 4,
    InvalidDeadline = 5,
    VaultExpired = 6,
    VaultNotActive = 7,
    MilestoneNotFound = 8,
    MilestoneAlreadyValidated = 9,
    InvalidMilestoneAmount = 10,
}

// ─── Data types ──────────────────────────────────────────────────────────────

/// Status of the accountability vault.
///
/// NOTE: Enum variants MUST NOT be reordered or removed — Soroban encodes
/// enum variants by index. Append-only changes are safe; reordering or
/// deletion will break on-chain storage decoding for existing instances.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VaultStatus {
    Draft = 0,
    Active = 1,
    Completed = 2,
    Failed = 3,
    Cancelled = 4,
}

/// A single milestone within the vault.
///
/// STORAGE LAYOUT CONTRACT:
/// - Fields are encoded in declaration order by soroban-sdk.
/// - New fields MUST be appended at the end.
/// - Existing fields MUST NOT be reordered, renamed, or removed.
/// - Adding a field is safe IF the decoder tolerates missing trailing fields
///   (soroban-sdk does NOT — so a migration function must backfill).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Milestone {
    pub id: String,
    pub title: String,
    pub amount: i128,
    pub due_date: u64,
    pub validated: bool,
    pub validated_at: u64,
    pub validated_by: Address,
}

/// The core accountability vault struct persisted in contract storage.
///
/// STORAGE LAYOUT CONTRACT:
/// - Fields are encoded in declaration order by soroban-sdk.
/// - New fields MUST be appended at the end.
/// - Existing fields MUST NOT be reordered, renamed, or removed.
/// - The `version` field tracks the schema revision for migration logic.
///
/// ## Version history
///
/// | Version | Changes                                         |
/// |---------|-------------------------------------------------|
/// | 1       | Initial layout (all fields below `version`)     |
/// | 2       | Added `description` field (append-only)          |
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vault {
    /// Schema version — always the first field.
    pub version: u32,
    /// Unique identifier for this vault.
    pub vault_id: String,
    /// Stellar address of the vault creator.
    pub creator: Address,
    /// Total locked amount in stroops.
    pub amount: i128,
    /// Unix timestamp when the vault becomes active.
    pub start_date: u64,
    /// Unix timestamp when the vault expires.
    pub end_date: u64,
    /// Address of the designated verifier.
    pub verifier: Address,
    /// Destination address on successful completion.
    pub success_destination: Address,
    /// Destination address on failure.
    pub failure_destination: Address,
    /// Current status of the vault.
    pub status: VaultStatus,
    /// Milestones attached to this vault.
    pub milestones: Vec<Milestone>,
    /// Creation timestamp.
    pub created_at: u64,
    // ── v2 fields (appended) ──────────────────────────────────────────
    /// Optional description — added in schema v2.
    pub description: String,
}

/// Legacy V1 vault layout — used by migration tests to verify that
/// data written under the old schema can be decoded and upgraded.
///
/// This struct mirrors the original Vault layout WITHOUT the `description`
/// field. It is intentionally kept in sync with the v1 column list so
/// snapshot-based tests can round-trip pre-upgrade data.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultV1 {
    pub version: u32,
    pub vault_id: String,
    pub creator: Address,
    pub amount: i128,
    pub start_date: u64,
    pub end_date: u64,
    pub verifier: Address,
    pub success_destination: Address,
    pub failure_destination: Address,
    pub status: VaultStatus,
    pub milestones: Vec<Milestone>,
    pub created_at: u64,
}

// ─── Current schema version ──────────────────────────────────────────────────

pub const CURRENT_VAULT_VERSION: u32 = 2;

// ─── Contract implementation ─────────────────────────────────────────────────

#[contract]
pub struct AccountabilityVault;

#[contractimpl]
impl AccountabilityVault {
    /// Initialize a new accountability vault.
    pub fn initialize(
        env: Env,
        vault_id: String,
        creator: Address,
        amount: i128,
        start_date: u64,
        end_date: u64,
        verifier: Address,
        success_destination: Address,
        failure_destination: Address,
        milestones: Vec<Milestone>,
        description: String,
    ) -> Result<(), VaultError> {
        // Ensure caller is authorized
        creator.require_auth();

        // Guard: cannot re-initialize
        if env.storage().instance().has(&symbol_short!("Vault")) {
            return Err(VaultError::AlreadyInitialized);
        }

        if amount <= 0 {
            return Err(VaultError::InvalidAmount);
        }

        if end_date <= start_date {
            return Err(VaultError::InvalidDeadline);
        }

        let vault = Vault {
            version: CURRENT_VAULT_VERSION,
            vault_id,
            creator,
            amount,
            start_date,
            end_date,
            verifier,
            success_destination,
            failure_destination,
            status: VaultStatus::Draft,
            milestones,
            created_at: env.ledger().timestamp(),
            description,
        };

        env.storage().instance().set(&symbol_short!("Vault"), &vault);
        // Write version sentinel so migrate() can probe the schema without
        // decoding the vault struct (avoids XDR field-count panics).
        env.storage()
            .instance()
            .set(&symbol_short!("VaultVer"), &CURRENT_VAULT_VERSION);
        log!(&env, "vault.initialized version={}", CURRENT_VAULT_VERSION);
        Ok(())
    }

    /// Read the current vault state.
    pub fn get_vault(env: Env) -> Result<Vault, VaultError> {
        env.storage()
            .instance()
            .get(&symbol_short!("Vault"))
            .ok_or(VaultError::NotInitialized)
    }

    /// Activate the vault (transition from Draft to Active).
    pub fn activate(env: Env, caller: Address) -> Result<(), VaultError> {
        caller.require_auth();
        let mut vault: Vault = Self::get_vault(env.clone())?;

        if vault.status != VaultStatus::Draft {
            return Err(VaultError::VaultNotActive);
        }

        vault.status = VaultStatus::Active;
        env.storage().instance().set(&symbol_short!("Vault"), &vault);
        log!(&env, "vault.activated");
        Ok(())
    }

    /// Validate a milestone by the designated verifier.
    pub fn validate_milestone(
        env: Env,
        verifier: Address,
        milestone_id: String,
    ) -> Result<(), VaultError> {
        verifier.require_auth();
        let mut vault: Vault = Self::get_vault(env.clone())?;

        if vault.status != VaultStatus::Active {
            return Err(VaultError::VaultNotActive);
        }

        if verifier != vault.verifier {
            return Err(VaultError::Unauthorized);
        }

        let mut found = false;
        let mut new_milestones = Vec::new(&env);
        let mut all_validated = true;

        for i in 0..vault.milestones.len() {
            let mut ms = vault.milestones.get(i).unwrap();
            if ms.id == milestone_id {
                if ms.validated {
                    return Err(VaultError::MilestoneAlreadyValidated);
                }
                ms.validated = true;
                ms.validated_at = env.ledger().timestamp();
                ms.validated_by = verifier.clone();
                found = true;
            }
            if !ms.validated {
                all_validated = false;
            }
            new_milestones.push_back(ms);
        }

        if !found {
            return Err(VaultError::MilestoneNotFound);
        }

        vault.milestones = new_milestones;

        if all_validated {
            vault.status = VaultStatus::Completed;
            log!(&env, "vault.completed");
        }

        env.storage().instance().set(&symbol_short!("Vault"), &vault);
        log!(&env, "milestone.validated");
        Ok(())
    }

    /// Return the current schema version.
    pub fn version(_env: Env) -> u32 {
        CURRENT_VAULT_VERSION
    }

    /// Migrate storage from a previous schema version to the current one.
    /// This is the upgrade entry point called after a contract WASM upgrade.
    ///
    /// # Migration strategy
    ///
    /// `migrate()` first reads the lightweight `"VaultVer"` sentinel (a `u32`)
    /// to determine the stored schema without touching the vault struct itself.
    /// This is necessary because Soroban XDR decoding is **field-count strict**:
    /// attempting to decode a 12-field `VaultV1` blob as the 13-field `Vault`
    /// panics with `Error(Object, UnexpectedSize)`. The sentinel lets us choose
    /// the right decoder before reading the vault.
    ///
    /// - **v1 → v2**: read `VaultV1`, write `Vault` with `description` defaulted
    ///   to `""` and update the sentinel to `CURRENT_VAULT_VERSION`.
    ///
    /// Idempotent: calling `migrate()` when the sentinel already equals
    /// `CURRENT_VAULT_VERSION` is a no-op that never mutates storage.
    pub fn migrate(env: Env) -> Result<(), VaultError> {
        let vault_key = symbol_short!("Vault");
        let ver_key = symbol_short!("VaultVer");

        // ── 1. Fast-path via version sentinel ────────────────────────────────
        // The sentinel is a plain u32, safe to read regardless of vault layout.
        if let Some(stored_ver) = env.storage().instance().get::<_, u32>(&ver_key) {
            if stored_ver >= CURRENT_VAULT_VERSION {
                log!(&env, "migrate: already at version={}", stored_ver);
                return Ok(());
            }
            // stored_ver < CURRENT_VAULT_VERSION — fall through to upgrade.
        }

        // ── 2. v1 → v2 upgrade ──────────────────────────────────────────────
        // Only reached when the sentinel is absent (legacy V1 on-chain data
        // written before this harness) or < CURRENT_VAULT_VERSION.
        // VaultV1 exactly mirrors the 12-field wire format for schema v1;
        // decoding it panics only if the stored field count doesn't match —
        // which is safe here because we verify the sentinel first.
        if let Some(v1) = env.storage().instance().get::<_, VaultV1>(&vault_key) {
            let upgraded = Vault {
                version: CURRENT_VAULT_VERSION,
                vault_id: v1.vault_id,
                creator: v1.creator,
                amount: v1.amount,
                start_date: v1.start_date,
                end_date: v1.end_date,
                verifier: v1.verifier,
                success_destination: v1.success_destination,
                failure_destination: v1.failure_destination,
                status: v1.status,
                milestones: v1.milestones,
                created_at: v1.created_at,
                description: String::from_str(&env, ""),
            };

            env.storage().instance().set(&vault_key, &upgraded);
            env.storage().instance().set(&ver_key, &CURRENT_VAULT_VERSION);
            log!(&env, "migrate: upgraded v1 → v{}", CURRENT_VAULT_VERSION);
            return Ok(());
        }

        // ── 3. Nothing stored — contract was never initialised ───────────────
        Err(VaultError::NotInitialized)
    }
}

#[cfg(test)]
mod test;

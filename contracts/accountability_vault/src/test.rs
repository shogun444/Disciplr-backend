//! Storage-migration test harness for `accountability_vault`.
//!
//! # Purpose
//! Prevent storage-layout regressions during contract upgrades by:
//!   1. Writing a pre-upgrade V1 snapshot into the environment.
//!   2. Running `migrate()` to upgrade the slot to V2.
//!   3. Asserting every field decodes correctly and the new `description`
//!      field is initialised to an empty string.
//!
//! # Re-generating snapshots
//! If you intentionally change the Vault storage layout, bump
//! `CURRENT_VAULT_VERSION` in `lib.rs`, add a new `VaultVN` legacy struct, add
//! a migration branch in `migrate()`, then re-run:
//!
//! ```sh
//! cd contracts/accountability_vault
//! cargo test -- --nocapture 2>&1 | grep -E 'SNAPSHOT|test .* ok|FAILED'
//! ```
//!
//! Compare the "SNAPSHOT before migrate" and "SNAPSHOT after migrate" lines to
//! confirm every field survives the round-trip.  Any unexpected difference is a
//! storage regression — fix the migration function before merging.
//!
//! # Reviewing snapshots in PRs
//! See `contracts/README.md` → "Reviewing Snapshots in Pull Requests".

#![cfg(test)]

extern crate std;
use std::println;

use soroban_sdk::{
    testutils::Address as _,
    Address, Env, String, Vec,
};

use crate::{
    AccountabilityVaultClient, Milestone, Vault, VaultError, VaultStatus, VaultV1,
    CURRENT_VAULT_VERSION,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a minimal [`Milestone`] for use in tests.
fn make_milestone(env: &Env, id: &str) -> Milestone {
    Milestone {
        id: String::from_str(env, id),
        title: String::from_str(env, "Test Milestone"),
        amount: 1_000_000,
        due_date: 9_999_999,
        validated: false,
        validated_at: 0,
        validated_by: Address::generate(env),
    }
}

/// Write a V1 vault directly into instance storage, bypassing the contract
/// entry points.  This simulates the state that exists on-chain *before* a
/// WASM upgrade to V2.
///
/// Must be called inside `env.as_contract(&contract_id, || { … })`.
fn write_v1_snapshot(env: &Env, creator: &Address, verifier: &Address) -> VaultV1 {
    use soroban_sdk::symbol_short;

    let milestone = make_milestone(env, "m-001");
    let mut milestones = Vec::new(env);
    milestones.push_back(milestone);

    let v1 = VaultV1 {
        version: 1,
        vault_id: String::from_str(env, "vault-v1-snapshot"),
        creator: creator.clone(),
        amount: 50_000_000,
        start_date: 1_000_000,
        end_date: 2_000_000,
        verifier: verifier.clone(),
        success_destination: Address::generate(env),
        failure_destination: Address::generate(env),
        status: VaultStatus::Draft,
        milestones,
        created_at: env.ledger().timestamp(),
    };

    env.storage()
        .instance()
        .set(&symbol_short!("Vault"), &v1);
    // Write version sentinel so migrate() can probe schema without touching
    // the vault struct (Soroban XDR decoding is field-count strict).
    env.storage()
        .instance()
        .set(&symbol_short!("VaultVer"), &1_u32);

    v1
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Happy-path: a V1 snapshot written to storage is successfully decoded by the
/// V2 contract after `migrate()` runs.
#[test]
fn test_v1_to_v2_migration_succeeds() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(crate::AccountabilityVault, ());
    let client = AccountabilityVaultClient::new(&env, &contract_id);

    let creator = Address::generate(&env);
    let verifier = Address::generate(&env);

    // Write pre-upgrade V1 state directly into the contract's storage slot.
    let v1 = env.as_contract(&contract_id, || {
        write_v1_snapshot(&env, &creator, &verifier)
    });

    println!("SNAPSHOT before migrate: {:?}", v1);

    // `migrate()` panics on error — a panic here means the harness caught a
    // regression before it could reach mainnet.
    client.migrate();

    // `get_vault()` returns Vault directly; panics if storage is unreadable.
    let vault: Vault = client.get_vault();

    println!("SNAPSHOT after migrate: {:?}", vault);

    // ── Field-by-field assertions ─────────────────────────────────────────

    assert_eq!(
        vault.version, CURRENT_VAULT_VERSION,
        "version must be bumped to {}",
        CURRENT_VAULT_VERSION
    );
    assert_eq!(vault.vault_id, v1.vault_id, "vault_id must be preserved");
    assert_eq!(vault.creator, v1.creator, "creator must be preserved");
    assert_eq!(vault.amount, v1.amount, "amount must be preserved");
    assert_eq!(vault.start_date, v1.start_date, "start_date must be preserved");
    assert_eq!(vault.end_date, v1.end_date, "end_date must be preserved");
    assert_eq!(vault.verifier, v1.verifier, "verifier must be preserved");
    assert_eq!(
        vault.success_destination, v1.success_destination,
        "success_destination must be preserved"
    );
    assert_eq!(
        vault.failure_destination, v1.failure_destination,
        "failure_destination must be preserved"
    );
    assert_eq!(vault.status, v1.status, "status must be preserved");
    assert_eq!(
        vault.milestones.len(),
        v1.milestones.len(),
        "milestone count must be preserved"
    );
    assert_eq!(vault.created_at, v1.created_at, "created_at must be preserved");

    // New v2 field — must default to empty string, never panic.
    assert_eq!(
        vault.description,
        String::from_str(&env, ""),
        "description must default to empty string after v1→v2 migration"
    );
}

/// Migration must be idempotent: calling `migrate()` twice on an already-v2
/// vault must succeed without corrupting data.
#[test]
fn test_migration_is_idempotent() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(crate::AccountabilityVault, ());
    let client = AccountabilityVaultClient::new(&env, &contract_id);

    let creator = Address::generate(&env);
    let verifier = Address::generate(&env);
    let success_dest = Address::generate(&env);
    let failure_dest = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(make_milestone(&env, "m-001"));

    // Initialise directly at v2.
    client.initialize(
        &String::from_str(&env, "vault-idempotent"),
        &creator,
        &50_000_000_i128,
        &1_000_000_u64,
        &2_000_000_u64,
        &verifier,
        &success_dest,
        &failure_dest,
        &milestones,
        &String::from_str(&env, "already v2"),
    );

    let before: Vault = client.get_vault();

    // First migration: no-op on a v2 vault.
    client.migrate();
    // Second migration: still a no-op.
    client.migrate();

    let after: Vault = client.get_vault();

    assert_eq!(before, after, "double migrate must not mutate a v2 vault");
}

/// `migrate()` on a completely empty storage slot must return `NotInitialized`.
#[test]
fn test_migration_on_empty_storage_returns_not_initialized() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(crate::AccountabilityVault, ());
    let client = AccountabilityVaultClient::new(&env, &contract_id);

    // `try_migrate()` returns Result<(), Result<VaultError, InvokeError>>
    let result = client.try_migrate();

    assert_eq!(
        result,
        Err(Ok(VaultError::NotInitialized)),
        "migrate() on empty storage must return NotInitialized"
    );
}

/// Milestone fields survive the v1 → v2 round-trip without truncation or
/// reordering.
#[test]
fn test_milestone_fields_preserved_after_migration() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(crate::AccountabilityVault, ());
    let client = AccountabilityVaultClient::new(&env, &contract_id);

    let creator = Address::generate(&env);
    let verifier = Address::generate(&env);

    let v1 = env.as_contract(&contract_id, || {
        use soroban_sdk::symbol_short;

        // Two milestones to confirm ordering is preserved.
        let ms1 = Milestone {
            id: String::from_str(&env, "ms-alpha"),
            title: String::from_str(&env, "Alpha"),
            amount: 10_000,
            due_date: 1_111_111,
            validated: false,
            validated_at: 0,
            validated_by: Address::generate(&env),
        };
        let ms2 = Milestone {
            id: String::from_str(&env, "ms-beta"),
            title: String::from_str(&env, "Beta"),
            amount: 20_000,
            due_date: 2_222_222,
            validated: true,
            validated_at: 1_500_000,
            validated_by: verifier.clone(),
        };
        let mut milestones = Vec::new(&env);
        milestones.push_back(ms1);
        milestones.push_back(ms2);

        let v1 = VaultV1 {
            version: 1,
            vault_id: String::from_str(&env, "vault-ms-test"),
            creator: creator.clone(),
            amount: 30_000,
            start_date: 1_000_000,
            end_date: 3_000_000,
            verifier: verifier.clone(),
            success_destination: Address::generate(&env),
            failure_destination: Address::generate(&env),
            status: VaultStatus::Active,
            milestones,
            created_at: 500_000,
        };
        env.storage().instance().set(&symbol_short!("Vault"), &v1);
        // Version sentinel — must be written alongside the vault struct so
        // migrate() can select the right decoder without touching the vault.
        env.storage().instance().set(&symbol_short!("VaultVer"), &1_u32);
        v1
    });

    client.migrate();
    let vault: Vault = client.get_vault();

    assert_eq!(vault.milestones.len(), 2, "both milestones must survive migration");

    let ms1_post = vault.milestones.get(0).expect("milestone 0 must exist");
    let ms2_post = vault.milestones.get(1).expect("milestone 1 must exist");

    assert_eq!(ms1_post.id, String::from_str(&env, "ms-alpha"));
    assert_eq!(ms1_post.amount, 10_000_i128);
    assert!(!ms1_post.validated, "ms-alpha must remain un-validated");

    assert_eq!(ms2_post.id, String::from_str(&env, "ms-beta"));
    assert_eq!(ms2_post.amount, 20_000_i128);
    assert!(ms2_post.validated, "ms-beta must remain validated");
    assert_eq!(ms2_post.validated_at, 1_500_000_u64);

    // v2 sentinels
    assert_eq!(vault.version, CURRENT_VAULT_VERSION);
    assert_eq!(vault.description, String::from_str(&env, ""));

    // Confirm V1 fields match
    assert_eq!(vault.creator, v1.creator);
    assert_eq!(vault.status, v1.status);
}

/// Verify that the `version()` entry-point returns `CURRENT_VAULT_VERSION`.
#[test]
fn test_version_entry_point() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(crate::AccountabilityVault, ());
    let client = AccountabilityVaultClient::new(&env, &contract_id);

    assert_eq!(client.version(), CURRENT_VAULT_VERSION);
}

/// A fully v2-initialised vault must decode correctly without any migration.
#[test]
fn test_v2_vault_round_trip() {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(crate::AccountabilityVault, ());
    let client = AccountabilityVaultClient::new(&env, &contract_id);

    let creator = Address::generate(&env);
    let verifier = Address::generate(&env);
    let success_dest = Address::generate(&env);
    let failure_dest = Address::generate(&env);

    let mut milestones = Vec::new(&env);
    milestones.push_back(make_milestone(&env, "m-v2"));

    client.initialize(
        &String::from_str(&env, "vault-v2-roundtrip"),
        &creator,
        &100_000_000_i128,
        &1_000_000_u64,
        &5_000_000_u64,
        &verifier,
        &success_dest,
        &failure_dest,
        &milestones,
        &String::from_str(&env, "hello v2"),
    );

    let vault: Vault = client.get_vault();

    assert_eq!(vault.version, CURRENT_VAULT_VERSION);
    assert_eq!(vault.vault_id, String::from_str(&env, "vault-v2-roundtrip"));
    assert_eq!(vault.amount, 100_000_000_i128);
    assert_eq!(vault.description, String::from_str(&env, "hello v2"));
    assert_eq!(vault.status, VaultStatus::Draft);
    assert_eq!(vault.milestones.len(), 1);
}

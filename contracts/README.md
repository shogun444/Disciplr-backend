# Disciplr Smart Contracts

This directory contains the Soroban smart contracts that power the Disciplr accountability protocol on Stellar.

---

## `accountability_vault`

A programmable, time-locked accountability vault.  Funds are locked until
milestones are independently verified, then routed to a success or failure
destination automatically.

### Storage-Layout Rules

> **CRITICAL** — Soroban encodes `#[contracttype]` structs and enums by
> **field/variant index**, not by name.  Breaking the wire format corrupts
> **all existing on-chain instances** with no recovery path.

| Rule | Detail |
|------|--------|
| **Append-only fields** | New fields must be added at the **end** of a struct |
| **No reordering** | Swapping two fields produces a silent decode error |
| **No removal** | Removing a field shifts subsequent field indices |
| **No renames** | Rename is safe for Rust readability but irrelevant to encoding |
| **Enum variants** | Same rules apply — append only, no reordering/removal |
| **Bump `version`** | Increment `CURRENT_VAULT_VERSION` in `lib.rs` for every layout change |

The `Vault.version` field (always first) lets the `migrate()` entry-point
detect and upgrade stored data when the WASM is upgraded on-chain.

---

## Storage-Migration Test Harness

### Overview

`contracts/accountability_vault/src/test.rs` contains a snapshot-based
migration harness that catches storage-layout regressions **before** they
reach mainnet.

The harness works by:

1. Writing a **pre-upgrade V1 `VaultV1` struct** directly into the contract's
   instance storage (bypassing the `initialize()` entry-point), **together with
   a `"VaultVer"` sentinel key** set to `1`.
2. Calling `migrate()` to upgrade the slot to the current schema version.
3. Reading back the slot as the current `Vault` struct and **asserting every
   field decodes correctly** and new fields are set to their expected defaults.

> **Why the sentinel?**  
> Soroban's XDR decoder for `#[contracttype]` structs is **field-count strict**.
> Trying to decode a 12-field `VaultV1` blob as the 13-field `Vault` panics
> with `Error(Object, UnexpectedSize)` before any `if let Some(...)` fallback
> can run.  The `"VaultVer"` key is a plain `u32` that `migrate()` reads first
> to select the right decoder — cheaply and safely.

### Test coverage

| Test | What it guards |
|------|---------------|
| `test_v1_to_v2_migration_succeeds` | V1 → V2 happy path; all fields preserved, `description` defaults to `""` |
| `test_migration_is_idempotent` | Running `migrate()` twice on a V2 vault does not mutate data |
| `test_migration_on_empty_storage_returns_not_initialized` | `migrate()` on an empty storage slot returns `NotInitialized` |
| `test_milestone_fields_preserved_after_migration` | Multi-milestone ordering and validated state survive the round-trip |
| `test_version_entry_point` | `version()` returns `CURRENT_VAULT_VERSION` |
| `test_v2_vault_round_trip` | A freshly initialised V2 vault decodes correctly without migration |

### Running the tests

```sh
# From the repo root
cd contracts/accountability_vault
cargo test 2>&1
```

All tests should pass with exit code `0`.  Output is captured by default; add
`-- --nocapture` to see `println!` diagnostics.

---

## Regenerating Snapshots

When you **intentionally** change the `Vault` storage layout, follow these
steps to update the harness:

### 1 — Bump the schema version

In `contracts/accountability_vault/src/lib.rs`:

```rust
pub const CURRENT_VAULT_VERSION: u32 = 3; // ← increment
```

### 2 — Snapshot the old struct as `VaultVN`

Copy the current `VaultV1` (or the latest legacy struct) and rename it to
`VaultV2` (or the previous version number).  This struct **must exactly match**
the layout that is currently live on-chain:

```rust
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultV2 {
    // ... exact copy of the V2 fields, no changes
}
```

### 3 — Add the migration branch in `migrate()`

Inside `AccountabilityVault::migrate()`, add a new `if let Some(v2) = ...`
branch that reads `VaultV2` and writes `Vault` (V3), back-filling the new
field with its default:

```rust
if let Some(v2) = env.storage().instance().get::<_, VaultV2>(&key) {
    let upgraded = Vault {
        version: CURRENT_VAULT_VERSION,
        // ... copy v2 fields
        new_field: DefaultValue::default(),
    };
    env.storage().instance().set(&key, &upgraded);
    return Ok(());
}
```

### 4 — Add a test case in `test.rs`

Mirror `test_v1_to_v2_migration_succeeds` but using `VaultV2` as the
pre-upgrade snapshot writer. **Remember to write the `"VaultVer"` sentinel**
when setting up the snapshot — without it, `migrate()` falls through to the
VaultV1 decoder which will panic on V2-sized data:

```rust
#[test]
fn test_v2_to_v3_migration_succeeds() {
    // Inside env.as_contract(&contract_id, || { … }):
    env.storage().instance().set(&symbol_short!("Vault"), &v2_snapshot);
    env.storage().instance().set(&symbol_short!("VaultVer"), &2_u32); // ← required
    // migrate() → assert Vault (V3)
}
```

### 5 — Run tests and review the diff

```sh
cargo test -- --nocapture 2>&1 | grep -E 'SNAPSHOT|FAILED|ok'
```

Compare the **"before migrate"** and **"after migrate"** debug dumps for each
field.  Any unexpected change is a storage regression — fix the migration
function before merging.

### 6 — Update this document

Add the new version to the version-history table in `lib.rs` doc-comment and
confirm the `test coverage` table in this README is current.

---

## Reviewing Snapshots in Pull Requests

When reviewing a PR that touches `contracts/accountability_vault/src/lib.rs`
or `src/test.rs`:

1. **Check `CURRENT_VAULT_VERSION`** — did the author bump it?  A layout change
   without a version bump is an automatic blocker.
2. **Check the legacy struct** — does `VaultV1` (or the latest `VaultVN`)
   still exactly mirror the previous wire format?
3. **Check `migrate()`** — does every old version have a corresponding
   upgrade branch?
4. **Run the tests locally** — `cargo test` must be green with no warnings.
5. **Read the `--nocapture` output** — the `SNAPSHOT before/after` lines make
   field-level changes visible at a glance.

---

## Version History

| Version | Changes |
|---------|---------|
| 1 | Initial layout: `version`, `vault_id`, `creator`, `amount`, `start_date`, `end_date`, `verifier`, `success_destination`, `failure_destination`, `status`, `milestones`, `created_at` |
| 2 | Added `description: String` (append-only, defaults to `""` on migration) |

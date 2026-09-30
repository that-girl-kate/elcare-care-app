//! NormalNFT1155 — ERC-1155-equivalent on Soroban.
//!
//! Supports multiple token types per contract. Each token type can have
//! fungible supply (edition sizes). The creator mints token IDs on demand
//! via `mint_new` (auto-increments ID) or `mint` (explicit ID for resupply).
//! Batch operations mirror ERC-1155 `safeBatchTransferFrom`.
//!
//! # New features (parity with collection_nft_erc721)
//!
//! - **Pausability**: creator-only `pause`/`unpause`/`is_paused`; gates
//!   `mint_new`, `mint`, `mint_batch`, `transfer`, `transfer_from`,
//!   `batch_transfer`, and `burn`.
//! - **Metadata management**: `set_base_uri`/`base_uri`,
//!   `freeze_metadata`/`is_metadata_frozen`.  `uri()` resolves in order:
//!   (1) explicit per-token URI stored at first mint, **unless** a base URI is
//!   set, in which case `base_uri + token_id` is returned instead.
//! - **Per-token royalties (ERC-2981 parity)**: `set_default_royalty`,
//!   `set_token_royalty`, `royalty_info_for(token_id, sale_price)`.
//!   `update_royalty`/`royalty_info` kept for backward compatibility as
//!   default-royalty view/setter.
#![no_std]
#![allow(clippy::too_many_arguments, deprecated)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Bytes, BytesN, Env,
    String, Symbol, Vec,
};

/// Shared metadata validation rules (Issue #476).
pub mod metadata;

const TTL_THRESHOLD: u32 = 50_000;
const TTL_BUMP: u32 = 100_000;
const MAX_BPS: u32 = 10_000; // 100 % in basis points
/// Maximum number of items accepted by any single batch call (#274).
const MAX_BATCH_SIZE: u32 = 200;
/// Maximum URI length in bytes (#276).
const MAX_URI_LEN: u32 = 2048;

// ─── Errors ──────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotApproved = 3,
    InsufficientBalance = 4,
    LengthMismatch = 5,
    NotCreator = 6,
    MaxSupplyReached = 7,
    WalletLimitReached = 8,
    CollectionPaused = 9,
    MetadataFrozen = 10,
    AlreadyFrozen = 11,
    InvalidBps = 12,
    /// migrate() called for a version already marked done.
    AlreadyMigrated = 13,
    /// Unsupported version jump.
    UnsupportedMigration = 14,
    /// Empty URI provided.
    EmptyUri = 15,
    /// URI exceeds maximum length.
    UriTooLong = 16,
    /// Zero amount provided.
    ZeroAmount = 17,
    /// Empty batch provided.
    EmptyBatch = 18,
    /// Batch exceeds maximum size.
    BatchTooLarge = 19,
    /// Token does not exist.
    TokenNotFound = 20,
    /// Approval expiry is in the past.
    ApprovalExpired = 21,
    /// Collection name is empty (Issue #476).
    EmptyName = 22,
    /// Collection name exceeds maximum length (Issue #476).
    NameTooLong = 23,
    /// accept_creator() called when no succession proposal is pending (#484).
    NoPendingCreator = 24,
    /// accept_creator() called by an address that is not the proposed successor (#484).
    NotPendingCreator = 25,
    /// accept_creator() called after the pending proposal's expiry ledger (#484).
    ProposalExpired = 26,
}

// ─── Storage Keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    // Instance storage
    Initialized,
    Creator,
    Name,
    CurrentWasmHash,
    NextTokenId,
    RoyaltyBps,
    RoyaltyReceiver,
    /// Optional global per-wallet mint cap. Stored in instance storage.
    PerWalletLimit,
    /// bool — collection is paused when true.
    Paused,
    /// String — collection-level base URI (optional).
    BaseUri,
    /// bool — permanently frozen when true.
    MetadataFrozen,
    /// bool — per-token metadata frozen when true.
    TokenFrozen(u64),
    // Persistent storage
    Balance(Address, u64),            // (account, token_id) → u128
    ApprovedForAll(Address, Address), // (owner, operator) → bool
    /// Optional expiry (ledger sequence) for an operator approval.
    ApprovedForAllExpiry(Address, Address), // (owner, operator) → u32
    TokenUri(u64),
    TotalSupply(u64), // per token_id
    /// Per-token maximum supply cap. 0 means no cap.
    MaxTokenSupply(u64),
    /// Cumulative amount minted per wallet per token_id.
    WalletMinted(Address, u64),
    /// Per-token royalty override — receiver address.
    TokenRoyaltyReceiver(u64),
    /// Per-token royalty override — bps.
    TokenRoyaltyBps(u64),
    // ── Versioned migration registry ─────────────────────────────────────
    MigrationDone(String),
    MigrationCursor(String),
    ContractVersion,
    /// Original creator address set at initialization — never changed by
    /// succession (#484).
    OriginalCreator,
    /// Pending successor proposed via `propose_creator` (#484).
    PendingCreator,
    /// Ledger sequence at which the pending proposal expires (#484).
    PendingCreatorExpiry,
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Convert a u64 to its decimal ASCII representation as a Soroban `String`.
/// Used to build `base_uri + token_id` paths in `uri()`.
fn u64_to_string(env: &Env, mut n: u64) -> String {
    let mut buf = [0u8; 20]; // u64::MAX has 20 decimal digits
    let mut len = 0usize;
    if n == 0 {
        buf[0] = b'0';
        len = 1;
    } else {
        while n > 0 {
            buf[len] = b'0' + (n % 10) as u8;
            n /= 10;
            len += 1;
        }
        buf[..len].reverse();
    }
    String::from_bytes(env, &buf[..len])
}

/// Rejects empty or oversized metadata URIs (#276). Applied to every mint
/// path and to `set_base_uri` so boundary/malformed values are caught
/// consistently regardless of entry point.
fn validate_uri(uri: &String) -> Result<(), Error> {
    let len = uri.len();
    if len == 0 {
        return Err(Error::EmptyUri);
    }
    if len > MAX_URI_LEN {
        return Err(Error::UriTooLong);
    }
    Ok(())
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct NormalNFT1155;

#[contractimpl]
impl NormalNFT1155 {
    // ── Initializer ───────────────────────────────────────────────────────

    pub fn initialize(
        env: Env,
        creator: Address,
        name: String,
        royalty_bps: u32,
        royalty_receiver: Address,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Initialized) {
            return Err(Error::AlreadyInitialized);
        }
        if royalty_bps > MAX_BPS {
            return Err(Error::InvalidBps);
        }
        // Issue #476: apply shared metadata validation rules.
        metadata::validate_name(&name, Error::EmptyName, Error::NameTooLong)?;
        metadata::validate_royalty_bps(royalty_bps, Error::InvalidBps)?;
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Creator, &creator);
        // OriginalCreator is set once at initialization and never overwritten
        // by succession — preserves historical royalty attribution (#484).
        env.storage().instance().set(&DataKey::OriginalCreator, &creator);
        env.storage().instance().set(&DataKey::Name, &name);
        env.storage().instance().set(&DataKey::CurrentWasmHash, &BytesN::from_array(&env, &[0u8; 32]));
        env.storage().instance().set(&DataKey::NextTokenId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::RoyaltyBps, &royalty_bps);
        env.storage()
            .instance()
            .set(&DataKey::RoyaltyReceiver, &royalty_receiver);
        env.storage().instance().extend_ttl(50_000, 100_000);
        Ok(())
    }

    // ── WASM upgrade ──────────────────────────────────────────────────────

    /// Replace the contract WASM. Callable only by creator.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        let old_wasm_hash: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::CurrentWasmHash)
            .unwrap_or(BytesN::from_array(&env, &[0u8; 32]));
        env.storage()
            .instance()
            .set(&DataKey::CurrentWasmHash, &new_wasm_hash);
        env.deployer().update_current_contract_wasm(new_wasm_hash.clone());
        env.events().publish(
            (symbol_short!("upgraded"),),
            (old_wasm_hash, new_wasm_hash),
        );
        Ok(())
    }

    pub fn set_token_max_supply(env: Env, token_id: u64, max_supply: u128) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        if max_supply == 0 {
            env.storage()
                .persistent()
                .remove(&DataKey::MaxTokenSupply(token_id));
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::MaxTokenSupply(token_id), &max_supply);
            env.storage().persistent().extend_ttl(
                &DataKey::MaxTokenSupply(token_id),
                TTL_THRESHOLD,
                TTL_BUMP,
            );
        }
        Ok(())
    }

    /// Set or clear the global per-wallet mint limit (0 = no limit).
    pub fn set_per_wallet_limit(env: Env, limit: u128) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::PerWalletLimit, &limit);
        Ok(())
    }

    /// Read the configured per-wallet limit (0 = no limit).
    pub fn per_wallet_limit(env: Env) -> u128 {
        env.storage()
            .instance()
            .get(&DataKey::PerWalletLimit)
            .unwrap_or(0)
    }

    /// Read the configured per-token max supply (0 = no cap).
    pub fn token_max_supply(env: Env, token_id: u64) -> u128 {
        env.storage()
            .persistent()
            .get(&DataKey::MaxTokenSupply(token_id))
            .unwrap_or(0)
    }

    /// Read cumulative amount minted by `wallet` for `token_id`.
    pub fn wallet_minted(env: Env, wallet: Address, token_id: u64) -> u128 {
        env.storage()
            .persistent()
            .get(&DataKey::WalletMinted(wallet, token_id))
            .unwrap_or(0)
    }

    // ── Pause mechanism ───────────────────────────────────────────────────

    /// Pause all state-mutating operations. Callable only by creator.
    pub fn pause(env: Env) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((symbol_short!("pause"),), true);
        Ok(())
    }

    /// Resume operations. Callable only by creator.
    pub fn unpause(env: Env) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((symbol_short!("pause"),), false);
        Ok(())
    }

    /// Returns `true` if the collection is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
    }

    // ── Metadata management ───────────────────────────────────────────────

    /// Update the collection-level base URI.  Reverts if metadata is frozen.
    /// Callable only by creator.
    pub fn set_base_uri(env: Env, base_uri: String) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let creator = Self::only_creator(&env)?;
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::MetadataFrozen)
            .unwrap_or(false)
        {
            return Err(Error::MetadataFrozen);
        }
        validate_uri(&base_uri)?;
        let old_uri: Option<String> = env.storage().instance().get(&DataKey::BaseUri);
        env.storage().instance().set(&DataKey::BaseUri, &base_uri);
        env.events().publish(
            (symbol_short!("meta_upd"), creator),
            (old_uri, base_uri),
        );
        Ok(())
    }

    /// Returns the stored base URI, or `None` if unset.
    pub fn base_uri(env: Env) -> Option<String> {
        env.storage().instance().get(&DataKey::BaseUri)
    }

    /// Permanently freeze metadata. Can only be called once.
    /// After this, `set_base_uri` reverts forever. Callable only by creator.
    pub fn freeze_metadata(env: Env) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let creator = Self::only_creator(&env)?;
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::MetadataFrozen)
            .unwrap_or(false)
        {
            return Err(Error::AlreadyFrozen);
        }
        env.storage()
            .instance()
            .set(&DataKey::MetadataFrozen, &true);
        env.events()
            .publish((symbol_short!("meta_frz"),), creator);
        Ok(())
    }

    /// Returns `true` if metadata has been permanently frozen.
    pub fn is_metadata_frozen(env: Env) -> bool {
        env.storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::MetadataFrozen)
            .unwrap_or(false)
    }

    /// Permanently freeze metadata for a specific token. Can only be called once
    /// per token; subsequent calls revert with `AlreadyFrozen`. Callable by the
    /// collection owner or any holder of the token.
    pub fn freeze_token(env: Env, caller: Address, token_id: u64) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        caller.require_auth();

        // Verify token exists (has been minted)
        if !env.storage().persistent().has(&DataKey::TokenUri(token_id)) {
            return Err(Error::TokenNotFound);
        }

        // Allow creator or any token holder — read creator address directly
        // without calling only_creator() which would require creator auth.
        let creator: Address = env
            .storage()
            .instance()
            .get(&DataKey::Creator)
            .ok_or(Error::NotInitialized)?;
        let balance: u128 = env
            .storage()
            .persistent()
            .get(&DataKey::Balance(caller.clone(), token_id))
            .unwrap_or(0);

        if caller != creator && balance == 0 {
            return Err(Error::NotApproved);
        }

        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::TokenFrozen(token_id))
            .unwrap_or(false)
        {
            return Err(Error::AlreadyFrozen);
        }

        env.storage()
            .persistent()
            .set(&DataKey::TokenFrozen(token_id), &true);
        env.storage().persistent().extend_ttl(
            &DataKey::TokenFrozen(token_id),
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        env.events()
            .publish((symbol_short!("token_frz"),), token_id);
        Ok(())
    }

    /// Update the URI for a specific token. Reverts if either the collection
    /// or the token's metadata is frozen. Callable only by creator.
    pub fn set_token_uri(env: Env, token_id: u64, uri: String) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let creator = Self::only_creator(&env)?;
        
        // Check collection-level freeze
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::MetadataFrozen)
            .unwrap_or(false)
        {
            return Err(Error::MetadataFrozen);
        }
        
        // Check token-level freeze
        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::TokenFrozen(token_id))
            .unwrap_or(false)
        {
            return Err(Error::MetadataFrozen);
        }
        
        // Verify token exists
        if !env.storage().persistent().has(&DataKey::TokenUri(token_id)) {
            return Err(Error::TokenNotFound);
        }
        
        validate_uri(&uri)?;
        let old_uri: Option<String> = env.storage().persistent().get(&DataKey::TokenUri(token_id));
        env.storage()
            .persistent()
            .set(&DataKey::TokenUri(token_id), &uri);
        env.storage().persistent().extend_ttl(
            &DataKey::TokenUri(token_id),
            TTL_THRESHOLD,
            TTL_BUMP,
        );
        
        env.events().publish(
            (symbol_short!("meta_upd"), creator),
            (token_id, old_uri, uri),
        );
        Ok(())
    }

    /// Returns `true` if a specific token's metadata has been permanently frozen.
    pub fn is_token_frozen(env: Env, token_id: u64) -> bool {
        env.storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::TokenFrozen(token_id))
            .unwrap_or(false)
    }

    // ── Royalty management ────────────────────────────────────────────────

    /// Set the collection-level default royalty with bps validation.
    /// Also updates the legacy `royalty_info` storage. Callable only by creator.
    pub fn set_default_royalty(env: Env, receiver: Address, bps: u32) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        if bps > MAX_BPS {
            return Err(Error::InvalidBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::RoyaltyReceiver, &receiver);
        env.storage().instance().set(&DataKey::RoyaltyBps, &bps);
        Ok(())
    }

    /// Set a per-token royalty override with bps validation.
    /// When set, `royalty_info_for(token_id, ..)` uses this instead of the default.
    /// Callable only by creator.
    pub fn set_token_royalty(
        env: Env,
        token_id: u64,
        receiver: Address,
        bps: u32,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        if bps > MAX_BPS {
            return Err(Error::InvalidBps);
        }
        env.storage()
            .persistent()
            .set(&DataKey::TokenRoyaltyReceiver(token_id), &receiver);
        env.storage()
            .persistent()
            .set(&DataKey::TokenRoyaltyBps(token_id), &bps);
        env.storage().persistent().extend_ttl(
            &DataKey::TokenRoyaltyReceiver(token_id),
            TTL_THRESHOLD,
            TTL_BUMP,
        );
        env.storage().persistent().extend_ttl(
            &DataKey::TokenRoyaltyBps(token_id),
            TTL_THRESHOLD,
            TTL_BUMP,
        );
        Ok(())
    }

    /// EIP-2981-style royalty query: returns (recipient, royalty_amount) for a
    /// given token and sale price.
    ///
    /// Resolution order:
    /// 1. Per-token override (`set_token_royalty`) if present.
    /// 2. Collection default (`set_default_royalty` / `update_royalty`).
    ///
    /// `royalty_amount = sale_price * bps / 10_000` (integer division, rounds down).
    pub fn royalty_info_for(
        env: Env,
        token_id: u64,
        sale_price: i128,
    ) -> Result<(Address, i128), Error> {
        let (receiver, bps) = if env
            .storage()
            .persistent()
            .has(&DataKey::TokenRoyaltyBps(token_id))
        {
            let r: Address = env
                .storage()
                .persistent()
                .get(&DataKey::TokenRoyaltyReceiver(token_id))
                .ok_or(Error::NotInitialized)?;
            let b: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::TokenRoyaltyBps(token_id))
                .unwrap_or(0);
            (r, b)
        } else {
            (
                env.storage()
                    .instance()
                    .get(&DataKey::RoyaltyReceiver)
                    .ok_or(Error::NotInitialized)?,
                env.storage()
                    .instance()
                    .get(&DataKey::RoyaltyBps)
                    .unwrap_or(0),
            )
        };

        let amount = sale_price
            .checked_mul(bps as i128)
            .unwrap_or(0)
            .checked_div(MAX_BPS as i128)
            .unwrap_or(0);

        Ok((receiver, amount))
    }

    // ── Minting ───────────────────────────────────────────────────────────

    /// Create a brand new token type, auto-assign the next ID.
    /// Returns the new token_id.
    ///
    /// `amount` may be 0 to register a token type without minting any supply
    /// yet (useful when setting a max supply before first mint).
    /// Blocked while paused. Enforces per-wallet limit if amount > 0.
    pub fn mint_new(env: Env, to: Address, amount: u128, uri: String) -> Result<u64, Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        Self::require_not_paused(&env)?;
        validate_uri(&uri)?;
        let token_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);
        if amount > 0 {
            Self::_check_wallet_limit(&env, &to, token_id, amount)?;
        }
        Self::_mint(&env, &to, token_id, amount, &uri);
        if amount > 0 {
            Self::_update_wallet_minted(&env, &to, token_id, amount);
        }
        env.storage()
            .instance()
            .set(&DataKey::NextTokenId, &(token_id + 1));
        Ok(token_id)
    }

    /// Mint additional supply of an existing token type (explicit token_id).
    ///
    /// Blocked while paused. Enforces supply cap and per-wallet limit if set.
    pub fn mint(
        env: Env,
        to: Address,
        token_id: u64,
        amount: u128,
        uri: String,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        Self::require_not_paused(&env)?;
        if amount == 0 {
            return Err(Error::ZeroAmount);
        }
        Self::_check_supply_cap(&env, token_id, amount)?;
        Self::_check_wallet_limit(&env, &to, token_id, amount)?;
        Self::_mint(&env, &to, token_id, amount, &uri);
        Self::_update_wallet_minted(&env, &to, token_id, amount);
        Ok(())
    }

    /// Batch-mint multiple token types in one call.
    ///
    /// Blocked while paused. Enforces supply cap and per-wallet limit per
    /// token_id, correctly handling duplicate IDs within the same batch by
    /// accumulating amounts before checking caps.
    pub fn mint_batch(
        env: Env,
        to: Address,
        token_ids: Vec<u64>,
        amounts: Vec<u128>,
        uris: Vec<String>,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        Self::require_not_paused(&env)?;

        let len = token_ids.len();
        if len == 0 {
            return Err(Error::EmptyBatch);
        }
        if len > MAX_BATCH_SIZE {
            return Err(Error::BatchTooLarge);
        }
        if token_ids.len() != amounts.len() || token_ids.len() != uris.len() {
            return Err(Error::LengthMismatch);
        }

        // Validate every URI up front (#276) — before any storage mutation.
        for uri in uris.iter() {
            validate_uri(&uri)?;
        }

        // ── Invariant hardening: accumulate amounts per-id within the batch ──
        // This prevents duplicate token IDs from bypassing supply/wallet caps.
        //
        // We use a simple O(n²) dedup scan — no std HashMap in no_std Soroban.
        // For typical batch sizes (tens of items) this is fine.
        let mut checked_ids: Vec<u64> = Vec::new(&env);
        let mut checked_amounts: Vec<u128> = Vec::new(&env);

        for i in 0..len {
            let tid = token_ids.get(i).unwrap();
            let amt = amounts.get(i).unwrap();
            if amt == 0 {
                return Err(Error::ZeroAmount);
            }

            // Find whether `tid` was already seen in this batch.
            let mut found = false;
            for j in 0..checked_ids.len() {
                if checked_ids.get(j).unwrap() == tid {
                    let prev = checked_amounts.get(j).unwrap();
                    // Use saturating_add to handle u128 overflow safely.
                    let new_total = prev.saturating_add(amt);
                    checked_amounts.set(j, new_total);
                    found = true;
                    break;
                }
            }
            if !found {
                checked_ids.push_back(tid);
                checked_amounts.push_back(amt);
            }
        }

        // Pre-flight cap checks on the per-id totals before writing any state.
        for i in 0..checked_ids.len() {
            let tid = checked_ids.get(i).unwrap();
            let total_amt = checked_amounts.get(i).unwrap();
            Self::_check_supply_cap(&env, tid, total_amt)?;
            Self::_check_wallet_limit(&env, &to, tid, total_amt)?;
        }

        // Read NextTokenId once.
        let next_token_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);
        let mut max_token_id = next_token_id;

        // Process each entry in the original (possibly duplicate) list.
        for i in 0..len {
            let token_id = token_ids.get(i).unwrap();
            let amount = amounts.get(i).unwrap();
            let uri = uris.get(i).unwrap();

            if token_id >= max_token_id {
                max_token_id = token_id + 1;
            }

            // URI set on first encounter only.
            if !env.storage().persistent().has(&DataKey::TokenUri(token_id)) {
                env.storage()
                    .persistent()
                    .set(&DataKey::TokenUri(token_id), &uri);
                env.storage().persistent().extend_ttl(
                    &DataKey::TokenUri(token_id),
                    TTL_THRESHOLD,
                    TTL_BUMP,
                );
            }

            // Update balance.
            let balance_key = DataKey::Balance(to.clone(), token_id);
            let cur_bal: u128 = env.storage().persistent().get(&balance_key).unwrap_or(0);
            env.storage()
                .persistent()
                .set(&balance_key, &(cur_bal + amount));
            env.storage()
                .persistent()
                .extend_ttl(&balance_key, TTL_THRESHOLD, TTL_BUMP);

            // Update total supply.
            let supply_key = DataKey::TotalSupply(token_id);
            let cur_supply: u128 = env.storage().persistent().get(&supply_key).unwrap_or(0);
            env.storage()
                .persistent()
                .set(&supply_key, &(cur_supply + amount));
            env.storage()
                .persistent()
                .extend_ttl(&supply_key, TTL_THRESHOLD, TTL_BUMP);

            // Update wallet minted counter.
            let wm_key = DataKey::WalletMinted(to.clone(), token_id);
            let prev_minted: u128 = env.storage().persistent().get(&wm_key).unwrap_or(0);
            env.storage()
                .persistent()
                .set(&wm_key, &(prev_minted + amount));
            env.storage()
                .persistent()
                .extend_ttl(&wm_key, TTL_THRESHOLD, TTL_BUMP);

            let creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
            env.events().publish(
                (symbol_short!("mint"), creator.clone(), to.clone()),
                (token_id, amount),
            );
        }

        if max_token_id > next_token_id {
            env.storage()
                .instance()
                .set(&DataKey::NextTokenId, &max_token_id);
        }

        Ok(())
    }

    // ── Transfers ─────────────────────────────────────────────────────────

    /// Blocked while paused.
    pub fn transfer(
        env: Env,
        from: Address,
        to: Address,
        token_id: u64,
        amount: u128,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        from.require_auth();
        Self::require_not_paused(&env)?;
        Self::_transfer(&env, &from, &to, token_id, amount)
    }

    /// Transfer on behalf of `from` — the owner themselves or an authorized
    /// operator (#275: matches the owner-shortcut already used by
    /// `batch_transfer`/`burn`, so single and batch paths behave the same).
    pub fn transfer_from(
        env: Env,
        operator: Address,
        from: Address,
        to: Address,
        token_id: u64,
        amount: u128,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        operator.require_auth();
        Self::require_not_paused(&env)?;
        if operator != from && !Self::_is_approved_for_all(&env, &operator, &from) {
            return Err(Error::NotApproved);
        }
        Self::_transfer_with_operator(&env, &from, &to, token_id, amount)
    }

    /// Batch transfer — mirrors `safeBatchTransferFrom`. Blocked while paused.
    pub fn batch_transfer(
        env: Env,
        spender: Address,
        from: Address,
        to: Address,
        token_ids: Vec<u64>,
        amounts: Vec<u128>,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        spender.require_auth();
        Self::require_not_paused(&env)?;

        // Allow owner or authorized operator.
        if spender != from && !Self::_is_approved_for_all(&env, &spender, &from) {
            return Err(Error::NotApproved);
        }

        if token_ids.is_empty() {
            return Err(Error::EmptyBatch);
        }
        if token_ids.len() != amounts.len() {
            return Err(Error::LengthMismatch);
        }
        if token_ids.len() > MAX_BATCH_SIZE {
            return Err(Error::BatchTooLarge);
        }

        for i in 0..token_ids.len() {
            let id = token_ids.get(i).unwrap();
            let amount = amounts.get(i).unwrap();
            if amount == 0 {
                return Err(Error::ZeroAmount);
            }
            Self::_transfer(&env, &from, &to, id, amount)?;
        }
        Ok(())
    }

    /// ERC-1155 standard `safeBatchTransferFrom` — alias for `batch_transfer`.
    /// Transfers multiple token types from `from` to `to` atomically.
    /// Requires operator approval if caller is not the owner.
    pub fn batch_transfer_from(
        env: Env,
        operator: Address,
        from: Address,
        to: Address,
        ids: Vec<u64>,
        amounts: Vec<u128>,
        _data: Bytes,
    ) -> Result<(), Error> {
        Self::batch_transfer(env, operator, from, to, ids, amounts)
    }

    // ── Approvals ─────────────────────────────────────────────────────────

    /// Grant or revoke operator-level approval over all tokens owned by the
    /// caller.  `expires_at` — optional ledger sequence after which this
    /// approval is treated as absent.  A past expiry on a new grant is
    /// rejected with `ApprovalExpired`.  Ignored when `approved` is `false`.
    pub fn set_approval_for_all(
        env: Env,
        owner: Address,
        operator: Address,
        approved: bool,
        expires_at: Option<u32>,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        owner.require_auth();

        if approved {
            if let Some(exp) = expires_at {
                if env.ledger().sequence() >= exp {
                    return Err(Error::ApprovalExpired);
                }
            }
        }

        let key = DataKey::ApprovedForAll(owner.clone(), operator.clone());
        let expiry_key = DataKey::ApprovedForAllExpiry(owner.clone(), operator.clone());

        env.storage().persistent().set(&key, &approved);
        env.storage().persistent().extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);

        match (approved, expires_at) {
            (true, Some(exp)) => {
                env.storage().persistent().set(&expiry_key, &exp);
                env.storage()
                    .persistent()
                    .extend_ttl(&expiry_key, TTL_THRESHOLD, TTL_BUMP);
            }
            _ => {
                env.storage().persistent().remove(&expiry_key);
            }
        }

        env.events().publish(
            (symbol_short!("apfa_set"), owner.clone()),
            (operator.clone(), approved, expires_at),
        );
        env.events()
            .publish((symbol_short!("appr_all"), owner), (operator, approved));
        Ok(())
    }

    // ── Burn ──────────────────────────────────────────────────────────────

    /// Blocked while paused.
    pub fn burn(
        env: Env,
        spender: Address,
        from: Address,
        token_id: u64,
        amount: u128,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        spender.require_auth();
        Self::require_not_paused(&env)?;

        // Allow owner or authorized operator.
        if spender != from && !Self::_is_approved_for_all(&env, &spender, &from) {
            return Err(Error::NotApproved);
        }

        let bal: u128 = env
            .storage()
            .persistent()
            .get(&DataKey::Balance(from.clone(), token_id))
            .unwrap_or(0);
        if bal < amount {
            return Err(Error::InsufficientBalance);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone(), token_id), &(bal - amount));
        env.storage().persistent().extend_ttl(
            &DataKey::Balance(from.clone(), token_id),
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        let supply: u128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalSupply(token_id))
            .unwrap_or(0);
        env.storage().persistent().set(
            &DataKey::TotalSupply(token_id),
            &(supply.saturating_sub(amount)),
        );
        env.storage().persistent().extend_ttl(
            &DataKey::TotalSupply(token_id),
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        env.events()
            .publish((symbol_short!("burn"), from.clone()), (token_id, amount));
        Ok(())
    }

    // ── View functions ────────────────────────────────────────────────────

    pub fn balance_of(env: Env, account: Address, token_id: u64) -> u128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(account, token_id))
            .unwrap_or(0)
    }

    /// Batch balance query — mirrors ERC-1155 `balanceOfBatch`.
    pub fn balance_of_batch(env: Env, accounts: Vec<Address>, token_ids: Vec<u64>) -> Vec<u128> {
        if accounts.len() != token_ids.len() {
            return Vec::new(&env);
        }
        let mut result = Vec::new(&env);
        for i in 0..accounts.len() {
            let account = accounts.get(i).unwrap();
            let token_id = token_ids.get(i).unwrap();
            let bal: u128 = env
                .storage()
                .persistent()
                .get(&DataKey::Balance(account.clone(), token_id))
                .unwrap_or(0);
            result.push_back(bal);
        }
        result
    }

    pub fn is_approved_for_all(env: Env, owner: Address, operator: Address) -> bool {
        Self::_is_approved_for_all(&env, &operator, &owner)
    }

    /// Resolve URI for `token_id`.
    ///
    /// Resolution order:
    /// 1. If a base URI is set, returns `base_uri + token_id` (decimal string).
    /// 2. Otherwise returns the per-token URI stored at mint time.
    pub fn uri(env: Env, token_id: u64) -> String {
        if let Some(base) = env
            .storage()
            .instance()
            .get::<DataKey, String>(&DataKey::BaseUri)
        {
            let id_str = u64_to_string(&env, token_id);
            let mut combined: Bytes = base.into();
            let id_bytes: Bytes = id_str.into();
            combined.append(&id_bytes);
            return String::from(&combined);
        }
        env.storage()
            .persistent()
            .get(&DataKey::TokenUri(token_id))
            .unwrap()
    }

    pub fn total_supply(env: Env, token_id: u64) -> u128 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalSupply(token_id))
            .unwrap_or(0)
    }

    pub fn next_token_id(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0)
    }

    /// Marketplace compatibility tag (Issue #841). Must match Symbol `"ERC1155"`.
    pub fn contract_type(env: Env) -> Symbol {
        Symbol::new(&env, "ERC1155")
    }

    pub fn name(env: Env) -> String {
        env.storage().instance().get(&DataKey::Name).unwrap()
    }

    pub fn creator(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Creator).unwrap()
    }

    /// Legacy collection-level royalty view. Kept for backward compatibility.
    pub fn royalty_info(env: Env) -> (Address, u32) {
        (
            env.storage()
                .instance()
                .get(&DataKey::RoyaltyReceiver)
                .unwrap(),
            env.storage()
                .instance()
                .get(&DataKey::RoyaltyBps)
                .unwrap_or(0),
        )
    }

    // ── Admin ─────────────────────────────────────────────────────────────

    pub fn transfer_ownership(env: Env, new_creator: Address) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Creator, &new_creator);
        Ok(())
    }

    /// Step 1 of the two-step creator succession (#484).
    /// Proposes `new_creator` as the next collection administrator.
    pub fn propose_creator(
        env: Env,
        new_creator: Address,
        expires_at: u32,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let current = Self::only_creator(&env)?;
        if env.ledger().sequence() >= expires_at {
            return Err(Error::ApprovalExpired);
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingCreator, &new_creator);
        env.storage()
            .instance()
            .set(&DataKey::PendingCreatorExpiry, &expires_at);
        env.events().publish(
            (symbol_short!("cr_prop"), current),
            (new_creator, expires_at),
        );
        Ok(())
    }

    /// Step 2 of the two-step creator succession (#484).
    /// Proposed successor accepts the role. `OriginalCreator` is unchanged.
    pub fn accept_creator(env: Env, new_creator: Address) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        new_creator.require_auth();
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingCreator)
            .ok_or(Error::NoPendingCreator)?;
        if new_creator != pending {
            return Err(Error::NotPendingCreator);
        }
        let expiry: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PendingCreatorExpiry)
            .unwrap_or(0);
        if env.ledger().sequence() >= expiry {
            return Err(Error::ProposalExpired);
        }
        let old_creator: Address = env
            .storage()
            .instance()
            .get(&DataKey::Creator)
            .ok_or(Error::NotInitialized)?;
        env.storage()
            .instance()
            .set(&DataKey::Creator, &new_creator);
        env.storage().instance().remove(&DataKey::PendingCreator);
        env.storage()
            .instance()
            .remove(&DataKey::PendingCreatorExpiry);
        env.events().publish(
            (symbol_short!("cr_acc"), old_creator),
            new_creator,
        );
        Ok(())
    }

    /// Cancel a pending creator succession proposal (#484). Creator-only.
    pub fn cancel_creator_proposal(env: Env) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let current = Self::only_creator(&env)?;
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingCreator)
            .ok_or(Error::NoPendingCreator)?;
        env.storage().instance().remove(&DataKey::PendingCreator);
        env.storage()
            .instance()
            .remove(&DataKey::PendingCreatorExpiry);
        env.events().publish(
            (symbol_short!("cr_canc"), current),
            pending,
        );
        Ok(())
    }

    /// Returns the original creator address set at initialization (#484).
    pub fn original_creator(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::OriginalCreator)
            .unwrap_or_else(|| {
                env.storage().instance().get(&DataKey::Creator).unwrap()
            })
    }

    /// Returns the pending creator and expiry, or None (#484).
    pub fn pending_creator(env: Env) -> Option<(Address, u32)> {
        let pending = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::PendingCreator)?;
        let expiry = env
            .storage()
            .instance()
            .get::<DataKey, u32>(&DataKey::PendingCreatorExpiry)
            .unwrap_or(0);
        Some((pending, expiry))
    }

    // ── Versioning & Migration ─────────────────────────────────────────────

    pub fn version(env: Env) -> soroban_sdk::String {
        soroban_sdk::String::from_str(&env, "1.0.0")
    }

    pub fn contract_version(env: Env) -> Option<String> {
        env.storage().instance().get(&DataKey::ContractVersion)
    }

    /// Creator-guarded idempotent migration entry point.
    /// v1.0.0: records the completion marker and on-chain version string.
    pub fn migrate(env: Env) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;

        let target = String::from_str(&env, "1.0.0");
        let done_key = DataKey::MigrationDone(target.clone());

        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&done_key)
            .unwrap_or(false)
        {
            return Err(Error::AlreadyMigrated);
        }

        // v1.0.0 migration body: nothing to migrate for the initial version.

        env.storage().persistent().set(&done_key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&done_key, TTL_THRESHOLD, TTL_BUMP);
        env.storage()
            .instance()
            .set(&DataKey::ContractVersion, &target);
        env.events()
            .publish((soroban_sdk::symbol_short!("migrated"), target), ());
        Ok(())
    }

    /// Legacy default-royalty setter. Alias for set_default_royalty without bps
    /// validation (preserved for backward compatibility). New callers should use
    /// `set_default_royalty` which validates bps ≤ MAX_BPS.
    pub fn update_royalty(env: Env, receiver: Address, bps: u32) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        if bps > MAX_BPS {
            return Err(Error::InvalidBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::RoyaltyReceiver, &receiver);
        env.storage().instance().set(&DataKey::RoyaltyBps, &bps);
        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────

    fn extend_instance_ttl(env: &Env) {
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_BUMP);
    }

    fn only_creator(env: &Env) -> Result<Address, Error> {
        let creator: Address = env
            .storage()
            .instance()
            .get(&DataKey::Creator)
            .ok_or(Error::NotInitialized)?;
        creator.require_auth();
        Ok(creator)
    }

    fn require_not_paused(env: &Env) -> Result<(), Error> {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(Error::CollectionPaused);
        }
        Ok(())
    }

    fn _mint(env: &Env, to: &Address, token_id: u64, amount: u128, uri: &String) {
        let bal: u128 = env
            .storage()
            .persistent()
            .get(&DataKey::Balance(to.clone(), token_id))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone(), token_id), &(bal + amount));
        env.storage().persistent().extend_ttl(
            &DataKey::Balance(to.clone(), token_id),
            50_000,
            100_000,
        );

        // URI is set once; resupply mints don't overwrite it.
        if !env.storage().persistent().has(&DataKey::TokenUri(token_id)) {
            env.storage()
                .persistent()
                .set(&DataKey::TokenUri(token_id), uri);
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::TokenUri(token_id), 50_000, 100_000);
        }

        let supply: u128 = env
            .storage()
            .persistent()
            .get(&DataKey::TotalSupply(token_id))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::TotalSupply(token_id), &(supply + amount));
        env.storage().persistent().extend_ttl(
            &DataKey::TotalSupply(token_id),
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        let creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        env.events().publish(
            (symbol_short!("mint"), creator, to.clone()),
            (token_id, amount),
        );
    }

    fn _transfer(
        env: &Env,
        from: &Address,
        to: &Address,
        token_id: u64,
        amount: u128,
    ) -> Result<(), Error> {
        let from_bal: u128 = env
            .storage()
            .persistent()
            .get(&DataKey::Balance(from.clone(), token_id))
            .unwrap_or(0);
        if from_bal < amount {
            return Err(Error::InsufficientBalance);
        }
        env.storage().persistent().set(
            &DataKey::Balance(from.clone(), token_id),
            &(from_bal - amount),
        );
        env.storage().persistent().extend_ttl(
            &DataKey::Balance(from.clone(), token_id),
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        let to_bal: u128 = env
            .storage()
            .persistent()
            .get(&DataKey::Balance(to.clone(), token_id))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone(), token_id), &(to_bal + amount));
        env.storage().persistent().extend_ttl(
            &DataKey::Balance(to.clone(), token_id),
            50_000,
            100_000,
        );

        env.events().publish(
            (symbol_short!("transfer"), from.clone(), to.clone()),
            (token_id, amount),
        );
        Ok(())
    }

    fn _transfer_with_operator(
        env: &Env,
        from: &Address,
        to: &Address,
        token_id: u64,
        amount: u128,
    ) -> Result<(), Error> {
        Self::_transfer(env, from, to, token_id, amount)
    }

    fn _is_approved_for_all(env: &Env, operator: &Address, owner: &Address) -> bool {
        let approved: bool = env
            .storage()
            .persistent()
            .get(&DataKey::ApprovedForAll(owner.clone(), operator.clone()))
            .unwrap_or(false);
        if !approved {
            return false;
        }
        if let Some(exp) = env
            .storage()
            .persistent()
            .get::<DataKey, u32>(&DataKey::ApprovedForAllExpiry(
                owner.clone(),
                operator.clone(),
            ))
        {
            if env.ledger().sequence() >= exp {
                return false;
            }
        }
        true
    }

    fn _check_supply_cap(env: &Env, token_id: u64, amount: u128) -> Result<(), Error> {
        if let Some(max_supply) = env
            .storage()
            .persistent()
            .get::<DataKey, u128>(&DataKey::MaxTokenSupply(token_id))
        {
            if max_supply > 0 {
                let current: u128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::TotalSupply(token_id))
                    .unwrap_or(0);
                if current.saturating_add(amount) > max_supply {
                    return Err(Error::MaxSupplyReached);
                }
            }
        }
        Ok(())
    }

    fn _check_wallet_limit(
        env: &Env,
        wallet: &Address,
        token_id: u64,
        amount: u128,
    ) -> Result<(), Error> {
        let limit: u128 = env
            .storage()
            .instance()
            .get(&DataKey::PerWalletLimit)
            .unwrap_or(0);
        if limit > 0 {
            let already: u128 = env
                .storage()
                .persistent()
                .get(&DataKey::WalletMinted(wallet.clone(), token_id))
                .unwrap_or(0);
            if already.saturating_add(amount) > limit {
                return Err(Error::WalletLimitReached);
            }
        }
        Ok(())
    }

    fn _update_wallet_minted(env: &Env, wallet: &Address, token_id: u64, amount: u128) {
        let key = DataKey::WalletMinted(wallet.clone(), token_id);
        let prev: u128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(prev + amount));
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_BUMP);
    }
}

#[cfg(test)]
mod test;

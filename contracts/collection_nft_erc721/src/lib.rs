//! NormalNFT721 — ERC-721-equivalent on Soroban.
//!
//! Deployed by the Launchpad factory. The `creator` (collection owner) calls
//! `mint` to issue tokens. Standard transfer / approve / burn logic follows
//! ERC-721 semantics.  Royalty info (bps + receiver) is stored on-chain so
//! marketplaces (Litemint, etc.) can query it.
//!
//! ## Approval model (updated)
//! - Per-token `approve(spender, approved, token_id, expires_at)` stores an
//!   optional expiry (ledger sequence).  Expired approvals are treated as absent.
//! - `set_approval_for_all(operator, approved, expires_at)` stores an optional
//!   expiry per (owner, operator) pair.
//! - `is_approved_for_all` checks the flag **and** that the current ledger
//!   sequence is before the stored expiry (when present).
//! - `revoke_all_approvals(token_id)` lets the token owner clear the per-token
//!   approval immediately and emits `ApprovalRevoked`.
#![no_std]
#![allow(clippy::too_many_arguments, deprecated)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Bytes, Env, String,
    Symbol, Vec,
};

/// Shared metadata validation rules (Issue #476).
pub mod metadata;

const TTL_THRESHOLD: u32 = 50_000;
const TTL_BUMP: u32 = 100_000;
const MAX_BPS: u32 = 10_000; // 100% in basis points
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
    NotOwner = 3,
    NotApproved = 4,
    TokenNotFound = 5,
    MaxSupplyReached = 6,
    NotCreator = 7,
    InsufficientBalance = 8,
    MetadataFrozen = 9,
    AlreadyFrozen = 10,
    InvalidBps = 11,
    CollectionPaused = 12,
    ApprovalExpired = 13,
    /// migrate() called for a version already marked done in persistent storage.
    AlreadyMigrated = 14,
    /// Unsupported version jump — only sequential upgrades are permitted.
    UnsupportedMigration = 15,
    /// Empty URI provided.
    EmptyUri = 16,
    /// URI exceeds maximum length.
    UriTooLong = 17,
    /// Empty batch provided.
    EmptyBatch = 18,
    /// Batch exceeds maximum size.
    BatchTooLarge = 19,
    /// Collection name is empty (Issue #476).
    EmptyName = 20,
    /// Collection name exceeds maximum length (Issue #476).
    NameTooLong = 21,
    /// Collection symbol is empty (Issue #476).
    EmptySymbol = 22,
    /// Collection symbol exceeds maximum length (Issue #476).
    SymbolTooLong = 23,
    /// max_supply is zero or exceeds the platform cap (Issue #476).
    InvalidMaxSupply = 24,
    /// accept_creator() called when no succession proposal is pending (#484).
    NoPendingCreator = 25,
    /// accept_creator() called by an address that is not the proposed successor (#484).
    NotPendingCreator = 26,
    /// accept_creator() called after the pending proposal's expiry ledger (#484).
    ProposalExpired = 27,
}

// ─── Storage Keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    // Instance storage — cheap, shared TTL with contract instance
    Initialized,
    Creator,
    /// Original creator address — set once at initialization and never changed
    /// even after a successful two-step creator succession (#484).  Used to
    /// preserve historical royalty attribution and voucher revocation authority
    /// for pre-succession tokens.
    OriginalCreator,
    /// Pending successor address proposed via `propose_creator` (#484).
    PendingCreator,
    /// Ledger sequence at which the pending proposal expires (#484).
    /// Redemptions after this sequence are rejected with `ProposalExpired`.
    PendingCreatorExpiry,
    Name,
    Symbol,
    MaxSupply,
    NextTokenId,
    TotalSupply,
    RoyaltyBps,
    RoyaltyReceiver,
    // Persistent storage — per-token / per-owner, independent TTL
    Owner(u64),
    TokenUri(u64),
    Approved(u64),
    /// Optional expiry (ledger sequence) for a per-token approval.
    /// Absent means the approval never expires.
    ApprovedExpiry(u64),
    BalanceOf(Address),
    ApprovedForAll(Address, Address),
    /// Optional expiry (ledger sequence) for an operator-level approval.
    /// Absent means the approval never expires.
    ApprovedForAllExpiry(Address, Address),
    BaseUri,        // String — collection-level base URI (optional)
    MetadataFrozen, // bool   — permanently frozen when true
    TokenFrozen(u64), // bool  — per-token metadata frozen when true
    // Per-token royalty overrides (persistent, optional)
    TokenRoyaltyReceiver(u64), // Address
    TokenRoyaltyBps(u64),      // u32
    Paused,                    // bool   — minting paused when true
    // ── Versioned migration registry ─────────────────────────────────────
    /// Completion marker: present when migration to `version` is done.
    MigrationDone(String),
    /// Resumable progress cursor during an in-flight migration.
    MigrationCursor(String),
    /// Version string last written to on-chain storage by `migrate()`.
    ContractVersion,
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Convert a u64 to its decimal ASCII representation as a Soroban `String`.
/// Used to build `base_uri + token_id` paths in `token_uri()`.
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
pub struct NormalNFT721;

#[contractimpl]
#[allow(clippy::too_many_arguments)]
impl NormalNFT721 {
    // ── Initializer (called by Launchpad factory) ──────────────────────────

    pub fn initialize(
        env: Env,
        creator: Address,
        name: String,
        symbol: String,
        max_supply: u64,  // pass u64::MAX for unlimited
        royalty_bps: u32, // e.g. 500 = 5%
        royalty_receiver: Address,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Initialized) {
            return Err(Error::AlreadyInitialized);
        }
        // Issue #476: apply shared metadata validation rules before writing any
        // state so an invalid combination cannot leave the collection partially
        // initialised.
        metadata::validate_name(&name, Error::EmptyName, Error::NameTooLong)?;
        metadata::validate_symbol(&symbol, Error::EmptySymbol, Error::SymbolTooLong)?;
        metadata::validate_max_supply(max_supply, Error::InvalidMaxSupply)?;
        metadata::validate_royalty_bps(royalty_bps, Error::InvalidBps)?;

        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Creator, &creator);
        // OriginalCreator is set once at initialization and never overwritten
        // by succession — it preserves historical royalty attribution (#484).
        env.storage().instance().set(&DataKey::OriginalCreator, &creator);
        env.storage().instance().set(&DataKey::Name, &name);
        env.storage().instance().set(&DataKey::Symbol, &symbol);
        env.storage()
            .instance()
            .set(&DataKey::MaxSupply, &max_supply);
        env.storage().instance().set(&DataKey::NextTokenId, &0u64);
        env.storage().instance().set(&DataKey::TotalSupply, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::RoyaltyBps, &royalty_bps);
        env.storage()
            .instance()
            .set(&DataKey::RoyaltyReceiver, &royalty_receiver);
        env.storage().instance().extend_ttl(50_000, 100_000);
        Ok(())
    }

    pub fn next_token_id(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0)
    }

    /// Marketplace compatibility tag (Issue #841). Must match
    /// `CollectionStandard::Erc721` / Symbol `"ERC721"`.
    pub fn contract_type(env: Env) -> Symbol {
        Symbol::new(&env, "ERC721")
    }

    // ── Minting ───────────────────────────────────────────────────────────

    /// Creator mints a single token to `to` with the given metadata URI.
    /// Returns the new token_id.
    pub fn mint(env: Env, to: Address, uri: String) -> Result<u64, Error> {
        Self::extend_instance_ttl(&env);
        let creator = Self::only_creator(&env)?;

        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(Error::CollectionPaused);
        }

        validate_uri(&uri)?;

        let token_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);
        let max: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MaxSupply)
            .unwrap_or(u64::MAX);

        if token_id >= max {
            return Err(Error::MaxSupplyReached);
        }

        Self::_do_mint(&env, &to, token_id, &uri);

        env.events().publish(
            (symbol_short!("mint"), creator.clone(), to.clone()),
            (token_id, 1u128),
        );
        Ok(token_id)
    }

    /// Batch mint multiple tokens to the same recipient.
    /// Optimized to minimize storage I/O - reads storage once, mints in memory, writes back once.
    pub fn batch_mint(env: Env, to: Address, uris: Vec<String>) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let creator = Self::only_creator(&env)?;

        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(Error::CollectionPaused);
        }

        let uris_len = uris.len();
        if uris_len == 0 {
            return Err(Error::EmptyBatch);
        }
        if uris_len > MAX_BATCH_SIZE {
            return Err(Error::BatchTooLarge);
        }

        // Validate every URI up front (#276) — before any storage mutation,
        // so a single malformed entry can't leave a partially-minted batch.
        for uri in uris.iter() {
            validate_uri(&uri)?;
        }

        // Read storage ONCE before the loop
        let mut next_token_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);
        let max_supply: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MaxSupply)
            .unwrap_or(u64::MAX);
        let mut total_supply: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);

        // Check if we have enough supply for all tokens
        if next_token_id + (uris_len as u64) > max_supply {
            return Err(Error::MaxSupplyReached);
        }

        // Get current balance once (will be incremented for each mint)
        let mut current_balance: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::BalanceOf(to.clone()))
            .unwrap_or(0);

        // Collect token IDs to emit events
        let mut minted_ids = Vec::new(&env);

        // Mint all tokens in memory first
        for uri in uris.iter() {
            let token_id = next_token_id;

            // Store token data
            env.storage()
                .persistent()
                .set(&DataKey::Owner(token_id), &to);
            env.storage()
                .persistent()
                .set(&DataKey::TokenUri(token_id), &uri);
            env.storage().persistent().extend_ttl(
                &DataKey::Owner(token_id),
                TTL_THRESHOLD,
                TTL_BUMP,
            );
            env.storage().persistent().extend_ttl(
                &DataKey::TokenUri(token_id),
                TTL_THRESHOLD,
                TTL_BUMP,
            );

            minted_ids.push_back(token_id);

            // Increment in memory
            next_token_id += 1;
            total_supply += 1;
            current_balance += 1;
        }

        // Write back to storage ONCE after the loop
        env.storage()
            .instance()
            .set(&DataKey::NextTokenId, &next_token_id);
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &total_supply);
        env.storage()
            .persistent()
            .set(&DataKey::BalanceOf(to.clone()), &current_balance);
        env.storage().persistent().extend_ttl(
            &DataKey::BalanceOf(to.clone()),
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        for token_id in minted_ids.iter() {
            env.events().publish(
                (symbol_short!("mint"), creator.clone(), to.clone()),
                (token_id, 1u128),
            );
        }

        Ok(())
    }

    // ── Transfers ─────────────────────────────────────────────────────────

    /// Owner transfers their token.  Blocked while collection is paused.
    pub fn transfer(env: Env, from: Address, to: Address, token_id: u64) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        from.require_auth();
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(Error::CollectionPaused);
        }
        Self::_transfer(&env, &from, &to, token_id)
    }

    /// Approved spender (or operator) transfers on behalf of owner.  Blocked while paused.
    pub fn transfer_from(
        env: Env,
        spender: Address,
        from: Address,
        to: Address,
        token_id: u64,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        spender.require_auth();
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(Error::CollectionPaused);
        }
        Self::_check_approved(&env, &spender, &from, token_id)?;
        // clear single-token approval + its expiry on transfer
        env.storage()
            .persistent()
            .remove(&DataKey::Approved(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::ApprovedExpiry(token_id));
        Self::_transfer(&env, &from, &to, token_id)
    }

    // ── Approvals ─────────────────────────────────────────────────────────

    /// Grant `approved` permission to transfer `token_id`.
    ///
    /// `expires_at` — optional ledger sequence number after which this
    /// approval is treated as absent.  Passing a sequence that has already
    /// passed is rejected with `ApprovalExpired`.
    pub fn approve(
        env: Env,
        spender: Address, // caller — must be owner or authorized operator
        approved: Address,
        token_id: u64,
        expires_at: Option<u32>,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        spender.require_auth();
        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(Error::TokenNotFound)?;

        // [SECURITY] Allow owner or authorized operator to approve (#48)
        if spender != owner
            && !Self::is_approved_for_all(env.clone(), owner.clone(), spender.clone())
        {
            return Err(Error::NotApproved);
        }

        // Reject an expiry that is already in the past.
        if let Some(exp) = expires_at {
            if env.ledger().sequence() >= exp {
                return Err(Error::ApprovalExpired);
            }
        }

        env.storage()
            .persistent()
            .set(&DataKey::Approved(token_id), &approved);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Approved(token_id), TTL_THRESHOLD, TTL_BUMP);

        // Store or clear expiry
        match expires_at {
            Some(exp) => {
                env.storage()
                    .persistent()
                    .set(&DataKey::ApprovedExpiry(token_id), &exp);
                env.storage()
                    .persistent()
                    .extend_ttl(&DataKey::ApprovedExpiry(token_id), TTL_THRESHOLD, TTL_BUMP);
            }
            None => {
                env.storage()
                    .persistent()
                    .remove(&DataKey::ApprovedExpiry(token_id));
            }
        }

        // Emit ApprovalSet (primary) + legacy approve topic
        env.events().publish(
            (symbol_short!("appr_set"), owner.clone()),
            (approved.clone(), token_id, expires_at),
        );
        env.events()
            .publish((symbol_short!("approve"), owner), (approved, token_id));
        Ok(())
    }

    /// Grant or revoke operator-level approval over all tokens owned by
    /// the caller.
    ///
    /// `expires_at` — optional ledger sequence number after which this
    /// approval is treated as absent.  Ignored when `approved` is `false`.
    pub fn set_approval_for_all(
        env: Env,
        owner: Address,
        operator: Address,
        approved: bool,
        expires_at: Option<u32>,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        owner.require_auth();

        // Reject an expiry already in the past (only relevant when granting).
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

        // Store or clear expiry
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

        // Emit ApprovalForAllSet (primary) + legacy appr_all topic
        env.events().publish(
            (symbol_short!("apfa_set"), owner.clone()),
            (operator.clone(), approved, expires_at),
        );
        env.events()
            .publish((symbol_short!("appr_all"), owner), (operator, approved));
        Ok(())
    }

    /// Remove the per-token approval for `token_id` immediately.
    ///
    /// Only the token owner can call this.  Emits `ApprovalRevoked`.
    pub fn revoke_all_approvals(env: Env, token_id: u64) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(Error::TokenNotFound)?;
        owner.require_auth();

        let had_approval = env
            .storage()
            .persistent()
            .has(&DataKey::Approved(token_id));

        env.storage()
            .persistent()
            .remove(&DataKey::Approved(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::ApprovedExpiry(token_id));

        if had_approval {
            // Emit ApprovalRevoked event
            env.events().publish(
                (symbol_short!("appr_rev"), owner),
                token_id,
            );
        }
        Ok(())
    }

    // ── Burn ──────────────────────────────────────────────────────────────

    /// Burn a token. Blocked while collection is paused. Also clears any
    /// per-token royalty override so stale entries cannot accumulate.
    pub fn burn(env: Env, spender: Address, token_id: u64) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        spender.require_auth();
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(Error::CollectionPaused);
        }
        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(Error::TokenNotFound)?;

        // [SECURITY] Allow owner, approved spender, or operator to burn (#48)
        Self::_check_approved(&env, &spender, &owner, token_id)?;

        let bal: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::BalanceOf(owner.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::BalanceOf(owner.clone()), &(bal.saturating_sub(1)));
        env.storage().persistent().extend_ttl(
            &DataKey::BalanceOf(owner.clone()),
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        env.storage().persistent().remove(&DataKey::Owner(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::TokenUri(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::Approved(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::ApprovedExpiry(token_id));
        // Clear per-token royalty overrides so they cannot be read back as
        // stale data after the token ceases to exist.
        env.storage()
            .persistent()
            .remove(&DataKey::TokenRoyaltyReceiver(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::TokenRoyaltyBps(token_id));

        let supply: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply.saturating_sub(1)));

        env.events()
            .publish((symbol_short!("burn"), owner), token_id);
        Ok(())
    }

    // ── View functions ────────────────────────────────────────────────────

    pub fn owner_of(env: Env, token_id: u64) -> Result<Address, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(Error::TokenNotFound)
    }

    pub fn token_uri(env: Env, token_id: u64) -> Result<String, Error> {
        // Verify the token exists first.
        if !env.storage().persistent().has(&DataKey::Owner(token_id)) {
            return Err(Error::TokenNotFound);
        }
        // If a base URI is set, return base_uri + token_id.
        if let Some(base) = env
            .storage()
            .instance()
            .get::<DataKey, String>(&DataKey::BaseUri)
        {
            let id_str = u64_to_string(&env, token_id);
            let mut combined: Bytes = base.into();
            let id_bytes: Bytes = id_str.into();
            combined.append(&id_bytes);
            return Ok(String::from(&combined));
        }
        // Fall back to per-token URI stored at mint time.
        env.storage()
            .persistent()
            .get(&DataKey::TokenUri(token_id))
            .ok_or(Error::TokenNotFound)
    }

    pub fn balance_of(env: Env, owner: Address) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::BalanceOf(owner))
            .unwrap_or(0)
    }

    pub fn total_supply(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    pub fn max_supply(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MaxSupply)
            .unwrap_or(u64::MAX)
    }

    pub fn name(env: Env) -> String {
        env.storage().instance().get(&DataKey::Name).unwrap()
    }

    pub fn symbol(env: Env) -> String {
        env.storage().instance().get(&DataKey::Symbol).unwrap()
    }

    pub fn creator(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Creator).unwrap()
    }

    /// Returns (royalty_receiver, royalty_bps) for the collection default.
    /// Preserved for backward compatibility with existing marketplace integrations.
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

    /// EIP-2981-style royalty query: returns (recipient, royalty_amount) for a
    /// given token and sale price.  Per-token overrides take priority over the
    /// collection default.  Returns `TokenNotFound` if the token does not exist.
    /// Royalty amount = sale_price * bps / 10_000 using checked arithmetic.
    pub fn royalty_info_for(
        env: Env,
        token_id: u64,
        sale_price: i128,
    ) -> Result<(Address, i128), Error> {
        // Reject queries for non-existent (never minted or already burnt) tokens.
        if !env.storage().persistent().has(&DataKey::Owner(token_id)) {
            return Err(Error::TokenNotFound);
        }
        // Resolve recipient and bps — per-token override wins if present.
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

        // Checked arithmetic: sale_price * bps / MAX_BPS
        let amount = sale_price
            .checked_mul(bps as i128)
            .unwrap_or(0)
            .checked_div(MAX_BPS as i128)
            .unwrap_or(0);

        Ok((receiver, amount))
    }

    pub fn get_approved(env: Env, token_id: u64) -> Option<Address> {
        // Return None if the approval has expired
        if let Some(exp) = env
            .storage()
            .persistent()
            .get::<DataKey, u32>(&DataKey::ApprovedExpiry(token_id))
        {
            if env.ledger().sequence() >= exp {
                return None;
            }
        }
        env.storage().persistent().get(&DataKey::Approved(token_id))
    }

    /// Returns `true` when `operator` has a valid (non-expired)
    /// approval-for-all over `owner`'s tokens.
    pub fn is_approved_for_all(env: Env, owner: Address, operator: Address) -> bool {
        let flag: bool = env
            .storage()
            .persistent()
            .get(&DataKey::ApprovedForAll(owner.clone(), operator.clone()))
            .unwrap_or(false);

        if !flag {
            return false;
        }

        // Check expiry when present
        if let Some(exp) = env
            .storage()
            .persistent()
            .get::<DataKey, u32>(&DataKey::ApprovedForAllExpiry(owner, operator))
        {
            if env.ledger().sequence() >= exp {
                return false;
            }
        }

        true
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
    ///
    /// The current creator proposes `new_creator` as the next collection
    /// administrator.  The proposal expires at `expires_at` (ledger sequence).
    ///
    /// - Overwrites any previously pending proposal.
    /// - Does NOT transfer any authority until `accept_creator` is called.
    /// - If `expires_at` is in the past the call reverts with `ApprovalExpired`.
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
    ///
    /// The proposed successor accepts the role.  After a successful call:
    /// - `Creator` is updated to `new_creator`.
    /// - `OriginalCreator` is **unchanged** so historical royalty attribution
    ///    and voucher revocation records remain intact.
    /// - The pending proposal and expiry are cleared.
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

    /// Cancel a pending creator succession proposal (#484).  Only the current
    /// creator may cancel.  Returns `NoPendingCreator` if no proposal exists.
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
    /// This never changes even after a successful creator succession.
    pub fn original_creator(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::OriginalCreator)
            // Fallback for collections initialized before #484: treat the
            // current Creator as the original creator (safe because succession
            // could not have happened without OriginalCreator being set first).
            .unwrap_or_else(|| {
                env.storage().instance().get(&DataKey::Creator).unwrap()
            })
    }

    /// Returns the pending creator address and expiry, or None if no proposal
    /// is currently active (#484).
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

    /// Set the collection-level default royalty with bps validation.
    /// Replaces any previously stored default.  Callable only by creator.
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
    /// Callable only by creator.  The token must exist.
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
        if !env.storage().persistent().has(&DataKey::Owner(token_id)) {
            return Err(Error::TokenNotFound);
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

    /// Pause minting.  Callable only by creator.
    pub fn pause(env: Env) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    /// Resume minting.  Callable only by creator.
    pub fn unpause(env: Env) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Returns `true` if minting is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false)
    }

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

    /// Permanently freeze metadata.  Can only be called once; subsequent calls
    /// revert with `AlreadyFrozen`.  Callable only by creator.
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
    /// collection owner or the token owner.
    pub fn freeze_token(env: Env, caller: Address, token_id: u64) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        caller.require_auth();
        
        // Verify token exists
        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(Error::TokenNotFound)?;
        
        // Allow creator or token owner
        let creator = Self::only_creator(&env).ok();
        if creator != Some(caller.clone()) && owner != caller {
            return Err(Error::NotOwner);
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
        if !env.storage().persistent().has(&DataKey::Owner(token_id)) {
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

    /// Returns the stored base URI, or `None` if unset.
    pub fn base_uri(env: Env) -> Option<String> {
        env.storage().instance().get(&DataKey::BaseUri)
    }

    // ── Versioning & Migration ─────────────────────────────────────────────

    /// Semantic version compiled into this WASM.
    pub fn version(env: Env) -> soroban_sdk::String {
        soroban_sdk::String::from_str(&env, "1.0.0")
    }

    /// On-chain version string last written by `migrate()`, or `None` before
    /// the first migration.
    pub fn contract_version(env: Env) -> Option<String> {
        env.storage().instance().get(&DataKey::ContractVersion)
    }

    /// Creator-guarded, idempotent storage migration entry point.
    ///
    /// Reverts with `AlreadyMigrated` when the "1.0.0" marker is already set.
    ///
    /// **v1.0.0 migration**: this is the initial version; the migration body
    /// is intentionally empty — calling it simply records the completion marker
    /// and the on-chain version string so operators can verify the upgrade
    /// script applied correctly.  Future versions will add data-backfill steps
    /// here following the same idempotent-by-marker pattern.
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

        // ── v1.0.0 migration body ─────────────────────────────────────────
        // Nothing to migrate for the initial version.  Future versions insert
        // data-backfill logic here before recording the marker.
        // ─────────────────────────────────────────────────────────────────

        // Record completion marker (persistent so it survives instance bumps).
        env.storage().persistent().set(&done_key, &true);
        env.storage().persistent().extend_ttl(
            &done_key,
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        // Write the version to instance storage for operator verification.
        env.storage()
            .instance()
            .set(&DataKey::ContractVersion, &target);

        env.events().publish(
            (soroban_sdk::symbol_short!("migrated"), target),
            (),
        );
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

    fn _do_mint(env: &Env, to: &Address, token_id: u64, uri: &String) {
        env.storage()
            .persistent()
            .set(&DataKey::Owner(token_id), to);
        env.storage()
            .persistent()
            .set(&DataKey::TokenUri(token_id), uri);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Owner(token_id), 50_000, 100_000);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::TokenUri(token_id), 50_000, 100_000);

        let bal: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::BalanceOf(to.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::BalanceOf(to.clone()), &(bal + 1));
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::BalanceOf(to.clone()), 50_000, 100_000);

        let next: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NextTokenId, &(next + 1));

        let supply: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply + 1));
    }

    fn _transfer(env: &Env, from: &Address, to: &Address, token_id: u64) -> Result<(), Error> {
        // [SECURITY] Clear single-token approval + expiry on every transfer (#50)
        env.storage()
            .persistent()
            .remove(&DataKey::Approved(token_id));
        env.storage()
            .persistent()
            .remove(&DataKey::ApprovedExpiry(token_id));

        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(Error::TokenNotFound)?;
        if owner != *from {
            return Err(Error::NotOwner);
        }

        let from_bal: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::BalanceOf(from.clone()))
            .unwrap_or(0);
        env.storage().persistent().set(
            &DataKey::BalanceOf(from.clone()),
            &(from_bal.saturating_sub(1)),
        );
        env.storage().persistent().extend_ttl(
            &DataKey::BalanceOf(from.clone()),
            TTL_THRESHOLD,
            TTL_BUMP,
        );

        let to_bal: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::BalanceOf(to.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::BalanceOf(to.clone()), &(to_bal + 1));
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::BalanceOf(to.clone()), 50_000, 100_000);

        env.storage()
            .persistent()
            .set(&DataKey::Owner(token_id), to);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Owner(token_id), TTL_THRESHOLD, TTL_BUMP);
        env.events().publish(
            (symbol_short!("transfer"), from.clone(), to.clone()),
            (token_id, 1u128),
        );
        Ok(())
    }

    fn _check_approved(
        env: &Env,
        spender: &Address,
        from: &Address,
        token_id: u64,
    ) -> Result<(), Error> {
        // Check single-token approval (respecting expiry)
        if let Some(approved) = env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::Approved(token_id))
        {
            if approved == *spender {
                // Verify the approval has not expired
                let expired = env
                    .storage()
                    .persistent()
                    .get::<DataKey, u32>(&DataKey::ApprovedExpiry(token_id))
                    .map(|exp| env.ledger().sequence() >= exp)
                    .unwrap_or(false);

                if !expired {
                    return Ok(());
                }
            }
        }
        // Check operator approval (with expiry handled inside is_approved_for_all)
        if Self::is_approved_for_all(env.clone(), from.clone(), spender.clone()) {
            return Ok(());
        }
        Err(Error::NotApproved)
    }
}

#[cfg(test)]
mod test;

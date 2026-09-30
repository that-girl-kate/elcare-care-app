//! LazyMint721 — Lazy-minting ERC-721-equivalent on Soroban.
//!
//! # How lazy minting works
//!
//! 1. Creator builds a `MintVoucher` off-chain.
//! 2. Creator hashes it with `sha256(contract_addr ‖ token_id ‖ price ‖ valid_until ‖ uri_hash ‖ currency_xdr)`
//!    and signs the 32-byte digest with their ed25519 private key.
//! 3. Buyer submits the voucher + signature on-chain via `redeem()`.
//! 4. Contract re-hashes, verifies ed25519, takes payment, then mints.
//!
//! # Replay protection (#39)
//! Every redeemed `token_id` is tracked in `UsedVoucher`. Once redeemed it
//! can never be claimed again (`VoucherAlreadyRedeemed`).
//!
//! # Voucher revocation
//! The creator can revoke a specific voucher nonce before it is redeemed via
//! `revoke_voucher(nonce)` or batch-revoke with `revoke_vouchers(nonces)`.
//! Attempting to redeem a revoked voucher returns `VoucherRevoked`.
//! Revoking an already-redeemed nonce returns `VoucherAlreadyRedeemed`.
//!
//! # Merkle allowlist
//! A Merkle-root-based allowlist phase gates redemptions before
//! `set_public_phase()` is called.
//!
//! # Platform fee (#38)
//! A per-collection `platform_fee_bps` is stored at initialization. When a
//! buyer redeems a priced voucher the fee portion is transferred to
//! `platform_fee_receiver` and the remainder to the creator.
//!
//! # Batch redemption
//! `redeem_batch` verifies and mints multiple vouchers atomically (all-or-nothing).
//! Payments are aggregated per currency to minimise token transfer calls.
#![no_std]
#![allow(clippy::too_many_arguments, deprecated)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short,
    token::Client as TokenClient, xdr::ToXdr, Address, Bytes, BytesN, Env, Map, String, Symbol, Vec,
};

/// Shared metadata validation rules (Issue #476).
pub mod metadata;

const TTL_THRESHOLD: u32 = 50_000;
const TTL_BUMP: u32 = 100_000;
/// Maximum number of vouchers accepted by a single redeem_batch call (#274).
const MAX_BATCH_SIZE: u32 = 100;
const MAX_BPS: u32 = 10_000;
/// Maximum allowed byte length for a token URI (#481).
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
    VoucherExpired = 7,
    /// Voucher nonce already redeemed (#273).
    VoucherAlreadyRedeemed = 8,
    NotCreator = 9,
    InvalidSignature = 10,
    NotAllowlisted = 11,
    InvalidMerkleProof = 12,
    VoucherRevoked = 13,
    /// migrate() called for a version already marked done.
    AlreadyMigrated = 14,
    /// Unsupported version jump.
    UnsupportedMigration = 15,
    /// redeem_batch called with an empty items list.
    EmptyBatch = 16,
    /// redeem_batch called with more items than MAX_BATCH_SIZE.
    BatchTooLarge = 17,
    /// redeem_batch contains two items with the same voucher token_id.
    DuplicateVoucherInBatch = 18,
    /// set_approval_for_all called with an already-past `expires_at`.
    ApprovalExpired = 19,
    /// Royalty BPS exceeds 10 000 (100 %).
    InvalidBps = 20,
    /// Collection name is empty (Issue #476).
    EmptyName = 21,
    /// Collection name exceeds maximum length (Issue #476).
    NameTooLong = 22,
    /// Collection symbol is empty (Issue #476).
    EmptySymbol = 23,
    /// Collection symbol exceeds maximum length (Issue #476).
    SymbolTooLong = 24,
    /// max_supply is zero or exceeds the platform cap (Issue #476).
    InvalidMaxSupply = 25,
    /// accept_creator() called when no succession proposal is pending (#484).
    NoPendingCreator = 26,
    /// accept_creator() called by an address that is not the proposed successor (#484).
    NotPendingCreator = 27,
    /// accept_creator() called after the pending proposal's expiry ledger (#484).
    ProposalExpired = 28,
    /// redeem/check_voucher called with an empty URI.
    EmptyUri = 29,
    /// redeem/check_voucher called with a URI exceeding MAX_URI_LEN bytes.
    UriTooLong = 30,
}

// ─── Data types ───────────────────────────────────────────────────────────────

/// Off-chain voucher created by the collection creator.
///
/// `uri_hash` = sha256(uri_string) computed off-chain; included in the signed
/// digest so a relayer cannot swap the URI while keeping the signature valid.
///
/// # Issue #273 — nonce-based replay protection
/// `nonce` is the unique per-voucher identifier used for replay protection and
/// revocation.  It is intentionally separate from `token_id` so that:
///   * A creator can issue multiple vouchers for the same token at different
///     prices / recipients without one redemption invalidating the others.
///   * The nonce can be incremented independently of the on-chain mint counter.
///
/// The signed digest now also includes the network passphrase bound at
/// initialization, preventing cross-deployment and cross-network replay.
#[contracttype]
#[derive(Clone)]
pub struct MintVoucher {
    pub token_id: u64,
    /// Unique per-voucher identifier used for replay protection (#273).
    /// Must be unique across all vouchers for this contract instance.
    pub nonce: u64,
    pub price: i128,          // 0 = free
    pub currency: Address,    // SAC address (ignored when price == 0)
    pub uri: String,          // IPFS / HTTPS metadata URI
    pub uri_hash: BytesN<32>, // sha256(uri bytes) — included in signature
    pub valid_until: u64,     // ledger sequence; 0 = no expiry
}

/// One element of a `redeem_batch` call.
#[contracttype]
#[derive(Clone)]
pub struct BatchVoucherItem {
    pub voucher: MintVoucher,
    pub signature: BytesN<64>,
    pub merkle_proof: Vec<BytesN<32>>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Initialized,
    Creator,
    CreatorPubkey,
    CurrentWasmHash,
    Name,
    Symbol,
    MaxSupply,
    NextTokenId,
    TotalSupply,
    RoyaltyBps,
    RoyaltyReceiver,
    /// Platform fee receiver address (#38).
    PlatformFeeReceiver,
    /// Platform fee in basis points (#38).
    PlatformFeeBps,
    Owner(u64),
    TokenUri(u64),
    Approved(u64),
    BalanceOf(Address),
    ApprovedForAll(Address, Address),
    ApprovedForAllExpiry(Address, Address), // (owner, operator) → u32 ledger sequence
    UsedVoucher(u64),    // nonce → bool  (redeemed)
    RevokedVoucher(u64), // nonce → bool  (creator-revoked, per-nonce)
    MerkleRoot,          // BytesN<32> — root of allowlist Merkle tree
    IsPublicPhase,       // bool — true once public minting is enabled
    /// Network passphrase bound at initialization.
    /// Included in the signed digest to prevent cross-network replay (#273).
    NetworkPassphrase,   // String
    /// On-chain version string written by migrate().
    ContractVersion,
    /// Migration completion marker (version string → bool).
    MigrationDone(soroban_sdk::String),
    /// Original creator address set at initialization — never changed by
    /// succession (#484).
    OriginalCreator,
    /// Pending successor proposed via `propose_creator` (#484).
    PendingCreator,
    /// Ledger sequence at which the pending proposal expires (#484).
    PendingCreatorExpiry,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct LazyMint721;

impl LazyMint721 {
    fn verify_signature_or_panic(
        env: &Env,
        pubkey: &BytesN<32>,
        digest: &Bytes,
        signature: &BytesN<64>,
    ) {
        env.crypto().ed25519_verify(pubkey, digest, signature);
    }

    /// Verify a standard binary Merkle proof against `root`.
    /// Leaf = sha256(address XDR).  Siblings are sorted (smaller first) at each
    /// level so proofs are position-independent (OpenZeppelin convention).
    fn verify_merkle_proof(
        env: &Env,
        root: &BytesN<32>,
        leaf_preimage: &Address,
        proof: &Vec<BytesN<32>>,
    ) -> bool {
        let mut computed: BytesN<32> = env
            .crypto()
            .sha256(&leaf_preimage.clone().to_xdr(env))
            .into();
        for sibling in proof.iter() {
            let mut pair = Bytes::new(env);
            if computed.to_array() <= sibling.to_array() {
                pair.append(&computed.clone().into());
                pair.append(&sibling.clone().into());
            } else {
                pair.append(&sibling.into());
                pair.append(&computed.clone().into());
            }
            computed = env.crypto().sha256(&pair).into();
        }
        &computed == root
    }

    /// Enforce the allowlist gate for `buyer`.  No-op in public phase.
    fn check_allowlist(
        env: &Env,
        buyer: &Address,
        merkle_proof: &Vec<BytesN<32>>,
    ) -> Result<(), Error> {
        let is_public: bool = env
            .storage()
            .instance()
            .get(&DataKey::IsPublicPhase)
            .unwrap_or(false);
        if is_public {
            return Ok(());
        }
        let root: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::MerkleRoot)
            .ok_or(Error::NotAllowlisted)?;
        if merkle_proof.is_empty() {
            return Err(Error::NotAllowlisted);
        }
        if !Self::verify_merkle_proof(env, &root, buyer, merkle_proof) {
            return Err(Error::InvalidMerkleProof);
        }
        Ok(())
    }

    /// Validate a single voucher (expiry → replay → revocation → supply → sig).
    /// Does NOT write state or transfer funds.
    fn check_voucher(
        env: &Env,
        voucher: &MintVoucher,
        signature: &BytesN<64>,
        pubkey: &BytesN<32>,
        max: u64,
        next_id: u64,
    ) -> Result<(), Error> {
        if voucher.valid_until != 0 && env.ledger().sequence() > voucher.valid_until as u32 {
            env.events()
                .publish((symbol_short!("expired"),), voucher.nonce);
            return Err(Error::VoucherExpired);
        }
        // URI boundary validation (#276)
        let uri_len = voucher.uri.len();
        if uri_len == 0 {
            return Err(Error::EmptyUri);
        }
        if uri_len > MAX_URI_LEN {
            return Err(Error::UriTooLong);
        }
        // Replay protection uses the voucher's nonce (not token_id) so the same
        // token can be covered by multiple vouchers with independent lifetimes.
        if env
            .storage()
            .persistent()
            .has(&DataKey::UsedVoucher(voucher.nonce))
        {
            return Err(Error::VoucherAlreadyRedeemed);
        }
        if env
            .storage()
            .persistent()
            .has(&DataKey::RevokedVoucher(voucher.nonce))
        {
            return Err(Error::VoucherRevoked);
        }
        if next_id >= max {
            return Err(Error::MaxSupplyReached);
        }
        let digest = Self::_voucher_digest(env, voucher);
        Self::verify_signature_or_panic(env, pubkey, &digest, signature);
        Ok(())
    }

    /// Execute payment split for one voucher's price.
    fn pay(
        env: &Env,
        buyer: &Address,
        creator: &Address,
        currency: &Address,
        price: i128,
        fee_bps: u32,
        fee_receiver: &Address,
    ) {
        if price <= 0 {
            return;
        }
        if fee_bps > 0 {
            let fee_amount = (price * fee_bps as i128) / 10_000;
            let creator_amount = price - fee_amount;
            if fee_amount > 0 {
                TokenClient::new(env, currency).transfer(buyer, fee_receiver, &fee_amount);
            }
            if creator_amount > 0 {
                TokenClient::new(env, currency).transfer(buyer, creator, &creator_amount);
            }
        } else {
            TokenClient::new(env, currency).transfer(buyer, creator, &price);
        }
    }

    /// Mint a single token after all checks have passed.
    /// Updates Owner, TokenUri, UsedVoucher (keyed by nonce), BalanceOf, TotalSupply, NextTokenId.
    fn mint_token(env: &Env, buyer: &Address, token_id: u64, nonce: u64, uri: &String, next_id: u64) {
        env.storage()
            .persistent()
            .set(&DataKey::Owner(token_id), buyer);
        env.storage()
            .persistent()
            .set(&DataKey::TokenUri(token_id), uri);
        // Replay protection: mark the voucher nonce as consumed (not token_id).
        env.storage()
            .persistent()
            .set(&DataKey::UsedVoucher(nonce), &true);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Owner(token_id), TTL_THRESHOLD, TTL_BUMP);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::TokenUri(token_id), TTL_THRESHOLD, TTL_BUMP);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::UsedVoucher(nonce), TTL_THRESHOLD, TTL_BUMP);

        let bal: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::BalanceOf(buyer.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::BalanceOf(buyer.clone()), &(bal + 1));
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::BalanceOf(buyer.clone()), TTL_THRESHOLD, TTL_BUMP);

        let supply: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply + 1));

        if token_id >= next_id {
            env.storage()
                .instance()
                .set(&DataKey::NextTokenId, &(token_id + 1));
        }
    }
}

#[contractimpl]
impl LazyMint721 {
    // ── Initializer ───────────────────────────────────────────────────────

    /// Issue #38: accepts `platform_fee_receiver` and `platform_fee_bps` so
    /// the launchpad can configure per-collection fee splits at deployment time.
    /// Issue #273: accepts `network_passphrase` for cross-network domain separation.
    pub fn initialize(
        env: Env,
        creator: Address,
        creator_pubkey: BytesN<32>,
        name: String,
        symbol: String,
        max_supply: u64,
        royalty_bps: u32,
        royalty_receiver: Address,
        platform_fee_receiver: Address,
        platform_fee_bps: u32,
        network_passphrase: String,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Initialized) {
            return Err(Error::AlreadyInitialized);
        }
        // Issue #476: apply shared metadata validation rules before writing state.
        metadata::validate_name(&name, Error::EmptyName, Error::NameTooLong)?;
        metadata::validate_symbol(&symbol, Error::EmptySymbol, Error::SymbolTooLong)?;
        metadata::validate_max_supply(max_supply, Error::InvalidMaxSupply)?;
        metadata::validate_royalty_bps(royalty_bps, Error::InvalidBps)?;
        env.storage().instance().set(&DataKey::Initialized, &true);
        env.storage().instance().set(&DataKey::Creator, &creator);
        // OriginalCreator is set once at initialization and never overwritten
        // by succession — it preserves historical attribution (#484).
        env.storage().instance().set(&DataKey::OriginalCreator, &creator);
        env.storage().instance().set(&DataKey::CurrentWasmHash, &BytesN::from_array(&env, &[0u8; 32]));
        env.storage()
            .instance()
            .set(&DataKey::CreatorPubkey, &creator_pubkey);
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
        env.storage()
            .instance()
            .set(&DataKey::PlatformFeeReceiver, &platform_fee_receiver);
        env.storage()
            .instance()
            .set(&DataKey::PlatformFeeBps, &platform_fee_bps);
        // Store the network passphrase for cross-network domain separation (#273).
        env.storage()
            .instance()
            .set(&DataKey::NetworkPassphrase, &network_passphrase);
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_BUMP);
        Ok(())
    }

    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
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

    // ── Lazy Mint (single) ────────────────────────────────────────────────

    /// Buyer submits a signed voucher to mint their NFT.
    /// During the allowlist phase a valid Merkle proof for `buyer` is required.
    pub fn redeem(
        env: Env,
        buyer: Address,
        voucher: MintVoucher,
        signature: BytesN<64>,
        merkle_proof: Vec<BytesN<32>>,
    ) -> Result<u64, Error> {
        Self::extend_instance_ttl(&env);
        buyer.require_auth();

        // 0. Allowlist phase check
        Self::check_allowlist(&env, &buyer, &merkle_proof)?;

        // 1–5. Validate
        let pubkey: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::CreatorPubkey)
            .ok_or(Error::NotInitialized)?;
        let next_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);
        let max: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MaxSupply)
            .unwrap_or(u64::MAX);
        Self::check_voucher(&env, &voucher, &signature, &pubkey, max, next_id)?;

        // 6. Payment
        let creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        let fee_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PlatformFeeBps)
            .unwrap_or(0);
        let fee_receiver: Address = env
            .storage()
            .instance()
            .get(&DataKey::PlatformFeeReceiver)
            .unwrap_or(creator.clone());
        Self::pay(
            &env,
            &buyer,
            &creator,
            &voucher.currency,
            voucher.price,
            fee_bps,
            &fee_receiver,
        );

        // 7. Mint
        let token_id = voucher.token_id;
        Self::mint_token(&env, &buyer, token_id, voucher.nonce, &voucher.uri, next_id);

        // Emit detailed redemption event for indexer auditability (#273).
        env.events().publish(
            (symbol_short!("redeemed"), creator, buyer.clone()),
            (token_id, voucher.nonce, 1u128),
        );
        Ok(token_id)
    }

    // ── Lazy Mint (batch) ─────────────────────────────────────────────────

    /// Atomically redeem multiple vouchers.  All-or-nothing: if any voucher
    /// fails validation the entire batch reverts.
    ///
    /// Each item carries its own `merkle_proof` so mixed allowlist / open-entry
    /// batches are possible after `set_public_phase()`.
    ///
    /// Payments are aggregated per currency: for each unique currency the total
    /// fee portion and creator portion are summed and transferred in two calls
    /// (fee receiver then creator).  This minimises the number of token
    /// transfers for homogeneous batches.
    pub fn redeem_batch(
        env: Env,
        buyer: Address,
        items: Vec<BatchVoucherItem>,
    ) -> Result<Vec<u64>, Error> {
        Self::extend_instance_ttl(&env);
        buyer.require_auth();

        if items.len() == 0 {
            return Err(Error::EmptyBatch);
        }
        if items.len() > MAX_BATCH_SIZE {
            return Err(Error::BatchTooLarge);
        }

        let pubkey: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::CreatorPubkey)
            .ok_or(Error::NotInitialized)?;
        let max: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MaxSupply)
            .unwrap_or(u64::MAX);
        let next_id_start: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextTokenId)
            .unwrap_or(0);
        let fee_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::PlatformFeeBps)
            .unwrap_or(0);
        let creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        let fee_receiver: Address = env
            .storage()
            .instance()
            .get(&DataKey::PlatformFeeReceiver)
            .unwrap_or(creator.clone());

        // Phase 1: validate every item (all-or-nothing — no state changes yet).
        // We track supply headroom manually since NextTokenId is not yet updated.
        //
        // Duplicate-nonce hardening (#274): UsedVoucher(token_id) is only set
        // during Phase 4 minting, so two items sharing the same voucher
        // token_id would both pass validation here and get double-minted —
        // inflating balance/total_supply from a single voucher. Reject any
        // in-batch duplicate before any state mutation.
        let mut seen_ids: Vec<u64> = Vec::new(&env);
        let mut supply_used: u64 = 0u64;
        for item in items.iter() {
            let tid = item.voucher.token_id;
            for i in 0..seen_ids.len() {
                if seen_ids.get(i).unwrap() == tid {
                    return Err(Error::DuplicateVoucherInBatch);
                }
            }
            seen_ids.push_back(tid);

            Self::check_allowlist(&env, &buyer, &item.merkle_proof)?;
            let effective_next = next_id_start.saturating_add(supply_used);
            Self::check_voucher(
                &env,
                &item.voucher,
                &item.signature,
                &pubkey,
                max,
                effective_next,
            )?;
            supply_used = supply_used.saturating_add(1);
        }

        // Phase 2: aggregate payments per currency.
        // Map<currency_address_string, (fee_total, creator_total)>
        // We use a Vec of pairs because Map requires ScVal keys and Address
        // implements IntoVal — but to keep things simple we iterate twice.
        // For each currency accumulate: fee_amount and creator_amount.
        let mut fee_totals: Map<Address, i128> = Map::new(&env);
        let mut creator_totals: Map<Address, i128> = Map::new(&env);
        for item in items.iter() {
            let price = item.voucher.price;
            if price <= 0 {
                continue;
            }
            let cur = item.voucher.currency.clone();
            let fee_amount = if fee_bps > 0 {
                (price * fee_bps as i128) / 10_000
            } else {
                0i128
            };
            let creator_amount = price - fee_amount;

            let prev_fee: i128 = fee_totals.get(cur.clone()).unwrap_or(0);
            fee_totals.set(cur.clone(), prev_fee + fee_amount);

            let prev_creator: i128 = creator_totals.get(cur.clone()).unwrap_or(0);
            creator_totals.set(cur.clone(), prev_creator + creator_amount);
        }

        // Phase 3: transfer payments (aggregated).
        for (cur, fee_total) in fee_totals.iter() {
            if fee_total > 0 {
                TokenClient::new(&env, &cur).transfer(&buyer, &fee_receiver, &fee_total);
            }
        }
        for (cur, creator_total) in creator_totals.iter() {
            if creator_total > 0 {
                TokenClient::new(&env, &cur).transfer(&buyer, &creator, &creator_total);
            }
        }

        // Phase 4: mint all tokens.
        let mut minted_ids: Vec<u64> = Vec::new(&env);
        let mut next_id = next_id_start;
        for item in items.iter() {
            let token_id = item.voucher.token_id;
            Self::mint_token(&env, &buyer, token_id, item.voucher.nonce, &item.voucher.uri, next_id);
            if token_id >= next_id {
                next_id = token_id + 1;
            }
            // Emit detailed redemption event for each item (#273).
            env.events().publish(
                (symbol_short!("redeemed"), creator.clone(), buyer.clone()),
                (token_id, item.voucher.nonce, 1u128),
            );
            minted_ids.push_back(token_id);
        }

        Ok(minted_ids)
    }

    // ── Voucher Revocation ────────────────────────────────────────────────

    /// Revoke a single voucher by its nonce (token_id).  Creator-only.
    /// Returns `VoucherAlreadyRedeemed` if the nonce was already redeemed.
    pub fn revoke_voucher(env: Env, nonce: u64) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let creator = Self::only_creator(&env)?;

        if env
            .storage()
            .persistent()
            .has(&DataKey::UsedVoucher(nonce))
        {
            return Err(Error::VoucherAlreadyRedeemed);
        }
        env.storage()
            .persistent()
            .set(&DataKey::RevokedVoucher(nonce), &true);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::RevokedVoucher(nonce), TTL_THRESHOLD, TTL_BUMP);
        env.events()
            .publish((symbol_short!("revoke"), creator), nonce);
        Ok(())
    }

    /// Batch-revoke a list of voucher nonces.  Creator-only.  All-or-nothing:
    /// if any nonce is already redeemed the call reverts and nothing is revoked.
    pub fn revoke_vouchers(env: Env, nonces: Vec<u64>) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        let creator = Self::only_creator(&env)?;

        // Validate all first (all-or-nothing)
        for nonce in nonces.iter() {
            if env
                .storage()
                .persistent()
                .has(&DataKey::UsedVoucher(nonce))
            {
                return Err(Error::VoucherAlreadyRedeemed);
            }
        }
        for nonce in nonces.iter() {
            env.storage()
                .persistent()
                .set(&DataKey::RevokedVoucher(nonce), &true);
            env.storage().persistent().extend_ttl(
                &DataKey::RevokedVoucher(nonce),
                TTL_THRESHOLD,
                TTL_BUMP,
            );
            env.events()
                .publish((symbol_short!("revoke"), creator.clone()), nonce);
        }
        Ok(())
    }

    /// Return `true` if the voucher nonce has been explicitly revoked by the creator.
    pub fn is_voucher_revoked(env: Env, nonce: u64) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::RevokedVoucher(nonce))
    }

    /// Returns the composite status of a voucher nonce (#480):
    ///   "Revoked"  — creator has explicitly revoked this nonce
    ///   "Redeemed" — nonce has been consumed by a successful redeem call
    ///   "Issued"   — nonce is still valid (not revoked, not redeemed)
    ///
    /// Revocation takes priority in the display string so a race where the
    /// contract sets both flags is surfaced as "Revoked" (the creator-visible
    /// terminal state), consistent with the on-chain check order in check_voucher.
    pub fn voucher_status(env: Env, nonce: u64) -> String {
        if env.storage().persistent().has(&DataKey::RevokedVoucher(nonce)) {
            String::from_str(&env, "Revoked")
        } else if env.storage().persistent().has(&DataKey::UsedVoucher(nonce)) {
            String::from_str(&env, "Redeemed")
        } else {
            String::from_str(&env, "Issued")
        }
    }

    // ── Transfers ─────────────────────────────────────────────────────────

    pub fn transfer(env: Env, from: Address, to: Address, token_id: u64) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        from.require_auth();
        Self::_transfer(&env, &from, &to, token_id)
    }

    pub fn transfer_from(
        env: Env,
        spender: Address,
        from: Address,
        to: Address,
        token_id: u64,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        spender.require_auth();
        Self::_check_approved(&env, &spender, &from, token_id)?;
        env.storage()
            .persistent()
            .remove(&DataKey::Approved(token_id));
        Self::_transfer(&env, &from, &to, token_id)
    }

    // ── Approvals ─────────────────────────────────────────────────────────

    pub fn approve(
        env: Env,
        spender: Address,
        approved: Address,
        token_id: u64,
    ) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        spender.require_auth();
        let owner: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Owner(token_id))
            .ok_or(Error::TokenNotFound)?;
        if spender != owner
            && !Self::is_approved_for_all(env.clone(), owner.clone(), spender.clone())
        {
            return Err(Error::NotApproved);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Approved(token_id), &approved);
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::Approved(token_id), TTL_THRESHOLD, TTL_BUMP);
        Ok(())
    }

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
        env.storage()
            .persistent()
            .get(&DataKey::TokenUri(token_id))
            .ok_or(Error::TokenNotFound)
    }

    /// Always `true` — a lazy-minted token's URI comes from its signed
    /// voucher and is set once at redemption; there is no setter to change
    /// it afterwards (#276). Exposed as a method (rather than left implicit)
    /// so every collection type — normal and lazy — exposes the same
    /// `is_metadata_frozen()` query for frontend/indexer consumers.
    pub fn is_metadata_frozen(_env: Env) -> bool {
        true
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

    /// Returns true if the voucher nonce has already been redeemed.
    /// Uses the voucher's `nonce` field (not token_id) for lookup (#273).
    pub fn is_voucher_redeemed(env: Env, nonce: u64) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::UsedVoucher(nonce))
    }

    /// Returns the current ledger sequence at query time, which callers can
    /// compare against `MintVoucher.valid_until` to determine how many ledgers
    /// remain before a voucher expires.  A voucher with `valid_until == 0` never
    /// expires.  If `current_ledger > valid_until` the voucher is already expired
    /// and `redeem` will return `VoucherExpired` (#481).
    ///
    /// This is intentionally a lightweight view that does NOT check revocation
    /// or redemption state — callers should combine it with `is_voucher_redeemed`
    /// and `is_voucher_revoked` to get the full voucher status picture.
    pub fn voucher_expiry_info(env: Env, valid_until: u64) -> (u64, bool) {
        let current = env.ledger().sequence() as u64;
        let expired = valid_until != 0 && current > valid_until;
        (current, expired)
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

    /// Marketplace compatibility tag (Issue #841). Must match Symbol `"LazyMint721"`.
    pub fn contract_type(env: Env) -> Symbol {
        Symbol::new(&env, "LazyMint721")
    }

    pub fn platform_fee_info(env: Env) -> (Address, u32) {
        (
            env.storage()
                .instance()
                .get(&DataKey::PlatformFeeReceiver)
                .unwrap(),
            env.storage()
                .instance()
                .get(&DataKey::PlatformFeeBps)
                .unwrap_or(0),
        )
    }

    pub fn get_approved(env: Env, token_id: u64) -> Option<Address> {
        env.storage().persistent().get(&DataKey::Approved(token_id))
    }

    pub fn is_approved_for_all(env: Env, owner: Address, operator: Address) -> bool {
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
    /// Proposes `new_creator` as the next collection administrator.
    /// The proposal expires at `expires_at` (ledger sequence).
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
    /// The proposed successor accepts the role.  `OriginalCreator` is unchanged.
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

    pub fn update_creator_pubkey(env: Env, new_pubkey: BytesN<32>) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::CreatorPubkey, &new_pubkey);
        Ok(())
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

    /// Set the Merkle root for the allowlist.  Creator-only.
    /// Automatically resets to allowlist phase (clears public phase flag).
    pub fn set_merkle_root(env: Env, root: BytesN<32>) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage().instance().set(&DataKey::MerkleRoot, &root);
        env.storage()
            .instance()
            .set(&DataKey::IsPublicPhase, &false);
        Ok(())
    }

    /// Switch the sale to public phase — removes the allowlist restriction.
    /// Creator-only.  Reversible by calling `set_merkle_root` again.
    pub fn set_public_phase(env: Env) -> Result<(), Error> {
        Self::extend_instance_ttl(&env);
        Self::only_creator(&env)?;
        env.storage().instance().set(&DataKey::IsPublicPhase, &true);
        Ok(())
    }

    /// Return whether the sale is currently in public phase.
    pub fn is_public_phase(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::IsPublicPhase)
            .unwrap_or(false)
    }

    /// Return the current Merkle root (None if unset).
    pub fn merkle_root(env: Env) -> Option<BytesN<32>> {
        env.storage().instance().get(&DataKey::MerkleRoot)
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
        // UsedVoucher entries are already in persistent storage and remain
        // readable as-is.  RevokedVoucher entries are likewise unaffected.

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

    /// Build the 32-byte digest that the creator must sign off-chain.
    ///
    /// Layout (all big-endian / XDR where noted):
    ///   N   bytes  network_passphrase bytes  (binds to this network)
    ///   N   bytes  contract_address XDR      (binds to this deployment)
    ///   8   bytes  nonce             (u64 BE) — unique per voucher (#273)
    ///   8   bytes  token_id          (u64 BE)
    ///  16   bytes  price             (i128 BE)
    ///   8   bytes  valid_until       (u64 BE)
    ///  32   bytes  uri_hash
    ///   N   bytes  currency address XDR
    ///
    /// The network passphrase is stored at initialization and bound here so a
    /// voucher signed on testnet cannot be replayed on mainnet even if the
    /// contract address happens to be the same.
    ///
    /// ⚠ Byte layout is STABLE — do not reorder fields.
    #[allow(non_snake_case)]
    pub fn _voucher_digest(env: &Env, v: &MintVoucher) -> Bytes {
        let mut raw = Bytes::new(env);
        // Network passphrase — domain separator for cross-network protection.
        let passphrase: String = env
            .storage()
            .instance()
            .get(&DataKey::NetworkPassphrase)
            .unwrap_or_else(|| String::from_str(env, ""));
        raw.append(&passphrase.to_xdr(env));
        // Contract address — binds signature to this specific deployment.
        raw.append(&env.current_contract_address().to_xdr(env));
        // Unique per-voucher nonce.
        raw.extend_from_array(&v.nonce.to_be_bytes());
        raw.extend_from_array(&v.token_id.to_be_bytes());
        raw.extend_from_array(&v.price.to_be_bytes());
        raw.extend_from_array(&v.valid_until.to_be_bytes());
        raw.append(&v.uri_hash.clone().into());
        raw.append(&v.currency.clone().to_xdr(env));
        env.crypto().sha256(&raw).into()
    }

    fn _transfer(env: &Env, from: &Address, to: &Address, token_id: u64) -> Result<(), Error> {
        env.storage()
            .persistent()
            .remove(&DataKey::Approved(token_id));
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
        if from_bal == 0 {
            return Err(Error::NotOwner);
        }
        env.storage().persistent().set(
            &DataKey::BalanceOf(from.clone()),
            &(from_bal.saturating_sub(1)),
        );
        env.storage()
            .persistent()
            .extend_ttl(&DataKey::BalanceOf(from.clone()), TTL_THRESHOLD, TTL_BUMP);
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
            .extend_ttl(&DataKey::BalanceOf(to.clone()), TTL_THRESHOLD, TTL_BUMP);
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
        if let Some(approved) = env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::Approved(token_id))
        {
            if approved == *spender {
                return Ok(());
            }
        }
        let all_approved: bool = env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::ApprovedForAll(from.clone(), spender.clone()))
            .unwrap_or(false);
        if all_approved {
            let expiry: Option<u32> = env
                .storage()
                .persistent()
                .get::<DataKey, u32>(&DataKey::ApprovedForAllExpiry(
                    from.clone(),
                    spender.clone(),
                ));
            let expired = expiry.map_or(false, |exp| env.ledger().sequence() >= exp);
            if !expired {
                return Ok(());
            }
        }
        Err(Error::NotApproved)
    }
}

#[cfg(test)]
mod test;

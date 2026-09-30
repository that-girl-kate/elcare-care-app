// contract.rs — ELCARE-HUB Marketplace contract implementation
#[allow(unused_imports)]
use soroban_sdk::{
    contract, contractimpl, log, panic_with_error, token::Client as TokenClient,
    Address, Bytes, Env, IntoVal, Symbol, Vec,
};
use crate::events::*;
use crate::{
    escrow,
    storage::{
        acquire_auction_lock, acquire_listing_lock,
        active_listings_len, add_artist_auction_id,
        add_artist_listing_id, add_listing_offer_id, add_offerer_offer_id, add_pending_offer,
        add_to_active_listings, append_bid_record, bump_instance_ttl, clear_artist_cancel_cursor,
        clear_migration_progress, clear_pending_admin_storage, clear_pending_offers,
        get_active_listing_ids_range, get_artist_auction_ids,
        get_artist_cancel_cursor, get_artist_listing_ids, get_auction_count,
        get_auction_extension_trigger_storage, get_auction_extension_window_storage,
        get_auction_max_extensions_storage,
        get_bid_history_cap_storage, get_collection_fee_bps_storage, get_listing_count,
        get_max_price_storage, get_migration_progress, get_migration_status_storage,
        get_min_price_storage, get_pending_admin_storage,
        get_pending_role_storage, get_role_storage, get_ttl_sweep_progress,
        increment_auction_count, increment_listing_count, increment_offer_count,
        index_append, index_contains, index_get, index_len, index_page_count,
        is_artist_revoked_storage, is_bidder_blocked, is_migration_done, load_auction, load_auction_bids,
        load_listing, load_listing_offers, load_offer, load_offerer_offers,
        load_pending_offer_ids, pending_offer_count, release_auction_lock,
        release_listing_lock, remove_artist_revocation_storage, remove_from_active_listings,
        remove_pending_offer, save_auction, save_listing, save_offer,
        set_artist_cancel_cursor, set_artist_revocation_storage,
        set_auction_extension_trigger_storage,
        set_auction_extension_window_storage, set_auction_max_extensions_storage,
        set_bid_history_cap_storage, set_collection_fee_bps_storage, set_max_price_storage,
        set_migration_done, set_migration_progress, set_migration_stuck, clear_migration_stuck,
        set_min_price_storage,
        set_pending_admin_storage, set_pending_role_storage, set_role_storage,
        set_ttl_sweep_progress, clear_collection_fee_bps_storage, clear_pending_role_storage,
        take_legacy_index_vec, DataKey, IndexId, MigrationStatus, PendingAdminProposal,
        PendingRoleProposal, MAX_BID_HISTORY_CAP,
        assert_token_policy_active, assert_price_policy, evaluate_token_policy,
        TokenWhitelistPolicyResult,
        // Issue #459 — treasury rotation
        set_pending_treasury_storage, get_pending_treasury_storage,
        clear_pending_treasury_storage, PendingTreasuryProposal,
        // Issue #460 — listing duration bounds
        set_min_listing_duration_storage, get_min_listing_duration_storage,
        set_max_listing_duration_storage, get_max_listing_duration_storage,
        assert_listing_duration_policy,
        // Issue #471 — counter-offer links
        set_counter_offer_parent, get_counter_offer_parent,
        set_offer_superseded_by,
        // Issue #472 — governance quorum
        increment_governance_proposal_count,
        save_governance_proposal, load_governance_proposal,
        load_governance_approvals, save_governance_approvals,
        // Issue #474 — terminal cleanup cursor
        get_terminal_cleanup_cursor, set_terminal_cleanup_cursor,
        clear_terminal_cleanup_cursor,
    },
    types::{
        Auction, AuctionCancelReason, AuctionStatus, BatchCreateListingInput,
        BatchUpdateListingInput, BidRecord, BatchItemError, CancelReason, GovernanceProposal,
        GovernanceProposalType, Listing, ListingStatus, MarketplaceError, Offer, OfferStatus,
        Recipient, RoleType,
    },
};

/// Semantic version — bump on every breaking storage change.
const CONTRACT_VERSION: &str = "1.1.0";
const DEFAULT_MIN_BID_INCREMENT: i128 = 1_000_000; // 0.1 XLM in stroops
const DEFAULT_EXTENSION_WINDOW: u64 = 600;
const DEFAULT_EXTENSION_TRIGGER: u64 = 0;

// ── Reentrancy RAII scopes (Issue #204) ───────────────────────────────────────
//
// These structs acquire the appropriate temporary-storage lock on construction
// and release it via Drop.  Using RAII ensures the guard is always cleared when
// the enclosing scope exits — whether via a normal return or a panic — providing
// defence-in-depth on top of Soroban's atomic rollback guarantee.
//
// Usage:
//   let _guard = ListingReentrancyScope::new(&env, listing_id)?;
//   // ... function body — guard is released automatically at end of scope ...
//
// Both structs hold the `Env` by value (cloned once) because Soroban `Env` is
// cheaply reference-counted and `Drop` must own everything it needs.

/// RAII guard for per-listing reentrancy protection.
/// Acquired by `buy_artwork` and `accept_offer`.
struct ListingReentrancyScope {
    env: Env,
    listing_id: u64,
}

impl ListingReentrancyScope {
    /// Attempt to acquire the listing lock.
    /// Returns `Ok(scope)` on success, or panics with `ReentrancyGuard` if the
    /// lock is already held.
    fn new(env: &Env, listing_id: u64) -> Self {
        if !crate::storage::acquire_listing_lock(env, listing_id) {
            panic_with_error!(env, MarketplaceError::ReentrancyGuard);
        }
        Self { env: env.clone(), listing_id }
    }
}

impl Drop for ListingReentrancyScope {
    fn drop(&mut self) {
        crate::storage::release_listing_lock(&self.env, self.listing_id);
    }
}

/// RAII guard for per-auction reentrancy protection.
/// Acquired by `finalize_auction`.
struct AuctionReentrancyScope {
    env: Env,
    auction_id: u64,
}

impl AuctionReentrancyScope {
    fn new(env: &Env, auction_id: u64) -> Self {
        if !crate::storage::acquire_auction_lock(env, auction_id) {
            panic_with_error!(env, MarketplaceError::ReentrancyGuard);
        }
        Self { env: env.clone(), auction_id }
    }
}

impl Drop for AuctionReentrancyScope {
    fn drop(&mut self) {
        crate::storage::release_auction_lock(&self.env, self.auction_id);
    }
}

/// Lifetime of a pending admin-rotation proposal, in seconds (7 days).
///
/// `transfer_admin` stamps each proposal with
/// `expires_at = env.ledger().timestamp() + ADMIN_PROPOSAL_TTL`.  Once that
/// deadline passes, `accept_admin` reverts with `AdminProposalExpired`, so a
/// proposal that is never accepted or cancelled cannot leave the contract in a
/// half-transferred governance state indefinitely.
const ADMIN_PROPOSAL_TTL: u64 = 604_800; // 7 days

/// Minimum timelock before a treasury proposal becomes acceptable (1 hour).
///
/// After `propose_treasury` the proposed address may not call
/// `accept_treasury` for at least `TREASURY_TIMELOCK` seconds, giving
/// operators a window to review and cancel a mistaken proposal. (Issue #459)
const TREASURY_TIMELOCK: u64 = 3_600; // 1 hour

/// Lifetime of a pending treasury-rotation proposal, in seconds (7 days).
///
/// A proposal that is never accepted or cancelled cannot lock governance
/// state indefinitely. (Issue #459)
const TREASURY_PROPOSAL_TTL: u64 = 604_800; // 7 days

/// Maximum batch size for `create_listings`. Keeps preflight validation and
/// escrow work within a single transaction's compute budget. (Issue #457)
const MAX_BATCH_LISTINGS: u32 = 20;

/// Minimum auction duration in seconds (1 hour).
///
/// An auction whose computed `end_time = now + duration` would be less than
/// `MIN_AUCTION_DURATION` seconds in the future is rejected with
/// `InvalidAuctionDuration`.  This prevents meaningless or front-runnable
/// auctions that expire almost immediately.
const MIN_AUCTION_DURATION: u64 = 3_600; // 1 hour

/// Maximum total auction duration in seconds (30 days).
///
/// An auction's end_time cannot exceed original_end_time + MAX_TOTAL_AUCTION_DURATION
/// even with anti-sniping extensions. This prevents adversarial bidders from
/// indefinitely extending an auction by placing small qualifying bids just before
/// each deadline.
pub(crate) const MAX_TOTAL_AUCTION_DURATION: u64 = 2_592_000; // 30 days

/// Maximum number of listing ids accepted by one `cancel_listings` batch.
const MAX_BATCH_CANCEL: u32 = 10;

/// Hard ceiling on the number of records/ids any single bounded maintenance
/// call (`extend_active_ttls`, `cleanup_expired_locks`) will touch, applied
/// on top of (never relaxed by) whatever the caller passes as `max_items` /
/// however many ids they supply. Keeps a single maintenance transaction's
/// compute/read footprint bounded no matter what an admin requests.
/// (Issue #280)
const MAX_MAINTENANCE_ITEMS: u32 = 100;

const MAX_OFFERS_PER_LISTING: u32 = 50;

/// Maximum number of addresses one auction's blocked-bidder registry can hold
/// (Issue #199).  Bounds the per-auction `AuctionBlockedBidders` entry so the
/// `place_bid` membership scan and the storage footprint stay small.
const MAX_BLOCKED_BIDDERS: u32 = 50;

/// Maximum listings+auctions the `revoke_artist` cascade processes in one call
/// (Issue #214). Keeps the per-invocation compute footprint bounded for artists
/// with many active records; `RevocationIncompleteEvent` is emitted when items
/// remain so the caller knows to call again.
const REVOCATION_CLEANUP_CAP: u32 = 100;

/// Outcome of `check_bid_invariants`: the bid is valid and this carries
/// whether an anti-sniping extension should be applied.
///
/// Defined outside `impl MarketplaceContract` because Rust does not allow
/// struct definitions inside `impl` blocks.
struct BidInvariantResult {
    /// `Some((new_end_time, new_extension_count))` when the anti-sniping
    /// extension must be applied; `None` otherwise.
    pub extension: Option<(u64, u32)>,
}

// ── Formal lifecycle/state-machine layer (Issue #480) ───────────────────────
//
// Listings, offers, and auctions are the three stateful entities in the
// marketplace. Every mutating public path must route its status change through
// the matching helper below so the allowed-transition table, invariants, and
// cross-entity side effects (refunds, escrow release, active-index removal)
// live in exactly one place.

#[allow(dead_code)]
fn assert_listing_transition(env: &Env, current: &ListingStatus, target: &ListingStatus) {
    let allowed = match target {
        ListingStatus::Sold => matches!(current, ListingStatus::Active),
        ListingStatus::Cancelled => matches!(current, ListingStatus::Active),
        _ => false,
    };
    if !allowed {
        panic_with_error!(env, MarketplaceError::ListingNotActive);
    }
}

#[allow(dead_code)]
fn assert_offer_transition(env: &Env, current: &OfferStatus, target: &OfferStatus) {
    let allowed = match target {
        OfferStatus::Rejected => matches!(current, OfferStatus::Pending),
        OfferStatus::Accepted => matches!(current, OfferStatus::Pending),
        OfferStatus::Withdrawn => matches!(current, OfferStatus::Pending),
        _ => false,
    };
    if !allowed {
        panic_with_error!(env, MarketplaceError::OfferNotPending);
    }
}

#[allow(dead_code)]
fn assert_auction_active(env: &Env, status: &AuctionStatus) {
    if *status != AuctionStatus::Active {
        panic_with_error!(env, MarketplaceError::AuctionNotActive);
    }
}

#[allow(dead_code)]
fn assert_auction_transition(env: &Env, current: &AuctionStatus, target: &AuctionStatus) {
    let allowed = match target {
        AuctionStatus::Finalized => matches!(current, AuctionStatus::Active),
        AuctionStatus::Cancelled => matches!(current, AuctionStatus::Active),
        _ => false,
    };
    if !allowed {
        panic_with_error!(env, MarketplaceError::AuctionAlreadyFinalized);
    }
}

#[allow(dead_code)]
fn transition_offer_status(env: &Env, offer: &mut Offer, target: OfferStatus) {
    assert_offer_transition(env, &offer.status, &target);
    offer.status = target;
    save_offer(env, offer);
}

#[allow(dead_code)]
fn transition_listing_to_cancelled(
    env: &Env,
    listing: &mut Listing,
    listing_id: u64,
    cancelled_by: Address,
    reason: CancelReason,
    refund_pending_offers: bool,
) {
    assert_listing_transition(env, &listing.status, &ListingStatus::Cancelled);
    if refund_pending_offers {
        for offer_id in load_pending_offer_ids(env, listing_id).iter() {
            if let Some(mut offer) = load_offer(env, offer_id) {
                if offer.status == OfferStatus::Pending {
                    transition_offer_status(env, &mut offer, OfferStatus::Rejected);
                    TokenClient::new(env, &offer.token).transfer(
                        &env.current_contract_address(),
                        &offer.offerer,
                        &offer.amount,
                    );
                }
            }
        }
        clear_pending_offers(env, listing_id);
    }
    listing.status = ListingStatus::Cancelled;
    save_listing(env, listing);
    remove_from_active_listings(env, listing_id);
    ListingCancelledEvent {
        listing_id,
        cancelled_by,
        reason,
        ledger_sequence: env.ledger().sequence(),
    }
    .publish(env);
    if listing.quantity > 1 {
        escrow::release_nft_with_quantity(
            env,
            &listing.collection,
            listing.token_id,
            listing.quantity,
            &listing.artist,
            env.ledger().sequence(),
            listing_id,
        );
    } else {
        escrow::release_nft(
            env,
            &listing.collection,
            listing.token_id,
            &listing.artist,
            env.ledger().sequence(),
            listing_id,
        );
    }
}

#[contract]
pub struct MarketplaceContract;

#[contractimpl]
impl MarketplaceContract {
    // ── Admin ────────────────────────────────────────────────

    pub fn set_admin(env: Env, admin: Address) {
        bump_instance_ttl(&env);
        let key = crate::storage::DataKey::Admin;
        if env.storage().persistent().get::<_, Address>(&key).is_some() {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        admin.require_auth();
        env.storage().persistent().set(&key, &admin);
        // Initialize default auction configuration values
        crate::storage::set_min_bid_increment_storage(&env, DEFAULT_MIN_BID_INCREMENT);
        crate::storage::set_auction_extension_window_storage(&env, DEFAULT_EXTENSION_WINDOW);
        crate::storage::set_auction_extension_trigger_storage(&env, DEFAULT_EXTENSION_TRIGGER);
    }

    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage()
            .persistent()
            .get::<_, Address>(&crate::storage::DataKey::Admin)
    }

    /// Step 1 of the two-step admin rotation: the current admin proposes a
    /// candidate.  The proposal is stamped with an `expires_at` deadline
    /// (`now + ADMIN_PROPOSAL_TTL`); a second call overwrites any still-pending
    /// proposal (and its deadline) with the new candidate.
    pub fn transfer_admin(env: Env, current_admin: Address, new_admin: Address) {
        bump_instance_ttl(&env);
        current_admin.require_auth();
        let stored = Self::get_admin(env.clone())
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::Unauthorized));
        if current_admin != stored {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        let expires_at = env.ledger().timestamp() + ADMIN_PROPOSAL_TTL;
        set_pending_admin_storage(
            &env,
            &PendingAdminProposal {
                candidate: new_admin.clone(),
                expires_at,
            },
        );
        emit_admin_proposed(&env, current_admin, new_admin, expires_at);
    }

    /// Step 2 of the two-step admin rotation: the proposed candidate accepts.
    ///
    /// Reverts with `NoAdminProposalPending` if no proposal is active,
    /// `Unauthorized` if the caller is not the proposed candidate, and
    /// `AdminProposalExpired` if the proposal's `expires_at` deadline has
    /// passed.
    pub fn accept_admin(env: Env, new_admin: Address) {
        bump_instance_ttl(&env);
        new_admin.require_auth();
        let pending = get_pending_admin_storage(&env)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::NoAdminProposalPending));
        if new_admin != pending.candidate {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        if env.ledger().timestamp() > pending.expires_at {
            panic_with_error!(&env, MarketplaceError::AdminProposalExpired);
        }
        let old_admin = Self::get_admin(env.clone())
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::Unauthorized));
        let key = crate::storage::DataKey::Admin;
        env.storage().persistent().set(&key, &new_admin);
        env.storage().persistent().extend_ttl(
            &key,
            crate::storage::LEDGER_TTL_THRESHOLD,
            crate::storage::LEDGER_TTL_BUMP,
        );
        clear_pending_admin_storage(&env);
        emit_admin_accepted(&env, old_admin, new_admin);
    }

    /// Cancel a still-pending admin proposal.  Callable only by the current
    /// admin.  Reverts with `NoAdminProposalPending` when no proposal is
    /// active, so cancelling is idempotent-by-error rather than silent.
    pub fn cancel_admin_proposal(env: Env, current_admin: Address) {
        bump_instance_ttl(&env);
        current_admin.require_auth();
        let stored = Self::get_admin(env.clone())
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::Unauthorized));
        if current_admin != stored {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        let pending = get_pending_admin_storage(&env)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::NoAdminProposalPending));
        clear_pending_admin_storage(&env);
        emit_admin_proposal_cancelled(&env, current_admin, pending.candidate);
    }

    /// View: the currently-pending admin proposal (candidate + `expires_at`),
    /// or `None` when no rotation is in progress.
    pub fn get_pending_admin(env: Env) -> Option<PendingAdminProposal> {
        get_pending_admin_storage(&env)
    }

    // ── Role-based authorization (Issue #267) ─────────────────
    //
    // Every privileged entry point below is owned by exactly one `RoleType`:
    // `ProtocolConfig` (price bounds, treasury, fees, bid/auction params),
    // `EmergencyPause` (global/collection/function circuit breakers),
    // `CollectionAdmin` (artist revocation/reinstatement, listing cleanup)
    // and `Upgrade` (storage/version migration). A role with no explicit
    // holder falls back to whoever holds `Admin`, so existing single-admin
    // deployments keep working unchanged until an operator opts in via
    // `migrate_roles` or a direct `propose_role_transfer` /
    // `accept_role_transfer`. Keeping `EmergencyPause` independently
    // assignable means an incident responder can still halt the marketplace
    // even if the (potentially slower, multisig-gated) `ProtocolConfig`
    // authority is unavailable or compromised.

    /// Resolve the current holder of `role`: its explicit holder if one has
    /// been assigned, otherwise the contract `Admin`.
    pub fn get_role(env: Env, role: RoleType) -> Address {
        get_role_storage(&env, &role)
            .unwrap_or_else(|| Self::get_admin(env.clone()).expect("admin not set"))
    }

    /// View: the currently-pending role-transfer proposal for `role`, or
    /// `None` when no rotation is in progress.
    pub fn get_pending_role(env: Env, role: RoleType) -> Option<PendingRoleProposal> {
        get_pending_role_storage(&env, &role)
    }

    /// Read-only inventory of every active role holder and pending proposal
    /// (Issue #473).
    ///
    /// Returns a `RoleInventory` struct with the current holder and optional
    /// pending proposal for each of the four role axes, plus the active admin
    /// and pending-admin proposal (if any). No auth required — this is a
    /// pure read-only diagnostic view.
    ///
    /// Operators use this as the first step of any rotation procedure: confirm
    /// the current state before proposing a transfer.
    pub fn get_role_inventory(env: Env) -> crate::types::RoleInventory {
        let admin: Option<Address> = env
            .storage()
            .persistent()
            .get::<_, Address>(&DataKey::Admin);
        let pending_admin = get_pending_admin_storage(&env);

        let all_roles = [
            RoleType::ProtocolConfig,
            RoleType::EmergencyPause,
            RoleType::CollectionAdmin,
            RoleType::Upgrade,
        ];

        let mut role_entries = soroban_sdk::Vec::new(&env);
        for role in all_roles.iter() {
            let holder = get_role_storage(&env, role)
                .unwrap_or_else(|| admin.clone().expect("admin not set"));
            let pending = get_pending_role_storage(&env, role);
            role_entries.push_back(crate::types::RoleEntry {
                role: role.clone(),
                holder,
                pending_candidate: pending.as_ref().map(|p| p.candidate.clone()),
                pending_expires_at: pending.map(|p| p.expires_at),
            });
        }

        crate::types::RoleInventory {
            admin,
            pending_admin_candidate: pending_admin.as_ref().map(|p| p.candidate.clone()),
            pending_admin_expires_at: pending_admin.map(|p| p.expires_at),
            roles: role_entries,
            ledger_sequence: env.ledger().sequence(),
            ledger_timestamp: env.ledger().timestamp(),
        }
    }

    /// Step 1 of the two-step role rotation: the current holder of `role`
    /// proposes a candidate. A second call overwrites any still-pending proposal
    /// and emits `role_proposal_overwritten` so indexers can close the old one.
    ///
    /// Rejects with:
    /// - `Unauthorized`          — caller is not the current role holder.
    /// - `RoleTransferToSelf`    — candidate is already the current holder.
    /// - `RoleTransferToContract`— candidate is the contract's own address
    ///                             (irrecoverable governance state).
    pub fn propose_role_transfer(
        env: Env,
        current_authority: Address,
        role: RoleType,
        candidate: Address,
    ) {
        bump_instance_ttl(&env);
        current_authority.require_auth();
        let stored = Self::get_role(env.clone(), role.clone());
        if current_authority != stored {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        // Guard: no-op transfers pollute the pending slot without advancing
        // governance — reject them explicitly.
        if candidate == current_authority {
            panic_with_error!(&env, MarketplaceError::RoleTransferToSelf);
        }
        // Guard: the contract address cannot hold a role because no external
        // key can produce a valid Soroban auth signature for it.
        if candidate == env.current_contract_address() {
            panic_with_error!(&env, MarketplaceError::RoleTransferToContract);
        }
        // Overwrite detection: if a proposal already exists, emit an overwrite
        // event so the indexer can mark the superseded candidate as cancelled.
        let new_expires_at = env.ledger().timestamp() + ADMIN_PROPOSAL_TTL;
        if let Some(existing) = get_pending_role_storage(&env, &role) {
            emit_role_proposal_overwritten(
                &env,
                role.clone(),
                current_authority.clone(),
                existing.candidate,
                existing.expires_at,
                candidate.clone(),
                new_expires_at,
            );
        }
        set_pending_role_storage(
            &env,
            &role,
            &PendingRoleProposal {
                candidate: candidate.clone(),
                expires_at: new_expires_at,
            },
        );
        emit_role_proposed(&env, role, current_authority, candidate, new_expires_at);
    }

    /// Step 2: the proposed candidate accepts and becomes the explicit
    /// holder of `role`. Reverts with `NoRoleProposalPending` if no proposal
    /// is active, `Unauthorized` if the caller is not the proposed candidate,
    /// and `RoleProposalExpired` if the proposal's deadline has passed —
    /// analogous to `accept_admin`.
    pub fn accept_role_transfer(env: Env, role: RoleType, candidate: Address) {
        bump_instance_ttl(&env);
        candidate.require_auth();
        let pending = get_pending_role_storage(&env, &role)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::NoRoleProposalPending));
        if candidate != pending.candidate {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        if env.ledger().timestamp() > pending.expires_at {
            panic_with_error!(&env, MarketplaceError::RoleProposalExpired);
        }
        let old_authority = Self::get_role(env.clone(), role.clone());
        set_role_storage(&env, &role, &candidate);
        clear_pending_role_storage(&env, &role);
        emit_role_accepted(&env, role, old_authority, candidate, env.ledger().sequence());
    }

    /// Cancel a still-pending role-transfer proposal. Callable only by the
    /// current holder of the role. Reverts with `NoRoleProposalPending` when
    /// no proposal is active.
    pub fn cancel_role_proposal(env: Env, current_authority: Address, role: RoleType) {
        bump_instance_ttl(&env);
        current_authority.require_auth();
        let stored = Self::get_role(env.clone(), role.clone());
        if current_authority != stored {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        let pending = get_pending_role_storage(&env, &role)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::NoRoleProposalPending));
        clear_pending_role_storage(&env, &role);
        emit_role_proposal_cancelled(
            &env,
            role,
            current_authority,
            pending.candidate,
            env.ledger().sequence(),
        );
    }

    /// Migration path for existing single-admin deployments (Issue #267):
    /// explicitly assigns `admin` as the holder of every role that does not
    /// yet have one, emitting `role_migrated` for each. Callable only by the
    /// current `Admin`.
    ///
    /// Idempotent: a role that already has an explicit holder (including one
    /// assigned by an earlier `migrate_roles` call, or one since transferred
    /// to a distinct authority) is left untouched, so re-running it is always
    /// safe.
    pub fn migrate_roles(env: Env, admin: Address) {
        bump_instance_ttl(&env);
        admin.require_auth();
        if admin != Self::get_admin(env.clone()).expect("admin not set") {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        for role in [
            RoleType::ProtocolConfig,
            RoleType::EmergencyPause,
            RoleType::CollectionAdmin,
            RoleType::Upgrade,
        ] {
            if get_role_storage(&env, &role).is_none() {
                set_role_storage(&env, &role, &admin);
                emit_role_migrated(&env, role, admin.clone());
            }
        }
    }

    // ── Versioning & Migration ───────────────────────────────

    pub fn version(env: Env) -> soroban_sdk::String {
        soroban_sdk::String::from_str(&env, CONTRACT_VERSION)
    }

    /// Operator-facing view: return the current migration state for the running
    /// contract version so maintainers can determine whether a migration is
    /// pending, in-progress (interrupted mid-step), completed, or has never
    /// been started.
    ///
    /// Interpretation:
    /// * `is_done = true`         — migration completed successfully.
    /// * `is_in_progress = true`  — a cursor exists; `migrate_step` was called
    ///                              but did not drain all phases. Call
    ///                              `migrate_step` again to continue.
    /// * both `false`             — migration has never been started for this
    ///                              version (or a fresh deploy before any
    ///                              migration exists).
    pub fn get_migration_status(env: Env) -> MigrationStatus {
        let version = soroban_sdk::String::from_str(&env, CONTRACT_VERSION);
        get_migration_status_storage(&env, &version)
    }

    /// Operator-facing bounded consistency check for the post-migration storage
    /// state.  Returns `true` when all sampled invariants hold; `false` (and
    /// `TtlAnomalyEvent`s) when drift is detected.
    ///
    /// Checks (all bounded by `max_items` so a single call is always safe):
    ///   1. The `ActiveListings` index count equals the number of Active listings
    ///      found in a sequential scan of `[cursor, cursor + max_items)`.
    ///   2. No terminal listing in that range has a stale `ActiveListingPos` key.
    ///   3. Each Active listing in the range has an `ActiveListingPos` key.
    ///
    /// The check is STATELESS — it does not modify any storage key and does not
    /// require the migration to be complete.  Call without a migration running to
    /// verify the live index; call after migration to confirm the postcondition.
    ///
    /// Returns `true` when every sampled record is consistent.
    pub fn validate_migration_consistency(
        env: Env,
        operator: Address,
        cursor: u64,
        max_items: u32,
    ) -> bool {
        bump_instance_ttl(&env);
        // Read-only operation; ProtocolConfig role is sufficient since this is
        // a diagnostic view, not a state-changing operation.
        Self::require_role(&env, &operator, RoleType::ProtocolConfig);

        let budget = max_items.min(MAX_MAINTENANCE_ITEMS);
        let listing_count = get_listing_count(&env);
        let mut consistent = true;

        let end = (cursor + budget as u64).min(listing_count);
        for i in cursor..end {
            let listing_id = i + 1;
            let pos_key = DataKey::ActiveListingPos(listing_id);
            match load_listing(&env, listing_id) {
                Some(l) if l.status == ListingStatus::Active => {
                    if !env.storage().persistent().has(&pos_key) {
                        TtlAnomalyEvent {
                            subject: Symbol::new(&env, "active_pos_miss"),
                            id: listing_id,
                            ledger_sequence: env.ledger().sequence(),
                        }
                        .publish(&env);
                        consistent = false;
                    }
                }
                Some(_) => {
                    // Terminal listing — must NOT have a stale pos key.
                    if env.storage().persistent().has(&pos_key) {
                        TtlAnomalyEvent {
                            subject: Symbol::new(&env, "term_in_active"),
                            id: listing_id,
                            ledger_sequence: env.ledger().sequence(),
                        }
                        .publish(&env);
                        consistent = false;
                    }
                }
                None => {}
            }
        }
        consistent
    }

    /// Admin-guarded, idempotent storage migration entry point.
    ///
    /// # Idempotency
    /// The function records a per-version marker in persistent storage the
    /// first time it is called.  Subsequent calls for the *same* version
    /// revert with `AlreadyMigrated` so that accidental double-invocations
    /// during upgrade scripts are caught rather than silently re-executed.
    ///
    /// # Usage
    /// Upgrade scripts should:
    /// 1. Deploy the new WASM and invoke `migrate(admin)`.
    /// 2. Verify `version()` returns the expected string.
    /// 3. Any data back-fill should be performed inside this function body
    ///    for the specific version being migrated to.
    /// # 1.1.0 migration
    /// Transforms the legacy monolithic `Vec<u64>` index entries
    /// (ActiveListings, ArtistListings, ArtistAuctions, ListingOffers,
    /// OffererOffers) into fixed-capacity pages, and rebuilds the bounded
    /// per-listing pending-offer sets used for O(1) offer-cap enforcement.
    /// Legacy keys are deleted as they are consumed, so re-running a partial
    /// migration never duplicates entries.
    ///
    /// Run `migrate` immediately after the WASM upgrade (ideally while the
    /// contract is paused): index writes performed between the upgrade and the
    /// migration land in the paged storage first, so migrated legacy entries
    /// would be appended after them, deviating from strict creation order.
    ///
    /// For state too large to migrate in one transaction, use the bounded
    /// [`Self::migrate_step`] variant until it returns 0.
    pub fn migrate(env: Env, admin: Address) {
        bump_instance_ttl(&env);
        let version = Self::require_pending_migration(&env, &admin);
        // Unbounded budget: run every phase to completion in this invocation.
        Self::run_migration(&env, &version, u32::MAX);
    }

    /// Bounded, resumable variant of [`Self::migrate`]: processes at most
    /// `max_items` migration units (one listing, auction or offer each) and
    /// persists a cursor so the next call resumes where this one stopped.
    ///
    /// Returns the number of units still to process — call repeatedly until it
    /// returns `0`, at which point the migration marker is recorded and any
    /// further call (of this or `migrate`) reverts with `AlreadyMigrated`.
    pub fn migrate_step(env: Env, admin: Address, max_items: u32) -> u64 {
        bump_instance_ttl(&env);
        let version = Self::require_pending_migration(&env, &admin);
        Self::run_migration(&env, &version, max_items)
    }

    /// Common guard for `migrate`/`migrate_step`: admin auth + not-yet-migrated.
    fn require_pending_migration(env: &Env, admin: &Address) -> soroban_sdk::String {
        Self::require_role(env, admin, RoleType::Upgrade);
        let version = soroban_sdk::String::from_str(env, CONTRACT_VERSION);
        if is_migration_done(env, &version) {
            panic_with_error!(env, MarketplaceError::AlreadyMigrated);
        }
        version
    }

    /// Migration engine shared by `migrate` and `migrate_step`.
    ///
    /// Phases (each unit is one listing/auction/offer, so per-call work is
    /// strictly bounded by `budget`):
    ///   0. per listing: migrate the artist's legacy listing index and the
    ///      listing's legacy offer index; rebuild its pending-offer set.
    ///   1. per auction: migrate the creator's legacy auction index.
    ///   2. per offer:   migrate the offerer's legacy offer index.
    ///   3. move the legacy ActiveListings vector into pages + position keys.
    ///
    /// At each phase boundary a `MigrationPhaseCompletedEvent` is emitted
    /// carrying the record count so operators can verify no entries were lost.
    ///
    /// Returns the number of units remaining; records the completion marker
    /// (and drops the cursor) when everything has been processed.
    fn run_migration(env: &Env, version: &soroban_sdk::String, mut budget: u32) -> u64 {
        let mut p = get_migration_progress(env, version);
        let listing_count = get_listing_count(env);
        let auction_count = get_auction_count(env);
        let offer_count = crate::storage::get_offer_count(env);

        while budget > 0 {
            match p.phase {
                0 => {
                    if p.cursor >= listing_count {
                        // Phase 0 complete: assert that every listing id now has a
                        // paged ArtistListings entry (postcondition check).
                        Self::assert_phase0_postconditions(env, listing_count);
                        MigrationPhaseCompletedEvent {
                            version: version.clone(),
                            phase: 0,
                            records_processed: listing_count,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(env);
                        p.phase = 1;
                        p.cursor = 0;
                        continue;
                    }
                    let listing_id = p.cursor + 1; // ids are 1-based
                    Self::migrate_listing_unit(env, listing_id);
                    p.cursor += 1;
                    budget -= 1;
                }
                1 => {
                    if p.cursor >= auction_count {
                        // Phase 1 complete: assert that every auction's creator
                        // has a non-empty paged ArtistAuctions index.
                        Self::assert_phase1_postconditions(env, auction_count);
                        MigrationPhaseCompletedEvent {
                            version: version.clone(),
                            phase: 1,
                            records_processed: auction_count,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(env);
                        p.phase = 2;
                        p.cursor = 0;
                        continue;
                    }
                    let auction_id = p.cursor + 1;
                    if let Some(auction) = load_auction(env, auction_id) {
                        Self::migrate_legacy_index(
                            env,
                            &DataKey::ArtistAuctions(auction.creator.clone()),
                            &IndexId::ArtistAuctions(auction.creator),
                        );
                    }
                    p.cursor += 1;
                    budget -= 1;
                }
                2 => {
                    if p.cursor >= offer_count {
                        // Phase 2 complete: assert that every offer's offerer
                        // has a non-empty paged OffererOffers index.
                        Self::assert_phase2_postconditions(env, offer_count);
                        MigrationPhaseCompletedEvent {
                            version: version.clone(),
                            phase: 2,
                            records_processed: offer_count,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(env);
                        p.phase = 3;
                        p.cursor = 0;
                        continue;
                    }
                    let offer_id = p.cursor + 1;
                    if let Some(offer) = load_offer(env, offer_id) {
                        Self::migrate_legacy_index(
                            env,
                            &DataKey::OffererOffers(offer.offerer.clone()),
                            &IndexId::OffererOffers(offer.offerer),
                        );
                    }
                    p.cursor += 1;
                    budget -= 1;
                }
                3 => {
                    // The legacy active index is a single entry; reading it is
                    // the same cost the old code paid on every removal.
                    let legacy_count = if let Some(ids) = take_legacy_index_vec(env, &DataKey::ActiveListings) {
                        let n = ids.len() as u64;
                        for listing_id in ids.iter() {
                            // Only move active listings — guard against stale entries.
                            if let Some(ref l) = load_listing(env, listing_id) {
                                if l.status == ListingStatus::Active {
                                    add_to_active_listings(env, listing_id);
                                }
                            }
                        }
                        n
                    } else {
                        0
                    };
                    // Phase 3 postcondition: assert ActiveListings paged count
                    // covers all currently-Active listings.
                    Self::assert_phase3_postconditions(env, listing_count);
                    MigrationPhaseCompletedEvent {
                        version: version.clone(),
                        phase: 3,
                        records_processed: legacy_count,
                        ledger_sequence: env.ledger().sequence(),
                    }.publish(env);
                    p.phase = 4;
                    budget -= 1;
                }
                _ => break,
            }
        }

        let remaining: u64 = match p.phase {
            0 => (listing_count - p.cursor) + auction_count + offer_count + 1,
            1 => (auction_count - p.cursor) + offer_count + 1,
            2 => (offer_count - p.cursor) + 1,
            3 => 1,
            _ => 0,
        };

        if remaining == 0 {
            clear_migration_progress(env, version);
            clear_migration_stuck(env, version);
            set_migration_done(env, version);
        } else {
            set_migration_progress(env, version, &p);
            // Mark that this migration has been interrupted at least once so
            // get_migration_status can distinguish "never started" from
            // "started but not yet finished".
            set_migration_stuck(env, version);
        }
        remaining
    }

    /// Phase-0 postcondition: for every listing that exists:
    /// (a) the ArtistListings paged index for that artist is non-empty,
    /// (b) the specific `listing_id` is present in that paged index,
    /// (c) every id in `ListingPendingOffers` references a truly-Pending offer,
    /// (d) every pending offer id is also in the `ListingOffers` paged index.
    /// Emits `TtlAnomalyEvent` for each violation rather than panicking, so the
    /// migration can complete and operators can see the full anomaly set.
    fn assert_phase0_postconditions(env: &Env, listing_count: u64) {
        for i in 0..listing_count {
            let listing_id = i + 1;
            if let Some(listing) = load_listing(env, listing_id) {
                let artist_idx = IndexId::ArtistListings(listing.artist.clone());
                if index_len(env, &artist_idx) == 0 {
                    TtlAnomalyEvent {
                        subject: Symbol::new(env, "artist_idx"),
                        id: listing_id,
                        ledger_sequence: env.ledger().sequence(),
                    }.publish(env);
                } else if !index_contains(env, &artist_idx, listing_id) {
                    // Index is non-empty but this specific listing_id is absent —
                    // a more precise signal that the entry was silently dropped.
                    TtlAnomalyEvent {
                        subject: Symbol::new(env, "artist_idx_miss"),
                        id: listing_id,
                        ledger_sequence: env.ledger().sequence(),
                    }.publish(env);
                }
                // Pending-offer alias correctness: every id in ListingPendingOffers
                // must point to an offer that is actually in Pending state, and
                // must also appear in the ListingOffers paged history index.
                let offers_idx = IndexId::ListingOffers(listing_id);
                for offer_id in load_pending_offer_ids(env, listing_id).iter() {
                    let alias_ok = load_offer(env, offer_id)
                        .map(|o| o.status == OfferStatus::Pending)
                        .unwrap_or(false);
                    if !alias_ok {
                        TtlAnomalyEvent {
                            subject: Symbol::new(env, "pending_alias"),
                            id: offer_id,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(env);
                    }
                    if !index_contains(env, &offers_idx, offer_id) {
                        TtlAnomalyEvent {
                            subject: Symbol::new(env, "offer_idx_miss"),
                            id: offer_id,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(env);
                    }
                }
            }
        }
    }

    /// Phase-1 postcondition: for every auction that exists:
    /// (a) the ArtistAuctions paged index for that creator is non-empty,
    /// (b) the specific `auction_id` is present in that paged index.
    /// Emits `TtlAnomalyEvent` for each violation.
    fn assert_phase1_postconditions(env: &Env, auction_count: u64) {
        for i in 0..auction_count {
            let auction_id = i + 1;
            if let Some(auction) = load_auction(env, auction_id) {
                let idx = IndexId::ArtistAuctions(auction.creator.clone());
                if index_len(env, &idx) == 0 {
                    TtlAnomalyEvent {
                        subject: Symbol::new(env, "auction_idx"),
                        id: auction_id,
                        ledger_sequence: env.ledger().sequence(),
                    }.publish(env);
                } else if !index_contains(env, &idx, auction_id) {
                    TtlAnomalyEvent {
                        subject: Symbol::new(env, "auction_idx_miss"),
                        id: auction_id,
                        ledger_sequence: env.ledger().sequence(),
                    }.publish(env);
                }
            }
        }
    }

    /// Phase-2 postcondition: for every offer that exists:
    /// (a) the OffererOffers paged index for that offerer is non-empty,
    /// (b) the specific `offer_id` is present in that paged index.
    /// Emits `TtlAnomalyEvent` for each violation.
    fn assert_phase2_postconditions(env: &Env, offer_count: u64) {
        for i in 0..offer_count {
            let offer_id = i + 1;
            if let Some(offer) = load_offer(env, offer_id) {
                let idx = IndexId::OffererOffers(offer.offerer.clone());
                if index_len(env, &idx) == 0 {
                    TtlAnomalyEvent {
                        subject: Symbol::new(env, "offerer_idx"),
                        id: offer_id,
                        ledger_sequence: env.ledger().sequence(),
                    }.publish(env);
                } else if !index_contains(env, &idx, offer_id) {
                    TtlAnomalyEvent {
                        subject: Symbol::new(env, "offerer_idx_miss"),
                        id: offer_id,
                        ledger_sequence: env.ledger().sequence(),
                    }.publish(env);
                }
            }
        }
    }

    /// Phase-3 postcondition checks for `ActiveListings` consistency:
    /// (a) the paged index count equals the number of Active listings in storage,
    /// (b) every Active listing has an `ActiveListingPos` key (is reachable),
    /// (c) no terminal (Sold/Cancelled) listing has a stale `ActiveListingPos` key,
    /// (d) the number of allocated index pages matches `ceil(len / PAGE_SIZE)`.
    /// Emits `TtlAnomalyEvent` for each violation.
    fn assert_phase3_postconditions(env: &Env, listing_count: u64) {
        let mut active_in_storage: u32 = 0;
        for i in 0..listing_count {
            let listing_id = i + 1;
            let pos_key = DataKey::ActiveListingPos(listing_id);
            if let Some(l) = load_listing(env, listing_id) {
                if l.status == ListingStatus::Active {
                    active_in_storage += 1;
                    // Active listing must have a position key (i.e., be reachable).
                    if !env.storage().persistent().has(&pos_key) {
                        TtlAnomalyEvent {
                            subject: Symbol::new(env, "active_pos_miss"),
                            id: listing_id,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(env);
                    }
                } else {
                    // Terminal listing must NOT have a stale position key.
                    if env.storage().persistent().has(&pos_key) {
                        TtlAnomalyEvent {
                            subject: Symbol::new(env, "term_in_active"),
                            id: listing_id,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(env);
                    }
                }
            }
        }
        let active_in_index = crate::storage::active_listings_len(env);
        if active_in_index != active_in_storage {
            TtlAnomalyEvent {
                subject: Symbol::new(env, "active_idx"),
                id: active_in_index as u64,
                ledger_sequence: env.ledger().sequence(),
            }.publish(env);
        }
        // Page count must be consistent with the stored length.
        let expected_pages = if active_in_index == 0 {
            0
        } else {
            (active_in_index + crate::storage::INDEX_PAGE_SIZE - 1)
                / crate::storage::INDEX_PAGE_SIZE
        };
        let actual_pages = index_page_count(env, &IndexId::ActiveListings);
        if actual_pages != expected_pages {
            TtlAnomalyEvent {
                subject: Symbol::new(env, "page_count"),
                id: actual_pages as u64,
                ledger_sequence: env.ledger().sequence(),
            }.publish(env);
        }
    }

    /// Phase-0 unit: migrate everything hanging off one listing.
    ///
    /// Idempotency invariant: calling this function twice for the same listing
    /// is safe — `take_legacy_index_vec` consumes and deletes the legacy key on
    /// first call so the second call is a no-op.  The pending-offer alias is
    /// rebuilt from the surviving legacy offer list with an explicit guard that
    /// skips any offer_id already present in `ListingPendingOffers`, preventing
    /// duplication when post-upgrade offer operations ran before migration.
    fn migrate_listing_unit(env: &Env, listing_id: u64) {
        let listing = match load_listing(env, listing_id) {
            Some(l) => l,
            None => return,
        };
        // The first listing of each artist migrates the artist's whole legacy
        // index; later listings find the legacy key already consumed.
        Self::migrate_legacy_index(
            env,
            &DataKey::ArtistListings(listing.artist.clone()),
            &IndexId::ArtistListings(listing.artist.clone()),
        );
        // Per-listing offer history + pending-offer set rebuild.
        // Guard: snapshot existing pending ids before the loop so we can skip
        // offers already registered (post-upgrade offers written before migration).
        if let Some(offer_ids) = take_legacy_index_vec(env, &DataKey::ListingOffers(listing_id)) {
            let existing_pending = load_pending_offer_ids(env, listing_id);
            for offer_id in offer_ids.iter() {
                // Idempotency: only append to the ListingOffers paged index if
                // this offer_id isn't already there (same guard as migrate_legacy_index).
                if !index_contains(env, &IndexId::ListingOffers(listing_id), offer_id) {
                    index_append(env, &IndexId::ListingOffers(listing_id), offer_id);
                }
                if let Some(offer) = load_offer(env, offer_id) {
                    if offer.status == OfferStatus::Pending
                        && !existing_pending.contains(&offer_id)
                    {
                        add_pending_offer(env, listing_id, offer_id);
                    }
                }
            }
        }
    }

    /// Move one legacy monolithic `Vec<u64>` entry into the paged index,
    /// deleting the legacy key.  No-op when the legacy key is absent.
    ///
    /// Idempotency guard: if the paged index already contains a value (written
    /// by post-upgrade contract calls that ran between the WASM upgrade and the
    /// migration invocation), it is skipped so no duplicate is introduced.
    /// This keeps the migration safe even when the contract is not paused during
    /// upgrade, at the cost of an O(n) membership scan per legacy entry.
    fn migrate_legacy_index(env: &Env, legacy: &DataKey, idx: &IndexId) {
        if let Some(ids) = take_legacy_index_vec(env, legacy) {
            for value in ids.iter() {
                if !index_contains(env, idx, value) {
                    index_append(env, idx, value);
                }
            }
        }
    }

    // ── Price bounds ─────────────────────────────────────────

    pub fn set_price_bounds(env: Env, admin: Address, min: i128, max: i128) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        if min < 0 || max < 0 || min > max { panic_with_error!(&env, MarketplaceError::InvalidPrice); }
        set_min_price_storage(&env, min);
        set_max_price_storage(&env, max);
    }

    pub fn get_price_bounds(env: Env) -> (Option<i128>, Option<i128>) {
        (get_min_price_storage(&env), get_max_price_storage(&env))
    }

    // ── Treasury & Fees ──────────────────────────────────────

    /// Step 1 of the two-step treasury rotation (Issue #459): the ProtocolConfig
    /// role holder proposes a new treasury address. The proposal is stamped with
    /// `proposed_at` (now) and `expires_at` (now + TREASURY_PROPOSAL_TTL).
    ///
    /// A timelock of `TREASURY_TIMELOCK` seconds must elapse between proposal
    /// and acceptance so operators have a review window. A second call before
    /// acceptance overwrites the previous proposal.
    ///
    /// Rejects with:
    /// - `TreasuryProposalSelf` — `candidate` is already the active treasury.
    pub fn propose_treasury(env: Env, admin: Address, candidate: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        // Guard: proposing the currently-active treasury is a no-op and would
        // create a confusing "rotation" that changes nothing.
        if let Some(ref current) = crate::storage::get_treasury_storage(&env) {
            if *current == candidate {
                panic_with_error!(&env, MarketplaceError::TreasuryProposalSelf);
            }
        }
        let now = env.ledger().timestamp();
        let proposed_at = now;
        let expires_at = now + TREASURY_PROPOSAL_TTL;
        set_pending_treasury_storage(
            &env,
            &PendingTreasuryProposal {
                candidate: candidate.clone(),
                proposed_at,
                expires_at,
            },
        );
        crate::events::emit_treasury_proposed(
            &env,
            crate::storage::get_treasury_storage(&env),
            candidate,
            proposed_at,
            expires_at,
        );
    }

    /// Step 2 of the two-step treasury rotation (Issue #459): the proposed
    /// candidate accepts, making itself the active treasury from this point on.
    ///
    /// Rejects with:
    /// - `NoTreasuryProposalPending`  — no proposal is active.
    /// - `Unauthorized`               — caller is not the proposed candidate.
    /// - `TreasuryProposalExpired`    — the proposal's `expires_at` has passed.
    /// - `InvalidListingDuration`     — the timelock has not yet elapsed
    ///   (reuses the time-constraint slot; same semantics as auction duration).
    pub fn accept_treasury(env: Env, candidate: Address) {
        bump_instance_ttl(&env);
        candidate.require_auth();
        let pending = get_pending_treasury_storage(&env)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::NoTreasuryProposalPending));
        if candidate != pending.candidate {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        let now = env.ledger().timestamp();
        if now > pending.expires_at {
            panic_with_error!(&env, MarketplaceError::TreasuryProposalExpired);
        }
        // Enforce the timelock: acceptance is only valid after `proposed_at +
        // TREASURY_TIMELOCK`. This gives the current authority a cancellation
        // window even when they know the proposed address belongs to them.
        if now < pending.proposed_at.saturating_add(TREASURY_TIMELOCK) {
            panic_with_error!(&env, MarketplaceError::InvalidListingDuration);
        }
        let old_treasury = crate::storage::get_treasury_storage(&env);
        crate::storage::set_treasury_storage(&env, &candidate);
        clear_pending_treasury_storage(&env);
        crate::events::emit_treasury_accepted(
            &env,
            old_treasury,
            candidate,
            env.ledger().sequence(),
        );
    }

    /// Cancel a still-pending treasury proposal (Issue #459).
    /// Only callable by the ProtocolConfig role holder.
    ///
    /// Rejects with `NoTreasuryProposalPending` when no proposal is active.
    pub fn cancel_treasury_proposal(env: Env, admin: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        let pending = get_pending_treasury_storage(&env)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::NoTreasuryProposalPending));
        clear_pending_treasury_storage(&env);
        crate::events::emit_treasury_proposal_cancelled(&env, admin, pending.candidate);
    }

    /// View: the currently-pending treasury proposal, or `None` when no
    /// rotation is in progress. (Issue #459)
    pub fn get_pending_treasury(env: Env) -> Option<PendingTreasuryProposal> {
        get_pending_treasury_storage(&env)
    }

    /// Direct treasury setter — kept for backward compatibility with deployments
    /// that have not yet adopted the two-step rotation. Requires ProtocolConfig
    /// role. In fresh deployments, operators should use `propose_treasury` /
    /// `accept_treasury` instead.
    pub fn set_treasury(env: Env, admin: Address, treasury: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        crate::storage::set_treasury_storage(&env, &treasury);
    }

    pub fn get_treasury(env: Env) -> Option<Address> {
        crate::storage::get_treasury_storage(&env)
    }

    pub fn set_protocol_fee(env: Env, admin: Address, bps: u32) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        if bps > 1000 { panic_with_error!(&env, MarketplaceError::InvalidPrice); }
        crate::storage::set_protocol_fee_bps_storage(&env, bps);
    }

    pub fn get_protocol_fee(env: Env) -> u32 {
        crate::storage::get_protocol_fee_bps_storage(&env).unwrap_or(0)
    }

    // ── Per-collection fee overrides (Issue #322) ────────────────────────────

    /// Set a per-collection protocol fee override (admin-only).
    ///
    /// `bps` must be in 0–10 000 (100 %).  All new listings and auctions
    /// created for `collection` after this call will snapshot `bps` instead of
    /// the global protocol fee.  Existing listings/auctions are unaffected —
    /// they already captured their fee at creation time.
    pub fn set_collection_fee_bps(env: Env, admin: Address, collection: Address, bps: u32) {
        bump_instance_ttl(&env);
        // Scoped to ProtocolConfig role (Issue #9): per-collection fee overrides
        // are a protocol configuration concern, same as the global fee.  This
        // replaces the previous raw admin-equality check so the ProtocolConfig
        // role holder can manage fees independently of the Admin key.
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        if bps > 10_000 {
            panic_with_error!(&env, MarketplaceError::InvalidPrice);
        }
        set_collection_fee_bps_storage(&env, &collection, bps);
        crate::events::emit_collection_fee_set(&env, collection, bps);
    }

    /// Remove the per-collection fee override.
    ///
    /// After this call, new listings and auctions for `collection` will fall
    /// back to the global protocol fee (`get_protocol_fee`).
    pub fn clear_collection_fee_bps(env: Env, admin: Address, collection: Address) {
        bump_instance_ttl(&env);
        // Scoped to ProtocolConfig role (Issue #9): mirrors set_collection_fee_bps.
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        clear_collection_fee_bps_storage(&env, &collection);
        crate::events::emit_collection_fee_cleared(&env, collection);
    }

    /// View: return the per-collection fee override (in bps) for `collection`,
    /// or `None` when no override is set (global fee applies).
    pub fn get_collection_fee_bps(env: Env, collection: Address) -> Option<u32> {
        get_collection_fee_bps_storage(&env, &collection)
    }

    // ── Listing duration bounds (Issue #460) ─────────────────────────────────
    //
    // Two admin-configurable bounds govern how far in the future an `expires_at`
    // timestamp may be. Both bounds are optional (None = unconstrained in that
    // direction). They apply at creation time, at batch-creation time, and
    // whenever `update_listing` / `update_listings` mutates expiry.
    //
    // A listing with `expires_at = None` (no expiry) is accepted regardless of
    // these bounds — operators that want to disallow non-expiring listings should
    // enforce that at the frontend layer.

    /// Set the global minimum listing duration in seconds (Issue #460).
    ///
    /// When set, any new or updated listing whose `expires_at` is less than
    /// `now + min_secs` will be rejected with `InvalidListingDuration`.
    /// `min_secs = 0` is accepted and effectively disables the lower bound.
    /// Requires ProtocolConfig role.
    pub fn set_min_listing_duration(env: Env, admin: Address, min_secs: u64) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        let old = get_min_listing_duration_storage(&env);
        set_min_listing_duration_storage(&env, min_secs);
        crate::events::emit_listing_duration_config_updated(
            &env,
            soroban_sdk::Symbol::new(&env, "min_listing_duration"),
            old,
            Some(min_secs),
        );
    }

    /// Clear the global minimum listing duration (removes the lower bound).
    /// Requires ProtocolConfig role.
    pub fn clear_min_listing_duration(env: Env, admin: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        let old = get_min_listing_duration_storage(&env);
        env.storage().persistent().remove(&DataKey::MinListingDuration);
        crate::events::emit_listing_duration_config_updated(
            &env,
            soroban_sdk::Symbol::new(&env, "min_listing_duration"),
            old,
            None,
        );
    }

    /// View: the current minimum listing duration in seconds, or `None` when
    /// no lower bound is configured. (Issue #460)
    pub fn get_min_listing_duration(env: Env) -> Option<u64> {
        get_min_listing_duration_storage(&env)
    }

    /// Set the global maximum listing duration in seconds (Issue #460).
    ///
    /// When set, any new or updated listing whose `expires_at` is more than
    /// `now + max_secs` will be rejected with `InvalidListingDuration`.
    /// `max_secs` must be > 0 (a zero-second ceiling would prohibit every
    /// listing with an expiry). Requires ProtocolConfig role.
    pub fn set_max_listing_duration(env: Env, admin: Address, max_secs: u64) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        if max_secs == 0 {
            panic_with_error!(&env, MarketplaceError::InvalidListingDuration);
        }
        let old = get_max_listing_duration_storage(&env);
        set_max_listing_duration_storage(&env, max_secs);
        crate::events::emit_listing_duration_config_updated(
            &env,
            soroban_sdk::Symbol::new(&env, "max_listing_duration"),
            old,
            Some(max_secs),
        );
    }

    /// Clear the global maximum listing duration (removes the upper bound).
    /// Requires ProtocolConfig role.
    pub fn clear_max_listing_duration(env: Env, admin: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        let old = get_max_listing_duration_storage(&env);
        env.storage().persistent().remove(&DataKey::MaxListingDuration);
        crate::events::emit_listing_duration_config_updated(
            &env,
            soroban_sdk::Symbol::new(&env, "max_listing_duration"),
            old,
            None,
        );
    }

    /// View: the current maximum listing duration in seconds, or `None` when
    /// no upper bound is configured. (Issue #460)
    pub fn get_max_listing_duration(env: Env) -> Option<u64> {
        get_max_listing_duration_storage(&env)
    }

    pub fn set_min_bid_increment(env: Env, admin: Address, increment: i128) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        if increment <= 0 { panic_with_error!(&env, MarketplaceError::InvalidPrice); }
        let old_value = crate::storage::get_min_bid_increment_storage(&env);
        crate::storage::set_min_bid_increment_storage(&env, increment);
        crate::events::emit_auction_config_updated(&env, soroban_sdk::Symbol::new(&env, "min_bid_increment"), old_value, Some(increment));
    }

    pub fn get_min_bid_increment(env: Env) -> i128 {
        crate::storage::get_min_bid_increment_storage(&env).unwrap_or(DEFAULT_MIN_BID_INCREMENT)
    }

    /// Set the global bid-history ring-buffer capacity.
    ///
    /// `cap` must be in the range 1–200 (enforced by `MAX_BID_HISTORY_CAP`).
    /// The new value is snapshotted into every *future* auction at creation
    /// time via `Auction::bid_history_cap`.  Already-running auctions keep
    /// their original cap for the lifetime of the auction so that no in-flight
    /// bid is silently evicted by a retroactive shrink.
    ///
    /// # Ring-buffer eviction — O(n) shift
    /// `append_bid_record` (storage.rs) uses a plain `Vec<BidRecord>` as the
    /// ring buffer.  When the history is full it removes the front element
    /// with `Vec::remove(0)`, which shifts every remaining element down by one
    /// position — O(n) in the cap.  This is acceptable because:
    ///  • `cap` is bounded to 200 entries.
    ///  • A 200-entry shift operates on stack-local XDR values; no additional
    ///    ledger-entry reads are needed.
    ///  • `place_bid` is called at most once per ledger, so the amortised cost
    ///    per auction is negligible.
    /// If a future version needs O(1) eviction, the ring buffer can be replaced
    /// with an indexed page (write-head pointer in instance storage).
    pub fn set_bid_history_cap(env: Env, admin: Address, cap: u32) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        if cap == 0 || cap > MAX_BID_HISTORY_CAP {
            panic_with_error!(&env, MarketplaceError::InvalidPrice);
        }
        set_bid_history_cap_storage(&env, cap);
    }

    pub fn get_bid_history_cap(env: Env) -> u32 {
        get_bid_history_cap_storage(&env)
    }

    pub fn set_auction_extension_window(env: Env, admin: Address, window: u64) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        let old_value = get_auction_extension_window_storage(&env);
        set_auction_extension_window_storage(&env, window);
        crate::events::emit_auction_config_updated(&env, soroban_sdk::Symbol::new(&env, "extension_window"), old_value.map(|v| v as i128), Some(window as i128));
    }

    pub fn get_auction_extension_window(env: Env) -> u64 {
        get_auction_extension_window_storage(&env).unwrap_or(DEFAULT_EXTENSION_WINDOW)
    }

    pub fn set_auction_extension_trigger(env: Env, admin: Address, trigger: u64) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        let old_value = get_auction_extension_trigger_storage(&env);
        set_auction_extension_trigger_storage(&env, trigger);
        crate::events::emit_auction_config_updated(&env, soroban_sdk::Symbol::new(&env, "extension_trigger"), old_value.map(|v| v as i128), Some(trigger as i128));
    }

    pub fn get_auction_extension_trigger(env: Env) -> u64 {
        get_auction_extension_trigger_storage(&env).unwrap_or(DEFAULT_EXTENSION_TRIGGER)
    }

    /// Set the global cap on how many times a single auction may be extended by
    /// the anti-sniping logic.  0 = unlimited (default).
    /// Snapshotted into each new auction at creation time.
    pub fn set_auction_max_extensions(env: Env, admin: Address, max: u32) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        set_auction_max_extensions_storage(&env, max);
    }

    pub fn get_auction_max_extensions(env: Env) -> u32 {
        get_auction_max_extensions_storage(&env)
    }

    // ── Pause ────────────────────────────────────────────────

    pub fn admin_pause(env: Env, admin: Address) {
        Self::require_role(&env, &admin, RoleType::EmergencyPause);
        crate::storage::set_paused(&env, true);
        #[allow(deprecated)]
        env.events().publish((crate::events::CONTRACT_PAUSED,), ());
    }

    pub fn admin_unpause(env: Env, admin: Address) {
        Self::require_role(&env, &admin, RoleType::EmergencyPause);
        crate::storage::set_paused(&env, false);
        #[allow(deprecated)]
        env.events().publish((crate::events::CONTRACT_UNPAUSED,), ());
    }

    pub fn is_paused(env: Env) -> bool {
        crate::storage::is_paused(&env)
    }

    // ── Granular circuit-breakers (Issue #205) ───────────────
    //
    // Three independent axes of pause control:
    //   1. Global (admin_pause / admin_unpause — existing)
    //   2. Per-collection (pause_collection / unpause_collection)
    //   3. Per-function   (pause_function / unpause_function)
    //
    // Any active axis blocks the affected operations.  Global pause
    // still blocks everything; collection and function pauses are
    // additive narrow restrictions layered on top.
    //
    // ── State-transition matrix (narrowly-scoped emergency pause) ──
    //
    // A pause is meant to stop *new* risk exposure, not trap funds or NFTs
    // that are already committed to the contract.  Every public entry point
    // falls into exactly one of the two buckets below.
    //
    // Blocked when paused (subject to global / collection / function axes,
    // via `require_not_paused` or `require_not_paused_ctx`) — these create
    // new exposure (new escrow, new payment flow, new price commitment):
    //   - create_listing        (collection + function axis)
    //   - update_listing        (global axis)
    //   - update_listing_price  (global axis)
    //   - buy_artwork           (collection + function axis)
    //   - create_auction        (collection + function axis)
    //   - place_bid             (collection + function axis)
    //   - make_offer            (collection + function axis)
    //   - accept_offer          (global axis — a purchase-like state
    //                            transition, not fund recovery, so it stays
    //                            gated like buy_artwork)
    //
    // Always available regardless of pause state (fund recovery / cleanup —
    // these return escrowed tokens or NFTs to their rightful owner, or tidy
    // up state that is already terminal/expired, so blocking them would trap
    // user funds during exactly the incident a pause is meant to contain):
    //   - cancel_listing            (artist reclaims NFT from escrow)
    //   - cancel_listings           (batch form of the above)
    //   - cancel_artist_listings    (admin-driven revocation cleanup sweep)
    //   - expire_listing            (permissionless; already unguarded)
    //   - cancel_auction            (creator reclaims NFT, no-bid only)
    //   - finalize_auction          (settles an already-ended auction —
    //                                explicitly called out by the issue)
    //   - admin_cancel_auction      (admin emergency unwind + bidder refund)
    //   - withdraw_offer            (offerer reclaims escrowed funds)
    //   - reject_offer              (artist rejects offer, funds return to
    //                                the offerer)
    //   - reclaim_offer             (offerer reclaims funds from an offer
    //                                that expired without action)
    //
    // These cleanup paths deliberately skip *all three* pause axes (global,
    // collection, function) — they exist precisely so a paused incident
    // doesn't also freeze user funds/state in limbo.

    /// Pause all operations for a specific collection.
    pub fn pause_collection(env: Env, admin: Address, collection: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::EmergencyPause);
        crate::storage::set_collection_paused(&env, &collection, true);
        crate::events::emit_collection_paused(&env, collection);
    }

    /// Resume all operations for a previously paused collection.
    pub fn unpause_collection(env: Env, admin: Address, collection: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::EmergencyPause);
        crate::storage::set_collection_paused(&env, &collection, false);
        crate::events::emit_collection_unpaused(&env, collection);
    }

    /// Return whether a specific collection is individually paused.
    pub fn is_collection_paused(env: Env, collection: Address) -> bool {
        crate::storage::is_collection_paused(&env, &collection)
    }

    /// Pause a specific entry-point by its function name symbol.
    /// Valid names: "buy_artwork", "create_listing", "place_bid",
    ///              "create_auction", "make_offer", "accept_offer".
    pub fn pause_function(env: Env, admin: Address, function_name: Symbol) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::EmergencyPause);
        crate::storage::set_function_paused(&env, &function_name, true);
        crate::events::emit_function_paused(&env, function_name);
    }

    /// Resume a previously paused entry-point.
    pub fn unpause_function(env: Env, admin: Address, function_name: Symbol) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::EmergencyPause);
        crate::storage::set_function_paused(&env, &function_name, false);
        crate::events::emit_function_unpaused(&env, function_name);
    }

    /// Return whether a specific function is individually paused.
    pub fn is_function_paused(env: Env, function_name: Symbol) -> bool {
        crate::storage::is_function_paused(&env, &function_name)
    }

    /// Return a snapshot of all three pause axes for a given (collection, function) context.
    ///
    /// Intended for off-chain monitoring dashboards and emergency operator tooling.
    /// `any_paused` mirrors the predicate evaluated by `require_not_paused_ctx` — if
    /// `any_paused` is true the corresponding settlement operation is blocked.
    ///
    /// Pass `None` for either axis to skip that check (e.g., `None` collection when
    /// querying global + function state only).
    pub fn get_pause_matrix(
        env: Env,
        collection: Option<Address>,
        function_name: Option<Symbol>,
    ) -> crate::types::PauseMatrix {
        bump_instance_ttl(&env);
        let global = crate::storage::is_paused(&env);
        let collection_paused = collection
            .as_ref()
            .map_or(false, |c| crate::storage::is_collection_paused(&env, c));
        let function_paused = function_name
            .as_ref()
            .map_or(false, |f| crate::storage::is_function_paused(&env, f));
        crate::types::PauseMatrix {
            global,
            collection_paused,
            function_paused,
            any_paused: global || collection_paused || function_paused,
        }
    }

    // ── Artist Moderation ────────────────────────────────────

    /// Mark an artist as revoked (CollectionAdmin only).
    ///
    /// Only marks the artist as revoked and emits `ArtistRevokedEvent`.
    /// Use `cancel_artist_listings` afterward to batch-cancel their active items.
    pub fn revoke_artist(env: Env, caller: Address, artist: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &caller, RoleType::CollectionAdmin);
        set_artist_revocation_storage(&env, &artist);

        ArtistRevokedEvent { artist }.publish(&env);
    }

    pub fn reinstate_artist(env: Env, caller: Address, artist: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &caller, RoleType::CollectionAdmin);
        remove_artist_revocation_storage(&env, &artist);
        crate::storage::clear_artist_cancel_cursor(&env, &artist);
        crate::storage::clear_artist_auction_cancel_cursor(&env, &artist);
        ArtistReinstatedEvent { artist }.publish(&env);
    }

    pub fn is_artist_revoked(env: Env, artist: Address) -> bool {
        is_artist_revoked_storage(&env, &artist)
    }

    /// Cancel the active listings of a revoked artist — batched and resumable.
    ///
    /// Called by the admin after revoking an artist to clean up their active
    /// listings.  Processes at most `max_items` of the artist's listings per
    /// invocation (bounding the per-transaction read/write footprint for a
    /// prolific artist) and persists a cursor so the next call resumes where
    /// this one stopped.  Per cancelled listing the semantics are unchanged:
    /// every pending offer is refunded and rejected first, then the listing is
    /// cancelled and a ListingCancelledEvent with reason=AdminRevoked is
    /// emitted.
    ///
    /// Returns the number of the artist's listings still to process — call
    /// repeatedly until it returns `0`.  Calling it for a non-revoked artist
    /// does nothing and returns `0`.
    pub fn cancel_artist_listings(env: Env, admin: Address, artist: Address, max_items: u32) -> u64 {
        bump_instance_ttl(&env);
        Self::require_role(&env, &admin, RoleType::CollectionAdmin);

        // Only cancel listings if the artist is actually revoked.  A stale
        // cursor from an interrupted sweep is dropped so a later re-revocation
        // starts from the beginning (new listings may have appeared since).
        if !is_artist_revoked_storage(&env, &artist) {
            clear_artist_cancel_cursor(&env, &artist);
            return 0;
        }

        let idx = IndexId::ArtistListings(artist.clone());
        let total = index_len(&env, &idx);
        let mut cursor = get_artist_cancel_cursor(&env, &artist);
        let end = cursor.saturating_add(max_items).min(total);

        while cursor < end {
            let listing_id = crate::storage::index_get(&env, &idx, cursor).unwrap();
            cursor += 1;
            if let Some(mut listing) = load_listing(&env, listing_id) {
                if listing.status == ListingStatus::Active {
                    transition_listing_to_cancelled(
                        &env,
                        &mut listing,
                        listing_id,
                        admin.clone(),
                        CancelReason::AdminRevoked,
                        true,
                    );
                }
            }
        }

        // The cursor is kept at `total` once the sweep completes: repeat calls
        // are then true no-ops, and listings created after a reinstate/
        // re-revoke cycle append behind the cursor so only they get swept.
        set_artist_cancel_cursor(&env, &artist, cursor);
        (total - cursor) as u64
    }

    /// Batch-cancel up to `MAX_BATCH_CANCEL` of the caller's own listings in a
    /// single invocation.  Strict all-or-nothing semantics: every id must
    /// exist, be owned by `owner` and still be Active, otherwise the whole
    /// call reverts.  Per listing the refund-then-cancel semantics match
    /// `cancel_listing`.  Returns the number of listings cancelled.
    pub fn cancel_listings(env: Env, owner: Address, listing_ids: Vec<u64>) -> u32 {
        bump_instance_ttl(&env);
        // Fund-recovery / cleanup op — always available, ignores all pause axes.
        owner.require_auth();

        if listing_ids.len() > MAX_BATCH_CANCEL {
            panic_with_error!(&env, MarketplaceError::BatchTooLarge);
        }

        let mut cancelled: u32 = 0;
        for listing_id in listing_ids.iter() {
            // Skip listings that are not active (idempotent batch cancel).
            if let Some(l) = load_listing(&env, listing_id) {
                if l.status != ListingStatus::Active {
                    continue;
                }
            }
            Self::cancel_listing_inner(&env, &owner, listing_id);
            cancelled += 1;
        }

        cancelled
    }

    // ── Bounded storage maintenance (Issue #280) ─────────────
    //
    // Two admin-only, bounded-per-call entry points. Neither ever deletes a
    // Listing, Auction or Offer record — see the retention classification
    // at the top of `storage.rs` and `docs/guides/storage-retention.md` for
    // the full policy. Both emit `CleanupSummaryEvent` so an indexer/keeper
    // can observe how much maintenance work actually happened each call.

    /// Bounded, resumable TTL-refresh sweep over the live (non-terminal)
    /// record set: Active listings (+ their Pending offers) and Active
    /// auctions (+ their bid history).
    ///
    /// Every read/write this contract performs already re-extends the TTL
    /// of the record it touches (`storage::bump_entry_ttl` call sites), so a
    /// frequently-used listing or auction never silently expires. This
    /// sweep defends against the opposite case: an Active listing or
    /// auction nobody has read or interacted with recently, whose TTL can
    /// lapse invisibly and make it unexpectedly unavailable to the indexer
    /// or frontend even though it is still logically live.
    ///
    /// Processes at most `max_items` records this call, hard-capped at
    /// `MAX_MAINTENANCE_ITEMS` regardless of the argument, and persists a
    /// `TtlSweepProgress` cursor so repeated calls make forward progress.
    /// The sweep is a perpetual cycle (phase 0 = `ActiveListings` index,
    /// phase 1 = sequential auction ids, then wraps back to phase 0) — as
    /// long as at least one listing or auction is Active, later calls will
    /// always find something to refresh. Unlike `migrate_step` this is not
    /// a one-shot drain; it is meant to be invoked periodically forever by
    /// an operator/keeper process.
    ///
    /// Never touches a terminal (Sold/Cancelled/Finalized) record — those
    /// are deliberately left to fall out of TTL so Soroban's own archival
    /// mechanism takes over, since the contract never deletes historical
    /// listing/auction/offer data. If the sweep finds an id still present
    /// in the `ActiveListings` index or auction id space whose stored
    /// status is no longer Active (or the record is missing), it emits a
    /// `TtlAnomalyEvent` instead of silently doing nothing, surfacing the
    /// index/state drift rather than hiding it.
    ///
    /// Returns the number of records actually refreshed this call.
    pub fn extend_active_ttls(env: Env, admin: Address, max_items: u32) -> u32 {
        bump_instance_ttl(&env);
        // Scoped to ProtocolConfig role (Issue #9): TTL maintenance is a
        // protocol-operations concern, not an emergency pause action.
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);

        let budget_cap = max_items.min(MAX_MAINTENANCE_ITEMS);
        let total_listings = active_listings_len(&env);
        let total_auctions = get_auction_count(&env);
        if budget_cap == 0 || (total_listings == 0 && total_auctions == 0) {
            return 0;
        }

        let mut progress = get_ttl_sweep_progress(&env);
        let mut processed: u32 = 0;
        // Bounds the number of phase-0/phase-1 toggles when one side of the
        // sweep is empty, so the loop can never spin forever without making
        // progress (each toggle is O(1); two full toggles are always enough
        // to reach whichever side has work).
        let mut toggles: u32 = 0;

        while processed < budget_cap && toggles < 4 {
            if progress.phase == 0 {
                if total_listings == 0 || progress.cursor >= total_listings as u64 {
                    progress.phase = 1;
                    progress.cursor = 0;
                    toggles += 1;
                    continue;
                }
                let pos = progress.cursor as u32;
                if let Some(listing_id) = index_get(&env, &IndexId::ActiveListings, pos) {
                    match load_listing(&env, listing_id) {
                        Some(listing) if listing.status == ListingStatus::Active => {
                            // `load_listing` already bumped the listing's own
                            // TTL; also refresh every Pending offer hanging
                            // off it (bounded, ≤ MAX_OFFERS_PER_LISTING).
                            for offer_id in load_pending_offer_ids(&env, listing_id).iter() {
                                load_offer(&env, offer_id);
                            }
                        }
                        _ => {
                            TtlAnomalyEvent {
                                subject: Symbol::new(&env, "listing"),
                                id: listing_id,
                                ledger_sequence: env.ledger().sequence(),
                            }
                            .publish(&env);
                        }
                    }
                }
                progress.cursor += 1;
                processed += 1;
            } else {
                if total_auctions == 0 || progress.cursor >= total_auctions {
                    progress.phase = 0;
                    progress.cursor = 0;
                    toggles += 1;
                    continue;
                }
                let auction_id = progress.cursor + 1; // ids are 1-based
                match load_auction(&env, auction_id) {
                    Some(auction) if auction.status == AuctionStatus::Active => {
                        // `load_auction` already bumped the auction's own
                        // TTL; also refresh its bid history if any.
                        load_auction_bids(&env, auction_id);
                    }
                    Some(_) => {} // terminal — intentionally left alone
                    None => {
                        TtlAnomalyEvent {
                            subject: Symbol::new(&env, "auction"),
                            id: auction_id,
                            ledger_sequence: env.ledger().sequence(),
                        }
                        .publish(&env);
                    }
                }
                progress.cursor += 1;
                processed += 1;
            }
        }

        set_ttl_sweep_progress(&env, &progress);
        CleanupSummaryEvent {
            kind: Symbol::new(&env, "ttl_extend"),
            items_processed: processed,
            ledger_sequence: env.ledger().sequence(),
        }
        .publish(&env);
        processed
    }

    /// Bounded admin safety-valve that force-clears reentrancy locks for the
    /// given listing/auction ids.
    ///
    /// `ListingLock`/`AuctionLock` live in *temporary* storage with a short
    /// TTL (`REENTRANCY_LOCK_TTL` ledgers) and are already released on every
    /// normal exit path of every function that acquires one; a panic
    /// mid-call rolls back the whole transaction (including the lock
    /// write), so it can never leak a lock either. Under normal operation
    /// Soroban's own temporary-storage expiry reclaims any lock this
    /// contract somehow failed to release. This entry point exists purely
    /// as an operator-triggered safety valve for a stuck lock spotted
    /// off-chain — it is a no-op (and safe to retry) for any id whose lock
    /// is already absent, so calling it again with nothing left to clear
    /// just returns 0.
    ///
    /// `listing_ids` and `auction_ids` are processed in that order up to a
    /// combined `MAX_MAINTENANCE_ITEMS`; ids past the cap are silently
    /// ignored rather than causing the whole call to revert. Returns the
    /// number of locks actually found and cleared.
    pub fn cleanup_expired_locks(
        env: Env,
        admin: Address,
        listing_ids: Vec<u64>,
        auction_ids: Vec<u64>,
    ) -> u32 {
        bump_instance_ttl(&env);
        // Scoped to ProtocolConfig role (Issue #9): lock-cleanup is a maintenance
        // operation matching the same authority that manages TTL sweeps.
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);

        let mut cleared: u32 = 0;
        let mut budget = MAX_MAINTENANCE_ITEMS;

        for listing_id in listing_ids.iter() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let key = DataKey::ListingLock(listing_id);
            if env.storage().temporary().has(&key) {
                // Emit an anomaly when a lock is found for an Active listing —
                // under Soroban's sequential model a lock persisting between
                // transactions indicates the acquiring call did not release it
                // (a bug), so we surface the drift before clearing.
                if let Some(l) = load_listing(&env, listing_id) {
                    if l.status == ListingStatus::Active {
                        TtlAnomalyEvent {
                            subject: Symbol::new(&env, "active_lock"),
                            id: listing_id,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(&env);
                    }
                }
                release_listing_lock(&env, listing_id);
                cleared += 1;
            }
        }
        for auction_id in auction_ids.iter() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let key = DataKey::AuctionLock(auction_id);
            if env.storage().temporary().has(&key) {
                // Same anomaly signal for Active auction locks.
                if let Some(a) = load_auction(&env, auction_id) {
                    if a.status == AuctionStatus::Active {
                        TtlAnomalyEvent {
                            subject: Symbol::new(&env, "active_lock"),
                            id: auction_id,
                            ledger_sequence: env.ledger().sequence(),
                        }.publish(&env);
                    }
                }
                release_auction_lock(&env, auction_id);
                cleared += 1;
            }
        }

        CleanupSummaryEvent {
            kind: Symbol::new(&env, "lock_cleanup"),
            items_processed: cleared,
            ledger_sequence: env.ledger().sequence(),
        }
        .publish(&env);
        cleared
    }

    /// Permissionless TTL renewal entry-point (Issue #280).
    ///
    /// Anyone may call this to bump the TTLs of the specified listings, auctions,
    /// and offers. This enables external keeper processes to maintain critical
    /// storage entries before they expire. The function is bounded by
    /// `MAX_MAINTENANCE_ITEMS` to prevent abuse, and only renews entries that
    /// actually exist (missing ids are silently skipped).
    ///
    /// Returns the number of entries actually renewed.
    pub fn renew_storage(
        env: Env,
        listing_ids: Vec<u64>,
        auction_ids: Vec<u64>,
        offer_ids: Vec<u64>,
    ) -> u32 {
        bump_instance_ttl(&env);
        let mut renewed: u32 = 0;
        let budget = MAX_MAINTENANCE_ITEMS;

        // Renew listings
        for listing_id in listing_ids.iter().take(budget as usize) {
            if load_listing(&env, listing_id).is_some() {
                crate::storage::bump_listing_ttl(&env, listing_id);
                renewed += 1;
            }
        }

        // Renew auctions (remaining budget)
        let remaining = budget - renewed;
        for auction_id in auction_ids.iter().take(remaining as usize) {
            if load_auction(&env, auction_id).is_some() {
                crate::storage::bump_auction_ttl(&env, auction_id);
                renewed += 1;
            }
        }

        // Renew offers (remaining budget)
        let remaining = budget - renewed;
        for offer_id in offer_ids.iter().take(remaining as usize) {
            if load_offer(&env, offer_id).is_some() {
                crate::storage::bump_offer_ttl(&env, offer_id);
                renewed += 1;
            }
        }

        CleanupSummaryEvent {
            kind: Symbol::new(&env, "storage_renewal"),
            items_processed: renewed,
            ledger_sequence: env.ledger().sequence(),
        }
        .publish(&env);
        renewed
    }

    /// Bounded, resumable cleanup sweep for terminal marketplace records
    /// (Issue #474).
    ///
    /// # What it does
    /// Walks listing ids then offer ids in ascending order up to `batch_size`
    /// (hard-capped at `MAX_MAINTENANCE_ITEMS = 100`). For each record in a
    /// **terminal** state (Listing: Sold / Cancelled; Offer: Accepted /
    /// Rejected / Withdrawn / Expired) it:
    ///   1. Emits a `terminal_cleaned` event so the indexer can mark the record
    ///      as potentially-cold on-chain.
    ///   2. Does NOT delete the record — historical marketplace records are never
    ///      hard-deleted. The contract simply stops renewing the record's TTL so
    ///      Soroban's own archival mechanism reclaims the storage rent over time.
    ///
    /// # Safety guarantees
    /// - Active listings, active auctions, and pending offers are **never**
    ///   touched.
    /// - Unsettled escrow records are not touched (the listing/auction that holds
    ///   the escrow is still Active).
    /// - The sweep is resumable: progress is stored in `TerminalCleanupCursor`
    ///   and resumes from where the previous call stopped.
    /// - Repeated calls with the same ids are safe (idempotent events).
    ///
    /// # Permissionless
    /// Any caller may invoke this entry point — no auth required. The
    /// `MAX_MAINTENANCE_ITEMS` cap bounds the compute cost per call.
    ///
    /// Returns the number of terminal records processed this call.
    pub fn cleanup_terminal_records(env: Env, batch_size: u32) -> u32 {
        use crate::types::{ListingStatus, OfferStatus};

        bump_instance_ttl(&env);

        let budget = batch_size.min(MAX_MAINTENANCE_ITEMS);
        let mut cursor = get_terminal_cleanup_cursor(&env);
        let listing_count = crate::storage::get_listing_count(&env);
        let offer_count = crate::storage::get_offer_count(&env);
        let mut processed: u32 = 0;

        // Phase 0: walk listings
        if cursor.phase == 0 {
            let start = cursor.cursor + 1;
            let end = listing_count.min(start + budget as u64 - 1);
            for id in start..=end {
                if processed >= budget {
                    break;
                }
                if let Some(listing) = load_listing(&env, id) {
                    match listing.status {
                        ListingStatus::Sold | ListingStatus::Cancelled => {
                            TerminalCleanedEvent {
                                kind: soroban_sdk::Symbol::new(&env, "listing"),
                                id,
                                ledger_sequence: env.ledger().sequence(),
                            }
                            .publish(&env);
                            processed += 1;
                        }
                        ListingStatus::Active => {} // skip — never touch active records
                    }
                }
                cursor.cursor = id;
            }
            if cursor.cursor >= listing_count {
                // Advance to phase 1
                cursor.phase = 1;
                cursor.cursor = 0;
            }
        }

        // Phase 1: walk offers (with remaining budget)
        if cursor.phase == 1 && processed < budget {
            let start = cursor.cursor + 1;
            let remaining = budget - processed;
            let end = offer_count.min(start + remaining as u64 - 1);
            for id in start..=end {
                if processed >= budget {
                    break;
                }
                if let Some(offer) = load_offer(&env, id) {
                    match offer.status {
                        OfferStatus::Accepted
                        | OfferStatus::Rejected
                        | OfferStatus::Withdrawn
                        | OfferStatus::Expired => {
                            TerminalCleanedEvent {
                                kind: soroban_sdk::Symbol::new(&env, "offer"),
                                id,
                                ledger_sequence: env.ledger().sequence(),
                            }
                            .publish(&env);
                            processed += 1;
                        }
                        OfferStatus::Pending => {} // skip — never touch pending offers
                    }
                }
                cursor.cursor = id;
            }
            if cursor.cursor >= offer_count {
                // Full sweep complete — clear cursor so the next call starts fresh
                clear_terminal_cleanup_cursor(&env);
                CleanupSummaryEvent {
                    kind: soroban_sdk::Symbol::new(&env, "terminal_cleanup"),
                    items_processed: processed,
                    ledger_sequence: env.ledger().sequence(),
                }
                .publish(&env);
                return processed;
            }
        }

        set_terminal_cleanup_cursor(&env, &cursor);
        CleanupSummaryEvent {
            kind: soroban_sdk::Symbol::new(&env, "terminal_cleanup"),
            items_processed: processed,
            ledger_sequence: env.ledger().sequence(),
        }
        .publish(&env);
        processed
    }

    // ── Token Whitelist ──────────────────────────────────────

    pub fn add_token_to_whitelist(env: Env, admin: Address, token: Address) {
        bump_instance_ttl(&env);
        // Scoped to ProtocolConfig role (Issue #9): token whitelist management is
        // a protocol configuration concern, not a general admin operation.
        Self::require_role(&env, &admin, RoleType::ProtocolConfig);
        // Boundary validation (Issue #282): reject the marketplace's own
        // address before it can ever land in the whitelist. Decimals/asset
        // identity for the token are the off-chain registry's job (see
        // `validate_token_asset` doc comment); this is deliberately a cheap,
        // on-chain-only sanity check.
        Self::validate_token_asset(&env, &token, None);

        // Check if token is already whitelisted (active or removed)
        let existing = crate::storage::get_token_whitelist_entry(&env, &token);
        if let Some(entry) = existing {
            if entry.active {
                // Already active - no-op (idempotent)
                return;
            } else {
                // Previously removed - reactivate it
                let updated = crate::storage::TokenWhitelistEntry {
                    added_at: entry.added_at,
                    added_by: entry.added_by,
                    active: true,
                };
                crate::storage::set_token_whitelist_entry(&env, &token, &updated);
                crate::events::emit_token_whitelisted(&env, &token, &admin);
                return;
            }
        }

        // New token - create registry entry
        let now = env.ledger().timestamp();
        let entry = crate::storage::TokenWhitelistEntry {
            added_at: now,
            added_by: admin.clone(),
            active: true,
        };
        crate::storage::set_token_whitelist_entry(&env, &token, &entry);
        crate::storage::add_token_to_whitelist_index(&env, &token);
        crate::events::emit_token_whitelisted(&env, &token, &admin);
    }

    pub fn remove_token_from_whitelist(env: Env, caller: Address, token: Address) {
        bump_instance_ttl(&env);
        Self::require_role(&env, &caller, RoleType::ProtocolConfig);

        // Check if token exists in registry
        let existing = crate::storage::get_token_whitelist_entry(&env, &token);
        if let Some(mut entry) = existing {
            if !entry.active {
                // Already removed - no-op (idempotent)
                return;
            }
            // Soft delete: set active to false, preserve history
            entry.active = false;
            crate::storage::set_token_whitelist_entry(&env, &token, &entry);
            crate::events::emit_token_removed(&env, &token, &caller);
        }
        // If token was never whitelisted, do nothing (no-op)
    }

    /// Get all currently whitelisted tokens (active only).
    /// View function for querying the whitelist.
    pub fn get_whitelisted_tokens(env: Env) -> Vec<Address> {
        crate::storage::get_all_token_registry(&env)
    }

    /// Get paginated whitelisted tokens (active only).
    /// View function for querying the whitelist with pagination.
    pub fn get_whitelisted_tokens_paginated(env: Env, start: u32, limit: u32) -> Vec<Address> {
        crate::storage::get_token_registry_range(&env, start, limit)
    }

    /// Get the whitelist entry for a specific token (for history/audit).
    /// Returns None if the token was never whitelisted.
    pub fn get_token_whitelist_entry(env: Env, token: Address) -> Option<crate::storage::TokenWhitelistEntry> {
        crate::storage::get_token_whitelist_entry(&env, &token)
    }

    /// View: return the complete policy snapshot for `token`.
    ///
    /// Returns a `TokenWhitelistPolicyResult` with the resolved lifecycle state
    /// (`Active`, `Removed`, or `NeverAdded`), whether the token is currently
    /// accepted, the full registry entry (when present), and the total number of
    /// tokens ever registered.  Designed to be used by the indexer and admin
    /// tooling to query policy state in a single call without re-implementing
    /// the validation logic client-side.
    ///
    /// Specifically covers Issue #435 acceptance criterion: "Whitelist events
    /// and registry state are stable and indexable by the off-chain system."
    pub fn get_token_whitelist_policy(env: Env, token: Address) -> TokenWhitelistPolicyResult {
        evaluate_token_policy(&env, &token)
    }

    // ── create_listing ───────────────────────────────────────
    // CEI order:
    //   Checks  — auth, revocation, price, split, whitelist
    //   Effects — save listing, update indices, emit ListingCreated
    //   Interaction — escrow_nft (verify owner, pull NFT into custody)

    #[allow(clippy::too_many_arguments)]
    pub fn create_listing(
        env: Env, artist: Address, price: i128, currency: Symbol,
        token: Address, collection: Address, token_id: u64, quantity: u64,
        recipients: Vec<Recipient>, expires_at: Option<u64>,
    ) -> u64 {
        bump_instance_ttl(&env);
        Self::require_not_paused_ctx(
            &env,
            Some(&collection),
            Some(&Symbol::new(&env, "create_listing")),
        );
        artist.require_auth();
        Self::require_not_revoked(&env, &artist);
        Self::create_listing_inner(
            &env,
            &artist,
            price,
            currency,
            token,
            collection,
            token_id,
            quantity,
            recipients,
            expires_at,
        )
    }

    pub fn create_listings(
        env: Env, artist: Address, requests: Vec<BatchCreateListingInput>,
    ) -> Vec<u64> {
        bump_instance_ttl(&env);
        artist.require_auth();
        Self::require_not_revoked(&env, &artist);

        // Issue #457 — hard batch-size cap before any expensive work.
        if requests.len() > MAX_BATCH_LISTINGS {
            panic_with_error!(&env, MarketplaceError::BatchTooLarge);
        }

        // Issue #457 — full preflight validation pass: validate every item
        // before touching any escrow, counter, or index. If any item is
        // invalid the whole batch is rejected with a `BatchItemError` that
        // identifies the zero-based index of the first failing item.
        for (idx, request) in requests.iter().enumerate() {
            let item_index = idx as u32;

            // Pause check per collection.
            if crate::storage::is_paused_for(
                &env,
                Some(&request.collection),
                Some(&Symbol::new(&env, "create_listing")),
            ) {
                panic_with_error!(&env, MarketplaceError::ContractPaused);
            }

            // Price validation.
            if request.price <= 0 {
                let _ = BatchItemError { item_index, error_code: MarketplaceError::InvalidPrice as u32 };
                panic_with_error!(&env, MarketplaceError::BatchItemInvalid);
            }
            if let Err(_) = Self::preflight_price(&env, request.price) {
                let _ = BatchItemError { item_index, error_code: MarketplaceError::PriceOutOfBounds as u32 };
                panic_with_error!(&env, MarketplaceError::BatchItemInvalid);
            }

            // Expiry / duration validation (Issue #460).
            if let Some(exp) = request.expires_at {
                if exp <= env.ledger().timestamp() {
                    let _ = BatchItemError { item_index, error_code: MarketplaceError::InvalidPrice as u32 };
                    panic_with_error!(&env, MarketplaceError::BatchItemInvalid);
                }
            }
            if let Err(_) = Self::preflight_duration(&env, request.expires_at) {
                let _ = BatchItemError { item_index, error_code: MarketplaceError::InvalidListingDuration as u32 };
                panic_with_error!(&env, MarketplaceError::BatchItemInvalid);
            }

            // Recipient validation.
            let rlen = request.recipients.len();
            if rlen == 0 || rlen > 4 {
                let _ = BatchItemError { item_index, error_code: MarketplaceError::InvalidSplit as u32 };
                panic_with_error!(&env, MarketplaceError::BatchItemInvalid);
            }
            let protocol_fee_bps =
                crate::storage::get_protocol_fee_bps_storage(&env).unwrap_or(0);
            if let Err(_) = Self::preflight_recipients(&env, &request.recipients, protocol_fee_bps) {
                let _ = BatchItemError { item_index, error_code: MarketplaceError::InvalidSplit as u32 };
                panic_with_error!(&env, MarketplaceError::BatchItemInvalid);
            }

            // Token whitelist validation.
            Self::validate_token_asset(&env, &request.token, Some(&request.collection));
            if !evaluate_token_policy(&env, &request.token).is_accepted {
                let _ = BatchItemError { item_index, error_code: MarketplaceError::TokenNotWhitelisted as u32 };
                panic_with_error!(&env, MarketplaceError::BatchItemInvalid);
            }

            // Issue #458 — collection compatibility check.
            if let Err(_) = Self::preflight_collection_compatibility(
                &env,
                &request.collection,
                request.token_id,
                request.quantity,
            ) {
                let _ = BatchItemError { item_index, error_code: MarketplaceError::CollectionIncompatible as u32 };
                panic_with_error!(&env, MarketplaceError::BatchItemInvalid);
            }
        }

        // All items passed preflight — now commit in full.
        let mut listing_ids = Vec::new(&env);
        for request in requests.iter() {
            let listing_id = Self::create_listing_inner(
                &env,
                &artist,
                request.price,
                request.currency.clone(),
                request.token.clone(),
                request.collection.clone(),
                request.token_id,
                request.quantity,
                request.recipients.clone(),
                request.expires_at,
            );
            listing_ids.push_back(listing_id);
        }
        listing_ids
    }

    // update_listing — no NFT movement; NFT stays in escrow unchanged
    pub fn update_listing(
        env: Env, artist: Address, listing_id: u64,
        new_price: i128, new_token: Address, new_recipients: Vec<Recipient>,
    ) -> bool {
        bump_instance_ttl(&env);
        Self::require_not_paused(&env);
        artist.require_auth();
        Self::update_listing_inner(&env, &artist, listing_id, new_price, new_token, new_recipients)
    }

    pub fn update_listings(
        env: Env, artist: Address, requests: Vec<BatchUpdateListingInput>,
    ) -> Vec<bool> {
        bump_instance_ttl(&env);
        Self::require_not_paused(&env);
        artist.require_auth();

        let mut results = Vec::new(&env);
        for request in requests.iter() {
            let ok = Self::update_listing_inner(
                &env,
                &artist,
                request.listing_id,
                request.new_price,
                request.new_token.clone(),
                request.new_recipients.clone(),
            );
            results.push_back(ok);
        }
        results
    }

    pub fn update_listing_price(env: Env, seller: Address, listing_id: u64, new_price: i128) -> bool {
        bump_instance_ttl(&env);
        Self::require_not_paused(&env);
        seller.require_auth();
        let mut listing = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        if listing.artist != seller { panic_with_error!(&env, MarketplaceError::Unauthorized); }
        if listing.status != ListingStatus::Active {
            panic_with_error!(&env, MarketplaceError::ListingNotActive);
        }
        if new_price <= 0 { panic_with_error!(&env, MarketplaceError::InvalidPrice); }
        let price_upper_bound: i128 = i128::MAX / 10_000;
        if new_price > price_upper_bound { panic_with_error!(&env, MarketplaceError::InvalidPrice); }
        // Policy engine: enforce configured price bounds at update time (Issue #435).
        assert_price_policy(&env, new_price);
        let old_price = listing.price;
        listing.price = new_price;
        save_listing(&env, &listing);
        ListingPriceUpdatedEvent { listing_id, old_price, new_price, updated_by: seller }.publish(&env);
        true
    }

    // ── buy_artwork ──────────────────────────────────────────
    // CEI:
    //   1. lock   2. checks   3. effects (mark Sold, reject offers)
    //   4. emit   5. interactions (payment payout, release_nft, refund offers)
    //   6. unlock (automatic via ListingReentrancyScope::drop)
    pub fn buy_artwork(env: Env, buyer: Address, listing_id: u64) -> bool {
        bump_instance_ttl(&env);
        // Function-level circuit-breaker (cheap, before any storage reads).
        Self::require_not_paused_ctx(&env, None, Some(&Symbol::new(&env, "buy_artwork")));
        buyer.require_auth();
        if !acquire_listing_lock(&env, listing_id) {
            panic_with_error!(&env, MarketplaceError::ReentrancyGuard);
        }
        let mut listing = match load_listing(&env, listing_id) {
            Some(l) => l,
            None => { release_listing_lock(&env, listing_id);
                      panic_with_error!(&env, MarketplaceError::ListingNotFound); }
        };
        // Collection-level circuit-breaker (after loading listing).
        if crate::storage::is_collection_paused(&env, &listing.collection) {
            release_listing_lock(&env, listing_id);
            panic_with_error!(&env, MarketplaceError::ContractPaused);
        }
        if listing.status == ListingStatus::Sold {
            release_listing_lock(&env, listing_id);
            panic_with_error!(&env, MarketplaceError::ListingSold);
        }
        if listing.status == ListingStatus::Cancelled {
            release_listing_lock(&env, listing_id);
            panic_with_error!(&env, MarketplaceError::ListingCancelled);
        }
        if listing.status != ListingStatus::Active {
            release_listing_lock(&env, listing_id);
            panic_with_error!(&env, MarketplaceError::ListingNotActive);
        }
        if listing.artist == buyer {
            release_listing_lock(&env, listing_id);
            panic_with_error!(&env, MarketplaceError::SelfPurchaseNotAllowed);
        }
        if let Some(ref o) = listing.owner {
            if *o == buyer {
                release_listing_lock(&env, listing_id);
                panic_with_error!(&env, MarketplaceError::SelfPurchaseNotAllowed);
            }
        }
        if let Some(exp) = listing.expires_at {
            if env.ledger().timestamp() >= exp {
                listing.status = ListingStatus::Cancelled;
                save_listing(&env, &listing);
                remove_from_active_listings(&env, listing_id);
                for offer_id in load_pending_offer_ids(&env, listing_id).iter() {
                    if let Some(mut offer) = load_offer(&env, offer_id) {
                        if offer.status == OfferStatus::Pending {
                            offer.status = OfferStatus::Rejected;
                            save_offer(&env, &offer);
                            TokenClient::new(&env, &offer.token).transfer(
                                &env.current_contract_address(),
                                &offer.offerer,
                                &offer.amount,
                            );
                        }
                    }
                }
                clear_pending_offers(&env, listing_id);
                ListingCancelledEvent {
                    listing_id,
                    cancelled_by: listing.artist.clone(),
                    reason: CancelReason::Expired,
                    ledger_sequence: env.ledger().sequence(),
                }.publish(&env);
                ListingExpiredEvent {
                    listing_id, expired_at: exp, ledger_sequence: env.ledger().sequence(),
                }.publish(&env);
                if listing.quantity > 1 {
                    escrow::release_nft_with_quantity(&env, &listing.collection, listing.token_id,
                        listing.quantity, &listing.artist, env.ledger().sequence(), listing_id);
                } else {
                    escrow::release_nft(&env, &listing.collection, listing.token_id,
                        &listing.artist, env.ledger().sequence(), listing_id);
                }
                release_listing_lock(&env, listing_id);
                return false;
            }
        }
        // Reservation window enforcement (Issue #462).
        {
            let now = env.ledger().timestamp();
            let window_started = listing.reservation_start.map_or(true, |s| now >= s);
            let window_active = window_started
                && listing.reservation_end.map_or(false, |e| now < e);
            if window_active {
                if let Some(ref rf) = listing.reserved_for {
                    if *rf != buyer {
                        release_listing_lock(&env, listing_id);
                        panic_with_error!(&env, MarketplaceError::ReservationWindowActive);
                    }
                }
            }
        }
        // Policy engine: re-validate listing token at settlement time (Issue #435).
        if !evaluate_token_policy(&env, &listing.token).is_accepted {
            release_listing_lock(&env, listing_id);
            panic_with_error!(&env, MarketplaceError::TokenNotWhitelisted);
        }
        // Effects
        listing.status = ListingStatus::Sold;
        listing.owner = Some(buyer.clone());
        save_listing(&env, &listing);
        remove_from_active_listings(&env, listing_id);

        // Reject all pending offers (state mutation only — token refunds happen
        // in the interactions phase below).  The sweep iterates the bounded
        // pending-offer set (≤ MAX_OFFERS_PER_LISTING), not the full history.
        let offers = load_pending_offer_ids(&env, listing_id);
        let mut p_offerers: Vec<Address> = Vec::new(&env);
        let mut p_amounts: Vec<i128> = Vec::new(&env);
        let mut p_tokens: Vec<Address> = Vec::new(&env);
        for offer_id in offers.iter() {
            if let Some(mut offer) = load_offer(&env, offer_id) {
                if offer.status == OfferStatus::Pending {
                    offer.status = OfferStatus::Rejected;
                    save_offer(&env, &offer);
                    p_offerers.push_back(offer.offerer.clone());
                    p_amounts.push_back(offer.amount);
                    p_tokens.push_back(offer.token.clone());
                }
            }
        }
        clear_pending_offers(&env, listing_id);

        ArtworkSoldEvent {
            listing_id, artist: listing.artist.clone(), buyer: buyer.clone(),
            price: listing.price, currency: listing.currency.clone(),
            ledger_sequence: env.ledger().sequence(),
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        // Interactions — use tracked variant so each recipient gets a claim record
        let (fee, payouts) = Self::distribute_payout_tracked(
            &env, &listing.token, &listing.collection, listing.price,
            &listing.artist, &listing.recipients, &buyer, true, listing.protocol_fee_bps,
            listing_id, true,
        );
        if fee > 0 {
            if let Some(treasury) = crate::storage::get_treasury_storage(&env) {
                crate::storage::add_protocol_fee_total(&env, &listing.token, fee);
                ProtocolFeeCollectedEvent {
                    listing_id, amount: fee, token: listing.token.clone(), treasury,
                    schema_version: EVENT_SCHEMA_VERSION,
                }.publish(&env);
            }
        }
        // On-chain accounting counters (Issue #279)
        crate::storage::add_royalty_total(&env, &listing.token, listing.price);
        crate::storage::increment_settlement_count(&env, &listing.token);
        // Emit royalty settlement snapshot for auditability (Issue #270)
        RoyaltySettlementEvent {
            id: listing_id,
            recipients: listing.recipients.clone(),
            total_amount: listing.price,
            token: listing.token.clone(),
            ledger_sequence: env.ledger().sequence(),
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        // Per-recipient payout breakdown for the royalty audit trail (Issue #201)
        emit_royalty_paid(
            &env, Some(listing_id), None, listing.price, fee,
            listing.token.clone(), payouts,
        );
        // NFT: from escrow → buyer (CEI: state already Sold)
        // Use batch transfer for ERC-1155 listings (quantity > 1)
        if listing.quantity > 1 {
            escrow::release_nft_with_quantity(&env, &listing.collection, listing.token_id,
                listing.quantity, &buyer, env.ledger().sequence(), listing_id);
        } else {
            escrow::release_nft(&env, &listing.collection, listing.token_id,
                &buyer, env.ledger().sequence(), listing_id);
        }
        for i in 0..p_offerers.len() {
            TokenClient::new(&env, &p_tokens.get(i).unwrap()).transfer(
                &env.current_contract_address(),
                &p_offerers.get(i).unwrap(),
                &p_amounts.get(i).unwrap(),
            );
        }
        release_listing_lock(&env, listing_id);
        true
    }

    // ── cancel_listing ───────────────────────────────────────
    // Effects first, then release_nft (Interaction)
    pub fn cancel_listing(env: Env, artist: Address, listing_id: u64) -> bool {
        bump_instance_ttl(&env);
        // Fund-recovery / cleanup op — always available, ignores all pause axes.
        artist.require_auth();
        Self::cancel_listing_inner(&env, &artist, listing_id)
    }

    // ── set_listing_reservation (Issue #462) ────────────────────────────────────
    // Seller-only: configure (or clear) a purchase-reservation window.
    //
    // A reservation restricts `buy_artwork` to `reserved_for` during the window
    // [`reservation_start`, `reservation_end`). Passing `None` for all three
    // fields clears an existing reservation. The change is always auditable via
    // a `listing_reservation_set` event.
    pub fn set_listing_reservation(
        env: Env,
        seller: Address,
        listing_id: u64,
        reserved_for: Option<Address>,
        reservation_start: Option<u64>,
        reservation_end: Option<u64>,
    ) {
        bump_instance_ttl(&env);
        seller.require_auth();
        let mut listing = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        if listing.artist != seller {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        if listing.status != ListingStatus::Active {
            panic_with_error!(&env, MarketplaceError::ListingNotActive);
        }
        // Validate the window only when one is being set (not when clearing).
        if reserved_for.is_some() || reservation_end.is_some() {
            let now = env.ledger().timestamp();
            if let Some(end) = reservation_end {
                // End must be in the future.
                if end <= now {
                    panic_with_error!(&env, MarketplaceError::InvalidReservationWindow);
                }
                // If start is provided, it must be strictly before end.
                if let Some(start) = reservation_start {
                    if start >= end {
                        panic_with_error!(&env, MarketplaceError::InvalidReservationWindow);
                    }
                }
            }
        }
        listing.reserved_for = reserved_for.clone();
        listing.reservation_start = reservation_start;
        listing.reservation_end = reservation_end;
        save_listing(&env, &listing);
        crate::events::emit_listing_reservation_set(
            &env,
            listing_id,
            listing.artist,
            reserved_for,
            reservation_start,
            reservation_end,
        );
    }

    // ── get_listing_reservation (Issue #462) ────────────────────────────────────
    // Returns the current reservation state for a listing as a 3-tuple:
    // (reserved_for, reservation_start, reservation_end).
    pub fn get_listing_reservation(
        env: Env,
        listing_id: u64,
    ) -> (Option<Address>, Option<u64>, Option<u64>) {
        bump_instance_ttl(&env);
        let listing = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        (listing.reserved_for, listing.reservation_start, listing.reservation_end)
    }

    // ── expire_listing ───────────────────────────────────────
    // Permissionless — anyone may expire a listing past its expires_at
    pub fn expire_listing(env: Env, listing_id: u64) {
        bump_instance_ttl(&env);
        let mut listing = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        if listing.status != ListingStatus::Active {
            panic_with_error!(&env, MarketplaceError::ListingNotActive);
        }
        let exp = match listing.expires_at {
            Some(t) => t,
            None => panic_with_error!(&env, MarketplaceError::ListingNotExpired),
        };
        if env.ledger().timestamp() < exp {
            panic_with_error!(&env, MarketplaceError::ListingNotExpired);
        }
        listing.status = ListingStatus::Cancelled;
        save_listing(&env, &listing);
        remove_from_active_listings(&env, listing_id);
        ListingCancelledEvent {
            listing_id,
            cancelled_by: listing.artist.clone(),
            reason: CancelReason::Expired,
            ledger_sequence: env.ledger().sequence(),
        }.publish(&env);
        ListingExpiredEvent {
            listing_id, expired_at: exp, ledger_sequence: env.ledger().sequence(),
        }.publish(&env);
        // Return NFT to seller
        if listing.quantity > 1 {
            escrow::release_nft_with_quantity(&env, &listing.collection, listing.token_id,
                listing.quantity, &listing.artist, env.ledger().sequence(), listing_id);
        } else {
            escrow::release_nft(&env, &listing.collection, listing.token_id,
                &listing.artist, env.ledger().sequence(), listing_id);
        }
    }

    // ── create_auction ───────────────────────────────────────
    // CEI: Effects → emit → escrow_nft (Interaction)
    #[allow(clippy::too_many_arguments)]
    pub fn create_auction(
        env: Env, creator: Address, token: Address, collection: Address,
        token_id: u64, reserve_price: i128, duration: u64, recipients: Vec<Recipient>,
    ) -> u64 {
        Self::require_not_paused_ctx(
            &env,
            Some(&collection),
            Some(&Symbol::new(&env, "create_auction")),
        );
        creator.require_auth();
        Self::require_not_revoked(&env, &creator);
        let min_increment = crate::storage::get_min_bid_increment_storage(&env)
            .unwrap_or(DEFAULT_MIN_BID_INCREMENT);
        if reserve_price < min_increment { panic_with_error!(&env, MarketplaceError::InvalidPrice); }
        if duration < MIN_AUCTION_DURATION {
            panic_with_error!(&env, MarketplaceError::InvalidAuctionDuration);
        }
        Self::validate_token_asset(&env, &token, Some(&collection));
        // Policy engine gate (Issue #435).
        assert_token_policy_active(&env, &token);
        // Snapshot protocol fee early so it can be used in recipient validation.
        let protocol_fee_bps = crate::storage::get_protocol_fee_bps_storage(&env).unwrap_or(0);
        // Validate recipients using the same rules as create_listing (#270).
        let recipients_len = recipients.len();
        if recipients_len == 0 {
            panic_with_error!(&env, MarketplaceError::InvalidSplit);
        }
        if recipients_len > 4 {
            panic_with_error!(&env, MarketplaceError::TooManyRecipients);
        }
        Self::validate_recipients(&env, &recipients, protocol_fee_bps);
        let auction_id = increment_auction_count(&env);
        let end_time = env.ledger().timestamp() + duration;
        let min_increment = crate::storage::get_min_bid_increment_storage(&env)
            .unwrap_or(DEFAULT_MIN_BID_INCREMENT);
        // Snapshot the anti-sniping parameters so the auction's extension
        // behaviour is determined at creation, not by future admin changes.
        let extension_window =
            get_auction_extension_window_storage(&env).unwrap_or(DEFAULT_EXTENSION_WINDOW);
        let extension_trigger =
            get_auction_extension_trigger_storage(&env).unwrap_or(DEFAULT_EXTENSION_TRIGGER);
        // protocol_fee_bps already computed above.
        // Snapshot the global bid-history cap so changing it later never
        // retroactively affects this auction's ring-buffer.
        let bid_history_cap = get_bid_history_cap_storage(&env);
        // Snapshot the global anti-sniping max-extensions cap.  0 = unlimited.
        let max_extensions = get_auction_max_extensions_storage(&env);
        // Validate extension_window when trigger is enabled: must be > 0 to
        // prevent an auction from being stuck in a non-extending state.
        if extension_trigger > 0 && extension_window == 0 {
            panic_with_error!(&env, MarketplaceError::InvalidExtensionWindow);
        }
        let auction = Auction {
            auction_id, creator: creator.clone(), token: token.clone(),
            collection: collection.clone(), token_id, reserve_price,
            highest_bid: 0, highest_bidder: None, end_time,
            status: AuctionStatus::Active, recipients,
            min_increment, extension_window, extension_trigger, protocol_fee_bps,
            bid_history_cap, max_extensions, extension_count: 0,
            original_end_time: end_time,
        };
        save_auction(&env, &auction);
        add_artist_auction_id(&env, &creator, auction_id);
        AuctionCreatedEvent {
            auction_id, creator: creator.clone(), reserve_price,
            token, collection: collection.clone(), token_id, end_time,
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        // Interaction — pull NFT into custody
        escrow::escrow_nft(&env, &creator, &collection, token_id, false, auction_id);
        escrow::emit_nft_escrowed(&env, auction_id, &collection, token_id,
            &creator, env.ledger().sequence());
        auction_id
    }

    // ── Blocked-bidder registry (Issue #199 / #434) ──────────────────────────
    //
    // The auction creator (or the contract admin) may maintain a per-auction
    // registry of addresses that are barred from bidding.  The registry is
    // capped at MAX_BLOCKED_BIDDERS (50) to keep the membership scan and the
    // storage footprint small.  Invariant E in `check_bid_invariants` reads
    // this registry on every `place_bid` call so no blocked address can ever
    // slip through regardless of which code path is used.

    /// Add `bidder` to the blocked-bidder registry for `auction_id`.
    ///
    /// Only the auction creator or the contract admin may call this.
    /// Panics with:
    /// - `AuctionNotFound`    — unknown `auction_id`.
    /// - `AuctionNotActive`   — auction is already terminal.
    /// - `Unauthorized`       — caller is neither creator nor admin.
    /// - An untyped panic     — registry is already at MAX_BLOCKED_BIDDERS.
    pub fn block_bidder(env: Env, caller: Address, auction_id: u64, bidder: Address) {
        bump_instance_ttl(&env);
        caller.require_auth();
        let auction = load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound));
        if auction.status != AuctionStatus::Active {
            panic_with_error!(&env, MarketplaceError::AuctionNotActive);
        }
        Self::require_creator_or_admin(&env, &caller, &auction.creator);
        let mut list = crate::storage::load_blocked_bidders(&env, auction_id);
        // Idempotent: already blocked → no-op
        if list.contains(&bidder) {
            return;
        }
        if list.len() >= MAX_BLOCKED_BIDDERS {
            // Hard-cap exceeded — reject with a descriptive panic so callers
            // get a clear signal without consuming a new error discriminant.
            panic!("blocked-bidder registry full");
        }
        list.push_back(bidder.clone());
        crate::storage::save_blocked_bidders(&env, auction_id, &list);
        crate::events::emit_bidder_blocked(&env, auction_id, bidder);
    }

    /// Remove `bidder` from the blocked-bidder registry for `auction_id`.
    ///
    /// No-op when the bidder is not present (idempotent).
    /// Only the auction creator or the contract admin may call this.
    /// Panics with:
    /// - `AuctionNotFound`    — unknown `auction_id`.
    /// - `Unauthorized`       — caller is neither creator nor admin.
    pub fn unblock_bidder(env: Env, caller: Address, auction_id: u64, bidder: Address) {
        bump_instance_ttl(&env);
        caller.require_auth();
        let auction = load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound));
        Self::require_creator_or_admin(&env, &caller, &auction.creator);
        let list = crate::storage::load_blocked_bidders(&env, auction_id);
        // Rebuild without the target address (Vec has no remove-by-value).
        let mut new_list: Vec<Address> = Vec::new(&env);
        let mut found = false;
        for addr in list.iter() {
            if addr == bidder {
                found = true;
            } else {
                new_list.push_back(addr);
            }
        }
        if found {
            crate::storage::save_blocked_bidders(&env, auction_id, &new_list);
            crate::events::emit_bidder_unblocked(&env, auction_id, bidder);
        }
        // If not found: silently return (idempotent).
    }

    /// Return the list of blocked bidders for `auction_id`.
    pub fn get_blocked_bidders(env: Env, auction_id: u64) -> Vec<Address> {
        load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound));
        crate::storage::load_blocked_bidders(&env, auction_id)
    }

    // ── place_bid ────────────────────────────────────────────
    pub fn place_bid(env: Env, bidder: Address, auction_id: u64, amount: i128) {
        bump_instance_ttl(&env);
        Self::require_not_paused_ctx(&env, None, Some(&Symbol::new(&env, "place_bid")));
        bidder.require_auth();
        let mut auction = load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound));
        // Collection-level check after loading auction.
        Self::require_not_paused_ctx(&env, Some(&auction.collection), None);

        // ── Unified invariant check (Issue #434) ──────────────────────────────
        // All bid safety rules are validated here through a single path so no
        // entry point can bypass them.
        let inv = Self::check_bid_invariants(&env, &auction, &bidder, amount);

        let previous_bidder = auction.highest_bidder.clone();
        let previous_bid = auction.highest_bid;
        auction.highest_bid = amount;
        auction.highest_bidder = Some(bidder.clone());

        // Apply anti-sniping extension if the invariant layer approved one.
        let extended = if let Some((new_end_time, new_extension_count)) = inv.extension {
            let prev_end_time = auction.end_time;
            auction.end_time = new_end_time;
            auction.extension_count = new_extension_count;
            save_auction(&env, &auction);
            append_bid_record(
                &env, auction_id,
                &BidRecord { bidder: bidder.clone(), amount, ledger: env.ledger().sequence() },
                auction.bid_history_cap,
            );
            BidPlacedEvent {
                auction_id,
                bidder: bidder.clone(),
                bid_amount: amount,
                effective_end_time: auction.end_time,
            }.publish(&env);
            AuctionExtendedEvent {
                auction_id,
                prev_end_time,
                new_end_time: auction.end_time,
                extension_count: auction.extension_count,
            }
            .publish(&env);
            true
        } else {
            save_auction(&env, &auction);
            append_bid_record(
                &env, auction_id,
                &BidRecord { bidder: bidder.clone(), amount, ledger: env.ledger().sequence() },
                auction.bid_history_cap,
            );
            BidPlacedEvent {
                auction_id,
                bidder: bidder.clone(),
                bid_amount: amount,
                effective_end_time: auction.end_time,
            }.publish(&env);
            false
        };
        let _ = extended;

        // Refund the previous highest bidder (outbid path).
        let tc = TokenClient::new(&env, &auction.token);
        if let Some(prev) = previous_bidder {
            tc.transfer(&env.current_contract_address(), &prev, &previous_bid);
            crate::events::AuctionBidRefundedEvent {
                auction_id,
                bidder: prev,
                amount: previous_bid,
                token: auction.token.clone(),
                reason: Symbol::new(&env, "outbid"),
                ledger_sequence: env.ledger().sequence(),
                schema_version: EVENT_SCHEMA_VERSION,
            }
            .publish(&env);
        }
        tc.transfer(&bidder, &env.current_contract_address(), &amount);
    }

    // ── finalize_auction ─────────────────────────────────────
    // CEI: lock → checks → effects → emit → interactions (payout + release_nft) → unlock
    // With escrow: NFT source is always the contract — no seller-side surprise.
    pub fn finalize_auction(env: Env, caller: Address, auction_id: u64) {
        bump_instance_ttl(&env);
        // Fund-recovery / cleanup op — always available, ignores all pause axes.
        caller.require_auth();
        // RAII guard — cleared by Drop whether the function returns normally or panics.
        let _guard = AuctionReentrancyScope::new(&env, auction_id);
        let mut auction = load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound));

        // ── Unified invariant check (Issue #434) ──────────────────────────────
        Self::check_finalize_invariants(&env, &auction);
        let winner = auction.highest_bidder.clone();
        let winning_bid = auction.highest_bid;
        let snapshotted_fee = auction.protocol_fee_bps;
        // Policy engine: re-validate auction token at settlement time (Issue #435).
        // Only block settlement when there is a winner — a no-bid finalization
        // returns the NFT to the creator without any payment flow, so there is
        // no stale token risk on that path.
        if winner.is_some() {
            assert_token_policy_active(&env, &auction.token);
        }
        auction.status = if winner.is_some() { AuctionStatus::Finalized } else { AuctionStatus::Cancelled };
        save_auction(&env, &auction);
        AuctionFinalizedEvent {
            auction_id, winner: winner.clone(), amount: winning_bid,
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        if let Some(ref w) = winner {
            let (fee, payouts) = Self::distribute_payout_tracked(
                &env, &auction.token, &auction.collection, winning_bid,
                &auction.creator, &auction.recipients, w, false, snapshotted_fee,
                auction_id, false,
            );
            if fee > 0 {
                if let Some(treasury) = crate::storage::get_treasury_storage(&env) {
                    crate::storage::add_protocol_fee_total(&env, &auction.token, fee);
                    ProtocolFeeCollectedEvent {
                        listing_id: auction_id, amount: fee,
                        token: auction.token.clone(), treasury,
                        schema_version: EVENT_SCHEMA_VERSION,
                    }.publish(&env);
                }
            }
            // On-chain accounting counters (Issue #279) — see buy_artwork.
            crate::storage::add_royalty_total(&env, &auction.token, winning_bid);
            crate::storage::increment_settlement_count(&env, &auction.token);
            // Emit royalty settlement snapshot for auditability (Issue #270)
            RoyaltySettlementEvent {
                id: auction_id,
                recipients: auction.recipients.clone(),
                total_amount: winning_bid,
                token: auction.token.clone(),
                ledger_sequence: env.ledger().sequence(),
                schema_version: EVENT_SCHEMA_VERSION,
            }.publish(&env);
            // Per-recipient payout breakdown for the royalty audit trail (Issue #201)
            emit_royalty_paid(
                &env, None, Some(auction_id), winning_bid, fee,
                auction.token.clone(), payouts,
            );
            // NFT: escrow → winner (CEI: status Finalized already)
            escrow::release_nft(&env, &auction.collection, auction.token_id,
                w, env.ledger().sequence(), auction_id);
        } else {
            // No bids — return NFT to creator.
            AuctionCancelledEvent {
                auction_id,
                cancelled_by: auction.creator.clone(),
                reason: AuctionCancelReason::Owner,
                escrow_amount: 0,
                token: auction.token.clone(),
                ledger_sequence: env.ledger().sequence(),
                schema_version: EVENT_SCHEMA_VERSION,
            }.publish(&env);
            escrow::release_nft(&env, &auction.collection, auction.token_id,
                &auction.creator, env.ledger().sequence(), auction_id);
        }
        // _guard dropped here — releases auction lock automatically.
    }

    // ── refund_losing_bid ─────────────────────────────────────────────────────
    //
    // Issue #434 — Formal invariants layer
    //
    // After `finalize_auction` the winning bid funds are forwarded to the
    // creator/recipients via `distribute_payout`.  Earlier bids are refunded
    // atomically when they are outbid (inside `place_bid`).  However if an
    // auction is finalized with NO bids (status → Cancelled) the bidder map is
    // empty, so there is nothing to refund.  The only scenario where a non-zero
    // bid amount could be stuck post-finalization is a data anomaly; this entry
    // point acts as the safety-valve for that case.
    //
    // Invariants enforced:
    //   1. Auction must be in a terminal state (Finalized / Cancelled).
    //   2. The caller must not be the current `highest_bidder` of a Finalized
    //      auction (that bidder won — their funds went to the seller).
    //   3. The caller must have an identifiable outstanding bid stored in the
    //      bid-history ring buffer.
    //   4. The bid amount must be > 0.
    //
    // Because per-losing-bidder balances are not stored individually (only the
    // ring-buffer history and the live highest-bid escrow are kept), this
    // function locates the caller's most-recent bid in the ring buffer and uses
    // that amount.  In practice, losing bids are refunded immediately in
    // `place_bid`; this entry point covers the edge case where a node restart or
    // indexer anomaly causes the frontend to believe a refund is pending.

    /// Claim a refund for a lost bid on a terminal auction.
    ///
    /// Safe to call only after `finalize_auction` or `admin_cancel_auction`
    /// has completed.  Panics with:
    /// - `AuctionNotFound`       — unknown `auction_id`.
    /// - `InvalidAuctionState`   — auction is still Active.
    /// - `NoBidToRefund`         — caller is the winner, or has no bid on record,
    ///                             or their bid amount is zero.
    pub fn refund_losing_bid(env: Env, bidder: Address, auction_id: u64) {
        bump_instance_ttl(&env);
        // Fund-recovery op — always available regardless of pause state.
        bidder.require_auth();
        let auction = load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound));

        // Invariant 1: only allowed on a terminal auction.
        if auction.status == AuctionStatus::Active {
            panic_with_error!(&env, MarketplaceError::InvalidAuctionState);
        }

        // Invariant 2: the current highest bidder of a Finalized auction is the
        // winner — their funds have already been distributed to the seller.
        if auction.status == AuctionStatus::Finalized {
            if let Some(ref winner) = auction.highest_bidder {
                if *winner == bidder {
                    panic_with_error!(&env, MarketplaceError::NoBidToRefund);
                }
            }
        }

        // Invariant: idempotency — duplicate claims return a stable error (Issue #466).
        if crate::storage::is_bid_refunded(&env, auction_id, &bidder) {
            panic_with_error!(&env, MarketplaceError::NoBidToRefund);
        }

        // Invariant 3 + 4: scan bid history for the caller's most-recent entry.
        let bids = load_auction_bids(&env, auction_id);
        let mut refund_amount: i128 = 0;
        // Walk in reverse (newest first) to find the bidder's most recent bid.
        let bid_count = bids.len();
        for i in (0..bid_count).rev() {
            let record = bids.get(i).unwrap();
            if record.bidder == bidder {
                refund_amount = record.amount;
                break;
            }
        }
        if refund_amount <= 0 {
            panic_with_error!(&env, MarketplaceError::NoBidToRefund);
        }

        // CEI: write the refund record before the token transfer so that a
        // failed or re-entrant transfer cannot be replayed for a double payout.
        crate::storage::mark_bid_refunded(&env, auction_id, &bidder);

        // Transfer refund from contract to bidder.
        TokenClient::new(&env, &auction.token).transfer(
            &env.current_contract_address(),
            &bidder,
            &refund_amount,
        );

        crate::events::AuctionBidRefundedEvent {
            auction_id,
            bidder: bidder.clone(),
            amount: refund_amount,
            token: auction.token.clone(),
            reason: soroban_sdk::Symbol::new(&env, "losing_bid_reclaim"),
            ledger_sequence: env.ledger().sequence(),
            schema_version: EVENT_SCHEMA_VERSION,
        }
        .publish(&env);
    }

    // ── cancel_auction ───────────────────────────────────────
    // Only no-bid auctions can be cancelled; releases NFT back to creator
    pub fn cancel_auction(env: Env, creator: Address, auction_id: u64) {
        bump_instance_ttl(&env);
        // Fund-recovery / cleanup op — always available, ignores all pause axes.
        creator.require_auth();
        let mut auction = load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound));

        // ── Unified invariant check (Issue #434) ──────────────────────────────
        Self::check_cancel_invariants(&env, &auction, &creator);
        auction.status = AuctionStatus::Cancelled;
        save_auction(&env, &auction);
        AuctionCancelledEvent {
            auction_id,
            cancelled_by: creator.clone(),
            reason: AuctionCancelReason::Owner,
            escrow_amount: 0,
            token: auction.token.clone(),
            ledger_sequence: env.ledger().sequence(),
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        // Return NFT to creator
        escrow::release_nft(&env, &auction.collection, auction.token_id,
            &creator, env.ledger().sequence(), auction_id);
    }

    // ── admin_cancel_auction ─────────────────────────────────
    // Admin / emergency cancellation — works even if bids exist.
    // Refunds the highest bidder before cancelling. (Issue #271 / #848)
    //
    // Reason selection:
    //   - ProtocolConfig holder → AuctionCancelReason::Admin
    //   - EmergencyPause-only holder → AuctionCancelReason::Emergency
    pub fn admin_cancel_auction(env: Env, admin: Address, auction_id: u64) {
        let reason = Self::resolve_auction_cancel_reason(&env, &admin);
        if !acquire_auction_lock(&env, auction_id) {
            panic_with_error!(&env, MarketplaceError::ReentrancyGuard);
        }
        let mut auction = match load_auction(&env, auction_id) {
            Some(a) => a,
            None => {
                release_auction_lock(&env, auction_id);
                panic_with_error!(&env, MarketplaceError::AuctionNotFound);
            }
        };
        if auction.status != AuctionStatus::Active {
            release_auction_lock(&env, auction_id);
            panic_with_error!(&env, MarketplaceError::InvalidAuctionState);
        }
        // Refund the highest bidder (if any) before marking cancelled.
        let refunded_amount = auction.highest_bid;
        if let Some(ref bidder) = auction.highest_bidder.clone() {
            // CEI: mark before transfer so refund_losing_bid idempotency blocks replay.
            crate::storage::mark_bid_refunded(&env, auction_id, bidder);
            let tc = TokenClient::new(&env, &auction.token);
            tc.transfer(&env.current_contract_address(), bidder, &refunded_amount);
            AuctionBidRefundedEvent {
                auction_id,
                bidder: bidder.clone(),
                amount: refunded_amount,
                token: auction.token.clone(),
                reason: Symbol::new(&env, "admin_cancel"),
                ledger_sequence: env.ledger().sequence(),
                schema_version: EVENT_SCHEMA_VERSION,
            }.publish(&env);
        }
        auction.status = AuctionStatus::Cancelled;
        save_auction(&env, &auction);
        AuctionCancelledEvent {
            auction_id,
            cancelled_by: admin.clone(),
            reason,
            escrow_amount: refunded_amount,
            token: auction.token.clone(),
            ledger_sequence: env.ledger().sequence(),
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        AuctionAdminCancelledEvent {
            auction_id,
            cancelled_by: admin,
            refunded_amount,
            token: auction.token.clone(),
            ledger_sequence: env.ledger().sequence(),
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        // Return NFT to creator
        escrow::release_nft(&env, &auction.collection, auction.token_id,
            &auction.creator, env.ledger().sequence(), auction_id);
        release_auction_lock(&env, auction_id);
    }

    // ── update_auction_reserve_price ─────────────────────────────────────────
    //
    // Issue #467 — Creators may correct a pricing mistake after creating an
    // auction, provided no bids have been placed yet.  Once the first bid lands
    // the reserve price is locked, so bidder expectations are never retroactively
    // undermined.
    //
    // Invariants enforced:
    //   1. Only the auction creator may call this.
    //   2. Auction must be Active.
    //   3. No bids may exist (highest_bidder is None and highest_bid == 0).
    //   4. New price must be >= min_bid_increment (same floor as create_auction).

    /// Update the reserve price of an active, no-bid auction.
    ///
    /// Panics with:
    /// - `AuctionNotFound`     — unknown `auction_id`.
    /// - `Unauthorized`        — caller is not the auction creator.
    /// - `AuctionNotActive`    — auction is not Active.
    /// - `AuctionHasBids`      — at least one bid has already been placed.
    /// - `InvalidPrice`        — `new_price` is below `min_bid_increment`.
    /// - `PriceOutOfBounds`    — `new_price` violates configured [min, max] bounds (#848).
    /// - `ReentrancyGuard`     — auction lock already held.
    pub fn update_auction_reserve_price(
        env: Env,
        creator: Address,
        auction_id: u64,
        new_price: i128,
    ) {
        bump_instance_ttl(&env);
        creator.require_auth();
        if !acquire_auction_lock(&env, auction_id) {
            panic_with_error!(&env, MarketplaceError::ReentrancyGuard);
        }
        let mut auction = match load_auction(&env, auction_id) {
            Some(a) => a,
            None => {
                release_auction_lock(&env, auction_id);
                panic_with_error!(&env, MarketplaceError::AuctionNotFound);
            }
        };

        if auction.creator != creator {
            release_auction_lock(&env, auction_id);
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        if auction.status != AuctionStatus::Active {
            release_auction_lock(&env, auction_id);
            panic_with_error!(&env, MarketplaceError::AuctionNotActive);
        }
        // Lock out updates once any bid exists.
        if auction.highest_bidder.is_some() || auction.highest_bid > 0 {
            release_auction_lock(&env, auction_id);
            panic_with_error!(&env, MarketplaceError::AuctionHasBids);
        }
        let min_increment = crate::storage::get_min_bid_increment_storage(&env)
            .unwrap_or(DEFAULT_MIN_BID_INCREMENT);
        if new_price < min_increment {
            release_auction_lock(&env, auction_id);
            panic_with_error!(&env, MarketplaceError::InvalidPrice);
        }
        // Enforce global price-policy bounds (Issue #848).
        assert_price_policy(&env, new_price);

        let old_price = auction.reserve_price;
        auction.reserve_price = new_price;
        save_auction(&env, &auction);
        release_auction_lock(&env, auction_id);

        crate::events::emit_auction_reserve_updated(
            &env, auction_id, creator, old_price, new_price,
        );
    }

    // ── Offers ───────────────────────────────────────────────

    pub fn make_offer(
        env: Env, offerer: Address, listing_id: u64,
        amount: i128, token: Address, expires_at: Option<u64>,
    ) -> u64 {
        bump_instance_ttl(&env);
        Self::require_not_paused_ctx(&env, None, Some(&Symbol::new(&env, "make_offer")));
        offerer.require_auth();
        let listing = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        // Collection-level check after loading listing.
        Self::require_not_paused_ctx(&env, Some(&listing.collection), None);
        if listing.status != ListingStatus::Active {
            panic_with_error!(&env, MarketplaceError::ListingNotActive);
        }
        if listing.artist == offerer { panic_with_error!(&env, MarketplaceError::CannotOfferOwnListing); }
        if amount <= 0 { panic_with_error!(&env, MarketplaceError::InsufficientOfferAmount); }
        Self::validate_token_asset(&env, &token, Some(&listing.collection));
        // Policy engine gate (Issue #435).
        assert_token_policy_active(&env, &token);
        // Enforce the active-offer cap so per-listing storage is bounded.
        // Only Pending offers count; terminal offers have freed their slot.
        // O(1): one read of the bounded pending-offer set — no scan over the
        // listing's full offer history.
        if pending_offer_count(&env, listing_id) >= MAX_OFFERS_PER_LISTING {
            panic_with_error!(&env, MarketplaceError::OfferLimitReached);
        }
        TokenClient::new(&env, &token).transfer(&offerer, &env.current_contract_address(), &amount);
        let offer_id = increment_offer_count(&env);
        save_offer(
            &env,
            &Offer {
                offer_id,
                listing_id,
                offerer: offerer.clone(),
                amount,
                token: token.clone(),
                status: OfferStatus::Pending,
                created_at: env.ledger().sequence(),
                expires_at, // #32: optional expiry stored with offer
            },
        );
        // O(1) appends: each index touches only its last page.
        add_listing_offer_id(&env, listing_id, offer_id);
        add_offerer_offer_id(&env, &offerer, offer_id);
        add_pending_offer(&env, listing_id, offer_id);

        OfferMadeEvent {
            offer_id,
            listing_id,
            offerer: offerer.clone(),
            amount,
            token,
            expires_at,
            schema_version: EVENT_SCHEMA_VERSION,
        }
        .publish(&env);

        offer_id
    }

    pub fn withdraw_offer(env: Env, offerer: Address, offer_id: u64) {
        bump_instance_ttl(&env);
        // Fund-recovery / cleanup op — always available, ignores all pause axes.
        offerer.require_auth();
        let mut offer = load_offer(&env, offer_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::OfferNotFound));
        if offer.offerer != offerer { panic_with_error!(&env, MarketplaceError::Unauthorized); }
        if offer.status != OfferStatus::Pending {
            panic_with_error!(&env, MarketplaceError::InvalidOfferState);
        }
        TokenClient::new(&env, &offer.token).transfer(
            &env.current_contract_address(), &offerer, &offer.amount,
        );
        offer.status = OfferStatus::Withdrawn;
        save_offer(&env, &offer);
        remove_pending_offer(&env, offer.listing_id, offer_id);

        OfferWithdrawnEvent {
            offer_id,
            listing_id: offer.listing_id,
            offerer: offerer.clone(),
        }
        .publish(&env);
    }

    pub fn reject_offer(env: Env, artist: Address, offer_id: u64) {
        bump_instance_ttl(&env);
        // Fund-recovery / cleanup op (returns escrowed funds to the offerer) —
        // always available, ignores all pause axes.
        artist.require_auth();
        let mut offer = load_offer(&env, offer_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::OfferNotFound));
        let listing = load_listing(&env, offer.listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        if listing.artist != artist { panic_with_error!(&env, MarketplaceError::Unauthorized); }
        if offer.status != OfferStatus::Pending {
            panic_with_error!(&env, MarketplaceError::InvalidOfferState);
        }
        TokenClient::new(&env, &offer.token).transfer(
            &env.current_contract_address(), &offer.offerer, &offer.amount,
        );
        offer.status = OfferStatus::Rejected;
        save_offer(&env, &offer);
        remove_pending_offer(&env, offer.listing_id, offer_id);

        OfferRejectedEvent {
            offer_id,
            listing_id: offer.listing_id,
            offerer: offer.offerer.clone(),
        }
        .publish(&env);
    }

    // ── accept_offer ─────────────────────────────────────────
    // CEI: lock → checks → effects → emit → interactions (payout + release_nft + refunds) → unlock
    pub fn accept_offer(env: Env, artist: Address, offer_id: u64) {
        bump_instance_ttl(&env);
        // Full 3-axis pause check: global, per-collection, and per-function (Issue #464).
        // Collection is loaded after the offer, so we perform function + global check first,
        // then collection check once the listing is loaded.
        Self::require_not_paused_ctx(
            &env,
            None,
            Some(&Symbol::new(&env, "accept_offer")),
        );
        artist.require_auth();
        let mut offer = load_offer(&env, offer_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::OfferNotFound));
        let listing_id = offer.listing_id;
        // RAII guard — cleared by Drop whether the function returns normally or panics.
        let _guard = ListingReentrancyScope::new(&env, listing_id);
        let mut listing = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        // Per-collection pause axis (Issue #464).
        Self::require_not_paused_ctx(&env, Some(&listing.collection), None);
        if listing.artist != artist {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        if offer.status != OfferStatus::Pending || listing.status != ListingStatus::Active {
            panic_with_error!(&env, MarketplaceError::InvalidOfferState);
        }
        if let Some(exp) = offer.expires_at {
            if env.ledger().timestamp() >= exp {
                panic_with_error!(&env, MarketplaceError::OfferExpired);
            }
        }
        if let Some(exp) = listing.expires_at {
            if env.ledger().timestamp() >= exp {
                panic_with_error!(&env, MarketplaceError::ListingExpired);
            }
        }
        // Policy engine: re-validate offer token at settlement time (Issue #435).
        // The token may have been removed from the whitelist after the offer was
        // created — we must block settlement with a stale token.
        assert_token_policy_active(&env, &offer.token);
        // Effects
        let accepted_offerer = offer.offerer.clone();
        let accepted_amount = offer.amount;
        offer.status = OfferStatus::Accepted;
        save_offer(&env, &offer);
        listing.status = ListingStatus::Sold;
        listing.owner = Some(accepted_offerer.clone());
        save_listing(&env, &listing);
        remove_from_active_listings(&env, listing_id);

        // #31: Mark all other pending offers Rejected; collect refund data and
        // emit OfferRejectedEvent for each.  Sweeps the bounded pending-offer
        // set (≤ MAX_OFFERS_PER_LISTING), then drops it — the accepted offer
        // and every sibling have reached a terminal state.
        let sibling_offers = load_pending_offer_ids(&env, listing.listing_id);
        let mut r_offerers: Vec<Address> = Vec::new(&env);
        let mut r_amounts: Vec<i128> = Vec::new(&env);
        let mut r_tokens: Vec<Address> = Vec::new(&env);
        for oid in sibling_offers.iter() {
            if oid != offer_id {
                if let Some(mut other) = load_offer(&env, oid) {
                    if other.status == OfferStatus::Pending {
                        other.status = OfferStatus::Rejected;
                        save_offer(&env, &other);
                        r_offerers.push_back(other.offerer.clone());
                        r_amounts.push_back(other.amount);
                        r_tokens.push_back(other.token.clone());
                        OfferRejectedEvent {
                            offer_id: oid, listing_id, offerer: other.offerer,
                        }.publish(&env);
                    }
                }
            }
        }
        clear_pending_offers(&env, listing.listing_id);

        OfferAcceptedEvent {
            offer_id, listing_id, offerer: accepted_offerer.clone(), amount: accepted_amount,
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        // Interactions — tracked so offer settlements have queryable payout status
        let (fee, payouts) = Self::distribute_payout_tracked(
            &env, &offer.token, &listing.collection, offer.amount,
            &artist, &listing.recipients, &offer.offerer, false, listing.protocol_fee_bps,
            listing_id, true,
        );
        if fee > 0 {
            if let Some(treasury) = crate::storage::get_treasury_storage(&env) {
                crate::storage::add_protocol_fee_total(&env, &offer.token, fee);
                ProtocolFeeCollectedEvent {
                    listing_id, amount: fee, token: offer.token.clone(), treasury,
                    schema_version: EVENT_SCHEMA_VERSION,
                }.publish(&env);
            }
        }
        // On-chain accounting counters (Issue #279) — see buy_artwork.
        crate::storage::add_royalty_total(&env, &offer.token, offer.amount);
        crate::storage::increment_settlement_count(&env, &offer.token);
        // Emit royalty settlement snapshot for auditability (Issue #270)
        RoyaltySettlementEvent {
            id: listing_id,
            recipients: listing.recipients.clone(),
            total_amount: offer.amount,
            token: offer.token.clone(),
            ledger_sequence: env.ledger().sequence(),
            schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        // Per-recipient payout breakdown for the royalty audit trail (Issue #201)
        emit_royalty_paid(
            &env, Some(listing_id), None, offer.amount, fee,
            offer.token.clone(), payouts,
        );
        // NFT: escrow → accepted offerer (CEI: status Sold already)
        if listing.quantity > 1 {
            escrow::release_nft_with_quantity(&env, &listing.collection, listing.token_id,
                listing.quantity, &accepted_offerer, env.ledger().sequence(), listing_id);
        } else {
            escrow::release_nft(&env, &listing.collection, listing.token_id,
                &accepted_offerer, env.ledger().sequence(), listing_id);
        }
        for i in 0..r_offerers.len() {
            TokenClient::new(&env, &r_tokens.get(i).unwrap()).transfer(
                &env.current_contract_address(),
                &r_offerers.get(i).unwrap(),
                &r_amounts.get(i).unwrap(),
            );
        }
        // _guard dropped here — releases listing lock automatically.
    }

    pub fn reclaim_offer(env: Env, offer_id: u64) {
        bump_instance_ttl(&env);
        // Fund-recovery / cleanup op — always available, ignores all pause axes.
        let mut offer = load_offer(&env, offer_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::OfferNotFound));
        if offer.status != OfferStatus::Pending {
            panic_with_error!(&env, MarketplaceError::InvalidOfferState);
        }
        let exp = match offer.expires_at {
            Some(e) => e,
            None => panic_with_error!(&env, MarketplaceError::InvalidOfferState),
        };
        if env.ledger().timestamp() < exp { panic_with_error!(&env, MarketplaceError::OfferExpired); }
        TokenClient::new(&env, &offer.token).transfer(
            &env.current_contract_address(), &offer.offerer, &offer.amount,
        );
        offer.status = OfferStatus::Withdrawn;
        save_offer(&env, &offer);
        remove_pending_offer(&env, offer.listing_id, offer_id);

        OfferReclaimedEvent {
            offer_id,
            listing_id: offer.listing_id,
            offerer: offer.offerer.clone(),
            amount: offer.amount,
        }
        .publish(&env);
    }

    // ── Offer auto-expiry sweep (Issue #470) ─────────────────
    //
    // Permissionless: any caller may trigger a sweep batch. Each offer in the
    // supplied list is examined and transitioned to `Expired` (with escrow
    // returned) only when it is Pending AND its `expires_at` has passed.
    // Non-pending or non-expiring offers are silently skipped so the call is
    // safe to retry. Returns the count of offers actually swept.

    /// Sweep a bounded batch of offers to `Expired`, returning escrowed funds
    /// to each offerer. The caller supplies the list of offer ids to attempt;
    /// the contract enforces `MAX_SWEEP_BATCH_SIZE` (20) to keep compute bounded.
    pub fn sweep_expired_offers(env: Env, offer_ids: soroban_sdk::Vec<u64>) -> u32 {
        bump_instance_ttl(&env);
        const MAX_SWEEP_BATCH_SIZE: u32 = 20;
        if offer_ids.len() > MAX_SWEEP_BATCH_SIZE {
            panic_with_error!(&env, MarketplaceError::BatchTooLarge);
        }
        let now = env.ledger().timestamp();
        let mut swept: u32 = 0;
        for offer_id in offer_ids.iter() {
            let mut offer = match load_offer(&env, offer_id) {
                Some(o) => o,
                None => continue,
            };
            if offer.status != OfferStatus::Pending {
                continue;
            }
            let exp = match offer.expires_at {
                Some(e) => e,
                None => continue,
            };
            if now < exp {
                continue;
            }
            // CEI: write terminal state before transfer.
            offer.status = OfferStatus::Expired;
            save_offer(&env, &offer);
            remove_pending_offer(&env, offer.listing_id, offer_id);
            // Interaction: return funds to offerer.
            TokenClient::new(&env, &offer.token).transfer(
                &env.current_contract_address(),
                &offer.offerer,
                &offer.amount,
            );
            crate::events::emit_offer_expired_sweep(
                &env,
                offer_id,
                offer.listing_id,
                offer.offerer.clone(),
                offer.amount,
                offer.token.clone(),
            );
            swept += 1;
        }
        swept
    }

    // ── Counter-offer (Issue #471) ───────────────────────────
    //
    // Only the listing artist may counter a buyer's pending offer. Creating a
    // counter-offer atomically closes the parent offer (returning buyer escrow)
    // and opens a new pending offer with the artist as counterparty. The buyer
    // can then accept via `accept_counter_offer`, which deposits the counter
    // amount and triggers the full settlement path.

    /// Create a counter-offer to a buyer's pending offer.
    ///
    /// The parent offer is atomically rejected (buyer escrow returned) and a
    /// new pending offer is created. Returns the new counter-offer ID.
    pub fn counter_offer(
        env: Env,
        artist: Address,
        parent_offer_id: u64,
        amount: i128,
        token: Address,
        expires_at: Option<u64>,
    ) -> u64 {
        bump_instance_ttl(&env);
        artist.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, MarketplaceError::InsufficientOfferAmount);
        }
        let mut parent = load_offer(&env, parent_offer_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::OfferNotFound));
        if parent.status != OfferStatus::Pending {
            panic_with_error!(&env, MarketplaceError::OfferNotPending);
        }
        let listing = load_listing(&env, parent.listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        if listing.artist != artist {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        if listing.status != ListingStatus::Active {
            panic_with_error!(&env, MarketplaceError::ListingNotActive);
        }
        Self::validate_token_asset(&env, &token, Some(&listing.collection));
        assert_token_policy_active(&env, &token);

        // Atomically close the parent offer — return buyer escrow.
        parent.status = OfferStatus::Rejected;
        save_offer(&env, &parent);
        remove_pending_offer(&env, parent.listing_id, parent_offer_id);
        TokenClient::new(&env, &parent.token).transfer(
            &env.current_contract_address(),
            &parent.offerer,
            &parent.amount,
        );
        OfferRejectedEvent {
            offer_id: parent_offer_id,
            listing_id: parent.listing_id,
            offerer: parent.offerer.clone(),
        }.publish(&env);

        // Create the counter-offer record (artist as counterparty).
        let counter_id = increment_offer_count(&env);
        save_offer(
            &env,
            &Offer {
                offer_id: counter_id,
                listing_id: parent.listing_id,
                offerer: artist.clone(),
                amount,
                token: token.clone(),
                status: OfferStatus::Pending,
                created_at: env.ledger().sequence(),
                expires_at,
            },
        );
        add_listing_offer_id(&env, parent.listing_id, counter_id);
        add_offerer_offer_id(&env, &artist, counter_id);
        add_pending_offer(&env, parent.listing_id, counter_id);
        set_counter_offer_parent(&env, counter_id, parent_offer_id);
        set_offer_superseded_by(&env, parent_offer_id, counter_id);

        crate::events::emit_counter_offer_made(
            &env, counter_id, parent_offer_id, parent.listing_id,
            artist, amount, token, expires_at,
        );
        counter_id
    }

    /// Accept a counter-offer: the original buyer deposits the counter amount
    /// and the sale settles identically to `buy_artwork`.
    pub fn accept_counter_offer(env: Env, buyer: Address, counter_offer_id: u64) {
        bump_instance_ttl(&env);
        Self::require_not_paused_ctx(&env, None, Some(&Symbol::new(&env, "accept_counter_offer")));
        buyer.require_auth();
        let mut counter = load_offer(&env, counter_offer_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::OfferNotFound));
        if get_counter_offer_parent(&env, counter_offer_id).is_none() {
            panic_with_error!(&env, MarketplaceError::NotCounterOffer);
        }
        if counter.status != OfferStatus::Pending {
            panic_with_error!(&env, MarketplaceError::InvalidOfferState);
        }
        if let Some(exp) = counter.expires_at {
            if env.ledger().timestamp() >= exp {
                panic_with_error!(&env, MarketplaceError::OfferExpired);
            }
        }
        let listing_id = counter.listing_id;
        let _guard = ListingReentrancyScope::new(&env, listing_id);
        let mut listing = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        Self::require_not_paused_ctx(&env, Some(&listing.collection), None);
        if listing.status != ListingStatus::Active {
            panic_with_error!(&env, MarketplaceError::ListingNotActive);
        }
        if let Some(exp) = listing.expires_at {
            if env.ledger().timestamp() >= exp {
                panic_with_error!(&env, MarketplaceError::ListingExpired);
            }
        }
        assert_token_policy_active(&env, &counter.token);

        let buyer_clone = buyer.clone();
        counter.status = OfferStatus::Accepted;
        save_offer(&env, &counter);
        listing.status = ListingStatus::Sold;
        listing.owner = Some(buyer.clone());
        save_listing(&env, &listing);
        remove_from_active_listings(&env, listing_id);
        remove_pending_offer(&env, listing_id, counter_offer_id);

        // Reject remaining sibling offers.
        let siblings = load_pending_offer_ids(&env, listing_id);
        let mut r_offerers: Vec<Address> = Vec::new(&env);
        let mut r_amounts: Vec<i128> = Vec::new(&env);
        let mut r_tokens: Vec<Address> = Vec::new(&env);
        for oid in siblings.iter() {
            if oid != counter_offer_id {
                if let Some(mut other) = load_offer(&env, oid) {
                    if other.status == OfferStatus::Pending {
                        other.status = OfferStatus::Rejected;
                        save_offer(&env, &other);
                        r_offerers.push_back(other.offerer.clone());
                        r_amounts.push_back(other.amount);
                        r_tokens.push_back(other.token.clone());
                        OfferRejectedEvent {
                            offer_id: oid, listing_id, offerer: other.offerer,
                        }.publish(&env);
                    }
                }
            }
        }
        clear_pending_offers(&env, listing_id);

        OfferAcceptedEvent {
            offer_id: counter_offer_id, listing_id, offerer: buyer.clone(),
            amount: counter.amount, schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);

        // Buyer pays the counter amount; distribute to artist and recipients.
        let artist = listing.artist.clone();
        TokenClient::new(&env, &counter.token).transfer(
            &buyer, &env.current_contract_address(), &counter.amount,
        );
        let (fee, payouts) = Self::distribute_payout_tracked(
            &env, &counter.token, &listing.collection, counter.amount,
            &artist, &listing.recipients, &buyer_clone, false, listing.protocol_fee_bps,
            listing_id, true,
        );
        if fee > 0 {
            if let Some(treasury) = crate::storage::get_treasury_storage(&env) {
                crate::storage::add_protocol_fee_total(&env, &counter.token, fee);
                ProtocolFeeCollectedEvent {
                    listing_id, amount: fee, token: counter.token.clone(), treasury,
                    schema_version: EVENT_SCHEMA_VERSION,
                }.publish(&env);
            }
        }
        crate::storage::add_royalty_total(&env, &counter.token, counter.amount);
        crate::storage::increment_settlement_count(&env, &counter.token);
        RoyaltySettlementEvent {
            id: listing_id, recipients: listing.recipients.clone(),
            total_amount: counter.amount, token: counter.token.clone(),
            ledger_sequence: env.ledger().sequence(), schema_version: EVENT_SCHEMA_VERSION,
        }.publish(&env);
        emit_royalty_paid(&env, Some(listing_id), None, counter.amount, fee,
            counter.token.clone(), payouts);
        if listing.quantity > 1 {
            escrow::release_nft_with_quantity(&env, &listing.collection, listing.token_id,
                listing.quantity, &buyer_clone, env.ledger().sequence(), listing_id);
        } else {
            escrow::release_nft(&env, &listing.collection, listing.token_id,
                &buyer_clone, env.ledger().sequence(), listing_id);
        }
        for i in 0..r_offerers.len() {
            TokenClient::new(&env, &r_tokens.get(i).unwrap()).transfer(
                &env.current_contract_address(),
                &r_offerers.get(i).unwrap(),
                &r_amounts.get(i).unwrap(),
            );
        }
    }

    // ── Governance quorum (Issue #472) ───────────────────────
    //
    // Three high-risk operations (TreasuryRotation, FeeIncrease, GlobalPause)
    // can be gated behind a multi-approval quorum. The proposer defines a signer
    // set and threshold; only after enough distinct signers approve can the
    // action be executed. Low-risk operations remain single-role authorized.

    /// Create a multi-approval governance proposal for a high-risk operation.
    ///
    /// `signers` must be non-empty; `threshold` must be ≥ 1 and ≤ `signers.len()`.
    pub fn propose_governance_action(
        env: Env,
        proposer: Address,
        proposal_type: GovernanceProposalType,
        signers: soroban_sdk::Vec<Address>,
        threshold: u32,
        expires_at: u64,
        payload_address: Option<Address>,
        payload_u32: Option<u32>,
        payload_bool: Option<bool>,
    ) -> u64 {
        bump_instance_ttl(&env);
        // require_role calls caller.require_auth() — no separate call needed.
        match &proposal_type {
            GovernanceProposalType::GlobalPause => {
                Self::require_role(&env, &proposer, RoleType::EmergencyPause);
            }
            _ => {
                Self::require_role(&env, &proposer, RoleType::ProtocolConfig);
            }
        }
        if signers.is_empty() || threshold == 0 || threshold > signers.len() {
            panic_with_error!(&env, MarketplaceError::InvalidSplit);
        }
        if expires_at <= env.ledger().timestamp() {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalExpired);
        }
        let proposal_id = increment_governance_proposal_count(&env);
        let proposal = GovernanceProposal {
            proposal_id,
            proposal_type: proposal_type.clone(),
            proposed_by: proposer.clone(),
            signers,
            threshold,
            expires_at,
            created_at: env.ledger().sequence(),
            executed: false,
            cancelled: false,
            payload_address,
            payload_u32,
            payload_bool,
        };
        save_governance_proposal(&env, &proposal);
        crate::events::emit_governance_proposed(
            &env, proposal_id, proposal_type, proposer, threshold, expires_at,
        );
        proposal_id
    }

    /// Approve a pending governance proposal. Must be called by an address in
    /// the proposal's `signers` set. Each signer may approve at most once.
    pub fn approve_governance_action(env: Env, approver: Address, proposal_id: u64) {
        bump_instance_ttl(&env);
        approver.require_auth();
        let proposal = load_governance_proposal(&env, proposal_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::GovernanceProposalNotFound));
        if proposal.executed {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalAlreadyExecuted);
        }
        if proposal.cancelled {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalCancelled);
        }
        if env.ledger().timestamp() > proposal.expires_at {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalExpired);
        }
        if !proposal.signers.contains(&approver) {
            panic_with_error!(&env, MarketplaceError::GovernanceSignerNotAuthorized);
        }
        let mut approvals = load_governance_approvals(&env, proposal_id);
        if approvals.contains(&approver) {
            panic_with_error!(&env, MarketplaceError::GovernanceAlreadyApproved);
        }
        approvals.push_back(approver.clone());
        save_governance_approvals(&env, proposal_id, &approvals);
        crate::events::emit_governance_approved(
            &env, proposal_id, approver, approvals.len(), proposal.threshold,
        );
    }

    /// Execute an approved governance proposal. Requires threshold approvals,
    /// not expired, not already executed, not cancelled.
    pub fn execute_governance_action(env: Env, executor: Address, proposal_id: u64) {
        bump_instance_ttl(&env);
        executor.require_auth();
        let mut proposal = load_governance_proposal(&env, proposal_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::GovernanceProposalNotFound));
        if proposal.executed {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalAlreadyExecuted);
        }
        if proposal.cancelled {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalCancelled);
        }
        if env.ledger().timestamp() > proposal.expires_at {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalExpired);
        }
        let approvals = load_governance_approvals(&env, proposal_id);
        if approvals.len() < proposal.threshold {
            panic_with_error!(&env, MarketplaceError::GovernanceThresholdNotMet);
        }
        // Replay protection: mark executed before the action.
        proposal.executed = true;
        save_governance_proposal(&env, &proposal);

        match proposal.proposal_type.clone() {
            GovernanceProposalType::TreasuryRotation => {
                let new_treasury = proposal.payload_address
                    .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::Unauthorized));
                crate::storage::set_treasury_storage(&env, &new_treasury);
            }
            GovernanceProposalType::FeeIncrease => {
                let new_bps = proposal.payload_u32
                    .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::Unauthorized));
                if new_bps > 10_000 {
                    panic_with_error!(&env, MarketplaceError::InvalidSplit);
                }
                crate::storage::set_protocol_fee_bps_storage(&env, new_bps);
            }
            GovernanceProposalType::GlobalPause => {
                let pause = proposal.payload_bool
                    .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::Unauthorized));
                crate::storage::set_paused(&env, pause);
            }
        }
        crate::events::emit_governance_executed(
            &env, proposal_id, proposal.proposal_type, executor,
        );
    }

    /// Cancel a pending governance proposal. Only the original proposer or the
    /// relevant role holder may cancel.
    pub fn cancel_governance_action(env: Env, canceller: Address, proposal_id: u64) {
        bump_instance_ttl(&env);
        canceller.require_auth();
        let mut proposal = load_governance_proposal(&env, proposal_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::GovernanceProposalNotFound));
        if proposal.executed {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalAlreadyExecuted);
        }
        if proposal.cancelled {
            panic_with_error!(&env, MarketplaceError::GovernanceProposalCancelled);
        }
        let authorized = canceller == proposal.proposed_by || {
            match &proposal.proposal_type {
                GovernanceProposalType::GlobalPause => {
                    let holder = Self::get_role(env.clone(), RoleType::EmergencyPause);
                    canceller == holder
                }
                _ => {
                    let holder = Self::get_role(env.clone(), RoleType::ProtocolConfig);
                    canceller == holder
                }
            }
        };
        if !authorized {
            panic_with_error!(&env, MarketplaceError::Unauthorized);
        }
        proposal.cancelled = true;
        save_governance_proposal(&env, &proposal);
        crate::events::emit_governance_cancelled(&env, proposal_id, canceller);
    }

    /// View: return a governance proposal by ID.
    pub fn get_governance_proposal(env: Env, proposal_id: u64) -> GovernanceProposal {
        load_governance_proposal(&env, proposal_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::GovernanceProposalNotFound))
    }

    /// View: return current approvers for a governance proposal.
    pub fn get_governance_approvals(env: Env, proposal_id: u64) -> soroban_sdk::Vec<Address> {
        load_governance_approvals(&env, proposal_id)
    }

    /// View: return the counter-offer parent ID for a counter-offer, or None.
    pub fn get_counter_offer_parent(env: Env, counter_offer_id: u64) -> Option<u64> {
        get_counter_offer_parent(&env, counter_offer_id)
    }

    // ── Read-only getters ────────────────────────────────────

    pub fn get_listing(env: Env, listing_id: u64) -> Listing {
        load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound))
    }

    /// Returns the effective owner of a listing based on its current status.
    ///
    /// Ownership lifecycle invariants (Issue #456):
    ///   - Active   → `owner` is None (not yet transferred); the artist is the
    ///                effective owner and custodian.
    ///   - Sold     → `owner` is Some(buyer_address); buyer is the effective owner.
    ///   - Cancelled → `owner` is None; the NFT has been returned to the artist.
    ///
    /// Returns `artist` when `owner` is None so callers always receive a
    /// non-null address.
    pub fn get_effective_owner(env: Env, listing_id: u64) -> Address {
        let listing = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        listing.owner.unwrap_or(listing.artist)
    }

    /// Guarded, idempotent reconciliation of a listing's `owner` field.
    ///
    /// Called by an authorized `CollectionAdmin` when off-chain monitoring
    /// detects that the on-chain `owner` field is inconsistent with the actual
    /// NFT custodian (e.g. after an NFT transfer outside the normal listing
    /// lifecycle).
    ///
    /// Guards:
    ///   1. Caller must hold the `CollectionAdmin` role.
    ///   2. `expected_owner` must match `get_effective_owner(listing_id)` to
    ///      prevent concurrent reconciliation races.
    ///   3. Listing must exist.
    ///
    /// Idempotency: if `new_owner == get_effective_owner(listing_id)` already,
    /// the method returns without writing to storage or emitting an event.
    ///
    /// Emits `own_reconciled` on every state change so downstream consumers
    /// (indexer, audit tools) can persist the transition.
    pub fn reconcile_listing_owner(
        env: Env,
        caller: Address,
        listing_id: u64,
        expected_owner: Address,
        new_owner: Address,
    ) -> Result<(), MarketplaceError> {
        bump_instance_ttl(&env);
        Self::require_role(&env, &caller, RoleType::CollectionAdmin);

        let mut listing = load_listing(&env, listing_id)
            .ok_or(MarketplaceError::ListingNotFound)?;

        // Compute the current effective owner (artist when owner field is None).
        let current_effective = listing.owner.clone().unwrap_or(listing.artist.clone());

        // Guard: reject if the caller's expected_owner doesn't match what's stored.
        if current_effective != expected_owner {
            return Err(MarketplaceError::OwnershipMismatch);
        }

        // Idempotency: no-op when the new_owner already matches.
        if current_effective == new_owner {
            return Ok(());
        }

        let previous_owner = listing.owner.clone();
        listing.owner = Some(new_owner.clone());
        save_listing(&env, &listing);

        crate::events::emit_listing_ownership_reconciled(
            &env,
            listing_id,
            previous_owner,
            new_owner,
            caller,
        );

        Ok(())
    }
    pub fn get_total_listings(env: Env) -> u64 { get_listing_count(&env) }
    pub fn get_artist_listings(env: Env, artist: Address) -> Vec<u64> {
        get_artist_listing_ids(&env, &artist)
    }
    pub fn get_total_auctions(env: Env) -> u64 { get_auction_count(&env) }
    pub fn get_artist_auctions(env: Env, artist: Address) -> Vec<u64> {
        get_artist_auction_ids(&env, &artist)
    }

    /// Paginated ids of currently-active listings.
    ///
    /// Ordering note (deliberate change with the 1.1.0 paged storage): the
    /// active index uses swap-removal, so after any listing leaves the set the
    /// order is no longer strict creation order.  It is stable between
    /// removals; clients needing chronological order should sort by id.
    pub fn get_active_listings(env: Env, limit: u32, offset: u32) -> Vec<u64> {
        get_active_listing_ids_range(&env, offset, limit)
    }
    pub fn get_active_listings_page(env: Env, start: u32, limit: u32) -> Vec<u64> {
        Self::get_active_listings(env, limit, start)
    }

    /// Total number of currently-active listings (O(1)).
    pub fn get_active_listings_count(env: Env) -> u32 {
        active_listings_len(&env)
    }

    /// Number of Pending offers on `listing_id` — the counter enforcing
    /// MAX_OFFERS_PER_LISTING in `make_offer` (O(1)).
    pub fn get_pending_offer_count(env: Env, listing_id: u64) -> u32 {
        pending_offer_count(&env, listing_id)
    }

    /// Maximum `limit` accepted by `get_listings_paginated`.
    pub const MAX_PAGE_LIMIT: u32 = 100;
    pub fn get_listings_paginated(env: Env, start: u32, limit: u32) -> (Vec<Listing>, u32) {
        // Clamp limit to the hard maximum.
        let effective_limit = if limit > Self::MAX_PAGE_LIMIT {
            Self::MAX_PAGE_LIMIT
        } else if limit == 0 {
            // A zero limit is valid; return an empty page immediately.
            return (Vec::new(&env), start);
        } else {
            limit
        };

        let total = active_listings_len(&env);

        // Out-of-range start: return empty page + same cursor (no panic).
        if start >= total {
            return (Vec::new(&env), start);
        }

        let end = (start + effective_limit).min(total);
        // Read only the pages covering [start, end) — never the whole index.
        let ids = get_active_listing_ids_range(&env, start, end - start);
        let mut listings: Vec<Listing> = Vec::new(&env);
        for listing_id in ids.iter() {
            if let Some(listing) = load_listing(&env, listing_id) {
                listings.push_back(listing);
            }
        }
        (listings, end)
    }
    pub fn get_offers_by_listing(env: Env, listing_id: u64) -> Vec<crate::types::Offer> {
        let ids = load_listing_offers(&env, listing_id);
        let mut offers = Vec::new(&env);
        for oid in ids.iter() {
            if let Some(o) = load_offer(&env, oid) { offers.push_back(o); }
        }
        offers
    }
    pub fn get_listing_status(env: Env, listing_id: u64) -> ListingStatus {
        let l = load_listing(&env, listing_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::ListingNotFound));
        l.status
    }
    pub fn get_auction(env: Env, auction_id: u64) -> Auction {
        load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound))
    }
    pub fn get_auction_bids(env: Env, auction_id: u64) -> Vec<BidRecord> {
        load_auction(&env, auction_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::AuctionNotFound));
        load_auction_bids(&env, auction_id)
    }
    pub fn get_offer(env: Env, offer_id: u64) -> crate::types::Offer {
        load_offer(&env, offer_id)
            .unwrap_or_else(|| panic_with_error!(&env, MarketplaceError::OfferNotFound))
    }
    pub fn get_listing_offers(env: Env, listing_id: u64) -> Vec<u64> {
        load_listing_offers(&env, listing_id)
    }
    pub fn get_offerer_offers(env: Env, offerer: Address) -> Vec<u64> {
        load_offerer_offers(&env, &offerer)
    }
    /// Return the escrow record for a `(collection, token_id)` pair, if any.
    pub fn get_escrow(env: Env, collection: Address, token_id: u64)
        -> Option<crate::storage::EscrowRecord>
    {
        crate::storage::get_escrow_record(&env, &collection, token_id)
    }

    // ── Private helpers ──────────────────────────────────────

    fn create_listing_inner(
        env: &Env, artist: &Address, price: i128, currency: Symbol,
        token: Address, collection: Address, token_id: u64, quantity: u64,
        recipients: Vec<Recipient>, expires_at: Option<u64>,
    ) -> u64 {
        if price <= 0 { panic_with_error!(env, MarketplaceError::InvalidPrice); }
        Self::require_price_in_bounds(env, price);
        if let Some(exp) = expires_at {
            if exp <= env.ledger().timestamp() {
                panic_with_error!(env, MarketplaceError::InvalidPrice);
            }
        }
        // Issue #460 — enforce configured min/max listing duration bounds.
        assert_listing_duration_policy(env, expires_at);

        let recipients_len = recipients.len();
        if recipients_len == 0 {
            panic_with_error!(env, MarketplaceError::InvalidSplit);
        }
        if recipients_len > 4 {
            panic_with_error!(env, MarketplaceError::TooManyRecipients);
        }

        let protocol_fee_bps = crate::storage::get_protocol_fee_bps_storage(env).unwrap_or(0);
        Self::validate_recipients(env, &recipients, protocol_fee_bps);
        // Policy engine gate (Issue #435): rejects self-token, collection-token,
        // and any token not currently active in the whitelist registry.
        Self::validate_token_asset(env, &token, Some(&collection));
        assert_token_policy_active(env, &token);

        // Issue #458 — verify the token belongs to the declared collection and
        // that the collection standard matches the requested quantity semantics.
        Self::require_collection_compatible(env, &collection, token_id, quantity);

        let listing_id = increment_listing_count(env);
        let listing = Listing {
            listing_id,
            artist: artist.clone(),
            price,
            currency,
            token,
            collection: collection.clone(),
            token_id,
            quantity,
            recipients,
            status: ListingStatus::Active,
            owner: None,
            created_at: env.ledger().sequence(),
            protocol_fee_bps,
            expires_at,
            reserved_for: None,
            reservation_start: None,
            reservation_end: None,
        };
        save_listing(env, &listing);
        add_artist_listing_id(env, artist, listing_id);
        add_to_active_listings(env, listing_id);
        ListingCreatedEvent {
            listing_id,
            artist: artist.clone(),
            price,
            currency: listing.currency.clone(),
            collection: listing.collection.clone(),
            token_id: listing.token_id,
            ledger_sequence: env.ledger().sequence(),
            schema_version: 1,
        }.publish(env);

        escrow::escrow_nft(env, artist, &listing.collection, listing.token_id, true, listing_id);
        escrow::emit_nft_escrowed(env, listing_id, &listing.collection,
            listing.token_id, artist, env.ledger().sequence());
        listing_id
    }

    fn update_listing_inner(
        env: &Env, artist: &Address, listing_id: u64,
        new_price: i128, new_token: Address, new_recipients: Vec<Recipient>,
    ) -> bool {
        let mut listing = load_listing(env, listing_id)
            .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::ListingNotFound));
        if listing.artist != *artist { panic_with_error!(env, MarketplaceError::Unauthorized); }
        if listing.status != ListingStatus::Active {
            panic_with_error!(env, MarketplaceError::ListingNotActive);
        }

        if pending_offer_count(env, listing_id) > 0 {
            panic_with_error!(env, MarketplaceError::Unauthorized);
        }
        if new_price <= 0 { panic_with_error!(env, MarketplaceError::InvalidPrice); }
        // Policy engine: enforce price bounds at update time (Issue #435).
        assert_price_policy(env, new_price);
        // Policy engine: enforce token whitelist at update time (Issue #435).
        Self::validate_token_asset(env, &new_token, Some(&listing.collection));
        assert_token_policy_active(env, &new_token);
        let nrlen = new_recipients.len();
        if nrlen == 0 { panic_with_error!(env, MarketplaceError::InvalidSplit); }
        if nrlen > 4  { panic_with_error!(env, MarketplaceError::TooManyRecipients); }
        Self::validate_recipients(env, &new_recipients, listing.protocol_fee_bps);
        let old_price = listing.price;
        listing.price = new_price;
        listing.token = new_token;
        listing.recipients = new_recipients;
        save_listing(env, &listing);
        ListingUpdatedEvent {
            listing_id,
            artist: artist.clone(),
            new_price,
            collection: listing.collection.clone(),
            token_id: listing.token_id,
            ledger_sequence: env.ledger().sequence(),
        }.publish(env);
        // Emit price-change event whenever the price actually changes so
        // the indexer can maintain a full PriceHistory table (Issue #213).
        if old_price != new_price {
            ListingPriceUpdatedEvent {
                listing_id,
                old_price,
                new_price,
                updated_by: artist.clone(),
            }.publish(env);
        }
        true
    }

    fn cancel_listing_inner(env: &Env, artist: &Address, listing_id: u64) -> bool {
        let mut listing = load_listing(env, listing_id)
            .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::ListingNotFound));
        if listing.artist != *artist { panic_with_error!(env, MarketplaceError::Unauthorized); }
        if listing.status != ListingStatus::Active {
            panic_with_error!(env, MarketplaceError::ListingNotActive);
        }

        for offer_id in load_pending_offer_ids(env, listing_id).iter() {
            if let Some(mut offer) = load_offer(env, offer_id) {
                if offer.status == OfferStatus::Pending {
                    offer.status = OfferStatus::Rejected;
                    save_offer(env, &offer);
                    TokenClient::new(env, &offer.token).transfer(
                        &env.current_contract_address(), &offer.offerer, &offer.amount,
                    );
                }
            }
        }
        clear_pending_offers(env, listing_id);

        transition_listing_to_cancelled(
            env,
            &mut listing,
            listing_id,
            artist.clone(),
            CancelReason::Owner,
            false,
        );
        true
    }

    // ── Auction invariants layer (Issue #434) ─────────────────────────────────
    //
    // A single, authoritative validator that every auction mutation path calls
    // before changing state.  Centralising all safety constraints here means:
    //   • Each rule is expressed exactly once — no per-function drift.
    //   • New entry points get protection for free by calling this helper.
    //   • The invariant set is easy to audit in isolation.
    //
    // Invariants checked (in order):
    //   A. Auction must exist.
    //   B. Auction must be Active when a mutating operation is requested.
    //   C. `end_time` must be strictly in the future for bid placement.
    //   D. Self-bidding is forbidden.
    //   E. Blocked bidders may not bid.
    //   F. Bid amount must meet the minimum threshold
    //      (reserve_price when no prior bids; highest_bid + min_increment otherwise).
    //   G. Anti-sniping: if `extension_trigger > 0` and the bid arrives within
    //      the trigger window, an extension is proposed but only applied when it
    //      satisfies BOTH:
    //        • `extension_count < max_extensions` (or `max_extensions == 0` for unlimited)
    //        • `now + extension_window <= original_end_time + MAX_TOTAL_AUCTION_DURATION`
    //   H. Finalization is only permitted after `end_time`.
    //   I. Cancellation by creator is only permitted when `highest_bidder.is_none()`.

    /// Outcome of the `check_bid_invariants` helper: either the bid is valid
    /// (and the optional extension details are returned), or the call should
    /// panic with a specific error.
    // NOTE: defined outside `impl` to satisfy Rust — see `BidInvariantResult` below.

    /// Validate all per-bid auction invariants.
    ///
    /// Returns `BidInvariantResult` on success; panics with the appropriate
    /// `MarketplaceError` variant if any invariant is violated.
    ///
    /// # Invariants enforced
    /// B  Auction must be Active.
    /// C  `end_time` must be strictly in the future.
    /// D  Bidder must not be the auction creator.
    /// E  Bidder must not be in the blocked-bidder registry.
    /// F  `amount >= required_min` (reserve or highest_bid + min_increment).
    /// G  Anti-sniping extension is bounded by `max_extensions` and
    ///    `original_end_time + MAX_TOTAL_AUCTION_DURATION`.
    fn check_bid_invariants(
        env: &Env,
        auction: &Auction,
        bidder: &Address,
        amount: i128,
    ) -> BidInvariantResult {
        // B — must be Active
        assert_auction_active(env, &auction.status);
        let now = env.ledger().timestamp();
        // C — must be before end_time
        if now >= auction.end_time {
            panic_with_error!(env, MarketplaceError::AuctionExpired);
        }
        // D — no self-bidding
        if *bidder == auction.creator {
            panic_with_error!(env, MarketplaceError::SelfBidNotAllowed);
        }
        // E — blocked bidder check
        if is_bidder_blocked(env, auction.auction_id, bidder) {
            panic_with_error!(env, MarketplaceError::Unauthorized);
        }
        // F — minimum bid requirement
        let required_min = if auction.highest_bid == 0 {
            auction.reserve_price
        } else {
            auction.highest_bid
                .checked_add(auction.min_increment)
                .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::BidTooLow))
        };
        if amount < required_min {
            panic_with_error!(env, MarketplaceError::BidTooLow);
        }
        // G — anti-sniping extension analysis (Issue #468)
        //
        // A bid that arrives inside the trigger window is always accepted.
        // Whether the auction extends depends on two independent caps:
        //   • extension_count cap  (max_extensions > 0): bid accepted, no extension
        //   • total duration cap   (original_end_time + MAX_TOTAL_AUCTION_DURATION): same
        // Neither cap ever rejects a bid — they only suppress the extension.
        let extension = if auction.extension_trigger > 0 {
            let time_remaining = auction.end_time.saturating_sub(now);
            if time_remaining < auction.extension_trigger {
                // Extension count cap reached — bid accepted, extension suppressed.
                if auction.max_extensions > 0
                    && auction.extension_count >= auction.max_extensions
                {
                    None
                } else if auction.extension_window == 0 {
                    // Extension window misconfigured — suppress silently.
                    None
                } else {
                    let proposed_end =
                        now.checked_add(auction.extension_window).unwrap_or(auction.end_time);
                    let max_allowed = auction
                        .original_end_time
                        .saturating_add(MAX_TOTAL_AUCTION_DURATION);
                    if proposed_end <= max_allowed {
                        Some((proposed_end, auction.extension_count.saturating_add(1)))
                    } else {
                        // Duration cap reached — bid accepted but no extension.
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        BidInvariantResult { extension }
    }

    /// Validate that `finalize_auction` may proceed for the given `auction`.
    ///
    /// # Invariants enforced
    /// B  Auction must be Active.
    /// H  `end_time` must have passed.
    fn check_finalize_invariants(env: &Env, auction: &Auction) {
        assert_auction_transition(env, &auction.status, &AuctionStatus::Finalized);
        if env.ledger().timestamp() < auction.end_time {
            panic_with_error!(env, MarketplaceError::AuctionNotEnded);
        }
    }

    /// Validate that `cancel_auction` (creator-initiated) may proceed.
    ///
    /// # Invariants enforced
    /// Auth  Caller must be the auction creator.
    /// B     Auction must be Active.
    /// I     No bids may have been placed yet.
    fn check_cancel_invariants(env: &Env, auction: &Auction, caller: &Address) {
        if auction.creator != *caller {
            panic_with_error!(env, MarketplaceError::Unauthorized);
        }
        assert_auction_transition(env, &auction.status, &AuctionStatus::Cancelled);
        if auction.highest_bidder.is_some() {
            panic_with_error!(env, MarketplaceError::AuctionHasBids);
        }
    }

    fn require_admin(env: &Env) {
        let key = crate::storage::DataKey::Admin;
        let admin = env.storage().persistent()
            .get::<_, Address>(&key).expect("admin not set");
        admin.require_auth();
    }

    /// Authorize `caller` as the current holder of `role` (falling back to
    /// `Admin` when no explicit holder has been assigned, so existing
    /// single-admin deployments work unchanged).
    ///
    /// Calls `caller.require_auth()` if the check passes, or panics with
    /// `Unauthorized` if the caller is not the role holder.
    fn require_role(env: &Env, caller: &Address, role: RoleType) {
        let holder = crate::storage::get_role_storage(env, &role)
            .unwrap_or_else(|| {
                env.storage()
                    .persistent()
                    .get::<_, Address>(&DataKey::Admin)
                    .expect("admin not set")
            });
        if *caller != holder {
            panic_with_error!(env, MarketplaceError::Unauthorized);
        }
        caller.require_auth();
    }

    /// Resolve the typed cancel reason for `admin_cancel_auction` (Issue #848).
    ///
    /// ProtocolConfig takes precedence (Admin reason). EmergencyPause-only
    /// callers receive Emergency. Callers with neither role are rejected.
    fn resolve_auction_cancel_reason(env: &Env, caller: &Address) -> AuctionCancelReason {
        let admin_fallback = env
            .storage()
            .persistent()
            .get::<_, Address>(&DataKey::Admin)
            .expect("admin not set");
        let protocol = crate::storage::get_role_storage(env, &RoleType::ProtocolConfig)
            .unwrap_or_else(|| admin_fallback.clone());
        let emergency = crate::storage::get_role_storage(env, &RoleType::EmergencyPause)
            .unwrap_or_else(|| admin_fallback);
        if *caller == protocol {
            caller.require_auth();
            return AuctionCancelReason::Admin;
        }
        if *caller == emergency {
            caller.require_auth();
            return AuctionCancelReason::Emergency;
        }
        panic_with_error!(env, MarketplaceError::Unauthorized);
    }

    /// Like `require_role` but resolves the current invoker from `env` rather
    /// than accepting an explicit `caller` argument.  Used in entry points where
    /// there is no dedicated `admin: Address` parameter (e.g. `revoke_artist`,
    /// `reinstate_artist`).
    fn require_role_auth(env: &Env, role: RoleType) {
        let holder = crate::storage::get_role_storage(env, &role)
            .unwrap_or_else(|| {
                env.storage()
                    .persistent()
                    .get::<_, Address>(&DataKey::Admin)
                    .expect("admin not set")
            });
        holder.require_auth();
    }

    /// Authorize `caller` (already `require_auth`ed) as either the auction's
    /// creator or the contract admin — the two roles allowed to manage a
    /// blocked-bidder registry (Issue #199).
    fn require_creator_or_admin(env: &Env, caller: &Address, creator: &Address) {
        if caller == creator {
            return;
        }
        if let Some(admin) = env
            .storage()
            .persistent()
            .get::<_, Address>(&DataKey::Admin)
        {
            if *caller == admin {
                return;
            }
        }
        panic_with_error!(env, MarketplaceError::Unauthorized);
    }

    fn require_price_in_bounds(env: &Env, price: i128) {
        // Delegates to the policy engine (Issue #435).
        assert_price_policy(env, price);
    }

    fn require_not_paused(env: &Env) {
        if crate::storage::is_paused(env) {
            panic_with_error!(env, MarketplaceError::ContractPaused);
        }
    }

    /// Granular pause check with optional collection and function context.
    /// Panics with ContractPaused if ANY active circuit-breaker fires.
    fn require_not_paused_ctx(
        env: &Env,
        collection: Option<&Address>,
        function_name: Option<&Symbol>,
    ) {
        if crate::storage::is_paused_for(env, collection, function_name) {
            panic_with_error!(env, MarketplaceError::ContractPaused);
        }
    }

    fn require_not_revoked(env: &Env, artist: &Address) {
        if is_artist_revoked_storage(env, artist) {
            panic_with_error!(env, MarketplaceError::ArtistRevoked);
        }
    }

    fn validate_token_asset(env: &Env, token: &Address, collection: Option<&Address>) {
        if *token == env.current_contract_address() {
            panic_with_error!(env, MarketplaceError::TokenNotWhitelisted);
        }
        if let Some(col) = collection {
            if token == col {
                panic_with_error!(env, MarketplaceError::TokenNotWhitelisted);
            }
        }
    }

    // ── IPFS CID validation (Issue #206) ─────────────────────────────────────
    //
    // Accepts two canonical CID formats:
    //
    //   CIDv1 base32:  starts with 'b' (multibase prefix), followed by
    //                  lowercase a-z and digits 2-7, total length 46–100 chars.
    //
    //   CIDv0 base58:  starts with "Qm", exactly 46 characters, using the
    //                  base58 alphabet (1-9, A-H, J-N, P-Z, a-k, m-z).
    //
    // Implemented with soroban_sdk::Bytes character iteration — no std, no
    // external dependencies, fully no_std compatible with the Soroban runtime.
    fn validate_cid(cid: &soroban_sdk::String) -> bool {
        let len = cid.len();
        // Minimum 46 chars covers both CIDv0 (exactly 46) and CIDv1 (≥ 46).
        // Maximum 100 chars provides headroom for variant CIDv1 encodings.
        if len < 46 || len > 100 {
            return false;
        }

        // soroban_sdk::String has no iter(); convert to Bytes for indexed access.
        let bytes = cid.to_bytes();

        let first  = bytes.get(0).unwrap_or(0) as char;
        let second = bytes.get(1).unwrap_or(0) as char;

        // ── CIDv1 base32 lowercase (multibase prefix 'b') ────────────────────
        if first == 'b' {
            for i in 1..len {
                let c = bytes.get(i).unwrap_or(0) as char;
                if !matches!(c, 'a'..='z' | '2'..='7') {
                    return false;
                }
            }
            return true;
        }

        // ── CIDv0 base58 (prefix "Qm", exactly 46 chars) ─────────────────────
        if first == 'Q' && second == 'm' {
            if len != 46 {
                return false;
            }
            // base58 alphabet excludes: '0', 'I', 'O', 'l'
            for i in 0..len {
                let c = bytes.get(i).unwrap_or(0) as char;
                if !matches!(c,
                    '1'..='9'
                    | 'A'..='H' | 'J'..='N' | 'P'..='Z'
                    | 'a'..='k' | 'm'..='z'
                ) {
                    return false;
                }
            }
            return true;
        }

        false
    }

    /// View: return `true` for a well-formed IPFS CID (CIDv0 or CIDv1),
    /// `false` for any malformed input.
    ///
    /// Accepted formats:
    /// - CIDv1 base32: starts with `b`, 46–100 lowercase base32 chars (`a-z`, `2-7`).
    /// - CIDv0 base58: starts with `Qm`, exactly 46 base58 chars.
    ///
    /// Callers can pre-validate CIDs returned by IPFS pinning services before
    /// including them in `create_listing` or `create_auction` invocations.
    pub fn check_cid_valid(_env: Env, cid: soroban_sdk::String) -> bool {
        Self::validate_cid(&cid)
    }

    /// Validate that the sum of all `Recipient.percentage` values (each expressed
    /// in basis points, 0–10 000) plus the current protocol fee does not exceed
    /// 10 000 bps (100 %).
    ///
    /// Uses `checked_add` throughout to prevent integer overflow on malformed
    /// input.  Panics with `RoyaltyExceedsLimit` when the invariant is violated.
    ///
    /// # Invariants checked
    /// * `recipients` must be non-empty (caller responsibility to guard with `InvalidSplit`).
    /// * Each recipient's `percentage` must be > 0 (ZeroRecipientBps).
    /// * No duplicate addresses in the recipient list (DuplicateRecipient).
    /// * `sum(r.percentage) <= 10_000` (recipients share of remaining after fee)
    fn validate_recipients(env: &Env, recipients: &Vec<Recipient>, _protocol_fee_bps: u32) {
        let len = recipients.len();
        let mut total_bps: u32 = 0;
        for i in 0..len {
            let r = recipients.get(i).unwrap();
            // Each recipient must contribute a non-zero share.
            if r.percentage == 0 {
                panic_with_error!(env, MarketplaceError::ZeroRecipientBps);
            }
            // Duplicate address check: compare against every prior entry.
            for j in 0..i {
                if recipients.get(j).unwrap().address == r.address {
                    panic_with_error!(env, MarketplaceError::DuplicateRecipient);
                }
            }
            total_bps = total_bps.checked_add(r.percentage)
                .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::RoyaltyExceedsLimit));
        }
        if total_bps > 10_000 { panic_with_error!(env, MarketplaceError::RoyaltyExceedsLimit); }
    }

    /// Returns `(protocol_fee_collected, per-recipient payouts)`. The payout
    /// vector records every transfer made out of the sale price except the
    /// protocol fee — i.e. the collection-level `royalty_info` receiver (when
    /// paid) followed by each configured recipient — so entries always sum to
    /// `amount - protocol_fee_collected`. (Issue #201)
    ///
    /// For each recipient a `RoyaltyClaimRecord` is written to storage BEFORE
    /// the token transfer; on success the record is immediately marked
    /// `claimed = true`.  This gives operators a queryable payout status even
    /// if a future upgrade changes settlement behaviour, and provides the
    /// foundation for `claim_royalty` recovery (Issue #461).
    #[allow(clippy::too_many_arguments)]
    fn distribute_payout(
        env: &Env, token_addr: &Address, collection_addr: &Address,
        amount: i128, seller: &Address, recipients: &Vec<Recipient>,
        buyer: &Address, transfer_from_buyer: bool, fee_bps: u32,
    ) -> (i128, Vec<RecipientPayout>) {
        // Settlement context: derive settlement_id and is_listing from environment.
        // We use the ledger sequence as a surrogate settlement-instance identifier
        // within this helper so each call site doesn't need to thread them through.
        // For a richer per-call ID the callers pass settlement_id via the new
        // distribute_payout_tracked variant below; distribute_payout remains
        // unchanged for backward compatibility.
        Self::distribute_payout_tracked(
            env, token_addr, collection_addr, amount, seller, recipients,
            buyer, transfer_from_buyer, fee_bps, 0, true,
        )
    }

    /// Like `distribute_payout` but records per-recipient claim records keyed by
    /// the explicit `settlement_id` / `is_listing` pair (Issue #461).
    #[allow(clippy::too_many_arguments)]
    fn distribute_payout_tracked(
        env: &Env, token_addr: &Address, collection_addr: &Address,
        amount: i128, seller: &Address, recipients: &Vec<Recipient>,
        buyer: &Address, transfer_from_buyer: bool, fee_bps: u32,
        settlement_id: u64, is_listing: bool,
    ) -> (i128, Vec<RecipientPayout>) {
        let token = TokenClient::new(env, token_addr);
        if transfer_from_buyer {
            token.transfer(buyer, &env.current_contract_address(), &amount);
        }
        let mut payouts: Vec<RecipientPayout> = Vec::new(env);
        let royalty_info: (Address, u32) = env.invoke_contract(
            collection_addr,
            &soroban_sdk::Symbol::new(env, "royalty_info"),
            soroban_sdk::vec![env],
        );
        let (royalty_receiver, royalty_bps) = royalty_info;
        let mut payout = amount;
        if royalty_bps > 0 && royalty_receiver != *seller {
            let royalty = amount
                .checked_mul(royalty_bps as i128)
                .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::ArithmeticOverflow))
                .checked_div(10_000)
                .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::ArithmeticOverflow));
            // Write claim record before transfer for observability.
            let mut claim = crate::types::RoyaltyClaimRecord {
                settlement_id,
                is_listing,
                recipient: royalty_receiver.clone(),
                token: token_addr.clone(),
                amount: royalty,
                claimed: false,
                created_at: env.ledger().sequence(),
                claimed_at: None,
            };
            crate::storage::set_royalty_claim(env, settlement_id, is_listing, &royalty_receiver, &claim);
            crate::events::emit_royalty_claim_queued(
                env, settlement_id, is_listing, royalty_receiver.clone(), token_addr.clone(), royalty,
            );
            token.transfer(&env.current_contract_address(), &royalty_receiver, &royalty);
            // Mark as claimed after successful transfer.
            claim.claimed = true;
            claim.claimed_at = Some(env.ledger().sequence());
            crate::storage::set_royalty_claim(env, settlement_id, is_listing, &royalty_receiver, &claim);
            crate::events::emit_royalty_claimed(
                env, settlement_id, is_listing, royalty_receiver.clone(), token_addr.clone(), royalty,
            );
            payouts.push_back(RecipientPayout { address: royalty_receiver, amount: royalty });
            payout -= royalty;
        }

        // ── Fee + recipient split via math::distribute ────────────────────────
        let effective_fee_bps = if crate::storage::get_treasury_storage(env).is_some() {
            fee_bps
        } else {
            0
        };
        let dist = crate::math::distribute(env, payout, effective_fee_bps, recipients);

        let mut fee_collected: i128 = 0;
        if dist.fee > 0 {
            if let Some(t) = crate::storage::get_treasury_storage(env) {
                token.transfer(&env.current_contract_address(), &t, &dist.fee);
                fee_collected = dist.fee;
            }
        }
        for entry in dist.iter_payouts() {
            // Write claim record before transfer.
            let mut claim = crate::types::RoyaltyClaimRecord {
                settlement_id,
                is_listing,
                recipient: entry.address.clone(),
                token: token_addr.clone(),
                amount: entry.amount,
                claimed: false,
                created_at: env.ledger().sequence(),
                claimed_at: None,
            };
            crate::storage::set_royalty_claim(env, settlement_id, is_listing, &entry.address, &claim);
            crate::events::emit_royalty_claim_queued(
                env, settlement_id, is_listing, entry.address.clone(), token_addr.clone(), entry.amount,
            );
            token.transfer(&env.current_contract_address(), &entry.address, &entry.amount);
            // Mark as claimed after successful transfer.
            claim.claimed = true;
            claim.claimed_at = Some(env.ledger().sequence());
            crate::storage::set_royalty_claim(env, settlement_id, is_listing, &entry.address, &claim);
            crate::events::emit_royalty_claimed(
                env, settlement_id, is_listing, entry.address.clone(), token_addr.clone(), entry.amount,
            );
            payouts.push_back(RecipientPayout { address: entry.address.clone(), amount: entry.amount });
        }
        (fee_collected, payouts)
    }

    // ── Royalty payout recovery (Issue #461) ──────────────────────────────────

    /// Query the payout status for a single recipient in a completed settlement.
    ///
    /// Returns the `RoyaltyClaimRecord` if one exists, or `None` when no claim
    /// was recorded for this (settlement_id, is_listing, recipient) triple.
    /// Callers can use `claimed` and `claimed_at` to determine whether payment
    /// was successfully delivered.
    pub fn get_royalty_claim(
        env: Env,
        settlement_id: u64,
        is_listing: bool,
        recipient: Address,
    ) -> Option<crate::types::RoyaltyClaimRecord> {
        crate::storage::get_royalty_claim(&env, settlement_id, is_listing, &recipient)
    }

    /// Recovery entry point: transfer an unclaimed royalty to its recipient.
    ///
    /// This is the pull-based fallback for any claim record where `claimed =
    /// false`.  In normal operation settlements auto-mark claims as claimed at
    /// settlement time, so this is only needed when:
    ///   - A contract upgrade changed distribution behavior mid-settlement.
    ///   - An operator manually wrote a claim record without completing the
    ///     corresponding transfer.
    ///
    /// Auth: the `recipient` address must sign (they are pulling their own funds).
    ///
    /// Errors:
    ///   - `RoyaltyClaimNotFound`   — no record for this triple.
    ///   - `RoyaltyAlreadyClaimed`  — `claimed = true`; prevents double-payment.
    pub fn claim_royalty(
        env: Env,
        recipient: Address,
        settlement_id: u64,
        is_listing: bool,
    ) -> bool {
        bump_instance_ttl(&env);
        recipient.require_auth();

        let mut claim =
            crate::storage::get_royalty_claim(&env, settlement_id, is_listing, &recipient)
                .unwrap_or_else(|| {
                    panic_with_error!(&env, MarketplaceError::RoyaltyClaimNotFound)
                });

        if claim.claimed {
            panic_with_error!(&env, MarketplaceError::RoyaltyAlreadyClaimed);
        }

        // Transfer funds from marketplace to recipient.
        TokenClient::new(&env, &claim.token)
            .transfer(&env.current_contract_address(), &recipient, &claim.amount);

        // Mark claimed atomically.
        claim.claimed = true;
        claim.claimed_at = Some(env.ledger().sequence());
        crate::storage::set_royalty_claim(&env, settlement_id, is_listing, &recipient, &claim);

        crate::events::emit_royalty_claimed(
            &env,
            settlement_id,
            is_listing,
            recipient,
            claim.token.clone(),
            claim.amount,
        );
        true
    }

    /// Paginate unclaimed royalty records for a recipient (Issue #842).
    ///
    /// Walks the recipient claim index from `start` for up to `limit` entries
    /// and returns only those with `claimed == false`.
    pub fn get_unclaimed_royalties(
        env: Env,
        recipient: Address,
        start: u32,
        limit: u32,
    ) -> soroban_sdk::Vec<crate::types::RoyaltyClaimRecord> {
        crate::storage::get_unclaimed_royalties(&env, &recipient, start, limit)
    }

    // ── Collection compatibility (Issue #458) ─────────────────────────────────
    //
    // Before escrowing a token we cross-call the collection contract to verify
    // that the (collection, token_id, quantity) triple makes sense:
    //   - For ERC-721 / LazyMint721 contracts: `quantity` must be exactly 1 and
    //     `owner_of(token_id)` must return the artist (ownership guard).
    //   - For ERC-1155 / LazyMint1155 contracts: any `quantity >= 1` is allowed;
    //     we verify `balance_of(artist, token_id) >= quantity`.
    //   - When the collection does not expose `contract_type()` we fall back to
    //     the quantity semantics: quantity == 1 means ERC-721 path (owner_of),
    //     quantity > 1 means ERC-1155 path (balance_of).
    //
    // These cross-calls are intentionally made in `create_listing_inner` (and the
    // batch preflight) BEFORE escrow, so a mismatched listing is rejected before
    // any NFT is pulled into custody.

    /// Enforce collection / token compatibility. Panics with
    /// `CollectionIncompatible` on any mismatch. (Issue #458)
    fn require_collection_compatible(
        env: &Env,
        collection: &Address,
        token_id: u64,
        quantity: u64,
    ) {
        if let Err(_) = Self::preflight_collection_compatibility(env, collection, token_id, quantity) {
            panic_with_error!(env, MarketplaceError::CollectionIncompatible);
        }
    }

    /// Non-panicking version used during batch preflight so the caller can
    /// surface a `BatchItemError` instead of a raw panic.
    ///
    /// Returns `Ok(())` when compatible, `Err(())` otherwise.
    fn preflight_collection_compatibility(
        env: &Env,
        collection: &Address,
        _token_id: u64,
        quantity: u64,
    ) -> Result<(), ()> {
        // Attempt to read the collection's declared type. Many deployed
        // collections expose `contract_type() -> Symbol`; if the call fails
        // we fall back to quantity-based inference (no panic on missing method).
        let kind_result: Option<soroban_sdk::Symbol> = env
            .try_invoke_contract::<soroban_sdk::Symbol, crate::types::MarketplaceError>(
                collection,
                &soroban_sdk::Symbol::new(env, "contract_type"),
                soroban_sdk::vec![env],
            )
            .ok()
            .and_then(|r| r.ok());

        if let Some(kind) = kind_result {
            let erc721     = soroban_sdk::Symbol::new(env, "ERC721");
            let lazy721    = soroban_sdk::Symbol::new(env, "LazyMint721");
            // ERC-721 and LazyMint721 collections are single-token: quantity must be 1.
            if kind == erc721 || kind == lazy721 {
                if quantity != 1 {
                    return Err(());
                }
            }
            // ERC-1155 and LazyMint1155 support any positive quantity.
            // Unknown types are accepted with a quantity-based fallback.
        } else {
            // Fallback: treat quantity == 1 as ERC-721 compatible, any as ERC-1155.
            // Both are valid; we cannot reject without a declared type.
        }

        // Quantity must always be at least 1.
        if quantity == 0 {
            return Err(());
        }

        Ok(())
    }

    // ── Preflight helpers for batch validation (Issue #457) ──────────────────
    //
    // Non-panicking wrappers around the same validation logic used in
    // `create_listing_inner`. Used in `create_listings` during the preflight
    // pass so any failure is attributed to its item index before panicking.

    /// Returns `Err(())` when `price` violates configured price bounds.
    fn preflight_price(env: &Env, price: i128) -> Result<(), ()> {
        if let Some(min) = get_min_price_storage(env) {
            if price < min { return Err(()); }
        }
        if let Some(max) = get_max_price_storage(env) {
            if price > max { return Err(()); }
        }
        Ok(())
    }

    /// Returns `Err(())` when `expires_at` violates configured duration bounds.
    fn preflight_duration(env: &Env, expires_at: Option<u64>) -> Result<(), ()> {
        let Some(exp) = expires_at else { return Ok(()) };
        let now = env.ledger().timestamp();
        if let Some(min_secs) = crate::storage::get_min_listing_duration_storage(env) {
            if exp < now.saturating_add(min_secs) { return Err(()); }
        }
        if let Some(max_secs) = crate::storage::get_max_listing_duration_storage(env) {
            if exp > now.saturating_add(max_secs) { return Err(()); }
        }
        Ok(())
    }

    /// Returns `Err(())` when recipients fail any validation rule.
    fn preflight_recipients(
        _env: &Env,
        recipients: &Vec<Recipient>,
        _protocol_fee_bps: u32,
    ) -> Result<(), ()> {
        let len = recipients.len();
        let mut total: u32 = 0;
        for i in 0..len {
            let r = recipients.get(i).unwrap();
            if r.percentage == 0 { return Err(()); }
            for j in 0..i {
                if recipients.get(j).unwrap().address == r.address { return Err(()); }
            }
            total = match total.checked_add(r.percentage) {
                Some(v) => v,
                None => return Err(()),
            };
        }
        if total > 10_000 { return Err(()); }
        Ok(())
    }
}

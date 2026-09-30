// storage.rs
use crate::types::{Auction, BidRecord, Listing, MarketplaceError, Offer};
use soroban_sdk::{contracttype, panic_with_error, Address, Env, Vec};

// ── Storage retention classification (Issue #280) ────────────────────────
//
// Every `DataKey` variant below falls into exactly one of three retention
// classes. This classification is the contract's answer to "what happens to
// this key over time" and drives both the bounded maintenance entry points
// in `contract.rs` (`extend_active_ttls`, `cleanup_expired_locks`) and what
// the indexer/operators should expect to still be readable on-chain months
// or years later. See also `docs/guides/storage-retention.md`.
//
// ACTIVE — must never be cleaned up while the referenced record is live.
//   `Listing(id)` while status == Active, `Auction(id)` while status ==
//   Active, `Offer(id)` while status == Pending, `ListingPendingOffers(id)`
//   for such a listing, `ActiveListingPos(id)`, the `ActiveListings` index
//   pages/length, and the two lock keys while genuinely held mid-transaction
//   (`ListingLock`/`AuctionLock` — see the Recoverable note below on why
//   these are not actually a growth risk).  `extend_active_ttls` walks
//   exactly this set and re-extends its TTL; it never deletes anything.
//
// RECOVERABLE — safe to clear once terminal / stale; already bounded today.
//   - `ListingLock(id)` / `AuctionLock(id)`: written to *temporary* (not
//     persistent) storage with a short TTL (`REENTRANCY_LOCK_TTL` ledgers,
//     see `acquire_listing_lock`/`acquire_auction_lock`) and explicitly
//     released on every normal exit path of every function that acquires
//     one. A panic mid-call rolls back the whole transaction (including the
//     lock write), so it can never "leak" a lock either. Net effect: these
//     keys are not an unbounded persistent-storage growth vector at all —
//     Soroban's own temporary-entry expiry reclaims them even in the
//     hypothetical case this contract failed to release one. The
//     `cleanup_expired_locks` entry point exists as an operator-triggered
//     safety valve for a stuck lock spotted off-chain, not a routine sweep.
//   - Legacy pre-1.1.0 monolithic index keys (`ArtistListings(Address)`,
//     `ArtistAuctions(Address)`, `ListingOffers(u64)`, `OffererOffers
//     (Address)`, `ActiveListings`): already drained and deleted by the
//     existing bounded `migrate`/`migrate_step` entry point via
//     `take_legacy_index_vec`; nothing further to add here.
//   - `ActiveListingPos(id)`, `ListingPendingOffers(id)`, `IndexPage`/
//     `IndexLen` pages, `EscrowedToken`: already self-cleaning — each helper
//     in this file deletes its own key the moment the last element/flag is
//     removed (see `index_store_page`, `remove_from_active_listings`,
//     `remove_pending_offer`, `clear_escrow_record`). No batch job needed.
//   - `ArtistCancelCursor(Address)` / `MigrationCursor`: cleared on
//     completion (migration) or on reinstatement (cancel cursor); the tiny
//     residual left for a permanently-revoked artist is a single `u32` and
//     not considered worth a dedicated sweep.
//
// ARCHIVAL — retained indefinitely on-chain for provenance / dispute
// resolution; never actively deleted by contract code.
//   `Listing(id)`, `Auction(id)`, `Offer(id)` records themselves, in *any*
//   status, including terminal ones (Sold/Cancelled/Finalized/Accepted/
//   Rejected/Withdrawn). The contract does not hard-delete historical
//   marketplace records: doing so would destroy the provenance trail an
//   indexer, a dispute, or a future audit needs. Instead, `extend_active_ttls`
//   deliberately *skips* re-extending a terminal record's TTL, so once it
//   naturally lapses, Soroban's own archival mechanism takes over (the data
//   is still restorable on-chain, just no longer "hot"; see
//   `docs/guides/storage-retention.md` for the operator-facing explanation
//   of what this means for the indexer). `RevokedArtist`, `MigrationDone`
//   markers and config keys (`Admin`, `Treasury`, `ProtocolFeeBps`, price
//   bounds, etc.) are likewise small, permanent, and intentionally never
//   swept.
//
// Bounded maintenance (Issue #280 acceptance criteria #2/#3):
//   Both `extend_active_ttls` and `cleanup_expired_locks` (contract.rs)
//   process at most `MAX_MAINTENANCE_ITEMS` entries per call regardless of
//   the caller-supplied `max_items`, persist a resumable cursor
//   (`TtlSweepProgress`) or accept an explicit bounded id list, and check
//   status before touching anything so an Active listing, an Active
//   auction, or a Pending offer can never be removed by a maintenance call.
//
/// Identifies one of the growing id-collections kept by the marketplace.
///
/// Every index is stored as a sequence of fixed-capacity pages
/// (`DataKey::IndexPage(id, page_no)`) plus a single length key
/// (`DataKey::IndexLen(id)`), so no individual storage entry grows unboundedly
/// with protocol usage.  Page count is derived from the length
/// (`ceil(len / INDEX_PAGE_SIZE)`), so no separate page-count key is needed.
#[contracttype]
#[derive(Clone)]
pub enum IndexId {
    /// Global set of currently-active listing ids (supports swap-removal).
    ActiveListings,
    /// All listing ids ever created by an artist (append-only).
    ArtistListings(Address),
    /// All auction ids ever created by an artist (append-only).
    ArtistAuctions(Address),
    /// All offer ids ever made by an offerer (append-only).
    OffererOffers(Address),
    /// All offer ids ever made on a listing (append-only).
    ListingOffers(u64),
    /// All token addresses ever added to the whitelist (append-only, Issue #208).
    /// Used for pagination through the token registry. Includes both active and
    /// removed tokens to maintain full audit trail.
    TokenWhitelist,
}

/// A pending two-step admin rotation.
///
/// Stored under `DataKey::PendingAdmin` between `transfer_admin` (propose) and
/// `accept_admin`.  `expires_at` is an absolute ledger timestamp (seconds); once
/// `env.ledger().timestamp()` passes it, `accept_admin` reverts with
/// `AdminProposalExpired` so a proposal can never leave governance half-locked
/// forever.  The current admin can clear a live proposal early via
/// `cancel_admin_proposal`.
#[contracttype]
#[derive(Clone)]
pub struct PendingAdminProposal {
    /// Address invited to become the new admin.
    pub candidate: Address,
    /// Absolute ledger timestamp after which the proposal can no longer be
    /// accepted.
    pub expires_at: u64,
}

/// A pending two-step role-authority rotation (Issue #267).
///
/// Stored under `DataKey::PendingRole(role)` between `propose_role_transfer`
/// and `accept_role_transfer`, mirroring [`PendingAdminProposal`]'s semantics:
/// `expires_at` bounds how long a proposal can sit unaccepted, and the current
/// holder can clear it early via `cancel_role_proposal`.
#[contracttype]
#[derive(Clone)]
pub struct PendingRoleProposal {
    /// Address invited to become the new holder of the role.
    pub candidate: Address,
    /// Absolute ledger timestamp after which the proposal can no longer be
    /// accepted.
    pub expires_at: u64,
}

/// Resumable progress marker for a versioned storage migration.
#[contracttype]
#[derive(Clone)]
pub struct MigrationProgress {
    /// Which migration phase is in progress (see `contract::migrate_step`).
    pub phase: u32,
    /// Position within the phase (last fully-processed item id/index).
    pub cursor: u64,
}

/// Resumable progress marker for the periodic `contract::extend_active_ttls`
/// maintenance sweep (Issue #280).
///
/// Phase 0 walks the `ActiveListings` index (`cursor` = logical position);
/// phase 1 walks the sequential auction id space `1..=AuctionCount`
/// (`cursor` = last-processed auction id). Once phase 1 completes the sweep
/// wraps back to phase 0 rather than stopping — unlike `MigrationProgress`
/// this is not expected to ever "finish": TTL upkeep for the live record set
/// is an ongoing operational task, so as long as at least one listing or
/// auction is Active, later calls will always find something to refresh.
#[contracttype]
#[derive(Clone)]
pub struct TtlSweepProgress {
    pub phase: u32,
    pub cursor: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    ListingCount,
    Listing(u64),
    /// LEGACY (pre-1.1.0): monolithic `Vec<u64>` per-artist listing index.
    /// Superseded by `IndexPage(IndexId::ArtistListings(..), _)`; only read by
    /// `migrate` and removed once migrated.
    ArtistListings(Address),
    Admin,
    /// LEGACY (pre-1.1.0): flat Vec<Address> whitelist. Superseded by
    /// TokenWhitelistEntry(Address) map for O(1) lookups and history tracking.
    /// Only read by migration; removed once migrated.
    TokenWhitelist,
    /// Token whitelist registry entry (Issue #208). Maps token address to
    /// TokenWhitelistEntry with history tracking.
    TokenWhitelistEntry(Address),
    /// Total count of tokens ever added to the whitelist (Issue #208).
    TokenWhitelistCount,
    /// Maps registry ID to token address for pagination (Issue #208).
    TokenWhitelistId(u64),
    Treasury,
    ProtocolFeeBps,
    AuctionCount,
    Auction(u64),
    /// LEGACY (pre-1.1.0): monolithic per-artist auction index (see above).
    ArtistAuctions(Address),
    RevokedArtist(Address),
    OfferCount,
    Offer(u64),
    /// LEGACY (pre-1.1.0): monolithic per-listing offer index (see above).
    ListingOffers(u64),
    /// LEGACY (pre-1.1.0): monolithic per-offerer offer index (see above).
    OffererOffers(Address),
    ListingLock(u64),
    AuctionLock(u64),
    IsPaused,
    /// Per-collection pause flag. When present, operations on this collection
    /// are blocked regardless of the global pause state.
    CollectionPaused(Address),
    /// Per-function pause flag. Stored as a Symbol key; when present, the named
    /// entry-point is blocked regardless of global or collection pause state.
    FunctionPaused(soroban_sdk::Symbol),
    PendingAdmin,
    /// LEGACY (pre-1.1.0): monolithic active-listings index (see above).
    ActiveListings,
    MinBidIncrement,
    AuctionExtensionWindow,
    AuctionExtensionTrigger,
    AuctionBids(u64),
    MinPrice,
    MaxPrice,
    MigrationDone(soroban_sdk::String),
    /// Global admin-configurable bid-history ring-buffer capacity.
    /// Default: 50.  Valid range: 1 – 200.
    /// Each new auction snapshots this value into `Auction::bid_history_cap`
    /// so changes here never affect in-progress auctions.
    BidHistoryCap,
    /// Global cap on the number of times any single auction's end time may be
    /// extended by anti-sniping logic.  0 = unlimited (legacy behaviour).
    /// Each new auction snapshots this value into `Auction::max_extensions`.
    AuctionMaxExtensions,
    /// One fixed-capacity page (`Vec<u64>`, at most `INDEX_PAGE_SIZE` entries)
    /// of the identified index.
    IndexPage(IndexId, u32),
    /// Total number of elements stored across all pages of the index.
    IndexLen(IndexId),
    /// Current position of an active listing inside the ActiveListings index,
    /// enabling O(1) swap-removal.  Exists iff the listing is in the index.
    ActiveListingPos(u64),
    /// Bounded (≤ MAX_OFFERS_PER_LISTING) list of the listing's *Pending* offer
    /// ids.  Its length is the pending-offer counter used by `make_offer` for
    /// O(1) cap enforcement; entries are removed on every terminal transition.
    ListingPendingOffers(u64),
    /// Resume position for the batched `cancel_artist_listings` operation:
    /// number of entries of the artist-listings index already processed.
    ArtistCancelCursor(Address),
    /// Resume position for the batched auction cancellation on revocation
    /// (Issue #214): number of entries of the artist-auctions index processed.
    ArtistAuctionCancelCursor(Address),
    /// Resumable progress of the versioned `migrate`/`migrate_step` operation.
    MigrationCursor(soroban_sdk::String),
    /// Escrow record for a `(collection, token_id)` currently held in
    /// marketplace custody.  Exists iff the token is escrowed for an active
    /// listing or auction; a double-listing guard reads it and settlement /
    /// cancellation clears it.
    EscrowedToken(Address, u64),
    /// Bounded (≤ MAX_BLOCKED_BIDDERS) list of addresses barred from bidding
    /// on this auction (anti-shill-bidding registry, Issue #199).  Kept as a
    /// separate per-auction key — not a field on `Auction` — so auctions that
    /// never block anyone pay no extra storage.
    AuctionBlockedBidders(u64),
    /// Explicit holder of one of the four role axes (Issue #267). When absent
    /// for a given role, the contract Admin is the fallback holder.
    Role(crate::types::RoleType),
    /// Pending two-step role-transfer proposal (Issue #267). Mirrors
    /// `PendingAdmin` semantics with a per-role key.
    PendingRole(crate::types::RoleType),
    /// Lifetime protocol-fee total collected, keyed by payment token address
    /// (Issue #279). Monotonically increasing; never reset.
    ProtocolFeeTotal(Address),
    /// Lifetime royalty-settlement total, keyed by payment token address
    /// (Issue #279). Accumulates the gross sale value of every settlement.
    RoyaltyTotal(Address),
    /// Lifetime settlement count, keyed by payment token address (Issue #279).
    SettlementCount(Address),
    /// Resumable progress cursor for the `extend_active_ttls` TTL-sweep
    /// maintenance loop (Issue #280).
    TtlSweepState,
    /// Per-collection protocol fee override in basis points (Issue #322).
    /// When present, new listings/auctions for this collection snapshot this
    /// value instead of the global protocol fee.
    CollectionFeeBps(Address),
    /// Operator-readable marker recording whether the migration for a given
    /// version was interrupted (has a cursor) or completed cleanly. Stored
    /// alongside `MigrationCursor`/`MigrationDone` to support the
    /// `get_migration_status` view.
    MigrationStuck(soroban_sdk::String),
    /// Pending two-step treasury rotation proposal (Issue #459).
    /// Mirrors `PendingAdmin` semantics: written by `propose_treasury`,
    /// consumed (or cleared) by `accept_treasury` / `cancel_treasury_proposal`.
    PendingTreasury,
    /// Global minimum listing duration in seconds (Issue #460).
    /// When set, `expires_at` must be at least `now + MinListingDuration`.
    MinListingDuration,
    /// Global maximum listing duration in seconds (Issue #460).
    /// When set, `expires_at` must be at most `now + MaxListingDuration`.
    MaxListingDuration,
    /// Per-recipient royalty claim record (Issue #461).
    /// Keyed by (settlement_id, is_listing, recipient_address).
    /// Written at settlement, marked claimed after successful payout.
    RoyaltyClaim(u64, bool, Address),
    /// Ordered index of claim refs for a recipient (Issue #842).
    /// Enables `get_unclaimed_royalties` pagination without a full scan.
    RecipientRoyaltyIndex(Address),
    /// Parent offer ID for a counter-offer (Issue #471).
    /// `CounterOfferParent(counter_offer_id)` → parent_offer_id.
    CounterOfferParent(u64),
    /// Counter-offer ID that superseded a parent offer (Issue #471).
    /// `OfferSupersededBy(parent_offer_id)` → counter_offer_id.
    OfferSupersededBy(u64),
    /// Monotonically increasing counter for governance proposal IDs (Issue #472).
    GovernanceProposalCount,
    /// One governance proposal record (Issue #472).
    GovernanceProposal(u64),
    /// Ordered list of approver addresses for a governance proposal (Issue #472).
    GovernanceApprovals(u64),
    /// Resumable cursor for the `cleanup_terminal_records` permissionless sweep
    /// (Issue #474). Stores the last listing-id and offer-id processed so the
    /// sweep can be interrupted and resumed safely.
    TerminalCleanupCursor,
    /// Claim marker for an auction-losing-bid refund (Issue #466). Written
    /// before the refund transfer so duplicate `refund_losing_bid` claims
    /// cannot replay a payout; cleared version lives for the offer TTL.
    BidRefundRecord(u64, Address),
}

/// Custody record for an NFT held by the marketplace, keyed by
/// `DataKey::EscrowedToken(collection, token_id)`.  Written by `escrow_nft`
/// and removed by `release_nft`.
#[contracttype]
#[derive(Clone)]
pub struct EscrowRecord {
    /// True if the token backs a fixed-price listing; false for an auction.
    pub is_listing: bool,
    /// The listing_id or auction_id the escrow is bound to.
    pub id: u64,
}

/// Registry entry for a payment token that has been (or was previously)
/// whitelisted for use in listings, auctions, and offers (Issue #208).
///
/// Stored under `DataKey::TokenWhitelistEntry(token)`.  `active = false` is a
/// soft-delete: the historical record is preserved for audit, but the token
/// is no longer accepted for new operations.
#[contracttype]
#[derive(Clone, Debug)]
pub struct TokenWhitelistEntry {
    /// Ledger timestamp at which the token was first whitelisted.
    pub added_at: u64,
    /// Address that performed the `add_token_to_whitelist` call.
    pub added_by: Address,
    /// `true` while the token is actively accepted; `false` after soft-removal.
    pub active: bool,
}

/// Operator-facing summary of the current migration state for one contract
/// version. Returned by `MarketplaceContract::get_migration_status`.
#[contracttype]
#[derive(Clone)]
pub struct MigrationStatus {
    /// The contract version this status describes.
    pub version: soroban_sdk::String,
    /// `true` when the migration for this version completed successfully.
    pub is_done: bool,
    /// `true` when a cursor exists but `MigrationDone` has not been set —
    /// i.e. the migration was started but not yet finished.
    pub is_in_progress: bool,
    /// `true` when `migrate_step` has been called at least once but the
    /// migration was not completed in a single invocation (was interrupted).
    /// Distinct from `is_in_progress`: the stuck marker persists across
    /// calls even if the cursor advanced; it is only cleared on completion.
    pub is_stuck: bool,
    /// Current phase (0-based; meaningful only when `is_in_progress = true`).
    pub phase: u32,
    /// Current cursor within the active phase.
    pub cursor: u64,
}

// ── TTL Constants (Issue #280) ───────────────────────────────────────────────────
//
// These constants define the Time-To-Live (TTL) for different storage entry types.
// Values are in ledger numbers (assuming ~5s per ledger on mainnet).
//
// Safety buffer: 30 days = 518,400 ledgers
// Maximum listing duration: 90 days = 1,555,200 ledgers
// Maximum auction duration: 30 days = 518,400 ledgers
// Maximum offer duration: 30 days = 518,400 ledgers
//
// LISTING_TTL_LEDGERS: 90 days max + 30 days buffer = 120 days = 2,073,600 ledgers
// AUCTION_TTL_LEDGERS: 30 days max + 30 days buffer = 60 days = 1,036,800 ledgers
// OFFER_TTL_LEDGERS: 30 days max + 30 days buffer = 60 days = 1,036,800 ledgers
// INSTANCE_TTL_LEDGERS: Long-lived contract state = 365 days = 6,307,200 ledgers

pub const LISTING_TTL_LEDGERS: u32 = 2_073_600;  // 120 days
pub const AUCTION_TTL_LEDGERS: u32 = 1_036_800;   // 60 days
pub const OFFER_TTL_LEDGERS: u32 = 1_036_800;     // 60 days
pub const INSTANCE_TTL_LEDGERS: u32 = 6_307_200;  // 365 days

// Legacy TTL constants (retained for backward compatibility)
pub const LEDGER_TTL_BUMP: u32 = 432_000;
pub const LEDGER_TTL_THRESHOLD: u32 = 144_000;
pub const REENTRANCY_LOCK_TTL: u32 = 100;

pub fn bump_entry_ttl(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, LEDGER_TTL_THRESHOLD, LEDGER_TTL_BUMP);
}

// ── Type-specific TTL bump helpers (Issue #280) ─────────────────────────────────

/// Bump TTL for a listing entry using LISTING_TTL_LEDGERS
pub fn bump_listing_ttl(env: &Env, listing_id: u64) {
    let key = DataKey::Listing(listing_id);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, LISTING_TTL_LEDGERS);
}

/// Bump TTL for an auction entry using AUCTION_TTL_LEDGERS
pub fn bump_auction_ttl(env: &Env, auction_id: u64) {
    let key = DataKey::Auction(auction_id);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, AUCTION_TTL_LEDGERS);
}

/// Bump TTL for an offer entry using OFFER_TTL_LEDGERS
pub fn bump_offer_ttl(env: &Env, offer_id: u64) {
    let key = DataKey::Offer(offer_id);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, OFFER_TTL_LEDGERS);
}

/// Bump TTL for instance storage (contract-level state) using INSTANCE_TTL_LEDGERS
pub fn bump_instance_ttl(env: &Env) {
    let instance_keys: [DataKey; 12] = [
        DataKey::Admin,
        DataKey::Treasury,
        DataKey::ProtocolFeeBps,
        DataKey::TokenWhitelist,
        DataKey::IsPaused,
        DataKey::MinBidIncrement,
        DataKey::AuctionExtensionWindow,
        DataKey::AuctionExtensionTrigger,
        DataKey::MinPrice,
        DataKey::MaxPrice,
        DataKey::BidHistoryCap,
        DataKey::AuctionMaxExtensions,
    ];
    for key in instance_keys {
        if env.storage().persistent().has(&key) {
            env.storage()
                .persistent()
                .extend_ttl(&key, LEDGER_TTL_THRESHOLD, INSTANCE_TTL_LEDGERS);
        }
    }
}

// ── Paged index engine ───────────────────────────────────────
//
// Each `IndexId` collection is a sequence of fixed-capacity pages.  Element
// `i` lives in page `i / INDEX_PAGE_SIZE` at offset `i % INDEX_PAGE_SIZE`.
// Appending touches only the last page; swap-removal touches at most the
// page holding the removed slot plus the last page.  Emptied pages are
// deleted so dead keys do not accumulate.

/// Maximum number of ids held by one index page.
pub const INDEX_PAGE_SIZE: u32 = 100;

fn index_page_key(id: &IndexId, page: u32) -> DataKey {
    DataKey::IndexPage(id.clone(), page)
}

fn index_len_key(id: &IndexId) -> DataKey {
    DataKey::IndexLen(id.clone())
}

/// Total number of elements in the index.
pub fn index_len(env: &Env, id: &IndexId) -> u32 {
    let key = index_len_key(id);
    let len = env
        .storage()
        .persistent()
        .get::<DataKey, u32>(&key)
        .unwrap_or(0);
    // Keep the length entry alive alongside its pages — it is read on every
    // index access, making it the hottest entry of the index.
    if len > 0 {
        bump_entry_ttl(env, &key);
    }
    len
}

fn set_index_len(env: &Env, id: &IndexId, len: u32) {
    let key = index_len_key(id);
    if len == 0 {
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, &len);
        bump_entry_ttl(env, &key);
    }
}

/// Load one page of the index (empty vec if the page does not exist).
pub fn index_load_page(env: &Env, id: &IndexId, page: u32) -> Vec<u64> {
    let key = index_page_key(id, page);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<u64>>(&key)
        .unwrap_or_else(|| Vec::new(env));
    if !value.is_empty() {
        bump_entry_ttl(env, &key);
    }
    value
}

fn index_store_page(env: &Env, id: &IndexId, page: u32, entries: &Vec<u64>) {
    let key = index_page_key(id, page);
    if entries.is_empty() {
        // Dead pages are removed as soon as they empty out.
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, entries);
        bump_entry_ttl(env, &key);
    }
}

/// Append `value` to the end of the index. O(1): reads/writes only the last
/// page and the length key.
pub fn index_append(env: &Env, id: &IndexId, value: u64) {
    let len = index_len(env, id);
    let page = len / INDEX_PAGE_SIZE;
    let mut entries = index_load_page(env, id, page);
    entries.push_back(value);
    index_store_page(env, id, page, &entries);
    set_index_len(env, id, len + 1);
}

/// Read the element at logical position `pos`, or `None` when out of range.
pub fn index_get(env: &Env, id: &IndexId, pos: u32) -> Option<u64> {
    if pos >= index_len(env, id) {
        return None;
    }
    index_load_page(env, id, pos / INDEX_PAGE_SIZE).get(pos % INDEX_PAGE_SIZE)
}

/// Remove the element at logical position `pos` by moving the last element of
/// the index into its slot (swap-remove).  Returns `Some(moved_value)` when an
/// element was relocated into `pos`, `None` when `pos` was the last element.
///
/// NOTE: this deliberately does not preserve insertion order — the caller must
/// treat the index as an unordered set once removals occur.
pub fn index_swap_remove(env: &Env, id: &IndexId, pos: u32) -> Option<u64> {
    let len = index_len(env, id);
    if pos >= len {
        return None;
    }
    let last = len - 1;
    let last_page_no = last / INDEX_PAGE_SIZE;
    let last_off = last % INDEX_PAGE_SIZE;
    let mut last_page = index_load_page(env, id, last_page_no);
    let last_val = last_page.get(last_off).unwrap();

    let moved = if pos == last {
        last_page.remove(last_off);
        index_store_page(env, id, last_page_no, &last_page);
        None
    } else {
        let pos_page_no = pos / INDEX_PAGE_SIZE;
        let pos_off = pos % INDEX_PAGE_SIZE;
        if pos_page_no == last_page_no {
            last_page.set(pos_off, last_val);
            last_page.remove(last_off);
            index_store_page(env, id, last_page_no, &last_page);
        } else {
            last_page.remove(last_off);
            index_store_page(env, id, last_page_no, &last_page);
            let mut pos_page = index_load_page(env, id, pos_page_no);
            pos_page.set(pos_off, last_val);
            index_store_page(env, id, pos_page_no, &pos_page);
        }
        Some(last_val)
    };
    set_index_len(env, id, last);
    moved
}

/// Read up to `limit` elements starting at logical position `start`.
/// Positions past the end yield an empty vector.
pub fn index_range(env: &Env, id: &IndexId, start: u32, limit: u32) -> Vec<u64> {
    let mut out = Vec::new(env);
    let len = index_len(env, id);
    if start >= len || limit == 0 {
        return out;
    }
    let end = start.saturating_add(limit).min(len);
    let mut page_no = start / INDEX_PAGE_SIZE;
    let mut entries = index_load_page(env, id, page_no);
    for pos in start..end {
        let p = pos / INDEX_PAGE_SIZE;
        if p != page_no {
            page_no = p;
            entries = index_load_page(env, id, page_no);
        }
        out.push_back(entries.get(pos % INDEX_PAGE_SIZE).unwrap());
    }
    out
}

/// Read the whole index.  Unbounded in the number of pages — reserved for
/// view functions and tests; transaction paths must use `index_range`.
pub fn index_all(env: &Env, id: &IndexId) -> Vec<u64> {
    index_range(env, id, 0, index_len(env, id))
}

/// Check whether `value` is present anywhere in the index (O(n) page scan).
///
/// Reserved for migration postcondition assertions and tests — do not call
/// from normal transaction paths where the cost would be unbounded.
pub fn index_contains(env: &Env, id: &IndexId, value: u64) -> bool {
    let len = index_len(env, id);
    if len == 0 {
        return false;
    }
    let page_count = (len + INDEX_PAGE_SIZE - 1) / INDEX_PAGE_SIZE;
    for p in 0..page_count {
        if index_load_page(env, id, p).contains(value) {
            return true;
        }
    }
    false
}

/// Return the number of pages currently allocated for the index.
/// Consistent with `ceil(index_len / INDEX_PAGE_SIZE)`.
pub fn index_page_count(env: &Env, id: &IndexId) -> u32 {
    let len = index_len(env, id);
    if len == 0 { 0 } else { (len + INDEX_PAGE_SIZE - 1) / INDEX_PAGE_SIZE }
}

// ── Counters ─────────────────────────────────────────────────

pub fn get_listing_count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::ListingCount)
        .unwrap_or(0)
}

pub fn increment_listing_count(env: &Env) -> u64 {
    let count = get_listing_count(env) + 1;
    env.storage()
        .persistent()
        .set(&DataKey::ListingCount, &count);
    bump_entry_ttl(env, &DataKey::ListingCount);
    count
}

pub fn get_auction_count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::AuctionCount)
        .unwrap_or(0)
}

pub fn increment_auction_count(env: &Env) -> u64 {
    let count = get_auction_count(env) + 1;
    env.storage()
        .persistent()
        .set(&DataKey::AuctionCount, &count);
    bump_entry_ttl(env, &DataKey::AuctionCount);
    count
}

pub fn get_offer_count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::OfferCount)
        .unwrap_or(0)
}

pub fn increment_offer_count(env: &Env) -> u64 {
    let count = get_offer_count(env) + 1;
    env.storage().persistent().set(&DataKey::OfferCount, &count);
    bump_entry_ttl(env, &DataKey::OfferCount);
    count
}

// ── Accounting counters (Issue #279) ─────────────────────────
//
// On-chain, per-payment-token totals so operators/creators/indexers can
// reconcile expected fees and royalties against actual transfers without
// relying solely on off-chain event aggregation.
//
// Design (deliberately simple — see docs/guides/accounting-reconciliation.md
// for the full rationale):
//   • Lifetime, monotonic, non-resettable totals. Never reset, never
//     decremented — the simplest policy, hardest to game, and it matches
//     "cannot be manipulated by failed transactions" (Soroban transactions
//     are atomic, so a panic anywhere rolls back the whole invocation
//     including any counter bump that happened earlier in the same call —
//     these functions are only ever invoked *after* the corresponding token
//     transfer(s) have already succeeded, right alongside the existing
//     `ProtocolFeeCollectedEvent` / `RoyaltySettlementEvent` emissions).
//   • Keyed by payment token address only (not by recipient) — an unbounded
//     per-recipient breakdown would grow storage without bound as new
//     recipients appear; the existing `RoyaltySettlementEvent` snapshot
//     already carries the full per-recipient split for anyone who needs
//     finer granularity, so the on-chain counter intentionally stays a
//     per-token lifetime aggregate.
//   • `RoyaltyTotal` accumulates the same `total_amount` value that is
//     emitted on every `RoyaltySettlementEvent` (the gross settlement value:
//     listing price / winning bid / accepted offer amount), so an indexer
//     can reconcile by summing `RoyaltySettlementEvent.total_amount` grouped
//     by token and comparing against `get_royalty_total(token)`.
//   • `ProtocolFeeTotal` accumulates the same `amount` emitted on every
//     `ProtocolFeeCollectedEvent`.
//   • `SettlementCount` increments once per successful settlement (one per
//     `RoyaltySettlementEvent` emission), regardless of whether a protocol
//     fee was actually collected on that settlement.

pub fn get_protocol_fee_total(env: &Env, token: &Address) -> i128 {
    let key = DataKey::ProtocolFeeTotal(token.clone());
    let value = env.storage().persistent().get::<DataKey, i128>(&key).unwrap_or(0);
    if value != 0 {
        bump_entry_ttl(env, &key);
    }
    value
}

/// Add `amount` to the lifetime protocol-fee total for `token`. No-op when
/// `amount <= 0` (fee collection never subtracts). Panics with
/// `ArithmeticOverflow` on i128 overflow (practically unreachable given real
/// token supplies, kept for defense-in-depth consistency with the rest of
/// the contract's checked-arithmetic style).
pub fn add_protocol_fee_total(env: &Env, token: &Address, amount: i128) {
    if amount <= 0 {
        return;
    }
    let key = DataKey::ProtocolFeeTotal(token.clone());
    let current = get_protocol_fee_total(env, token);
    let updated = current
        .checked_add(amount)
        .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::ArithmeticOverflow));
    env.storage().persistent().set(&key, &updated);
    bump_entry_ttl(env, &key);
}

pub fn get_royalty_total(env: &Env, token: &Address) -> i128 {
    let key = DataKey::RoyaltyTotal(token.clone());
    let value = env.storage().persistent().get::<DataKey, i128>(&key).unwrap_or(0);
    if value != 0 {
        bump_entry_ttl(env, &key);
    }
    value
}

/// Add `amount` to the lifetime royalty-settlement total for `token`. No-op
/// when `amount <= 0`.
pub fn add_royalty_total(env: &Env, token: &Address, amount: i128) {
    if amount <= 0 {
        return;
    }
    let key = DataKey::RoyaltyTotal(token.clone());
    let current = get_royalty_total(env, token);
    let updated = current
        .checked_add(amount)
        .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::ArithmeticOverflow));
    env.storage().persistent().set(&key, &updated);
    bump_entry_ttl(env, &key);
}

pub fn get_settlement_count(env: &Env, token: &Address) -> u64 {
    let key = DataKey::SettlementCount(token.clone());
    let value = env.storage().persistent().get::<DataKey, u64>(&key).unwrap_or(0);
    if value != 0 {
        bump_entry_ttl(env, &key);
    }
    value
}

/// Increment the lifetime settlement count for `token` by one and return the
/// new value.
pub fn increment_settlement_count(env: &Env, token: &Address) -> u64 {
    let key = DataKey::SettlementCount(token.clone());
    let count = get_settlement_count(env, token)
        .checked_add(1)
        .unwrap_or_else(|| panic_with_error!(env, MarketplaceError::ArithmeticOverflow));
    env.storage().persistent().set(&key, &count);
    bump_entry_ttl(env, &key);
    count
}

// ── CRUD ─────────────────────────────────────────────────────

pub fn save_listing(env: &Env, listing: &Listing) {
    let key = DataKey::Listing(listing.listing_id);
    env.storage().persistent().set(&key, listing);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, LISTING_TTL_LEDGERS);
}

pub fn load_listing(env: &Env, listing_id: u64) -> Option<Listing> {
    let key = DataKey::Listing(listing_id);
    let res = env.storage().persistent().get::<DataKey, Listing>(&key);
    if res.is_some() {
        bump_entry_ttl(env, &key);
    }
    res
}

pub fn save_auction(env: &Env, auction: &Auction) {
    let key = DataKey::Auction(auction.auction_id);
    env.storage().persistent().set(&key, auction);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, AUCTION_TTL_LEDGERS);
}

pub fn load_auction(env: &Env, auction_id: u64) -> Option<Auction> {
    let key = DataKey::Auction(auction_id);
    let res = env.storage().persistent().get::<DataKey, Auction>(&key);
    if res.is_some() {
        bump_entry_ttl(env, &key);
    }
    res
}

pub fn save_offer(env: &Env, offer: &Offer) {
    let key = DataKey::Offer(offer.offer_id);
    env.storage().persistent().set(&key, offer);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, OFFER_TTL_LEDGERS);
}

pub fn load_offer(env: &Env, offer_id: u64) -> Option<Offer> {
    let key = DataKey::Offer(offer_id);
    let res = env.storage().persistent().get::<DataKey, Offer>(&key);
    if res.is_some() {
        bump_entry_ttl(env, &key);
    }
    res
}

// ── Lifecycle state machine ──────────────────────────────────
//
// Every listing, auction, and offer moves through a small number of explicit
// states. This table is the single source of truth for which transitions are
// legal; all marketplace entry points must consult it before mutating a
// record. Terminal states have no outgoing transitions; the cross-entity
// cleanup helpers below centralize the side effects that accompany a
// terminal listing or auction transition.
/// Entity whose lifecycle is being validated.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleEntity {
    Listing,
    Auction,
    Offer,
}

/// Canonical states shared by listings, auctions, and offers.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    Active,
    Pending,
    Sold,
    Finalized,
    Cancelled,
    Accepted,
    Rejected,
    Withdrawn,
    Superseded,
}

/// Explicit transition metadata.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleTransition {
    pub entity: LifecycleEntity,
    pub from: LifecycleState,
    pub to: LifecycleState,
}

/// Check whether `transition` is one of the allowed lifecycle moves.
pub fn is_lifecycle_transition_allowed(transition: &LifecycleTransition) -> bool {
    match (transition.entity, transition.from, transition.to) {
        (LifecycleEntity::Listing, LifecycleState::Active, LifecycleState::Sold) => true,
        (LifecycleEntity::Listing, LifecycleState::Active, LifecycleState::Cancelled) => true,
        (LifecycleEntity::Auction, LifecycleState::Active, LifecycleState::Finalized) => true,
        (LifecycleEntity::Auction, LifecycleState::Active, LifecycleState::Cancelled) => true,
        (LifecycleEntity::Offer, LifecycleState::Pending, LifecycleState::Accepted) => true,
        (LifecycleEntity::Offer, LifecycleState::Pending, LifecycleState::Rejected) => true,
        (LifecycleEntity::Offer, LifecycleState::Pending, LifecycleState::Withdrawn) => true,
        (LifecycleEntity::Offer, LifecycleState::Pending, LifecycleState::Superseded) => true,
        _ => false,
    }
}

/// Panic if `transition` is not allowed.
pub fn assert_lifecycle_transition_allowed(env: &Env, transition: &LifecycleTransition) {
    if !is_lifecycle_transition_allowed(transition) {
        panic_with_error!(env, MarketplaceError::InvalidStateTransition);
    }
}

/// Returns true for terminal states (no outgoing transitions).
pub fn is_terminal_lifecycle_state(state: &LifecycleState) -> bool {
    match *state {
        LifecycleState::Sold
        | LifecycleState::Finalized
        | LifecycleState::Cancelled
        | LifecycleState::Accepted
        | LifecycleState::Rejected
        | LifecycleState::Withdrawn
        | LifecycleState::Superseded => true,
        _ => false,
    }
}

/// Enumerate every allowed transition for exhaustive/property-style tests.
pub fn allowed_lifecycle_transitions(env: &Env) -> Vec<LifecycleTransition> {
    let mut transitions = Vec::new(env);
    for (entity, from, to) in [
        (LifecycleEntity::Listing, LifecycleState::Active, LifecycleState::Sold),
        (LifecycleEntity::Listing, LifecycleState::Active, LifecycleState::Cancelled),
        (LifecycleEntity::Auction, LifecycleState::Active, LifecycleState::Finalized),
        (LifecycleEntity::Auction, LifecycleState::Active, LifecycleState::Cancelled),
        (LifecycleEntity::Offer, LifecycleState::Pending, LifecycleState::Accepted),
        (LifecycleEntity::Offer, LifecycleState::Pending, LifecycleState::Rejected),
        (LifecycleEntity::Offer, LifecycleState::Pending, LifecycleState::Withdrawn),
        (LifecycleEntity::Offer, LifecycleState::Pending, LifecycleState::Superseded),
    ] {
        transitions.push_back(LifecycleTransition { entity, from, to });
    }
    transitions
}

/// Cross-entity side effects shared by every terminal listing transition.
pub fn clear_listing_cross_entity_state(
    env: &Env,
    listing_id: u64,
    collection: &Address,
    token_id: u64,
) {
    remove_from_active_listings(env, listing_id);
    clear_pending_offers(env, listing_id);
    clear_escrow_record(env, collection, token_id);
}

/// Cross-entity side effects shared by every terminal auction transition.
pub fn clear_auction_cross_entity_state(
    env: &Env,
    auction_id: u64,
    collection: &Address,
    token_id: u64,
) {
    clear_escrow_record(env, collection, token_id);
    let empty_bidders: Vec<Address> = Vec::new(env);
    save_blocked_bidders(env, auction_id, &empty_bidders);
}

// ── Indices (paged) ──────────────────────────────────────────

pub fn add_artist_listing_id(env: &Env, artist: &Address, listing_id: u64) {
    index_append(env, &IndexId::ArtistListings(artist.clone()), listing_id);
}

pub fn get_artist_listing_ids(env: &Env, artist: &Address) -> Vec<u64> {
    index_all(env, &IndexId::ArtistListings(artist.clone()))
}

// ── Active listings index ────────────────────────────────────
//
// The only index that shrinks.  A per-listing position key
// (`ActiveListingPos`) makes removal O(1): read the position, swap the last
// element into the vacated slot, fix up the moved element's position key.
// Consequence (deliberate, documented): once removals occur, the index is an
// unordered set — pagination order is stable between removals but is no
// longer strict insertion order.

pub fn add_to_active_listings(env: &Env, listing_id: u64) {
    let idx = IndexId::ActiveListings;
    let pos_key = DataKey::ActiveListingPos(listing_id);
    // Idempotency guard: never double-insert an id already in the index.
    if env.storage().persistent().has(&pos_key) {
        return;
    }
    let pos = index_len(env, &idx);
    index_append(env, &idx, listing_id);
    env.storage().persistent().set(&pos_key, &pos);
    bump_entry_ttl(env, &pos_key);
}

pub fn remove_from_active_listings(env: &Env, listing_id: u64) {
    let idx = IndexId::ActiveListings;
    let pos_key = DataKey::ActiveListingPos(listing_id);
    let pos = match env.storage().persistent().get::<DataKey, u32>(&pos_key) {
        Some(p) => p,
        None => return, // not in the index — nothing to do
    };
    // Defensive consistency check: the slot must actually hold this id.
    if index_get(env, &idx, pos) != Some(listing_id) {
        return;
    }
    if let Some(moved) = index_swap_remove(env, &idx, pos) {
        let moved_key = DataKey::ActiveListingPos(moved);
        env.storage().persistent().set(&moved_key, &pos);
        bump_entry_ttl(env, &moved_key);
    }
    env.storage().persistent().remove(&pos_key);
}

pub fn active_listings_len(env: &Env) -> u32 {
    index_len(env, &IndexId::ActiveListings)
}

pub fn get_active_listing_ids_range(env: &Env, start: u32, limit: u32) -> Vec<u64> {
    index_range(env, &IndexId::ActiveListings, start, limit)
}

/// Whole active index — used by tests and migration assertions only; the
/// contract's read surface pages through `get_active_listing_ids_range`.
#[allow(dead_code)]
pub fn get_active_listing_ids(env: &Env) -> Vec<u64> {
    index_all(env, &IndexId::ActiveListings)
}

pub fn add_artist_auction_id(env: &Env, artist: &Address, auction_id: u64) {
    index_append(env, &IndexId::ArtistAuctions(artist.clone()), auction_id);
}

pub fn get_artist_auction_ids(env: &Env, artist: &Address) -> Vec<u64> {
    index_all(env, &IndexId::ArtistAuctions(artist.clone()))
}

pub fn add_listing_offer_id(env: &Env, listing_id: u64, offer_id: u64) {
    index_append(env, &IndexId::ListingOffers(listing_id), offer_id);
}

pub fn load_listing_offers(env: &Env, listing_id: u64) -> Vec<u64> {
    index_all(env, &IndexId::ListingOffers(listing_id))
}

pub fn add_offerer_offer_id(env: &Env, offerer: &Address, offer_id: u64) {
    index_append(env, &IndexId::OffererOffers(offerer.clone()), offer_id);
}

pub fn load_offerer_offers(env: &Env, offerer: &Address) -> Vec<u64> {
    index_all(env, &IndexId::OffererOffers(offerer.clone()))
}

// ── Pending-offer tracking ───────────────────────────────────
//
// A single bounded entry per listing (≤ MAX_OFFERS_PER_LISTING ids).  Its
// length is the pending-offer counter: `make_offer` enforces the cap with one
// storage read instead of loading every historical offer.  Every terminal
// transition (accept / reject / withdraw / reclaim / auto-reject during
// buy_artwork or cancellation) removes the offer id here, and the refund
// sweeps iterate this bounded list instead of the full per-listing history.

pub fn load_pending_offer_ids(env: &Env, listing_id: u64) -> Vec<u64> {
    let key = DataKey::ListingPendingOffers(listing_id);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<u64>>(&key)
        .unwrap_or_else(|| Vec::new(env));
    if !value.is_empty() {
        bump_entry_ttl(env, &key);
    }
    value
}

/// Number of currently-Pending offers on the listing (O(1) storage reads).
pub fn pending_offer_count(env: &Env, listing_id: u64) -> u32 {
    load_pending_offer_ids(env, listing_id).len()
}

pub fn add_pending_offer(env: &Env, listing_id: u64, offer_id: u64) {
    let key = DataKey::ListingPendingOffers(listing_id);
    let mut ids = load_pending_offer_ids(env, listing_id);
    ids.push_back(offer_id);
    env.storage().persistent().set(&key, &ids);
    bump_entry_ttl(env, &key);
}

/// Remove `offer_id` from the listing's pending set.  No-op when absent (e.g.
/// offers created before the 1.1.0 migration ran).  The entry is deleted when
/// the last pending offer leaves.
pub fn remove_pending_offer(env: &Env, listing_id: u64, offer_id: u64) {
    let key = DataKey::ListingPendingOffers(listing_id);
    let ids = load_pending_offer_ids(env, listing_id);
    if let Some(i) = ids.first_index_of(offer_id) {
        let mut updated = ids;
        updated.remove(i);
        if updated.is_empty() {
            env.storage().persistent().remove(&key);
        } else {
            env.storage().persistent().set(&key, &updated);
            bump_entry_ttl(env, &key);
        }
    }
}

/// Drop the whole pending set (used when a listing reaches a terminal state
/// and all its pending offers were swept in the same invocation).
pub fn clear_pending_offers(env: &Env, listing_id: u64) {
    env.storage()
        .persistent()
        .remove(&DataKey::ListingPendingOffers(listing_id));
}

// ── NFT escrow records ───────────────────────────────────────
//
// One persistent entry per `(collection, token_id)` in marketplace custody.
// `escrow_nft` writes it (after a double-listing guard read); `release_nft`
// removes it once the token leaves custody.

pub fn get_escrow_record(env: &Env, collection: &Address, token_id: u64) -> Option<EscrowRecord> {
    let key = DataKey::EscrowedToken(collection.clone(), token_id);
    let res = env.storage().persistent().get::<DataKey, EscrowRecord>(&key);
    if res.is_some() {
        bump_entry_ttl(env, &key);
    }
    res
}

pub fn set_escrow_record(env: &Env, collection: &Address, token_id: u64, record: &EscrowRecord) {
    let key = DataKey::EscrowedToken(collection.clone(), token_id);
    env.storage().persistent().set(&key, record);
    bump_entry_ttl(env, &key);
}

pub fn clear_escrow_record(env: &Env, collection: &Address, token_id: u64) {
    env.storage()
        .persistent()
        .remove(&DataKey::EscrowedToken(collection.clone(), token_id));
}

// ── Token whitelist registry (Issue #208) ───────────────────
//
// Replaces the flat Vec whitelist with a map-based registry for O(1) lookups
// and full audit trail. Each token address maps to a TokenWhitelistEntry.

/// Check if a token is currently whitelisted (active: true). O(1) lookup.
/// When no tokens have ever been added (count == 0), all tokens are allowed.
pub fn is_token_whitelisted(env: &Env, token: &Address) -> bool {
    if get_token_whitelist_count(env) == 0 {
        return true;
    }
    let key = DataKey::TokenWhitelistEntry(token.clone());
    let entry = env.storage().persistent().get::<DataKey, TokenWhitelistEntry>(&key);
    match entry {
        Some(e) => {
            if e.active {
                bump_entry_ttl(env, &key);
                true
            } else {
                false
            }
        }
        None => false,
    }
}

/// Get the whitelist entry for a token, if it exists (active or removed).
pub fn get_token_whitelist_entry(env: &Env, token: &Address) -> Option<TokenWhitelistEntry> {
    let key = DataKey::TokenWhitelistEntry(token.clone());
    let entry = env.storage().persistent().get::<DataKey, TokenWhitelistEntry>(&key);
    if entry.is_some() {
        bump_entry_ttl(env, &key);
    }
    entry
}

/// Save or update a token whitelist entry.
pub fn set_token_whitelist_entry(env: &Env, token: &Address, entry: &TokenWhitelistEntry) {
    let key = DataKey::TokenWhitelistEntry(token.clone());
    env.storage().persistent().set(&key, entry);
    bump_entry_ttl(env, &key);
}

/// Add a token address to the whitelist index (for pagination).
/// Called when a token is first whitelisted. Returns the token's registry ID.
pub fn add_token_to_whitelist_index(env: &Env, token: &Address) -> u64 {
    let count = get_token_whitelist_count(env) + 1;
    set_token_whitelist_count(env, count);
    // Store the mapping from ID to Address
    let key = DataKey::TokenWhitelistId(count);
    env.storage().persistent().set(&key, token);
    bump_entry_ttl(env, &key);
    // Add the ID to the index for pagination
    index_append(env, &IndexId::TokenWhitelist, count);
    count
}

/// Get the total number of tokens ever added to the whitelist.
pub fn get_token_whitelist_count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::TokenWhitelistCount)
        .unwrap_or(0)
}

/// Set the total number of tokens in the whitelist.
pub fn set_token_whitelist_count(env: &Env, count: u64) {
    env.storage().persistent().set(&DataKey::TokenWhitelistCount, &count);
    bump_entry_ttl(env, &DataKey::TokenWhitelistCount);
}

/// Get token address by its registry ID.
pub fn get_token_by_id(env: &Env, id: u64) -> Option<Address> {
    let key = DataKey::TokenWhitelistId(id);
    let token = env.storage().persistent().get::<DataKey, Address>(&key);
    if token.is_some() {
        bump_entry_ttl(env, &key);
    }
    token
}

/// Get paginated token registry entries (addresses only).
/// Returns up to `limit` token addresses starting at `offset`.
pub fn get_token_registry_range(env: &Env, start: u32, limit: u32) -> Vec<Address> {
    let ids = index_range(env, &IndexId::TokenWhitelist, start, limit);
    let mut tokens = Vec::new(env);
    for id in ids.iter() {
        if let Some(token) = get_token_by_id(env, id) {
            if is_token_whitelisted(env, &token) {
                tokens.push_back(token);
            }
        }
    }
    tokens
}

/// Get all ACTIVE token addresses in the registry (excludes soft-deleted entries).
pub fn get_all_token_registry(env: &Env) -> Vec<Address> {
    let ids = index_all(env, &IndexId::TokenWhitelist);
    let mut tokens = Vec::new(env);
    for id in ids.iter() {
        if let Some(token) = get_token_by_id(env, id) {
            if is_token_whitelisted(env, &token) {
                tokens.push_back(token);
            }
        }
    }
    tokens
}

/// Get the full audit trail (TokenWhitelistEntry) for a specific token.
/// Returns the entry if the token has ever been whitelisted (active or removed).
pub fn token_whitelist_history(env: &Env, token: &Address) -> Option<TokenWhitelistEntry> {
    get_token_whitelist_entry(env, token)
}

// ── Policy Engine (Issue #435) ───────────────────────────────────────────────
//
// A single, authoritative validation layer for token-whitelist state and price
// bounds. Every entry point that creates or settles a listing, auction, or offer
// must go through `check_token_policy` / `check_price_policy` rather than
// duplicating the individual storage reads inline.
//
// Design goals:
//   1. One place to audit: all whitelist and price semantics live here.
//   2. Explicit lifecycle states: Active / Removed / NeverAdded are distinct —
//      callers cannot accidentally treat a removed token as valid.
//   3. Stable output for the indexer: `TokenWhitelistPolicyResult` is a
//      `#[contracttype]` struct that can be returned from view functions so the
//      off-chain system can query policy state without re-implementing the logic.
//   4. Zero dead code: every variant of `TokenWhitelistState` is reachable by
//      the tests added for Issue #435.

/// Describes the current lifecycle state of a token in the whitelist registry.
///
/// Returned by `get_token_whitelist_policy` and inspected by
/// `assert_token_policy_active` to give the caller an explicit, unambiguous
/// verdict rather than a plain `bool`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenWhitelistState {
    /// Token is currently active in the whitelist and may be used in new
    /// listings, auctions, and offers.
    Active,
    /// Token was previously whitelisted but has been soft-removed by an admin.
    /// It is preserved in the registry for historical audit but must not be
    /// accepted for any new operations.
    Removed,
    /// Token has never been added to the whitelist.  When the whitelist is
    /// non-empty this means the token is rejected; when the whitelist is
    /// empty (count == 0) the contract allows any token (pass-all mode).
    NeverAdded,
}

/// XDR-safe wrapper around `Option<TokenWhitelistEntry>`.
///
/// Soroban SDK 25.3.0 cannot derive XDR serialization for
/// `Option<CustomContractType>` inside a `#[contracttype]` struct.
/// This enum carries the same semantics with helper methods that match the
/// `Option` API used in tests and the indexer.
#[contracttype]
#[derive(Clone, Debug)]
pub enum OptionalTokenWhitelistEntry {
    Entry(TokenWhitelistEntry),
    Empty,
}

impl OptionalTokenWhitelistEntry {
    pub fn is_none(&self) -> bool { matches!(self, Self::Empty) }
    pub fn is_some(&self) -> bool { matches!(self, Self::Entry(_)) }
    pub fn unwrap(self) -> TokenWhitelistEntry {
        match self {
            Self::Entry(e) => e,
            Self::Empty => panic!("called unwrap on OptionalTokenWhitelistEntry::Empty"),
        }
    }
}

/// Complete policy snapshot for one token address. Returned by the
/// `get_token_whitelist_policy` view so operators and the indexer can query
/// the full registry state in one call without reconstructing it client-side.
#[contracttype]
#[derive(Clone, Debug)]
pub struct TokenWhitelistPolicyResult {
    /// The queried token address.
    pub token: Address,
    /// Resolved lifecycle state (Active / Removed / NeverAdded).
    pub state: TokenWhitelistState,
    /// `true` when the token may currently be used in new operations.
    /// Equivalent to `state == Active || (state == NeverAdded && count == 0)`.
    pub is_accepted: bool,
    /// The full registry entry when the token has ever been whitelisted,
    /// `Empty` when `state == NeverAdded`.
    pub entry: OptionalTokenWhitelistEntry,
    /// Total number of tokens ever registered (monotonically increasing).
    /// Useful context for callers building pagination UIs.
    pub total_registered: u64,
}

/// Compute the complete policy snapshot for `token` without modifying state.
///
/// Callers use this for both enforcement and view purposes:
/// - Enforcement: `assert_token_policy_active` panics when `is_accepted` is false.
/// - View: `get_token_whitelist_policy` returns this struct directly.
pub fn evaluate_token_policy(env: &Env, token: &Address) -> TokenWhitelistPolicyResult {
    let total = get_token_whitelist_count(env);
    let entry = get_token_whitelist_entry(env, token);

    let (state, is_accepted) = match &entry {
        Some(e) if e.active => (TokenWhitelistState::Active, true),
        Some(_) => (TokenWhitelistState::Removed, false),
        None => {
            // Pass-all mode when no tokens have ever been registered.
            let accepted = total == 0;
            (TokenWhitelistState::NeverAdded, accepted)
        }
    };

    let entry_opt = match entry {
        Some(e) => OptionalTokenWhitelistEntry::Entry(e),
        None => OptionalTokenWhitelistEntry::Empty,
    };

    TokenWhitelistPolicyResult {
        token: token.clone(),
        state,
        is_accepted,
        entry: entry_opt,
        total_registered: total,
    }
}

/// Enforce that `token` is currently accepted by the policy engine.
///
/// Panics with `TokenNotWhitelisted` when:
/// - `state == Removed`  (token was soft-deleted by an admin)
/// - `state == NeverAdded` AND the whitelist is non-empty (at least one token
///   has been registered, so pass-all mode is inactive)
///
/// This is the single authoritative gate used at every write surface:
/// listing creation, auction creation, offer creation, and settlement.
pub fn assert_token_policy_active(env: &Env, token: &Address) {
    let result = evaluate_token_policy(env, token);
    if !result.is_accepted {
        panic_with_error!(env, MarketplaceError::TokenNotWhitelisted);
    }
}

/// Enforce that `price` satisfies the configured `[min, max]` price bounds.
///
/// Panics with `PriceOutOfBounds` when either bound is set and `price`
/// violates it.  A bound of `None` means unconfigured (unbounded in that
/// direction).
pub fn assert_price_policy(env: &Env, price: i128) {
    if let Some(min) = get_min_price_storage(env) {
        if price < min {
            panic_with_error!(env, MarketplaceError::PriceOutOfBounds);
        }
    }
    if let Some(max) = get_max_price_storage(env) {
        if price > max {
            panic_with_error!(env, MarketplaceError::PriceOutOfBounds);
        }
    }
}

// ── Batched cancel_artist_listings cursor ────────────────────

pub fn get_artist_cancel_cursor(env: &Env, artist: &Address) -> u32 {
    env.storage()
        .persistent()
        .get::<DataKey, u32>(&DataKey::ArtistCancelCursor(artist.clone()))
        .unwrap_or(0)
}

pub fn set_artist_cancel_cursor(env: &Env, artist: &Address, cursor: u32) {
    let key = DataKey::ArtistCancelCursor(artist.clone());
    env.storage().persistent().set(&key, &cursor);
    bump_entry_ttl(env, &key);
}

pub fn clear_artist_cancel_cursor(env: &Env, artist: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::ArtistCancelCursor(artist.clone()));
}

// ── Batched cancel_artist_auctions cursor (Issue #214) ───────

pub fn get_artist_auction_cancel_cursor(env: &Env, artist: &Address) -> u32 {
    env.storage()
        .persistent()
        .get::<DataKey, u32>(&DataKey::ArtistAuctionCancelCursor(artist.clone()))
        .unwrap_or(0)
}

pub fn set_artist_auction_cancel_cursor(env: &Env, artist: &Address, cursor: u32) {
    let key = DataKey::ArtistAuctionCancelCursor(artist.clone());
    env.storage().persistent().set(&key, &cursor);
    bump_entry_ttl(env, &key);
}

pub fn clear_artist_auction_cancel_cursor(env: &Env, artist: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::ArtistAuctionCancelCursor(artist.clone()));
}

// ── Moderation & Config ────────────────────────────────────

pub fn set_artist_revocation_storage(env: &Env, artist: &Address) {
    let key = DataKey::RevokedArtist(artist.clone());
    env.storage().persistent().set(&key, &true);
    bump_entry_ttl(env, &key);
}

pub fn remove_artist_revocation_storage(env: &Env, artist: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::RevokedArtist(artist.clone()));
}

pub fn is_artist_revoked_storage(env: &Env, artist: &Address) -> bool {
    let key = DataKey::RevokedArtist(artist.clone());
    let revoked = env
        .storage()
        .persistent()
        .get::<_, bool>(&key)
        .unwrap_or(false);
    if revoked {
        bump_entry_ttl(env, &key);
    }
    revoked
}

pub fn set_treasury_storage(env: &Env, addr: &Address) {
    env.storage().persistent().set(&DataKey::Treasury, addr);
    bump_entry_ttl(env, &DataKey::Treasury);
}

pub fn get_treasury_storage(env: &Env) -> Option<Address> {
    let value = env.storage().persistent().get(&DataKey::Treasury);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::Treasury);
    }
    value
}

pub fn set_protocol_fee_bps_storage(env: &Env, bps: u32) {
    env.storage()
        .persistent()
        .set(&DataKey::ProtocolFeeBps, &bps);
    bump_entry_ttl(env, &DataKey::ProtocolFeeBps);
}

pub fn get_protocol_fee_bps_storage(env: &Env) -> Option<u32> {
    let value = env.storage().persistent().get(&DataKey::ProtocolFeeBps);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::ProtocolFeeBps);
    }
    value
}

pub fn set_min_bid_increment_storage(env: &Env, increment: i128) {
    env.storage()
        .persistent()
        .set(&DataKey::MinBidIncrement, &increment);
    bump_entry_ttl(env, &DataKey::MinBidIncrement);
}

pub fn get_min_bid_increment_storage(env: &Env) -> Option<i128> {
    let value = env.storage().persistent().get(&DataKey::MinBidIncrement);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::MinBidIncrement);
    }
    value
}

pub fn set_auction_extension_window_storage(env: &Env, window: u64) {
    env.storage()
        .persistent()
        .set(&DataKey::AuctionExtensionWindow, &window);
    bump_entry_ttl(env, &DataKey::AuctionExtensionWindow);
}

pub fn get_auction_extension_window_storage(env: &Env) -> Option<u64> {
    let value = env
        .storage()
        .persistent()
        .get(&DataKey::AuctionExtensionWindow);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::AuctionExtensionWindow);
    }
    value
}

pub fn set_auction_extension_trigger_storage(env: &Env, trigger: u64) {
    env.storage()
        .persistent()
        .set(&DataKey::AuctionExtensionTrigger, &trigger);
    bump_entry_ttl(env, &DataKey::AuctionExtensionTrigger);
}

pub fn get_auction_extension_trigger_storage(env: &Env) -> Option<u64> {
    let value = env
        .storage()
        .persistent()
        .get(&DataKey::AuctionExtensionTrigger);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::AuctionExtensionTrigger);
    }
    value
}

// ── Reentrancy Guards ────────────────────────────────────────

pub fn acquire_listing_lock(env: &Env, listing_id: u64) -> bool {
    let key = DataKey::ListingLock(listing_id);
    if env.storage().temporary().has(&key) {
        return false;
    }
    env.storage().temporary().set(&key, &true);
    env.storage()
        .temporary()
        .extend_ttl(&key, REENTRANCY_LOCK_TTL, REENTRANCY_LOCK_TTL);
    true
}

pub fn release_listing_lock(env: &Env, listing_id: u64) {
    let key = DataKey::ListingLock(listing_id);
    env.storage().temporary().remove(&key);
}

pub fn acquire_auction_lock(env: &Env, auction_id: u64) -> bool {
    let key = DataKey::AuctionLock(auction_id);
    if env.storage().temporary().has(&key) {
        return false;
    }
    env.storage().temporary().set(&key, &true);
    env.storage()
        .temporary()
        .extend_ttl(&key, REENTRANCY_LOCK_TTL, REENTRANCY_LOCK_TTL);
    true
}

pub fn release_auction_lock(env: &Env, auction_id: u64) {
    let key = DataKey::AuctionLock(auction_id);
    env.storage().temporary().remove(&key);
}

// ── Admin transfer ───────────────────────────────────────────

pub fn set_pending_admin_storage(env: &Env, pending: &PendingAdminProposal) {
    env.storage()
        .persistent()
        .set(&DataKey::PendingAdmin, pending);
    bump_entry_ttl(env, &DataKey::PendingAdmin);
}

pub fn get_pending_admin_storage(env: &Env) -> Option<PendingAdminProposal> {
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, PendingAdminProposal>(&DataKey::PendingAdmin);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::PendingAdmin);
    }
    value
}

pub fn clear_pending_admin_storage(env: &Env) {
    env.storage().persistent().remove(&DataKey::PendingAdmin);
}

// ── Role-based authorization (Issue #267) ────────────────────

pub fn get_role_storage(env: &Env, role: &crate::types::RoleType) -> Option<Address> {
    let key = DataKey::Role(role.clone());
    let value = env.storage().persistent().get::<DataKey, Address>(&key);
    if value.is_some() {
        bump_entry_ttl(env, &key);
    }
    value
}

pub fn set_role_storage(env: &Env, role: &crate::types::RoleType, authority: &Address) {
    let key = DataKey::Role(role.clone());
    env.storage().persistent().set(&key, authority);
    bump_entry_ttl(env, &key);
}

pub fn set_pending_role_storage(
    env: &Env,
    role: &crate::types::RoleType,
    pending: &PendingRoleProposal,
) {
    let key = DataKey::PendingRole(role.clone());
    env.storage().persistent().set(&key, pending);
    bump_entry_ttl(env, &key);
}

pub fn get_pending_role_storage(
    env: &Env,
    role: &crate::types::RoleType,
) -> Option<PendingRoleProposal> {
    let key = DataKey::PendingRole(role.clone());
    let value = env.storage().persistent().get::<DataKey, PendingRoleProposal>(&key);
    if value.is_some() {
        bump_entry_ttl(env, &key);
    }
    value
}

pub fn clear_pending_role_storage(env: &Env, role: &crate::types::RoleType) {
    env.storage()
        .persistent()
        .remove(&DataKey::PendingRole(role.clone()));
}

// ── Bid history ──────────────────────────────────────────────

pub fn append_bid_record(env: &Env, auction_id: u64, record: &BidRecord, cap: u32) {
    let key = DataKey::AuctionBids(auction_id);
    let mut history = env
        .storage()
        .persistent()
        .get::<DataKey, soroban_sdk::Vec<BidRecord>>(&key)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env));
    if history.len() >= cap {
        let mut trimmed = soroban_sdk::Vec::new(env);
        for i in 1..history.len() {
            trimmed.push_back(history.get(i).unwrap());
        }
        history = trimmed;
    }
    history.push_back(record.clone());
    env.storage().persistent().set(&key, &history);
    bump_entry_ttl(env, &key);
}

pub fn load_auction_bids(env: &Env, auction_id: u64) -> soroban_sdk::Vec<BidRecord> {
    let key = DataKey::AuctionBids(auction_id);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, soroban_sdk::Vec<BidRecord>>(&key)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env));
    if !value.is_empty() {
        bump_entry_ttl(env, &key);
    }
    value
}

/// Records that `bidder` has claimed their losing-bid refund for this auction
/// (Issue #466). Written before the transfer so replay is impossible.
pub fn mark_bid_refunded(env: &Env, auction_id: u64, bidder: &Address) {
    let key = DataKey::BidRefundRecord(auction_id, bidder.clone());
    env.storage().persistent().set(&key, &true);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, OFFER_TTL_LEDGERS);
}

/// Returns `true` when `bidder` has already claimed their refund for this
/// auction (Issue #466). Used for idempotency checks in `refund_losing_bid`.
pub fn is_bid_refunded(env: &Env, auction_id: u64, bidder: &Address) -> bool {
    let key = DataKey::BidRefundRecord(auction_id, bidder.clone());
    let claimed = env
        .storage()
        .persistent()
        .get::<DataKey, bool>(&key)
        .unwrap_or(false);
    if claimed {
        bump_entry_ttl(env, &key);
    }
    claimed
}

// ── Blocked bidders (Issue #199) ─────────────────────────────

pub fn load_blocked_bidders(env: &Env, auction_id: u64) -> Vec<Address> {
    let key = DataKey::AuctionBlockedBidders(auction_id);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<Address>>(&key)
        .unwrap_or_else(|| Vec::new(env));
    if !value.is_empty() {
        bump_entry_ttl(env, &key);
    }
    value
}

pub fn save_blocked_bidders(env: &Env, auction_id: u64, list: &Vec<Address>) {
    let key = DataKey::AuctionBlockedBidders(auction_id);
    if list.is_empty() {
        // Drop the entry entirely so an emptied registry costs nothing.
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, list);
        bump_entry_ttl(env, &key);
    }
}

pub fn is_bidder_blocked(env: &Env, auction_id: u64, bidder: &Address) -> bool {
    load_blocked_bidders(env, auction_id).contains(bidder)
}

pub fn set_paused(env: &Env, paused: bool) {
    env.storage().persistent().set(&DataKey::IsPaused, &paused);
    bump_entry_ttl(env, &DataKey::IsPaused);
}

pub fn is_paused(env: &Env) -> bool {
    env.storage()
        .persistent()
        .get::<DataKey, bool>(&DataKey::IsPaused)
        .unwrap_or(false)
}

// ── Granular pause helpers (Issue #205) ──────────────────────────────────────
//
// Three independent circuit-breaker axes:
//   1. Global flag           — DataKey::IsPaused (existing)
//   2. Per-collection flag   — DataKey::CollectionPaused(address)
//   3. Per-function flag     — DataKey::FunctionPaused(symbol)
//
// is_paused_for() returns true when ANY of the three axes fires.

/// Pause a specific collection.
pub fn set_collection_paused(env: &Env, collection: &Address, paused: bool) {
    let key = DataKey::CollectionPaused(collection.clone());
    if paused {
        env.storage().persistent().set(&key, &true);
        bump_entry_ttl(env, &key);
    } else {
        env.storage().persistent().remove(&key);
    }
}

/// Return whether the given collection is individually paused.
pub fn is_collection_paused(env: &Env, collection: &Address) -> bool {
    let key = DataKey::CollectionPaused(collection.clone());
    let paused = env
        .storage()
        .persistent()
        .get::<DataKey, bool>(&key)
        .unwrap_or(false);
    if paused {
        bump_entry_ttl(env, &key);
    }
    paused
}

/// Pause a specific entry-point function by its symbol name.
pub fn set_function_paused(env: &Env, func: &soroban_sdk::Symbol, paused: bool) {
    let key = DataKey::FunctionPaused(func.clone());
    if paused {
        env.storage().persistent().set(&key, &true);
        bump_entry_ttl(env, &key);
    } else {
        env.storage().persistent().remove(&key);
    }
}

/// Return whether the given function symbol is individually paused.
pub fn is_function_paused(env: &Env, func: &soroban_sdk::Symbol) -> bool {
    let key = DataKey::FunctionPaused(func.clone());
    let paused = env
        .storage()
        .persistent()
        .get::<DataKey, bool>(&key)
        .unwrap_or(false);
    if paused {
        bump_entry_ttl(env, &key);
    }
    paused
}

/// Composite pause check: returns true when ANY of the three circuit-breakers
/// is active for the given (optional) collection and function context.
///
/// Call sites:
///   - Global-only check:           is_paused_for(env, None, None)
///   - Collection-scoped check:     is_paused_for(env, Some(&col), None)
///   - Function-scoped check:       is_paused_for(env, None, Some(&func))
///   - Full context check:          is_paused_for(env, Some(&col), Some(&func))
pub fn is_paused_for(
    env: &Env,
    collection: Option<&Address>,
    func: Option<&soroban_sdk::Symbol>,
) -> bool {
    // Global flag (cheapest read — check first).
    if is_paused(env) {
        return true;
    }
    // Per-function flag.
    if let Some(f) = func {
        if is_function_paused(env, f) {
            return true;
        }
    }
    // Per-collection flag.
    if let Some(c) = collection {
        if is_collection_paused(env, c) {
            return true;
        }
    }
    false
}

// ── Price bounds ─────────────────────────────────────────────

pub fn set_min_price_storage(env: &Env, min: i128) {
    env.storage().persistent().set(&DataKey::MinPrice, &min);
    bump_entry_ttl(env, &DataKey::MinPrice);
}

pub fn get_min_price_storage(env: &Env) -> Option<i128> {
    let value = env.storage().persistent().get(&DataKey::MinPrice);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::MinPrice);
    }
    value
}

pub fn set_max_price_storage(env: &Env, max: i128) {
    env.storage().persistent().set(&DataKey::MaxPrice, &max);
    bump_entry_ttl(env, &DataKey::MaxPrice);
}

pub fn get_max_price_storage(env: &Env) -> Option<i128> {
    let value = env.storage().persistent().get(&DataKey::MaxPrice);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::MaxPrice);
    }
    value
}

// ── Migration marker ─────────────────────────────────────────

pub fn set_migration_done(env: &Env, version: &soroban_sdk::String) {
    let key = DataKey::MigrationDone(version.clone());
    env.storage().persistent().set(&key, &true);
    bump_entry_ttl(env, &key);
}

pub fn is_migration_done(env: &Env, version: &soroban_sdk::String) -> bool {
    let key = DataKey::MigrationDone(version.clone());
    let done = env
        .storage()
        .persistent()
        .get::<_, bool>(&key)
        .unwrap_or(false);
    if done {
        bump_entry_ttl(env, &key);
    }
    done
}

/// Load the resumable migration progress for `version` (phase 0, cursor 0
/// when the migration has not started yet).
pub fn get_migration_progress(env: &Env, version: &soroban_sdk::String) -> MigrationProgress {
    env.storage()
        .persistent()
        .get::<DataKey, MigrationProgress>(&DataKey::MigrationCursor(version.clone()))
        .unwrap_or(MigrationProgress { phase: 0, cursor: 0 })
}

pub fn set_migration_progress(env: &Env, version: &soroban_sdk::String, progress: &MigrationProgress) {
    let key = DataKey::MigrationCursor(version.clone());
    env.storage().persistent().set(&key, progress);
    bump_entry_ttl(env, &key);
}

pub fn clear_migration_progress(env: &Env, version: &soroban_sdk::String) {
    env.storage()
        .persistent()
        .remove(&DataKey::MigrationCursor(version.clone()));
}

// ── Migration stuck marker ─────────────────────────────────────
//
// Written by `run_migration` whenever it saves partial progress (remaining > 0),
// indicating the migration was interrupted and requires further `migrate_step`
// calls. Cleared only when the migration completes so that the operator can
// distinguish "has never been started" (is_stuck = false, no cursor) from
// "was interrupted at least once" (is_stuck = true).

pub fn set_migration_stuck(env: &Env, version: &soroban_sdk::String) {
    let key = DataKey::MigrationStuck(version.clone());
    env.storage().persistent().set(&key, &true);
    bump_entry_ttl(env, &key);
}

pub fn clear_migration_stuck(env: &Env, version: &soroban_sdk::String) {
    env.storage()
        .persistent()
        .remove(&DataKey::MigrationStuck(version.clone()));
}

pub fn is_migration_stuck(env: &Env, version: &soroban_sdk::String) -> bool {
    let key = DataKey::MigrationStuck(version.clone());
    let stuck = env
        .storage()
        .persistent()
        .get::<_, bool>(&key)
        .unwrap_or(false);
    if stuck {
        bump_entry_ttl(env, &key);
    }
    stuck
}

// ── TTL-sweep cursor (Issue #280) ─────────────────────────────

/// Load the resumable progress of the `extend_active_ttls` maintenance
/// sweep (phase 0 cursor 0 the first time it is ever called).
pub fn get_ttl_sweep_progress(env: &Env) -> TtlSweepProgress {
    env.storage()
        .persistent()
        .get::<DataKey, TtlSweepProgress>(&DataKey::TtlSweepState)
        .unwrap_or(TtlSweepProgress { phase: 0, cursor: 0 })
}

pub fn set_ttl_sweep_progress(env: &Env, progress: &TtlSweepProgress) {
    let key = DataKey::TtlSweepState;
    env.storage().persistent().set(&key, progress);
    bump_entry_ttl(env, &key);
}

/// Read-and-delete a legacy (pre-1.1.0) monolithic `Vec<u64>` index entry.
/// Returns `None` when the key does not exist (already migrated or never
/// written).  Used exclusively by the 1.1.0 storage migration.
pub fn take_legacy_index_vec(env: &Env, key: &DataKey) -> Option<Vec<u64>> {
    let value = env.storage().persistent().get::<DataKey, Vec<u64>>(key);
    if value.is_some() {
        env.storage().persistent().remove(key);
    }
    value
}

// ── Per-collection fee overrides (Issue #322) ────────────────────────────────

pub fn set_collection_fee_bps_storage(env: &Env, collection: &Address, bps: u32) {
    let key = DataKey::CollectionFeeBps(collection.clone());
    env.storage().persistent().set(&key, &bps);
    bump_entry_ttl(env, &key);
}

pub fn get_collection_fee_bps_storage(env: &Env, collection: &Address) -> Option<u32> {
    let key = DataKey::CollectionFeeBps(collection.clone());
    let value = env.storage().persistent().get::<DataKey, u32>(&key);
    if value.is_some() {
        bump_entry_ttl(env, &key);
    }
    value
}

pub fn clear_collection_fee_bps_storage(env: &Env, collection: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::CollectionFeeBps(collection.clone()));
}

// ── Migration status view helper ─────────────────────────────────────────────

/// Compute the operator-facing migration status for `version`.
pub fn get_migration_status_storage(env: &Env, version: &soroban_sdk::String) -> MigrationStatus {
    let is_done = is_migration_done(env, version);
    let cursor_key = DataKey::MigrationCursor(version.clone());
    let progress_opt = env
        .storage()
        .persistent()
        .get::<DataKey, MigrationProgress>(&cursor_key);
    let is_in_progress = !is_done && progress_opt.is_some();
    let is_stuck = !is_done && is_migration_stuck(env, version);
    let (phase, cursor) = progress_opt
        .map(|p| (p.phase, p.cursor))
        .unwrap_or((0, 0));
    MigrationStatus {
        version: version.clone(),
        is_done,
        is_in_progress,
        is_stuck,
        phase,
        cursor,
    }
}

// ── Bid-history cap ──────────────────────────────────────────

/// Default bid-history ring-buffer capacity.
pub const DEFAULT_BID_HISTORY_CAP: u32 = 50;
/// Maximum allowed bid-history cap.  Kept at 200 so the O(n) eviction
/// shift (see `append_bid_record`) stays within acceptable compute limits.
pub const MAX_BID_HISTORY_CAP: u32 = 200;

/// Persist the global bid-history cap.
pub fn set_bid_history_cap_storage(env: &Env, cap: u32) {
    env.storage()
        .persistent()
        .set(&DataKey::BidHistoryCap, &cap);
    bump_entry_ttl(env, &DataKey::BidHistoryCap);
}

/// Read the global bid-history cap, defaulting to `DEFAULT_BID_HISTORY_CAP`.
pub fn get_bid_history_cap_storage(env: &Env) -> u32 {
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, u32>(&DataKey::BidHistoryCap);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::BidHistoryCap);
    }
    value.unwrap_or(DEFAULT_BID_HISTORY_CAP)
}

// ── Auction max-extensions cap ───────────────────────────────

/// Default: 0 = unlimited extensions (legacy behaviour preserved).
pub const DEFAULT_AUCTION_MAX_EXTENSIONS: u32 = 0;

/// Persist the global auction max-extensions cap.
pub fn set_auction_max_extensions_storage(env: &Env, max: u32) {
    env.storage()
        .persistent()
        .set(&DataKey::AuctionMaxExtensions, &max);
    bump_entry_ttl(env, &DataKey::AuctionMaxExtensions);
}

/// Read the global auction max-extensions cap.
pub fn get_auction_max_extensions_storage(env: &Env) -> u32 {
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, u32>(&DataKey::AuctionMaxExtensions);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::AuctionMaxExtensions);
    }
    value.unwrap_or(DEFAULT_AUCTION_MAX_EXTENSIONS)
}

// ── Pending treasury rotation (Issue #459) ───────────────────────────────────
//
// A two-step, time-bounded treasury rotation mirrors the `PendingAdminProposal`
// pattern: `propose_treasury` writes this struct, `accept_treasury` (callable
// only by the proposed address, before `expires_at`) atomically moves it to
// `DataKey::Treasury`, and `cancel_treasury_proposal` lets the ProtocolConfig
// role holder abort at any point before acceptance.

/// A pending two-step treasury address rotation (Issue #459).
#[contracttype]
#[derive(Clone)]
pub struct PendingTreasuryProposal {
    /// Address proposed to become the new protocol fee destination.
    pub candidate: Address,
    /// Absolute ledger timestamp after which the proposal can no longer be accepted.
    pub proposed_at: u64,
    /// Absolute ledger timestamp after which the proposal can no longer be accepted.
    pub expires_at: u64,
}

pub fn set_pending_treasury_storage(env: &Env, pending: &PendingTreasuryProposal) {
    env.storage()
        .persistent()
        .set(&DataKey::PendingTreasury, pending);
    bump_entry_ttl(env, &DataKey::PendingTreasury);
}

pub fn get_pending_treasury_storage(env: &Env) -> Option<PendingTreasuryProposal> {
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, PendingTreasuryProposal>(&DataKey::PendingTreasury);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::PendingTreasury);
    }
    value
}

pub fn clear_pending_treasury_storage(env: &Env) {
    env.storage()
        .persistent()
        .remove(&DataKey::PendingTreasury);
}

// ── Listing duration bounds (Issue #460) ─────────────────────────────────────

/// Persist the global minimum listing duration (seconds).
pub fn set_min_listing_duration_storage(env: &Env, secs: u64) {
    env.storage()
        .persistent()
        .set(&DataKey::MinListingDuration, &secs);
    bump_entry_ttl(env, &DataKey::MinListingDuration);
}

/// Read the global minimum listing duration, or `None` when unset.
pub fn get_min_listing_duration_storage(env: &Env) -> Option<u64> {
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::MinListingDuration);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::MinListingDuration);
    }
    value
}

/// Persist the global maximum listing duration (seconds).
pub fn set_max_listing_duration_storage(env: &Env, secs: u64) {
    env.storage()
        .persistent()
        .set(&DataKey::MaxListingDuration, &secs);
    bump_entry_ttl(env, &DataKey::MaxListingDuration);
}

/// Read the global maximum listing duration, or `None` when unset.
pub fn get_max_listing_duration_storage(env: &Env) -> Option<u64> {
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::MaxListingDuration);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::MaxListingDuration);
    }
    value
}

/// Enforce that `expires_at` (when provided) lies within the configured
/// duration window `[now + min, now + max]`.
///
/// - When `MinListingDuration` is unset, no lower-bound check is applied.
/// - When `MaxListingDuration` is unset, no upper-bound check is applied.
/// - `None` `expires_at` is accepted unless the configuration requires a
///   non-expiring listing to be explicitly gated (currently allowed; callers
///   that need to block non-expiring listings should check separately).
///
/// Panics with `InvalidListingDuration` on violation. (Issue #460)
pub fn assert_listing_duration_policy(env: &Env, expires_at: Option<u64>) {
    let Some(exp) = expires_at else { return };
    let now = env.ledger().timestamp();
    if let Some(min_secs) = get_min_listing_duration_storage(env) {
        let earliest = now.saturating_add(min_secs);
        if exp < earliest {
            panic_with_error!(env, MarketplaceError::InvalidListingDuration);
        }
    }
    if let Some(max_secs) = get_max_listing_duration_storage(env) {
        let latest = now.saturating_add(max_secs);
        if exp > latest {
            panic_with_error!(env, MarketplaceError::InvalidListingDuration);
        }
    }
}

// ── Royalty claim records (Issue #461) ──────────────────────────────────────
//
// One `RoyaltyClaimRecord` per (settlement_id, is_listing, recipient) triple.
// Written atomically with settlement and marked `claimed = true` immediately
// after the direct token transfer succeeds. This gives operators a queryable
// payout status for every settlement participant and provides a safe recovery
// path if distribution is ever interrupted by a future contract upgrade.
//
// TTL: same as OFFER_TTL_LEDGERS — records are operational data, not archival,
// and the `extend_active_ttls` maintenance sweep can bump them as needed.

pub fn set_royalty_claim(
    env: &Env,
    settlement_id: u64,
    is_listing: bool,
    recipient: &Address,
    record: &crate::types::RoyaltyClaimRecord,
) {
    let key = DataKey::RoyaltyClaim(settlement_id, is_listing, recipient.clone());
    let is_new = !env.storage().persistent().has(&key);
    env.storage().persistent().set(&key, record);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, OFFER_TTL_LEDGERS);
    // Index new claims under the recipient so get_unclaimed_royalties can paginate.
    if is_new {
        append_recipient_royalty_ref(env, recipient, settlement_id, is_listing);
    }
}

pub fn get_royalty_claim(
    env: &Env,
    settlement_id: u64,
    is_listing: bool,
    recipient: &Address,
) -> Option<crate::types::RoyaltyClaimRecord> {
    let key = DataKey::RoyaltyClaim(settlement_id, is_listing, recipient.clone());
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, crate::types::RoyaltyClaimRecord>(&key);
    if value.is_some() {
        bump_entry_ttl(env, &key);
    }
    value
}

fn append_recipient_royalty_ref(
    env: &Env,
    recipient: &Address,
    settlement_id: u64,
    is_listing: bool,
) {
    let key = DataKey::RecipientRoyaltyIndex(recipient.clone());
    let mut index: soroban_sdk::Vec<crate::types::RoyaltyClaimRef> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env));
    index.push_back(crate::types::RoyaltyClaimRef {
        settlement_id,
        is_listing,
    });
    env.storage().persistent().set(&key, &index);
    env.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_TTL_THRESHOLD, OFFER_TTL_LEDGERS);
}

/// Paginate unclaimed royalty records for `recipient` (Issue #842).
///
/// Walks the recipient index from `start` for up to `limit` entries and returns
/// only those whose stored claim has `claimed == false`.
pub fn get_unclaimed_royalties(
    env: &Env,
    recipient: &Address,
    start: u32,
    limit: u32,
) -> soroban_sdk::Vec<crate::types::RoyaltyClaimRecord> {
    let key = DataKey::RecipientRoyaltyIndex(recipient.clone());
    let index: soroban_sdk::Vec<crate::types::RoyaltyClaimRef> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env));
    if index.len() > 0 {
        bump_entry_ttl(env, &key);
    }
    let mut out: soroban_sdk::Vec<crate::types::RoyaltyClaimRecord> =
        soroban_sdk::Vec::new(env);
    let end = core::cmp::min(start.saturating_add(limit), index.len());
    let mut i = start;
    while i < end {
        if let Some(r) = index.get(i) {
            if let Some(claim) = get_royalty_claim(env, r.settlement_id, r.is_listing, recipient) {
                if !claim.claimed {
                    out.push_back(claim);
                }
            }
        }
        i = i.saturating_add(1);
    }
    out
}

// ── Counter-offer link storage (Issue #471) ──────────────────────────────────

/// Record that `counter_offer_id` was created as a counter to `parent_offer_id`.
pub fn set_counter_offer_parent(env: &Env, counter_offer_id: u64, parent_offer_id: u64) {
    let key = DataKey::CounterOfferParent(counter_offer_id);
    env.storage().persistent().set(&key, &parent_offer_id);
    bump_entry_ttl(env, &key);
}

/// Return the parent offer id for a counter-offer, or `None` if the offer is
/// not a counter-offer (i.e. it was created via `make_offer`).
pub fn get_counter_offer_parent(env: &Env, counter_offer_id: u64) -> Option<u64> {
    let key = DataKey::CounterOfferParent(counter_offer_id);
    let value = env.storage().persistent().get::<DataKey, u64>(&key);
    if value.is_some() {
        bump_entry_ttl(env, &key);
    }
    value
}

/// Record that `parent_offer_id` was superseded by `counter_offer_id`.
pub fn set_offer_superseded_by(env: &Env, parent_offer_id: u64, counter_offer_id: u64) {
    let key = DataKey::OfferSupersededBy(parent_offer_id);
    env.storage().persistent().set(&key, &counter_offer_id);
    bump_entry_ttl(env, &key);
}

/// Return the counter-offer id that superseded `parent_offer_id`, or `None`.
pub fn get_offer_superseded_by(env: &Env, parent_offer_id: u64) -> Option<u64> {
    let key = DataKey::OfferSupersededBy(parent_offer_id);
    let value = env.storage().persistent().get::<DataKey, u64>(&key);
    if value.is_some() {
        bump_entry_ttl(env, &key);
    }
    value
}

// ── Governance quorum storage (Issue #472) ───────────────────────────────────

pub fn get_governance_proposal_count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::GovernanceProposalCount)
        .unwrap_or(0)
}

pub fn increment_governance_proposal_count(env: &Env) -> u64 {
    let count = get_governance_proposal_count(env) + 1;
    env.storage()
        .persistent()
        .set(&DataKey::GovernanceProposalCount, &count);
    bump_entry_ttl(env, &DataKey::GovernanceProposalCount);
    count
}

pub fn save_governance_proposal(env: &Env, proposal: &crate::types::GovernanceProposal) {
    let key = DataKey::GovernanceProposal(proposal.proposal_id);
    env.storage().persistent().set(&key, proposal);
    bump_entry_ttl(env, &key);
}

pub fn load_governance_proposal(
    env: &Env,
    proposal_id: u64,
) -> Option<crate::types::GovernanceProposal> {
    let key = DataKey::GovernanceProposal(proposal_id);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, crate::types::GovernanceProposal>(&key);
    if value.is_some() {
        bump_entry_ttl(env, &key);
    }
    value
}

pub fn load_governance_approvals(env: &Env, proposal_id: u64) -> Vec<Address> {
    let key = DataKey::GovernanceApprovals(proposal_id);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<Address>>(&key)
        .unwrap_or_else(|| Vec::new(env));
    if !value.is_empty() {
        bump_entry_ttl(env, &key);
    }
    value
}

pub fn save_governance_approvals(env: &Env, proposal_id: u64, approvals: &Vec<Address>) {
    let key = DataKey::GovernanceApprovals(proposal_id);
    env.storage().persistent().set(&key, approvals);
    bump_entry_ttl(env, &key);
}

// ── Terminal-record cleanup cursor (Issue #474) ──────────────────────────────
//
// `cleanup_terminal_records` is a permissionless (or role-gated) entry point
// that emits a `TerminalCleanedEvent` marker for each terminal record it
// processes, then stops refreshing that record's TTL so Soroban's own archival
// mechanism eventually reclaims the storage rent. The cursor makes the sweep
// resumable across multiple calls.
//
// State layout:
//   phase 0 — walk listing ids 1..=ListingCount (skip Active / unknown)
//   phase 1 — walk offer ids 1..=OfferCount (skip Pending / unknown)
//   Once phase 1 completes the cursor is cleared; callers may re-run at will.

/// Resumable cursor for the `cleanup_terminal_records` permissionless sweep.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCleanupCursor {
    /// Current sweep phase: 0 = listings, 1 = offers.
    pub phase: u32,
    /// Last id processed in the current phase (0 = not started).
    pub cursor: u64,
}

pub fn get_terminal_cleanup_cursor(env: &Env) -> TerminalCleanupCursor {
    env.storage()
        .persistent()
        .get::<DataKey, TerminalCleanupCursor>(&DataKey::TerminalCleanupCursor)
        .unwrap_or(TerminalCleanupCursor { phase: 0, cursor: 0 })
}

pub fn set_terminal_cleanup_cursor(env: &Env, cursor: &TerminalCleanupCursor) {
    env.storage()
        .persistent()
        .set(&DataKey::TerminalCleanupCursor, cursor);
    bump_entry_ttl(env, &DataKey::TerminalCleanupCursor);
}

pub fn clear_terminal_cleanup_cursor(env: &Env) {
    env.storage()
        .persistent()
        .remove(&DataKey::TerminalCleanupCursor);
}

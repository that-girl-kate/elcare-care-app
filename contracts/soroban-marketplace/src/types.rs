// types.rs
use soroban_sdk::{contracterror, contracttype, Address, Symbol};

/// Four independent role axes that govern privileged entry points (Issue #267).
///
/// Every entry point is owned by exactly one role. When no explicit holder has
/// been assigned for a role, it falls back to the contract `Admin`, so existing
/// single-admin deployments keep working unchanged until an operator opts in via
/// `migrate_roles` or `propose_role_transfer`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoleType {
    /// Manages price bounds, treasury, fees, bid/auction config parameters.
    ProtocolConfig,
    /// Controls global/collection/function circuit breakers.
    EmergencyPause,
    /// Artist revocation, reinstatement, and collection-level listing cleanup.
    CollectionAdmin,
    /// Storage and version migration entry points.
    Upgrade,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum MarketplaceError {
    InvalidCid = 1,
    InvalidPrice = 2,
    ListingNotFound = 3,
    ListingNotActive = 4,
    Unauthorized = 5,
    CannotBuyOwnListing = 6,
    InvalidSplit = 7,
    TooManyRecipients = 8,
    AuctionNotFound = 9,
    AuctionNotActive = 10,
    BidTooLow = 11,
    AuctionExpired = 12,
    AuctionNotExpired = 13,
    AuctionAlreadyFinalized = 14,
    ArtistRevoked = 15,
    OfferNotFound = 16,
    CannotOfferOwnListing = 17,
    OfferNotPending = 18,
    InsufficientOfferAmount = 19,
    ListingSold = 20,
    ListingCancelled = 21,
    ReentrancyGuard = 22,
    ContractPaused = 23,
    /// Royalty bps greater than 10000 (100%) — rejects create_listing/create_auction
    InvalidRoyalty = 24,
    /// Token attempted at purchase time but is no longer whitelisted
    TokenNotWhitelisted = 25,
    /// The sum of all Recipient basis-point values plus the protocol fee exceeds
    /// 10 000 bps (100%).  Rejected at listing creation and on any update that
    /// would mutate recipients, so an invalid split can never be persisted.
    RoyaltyExceedsLimit = 26,
    /// The listing has passed its `expires_at` ledger timestamp and can no
    /// longer be purchased or updated.
    ListingExpired = 27,
    /// `expire_listing` was called on a listing whose `expires_at` is still in
    /// the future (or the listing has no expiry).
    ListingNotExpired = 28,
    /// `finalize_auction` was called before `end_time` has passed.
    AuctionNotEnded = 29,
    /// `cancel_auction` was called on an auction that already has at least one
    /// bid — cancelling would strand the bidder's escrowed funds.
    AuctionHasBids = 30,
    /// `create_auction` was called with an `end_time` (or `duration`) that is in
    /// the past or shorter than `MIN_AUCTION_DURATION`.
    InvalidAuctionDuration = 31,
    /// `place_bid` was called by the auction creator — self-bidding (shill
    /// bidding) is not allowed.  The bidder address must differ from the
    /// auction's `creator` field.
    SelfBidNotAllowed = 32,
    /// An offer state transition was attempted from a terminal state (Accepted,
    /// Rejected, or Withdrawn), or from Pending with the wrong authorizer.
    InvalidOfferState = 33,
    /// `accept_offer` called after the offer's `expires_at` has passed; or
    /// `reclaim_offer` called before expiry / on a non-expiring offer.
    OfferExpired = 34,
    /// A new offer would exceed MAX_OFFERS_PER_LISTING active (Pending) offers
    /// for this listing.  A cap bounds per-listing storage growth and keeps the
    /// auto-reject sweep economically viable.
    OfferLimitReached = 35,
    /// `cancel_listings` was called with more ids than MAX_BATCH_CANCEL in a
    /// single batch — split the request into smaller batches.
    BatchTooLarge = 36,
    /// `migrate` was called again for a version whose migration marker is
    /// already recorded in persistent storage.
    AlreadyMigrated = 37,
    /// `purchase` was attempted by the listing's own artist (or a recipient of
    /// the listing) — self-purchase is not allowed.
    SelfPurchaseNotAllowed = 38,
    /// A listing price violates the configured `[min, max]` price bounds.
    PriceOutOfBounds = 39,
    /// A checked arithmetic operation overflowed while computing fee splits.
    ArithmeticOverflow = 40,
    /// `accept_admin` was called after the pending admin proposal's `expires_at`
    /// ledger timestamp has passed.  The proposal must be re-issued.
    ///
    /// NOTE: Issue #202 suggested discriminant 35, but 35/36 are already taken
    /// (`OfferLimitReached`/`BatchTooLarge`); the next free codes 41/42 are used
    /// instead so existing on-chain error codes are not renumbered.
    AdminProposalExpired = 41,
    /// `accept_admin` or `cancel_admin_proposal` was called when no admin
    /// proposal is currently pending.
    NoAdminProposalPending = 42,
    /// A royalty `Recipient` has a `percentage` of zero basis points.
    /// Every recipient in the list must contribute a non-zero share so that
    /// the list cannot contain dead-weight entries that waste gas on every
    /// settlement.
    ZeroRecipientBps = 43,
    /// The recipient list contains a duplicate address.  Each address may
    /// appear at most once so payouts are unambiguous and the total bps
    /// calculation cannot be confused by double-counting.
    DuplicateRecipient = 44,
    /// `admin_cancel_auction` was called on an auction that is not Active, or
    /// `refund_losing_bid` was called on an auction that is not in a state
    /// that permits refunds.
    InvalidAuctionState = 45,
    /// A bidder attempted to call `refund_losing_bid` for an auction where
    /// they are the current highest bidder (their funds are still locked as
    /// the winning escrow) or they have no bid to refund.
    NoBidToRefund = 46,
    /// The anti-sniping extension window is zero or would overflow the auction
    /// end time.
    InvalidExtensionWindow = 47,
    /// The auction has reached its `max_extensions` cap and can no longer be
    /// extended by the anti-sniping logic.
    MaxExtensionsReached = 48,
    /// `escrow_nft` was called by an account that does not own the token.
    NotTokenOwner = 49,
    /// The token is already held in marketplace escrow for another listing
    /// or auction (double-listing guard).
    TokenAlreadyEscrowed = 50,
    /// An anti-sniping extension would push the auction's end_time beyond
    /// original_end_time + MAX_TOTAL_AUCTION_DURATION. The bid is still
    /// accepted, but the extension is not applied.
    AuctionDurationLimitReached = 51,
    /// `accept_role_transfer` or `cancel_role_proposal` was called when no
    /// role-transfer proposal is currently pending for this role.
    NoRoleProposalPending = 52,
    /// `accept_role_transfer` was called after the pending role proposal's
    /// `expires_at` ledger timestamp has passed. The proposal must be re-issued.
    RoleProposalExpired = 53,
    /// `propose_role_transfer` was called with `candidate == current_authority`.
    /// Transferring a role to its own current holder is a no-op and is rejected
    /// so that the pending-proposal slot is not polluted with a dead proposal.
    RoleTransferToSelf = 54,
    /// `propose_role_transfer` was called with `candidate` equal to this
    /// contract's own address. Assigning the contract as a role holder would
    /// create an irrecoverable governance state (no key can sign for a contract
    /// address in the normal Soroban auth model).
    RoleTransferToContract = 55,
    /// `accept_treasury` was called after the pending treasury proposal's
    /// `expires_at` ledger timestamp has passed. The proposal must be re-issued.
    /// (Issue #459)
    TreasuryProposalExpired = 56,
    /// `accept_treasury` or `cancel_treasury_proposal` was called when no
    /// treasury proposal is currently pending. (Issue #459)
    NoTreasuryProposalPending = 57,
    /// `propose_treasury` was called with `candidate == current_treasury`.
    /// Proposing the same address that is already the active treasury is a no-op
    /// and is rejected so the pending slot is not polluted. (Issue #459)
    TreasuryProposalSelf = 58,
    /// A listing or auction expiry duration violates the configured
    /// `[min_listing_duration, max_listing_duration]` bounds. (Issue #460)
    InvalidListingDuration = 59,
    /// The declared collection address is incompatible with the token: the token
    /// does not belong to the given collection or the collection standard does
    /// not match the requested quantity semantics. (Issue #458)
    CollectionIncompatible = 60,
    /// One item in a `create_listings` batch failed preflight validation.
    /// The `item_index` field of the returned `BatchItemError` identifies
    /// the zero-based position of the failing item. (Issue #457)
    BatchItemInvalid = 61,
    /// `reconcile_listing_owner` was called with an `expected_owner` that does
    /// not match the current effective owner of the listing. The reconciliation
    /// is rejected so stale or concurrent updates cannot silently overwrite
    /// each other.
    OwnershipMismatch = 62,
    /// `claim_royalty` was called for a settlement whose royalty for this
    /// recipient has already been claimed.  Prevents double-payment on retry.
    RoyaltyAlreadyClaimed = 63,
    /// `claim_royalty` was called for a (settlement_id, is_listing, recipient)
    /// triple that has no corresponding claim record in storage.
    RoyaltyClaimNotFound = 64,
    /// `buy_artwork` was called during an active reservation window by a buyer
    /// who is not the reserved address (`reserved_for`).
    ReservationWindowActive = 65,
    /// `set_listing_reservation` was called with an invalid window:
    /// `reservation_end <= reservation_start`, or `reservation_end` is in the past.
    InvalidReservationWindow = 66,
    /// The referenced governance proposal does not exist in persistent storage. (Issue #472)
    GovernanceProposalNotFound = 67,
    /// `execute_governance_action` was called before the proposal reached its
    /// approval threshold. (Issue #472)
    GovernanceThresholdNotMet = 68,
    /// `approve_governance_action` was called by a signer that has already
    /// approved this proposal. (Issue #472)
    GovernanceAlreadyApproved = 69,
    /// `approve_governance_action` or `execute_governance_action` was called
    /// after the proposal's `expires_at` deadline passed. (Issue #472)
    GovernanceProposalExpired = 70,
    /// `execute_governance_action` was called on a proposal that has already
    /// been executed (replay protection). (Issue #472)
    GovernanceProposalAlreadyExecuted = 71,
    /// `execute_governance_action` or `approve_governance_action` was called
    /// on a proposal that was cancelled. (Issue #472)
    GovernanceProposalCancelled = 72,
    /// `approve_governance_action` was called by an address not in the
    /// proposal's signer set. (Issue #472)
    GovernanceSignerNotAuthorized = 73,
    /// `counter_offer` was called on an offer that is not in Pending state,
    /// or `accept_counter_offer` was called on a non-counter-offer ID. (Issue #471)
    NotCounterOffer = 74,
    /// A lifecycle transition that is not in the allowed transition matrix was
    /// attempted (formal listing/auction/offer state machine, Issue #426).
    InvalidStateTransition = 75,
}

/// One pending or completed royalty claim for a single recipient.
///
/// Written at settlement time, updated (claimed → true) when the recipient
/// pulls their payment via `claim_royalty`.  The separate write-before-transfer
/// ordering means the payout status is always queryable via `get_royalty_claim`
/// even in the hypothetical case where a future upgrade interrupts settlement
/// mid-distribution.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoyaltyClaimRecord {
    pub settlement_id: u64,
    /// `true` when the settlement was a fixed-price listing or offer acceptance;
    /// `false` when it was an auction finalization.
    pub is_listing: bool,
    pub recipient: Address,
    pub token: Address,
    pub amount: i128,
    /// `true` once the recipient has successfully called `claim_royalty` (or the
    /// direct transfer at settlement time succeeded and the record was auto-marked).
    pub claimed: bool,
    /// Ledger sequence at which the claim record was written (settlement time).
    pub created_at: u32,
    /// Ledger sequence at which the claim was redeemed; `None` while unclaimed.
    pub claimed_at: Option<u32>,
}

/// Lightweight index entry for recipient-scoped royalty claim pagination (Issue #842).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoyaltyClaimRef {
    pub settlement_id: u64,
    pub is_listing: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListingStatus {
    Active,
    Sold,
    Cancelled,
}

/// A formal listing lifecycle transition.
///
/// Concrete transitions are validated by [`ListingStatus::transition`]; the
/// returned [`ListingTransitionEffect`] describes the escrow/refund side effects
/// that must be applied atomically when the transition is committed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListingTransition {
    /// Active -> Sold (direct purchase or offer acceptance).
    Sold,
    /// Active -> Cancelled (owner, expiry, or admin revocation).
    Cancelled(CancelReason),
}

/// Side effects associated with a committed listing transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListingTransitionEffect {
    pub to: ListingStatus,
    pub cancel_reason: Option<CancelReason>,
    /// The NFT held in marketplace escrow must be released to the buyer (Sold)
    /// or returned to the seller (Cancelled).
    pub release_escrow: bool,
    /// All remaining Pending offers must be rejected and their escrowed funds
    /// returned to the offerers before the transition is considered complete.
    pub refund_pending_offers: bool,
}

impl ListingStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, &ListingStatus::Active)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, &ListingStatus::Sold | &ListingStatus::Cancelled)
    }

    pub fn can_transition_to(&self, to: &ListingStatus) -> bool {
        matches!(
            (self, to),
            (&ListingStatus::Active, &ListingStatus::Sold)
                | (&ListingStatus::Active, &ListingStatus::Cancelled)
        )
    }

    pub fn require_transition_to(&self, to: &ListingStatus) -> Result<(), MarketplaceError> {
        if self.can_transition_to(to) {
            Ok(())
        } else {
            Err(MarketplaceError::ListingNotActive)
        }
    }

    pub fn transition(
        &self,
        transition: ListingTransition,
    ) -> Result<ListingTransitionEffect, MarketplaceError> {
        match (self, transition) {
            (&ListingStatus::Active, ListingTransition::Sold) => Ok(ListingTransitionEffect {
                to: ListingStatus::Sold,
                cancel_reason: None,
                release_escrow: true,
                refund_pending_offers: true,
            }),
            (&ListingStatus::Active, ListingTransition::Cancelled(reason)) => {
                Ok(ListingTransitionEffect {
                    to: ListingStatus::Cancelled,
                    cancel_reason: Some(reason),
                    release_escrow: true,
                    refund_pending_offers: true,
                })
            }
            _ => Err(MarketplaceError::ListingNotActive),
        }
    }
}

/// Discriminant carried in the ListingCancelledEvent to indicate why a listing
/// was cancelled.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum CancelReason {
    Owner = 1,
    Expired = 2,
    AdminRevoked = 3,
}

/// Typed reason carried in AuctionCancelledEvent (Issue #469).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AuctionCancelReason {
    /// Creator cancelled a no-bid auction voluntarily.
    Owner = 1,
    /// Admin/ProtocolConfig role cancelled an auction with bids outstanding.
    Admin = 2,
    /// EmergencyPause role performed an emergency cancellation.
    Emergency = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recipient {
    pub address: Address,
    /// Share expressed in basis points (0 – 10 000).
    pub percentage: u32,
}

/// One resolved leg of a settlement payout: who gets paid and the exact
/// amount (in the payment token's smallest unit) they receive.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutLeg {
    pub address: Address,
    pub amount: i128,
}

/// `Option<PayoutLeg>` substitute for use inside `#[contracttype]` structs.
///
/// Soroban XDR encoding does not support `Option<T>` when T is itself a
/// custom `#[contracttype]`. This enum provides the same semantics.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptionalPayoutLeg {
    Some(PayoutLeg),
    None,
}

/// A fully-computed settlement breakdown for a single sale/settlement amount.
///
/// Produced by `MarketplaceContract::calculate_payout_plan` (see the doc
/// comment there for the rounding policy) and returned as-is by the
/// `simulate_payout` read-only entry point so off-chain callers — including
/// the frontend fee display — can reproduce the exact on-chain split without
/// performing a real purchase.
///
/// Invariant, enforced before this value is ever returned or acted upon:
/// `royalty.amount (if any) + fee + sum(recipients[i].amount) == total == amount`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayoutPlan {
    /// `None` when no royalty applies (zero bps, or the royalty receiver is
    /// the seller themselves — see `calculate_payout_plan` for details).
    pub royalty: OptionalPayoutLeg,
    /// Protocol fee amount. `0` when no treasury is configured or `fee_bps`
    /// is `0`.
    pub fee: i128,
    /// One leg per input recipient, in the same order as the input
    /// `recipients` list (insertion order from listing/auction creation).
    /// The last leg absorbs the basis-point division's truncation remainder.
    pub recipients: soroban_sdk::Vec<PayoutLeg>,
    /// Total amount accounted for by this plan. Always exactly equal to the
    /// `amount` passed into `calculate_payout_plan`.
    pub total: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct BatchCreateListingInput {
    pub price: i128,
    pub currency: Symbol,
    pub token: Address,
    pub collection: Address,
    pub token_id: u64,
    pub quantity: u64,
    pub recipients: soroban_sdk::Vec<Recipient>,
    pub expires_at: Option<u64>,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct BatchUpdateListingInput {
    pub listing_id: u64,
    pub new_price: i128,
    pub new_token: Address,
    pub new_recipients: soroban_sdk::Vec<Recipient>,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Listing {
    pub listing_id: u64,
    pub artist: Address,
    /// Opaque base-unit amount denominated in `token` (e.g. stroops for
    /// native XLM, or the equivalent smallest unit for a whitelisted SAC).
    /// The contract performs no decimal scaling — see the `token` field doc
    /// below and `Contract::validate_token_asset`. Decimal/precision policy
    /// for display purposes lives in the off-chain token registry
    /// (frontend `config/tokens.ts`, indexer token metadata), not on-chain.
    pub price: i128,
    pub currency: Symbol,
    /// Address of the payment asset contract accepted for this listing —
    /// either the native XLM Stellar Asset Contract or another SAC present
    /// in the admin-managed whitelist (`get_token_whitelist`). All amounts
    /// (`price`, bids, offer amounts) are base units of this token's own
    /// `decimals()` (7, for both native XLM and any classic-asset SAC on
    /// Stellar); the contract treats them as opaque i128 values and never
    /// rescales them. Validated at write time by `is_token_whitelisted` and
    /// `validate_token_asset` — an unsupported or obviously-wrong asset
    /// address (e.g. equal to the collection or to this contract) is
    /// rejected before the listing/auction/offer can ever be created, so it
    /// can never reach settlement.
    pub token: Address,
    pub collection: Address,
    pub token_id: u64,
    /// Quantity for ERC-1155 listings (fungible editions). For ERC-721,
    /// this is always 1 (single NFT).
    pub quantity: u64,
    pub recipients: soroban_sdk::Vec<Recipient>,
    pub status: ListingStatus,
    pub owner: Option<Address>,
    pub created_at: u32,
    pub protocol_fee_bps: u32,
    pub expires_at: Option<u64>,
    /// Address that has the exclusive right to purchase during the reservation
    /// window. `None` means no reservation is active.
    pub reserved_for: Option<Address>,
    /// Ledger timestamp at which the reservation window opens (inclusive).
    /// `None` means the window has already started (i.e. effective immediately).
    pub reservation_start: Option<u64>,
    /// Ledger timestamp at which the reservation window closes (exclusive).
    /// Once `now >= reservation_end`, the listing is open to any buyer.
    /// `None` means no reservation end is set.
    pub reservation_end: Option<u64>,
}

impl Listing {
    /// Validates the transition against the current status and, if allowed,
    /// commits the new status. The returned effect tells the caller which
    /// escrow/refund obligations must be executed atomically with the status
    /// change.
    pub fn apply_transition(
        &mut self,
        transition: ListingTransition,
    ) -> Result<ListingTransitionEffect, MarketplaceError> {
        let effect = self.status.transition(transition)?;
        self.status = effect.to.clone();
        Ok(effect)
    }
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuctionStatus {
    Active,
    Finalized,
    Cancelled,
}

/// A formal auction lifecycle transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuctionTransition {
    /// Active -> Finalized after the auction ends with a winning bid.
    Finalized,
    /// Active -> Cancelled (creator no-bid cancellation, admin, or emergency).
    Cancelled(AuctionCancelReason),
}

/// Side effects associated with a committed auction transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuctionTransitionEffect {
    pub to: AuctionStatus,
    pub cancel_reason: Option<AuctionCancelReason>,
    /// The auctioned NFT must leave marketplace escrow.
    pub release_escrow: bool,
    /// Losing bids must be refunded. `false` for normal finalization, `true`
    /// for every cancellation path (the winning/highest bid is also refunded
    /// because no sale occurs).
    pub refund_bids: bool,
    /// The winning bidder's escrowed payment must be released to the seller on
    /// finalization.
    pub pay_winner: bool,
}

impl AuctionStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, &AuctionStatus::Active)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, &AuctionStatus::Finalized | &AuctionStatus::Cancelled)
    }

    pub fn can_transition_to(&self, to: &AuctionStatus) -> bool {
        matches!(
            (self, to),
            (&AuctionStatus::Active, &AuctionStatus::Finalized)
                | (&AuctionStatus::Active, &AuctionStatus::Cancelled)
        )
    }

    pub fn require_transition_to(&self, to: &AuctionStatus) -> Result<(), MarketplaceError> {
        if self.can_transition_to(to) {
            Ok(())
        } else {
            Err(MarketplaceError::InvalidAuctionState)
        }
    }

    pub fn transition(
        &self,
        transition: AuctionTransition,
    ) -> Result<AuctionTransitionEffect, MarketplaceError> {
        match (self, transition) {
            (&AuctionStatus::Active, AuctionTransition::Finalized) => {
                Ok(AuctionTransitionEffect {
                    to: AuctionStatus::Finalized,
                    cancel_reason: None,
                    release_escrow: true,
                    refund_bids: false,
                    pay_winner: true,
                })
            }
            (&AuctionStatus::Active, AuctionTransition::Cancelled(reason)) => {
                Ok(AuctionTransitionEffect {
                    to: AuctionStatus::Cancelled,
                    cancel_reason: Some(reason),
                    release_escrow: true,
                    refund_bids: true,
                    pay_winner: false,
                })
            }
            _ => Err(MarketplaceError::InvalidAuctionState),
        }
    }
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Auction {
    pub auction_id: u64,
    pub creator: Address,
    pub token: Address,
    pub collection: Address,
    pub token_id: u64,
    pub reserve_price: i128,
    pub highest_bid: i128,
    pub highest_bidder: Option<Address>,
    pub end_time: u64,
    pub status: AuctionStatus,
    pub recipients: soroban_sdk::Vec<Recipient>,
    pub min_increment: i128,
    pub extension_window: u64,
    pub extension_trigger: u64,
    pub protocol_fee_bps: u32,
    /// Bid-history ring-buffer capacity snapshotted at auction creation time.
    ///
    /// Snapshotting here means a later admin change to the global
    /// `BidHistoryCap` never retroactively shrinks or grows the history of
    /// an already-running auction — each auction's ring-buffer behaviour is
    /// fixed when it is created.
    ///
    /// Valid range: 1 – 200 (enforced by `set_bid_history_cap`).
    pub bid_history_cap: u32,
    /// Maximum number of times this auction's end time may be extended by
    /// the anti-sniping logic.  0 = unlimited (legacy behaviour).
    /// Snapshotted from the global `max_extensions` setting at creation time.
    pub max_extensions: u32,
    /// Running count of extensions applied so far.
    pub extension_count: u32,
    /// Original end time set at auction creation, used to enforce the
    /// maximum total auction duration cap. Extensions cannot push end_time
    /// beyond original_end_time + MAX_TOTAL_AUCTION_DURATION.
    pub original_end_time: u64,
}

impl Auction {
    /// Validates and commits an auction lifecycle transition.
    ///
    /// The status-machine layer determines whether the transition is legal;
    /// this helper then refines the payout side effect for auctions that end
    /// without a winning bid (e.g. an expired no-bid auction that is finalized).
    pub fn apply_transition(
        &mut self,
        transition: AuctionTransition,
    ) -> Result<AuctionTransitionEffect, MarketplaceError> {
        let mut effect = self.status.transition(transition)?;
        if effect.pay_winner {
            effect.pay_winner = self.highest_bidder.is_some() && self.highest_bid > 0;
        }
        self.status = effect.to.clone();
        Ok(effect)
    }
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BidRecord {
    pub bidder: Address,
    pub amount: i128,
    pub ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OfferStatus {
    Pending,
    Accepted,
    Rejected,
    Withdrawn,
    /// Auto-swept after expiry by `sweep_expired_offers` (Issue #470).
    Expired,
}

/// A formal offer lifecycle transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OfferTransition {
    Accept,
    Reject,
    Withdraw,
    Expire,
}

impl OfferStatus {
    pub fn is_pending(&self) -> bool {
        matches!(self, &OfferStatus::Pending)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            &OfferStatus::Accepted
                | &OfferStatus::Rejected
                | &OfferStatus::Withdrawn
                | &OfferStatus::Expired
        )
    }

    pub fn can_transition_to(&self, to: &OfferStatus) -> bool {
        matches!(
            (self, to),
            (&OfferStatus::Pending, &OfferStatus::Accepted)
                | (&OfferStatus::Pending, &OfferStatus::Rejected)
                | (&OfferStatus::Pending, &OfferStatus::Withdrawn)
                | (&OfferStatus::Pending, &OfferStatus::Expired)
        )
    }

    pub fn require_transition_to(&self, to: &OfferStatus) -> Result<(), MarketplaceError> {
        if self.can_transition_to(to) {
            Ok(())
        } else {
            Err(MarketplaceError::InvalidOfferState)
        }
    }

    pub fn transition(&self, transition: OfferTransition) -> Result<OfferStatus, MarketplaceError> {
        match (self, transition) {
            (&OfferStatus::Pending, OfferTransition::Accept) => Ok(OfferStatus::Accepted),
            (&OfferStatus::Pending, OfferTransition::Reject) => Ok(OfferStatus::Rejected),
            (&OfferStatus::Pending, OfferTransition::Withdraw) => Ok(OfferStatus::Withdrawn),
            (&OfferStatus::Pending, OfferTransition::Expire) => Ok(OfferStatus::Expired),
            _ => Err(MarketplaceError::InvalidOfferState),
        }
    }
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Offer {
    pub offer_id: u64,
    pub listing_id: u64,
    pub offerer: Address,
    pub amount: i128,
    pub token: Address,
    pub status: OfferStatus,
    pub created_at: u32,
    pub expires_at: Option<u64>,
}

impl Offer {
    /// Validates and commits an offer lifecycle transition.
    pub fn apply_transition(
        &mut self,
        transition: OfferTransition,
    ) -> Result<(), MarketplaceError> {
        let to = self.status.transition(transition)?;
        self.status = to;
        Ok(())
    }
}

// ── Role inventory types (Issue #473) ────────────────────────────────────────

/// One row in the role inventory: the current holder of a single role axis
/// and any pending rotation proposal.
#[contracttype]
#[derive(Clone, Debug)]
pub struct RoleEntry {
    pub role: RoleType,
    /// Current effective holder (either the explicit Role(role) storage value,
    /// or the fallback Admin when no explicit holder is set).
    pub holder: Address,
    /// Candidate address if a rotation proposal is currently pending.
    pub pending_candidate: Option<Address>,
    /// Expiry ledger timestamp of the pending proposal (seconds since epoch).
    pub pending_expires_at: Option<u64>,
}

/// Full read-only snapshot returned by `get_role_inventory`.
#[contracttype]
#[derive(Clone, Debug)]
pub struct RoleInventory {
    /// Current admin address, or None if the contract is not yet initialised.
    pub admin: Option<Address>,
    /// Candidate for the admin role if a proposal is pending.
    pub pending_admin_candidate: Option<Address>,
    /// Expiry timestamp of the pending admin proposal.
    pub pending_admin_expires_at: Option<u64>,
    /// One entry per role axis (ProtocolConfig, EmergencyPause,
    /// CollectionAdmin, Upgrade).
    pub roles: soroban_sdk::Vec<RoleEntry>,
    /// Ledger sequence at which this snapshot was taken.
    pub ledger_sequence: u32,
    /// Ledger timestamp (seconds since Unix epoch) at which this snapshot was taken.
    pub ledger_timestamp: u64,
}

/// Identifies which standard a deployed collection implements.
///
/// Returned by collection contracts via `contract_type()` so the marketplace
/// can enforce quantity-semantic compatibility at listing creation. (Issue #458)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectionStandard {
    /// ERC-721-equivalent single-token collection (quantity must be 1).
    Erc721,
    /// ERC-1155-equivalent multi-edition collection (quantity >= 1).
    Erc1155,
    /// Lazy-mint ERC-721 variant (quantity must be 1).
    LazyMint721,
    /// Lazy-mint ERC-1155 variant (quantity >= 1).
    LazyMint1155,
}

/// Returned by `create_listings` when any item in the batch fails preflight
/// validation.  Carries the zero-based index of the failing item so clients
/// can surface the exact problematic entry without guessing. (Issue #457)
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchItemError {
    /// Zero-based index of the item that failed.
    pub item_index: u32,
    /// The marketplace error code that describes the failure.
    pub error_code: u32,
}

/// Operation types that require multi-approval quorum governance (Issue #472).
///
/// Low-risk operations continue to use single-role authorization. Only the
/// three high-impact types below require a quorum of signers to approve before
/// execution.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GovernanceProposalType {
    /// Rotate the treasury destination address (high-value fund flow change).
    TreasuryRotation,
    /// Increase the protocol fee (fee-extracting change).
    FeeIncrease,
    /// Toggle the global circuit-breaker pause (market-halting change).
    GlobalPause,
}

/// An on-chain multi-approval proposal for a high-risk governance action (Issue #472).
///
/// Created by `propose_governance_action`; signers call `approve_governance_action`
/// until the `threshold` is met; then `execute_governance_action` carries out the
/// underlying operation.  `cancelled` and `executed` are mutually exclusive terminal
/// states; both prevent further approvals or execution (replay protection).
#[contracttype]
#[derive(Clone, Debug)]
pub struct GovernanceProposal {
    pub proposal_id: u64,
    pub proposal_type: GovernanceProposalType,
    pub proposed_by: Address,
    /// Ordered, deduplicated set of addresses authorized to approve this proposal.
    pub signers: soroban_sdk::Vec<Address>,
    /// Minimum number of distinct signer approvals required before execution.
    pub threshold: u32,
    /// Absolute ledger timestamp after which the proposal cannot be approved or
    /// executed — forces re-proposal of stale governance actions.
    pub expires_at: u64,
    /// Ledger sequence at proposal creation.
    pub created_at: u32,
    /// `true` once `execute_governance_action` has succeeded (replay guard).
    pub executed: bool,
    /// `true` once `cancel_governance_action` has been called.
    pub cancelled: bool,
    /// Payload for `TreasuryRotation`: the proposed new treasury address.
    pub payload_address: Option<Address>,
    /// Payload for `FeeIncrease`: the proposed new fee in basis points.
    pub payload_u32: Option<u32>,
    /// Payload for `GlobalPause`: `true` = pause, `false` = unpause.
    pub payload_bool: Option<bool>,
}

/// Snapshot of the three-axis pause state for a given (collection, function) context.
///
/// Returned by `get_pause_matrix` for off-chain monitoring and emergency tooling.
/// `any_paused` is the same predicate used by `require_not_paused_ctx` internally.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseMatrix {
    /// Global circuit-breaker (`admin_pause` / `admin_unpause`).
    pub global: bool,
    /// True when the queried collection is individually paused.
    pub collection_paused: bool,
    /// True when the queried function name is individually paused.
    pub function_paused: bool,
    /// True when ANY of the three axes is active (mirrors `require_not_paused_ctx`).
    pub any_paused: bool,
}

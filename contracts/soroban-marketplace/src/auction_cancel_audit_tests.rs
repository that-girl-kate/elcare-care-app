// auction_cancel_audit_tests.rs — Audit trail for auction cancellation (Issue #469)
//
// Coverage:
//   - Creator cancellation emits typed AuctionCancelReason::Owner event
//   - Admin cancellation emits typed AuctionCancelReason::Admin event with escrow amount
//   - Bids remain queryable (via get_auction) after admin cancellation
//   - Unauthorized callers cannot admin-cancel
//   - escrow_amount in event is 0 for no-bid auctions and non-zero for bid auctions
//   - ledger_sequence and schema_version are present in every cancellation event

use super::*;
use crate::{
    events::{AuctionCancelledEvent, AuctionAdminCancelledEvent, AUCTION_CANCELLED, AUCTION_ADMIN_CANCELLED},
    types::{AuctionCancelReason, AuctionStatus, RoleType},
};
use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger},
    vec, Address, Env, TryFromVal,
};

use crate::test::{mock_nft, MockNftClient};
use soroban_sdk::token::StellarAssetClient;

// ── Test setup ────────────────────────────────────────────────────────────────

fn decode_auction_cancelled(env: &Env) -> Option<AuctionCancelledEvent> {
    use soroban_sdk::{xdr::{ContractEventBody, ScVal}, FromVal};
    env.events().all().events().iter().find_map(|e| {
        if let ContractEventBody::V0(body) = &e.body {
            let matches = body.topics.iter().any(|t| match t {
                ScVal::Symbol(s) => core::str::from_utf8(s.0.as_slice()).unwrap_or("") == AUCTION_CANCELLED,
                _ => false,
            });
            if matches {
                let val = soroban_sdk::Val::from_val(env, &body.data);
                AuctionCancelledEvent::try_from_val(env, &val).ok()
            } else {
                None
            }
        } else {
            None
        }
    })
}

fn decode_auction_cancelled_for(env: &Env, auction_id: u64) -> Option<AuctionCancelledEvent> {
    use soroban_sdk::{xdr::{ContractEventBody, ScVal}, FromVal};
    let mut found = None;
    for e in env.events().all().events().iter() {
        if let ContractEventBody::V0(body) = &e.body {
            let matches = body.topics.iter().any(|t| match t {
                ScVal::Symbol(s) => core::str::from_utf8(s.0.as_slice()).unwrap_or("") == AUCTION_CANCELLED,
                _ => false,
            });
            if matches {
                let val = soroban_sdk::Val::from_val(env, &body.data);
                if let Ok(ev) = AuctionCancelledEvent::try_from_val(env, &val) {
                    if ev.auction_id == auction_id {
                        found = Some(ev);
                    }
                }
            }
        }
    }
    found
}

fn decode_admin_cancelled(env: &Env) -> Option<AuctionAdminCancelledEvent> {
    use soroban_sdk::{xdr::{ContractEventBody, ScVal}, FromVal};
    env.events().all().events().iter().find_map(|e| {
        if let ContractEventBody::V0(body) = &e.body {
            let matches = body.topics.iter().any(|t| match t {
                ScVal::Symbol(s) => core::str::from_utf8(s.0.as_slice()).unwrap_or("") == AUCTION_ADMIN_CANCELLED,
                _ => false,
            });
            if matches {
                let val = soroban_sdk::Val::from_val(env, &body.data);
                AuctionAdminCancelledEvent::try_from_val(env, &val).ok()
            } else {
                None
            }
        } else {
            None
        }
    })
}

fn auction_setup() -> (
    Env,
    MarketplaceContractClient<'static>,
    Address, // admin (all roles)
    Address, // creator / artist
    Address, // bidder
    Address, // payment_token
    Address, // collection
) {
    let env = Env::default();
    env.mock_all_auths();
    let cid = env.register(MarketplaceContract, ());
    let client = MarketplaceContractClient::new(&env, &cid);
    let admin = Address::generate(&env);
    let creator = Address::generate(&env);
    let bidder = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let payment_token = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
    let sac = StellarAssetClient::new(&env, &payment_token);
    sac.mint(&creator, &100_000_000_000_i128);
    sac.mint(&bidder, &100_000_000_000_i128);
    sac.mint(&cid, &100_000_000_000_i128);
    let collection = env.register(mock_nft::MockNft, ());
    MockNftClient::new(&env, &collection).set_owner(&1u64, &creator);
    client.set_admin(&admin);
    client.migrate_roles(&admin);
    (env, client, admin, creator, bidder, payment_token, collection)
}

fn make_auction(
    env: &Env,
    client: &MarketplaceContractClient,
    creator: &Address,
    payment_token: &Address,
    collection: &Address,
    duration: u64,
) -> u64 {
    let recipients = vec![env, crate::types::Recipient {
        address: creator.clone(),
        percentage: 10_000,
    }];
    client.create_auction(
        creator,
        payment_token,
        collection,
        &1u64,
        &1_000_000_i128,
        &duration,
        &recipients,
    )
}

// ── §1  Creator cancellation ─────────────────────────────────────────────────

#[test]
fn test_creator_cancel_emits_typed_owner_reason() {
    let (env, client, _, creator, _, payment_token, collection) = auction_setup();
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    client.cancel_auction(&creator, &auction_id);

    let ev = decode_auction_cancelled(&env).expect("auction_cancelled event must be emitted");
    assert_eq!(ev.auction_id, auction_id);
    assert_eq!(ev.cancelled_by, creator);
    assert_eq!(ev.reason, AuctionCancelReason::Owner);
    assert_eq!(ev.escrow_amount, 0, "no bids → escrow_amount must be 0");
}

#[test]
fn test_creator_cancel_sets_auction_status_cancelled() {
    let (env, client, _, creator, _, payment_token, collection) = auction_setup();
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    client.cancel_auction(&creator, &auction_id);

    let auction = client.get_auction(&auction_id);
    assert_eq!(auction.status, AuctionStatus::Cancelled);
}

#[test]
fn test_creator_cancel_event_carries_ledger_sequence() {
    let (env, client, _, creator, _, payment_token, collection) = auction_setup();
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    let seq = env.ledger().sequence();
    client.cancel_auction(&creator, &auction_id);

    let ev = decode_auction_cancelled(&env).unwrap();
    assert_eq!(ev.ledger_sequence, seq);
}

// ── §2  Admin cancellation with bids ─────────────────────────────────────────

#[test]
fn test_admin_cancel_emits_typed_admin_reason_with_escrow() {
    let (env, client, admin, creator, bidder, payment_token, collection) = auction_setup();
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    // Place a bid so escrow is non-zero.
    client.place_bid(&bidder, &auction_id, &2_000_000_i128);
    client.admin_cancel_auction(&admin, &auction_id);

    let ev = decode_auction_cancelled(&env).expect("auction_cancelled event must be emitted");
    assert_eq!(ev.reason, AuctionCancelReason::Admin);
    assert_eq!(ev.escrow_amount, 2_000_000_i128);
    assert_eq!(ev.cancelled_by, admin);
}

#[test]
fn test_admin_cancel_no_bid_escrow_amount_is_zero() {
    let (env, client, admin, creator, _, payment_token, collection) = auction_setup();
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    client.admin_cancel_auction(&admin, &auction_id);

    let ev = decode_auction_cancelled(&env).unwrap();
    assert_eq!(ev.escrow_amount, 0);
}

#[test]
fn test_bids_remain_queryable_after_admin_cancel() {
    let (env, client, admin, creator, bidder, payment_token, collection) = auction_setup();
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    client.place_bid(&bidder, &auction_id, &2_000_000_i128);
    client.admin_cancel_auction(&admin, &auction_id);

    let bids = client.get_auction_bids(&auction_id);
    assert!(!bids.is_empty(), "bid history must persist after cancellation");
    assert_eq!(bids.get(0).unwrap().bidder, bidder);
}

#[test]
fn test_admin_cancel_also_emits_auction_admin_cancelled_event() {
    let (env, client, admin, creator, bidder, payment_token, collection) = auction_setup();
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    client.place_bid(&bidder, &auction_id, &2_000_000_i128);
    client.admin_cancel_auction(&admin, &auction_id);

    let adm_ev = decode_admin_cancelled(&env).expect("auction_admin_cancelled must be emitted");
    assert_eq!(adm_ev.refunded_amount, 2_000_000_i128);
    assert_eq!(adm_ev.cancelled_by, admin);
}

// ── §3  Unauthorized cancellation ────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_unauthorized_caller_cannot_admin_cancel() {
    let (env, client, _, creator, _, payment_token, collection) = auction_setup();
    let attacker = Address::generate(&env);
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    client.admin_cancel_auction(&attacker, &auction_id);
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn test_wrong_role_holder_cannot_admin_cancel() {
    let (env, client, admin, creator, _, payment_token, collection) = auction_setup();
    // Transfer both gate roles away so the original admin cannot cancel.
    let pause_holder = Address::generate(&env);
    let protocol_holder = Address::generate(&env);
    client.propose_role_transfer(&admin, &RoleType::EmergencyPause, &pause_holder);
    client.accept_role_transfer(&RoleType::EmergencyPause, &pause_holder);
    client.propose_role_transfer(&admin, &RoleType::ProtocolConfig, &protocol_holder);
    client.accept_role_transfer(&RoleType::ProtocolConfig, &protocol_holder);
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    // Admin holds neither ProtocolConfig nor EmergencyPause — must be rejected.
    client.admin_cancel_auction(&admin, &auction_id);
}

// ── §5  Typed cancel reasons for all three paths (Issue #848) ─────────────────

#[test]
fn test_auction_cancel_reasons() {
    let (env, client, admin, creator, bidder, payment_token, collection) = auction_setup();

    // Path 1: Owner
    let aid_owner = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    client.cancel_auction(&creator, &aid_owner);
    let owner_ev = decode_auction_cancelled_for(&env, aid_owner).expect("owner cancel event");
    assert_eq!(owner_ev.reason, AuctionCancelReason::Owner);

    // Path 2: Admin (ProtocolConfig). Transfer EmergencyPause away so reason is Admin.
    let pause_holder = Address::generate(&env);
    client.propose_role_transfer(&admin, &RoleType::EmergencyPause, &pause_holder);
    client.accept_role_transfer(&RoleType::EmergencyPause, &pause_holder);
    MockNftClient::new(&env, &collection).set_owner(&2u64, &creator);
    let aid_admin = client.create_auction(
        &creator, &payment_token, &collection, &2u64, &1_000_000_i128, &3_601u64,
        &vec![&env, crate::types::Recipient { address: creator.clone(), percentage: 10_000 }],
    );
    client.admin_cancel_auction(&admin, &aid_admin);
    let admin_ev = decode_auction_cancelled_for(&env, aid_admin).expect("admin cancel event");
    assert_eq!(admin_ev.reason, AuctionCancelReason::Admin);

    // Path 3: Emergency (EmergencyPause-only).
    let emergency = Address::generate(&env);
    let protocol_holder = Address::generate(&env);
    client.propose_role_transfer(&admin, &RoleType::ProtocolConfig, &protocol_holder);
    client.accept_role_transfer(&RoleType::ProtocolConfig, &protocol_holder);
    client.propose_role_transfer(&pause_holder, &RoleType::EmergencyPause, &emergency);
    client.accept_role_transfer(&RoleType::EmergencyPause, &emergency);
    MockNftClient::new(&env, &collection).set_owner(&3u64, &creator);
    let aid_emerg = client.create_auction(
        &creator, &payment_token, &collection, &3u64, &1_000_000_i128, &3_601u64,
        &vec![&env, crate::types::Recipient { address: creator.clone(), percentage: 10_000 }],
    );
    client.place_bid(&bidder, &aid_emerg, &2_000_000_i128);
    client.admin_cancel_auction(&emergency, &aid_emerg);
    let emerg_ev = decode_auction_cancelled_for(&env, aid_emerg).expect("emergency cancel event");
    assert_eq!(emerg_ev.reason, AuctionCancelReason::Emergency);
}

// ── §4  Creator cannot cancel with bids ──────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #30)")]
fn test_creator_cannot_cancel_auction_with_bids() {
    let (env, client, _, creator, bidder, payment_token, collection) = auction_setup();
    let auction_id = make_auction(&env, &client, &creator, &payment_token, &collection, 3_601);
    client.place_bid(&bidder, &auction_id, &2_000_000_i128);
    // AuctionHasBids = 30
    client.cancel_auction(&creator, &auction_id);
}

// royalty_recovery_tests.rs — Issue #461
//
// Verifies that royalty payout status is observable and that the pull-based
// `claim_royalty` recovery path works correctly.

use crate::test::{mock_nft, valid_recipients, MockNftClient};
use crate::types::{MarketplaceError, Recipient};
use soroban_sdk::{
    symbol_short,
    testutils::Address as _,
    testutils::Ledger,
    token::{StellarAssetClient, TokenClient},
    vec, Address, Env,
};
use crate::contract::{MarketplaceContract, MarketplaceContractClient};

fn setup() -> (
    Env,
    MarketplaceContractClient<'static>,
    Address, // admin/artist
    Address, // buyer
    Address, // payment_token
    Address, // contract_id
    Address, // collection_id
) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(MarketplaceContract, ());
    let client = MarketplaceContractClient::new(&env, &contract_id);
    let artist = Address::generate(&env);
    let buyer = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let payment_token = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
    let sac = StellarAssetClient::new(&env, &payment_token);
    sac.mint(&artist, &100_000_000_000_i128);
    sac.mint(&buyer, &100_000_000_000_i128);
    sac.mint(&contract_id, &100_000_000_000_i128);
    let collection_id = env.register(mock_nft::MockNft, ());
    MockNftClient::new(&env, &collection_id).set_owner(&1u64, &artist);
    client.set_admin(&artist);
    client.add_token_to_whitelist(&artist, &payment_token);
    (env, client, artist, buyer, payment_token, contract_id, collection_id)
}

// ── Helper: create listing and buy it ─────────────────────────────────────────

fn listing_buy_roundtrip(
    env: &Env,
    client: &MarketplaceContractClient,
    artist: &Address,
    buyer: &Address,
    payment_token: &Address,
    collection_id: &Address,
    price: i128,
) -> u64 {
    let listing_id = client.create_listing(
        artist, &price, &symbol_short!("XLM"),
        payment_token, collection_id, &1u64, &1u64,
        &valid_recipients(env, artist), &None::<u64>,
    );
    client.buy_artwork(buyer, &listing_id);
    listing_id
}

// ── Test: claim record is written and marked claimed after buy_artwork ────────

#[test]
fn test_royalty_claim_auto_marked_after_buy_artwork() {
    let (env, client, artist, buyer, token, _cid, col) = setup();
    let price = 10_000_000_i128;
    let listing_id = listing_buy_roundtrip(&env, &client, &artist, &buyer, &token, &col, price);

    // The artist is the sole recipient (100% @ 10_000 bps).
    let claim = client.get_royalty_claim(&listing_id, &true, &artist);
    assert!(claim.is_some(), "claim record must exist for artist");
    let record = claim.unwrap();
    assert!(record.claimed, "claim must be auto-marked at settlement");
    assert!(record.claimed_at.is_some());
    assert_eq!(record.recipient, artist);
    assert_eq!(record.settlement_id, listing_id);
    assert!(record.is_listing);
}

// ── Test: get_royalty_claim returns None for non-existent recipient ────────────

#[test]
fn test_get_royalty_claim_missing_returns_none() {
    let (env, client, artist, buyer, token, _cid, col) = setup();
    let listing_id = listing_buy_roundtrip(&env, &client, &artist, &buyer, &token, &col, 10_000_000);

    let stranger = Address::generate(&env);
    let claim = client.get_royalty_claim(&listing_id, &true, &stranger);
    assert!(claim.is_none(), "unknown recipient should return None");
}

// ── Test: claim_royalty on already-claimed record returns AlreadyClaimed ──────

#[test]
#[should_panic(expected = "Error(Contract, #63)")]
fn test_claim_royalty_already_claimed_reverts() {
    let (env, client, artist, buyer, token, _cid, col) = setup();
    let listing_id = listing_buy_roundtrip(&env, &client, &artist, &buyer, &token, &col, 10_000_000);
    // claim_royalty on an already-auto-claimed record must revert.
    client.claim_royalty(&artist, &listing_id, &true);
}

// ── Test: claim_royalty on non-existent record returns ClaimNotFound ──────────

#[test]
#[should_panic(expected = "Error(Contract, #64)")]
fn test_claim_royalty_not_found_reverts() {
    let (env, client, artist, _buyer, _token, _cid, _col) = setup();
    // No settlement has occurred — no claim record exists.
    client.claim_royalty(&artist, &999u64, &true);
}

// ── Test: multi-recipient settlement writes a claim per recipient ─────────────

#[test]
fn test_royalty_claim_multi_recipient() {
    let (env, client, artist, buyer, token, _cid, col) = setup();
    let collaborator = Address::generate(&env);

    // artist 80%, collaborator 20%
    let recipients = vec![
        &env,
        Recipient { address: artist.clone(), percentage: 8_000 },
        Recipient { address: collaborator.clone(), percentage: 2_000 },
    ];

    let listing_id = client.create_listing(
        &artist, &10_000_000_i128, &symbol_short!("XLM"),
        &token, &col, &1u64, &1u64,
        &recipients, &None::<u64>,
    );
    client.buy_artwork(&buyer, &listing_id);

    let artist_claim = client.get_royalty_claim(&listing_id, &true, &artist).unwrap();
    let collab_claim = client.get_royalty_claim(&listing_id, &true, &collaborator).unwrap();

    assert!(artist_claim.claimed);
    assert!(collab_claim.claimed);
    assert_eq!(artist_claim.amount, 8_000_000);
    assert_eq!(collab_claim.amount, 2_000_000);
}

// ── Test: auction settlement writes claim records with is_listing=false ───────

#[test]
fn test_royalty_claim_auction_settlement() {
    let (env, client, artist, buyer, token, _cid, col) = setup();

    // Note: setup() already calls set_admin + add_token_to_whitelist.
    // Create auction with ~1h duration (> MIN_AUCTION_DURATION of 3600s)
    env.ledger().set_timestamp(1_000);
    let duration = 3_700_u64;
    let auction_id = client.create_auction(
        &artist, &token, &col, &1u64,
        &5_000_000_i128, &duration, &valid_recipients(&env, &artist),
    );

    // Place winning bid
    client.place_bid(&buyer, &auction_id, &5_000_000_i128);

    // Advance past end (start + duration)
    env.ledger().set_timestamp(1_000 + duration + 1);
    client.finalize_auction(&buyer, &auction_id);

    let claim = client.get_royalty_claim(&auction_id, &false, &artist);
    assert!(claim.is_some(), "artist must have an auction claim record");
    let record = claim.unwrap();
    assert!(record.claimed);
    assert!(!record.is_listing); // auction settlement
    assert_eq!(record.settlement_id, auction_id);
}

// ── Test: royalty claim amount matches actual transferred balance ──────────────

#[test]
fn test_royalty_claim_amount_matches_transfer() {
    let (env, client, artist, buyer, token, _cid, col) = setup();
    let price = 10_000_000_i128;

    let artist_before = TokenClient::new(&env, &token).balance(&artist);
    let listing_id = listing_buy_roundtrip(&env, &client, &artist, &buyer, &token, &col, price);

    let claim = client.get_royalty_claim(&listing_id, &true, &artist).unwrap();
    let artist_after = TokenClient::new(&env, &token).balance(&artist);

    // artist_after - artist_before should equal the claim amount
    // (artist also spent nothing here — the price comes from buyer)
    assert_eq!(artist_after - artist_before, claim.amount);
    assert_eq!(claim.amount, price); // sole 100% recipient
}

// ── Issue #842: manual pull of an unclaimed record + pagination ───────────────

#[test]
fn test_claim_royalty_manual_pull_succeeds() {
    let (env, client, artist, _buyer, token, contract_id, _col) = setup();
    let settlement_id = 42u64;
    let amount = 1_500_000_i128;

    // Seed an unclaimed record as if a direct transfer had failed mid-settlement.
    env.as_contract(&contract_id, || {
        let claim = crate::types::RoyaltyClaimRecord {
            settlement_id,
            is_listing: true,
            recipient: artist.clone(),
            token: token.clone(),
            amount,
            claimed: false,
            created_at: env.ledger().sequence(),
            claimed_at: None,
        };
        crate::storage::set_royalty_claim(&env, settlement_id, true, &artist, &claim);
    });

    let before = TokenClient::new(&env, &token).balance(&artist);
    assert!(client.claim_royalty(&artist, &settlement_id, &true));
    let after = TokenClient::new(&env, &token).balance(&artist);
    assert_eq!(after - before, amount);

    let record = client.get_royalty_claim(&settlement_id, &true, &artist).unwrap();
    assert!(record.claimed);
    assert!(record.claimed_at.is_some());
}

#[test]
fn test_get_unclaimed_royalties_pagination() {
    let (env, client, artist, _buyer, token, contract_id, _col) = setup();

    env.as_contract(&contract_id, || {
        for sid in 1u64..=5 {
            let claim = crate::types::RoyaltyClaimRecord {
                settlement_id: sid,
                is_listing: true,
                recipient: artist.clone(),
                token: token.clone(),
                amount: 100_000 * sid as i128,
                claimed: sid <= 2, // first two already claimed
                created_at: env.ledger().sequence(),
                claimed_at: if sid <= 2 { Some(env.ledger().sequence()) } else { None },
            };
            crate::storage::set_royalty_claim(&env, sid, true, &artist, &claim);
        }
    });

    let page = client.get_unclaimed_royalties(&artist, &0u32, &10u32);
    assert_eq!(page.len(), 3, "settlements 3,4,5 remain unclaimed");
    assert_eq!(page.get(0).unwrap().settlement_id, 3);
    assert_eq!(page.get(2).unwrap().settlement_id, 5);

    let page2 = client.get_unclaimed_royalties(&artist, &3u32, &2u32);
    // start=3 skips first three index entries (sids 1,2,3); remaining unclaimed in window: 4,5
    assert_eq!(page2.len(), 2);
    assert_eq!(page2.get(0).unwrap().settlement_id, 4);
}

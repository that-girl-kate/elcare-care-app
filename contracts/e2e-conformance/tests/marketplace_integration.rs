// Required conformance coverage — do not #[ignore] without justification comment.
//
// Prerequisite: build collection WASM (see tests/common.rs).

mod common;

use common::{deploy_normal_721, setup_launchpad};
use soroban_marketplace::{
    ListingStatus, MarketplaceContract, MarketplaceContractClient, Recipient,
};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger as _},
    token::{StellarAssetClient, TokenClient},
    vec, Address, Env, IntoVal, String, Vec,
};

fn valid_recipients(env: &Env, artist: &Address) -> soroban_sdk::Vec<Recipient> {
    vec![env, Recipient {
        address: artist.clone(),
        percentage: 10_000,
    }]
}

fn setup_marketplace(
    env: &Env,
) -> (
    MarketplaceContractClient<'_>,
    Address,
    Address,
    Address,
    Address,
) {
    env.mock_all_auths();
    let contract_id = env.register(MarketplaceContract, ());
    let client = MarketplaceContractClient::new(env, &contract_id);
    let artist = Address::generate(env);
    let buyer = Address::generate(env);
    let token_admin = Address::generate(env);
    let payment_token = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let sac = StellarAssetClient::new(env, &payment_token);
    sac.mint(&artist, &100_000_000_000_i128);
    sac.mint(&buyer, &100_000_000_000_i128);
    sac.mint(&contract_id, &100_000_000_000_i128);
    (client, artist, buyer, payment_token, contract_id)
}

fn mint_721_to_artist(env: &Env, collection: &Address, _creator: &Address, artist: &Address) -> u64 {
    let uri = String::from_str(env, "ipfs://marketplace-e2e");
    env.invoke_contract::<u64>(
        collection,
        &soroban_sdk::Symbol::new(env, "mint"),
        Vec::from_array(env, [artist.to_val(), uri.to_val()]),
    )
}

#[test]
#[should_panic(expected = "Error(Contract, #60)")]
fn erc721_listing_rejects_quantity_gt_one_with_collection_incompatible() {
    let env = Env::default();
    env.ledger().with_mut(|li| li.sequence_number = 1);
    let (launchpad, _admin, _fee, creator) = setup_launchpad(&env);
    let collection = deploy_normal_721(&env, &launchpad, &creator, 0xB1);

    let (mp, artist, _buyer, payment_token, _mp_id) = setup_marketplace(&env);
    mp.set_admin(&artist);
    mp.add_token_to_whitelist(&artist, &payment_token);

    let token_id = mint_721_to_artist(&env, &collection, &creator, &artist);

    env.invoke_contract::<()>(
        &collection,
        &soroban_sdk::Symbol::new(&env, "set_approval_for_all"),
        Vec::from_array(
            &env,
            [
                artist.to_val(),
                mp.address.to_val(),
                true.into_val(&env),
                Option::<u32>::None.into_val(&env),
            ],
        ),
    );

    let _ = mp.create_listing(
        &artist,
        &1_000_000_i128,
        &symbol_short!("XLM"),
        &payment_token,
        &collection,
        &token_id,
        &5u64,
        &valid_recipients(&env, &artist),
        &None::<u64>,
    );
}

#[test]
fn erc721_listing_buy_transfers_with_royalty_bps_configured() {
    let env = Env::default();
    env.ledger().with_mut(|li| li.sequence_number = 1);
    let (launchpad, _admin, _fee, creator) = setup_launchpad(&env);
    let collection = deploy_normal_721(&env, &launchpad, &creator, 0xB2);

    let (mp, artist, buyer, payment_token, _mp_id) = setup_marketplace(&env);
    mp.set_admin(&artist);
    mp.add_token_to_whitelist(&artist, &payment_token);

    let token_id = mint_721_to_artist(&env, &collection, &creator, &artist);

    env.invoke_contract::<()>(
        &collection,
        &soroban_sdk::Symbol::new(&env, "set_approval_for_all"),
        Vec::from_array(
            &env,
            [
                artist.to_val(),
                mp.address.to_val(),
                true.into_val(&env),
                Option::<u32>::None.into_val(&env),
            ],
        ),
    );

    let listing_id = mp.create_listing(
        &artist,
        &10_000_000_i128,
        &symbol_short!("XLM"),
        &payment_token,
        &collection,
        &token_id,
        &1u64,
        &valid_recipients(&env, &artist),
        &None::<u64>,
    );
    assert_eq!(
        mp.get_listing(&listing_id).status,
        ListingStatus::Active
    );

    let buyer_before = TokenClient::new(&env, &payment_token).balance(&buyer);
    mp.buy_artwork(&buyer, &listing_id);
    assert_eq!(
        mp.get_listing(&listing_id).status,
        ListingStatus::Sold
    );

    let owner: Address = env.invoke_contract(
        &collection,
        &soroban_sdk::Symbol::new(&env, "owner_of"),
        Vec::from_array(&env, [token_id.into_val(&env)]),
    );
    assert_eq!(owner, buyer, "buyer should own token after purchase");

    let buyer_after = TokenClient::new(&env, &payment_token).balance(&buyer);
    assert!(
        buyer_after < buyer_before,
        "buyer payment token balance should decrease after buy_artwork"
    );
}

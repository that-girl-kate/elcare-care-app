//! Shared Launchpad + WASM deploy helpers for e2e conformance integration tests.
//!
//! Build collection WASM before running tests:
//! ```text
//! cargo build --target wasm32v1-none --release \
//!   -p collection-nft-erc721 -p collection-nft-erc1155 \
//!   -p lazy-mint-erc721 -p lazy-mint-erc1155
//! ```

extern crate std;

use soroban_launchpad::{Launchpad, LaunchpadClient};
use soroban_sdk::{
    testutils::Address as _,
    token::StellarAssetClient,
    Address, BytesN, Env, String,
};

pub fn wasm_bytes(name: &str) -> std::vec::Vec<u8> {
    let exe = std::env::current_exe().unwrap();
    let target_dir = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .unwrap()
        .to_path_buf();
    let path = target_dir
        .join("wasm32v1-none")
        .join("release")
        .join(std::format!("{name}.wasm"));

    std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "missing wasm at {}. build first:\n  cargo build --target wasm32v1-none --release \
             -p collection-nft-erc721 -p collection-nft-erc1155 -p lazy-mint-erc721 -p lazy-mint-erc1155",
            path.display()
        )
    })
}

pub fn setup_launchpad(env: &Env) -> (LaunchpadClient<'_>, Address, Address, Address) {
    env.mock_all_auths();

    let launchpad_id = env.register(Launchpad, ());
    let client = LaunchpadClient::new(env, &launchpad_id);

    let admin = Address::generate(env);
    let fee_receiver = Address::generate(env);
    let creator = Address::generate(env);

    client.initialize(&admin, &fee_receiver, &0i128);

    let wasm_normal_721 = env
        .deployer()
        .upload_contract_wasm(wasm_bytes("collection_nft_erc721").as_slice());
    let wasm_normal_1155 = env
        .deployer()
        .upload_contract_wasm(wasm_bytes("collection_nft_erc1155").as_slice());
    let wasm_lazy_721 = env
        .deployer()
        .upload_contract_wasm(wasm_bytes("lazy_mint_erc721").as_slice());
    let wasm_lazy_1155 = env
        .deployer()
        .upload_contract_wasm(wasm_bytes("lazy_mint_erc1155").as_slice());

    client.set_wasm_hashes(
        &wasm_normal_721,
        &wasm_normal_1155,
        &wasm_lazy_721,
        &wasm_lazy_1155,
    );

    (client, admin, fee_receiver, creator)
}

pub fn setup_token(env: &Env, holder: &Address, amount: i128) -> Address {
    let token_admin = Address::generate(env);
    let token = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    StellarAssetClient::new(env, &token).mint(holder, &amount);
    token
}

pub fn deploy_normal_721(
    env: &Env,
    client: &LaunchpadClient<'_>,
    creator: &Address,
    salt_byte: u8,
) -> Address {
    let salt = BytesN::from_array(env, &[salt_byte; 32]);
    let royalty_receiver = Address::generate(env);
    let currency = setup_token(env, creator, 1_000_000);
    client.deploy_normal_721(
        creator,
        &currency,
        &String::from_str(env, "E2E Conformance 721"),
        &String::from_str(env, "E2E721"),
        &1_000u64,
        &500u32,
        &royalty_receiver,
        &0u32,
        &salt,
    )
}

pub fn deploy_normal_1155(
    env: &Env,
    client: &LaunchpadClient<'_>,
    creator: &Address,
    salt_byte: u8,
) -> Address {
    let salt = BytesN::from_array(env, &[salt_byte; 32]);
    let royalty_receiver = Address::generate(env);
    let currency = setup_token(env, creator, 1_000_000);
    client.deploy_normal_1155(
        creator,
        &currency,
        &String::from_str(env, "E2E Conformance 1155"),
        &500u32,
        &royalty_receiver,
        &0u32,
        &salt,
    )
}

pub fn deploy_lazy_721(
    env: &Env,
    client: &LaunchpadClient<'_>,
    creator: &Address,
    salt_byte: u8,
) -> Address {
    let salt = BytesN::from_array(env, &[salt_byte; 32]);
    let royalty_receiver = Address::generate(env);
    let currency = setup_token(env, creator, 1_000_000);
    let creator_pubkey = BytesN::from_array(env, &[0x11u8; 32]);
    client.deploy_lazy_721(
        creator,
        &currency,
        &creator_pubkey,
        &String::from_str(env, "E2E Lazy 721"),
        &String::from_str(env, "E2LZ721"),
        &1_000u64,
        &500u32,
        &royalty_receiver,
        &0u32,
        &salt,
        &String::from_str(env, "Test Network; September 2015"),
    )
}

pub fn deploy_lazy_1155(
    env: &Env,
    client: &LaunchpadClient<'_>,
    creator: &Address,
    salt_byte: u8,
) -> Address {
    let salt = BytesN::from_array(env, &[salt_byte; 32]);
    let royalty_receiver = Address::generate(env);
    let currency = setup_token(env, creator, 1_000_000);
    let creator_pubkey = BytesN::from_array(env, &[0x22u8; 32]);
    client.deploy_lazy_1155(
        creator,
        &currency,
        &creator_pubkey,
        &String::from_str(env, "E2E Lazy 1155"),
        &500u32,
        &royalty_receiver,
        &0u32,
        &salt,
        &String::from_str(env, "Test Network; September 2015"),
    )
}

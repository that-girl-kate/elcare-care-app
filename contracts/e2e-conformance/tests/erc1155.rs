// Required conformance coverage — do not #[ignore] without justification comment.
//
// Prerequisite: build collection WASM (see tests/common.rs).

mod common;

use common::{deploy_normal_1155, setup_launchpad};
use e2e_conformance::CollectionConformance;
use soroban_sdk::{testutils::Ledger as _, Env};

#[test]
fn launchpad_deployed_erc1155_passes_conformance_suite() {
    let env = Env::default();
    env.ledger().with_mut(|li| li.sequence_number = 1);
    let (client, _admin, _fee, creator) = setup_launchpad(&env);
    let collection = deploy_normal_1155(&env, &client, &creator, 0x15);

    CollectionConformance {
        env: &env,
        collection,
        creator: creator.clone(),
        kind: "ERC1155",
    }
    .run_erc1155_suite();
}

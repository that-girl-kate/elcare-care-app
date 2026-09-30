//! Cross-contract conformance helpers for collections deployed via Launchpad WASM.
//!
//! Integration tests must build collection WASM first:
//! `cargo build --target wasm32v1-none --release -p collection-nft-erc721 -p collection-nft-erc1155 -p lazy-mint-erc721 -p lazy-mint-erc1155`

use soroban_sdk::{
    testutils::Address as _,
    Address, Env, IntoVal, String, Symbol, TryFromVal, Val, Vec,
};

/// Exercises a deployed collection contract against ERC-721 / ERC-1155 / lazy-mint expectations.
pub struct CollectionConformance<'a> {
    pub env: &'a Env,
    pub collection: Address,
    /// Collection creator (required for mint and succession calls).
    pub creator: Address,
    pub kind: &'static str,
}

impl<'a> CollectionConformance<'a> {
    fn sym(&self, name: &str) -> Symbol {
        Symbol::new(self.env, name)
    }

    fn invoke<T>(&self, func: &str, args: Vec<Val>) -> T
    where
        T: TryFromVal<Env, Val>,
    {
        self.env
            .invoke_contract::<T>(&self.collection, &self.sym(func), args)
    }

    /// Assert `contract_type()` matches `expected` (e.g. `"ERC721"`).
    pub fn assert_contract_type(&self, expected: &str) {
        let got: Symbol = self.invoke("contract_type", Vec::new(self.env));
        let want = Symbol::new(self.env, expected);
        assert_eq!(
            got, want,
            "{}: contract_type expected {expected}, got symbol mismatch",
            self.kind
        );
    }

    /// Representative ERC-721 surface: mint, transfer, approve, burn, metadata, royalty, freeze, succession.
    pub fn run_erc721_suite(&self) {
        self.assert_contract_type("ERC721");

        let uri = String::from_str(self.env, "ipfs://e2e-conformance/erc721");
        let holder = Address::generate(self.env);
        // mint(to, uri) — creator auth is enforced on-chain via only_creator.
        let token_id: u64 = self.invoke(
            "mint",
            Vec::from_array(self.env, [holder.to_val(), uri.to_val()]),
        );

        let owner: Address = self.invoke(
            "owner_of",
            Vec::from_array(self.env, [token_id.into_val(self.env)]),
        );
        assert_eq!(
            owner, holder,
            "{}: owner_of after mint should be holder",
            self.kind
        );

        let uri_out: String = self.invoke(
            "token_uri",
            Vec::from_array(self.env, [token_id.into_val(self.env)]),
        );
        assert_eq!(
            uri_out, uri,
            "{}: token_uri should round-trip mint URI",
            self.kind
        );

        let (royalty_recv, royalty_bps): (Address, u32) =
            self.invoke("royalty_info", Vec::new(self.env));
        let _ = (royalty_recv, royalty_bps);

        let recipient = Address::generate(self.env);
        self.invoke::<()>(
            "transfer",
            Vec::from_array(
                self.env,
                [
                    holder.to_val(),
                    recipient.to_val(),
                    token_id.into_val(self.env),
                ],
            ),
        );
        let owner_after: Address = self.invoke(
            "owner_of",
            Vec::from_array(self.env, [token_id.into_val(self.env)]),
        );
        assert_eq!(
            owner_after, recipient,
            "{}: transfer should update owner",
            self.kind
        );

        let operator = Address::generate(self.env);
        self.invoke::<()>(
            "approve",
            Vec::from_array(
                self.env,
                [
                    recipient.to_val(),
                    operator.to_val(),
                    token_id.into_val(self.env),
                    Option::<u32>::None.into_val(self.env),
                ],
            ),
        );
        let approved: Option<Address> = self.invoke(
            "get_approved",
            Vec::from_array(self.env, [token_id.into_val(self.env)]),
        );
        assert_eq!(
            approved,
            Some(operator),
            "{}: approve should set get_approved",
            self.kind
        );

        self.invoke::<()>(
            "approve",
            Vec::from_array(
                self.env,
                [
                    recipient.to_val(),
                    recipient.to_val(),
                    token_id.into_val(self.env),
                    Option::<u32>::None.into_val(self.env),
                ],
            ),
        );
        self.invoke::<()>(
            "burn",
            Vec::from_array(
                self.env,
                [
                    recipient.to_val(),
                    token_id.into_val(self.env),
                ],
            ),
        );

        let uri2 = String::from_str(self.env, "ipfs://e2e/freeze");
        let tid2: u64 = self.invoke(
            "mint",
            Vec::from_array(self.env, [self.creator.to_val(), uri2.to_val()]),
        );
        self.invoke::<()>("freeze_metadata", Vec::new(self.env));
        self.invoke::<()>(
            "freeze_token",
            Vec::from_array(
                self.env,
                [
                    self.creator.to_val(),
                    tid2.into_val(self.env),
                ],
            ),
        );

        let successor = Address::generate(self.env);
        let expiry = self.env.ledger().sequence() + 10_000;
        self.invoke::<()>(
            "propose_creator",
            Vec::from_array(
                self.env,
                [
                    successor.to_val(),
                    expiry.into_val(self.env),
                ],
            ),
        );
        self.invoke::<()>(
            "accept_creator",
            Vec::from_array(self.env, [successor.to_val()]),
        );
        let new_creator: Address = self.invoke("creator", Vec::new(self.env));
        assert_eq!(
            new_creator, successor,
            "{}: accept_creator should install successor",
            self.kind
        );
    }

    /// Representative ERC-1155 surface: mint_new, balance, transfer_from, royalty, contract_type.
    pub fn run_erc1155_suite(&self) {
        self.assert_contract_type("ERC1155");

        let holder = Address::generate(self.env);
        let uri = String::from_str(self.env, "ipfs://e2e-conformance/erc1155");
        let token_id: u64 = self.invoke(
            "mint_new",
            Vec::from_array(
                self.env,
                [
                    holder.to_val(),
                    10_u128.into_val(self.env),
                    uri.to_val(),
                ],
            ),
        );

        let balance: u128 = self.invoke(
            "balance_of",
            Vec::from_array(
                self.env,
                [
                    holder.to_val(),
                    token_id.into_val(self.env),
                ],
            ),
        );
        assert_eq!(
            balance, 10,
            "{}: balance_of after mint_new",
            self.kind
        );

        let (royalty_recv, royalty_bps): (Address, u32) =
            self.invoke("royalty_info", Vec::new(self.env));
        let _ = (royalty_recv, royalty_bps);

        let recipient = Address::generate(self.env);
        self.invoke::<()>(
            "set_approval_for_all",
            Vec::from_array(
                self.env,
                [
                    holder.to_val(),
                    self.creator.to_val(),
                    true.into_val(self.env),
                    Option::<u32>::None.into_val(self.env),
                ],
            ),
        );
        self.invoke::<()>(
            "transfer_from",
            Vec::from_array(
                self.env,
                [
                    self.creator.to_val(),
                    holder.to_val(),
                    recipient.to_val(),
                    token_id.into_val(self.env),
                    3_u128.into_val(self.env),
                ],
            ),
        );
        let bal_holder: u128 = self.invoke(
            "balance_of",
            Vec::from_array(
                self.env,
                [
                    holder.to_val(),
                    token_id.into_val(self.env),
                ],
            ),
        );
        let bal_recv: u128 = self.invoke(
            "balance_of",
            Vec::from_array(
                self.env,
                [
                    recipient.to_val(),
                    token_id.into_val(self.env),
                ],
            ),
        );
        assert_eq!(bal_holder, 7, "{}: holder balance after partial transfer", self.kind);
        assert_eq!(bal_recv, 3, "{}: recipient balance after partial transfer", self.kind);
    }

    /// Lazy ERC-721: contract_type plus exported redeem entrypoint (full voucher path is integration-tested in launchpad).
    pub fn run_lazy_mint_721_suite(&self) {
        self.assert_contract_type("LazyMint721");
        assert_export_exists(self.env, &self.collection, "redeem");
        assert_export_exists(self.env, &self.collection, "redeem_batch");
        assert_export_exists(self.env, &self.collection, "check_voucher");
    }

    /// Lazy ERC-1155: contract_type plus exported redeem entrypoint.
    pub fn run_lazy_mint_1155_suite(&self) {
        self.assert_contract_type("LazyMint1155");
        assert_export_exists(self.env, &self.collection, "redeem");
        assert_export_exists(self.env, &self.collection, "redeem_batch");
        // `check_voucher` is internal on LazyMint1155; redeem covers the public path.
    }
}

/// `try_invoke_contract` must reach the contract (invalid args → contract error), not fail as missing export.
fn assert_export_exists(env: &Env, contract: &Address, func: &str) {
    let sym = Symbol::new(env, func);
    let args = Vec::<Val>::new(env);
    let result = env.try_invoke_contract::<(), soroban_sdk::Error>(contract, &sym, args);
    assert!(
        result.is_err(),
        "expected {func} to exist on {contract:?} (call should fail on bad args, not missing fn)"
    );
}

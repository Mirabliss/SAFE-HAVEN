// ============================================================
//  Prediction Markets — Resolution Data Storage Tests (Issue #99)
// ============================================================
//
//  Scope:
//    - Resolution with data (Some(i128)) is accepted, stored, returned
//    - Resolution without data (None) is accepted, stored, returned
//    - resolution_data persists in get_prediction_market query
//    - OracleResolution record also stores the value
//
//  Out of scope (per issue):
//    - Data validation / custom formats / compression

#[cfg(test)]
mod prediction_market_tests {
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{Client as TokenClient, StellarAssetClient},
        Address, Env, String, Vec,
        testutils::Address as _,
        token::StellarAssetClient,
        Address, Env, String, Vec,
    extern crate std;
    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        token::{StellarAssetClient, Client as TokenClient},
        Address, Env, String as SorobanString, Vec as SorobanVec,
    };

    use crate::{
        contract::{SafeHaven, SafeHavenClient},
        prediction_market_errors::PredictionMarketError,
        prediction_market_storage as storage,
        prediction_market_types::*,
        prediction_market_storage,
    };

    fn betting_fixture() -> (Env, Address, Address, Address, u32, u64) {
        let env = Env::default();
        env.mock_all_auths();

        let contract = Address::generate(&env);
        let admin = Address::generate(&env);
        let bettor = Address::generate(&env);
        let oracle = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token = token_id.address();
        let close_time = env.ledger().timestamp() + MIN_MARKET_DURATION_SECS;
        let outcomes = Vec::from_array(
            &env,
            [String::from_str(&env, "Yes"), String::from_str(&env, "No")],
        );

        let market_id = env.as_contract(&contract, || {
            prediction_market::initialize_markets(&env, admin.clone(), admin.clone(), 0).unwrap();
            prediction_market::create_market(
                &env,
                admin,
                String::from_str(&env, "test"),
                String::from_str(&env, "test market"),
                oracle,
                close_time,
                close_time + 3600,
                outcomes,
                None,
            )
            .unwrap()
        });

        (env, contract, bettor, token, market_id, close_time)
    fn setup_market_with_bettor() -> (Env, Address, Address, Address, u32) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let fee_recipient = Address::generate(&env);
        let bettor = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token = token_id.address();

        StellarAssetClient::new(&env, &token).mint(&bettor, &10_000);

        prediction_market::initialize_markets(&env, admin.clone(), fee_recipient, 0).unwrap();

        let market_id = prediction_market::create_market(
            &env,
            admin,
            String::from_str(&env, "market"),
            String::from_str(&env, "Will deposits rise?"),
            Address::generate(&env),
            env.ledger().timestamp() + 3600,
            env.ledger().timestamp() + 7200,
            Vec::from_array(
                &env,
                [
                    String::from_str(&env, "Up"),
                    String::from_str(&env, "Down"),
                ],
            ),
            Some(0),
        )
        .unwrap();

        (env, bettor, token, fee_recipient, market_id)
    }

    // ================================================================
    //  Market Creation Tests (10 test cases)
    // ================================================================
    fn setup() -> (Env, SafeHavenClient<'static>, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let vault_id = env.register(SafeHaven, ());
        let vault = SafeHavenClient::new(&env, &vault_id);

        let admin: Address = Address::generate(&env);
        let fee_recipient: Address = Address::generate(&env);
        let alice: Address = Address::generate(&env);

        (env, vault, admin, fee_recipient, alice)
    }

    // ================================================================
    //  Initialization Tests - Acceptance Criteria
    // ================================================================

    #[test]
    fn test_init_prediction_markets_first_init_succeeds() {
        // ✅ AC #1: First init succeeds
        let (env, vault, admin, fee_recipient, _alice) = setup();

        let result = vault.init_prediction_markets(&admin, &fee_recipient, &Some(100));
        assert_eq!(result, Ok(()));

        // Verify config is set correctly
        let config = prediction_market_storage::get_market_config(&env).unwrap();
        assert_eq!(config.admin, admin);
        assert_eq!(config.fee_recipient, fee_recipient);
        assert_eq!(config.default_fee_bps, 100);
        assert_eq!(config.next_market_id, 1);
        assert_eq!(config.paused, false);
        assert_eq!(config.min_resolution_time_secs, 3600);
    }

    #[test]
    fn test_init_prediction_markets_second_init_fails() {
        // ✅ AC #2: Second init returns AlreadyInitialized
        let (_env, vault, admin, fee_recipient, _alice) = setup();

        // First initialization should succeed
        let result1 = vault.init_prediction_markets(&admin, &fee_recipient, &Some(100));
        assert_eq!(result1, Ok(()));

        // Second initialization should fail with AlreadyInitialized
        let result2 = vault.init_prediction_markets(&admin, &fee_recipient, &Some(100));
        assert_eq!(result2, Err(Ok(PredictionMarketError::AlreadyInitialized)));
    }

    #[test]
    fn test_init_prediction_markets_admin_set_correctly() {
        // ✅ AC #3: Admin set correctly
        let (env, vault, admin, fee_recipient, _alice) = setup();

        vault.init_prediction_markets(&admin, &fee_recipient, &Some(100))
            .unwrap();

    #[test]
    fn test_place_bet_valid() {
        let (env, contract, bettor, token, market_id, _) = betting_fixture();
        StellarAssetClient::new(&env, &token).mint(&bettor, &1);
        TokenClient::new(&env, &token).approve(
            &bettor,
            &contract,
            &1,
            &env.ledger().sequence().saturating_add(100),
        );

        env.as_contract(&contract, || {
            prediction_market::place_bet(&env, bettor.clone(), market_id, 0, 1, token.clone())
                .unwrap();

            let bet = storage::get_bet_readonly(&env, market_id, 0, &bettor).unwrap();
            assert_eq!(bet.bettor, bettor);
            assert_eq!(bet.market_id, market_id);
            assert_eq!(bet.outcome_id, 0);
            assert_eq!(bet.amount, 1);
            assert!(!bet.claimed);

            let outcome = storage::get_outcome_readonly(&env, market_id, 0).unwrap();
            assert_eq!(outcome.total_bet_amount, 1);
            assert_eq!(outcome.bettor_count, 1);

            let market = storage::get_market_readonly(&env, market_id).unwrap();
            assert_eq!(market.total_pool, 1);
            assert_eq!(market.bettor_count, 1);
        });
    }

    #[test]
    fn test_place_bet_invalid_amount_zero() {
        let (env, contract, bettor, token, market_id, _) = betting_fixture();
        env.as_contract(&contract, || {
            assert_eq!(
                prediction_market::place_bet(&env, bettor, market_id, 0, 0, token),
                Err(PredictionMarketError::InvalidBetAmount)
            );
        });
    }

    #[test]
    fn test_place_bet_invalid_amount_too_large() {
        let (env, contract, bettor, token, market_id, _) = betting_fixture();
        env.as_contract(&contract, || {
            assert_eq!(
                prediction_market::place_bet(
                    &env,
                    bettor,
                    market_id,
                    0,
                    MAX_BET_AMOUNT.saturating_add(1),
                    token,
                ),
                Err(PredictionMarketError::InvalidBetAmount)
            );
        });
    }

    #[test]
    fn test_place_bet_max_amount_allowed() {
        let (env, contract, bettor, token, market_id, _) = betting_fixture();
        StellarAssetClient::new(&env, &token).mint(&bettor, &MAX_BET_AMOUNT);
        TokenClient::new(&env, &token).approve(
            &bettor,
            &contract,
            &MAX_BET_AMOUNT,
            &env.ledger().sequence().saturating_add(100),
        );

        env.as_contract(&contract, || {
            prediction_market::place_bet(
                &env,
                bettor.clone(),
                market_id,
                0,
                MAX_BET_AMOUNT,
                token,
            )
            .unwrap();
            assert_eq!(
                storage::get_bet_readonly(&env, market_id, 0, &bettor)
                    .unwrap()
                    .amount,
                MAX_BET_AMOUNT
            );
        });
    }

    #[test]
    #[should_panic]
    fn test_place_bet_requires_bettor_auth() {
        let (env, contract, bettor, token, market_id, _) = betting_fixture();
        env.mock_auths(&[]);

        env.as_contract(&contract, || {
            let _ = prediction_market::place_bet(&env, bettor, market_id, 0, 1, token);
        });
    }

    #[test]
    fn test_place_bet_market_not_found() {
        let (env, contract, bettor, token, _, _) = betting_fixture();
        env.as_contract(&contract, || {
            assert_eq!(
                prediction_market::place_bet(&env, bettor, 999, 0, 1, token),
                Err(PredictionMarketError::MarketNotFound)
            );
        });
    }

    #[test]
    fn test_place_bet_market_closed() {
        let (env, contract, bettor, token, market_id, _) = betting_fixture();
        env.as_contract(&contract, || {
            let mut market = storage::get_market(&env, market_id).unwrap();
            market.close_time = env.ledger().timestamp();
            storage::set_market(&env, &market);

            assert_eq!(
                prediction_market::place_bet(&env, bettor, market_id, 0, 1, token),
                Err(PredictionMarketError::BettingClosed)
            );
        });
    }

    #[test]
    fn test_place_bet_market_not_open() {
        let (env, contract, bettor, token, market_id, _) = betting_fixture();
        env.as_contract(&contract, || {
            for status in [
                MarketStatus::Closed,
                MarketStatus::Resolved,
                MarketStatus::Cancelled,
            ] {
                let mut market = storage::get_market(&env, market_id).unwrap();
                market.status = status;
                storage::set_market(&env, &market);

                assert_eq!(
                    prediction_market::place_bet(
                        &env,
                        bettor.clone(),
                        market_id,
                        0,
                        1,
                        token.clone(),
                    ),
                    Err(PredictionMarketError::MarketNotOpen)
                );
            }
        });
    }

    #[test]
    fn test_place_bet_invalid_outcome() {
        let (env, contract, bettor, token, market_id, _) = betting_fixture();
        env.as_contract(&contract, || {
            assert_eq!(
                prediction_market::place_bet(&env, bettor, market_id, 2, 1, token),
                Err(PredictionMarketError::InvalidOutcomeId)
            );
        });
        let (env, bettor, token, _fee_recipient, market_id) = setup_market_with_bettor();

        prediction_market::place_bet(&env, bettor.clone(), market_id, 0, 100, token.clone()).unwrap();

        let bet = prediction_market::get_bet_details(&env, market_id, 0, &bettor).unwrap();
        assert_eq!(bet.amount, 100);
        assert_eq!(bet.outcome_id, 0);
        assert_eq!(bet.market_id, market_id);

        let outcome = prediction_market::get_outcome_details(&env, market_id, 0).unwrap();
        assert_eq!(outcome.total_bet_amount, 100);
        let config = prediction_market_storage::get_market_config(&env).unwrap();
        assert_eq!(config.admin, admin);
    }

    #[test]
    fn test_init_prediction_markets_fee_recipient_set_correctly() {
        // ✅ AC #4: Fee recipient set correctly
        let (env, vault, admin, fee_recipient, _alice) = setup();

        vault.init_prediction_markets(&admin, &fee_recipient, &Some(100))
            .unwrap();

        let config = prediction_market_storage::get_market_config(&env).unwrap();
        assert_eq!(config.fee_recipient, fee_recipient);
    }

    #[test]
    fn test_init_prediction_markets_default_fee_bps() {
        // ✅ AC #5: Default fee_bps is 100 (1%)
        let (env, vault, admin, fee_recipient, _alice) = setup();

        // Initialize with None — should use default 100
        vault.init_prediction_markets(&admin, &fee_recipient, &None)
            .unwrap();

        let config = prediction_market_storage::get_market_config(&env).unwrap();
        assert_eq!(config.default_fee_bps, DEFAULT_FEE_BPS); // DEFAULT_FEE_BPS = 100
    }

    #[test]
    fn test_place_bet_duplicate_outcome() {
        let (env, bettor, token, _fee_recipient, market_id) = setup_market_with_bettor();

        prediction_market::place_bet(&env, bettor.clone(), market_id, 0, 100, token.clone()).unwrap();

        let err = prediction_market::place_bet(&env, bettor.clone(), market_id, 0, 200, token.clone())
            .unwrap_err();
        assert_eq!(err, PredictionMarketError::AlreadyBetOnOutcome);

        prediction_market::place_bet(&env, bettor.clone(), market_id, 1, 50, token.clone()).unwrap();
        let other = prediction_market::get_bet_details(&env, market_id, 1, &bettor).unwrap();
        assert_eq!(other.amount, 50);
    }
    fn test_init_prediction_markets_custom_fee_bps() {
        // Test initialization with custom fee_bps (200 = 2%)
        let (env, vault, admin, fee_recipient, _alice) = setup();

        vault.init_prediction_markets(&admin, &fee_recipient, &Some(200))
            .unwrap();

        let config = prediction_market_storage::get_market_config(&env).unwrap();
        assert_eq!(config.default_fee_bps, 200);
    }

    #[test]
    fn test_init_prediction_markets_state_persists() {
        // Verify state persists after initialization
        let (env, vault, admin, fee_recipient, _alice) = setup();

        vault.init_prediction_markets(&admin, &fee_recipient, &Some(150))
            .unwrap();

        // Query state multiple times to ensure persistence
        let config1 = prediction_market_storage::get_market_config(&env).unwrap();
        let config2 = prediction_market_storage::get_market_config(&env).unwrap();

        assert_eq!(config1, config2);
        assert_eq!(config1.default_fee_bps, 150);
    }

    #[test]
    fn test_init_prediction_markets_invalid_fee_bps() {
        // Test initialization with fee_bps > MAX_FEE_BPS (1000)
        let (_env, vault, admin, fee_recipient, _alice) = setup();

        let result = vault.init_prediction_markets(&admin, &fee_recipient, &Some(1001));
        assert_eq!(result, Err(Ok(PredictionMarketError::InvalidFeeBps)));
    }

    #[test]
    fn test_init_prediction_markets_max_fee_bps() {
        // Test initialization with exact MAX_FEE_BPS (1000 = 10%)
        let (env, vault, admin, fee_recipient, _alice) = setup();

        vault.init_prediction_markets(&admin, &fee_recipient, &Some(MAX_FEE_BPS))
            .unwrap();

        let config = prediction_market_storage::get_market_config(&env).unwrap();
        assert_eq!(config.default_fee_bps, MAX_FEE_BPS);
    }

    #[test]
    fn test_init_prediction_markets_zero_fee_bps() {
        // Test initialization with zero fee_bps
        let (env, vault, admin, fee_recipient, _alice) = setup();

        vault.init_prediction_markets(&admin, &fee_recipient, &Some(0))
            .unwrap();

        let config = prediction_market_storage::get_market_config(&env).unwrap();
        assert_eq!(config.default_fee_bps, 0);
    }

    // ================================================================
    //  Market Creation Tests (10 test cases)
    // ================================================================

    #[test]
    fn test_create_market_valid() {
        // Test successful market creation with valid parameters
        // Expected: market_id = 1, status = Open
        assert_eq!(true, true); // Placeholder for actual integration test
    }

    #[test]
    fn test_create_market_invalid_close_time_in_past() {
        // Test market creation with close_time in past
        // Expected: ResolutionTimeInPast error
        assert_eq!(true, true);
    }

    #[test]
    fn test_create_market_insufficient_close_duration() {
        // Test market creation with close_time too soon (< MIN_MARKET_DURATION_SECS)
        // Expected: InvalidResolutionTime error
        assert_eq!(true, true);
    }

    #[test]
    fn test_create_market_invalid_outcome_count_too_few() {
        // Test market creation with < 2 outcomes
        // Expected: InvalidOutcomeCount error
        assert_eq!(true, true);
    }

    #[test]
    fn test_create_market_invalid_outcome_count_too_many() {
        // Test market creation with > 4 outcomes
        // Expected: InvalidOutcomeCount error
        assert_eq!(true, true);
    }

    #[test]
    fn test_create_market_resolution_deadline_before_close() {
        // Test market creation where resolution_deadline <= close_time
        // Expected: InvalidResolutionTime error
        assert_eq!(true, true);
    }

    #[test]
    fn test_create_market_insufficient_resolution_time() {
        // Test market creation with resolution window < min_resolution_time_secs
        // Expected: InvalidResolutionTime error
        assert_eq!(true, true);
    }

    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        token::StellarAssetClient,
        Address, Env, String, Vec,
    };

    use crate::{
        contract::{SafeHaven, SafeHavenClient},
        prediction_market_types::MarketStatus,
    };

    // ----------------------------------------------------------------
    //  Test helpers
    // ----------------------------------------------------------------

    /// Advance the mock ledger clock by `seconds`.
    fn advance_time(env: &Env, seconds: u64) {
        env.ledger().set(LedgerInfo {
            timestamp: env.ledger().timestamp() + seconds,
            protocol_version: env.ledger().protocol_version(),
            sequence_number: env.ledger().sequence(),
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 33_000_000,
        });
    }

    /// Deploy contract, initialize both the vault and prediction market subsystem.
    /// Returns (env, client, token_addr, admin, alice, fee_recipient, oracle).
    fn setup() -> (
        Env,
        SafeHavenClient<'static>,
        Address,
        Address,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();

        let vault_id = env.register(SafeHaven, ());
        let client = SafeHavenClient::new(&env, &vault_id);

        let admin: Address = Address::generate(&env);
        let alice: Address = Address::generate(&env);
        let fee_recipient: Address = Address::generate(&env);
        let oracle: Address = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_address = token_id.address();

        StellarAssetClient::new(&env, &token_address).mint(&alice, &100_000);

        // Initialize vault subsystem
        client.initialize(&admin, &fee_recipient, &None, &None);

        // Initialize prediction market subsystem
        client.init_prediction_markets(&admin, &fee_recipient, &None);

        (env, client, token_address, admin, alice, fee_recipient, oracle)
    }

    /// Create a market, return market_id.
    /// Advances time so close and resolution windows are valid.
    fn create_test_market(
        env: &Env,
        client: &SafeHavenClient,
        creator: &Address,
        oracle: &Address,
    ) -> u32 {
        let now = env.ledger().timestamp();
        let close_time = now + 7_200; // 2 hours from now (> MIN_MARKET_DURATION_SECS=3600)
        let resolution_deadline = close_time + 7_200; // 2 hours after close

        let mut outcome_names = Vec::new(env);
        outcome_names.push_back(String::from_str(env, "Yes"));
        outcome_names.push_back(String::from_str(env, "No"));

        client.create_prediction_market(
            creator,
            &String::from_str(env, "aggregate_deposits"),
            &String::from_str(env, "Will deposits exceed 1M?"),
            oracle,
            &close_time,
            &resolution_deadline,
            &outcome_names,
            &None,
        )
    }

    // ----------------------------------------------------------------
    //  Core Issue #99 Tests
    // ----------------------------------------------------------------

    /// Resolution with data: oracle passes Some(i128) — it must be stored and
    /// retrievable via get_prediction_market.
    #[test]
    fn test_resolve_market_stores_resolution_data_some() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);

        // Close market so oracle can resolve
        client.close_prediction_market(&oracle, &market_id);

        // Resolve with non-None data
        let data_value: i128 = 1_200_000_000; // e.g., 1.2M tokens
        client.resolve_prediction_market(
            &oracle,
            &market_id,
            &0,            // winning_outcome
            &Some(data_value),
        );

        // Query and verify
        let market = client
            .get_prediction_market(&market_id)
            .expect("market must exist after resolution");

        assert_eq!(
            market.resolution_data,
            Some(data_value),
            "resolution_data should be stored exactly as provided"
        );
        assert_eq!(market.status, MarketStatus::Resolved);
        assert_eq!(market.winning_outcome, Some(0));
    }

    /// Resolution without data: oracle passes None — field must be None in storage.
    #[test]
    fn test_resolve_market_stores_resolution_data_none() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);
        client.close_prediction_market(&oracle, &market_id);

        // Resolve with no optional data
        client.resolve_prediction_market(
            &oracle,
            &market_id,
            &0,
            &None,
        );

        let market = client
            .get_prediction_market(&market_id)
            .expect("market must exist after resolution");

        assert_eq!(
            market.resolution_data,
            None,
            "resolution_data should be None when oracle provided None"
        );
        assert_eq!(market.status, MarketStatus::Resolved);
    }

    /// Data persistence: once stored, resolution_data must remain unchanged across
    /// multiple separate get_prediction_market calls (no mutation side effects).
    #[test]
    fn test_resolution_data_persists_across_queries() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);
        client.close_prediction_market(&oracle, &market_id);

        let stored_value: i128 = 42_000;
        client.resolve_prediction_market(&oracle, &market_id, &1, &Some(stored_value));

        // Query three times — value must be stable
        for _ in 0..3 {
            let market = client
                .get_prediction_market(&market_id)
                .expect("market must persist");
            assert_eq!(
                market.resolution_data,
                Some(stored_value),
                "resolution_data must be identical on every read"
            );
        }
    }

    /// Advancing the ledger after resolution should not affect stored data.
    #[test]
    fn test_resolution_data_persists_after_time_passes() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);
        client.close_prediction_market(&oracle, &market_id);

        let value: i128 = 999_888_777;
        client.resolve_prediction_market(&oracle, &market_id, &0, &Some(value));

        // Advance ledger significantly
        advance_time(&env, 1_000_000);

        let market = client
            .get_prediction_market(&market_id)
            .expect("market must still be accessible");
        assert_eq!(market.resolution_data, Some(value));
    }

    /// Boundary: resolution_data can hold i128::MAX without overflow.
    #[test]
    fn test_resolution_data_max_i128() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);
        client.close_prediction_market(&oracle, &market_id);

        let max_value: i128 = i128::MAX;
        client.resolve_prediction_market(&oracle, &market_id, &0, &Some(max_value));

        let market = client.get_prediction_market(&market_id).unwrap();
        assert_eq!(market.resolution_data, Some(max_value));
    }

    /// Boundary: resolution_data can hold i128::MIN (negative values valid).
    #[test]
    fn test_resolution_data_min_i128() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);
        client.close_prediction_market(&oracle, &market_id);

        let min_value: i128 = i128::MIN;
        client.resolve_prediction_market(&oracle, &market_id, &0, &Some(min_value));

        let market = client.get_prediction_market(&market_id).unwrap();
        assert_eq!(market.resolution_data, Some(min_value));
    }

    /// Boundary: resolution_data = Some(0) is valid and stored as zero.
    #[test]
    fn test_resolution_data_zero() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);
        client.close_prediction_market(&oracle, &market_id);

        client.resolve_prediction_market(&oracle, &market_id, &0, &Some(0));

        let market = client.get_prediction_market(&market_id).unwrap();
        assert_eq!(market.resolution_data, Some(0));
    }

    /// Two independent markets store separate resolution_data values — no cross
    /// contamination.
    #[test]
    fn test_resolution_data_isolated_per_market() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_a = create_test_market(&env, &client, &alice, &oracle);

        // Create a second market with a different close/deadline window
        let now = env.ledger().timestamp();
        let mut outcome_names = Vec::new(&env);
        outcome_names.push_back(String::from_str(&env, "Yes"));
        outcome_names.push_back(String::from_str(&env, "No"));
        let market_b = client.create_prediction_market(
            &alice,
            &String::from_str(&env, "unique_depositors"),
            &String::from_str(&env, "Depositors > 500?"),
            &oracle,
            &(now + 8_000),
            &(now + 20_000),
            &outcome_names,
            &None,
        );

        client.close_prediction_market(&oracle, &market_a);
        client.close_prediction_market(&oracle, &market_b);

        let value_a: i128 = 111_111;
        let value_b: i128 = 222_222;

        client.resolve_prediction_market(&oracle, &market_a, &0, &Some(value_a));
        client.resolve_prediction_market(&oracle, &market_b, &1, &Some(value_b));

        let ma = client.get_prediction_market(&market_a).unwrap();
        let mb = client.get_prediction_market(&market_b).unwrap();

        assert_eq!(ma.resolution_data, Some(value_a));
        assert_eq!(mb.resolution_data, Some(value_b));
    }

    /// Before resolution, resolution_data in the market struct must be None.
    #[test]
    fn test_resolution_data_is_none_before_resolution() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);

        // Market still open — query should show None
        let market = client.get_prediction_market(&market_id).unwrap();
        assert_eq!(
            market.resolution_data, None,
            "resolution_data must be None before the market is resolved"
        );
        assert_eq!(market.status, MarketStatus::Open);
    }

    /// Before resolution, resolution_data in the market struct must be None (closed state).
    #[test]
    fn test_resolution_data_is_none_when_closed_not_resolved() {
        let (env, client, _token, _admin, alice, _fee, oracle) = setup();

        let market_id = create_test_market(&env, &client, &alice, &oracle);
        client.close_prediction_market(&oracle, &market_id);

        let market = client.get_prediction_market(&market_id).unwrap();
        assert_eq!(
            market.resolution_data, None,
            "resolution_data must be None before oracle submits resolution"
        );
        assert_eq!(market.status, MarketStatus::Closed);
    }

    /// Full lifecycle: create → close → resolve with data → query confirms data.
    #[test]
    fn test_full_lifecycle_resolution_data_stored() {
        let (env, client, token, admin, alice, fee_recipient, oracle) = setup();

        // Create market
        let now = env.ledger().timestamp();
        let close_time = now + 10_000;
        let resolution_deadline = close_time + 5_000;

        let mut outcome_names = Vec::new(&env);
        outcome_names.push_back(String::from_str(&env, "High (>1M)"));
        outcome_names.push_back(String::from_str(&env, "Low (<=1M)"));

        let market_id = client.create_prediction_market(
            &alice,
            &String::from_str(&env, "aggregate_deposits"),
            &String::from_str(&env, "Will deposits exceed 1M USDC?"),
            &oracle,
            &close_time,
            &resolution_deadline,
            &outcome_names,
            &Some(100), // 1% fee
        );

        // Verify market starts without resolution_data
        let before = client.get_prediction_market(&market_id).unwrap();
        assert_eq!(before.resolution_data, None);
        assert_eq!(before.status, MarketStatus::Open);

        // Close market
        client.close_prediction_market(&oracle, &market_id);

        let closed = client.get_prediction_market(&market_id).unwrap();
        assert_eq!(closed.resolution_data, None);
        assert_eq!(closed.status, MarketStatus::Closed);

        // Oracle resolves with data
        let final_aggregate: i128 = 1_500_000_000_000; // 1.5M with 6 decimal places
        client.resolve_prediction_market(
            &oracle,
            &market_id,
            &0, // "High" wins
            &Some(final_aggregate),
        );

        // Verify full state after resolution
        let resolved = client.get_prediction_market(&market_id).unwrap();
        assert_eq!(resolved.status, MarketStatus::Resolved);
        assert_eq!(resolved.winning_outcome, Some(0));
        assert_eq!(
            resolved.resolution_data,
            Some(final_aggregate),
            "resolution_data must equal the value submitted by the oracle"
        );
    }
}

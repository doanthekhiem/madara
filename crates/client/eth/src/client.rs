use crate::client::StarknetCoreContract::StarknetCoreContractInstance;
use crate::utils::u256_to_felt;
use alloy::sol_types::SolEvent;
use alloy::{
    primitives::Address,
    providers::{Provider, ProviderBuilder, ReqwestProvider, RootProvider},
    rpc::types::Filter,
    sol,
    transports::http::{Client, Http},
};
use anyhow::{bail, Context};
use bitvec::macros::internal::funty::Fundamental;
use mc_metrics::{Gauge, MetricsRegistry, PrometheusError, F64};
use starknet_types_core::felt::Felt;
use std::sync::Arc;
use url::Url;

#[derive(Clone, Debug)]
pub struct L1BlockMetrics {
    // L1 network metrics
    pub l1_block_number: Gauge<F64>,
    // gas price is also define in sync/metrics/block_metrics.rs but this would be the price from l1
    pub l1_gas_price_wei: Gauge<F64>,
    pub l1_gas_price_strk: Gauge<F64>,
}

impl L1BlockMetrics {
    pub fn register(registry: &MetricsRegistry) -> Result<Self, PrometheusError> {
        Ok(Self {
            l1_block_number: registry
                .register(Gauge::new("madara_l1_block_number", "Gauge for madara L1 block number")?)?,

            l1_gas_price_wei: registry.register(Gauge::new("madara_l1_gas_price", "Gauge for madara L1 gas price")?)?,
            l1_gas_price_strk: registry
                .register(Gauge::new("madara_l1_gas_price_strk", "Gauge for madara L1 gas price in strk")?)?,
        })
    }
}

// abi taken from: https://etherscan.io/address/0x6e0acfdc3cf17a7f99ed34be56c3dfb93f464e24#code
// The official starknet core contract ^
sol!(
    #[sol(rpc)]
    #[derive(Debug)]
    StarknetCoreContract,
    "src/abis/starknet_core.json"
);

pub struct EthereumClient {
    pub provider: Arc<ReqwestProvider>,
    pub l1_core_contract: StarknetCoreContractInstance<Http<Client>, RootProvider<Http<Client>>>,
    pub l1_block_metrics: L1BlockMetrics,
}

impl Clone for EthereumClient {
    fn clone(&self) -> Self {
        EthereumClient {
            provider: Arc::clone(&self.provider),
            l1_core_contract: self.l1_core_contract.clone(),
            l1_block_metrics: self.l1_block_metrics.clone(),
        }
    }
}

impl EthereumClient {
    /// Create a new EthereumClient instance with the given RPC URL
    pub async fn new(url: Url, l1_core_address: Address, l1_block_metrics: L1BlockMetrics) -> anyhow::Result<Self> {
        let provider = ProviderBuilder::new().on_http(url);

        EthereumClient::assert_core_contract_exists(&provider, l1_core_address).await?;

        let core_contract = StarknetCoreContract::new(l1_core_address, provider.clone());

        Ok(Self { provider: Arc::new(provider), l1_core_contract: core_contract, l1_block_metrics })
    }

    /// Assert that L1 Core contract exists by checking its bytecode.
    async fn assert_core_contract_exists(
        provider: &RootProvider<Http<Client>>,
        l1_core_address: Address,
    ) -> anyhow::Result<()> {
        let l1_core_contract_bytecode = provider.get_code_at(l1_core_address).await?;
        if l1_core_contract_bytecode.is_empty() {
            bail!("The L1 Core Contract could not be found. Check that the L2 chain matches the L1 RPC endpoint.");
        }
        Ok(())
    }

    /// Retrieves the latest Ethereum block number
    pub async fn get_latest_block_number(&self) -> anyhow::Result<u64> {
        let block_number = self.provider.get_block_number().await?.as_u64();
        Ok(block_number)
    }

    /// Get the block number of the last occurrence of a given event.
    pub async fn get_last_event_block_number<T: SolEvent>(&self) -> anyhow::Result<u64> {
        let latest_block: u64 = self.get_latest_block_number().await?;

        // Assuming an avg Block time of 15sec we check for a LogStateUpdate occurence in the last ~24h
        let filter = Filter::new()
            .from_block(latest_block - 6000)
            .to_block(latest_block)
            .address(*self.l1_core_contract.address());

        let logs = self.provider.get_logs(&filter).await?;

        let filtered_logs = logs.into_iter().filter_map(|log| log.log_decode::<T>().ok()).collect::<Vec<_>>();

        if let Some(last_log) = filtered_logs.last() {
            let last_block: u64 = last_log.block_number.context("no block number in log")?;
            Ok(last_block)
        } else {
            bail!("no event found")
        }
    }

    /// Get the last Starknet block number verified on L1
    pub async fn get_last_verified_block_number(&self) -> anyhow::Result<u64> {
        let block_number = self.l1_core_contract.stateBlockNumber().call().await?;
        let last_block_number: u64 = (block_number._0).as_u64();
        Ok(last_block_number)
    }

    /// Get the last Starknet state root verified on L1
    pub async fn get_last_state_root(&self) -> anyhow::Result<Felt> {
        let state_root = self.l1_core_contract.stateRoot().call().await?;
        u256_to_felt(state_root._0)
    }

    /// Get the last Starknet block hash verified on L1
    pub async fn get_last_verified_block_hash(&self) -> anyhow::Result<Felt> {
        let block_hash = self.l1_core_contract.stateBlockHash().call().await?;
        u256_to_felt(block_hash._0)
    }
}

#[cfg(test)]
pub mod eth_client_getter_test {
    use super::*;
    use alloy::{
        node_bindings::{Anvil, AnvilInstance},
        primitives::U256,
    };
    use mc_metrics::MetricsService;
    use serial_test::serial;
    use tokio;
    // https://etherscan.io/tx/0xcadb202495cd8adba0d9b382caff907abf755cd42633d23c4988f875f2995d81#eventlog
    // The txn we are referring to it is here ^
    const L1_BLOCK_NUMBER: u64 = 20395662;
    const FORK_URL: &str = "https://eth.merkle.io";
    const ANVIL_PORT: u16 = 8545;
    const CORE_CONTRACT_ADDRESS: &str = "0xc662c410C0ECf747543f5bA90660f6ABeBD9C8c4";
    const L2_BLOCK_NUMBER: u64 = 662703;
    const L2_BLOCK_HASH: &str = "563216050958639290223177746678863910249919294431961492885921903486585884664";
    const L2_STATE_ROOT: &str = "1456190284387746219409791261254265303744585499659352223397867295223408682130";

    fn create_anvil_instance() -> AnvilInstance {
        let anvil = Anvil::new()
            .fork(FORK_URL)
            .fork_block_number(L1_BLOCK_NUMBER)
            .port(ANVIL_PORT)
            .try_spawn()
            .expect("failed to spawn anvil instance");
        println!("Anvil started and running at `{}`", anvil.endpoint());
        anvil
    }

    pub fn create_ethereum_client(url: Option<&str>) -> EthereumClient {
        let rpc_url: Url = url.unwrap_or("http://localhost:8545").parse().expect("issue while parsing URL");

        let provider = ProviderBuilder::new().on_http(rpc_url.clone());
        let address = Address::parse_checksummed(CORE_CONTRACT_ADDRESS, None).unwrap();
        let contract = StarknetCoreContract::new(address, provider.clone());

        let prometheus_service = MetricsService::new(true, false, 9615).unwrap();
        let l1_block_metrics = L1BlockMetrics::register(&prometheus_service.registry()).unwrap();

        EthereumClient { provider: Arc::new(provider), l1_core_contract: contract.clone(), l1_block_metrics }
    }

    #[serial]
    #[tokio::test]
    async fn fail_create_new_client_invalid_core_contract() {
        let anvil = create_anvil_instance();
        // Sepolia core contract instead of mainnet
        const INVALID_CORE_CONTRACT_ADDRESS: &str = "0xE2Bb56ee936fd6433DC0F6e7e3b8365C906AA057";

        let rpc_url: Url = anvil.endpoint_url();

        let core_contract_address = Address::parse_checksummed(INVALID_CORE_CONTRACT_ADDRESS, None).unwrap();
        let prometheus_service = MetricsService::new(true, false, 9615).unwrap();
        let l1_block_metrics = L1BlockMetrics::register(&prometheus_service.registry()).unwrap();

        let new_client_result = EthereumClient::new(rpc_url, core_contract_address, l1_block_metrics).await;
        assert!(new_client_result.is_err(), "EthereumClient::new should fail with an invalid core contract address");
    }

    #[serial]
    #[tokio::test]
    async fn get_latest_block_number_works() {
        let anvil = create_anvil_instance();
        let eth_client = create_ethereum_client(Some(anvil.endpoint().as_str()));
        let block_number =
            eth_client.provider.get_block_number().await.expect("issue while fetching the block number").as_u64();
        assert_eq!(block_number, L1_BLOCK_NUMBER, "provider unable to get the correct block number");
    }

    #[serial]
    #[tokio::test]
    async fn get_last_event_block_number_works() {
        let anvil = create_anvil_instance();
        let eth_client = create_ethereum_client(Some(anvil.endpoint().as_str()));
        let block_number = eth_client
            .get_last_event_block_number::<StarknetCoreContract::LogStateUpdate>()
            .await
            .expect("issue while getting the last block number with given event");
        assert_eq!(block_number, L1_BLOCK_NUMBER, "block number with given event not matching");
    }

    #[serial]
    #[tokio::test]
    async fn get_last_verified_block_hash_works() {
        let anvil = create_anvil_instance();
        let eth_client = create_ethereum_client(Some(anvil.endpoint().as_str()));
        let block_hash =
            eth_client.get_last_verified_block_hash().await.expect("issue while getting the last verified block hash");
        let expected = u256_to_felt(U256::from_str_radix(L2_BLOCK_HASH, 10).unwrap()).unwrap();
        assert_eq!(block_hash, expected, "latest block hash not matching");
    }

    #[serial]
    #[tokio::test]
    async fn get_last_state_root_works() {
        let anvil = create_anvil_instance();
        let eth_client = create_ethereum_client(Some(anvil.endpoint().as_str()));
        let state_root = eth_client.get_last_state_root().await.expect("issue while getting the state root");
        let expected = u256_to_felt(U256::from_str_radix(L2_STATE_ROOT, 10).unwrap()).unwrap();
        assert_eq!(state_root, expected, "latest block state root not matching");
    }

    #[serial]
    #[tokio::test]
    async fn get_last_verified_block_number_works() {
        let anvil = create_anvil_instance();
        let eth_client = create_ethereum_client(Some(anvil.endpoint().as_str()));
        let block_number = eth_client.get_last_verified_block_number().await.expect("issue");
        assert_eq!(block_number, L2_BLOCK_NUMBER, "verified block number not matching");
    }
}

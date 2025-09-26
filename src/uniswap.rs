use ethers::abi::Abi;
use ethers::contract::Contract;
use ethers::providers::Provider;
use ethers::types::{Address, U256};
use std::sync::Arc;

// Uniswap V2 ABI (only getAmountsOut function is needed)
const UNISWAP_V2_ABI: &str = r#"[{
    "name": "getAmountsOut",
    "type": "function",
    "stateMutability": "view",
    "inputs": [
        {"name": "amountIn", "type": "uint256"},
        {"name": "path", "type": "address[]"}
    ],
    "outputs": [
        {"name": "", "type": "uint256[]"}
    ]
}]"#;

pub async fn get_price(
    provider: &Provider<ethers::providers::Http>,
    router: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> anyhow::Result<f64> {
    let client = Arc::new(provider.clone());
    let abi: Abi = serde_json::from_str(UNISWAP_V2_ABI)?;
    let contract = Contract::new(router, abi, client);

    let path = vec![token_in, token_out];
    let amounts: Vec<U256> = contract
        .method::<_, Vec<U256>>("getAmountsOut", (amount_in, path))?
        .call()
        .await?;

    // amount_out is in smallest unit (wei, USDC 6 decimals etc.)
    let amount_out = amounts[1];
    let price = amount_out.as_u128() as f64 / 1e6; // assuming token_out = USDC (6 decimals)

    Ok(price)
}


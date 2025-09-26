//! Polygon Arbitrage Opportunity Detector Bot (Rust) - Fully Updated

use anyhow::Context;
use chrono::Utc;
use dotenv::dotenv;
use ethers::prelude::*;
use ethers::providers::{Http, Middleware, Provider};
use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;

// UniswapV2 Router binding
abigen!(
    UniswapV2Router,
    r#"[ 
        function getAmountsOut(uint256 amountIn, address[] memory path) external view returns (uint256[] memory amounts) 
    ]"#
);

// Minimal ERC20 for decimals
abigen!(
    ERC20,
    r#"[ 
        function decimals() external view returns (uint8) 
    ]"#
);

#[derive(Debug, Deserialize)]
struct Config {
    rpc_url: String,
    dex_a_router: Address,
    dex_b_router: Address,
    token_in: Address,
    token_out: Address,
    trade_size_wei: U256,
    min_profit_usdc: f64,
    poll_interval_secs: u64,
    simulated_gas_usdc: f64,
    database_path: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        dotenv().ok();
        Ok(Self {
            rpc_url: env::var("RPC_URL")?,
            dex_a_router: env::var("DEX_A_ROUTER")?.parse::<Address>()?,
            dex_b_router: env::var("DEX_B_ROUTER")?.parse::<Address>()?,
            token_in: env::var("TOKEN_IN")?.parse::<Address>()?,
            token_out: env::var("TOKEN_OUT")?.parse::<Address>()?,
            trade_size_wei: U256::from(env::var("TRADE_SIZE_WEI")?.parse::<u128>()?),
            min_profit_usdc: env::var("MIN_PROFIT_USDC")?.parse::<f64>()?,
            poll_interval_secs: env::var("POLL_INTERVAL_SECS")?.parse::<u64>()?,
            simulated_gas_usdc: env::var("SIMULATED_GAS_USDC")?.parse::<f64>()?,
            database_path: env::var("DATABASE_PATH")?,
        })
    }
}

// Global decimals cache
static DECIMALS_CACHE: Lazy<Mutex<HashMap<Address, u8>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cfg = Config::from_env().context("loading config from env")?;

    log::info!(
        "Starting polygon-arb-bot with poll_interval={}s, min_profit={} USDC",
        cfg.poll_interval_secs,
        cfg.min_profit_usdc
    );

    let provider = Arc::new(
        Provider::<Http>::try_from(cfg.rpc_url.as_str())?
            .interval(Duration::from_millis(500)),
    );

    // Open/create DB
    let conn = Connection::open(&cfg.database_path)?;
    init_db(&conn)?;

    let router_a = UniswapV2Router::new(cfg.dex_a_router, Arc::clone(&provider));
    let router_b = UniswapV2Router::new(cfg.dex_b_router, Arc::clone(&provider));

    let decimals_in = get_decimals_cached(Arc::clone(&provider), cfg.token_in)
        .await
        .unwrap_or(18u8);
    let decimals_out = get_decimals_cached(Arc::clone(&provider), cfg.token_out)
        .await
        .unwrap_or(18u8);

    loop {
        if let Err(e) = run_cycle(
            &cfg,
            &conn,
            &router_a,
            &router_b,
            decimals_in as u32,
            decimals_out as u32,
        )
        .await
        {
            log::error!("Error in arbitrage loop: {:?}", e);
        }
        sleep(Duration::from_secs(cfg.poll_interval_secs)).await;
    }
}

fn init_db(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS opportunities (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            dex_buy TEXT NOT NULL,
            dex_sell TEXT NOT NULL,
            amount_in TEXT NOT NULL,
            amount_out_buy TEXT NOT NULL,
            amount_out_sell TEXT NOT NULL,
            profit REAL NOT NULL
        )",
        [],
    )?;
    Ok(())
}

async fn run_cycle<M: Middleware + 'static>(
    cfg: &Config,
    conn: &Connection,
    router_a: &UniswapV2Router<M>,
    router_b: &UniswapV2Router<M>,
    decimals_in: u32,
    decimals_out: u32,
) -> anyhow::Result<()> {
    let path = vec![cfg.token_in, cfg.token_out];

    let a_amounts = router_a
        .get_amounts_out(cfg.trade_size_wei, path.clone())
        .call()
        .await?;
    let b_amounts = router_b
        .get_amounts_out(cfg.trade_size_wei, path.clone())
        .call()
        .await?;

    let amount_out_a = a_amounts.last().cloned().unwrap_or_else(U256::zero);
    let amount_out_b = b_amounts.last().cloned().unwrap_or_else(U256::zero);

    let trade_size_f = u256_to_f64(cfg.trade_size_wei, decimals_in);
    let price_a = u256_to_f64(amount_out_a, decimals_out) / trade_size_f;
    let price_b = u256_to_f64(amount_out_b, decimals_out) / trade_size_f;

    log::info!("Prices: A = {:.4} | B = {:.4}", price_a, price_b);

    // Determine arbitrage
    if price_b > price_a {
        let profit = (price_b - price_a) * trade_size_f - cfg.simulated_gas_usdc;
        if profit > cfg.min_profit_usdc {
            log::info!(
                " Arbitrage opportunity! BUY on A at {:.4}, SELL on B at {:.4} → Profit: {:.4} USDC",
                price_a,
                price_b,
                profit
            );
            insert_opportunity(
                conn,
                "A",
                "B",
                &cfg.trade_size_wei.to_string(),
                &amount_out_a.to_string(),
                &amount_out_b.to_string(),
                profit,
            )?;
        }
    } else if price_a > price_b {
        let profit = (price_a - price_b) * trade_size_f - cfg.simulated_gas_usdc;
        if profit > cfg.min_profit_usdc {
            log::info!(
                " Arbitrage opportunity! BUY on B at {:.4}, SELL on A at {:.4} → Profit: {:.4} USDC",
                price_b,
                price_a,
                profit
            );
            insert_opportunity(
                conn,
                "B",
                "A",
                &cfg.trade_size_wei.to_string(),
                &amount_out_b.to_string(),
                &amount_out_a.to_string(),
                profit,
            )?;
        }
    }

    Ok(())
}

fn insert_opportunity(
    conn: &Connection,
    dex_buy: &str,
    dex_sell: &str,
    amount_in: &str,
    amount_out_buy: &str,
    amount_out_sell: &str,
    profit: f64,
) -> anyhow::Result<()> {
    let ts = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO opportunities (timestamp, dex_buy, dex_sell, amount_in, amount_out_buy, amount_out_sell, profit) 
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![ts, dex_buy, dex_sell, amount_in, amount_out_buy, amount_out_sell, profit],
    )?;
    Ok(())
}

fn u256_to_f64(value: U256, decimals: u32) -> f64 {
    let mut v = value.as_u128() as f64;
    v /= 10f64.powi(decimals as i32);
    v
}

async fn get_decimals_cached<M: Middleware + 'static>(
    provider: Arc<M>,
    token: Address,
) -> Option<u8> {
    {
        let cache = DECIMALS_CACHE.lock().unwrap();
        if let Some(&d) = cache.get(&token) {
            return Some(d);
        }
    }

    let erc20 = ERC20::new(token, Arc::clone(&provider));
    match erc20.decimals().call().await {
        Ok(d) => {
            let mut cache = DECIMALS_CACHE.lock().unwrap();
            cache.insert(token, d);
            Some(d)
        }
        Err(_) => None,
    }
}

 //! Polygon arbitrage opportunity detector bot - Gourav Mehar

use anyhow::Context;    // Just normal error handling stuff with some helper context.

use chrono::Utc;  // Some time helpers for working with timestamps.

use dotenv::dotenv;   // Load environment variables from a .env file

use ethers::prelude::*;  // ethers-rs prelude: gives contracts, types, and helpers for working with Ethereum-like chains.
use ethers::providers::{Http, Middleware, Provider};

use once_cell::sync::Lazy;    // once_cell for a lazy-initialized global cache

use rusqlite::{params, Connection};  // rusqlite for a simple embedded SQL database

// Deserialize trait for config from env
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;


//  I just made a typed Rust binding for the router function from UniswapV2-like routers.  

  // Named it `TokenSwapCalculator` so it’s clear we’re only using getAmountsOut



  abigen!(
          TokenSwapCalculator,
    r#"[ 
        function getAmountsOut(uint256 amountIn, address[] memory path) external view returns (uint256[] memory amounts) 
    ]"#

   );

//  Just a simple ERC20 binding to check token decimals (helps us turn on-chain ints into human-readable values).

    abigen!(
    ERC20,
    r#"[  
        function decimals() external view returns (uint8) 
    ]"#
);


    // Config Structure
//  This just keeps the runtime config from .env vars, kept simple and clear.


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

               // TRADE_SIZE_WEI stored as string in env; convert to U256 via u128 intermediate

            trade_size_wei: U256::from(env::var("TRADE_SIZE_WEI")?.parse::<u128>()?),
            min_profit_usdc: env::var("MIN_PROFIT_USDC")?.parse::<f64>()?,
            poll_interval_secs: env::var("POLL_INTERVAL_SECS")?.parse::<u64>()?,
            simulated_gas_usdc: env::var("SIMULATED_GAS_USDC")?.parse::<f64>()?,
            database_path: env::var("DATABASE_PATH")?,

        })
    }
}

     // Global decimals cache

     // We cache ERC20 decimals per token address to avoid repeated on-chain calls.

     static DECIMALS_CACHE: Lazy<Mutex<HashMap<Address, u8>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[tokio::main]

async fn main() -> anyhow::Result<()> {

    // Initialize logger (reads RUST_LOG env var)

       env_logger::init();  
       // Load configuration and provide context in case of failure

    let cfg = Config::from_env().context("Failed to read config from .env, check environment variables")?;

      log::info!(
        "Starting Polygon Arb Bot | Poll every {}s | Min profit {} USDC",
        cfg.poll_interval_secs,
        cfg.min_profit_usdc
    );

    // Set up an HTTP provider using the RPC URL from config.
// Wrapped in an Arc so we can easily share and clone it between router bindings.

    let provider = Arc::new(
        Provider::<Http>::try_from(cfg.rpc_url.as_str())?
            .interval(Duration::from_millis(500)),
    );

                // create the SQLite database file specified in config.
    let conn = Connection::open(&cfg.database_path)?;

    init_db(&conn)?;

   // Making router objects for each DEX address.
// TokenSwapCalculator is just a helper we made earlier to use getAmountsOut.


    let dex_a_router = TokenSwapCalculator::new(cfg.dex_a_router, Arc::clone(&provider));
    let dex_b_router = TokenSwapCalculator::new(cfg.dex_b_router, Arc::clone(&provider));

    // Grab token decimals once and reuse them. If it doesn’t work, just use 18 as a safe backup.

      let decimals_in = get_decimals_cached(Arc::clone(&provider), cfg.token_in)
        .await
        .unwrap_or(18u8);
    let decimals_out = get_decimals_cached(Arc::clone(&provider), cfg.token_out)
        .await
        .unwrap_or(18u8);

             // Main loop: check prices, wait a bit, then do it again.

    loop {
        if let Err(e) = run_cycle(
             &cfg,
            &conn,
            &dex_a_router,
              &dex_b_router,
            decimals_in as u32,
            decimals_out as u32,
        )
        .await

        {
            log::error!("Error in arbitrage loop: {:?}", e);  // Log the error but don't exit; we want the bot to keep running
        }
        sleep(Duration::from_secs(cfg.poll_interval_secs)).await;
    }
}
         
         // Initialize the SQLite table used to persist detected opportunities.
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

// Each round: ask both routers for prices, compare them, and see if there’s an arbitrage chance.


 async fn run_cycle<M: Middleware + 'static>(
    cfg: &Config,
    conn: &Connection,
    dex_a_router: &TokenSwapCalculator<M>,

    dex_b_router: &TokenSwapCalculator<M>,
    decimals_in: u32,
    decimals_out: u32,
) -> anyhow::Result<()> {
    let path = vec![cfg.token_in, cfg.token_out];  // Build the simplest swap path: TOKEN_IN -> TOKEN_OUT


     // Call getAmountsOut on both routers. Each returns an array of amounts per hop.
     let a_amounts = dex_a_router
        .get_amounts_out(cfg.trade_size_wei, path.clone())
        .call()
        .await?;
    let b_amounts = dex_b_router
        .get_amounts_out(cfg.trade_size_wei, path.clone())
        .call()
        .await?;


    // Grab the last value from the array (that’s amountOut). If nothing’s there, just use 0.


    let dex_a_amount_out = a_amounts.last().cloned().unwrap_or_else(U256::zero);
     let dex_b_amount_out = b_amounts.last().cloned().unwrap_or_else(U256::zero);

    // Convert on-chain U256 values into human-readable floats using token decimals.
    let trade_size_f = u256_to_f64(cfg.trade_size_wei, decimals_in);
    let price_a = u256_to_f64(dex_a_amount_out, decimals_out) / trade_size_f;
    let price_b = u256_to_f64(dex_b_amount_out, decimals_out) / trade_size_f;

    log::info!("Prices: A = {:.4} | B = {:.4}", price_a, price_b);

    // Determine arbitrage: if price on one DEX is higher than the other, compute profit after simulated gas.
    if price_b > price_a {

        let profit = (price_b - price_a) * trade_size_f - cfg.simulated_gas_usdc;
        if profit > cfg.min_profit_usdc {
            log::info!(
                " Arb Opportunity: Buy on DEX A @ {:.4}, Sell on DEX B @ {:.4} → Profit: {:.4} USDC",
                price_a,
                price_b,
                profit
            );

            // Save the opportunity details so we can check them later.

            insert_opportunity(
                conn,
                "A",
                "B",
                &cfg.trade_size_wei.to_string(),
                &dex_a_amount_out.to_string(),
                &dex_b_amount_out.to_string(),
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
                &dex_a_amount_out.to_string(),
                &dex_b_amount_out.to_string(),
                profit,
            )?;
        }
    }

    Ok(())
}


     // Helper function: save the found opportunity.

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
// Convert U256 (on-chain integer) to f64 taking token decimals into account.
fn u256_to_f64(value: U256, decimals: u32) -> f64 {
    let mut v = value.as_u128() as f64;
    v /= 10f64.powi(decimals as i32);
    v
}
      
      // Query token decimals with caching to avoid repeated on-chain calls.
async fn get_decimals_cached<M: Middleware + 'static>(
    provider: Arc<M>,
    token: Address,
) -> Option<u8> {
    { 
             // Check cache first (fast, local)
        let cache = DECIMALS_CACHE.lock().unwrap();
        if let Some(&d) = cache.get(&token) {
            return Some(d);
        }
    }


     // If it’s not in cache, make an ERC20 object and call decimals().

    let erc20 = ERC20::new(token, Arc::clone(&provider));
    match erc20.decimals().call().await {
        Ok(d) => {

            // Save it in cache so we can reuse it later.

            let mut cache = DECIMALS_CACHE.lock().unwrap();
            cache.insert(token, d);
            Some(d)
        }
        Err(_) => None,    // If the call fails, return None and the caller can use a fallback

    }
}


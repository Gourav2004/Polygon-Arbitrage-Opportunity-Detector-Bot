 //! Polygon arbitrage bot with web dashboard

use anyhow::Context;
use chrono::Utc;
use dotenv::dotenv;
use ethers::prelude::*;
use ethers::providers::{Http, Middleware, Provider};
use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;
use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use actix_files::Files;

abigen!(
    TokenSwapCalculator,
    r#"[ function getAmountsOut(uint256 amountIn, address[] memory path) external view returns (uint256[] memory amounts) ]"#
);

abigen!(
    ERC20,
    r#"[ function decimals() external view returns (uint8) ]"#
);

#[derive(Debug, Deserialize, Clone)]
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

static DECIMALS_CACHE: Lazy<Mutex<HashMap<Address, u8>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Serialize)]
struct Opportunity {
    timestamp: String,
    dex_buy: String,
    dex_sell: String,
    amount_in: String,
    amount_out_buy: String,
    amount_out_sell: String,
    profit: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cfg = Config::from_env().context("Failed to read config from .env")?;
    log::info!(
        "Starting Polygon Arb Bot | Poll every {}s | Min profit {} USDC",
        cfg.poll_interval_secs,
        cfg.min_profit_usdc
    );

    let provider = Arc::new(
        Provider::<Http>::try_from(cfg.rpc_url.as_str())?.interval(Duration::from_millis(500)),
    );

    let conn = Arc::new(Mutex::new(Connection::open(&cfg.database_path)?));
    init_db(&conn.lock().unwrap())?;

    let dex_a_router = TokenSwapCalculator::new(cfg.dex_a_router, Arc::clone(&provider));
    let dex_b_router = TokenSwapCalculator::new(cfg.dex_b_router, Arc::clone(&provider));

    let decimals_in = get_decimals_cached(Arc::clone(&provider), cfg.token_in)
        .await
        .unwrap_or(18u8);
    let decimals_out = get_decimals_cached(Arc::clone(&provider), cfg.token_out)
        .await
        .unwrap_or(18u8);

    // Spawn background bot loop
    let cfg_clone = cfg.clone();
    let conn_clone = Arc::clone(&conn);
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_cycle(
                &cfg_clone,
                &conn_clone,
                &dex_a_router,
                &dex_b_router,
                decimals_in as u32,
                decimals_out as u32,
            )
            .await
            {
                log::error!("Error in arbitrage loop: {:?}", e);
            }
            sleep(Duration::from_secs(cfg_clone.poll_interval_secs)).await;
        }
    });

    // Start web server
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(Arc::clone(&conn)))
            .service(index)
            .service(get_opportunities)
            .service(Files::new("/static", "./static"))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await?;

    Ok(())
}

// ----- Database -----
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

fn insert_opportunity(
    conn: &Arc<Mutex<Connection>>,
    dex_buy: &str,
    dex_sell: &str,
    amount_in: f64,
    amount_out_buy: f64,
    amount_out_sell: f64,
    profit: f64,
) -> anyhow::Result<()> {
    let ts = Utc::now().to_rfc3339();
    conn.lock().unwrap().execute(
        "INSERT INTO opportunities (timestamp, dex_buy, dex_sell, amount_in, amount_out_buy, amount_out_sell, profit) 
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![ts, dex_buy, dex_sell, amount_in, amount_out_buy, amount_out_sell, profit],
    )?;
    Ok(())
}

// ----- Helpers -----
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

// ----- Bot cycle -----
async fn run_cycle<M: Middleware + 'static>(
    cfg: &Config,
    conn: &Arc<Mutex<Connection>>,
    dex_a_router: &TokenSwapCalculator<M>,
    dex_b_router: &TokenSwapCalculator<M>,
    decimals_in: u32,
    decimals_out: u32,
) -> anyhow::Result<()> {
    let path = vec![cfg.token_in, cfg.token_out];

    let a_amounts = dex_a_router
        .get_amounts_out(cfg.trade_size_wei, path.clone())
        .call()
        .await?;
    let b_amounts = dex_b_router
        .get_amounts_out(cfg.trade_size_wei, path.clone())
        .call()
        .await?;

    let dex_a_amount_out = a_amounts.last().cloned().unwrap_or_else(U256::zero);
    let dex_b_amount_out = b_amounts.last().cloned().unwrap_or_else(U256::zero);

    let trade_size_f = u256_to_f64(cfg.trade_size_wei, decimals_in); // now correct human-readable
    let price_a = u256_to_f64(dex_a_amount_out, decimals_out) / trade_size_f;
    let price_b = u256_to_f64(dex_b_amount_out, decimals_out) / trade_size_f;

    log::info!("Prices: A = {:.4} | B = {:.4}", price_a * trade_size_f, price_b * trade_size_f);

    if price_b > price_a {
        let profit = (price_b - price_a) * trade_size_f - cfg.simulated_gas_usdc;
        if profit > cfg.min_profit_usdc {
            log::info!(
                "Arb Opportunity: Buy on DEX A @ {:.4}, Sell on DEX B @ {:.4} → Profit: {:.4} USDC",
                price_a * trade_size_f,
                price_b * trade_size_f,
                profit
            );
            insert_opportunity(
                conn,
                "A",
                "B",
                trade_size_f,
                u256_to_f64(dex_a_amount_out, decimals_out),
                u256_to_f64(dex_b_amount_out, decimals_out),
                profit,
            )?;
        }
    } else if price_a > price_b {
        let profit = (price_a - price_b) * trade_size_f - cfg.simulated_gas_usdc;
        if profit > cfg.min_profit_usdc {
            log::info!(
                "Arb Opportunity: Buy on DEX B @ {:.4}, Sell on DEX A @ {:.4} → Profit: {:.4} USDC",
                price_b * trade_size_f,
                price_a * trade_size_f,
                profit
            );
            insert_opportunity(
                conn,
                "B",
                "A",
                trade_size_f,
                u256_to_f64(dex_a_amount_out, decimals_out),
                u256_to_f64(dex_b_amount_out, decimals_out),
                profit,
            )?;
        }
    }

    Ok(())
}

// ----- Web endpoints -----
#[get("/")]
async fn index() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html")
        .body(include_str!("../static/landing.html"))
}

#[get("/opportunities")]
async fn get_opportunities(conn: web::Data<Arc<Mutex<Connection>>>) -> impl Responder {
    let conn = conn.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT timestamp, dex_buy, dex_sell, amount_in, amount_out_buy, amount_out_sell, profit 
             FROM opportunities ORDER BY id DESC",
        )
        .unwrap();

    let rows = stmt
        .query_map([], |row| {
            Ok(Opportunity {
                timestamp: row.get(0)?,
                dex_buy: row.get(1)?,
                dex_sell: row.get(2)?,
                amount_in: row.get(3)?,
                amount_out_buy: row.get(4)?,
                amount_out_sell: row.get(5)?,
                profit: row.get(6)?,
            })
        })
        .unwrap();

    let data: Vec<_> = rows.map(|r| r.unwrap()).collect();
    HttpResponse::Ok().json(data)
}



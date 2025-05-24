/*
This is an example of a basic market making strategy.

We subscribe to the current mid price and build a market around this price. Whenever our market becomes outdated, we place and cancel orders to renew it.
*/
use ethers::signers::LocalWallet;
use ethers::signers::Signer;

use hyperliquid_rust_sdk::{MarketMaker, MarketMakerInput};

#[tokio::main]
async fn main() {
    env_logger::init();
    // Key was randomly generated for testing and shouldn't be used with any real funds
    let wallet: LocalWallet = "0x195099d9067808f79797db3584895666ed2f76072c442dbc40c29c8fa20eca60"
        .parse()
        .unwrap();
    println!("Wallet: {:?}", wallet.address());
    let market_maker_input = MarketMakerInput {
        asset: "FARTCOIN".to_string(),
        target_liquidity: 100.0,
        max_bps_diff: 2,
        half_spread: 1,
        max_absolute_position_size: 200.0,
        decimals: 2,
        wallet,
    };
    MarketMaker::new(market_maker_input).await.start().await
}

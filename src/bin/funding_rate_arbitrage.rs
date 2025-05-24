use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, InfoClient,
    ExchangeDataStatus, ExchangeResponseStatus, Message, Subscription, UserData,
};
use log::{error, info};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;
use dotenv::dotenv;
use std::env;

// 从环境变量读取配置，如果不存在则使用默认值
fn get_env_or_default<T: std::str::FromStr>(key: &str, default: T) -> T 
where 
    T::Err: std::fmt::Debug 
{
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// 计算资金费率套利机会
fn calculate_funding_arbitrage(
    funding_rates: &HashMap<String, f64>,
    min_profit_percentage: f64,
) -> Option<(String, String, f64)> {
    let mut best_opportunity: Option<(String, String, f64)> = None;
    
    // 遍历所有交易对
    for (asset1, rate1) in funding_rates {
        for (asset2, rate2) in funding_rates {
            if asset1 == asset2 {
                continue;
            }
            
            // 计算资金费率差异
            let rate_diff = (rate1 - rate2).abs();
            let profit_percentage = rate_diff * 100.0; // 转换为百分比
            
            // 如果收益率超过最小要求，且比当前最佳机会更好
            if profit_percentage > min_profit_percentage {
                match &best_opportunity {
                    None => {
                        best_opportunity = Some((asset1.clone(), asset2.clone(), profit_percentage));
                    }
                    Some((_, _, current_best_profit)) => {
                        if profit_percentage > *current_best_profit {
                            best_opportunity = Some((asset1.clone(), asset2.clone(), profit_percentage));
                        }
                    }
                }
            }
        }
    }
    
    best_opportunity
}

#[tokio::main]
async fn main() {
    // 加载 .env 文件
    dotenv().ok();
    env_logger::init();
    
    // 从环境变量读取私钥
    let private_key = env::var("PRIVATE_KEY").expect("PRIVATE_KEY must be set in .env file");
    
    // 初始化钱包
    let wallet: LocalWallet = private_key
        .parse()
        .unwrap();
    let user_address = wallet.address();
    println!("Wallet: {:?}", user_address);

    // 初始化客户端
    let mut info_client = InfoClient::new(None, Some(BaseUrl::Mainnet)).await.unwrap();
    let exchange_client = ExchangeClient::new(None, wallet, Some(BaseUrl::Mainnet), None, None)
        .await
        .unwrap();

    // 从环境变量读取交易参数
    let min_profit_percentage = get_env_or_default("MIN_PROFIT_PERCENTAGE", 0.1); // 最小收益率 0.1%
    let max_trade_amount = get_env_or_default("MAX_TRADE_AMOUNT", 1000.0); // 最大交易金额
    let check_interval = get_env_or_default("CHECK_INTERVAL", 60); // 检查间隔（秒）
    let leverage = get_env_or_default("LEVERAGE", 1) as u32;
    let funding_interval = get_env_or_default("FUNDING_INTERVAL", 8 * 60 * 60); // 资金费率结算间隔（秒）
    
    info!("=== 交易参数 ===");
    info!("最小收益率: {}%", min_profit_percentage);
    info!("最大交易金额: {}", max_trade_amount);
    info!("检查间隔: {}秒", check_interval);
    info!("杠杆倍数: {}x", leverage);
    info!("资金费率结算间隔: {}小时", funding_interval / 3600);

    // 存储资金费率信息
    let mut funding_rates: HashMap<String, f64> = HashMap::new();
    let mut active_positions: HashMap<String, (f64, bool)> = HashMap::new(); // (数量, 是否做多)
    let mut last_funding_time = SystemTime::now();
    let mut daily_pnl = 0.0;
    let mut last_daily_reset = SystemTime::now();

    // 创建消息通道
    let (sender, mut receiver) = unbounded_channel();

    // 订阅资金费率更新
    info_client
        .subscribe(Subscription::AllMids, sender.clone())
        .await
        .unwrap();
    
    info_client
        .subscribe(Subscription::UserEvents { user: user_address }, sender.clone())
        .await
        .unwrap();

    loop {
        info!("=== 开始新一轮检查 ===");

        // 检查是否需要重置每日统计
        let now = SystemTime::now();
        if now.duration_since(last_daily_reset).unwrap().as_secs() >= 24 * 60 * 60 {
            daily_pnl = 0.0;
            last_daily_reset = now;
            info!("重置每日统计");
        }

        // 检查是否需要结算资金费率
        if now.duration_since(last_funding_time).unwrap().as_secs() >= funding_interval {
            // 计算并收取资金费率
            for (asset, (size, is_long)) in &active_positions {
                if let Some(rate) = funding_rates.get(asset) {
                    let funding_payment = size * rate;
                    if *is_long {
                        daily_pnl -= funding_payment;
                    } else {
                        daily_pnl += funding_payment;
                    }
                    info!("资金费率结算 - {}: {} ({}%)", 
                        asset, 
                        funding_payment,
                        rate * 100.0
                    );
                }
            }
            last_funding_time = now;
        }

        // 获取价格和资金费率更新
        match receiver.recv().await {
            Some(Message::AllMids(all_mids)) => {
                let all_mids = all_mids.data.mids;
                
                // 更新资金费率信息
                for (asset, price) in all_mids {
                    if let Ok(rate) = price.parse::<f64>() {
                        funding_rates.insert(asset, rate);
                    }
                }

                // 检查套利机会
                if let Some((asset1, asset2, profit_percentage)) = calculate_funding_arbitrage(
                    &funding_rates,
                    min_profit_percentage,
                ) {
                    info!("发现资金费率套利机会！");
                    info!("交易对: {} - {}", asset1, asset2);
                    info!("预期收益率: {:.2}%", profit_percentage);

                    // 获取资金费率
                    let rate1 = funding_rates.get(&asset1).unwrap();
                    let rate2 = funding_rates.get(&asset2).unwrap();

                    // 确定交易方向
                    let (long_asset, short_asset) = if rate1 > rate2 {
                        (asset2, asset1)
                    } else {
                        (asset1, asset2)
                    };

                    // 设置杠杆倍数
                    for asset in [&long_asset, &short_asset] {
                        match exchange_client.update_leverage(leverage, asset, false, None).await {
                            Ok(_) => info!("成功设置{}杠杆倍数为 {}x", asset, leverage),
                            Err(e) => {
                                error!("设置{}杠杆倍数失败: {:?}", asset, e);
                                continue;
                            }
                        }
                    }

                    // 开仓
                    let position_size = max_trade_amount / 2.0; // 平均分配资金

                    // 做多
                    let long_order = ClientOrderRequest {
                        asset: long_asset.clone(),
                        is_buy: true,
                        reduce_only: false,
                        limit_px: 0.0, // 市价单
                        sz: position_size,
                        cloid: None,
                        order_type: ClientOrder::Limit(ClientLimit {
                            tif: "Gtc".to_string(),
                        }),
                    };

                    match exchange_client.order(long_order, None).await {
                        Ok(ExchangeResponseStatus::Ok(response)) => {
                            if let Some(data) = response.data {
                                if !data.statuses.is_empty() {
                                    if let ExchangeDataStatus::Resting(_order) = &data.statuses[0] {
                                        info!("做多订单已提交: {} {}", position_size, long_asset);
                                        active_positions.insert(long_asset.clone(), (position_size, true));
                                    }
                                }
                            }
                        },
                        Ok(ExchangeResponseStatus::Err(e)) => error!("做多订单失败: {:?}", e),
                        Err(e) => error!("做多订单失败: {:?}", e),
                    }

                    // 做空
                    let short_order = ClientOrderRequest {
                        asset: short_asset.clone(),
                        is_buy: false,
                        reduce_only: false,
                        limit_px: 0.0, // 市价单
                        sz: position_size,
                        cloid: None,
                        order_type: ClientOrder::Limit(ClientLimit {
                            tif: "Gtc".to_string(),
                        }),
                    };

                    match exchange_client.order(short_order, None).await {
                        Ok(ExchangeResponseStatus::Ok(response)) => {
                            if let Some(data) = response.data {
                                if !data.statuses.is_empty() {
                                    if let ExchangeDataStatus::Resting(_order) = &data.statuses[0] {
                                        info!("做空订单已提交: {} {}", position_size, short_asset);
                                        active_positions.insert(short_asset.clone(), (position_size, false));
                                    }
                                }
                            }
                        },
                        Ok(ExchangeResponseStatus::Err(e)) => error!("做空订单失败: {:?}", e),
                        Err(e) => error!("做空订单失败: {:?}", e),
                    }
                }
            },
            Some(Message::User(user_event)) => {
                // 处理用户事件
                match user_event.data {
                    UserData::Fills(fills) => {
                        for fill in fills {
                            info!("订单成交: ID={}, 价格={}, 数量={}, 方向={}", 
                                fill.oid, fill.px, fill.sz, if fill.side == "B" { "买入" } else { "卖出" });
                        }
                    },
                    _ => continue,
                }
            },
            Some(_) => continue,
            None => {
                error!("接收消息通道关闭");
                break;
            }
        }

        // 打印当前状态
        info!("\n=== 当前状态 ===");
        info!("活跃仓位数量: {}", active_positions.len());
        info!("每日盈亏: {}", daily_pnl);
        for (asset, (size, is_long)) in &active_positions {
            info!("{}: {} {} @ {}", 
                asset, 
                if *is_long { "做多" } else { "做空" },
                size,
                if let Some(rate) = funding_rates.get(asset) {
                    format!("{:.4}%", rate * 100.0)
                } else {
                    "未知".to_string()
                }
            );
        }

        // 等待一段时间再进行下一次检查
        info!("\n等待{}秒后进行下一次检查...", check_interval);
        sleep(Duration::from_secs(check_interval)).await;
    }
} 
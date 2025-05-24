use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, InfoClient,
    ClientCancelRequest, ExchangeDataStatus, ExchangeResponseStatus, Message, Subscription, UserData,
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

// 获取所有可用的交易对
fn get_available_pairs(prices: &HashMap<String, f64>) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for pair in prices.keys() {
        if let Some((base, quote)) = pair.split_once('/') {
            pairs.push((base.to_string(), quote.to_string()));
        }
    }
    pairs
}

// 获取所有可用的代币
fn get_available_tokens(pairs: &[(String, String)]) -> Vec<String> {
    let mut tokens = std::collections::HashSet::new();
    for (base, quote) in pairs {
        tokens.insert(base.clone());
        tokens.insert(quote.clone());
    }
    tokens.into_iter().collect()
}

// 计算套利机会
fn calculate_arbitrage_opportunity(
    prices: &HashMap<String, f64>,
    base_currency: &str,
    min_profit_percentage: f64,
) -> Option<(Vec<String>, f64)> {
    let pairs = get_available_pairs(prices);
    let tokens = get_available_tokens(&pairs);
    
    // 存储找到的最佳套利机会
    let mut best_opportunity: Option<(Vec<String>, f64)> = None;
    
    // 只考虑以 base_currency 为起始和结束的路径
    let start_token = base_currency.to_string();
    
    // 对每个可能的中间代币
    for middle_token in &tokens {
        if middle_token == &start_token {
            continue;
        }
        // 对每个可能的结束代币
        for end_token in &tokens {
            if end_token == &start_token || end_token == middle_token {
                continue;
            }
            
            // 构建交易路径
            let path = vec![start_token.clone(), middle_token.clone(), end_token.clone()];
            let mut amount = 1.0;
            let mut current_currency = &start_token;
            
            // 模拟交易路径
            let mut valid_path = true;
            for next_currency in &path[1..] {
                let pair = format!("{}/{}", current_currency, next_currency);
                let reverse_pair = format!("{}/{}", next_currency, current_currency);
                
                if let Some(price) = prices.get(&pair) {
                    amount *= price;
                } else if let Some(price) = prices.get(&reverse_pair) {
                    amount /= price;
                } else {
                    valid_path = false;
                    break;
                }
                current_currency = next_currency;
            }
            
            // 如果路径有效且回到起始代币
            if valid_path && current_currency == &start_token {
                let profit_percentage = (amount - 1.0) * 100.0;
                
                // 如果收益率超过最小要求，且比当前最佳机会更好
                if profit_percentage > min_profit_percentage {
                    match &best_opportunity {
                        None => {
                            best_opportunity = Some((path, profit_percentage));
                        }
                        Some((_, current_best_profit)) => {
                            if profit_percentage > *current_best_profit {
                                best_opportunity = Some((path, profit_percentage));
                            }
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
    let base_currency = env::var("BASE_CURRENCY").unwrap_or_else(|_| "USDT".to_string());
    let min_profit_percentage = get_env_or_default("MIN_PROFIT_PERCENTAGE", 0.5); // 最小收益率 0.5%
    let max_trade_amount = get_env_or_default("MAX_TRADE_AMOUNT", 1000.0); // 最大交易金额
    let check_interval = get_env_or_default("CHECK_INTERVAL", 1); // 检查间隔（秒）
    let leverage = get_env_or_default("LEVERAGE", 1) as u32;
    
    info!("=== 交易参数 ===");
    info!("基础货币: {}", base_currency);
    info!("最小收益率: {}%", min_profit_percentage);
    info!("最大交易金额: {}", max_trade_amount);
    info!("检查间隔: {}秒", check_interval);
    info!("杠杆倍数: {}x", leverage);

    // 设置杠杆倍数
    match exchange_client.update_leverage(leverage, "BTC", false, None).await {
        Ok(_) => info!("成功设置BTC杠杆倍数为 {}x", leverage),
        Err(e) => {
            error!("设置BTC杠杆倍数失败: {:?}", e);
            return;
        }
    }

    match exchange_client.update_leverage(leverage, "ETH", false, None).await {
        Ok(_) => info!("成功设置ETH杠杆倍数为 {}x", leverage),
        Err(e) => {
            error!("设置ETH杠杆倍数失败: {:?}", e);
            return;
        }
    }

    // 存储价格信息
    let mut prices: HashMap<String, f64> = HashMap::new();
    let mut active_orders: Vec<u64> = Vec::new();
    let mut _daily_pnl = 0.0; // 添加下划线前缀表示有意不使用
    let mut last_daily_reset = SystemTime::now();

    // 创建消息通道
    let (sender, mut receiver) = unbounded_channel();

    // 订阅所有交易对的价格
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
            _daily_pnl = 0.0;
            last_daily_reset = now;
            info!("重置每日统计");
        }

        // 获取价格更新
        match receiver.recv().await {
            Some(Message::AllMids(all_mids)) => {
                let all_mids = all_mids.data.mids;
                
                // 更新价格信息
                for (pair, price) in all_mids {
                    prices.insert(pair, price.parse().unwrap());
                }

                // 检查套利机会
                if let Some((path, profit_percentage)) = calculate_arbitrage_opportunity(
                    &prices,
                    &base_currency,
                    min_profit_percentage,
                ) {
                    info!("发现套利机会！");
                    info!("基础货币: {}", base_currency);
                    info!("套利路径: {} -> {} -> {}", path[0], path[1], path[2]);
                    info!("预期收益率: {:.2}%", profit_percentage);
                    
                    // 打印每个交易步骤的详细信息
                    let mut current_currency = &path[0];
                    let mut amount = max_trade_amount;
                    
                    for next_currency in &path[1..] {
                        let pair = format!("{}/{}", current_currency, next_currency);
                        let reverse_pair = format!("{}/{}", next_currency, current_currency);
                        
                        if let Some(price) = prices.get(&pair) {
                            info!("交易步骤: 买入 {} {} @ {}", amount / price, next_currency, price);
                            amount = amount / price;
                        } else if let Some(price) = prices.get(&reverse_pair) {
                            info!("交易步骤: 卖出 {} {} @ {}", amount, next_currency, price);
                            amount = amount * price;
                        }
                        current_currency = next_currency;
                    }
                    
                    info!("初始金额: {} {}", max_trade_amount, base_currency);
                    info!("最终金额: {} {}", amount, base_currency);
                    info!("预期利润: {} {}", amount - max_trade_amount, base_currency);
                    info!("预期收益率: {:.2}%", ((amount - max_trade_amount) / max_trade_amount) * 100.0);

                    // 取消所有现有订单
                    for order_id in &active_orders {
                        info!("取消订单: {}", order_id);
                        match exchange_client.cancel(ClientCancelRequest { 
                            asset: path[0].clone(), 
                            oid: *order_id 
                        }, None).await {
                            Ok(_) => info!("订单取消成功: {}", order_id),
                            Err(e) => error!("取消订单失败: {:?}", e),
                        }
                    }
                    active_orders.clear();

                    // 执行套利交易
                    let mut current_currency = &base_currency;
                    let mut amount = max_trade_amount;

                    for next_currency in &path {
                        let pair = format!("{}/{}", current_currency, next_currency);
                        if let Some(price) = prices.get(&pair) {
                            // 买入
                            let order = ClientOrderRequest {
                                asset: next_currency.clone(),
                                is_buy: true,
                                reduce_only: false,
                                limit_px: *price,
                                sz: amount / price,
                                cloid: None,
                                order_type: ClientOrder::Limit(ClientLimit {
                                    tif: "Gtc".to_string(),
                                }),
                            };

                            match exchange_client.order(order, None).await {
                                Ok(ExchangeResponseStatus::Ok(response)) => {
                                    if let Some(data) = response.data {
                                        if !data.statuses.is_empty() {
                                            if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                                                info!("买入订单已提交: {} {} @ {}", 
                                                    amount / price, next_currency, price);
                                                active_orders.push(order.oid);
                                            }
                                        }
                                    }
                                },
                                Ok(ExchangeResponseStatus::Err(e)) => {
                                    error!("买入订单失败: {:?}", e);
                                    break;
                                },
                                Err(e) => {
                                    error!("买入订单失败: {:?}", e);
                                    break;
                                },
                            }

                            amount = amount / price;
                            current_currency = next_currency;
                        } else {
                            let pair = format!("{}/{}", next_currency, current_currency);
                            if let Some(price) = prices.get(&pair) {
                                // 卖出
                                let order = ClientOrderRequest {
                                    asset: next_currency.clone(),
                                    is_buy: false,
                                    reduce_only: false,
                                    limit_px: *price,
                                    sz: amount,
                                    cloid: None,
                                    order_type: ClientOrder::Limit(ClientLimit {
                                        tif: "Gtc".to_string(),
                                    }),
                                };

                                match exchange_client.order(order, None).await {
                                    Ok(ExchangeResponseStatus::Ok(response)) => {
                                        if let Some(data) = response.data {
                                            if !data.statuses.is_empty() {
                                                if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                                                    info!("卖出订单已提交: {} {} @ {}", 
                                                        amount, next_currency, price);
                                                    active_orders.push(order.oid);
                                                }
                                            }
                                        }
                                    },
                                    Ok(ExchangeResponseStatus::Err(e)) => {
                                        error!("卖出订单失败: {:?}", e);
                                        break;
                                    },
                                    Err(e) => {
                                        error!("卖出订单失败: {:?}", e);
                                        break;
                                    },
                                }

                                amount = amount * price;
                                current_currency = next_currency;
                            }
                        }
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
                            
                            // 从活跃订单中移除
                            if let Some(pos) = active_orders.iter().position(|&x| x == fill.oid) {
                                active_orders.remove(pos);
                            }
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

        // 等待一段时间再进行下一次检查
        info!("\n等待{}秒后进行下一次检查...", check_interval);
        sleep(Duration::from_secs(check_interval)).await;
    }
} 
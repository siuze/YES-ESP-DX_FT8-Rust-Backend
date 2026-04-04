use serde::{Serialize, Deserialize};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use std::fs;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

use crate::config;

const PENDING_FILE: &str = "pending_logs.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QsoLog {
    pub time: String,
    pub freq: String,
    pub call: String,
    pub my_snr: String,
    pub his_snr: String,
    pub grid: String,
    pub region: String,
}

pub struct NotionLogger;

impl NotionLogger {
    pub fn new() -> (Self, tokio::sync::mpsc::Sender<QsoLog>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<QsoLog>(100);
        
        // 初始加载持久化的待上报列表
        let initial_queue = Self::load_pending();
        let queue = Arc::new(Mutex::new(initial_queue));
        
        let worker_queue = queue.clone();
        tokio::spawn(async move {
            // 准备两个客户端
            let proxy = reqwest::Proxy::all(config::PROXY_URL).unwrap();
            let client_proxy = reqwest::Client::builder().proxy(proxy).timeout(Duration::from_secs(30)).build().unwrap();
            let client_direct = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();

            // 任务接收者
            let q_recv = worker_queue.clone();
            tokio::spawn(async move {
                while let Some(log) = rx.recv().await {
                    let mut q = q_recv.lock().await;
                    q.push_back(log);
                    let _ = Self::save_pending(&q); // 每次入队保存一次
                }
            });

            // 消费循环
            loop {
                let log_opt = {
                    let q = worker_queue.lock().await;
                    q.front().cloned()
                };

                if let Some(log) = log_opt {
                    // 尝试 1: 代理
                    let mut res = Self::send_to_notion(&client_proxy, &log).await;
                    
                    // 如果代理网络层失败 (非 Notion API 明确退回的错误)，尝试 2: 直连
                    let mut is_api_err = false;
                    if let Err(ref e) = res {
                        if e.starts_with("Notion ") { // 此前改为 Notion xxx 错误...
                            is_api_err = true;
                        }
                    }

                    if res.is_err() && !is_api_err {
                        crate::utils::log_to_pc("🌐 代理网络超时或不可达，切换直连...");
                        res = Self::send_to_notion(&client_direct, &log).await;
                    }

                    match res {
                        Ok(_) => {
                            let mut q = worker_queue.lock().await;
                            q.pop_front();
                            let _ = Self::save_pending(&q);
                            crate::utils::log_to_pc(&format!("✅ Notion 上报成功: {}", log.call));
                        }
                        Err(e) => {
                            crate::utils::log_to_pc(&format!("❌ 上报失败: {}", e));
                            println!("DEBUG Notion Error: {}", e);
                            
                            // 如果是 4xx 或 5xx 错误，数据格式问题导致被 Notion 拒绝，重试没用还会阻塞队列和产生潜在重试雪崩
                            if e.starts_with("Notion 4") || e.starts_with("Notion 5") {
                                crate::utils::log_to_pc("🛑 API 错误不可达，移除该条以防止阻塞队列");
                                let mut q = worker_queue.lock().await;
                                q.pop_front();
                                let _ = Self::save_pending(&q);
                            } else {
                                // 网络错误过 30 秒重试
                                sleep(Duration::from_secs(30)).await;
                            }
                        }
                    }
                } else {
                    sleep(Duration::from_secs(5)).await;
                }
            }
        });

        (Self, tx)
    }

async fn send_to_notion(client: &reqwest::Client, log: &QsoLog) -> Result<(), String> {
        // 1. 严格按照 Flutter 版本的数据结构构造 Body
        let body = json!({
            "parent": { "database_id": config::NOTION_DATABASE_ID },
            "properties": {
                "对方呼号": {
                    "rich_text": [
                        { "text": { "content": log.call } }
                    ]
                },
                "通联时间(东八区)": {
                    "date": { "start": log.time }
                },
                "频率MHz": {
                    "select": { "name": if log.freq.is_empty() { "-" } else { &log.freq } }
                },
                "模式": {
                    "select": { "name": "FT8" }
                },
                "收信强度": {
                    "rich_text": [
                        { "text": { "content": log.my_snr } }
                    ]
                },
                "对方收信": {
                    "rich_text": [
                        { "text": { "content": log.his_snr } }
                    ]
                },
                "对方QTH": {
                    "rich_text": [
                        { "text": { "content": log.grid } }
                    ]
                },
                "对方归属": {
                    "select": { "name": if log.region.is_empty() { "Unknown" } else { &log.region } }
                },
                "发射电台": {
                    "select": { "name": config::NOTION_RIG_NAME }
                },
                "发射天线": {
                    "select": { "name": config::NOTION_ANTENNA_NAME }
                },
                "发射功率": {
                    "select": { "name": config::NOTION_TX_POWER }
                },
                "发射QTH": {
                    "select": { "name": config::NOTION_TX_QTH }
                }
            }
        });

        // 2. 打印请求内容 (用于调试)
        let request_json = serde_json::to_string_pretty(&body).unwrap_or_default();
        // println!("--- DEBUG REQUEST START --- \n{}\n--- DEBUG REQUEST END ---", request_json);
        // crate::log_to_pc(&format!("📤 发送请求到 Notion: {}", request_json));

        // 3. 执行请求
        let response = client.post("https://api.notion.com/v1/pages")
            .header("Authorization", format!("Bearer {}", config::NOTION_TOKEN))
            .header("Content-Type", "application/json")
            .header("Notion-Version", "2022-06-28")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("网络传输失败: {}", e))?;

        let status = response.status();
        
        // 4. 处理响应
        if status.is_success() {
            Ok(())
        } else {
            // 读取 Notion 返回的报错 JSON
            let err_text = response.text().await.unwrap_or_else(|_| "无法读取错误详情".to_string());
            let full_error = format!("Notion {} 错误详情: {} | 请求内容: {}", status.as_u16(), err_text, request_json);
            Err(full_error)
        }
    }
    fn load_pending() -> VecDeque<QsoLog> {
        fs::read_to_string(PENDING_FILE)
            .and_then(|s| Ok(serde_json::from_str(&s).unwrap_or_default()))
            .unwrap_or_default()
    }

    fn save_pending(queue: &VecDeque<QsoLog>) -> std::io::Result<()> {
        let j = serde_json::to_string_pretty(queue).unwrap();
        fs::write(PENDING_FILE, j)
    }
}
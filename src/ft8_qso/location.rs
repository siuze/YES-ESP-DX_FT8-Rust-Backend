use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CountryInfo {
    pub name: String,
    pub continent: String,
    pub primary_prefix: String,
}

pub struct LocationEngine {
    prefix_map: HashMap<String, Arc<CountryInfo>>,
    zh_map: HashMap<String, String>,
    callsign_cache: HashMap<String, String>,
}

impl LocationEngine {
    pub fn new() -> Self {
        Self {
            prefix_map: HashMap::new(),
            zh_map: HashMap::new(),
            callsign_cache: HashMap::new(),
        }
    }

    /// 加载 cty.dat 和 dxcc_zh.json
    pub fn init(&mut self, cty_path: &str, zh_json_path: &str) {
        // 1. 加载翻译
        if let Ok(content) = std::fs::read_to_string(zh_json_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&content) {
                self.zh_map = map;
            }
        }

        // 2. 解析 cty.dat
        if let Ok(file) = File::open(cty_path) {
            let reader = BufReader::new(file);
            let mut current_country: Option<Arc<CountryInfo>> = None;

            for line in reader.lines().flatten() {
                if line.trim().is_empty() { continue; }

                if line.starts_with(' ') || line.starts_with('\t') {
                    // 前缀行
                    if let Some(ref country) = current_country {
                        let parts = line.trim().trim_end_matches(';').split(',');
                        for pfx in parts {
                            // 清理前缀中的 (WAZ), [ITU], <Lat/Long>
                            let clean_pfx = pfx.split(|c| c == '(' || c == '[' || c == '<')
                                .next().unwrap_or("").trim();
                            if !clean_pfx.is_empty() {
                                self.prefix_map.insert(clean_pfx.to_string(), country.clone());
                            }
                        }
                    }
                } else {
                    // 国家行
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() >= 8 {
                        let country = Arc::new(CountryInfo {
                            name: parts[0].trim().to_string(),
                            continent: parts[3].trim().to_string(),
                            primary_prefix: parts[7].trim().to_string(),
                        });
                        self.prefix_map.insert(country.primary_prefix.clone(), country.clone());
                        current_country = Some(country);
                    }
                }
            }
            println!("📍 CTY 数据库加载成功: {} 条前缀", self.prefix_map.len());
        }
    }

    /// 核心匹配逻辑：最长前缀搜索
    fn match_callsign(&self, callsign: &str) -> Option<Arc<CountryInfo>> {
        let mut effective_call = callsign.to_uppercase();
        
        // 处理斜杠呼号
        if effective_call.contains('/') {
            let parts: Vec<&str> = effective_call.split('/').collect();
            // 逻辑参考 Flutter：通常取最长的部分作为识别依据，或者取第一部分
            if parts.len() >= 2 {
                if parts[0].len() <= 3 {
                    effective_call = parts[1].to_string(); // 如 BY/BG5VDH 取第二段
                } else {
                    effective_call = parts[0].to_string(); // 如 BG5VDH/P 取第一段
                }
            }
        }

        // 从完整呼号长度开始，逐位缩减进行匹配
        for i in (1..=effective_call.len()).rev() {
            let sub = &effective_call[0..i];
            if let Some(info) = self.prefix_map.get(sub) {
                return Some(info.clone());
            }
        }
        None
    }

    /// 获取归属地名称（中文优先）
    pub fn get_region(&mut self, sender: &str) -> String {
        if sender == "Unknown" || sender.is_empty() { return "未知".to_string(); }
        
        let clean_call = sender.trim_matches(|c| c == '<' || c == '>').to_uppercase();

        // 检查缓存
        if let Some(cached) = self.callsign_cache.get(&clean_call) {
            return cached.clone();
        }

        let country_name = if let Some(country) = self.match_callsign(&clean_call) {
            country.name.clone()
        } else {
            "未知地区".to_string()
        };

        // 翻译
        let chinese_name = self.zh_map.get(&country_name).cloned().unwrap_or(country_name);

        // 存入缓存（限制大小）
        if self.callsign_cache.len() > 5000 { self.callsign_cache.clear(); }
        self.callsign_cache.insert(clean_call, chinese_name.clone());

        chinese_name
    }
}
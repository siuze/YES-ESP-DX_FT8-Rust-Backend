use crate::types::{Ft8DecodeResult, STATE, RADIO_LO_FREQ};
use crate::ft8_qso::notion_logger::QsoLog;
use crate::utils::log_to_pc;
use std::collections::{VecDeque, HashMap, HashSet};
use std::time::{Duration, Instant};
use std::sync::atomic::Ordering;
use std::fs;
use std::io::Write;

use crate::config;
use crate::ft8_qso::location::LocationEngine; 

/// 根据频率获取 ADIF 标准波段名称
fn get_band_name(freq_mhz: f64) -> &'static str {
    match freq_mhz {
        1.8..=2.0 => "160M",
        3.5..=3.8 => "80M",
        7.0..=7.3 => "40M",
        10.1..=10.15 => "30M",
        14.0..=14.35 => "20M",
        18.068..=18.168 => "17M",
        21.0..=21.45 => "15M",
        24.89..=24.99 => "12M",
        28.0..=29.7 => "10M",
        50.0..=54.0 => "6M",
        144.0..=148.0 => "2M",
        _ => "OTHER",
    }
}

// --- 系统局部配置 ---
const CALL_GRID_FILE: &str = "call_to_grid.json";  // 呼号与网格的持久化映射文件
const SUCCESSFUL_FILE: &str = "qso_success.json";  // 已成功通联的呼号列表
const GRIDS_FILE: &str = "contacted_grids.json";   // 已成功通联的网格集合
// MY_CALL / MY_GRID / MY_GRID_DETAIL 已迁移至 src/config.rs

/// 清洗呼号：移除哈希标志 '<' '>'
fn clean_call(call: &str) -> String {
    let s = call.replace('<', "").replace('>', "");
    if s == "..." { "UNKNOWN".to_string() } else { s }
}

/// 网格转经纬度逻辑：将 梅登黑德网格 (Maidenhead Grid) 转换为地球经纬度坐标
fn grid_to_lat_lon(grid: &str) -> Option<(f64, f64)> {
    if grid.len() < 4 { return None; }
    let g = grid.as_bytes();
    let mut lon = (g[0].to_ascii_uppercase() as f64 - b'A' as f64) * 20.0 - 180.0;
    lon += (g[2] as f64 - b'0' as f64) * 2.0;
    let mut lat = (g[1].to_ascii_uppercase() as f64 - b'A' as f64) * 10.0 - 90.0;
    lat += (g[3] as f64 - b'0' as f64) * 1.0;
    if grid.len() >= 6 {
        lon += (g[4].to_ascii_lowercase() as f64 - b'a' as f64 + 0.5) * (2.0 / 24.0);
        lat += (g[5].to_ascii_lowercase() as f64 - b'a' as f64 + 0.5) * (1.0 / 24.0);
    } else {
        lon += 1.0; 
        lat += 0.5;
    }
    Some((lat, lon))
}

/// 计算地球表面距离：使用大圆公式 (Haversine formula)
fn calculate_distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0; 
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    r * c
}

/// 自动通联管理器 (AutoQsoManager)
/// 核心职责：处理解码流、维护状态机、决策发射频率、对接 Notion 日志与 PSK Reporter。
pub struct AutoQsoManager {
    msg_history: VecDeque<(Instant, Ft8DecodeResult, bool)>, // 最近 30 分钟的解码历史 (时间, 结果, 是否偶数窗口)
    successful_calls: HashSet<String>,                 // 已通联成功的呼号集合 (本地持久化)
    attempted_recent: HashMap<String, Instant>,        // 针对同一个目标重试的冷却计时器
    successful_grids: HashSet<String>,                 // 已达成的网格集合
    last_incoming_for_me: Instant,                     // 最近一次收到针对我消息的时间
    last_cq_time: Instant,                             // 上一次发起自动 CQ 的时间
    pub task_queue: VecDeque<(String, i16, i16, bool)>, // 通联等待队列 (待处理的任务)
    pub last_logged_at: HashMap<String, Instant>,      // 限制短时间内重复在 Notion 记录同一个 QSO
    pub notion_tx: tokio::sync::mpsc::Sender<QsoLog>,  // Notion 上报通道
    pub location_engine: LocationEngine,               // QTH/国家地理位置引擎
    pub call_to_grid: HashMap<String, String>,          // 动态维护的 呼号-网格 映射表
    
    // --- 基于 FFT 的智能底噪监测 (用于决策静音频率) ---
    pub fft_noise_even: Vec<f32>,      // 偶数发射窗口的波段背景噪声谱
    pub fft_noise_odd: Vec<f32>,       // 奇数发射窗口的波段背景噪声谱
    pub noise_acc: Vec<f32>,           // 实时噪声累加器 (采样中)
    pub noise_count: u32,              // 采样累积计数
    pub noise_dirty: bool,             // 发射期间底噪置脏标志 (发射时屏蔽监测)
    pub last_noise_sec: u32,           // 上次监测噪声的秒数
    pub last_update_even: Instant,     // 偶数窗口噪声数据更新时间
    pub last_update_odd: Instant,      // 奇数窗口噪声数据更新时间
    
    pub consecutive_failures: u32,     // 连续尝试失败次数 (触发紧急策略)
    
    // --- 持久化策略优化 (减少磁盘 IO) ---
    last_save_time: Instant,           // 上次存档时间
    is_dirty: bool,                    // 是否有未存档的变更
}

impl AutoQsoManager {
    pub fn new(notion_tx: tokio::sync::mpsc::Sender<QsoLog>) -> Self {
        let mut loc = LocationEngine::new();
        loc.init("cty.dat", "dxcc_zh.json"); 
        
        // 从本地 JSON 加载历史记录
        let successful_calls: HashSet<String> = fs::read_to_string(SUCCESSFUL_FILE)
            .ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let successful_grids: HashSet<String> = fs::read_to_string(GRIDS_FILE)
            .ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let call_to_grid: HashMap<String, String> = fs::read_to_string(CALL_GRID_FILE)
            .ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();

        log_to_pc(&format!("📂 已加载历史: {} 个呼号, {} 个网格", successful_calls.len(), successful_grids.len()));
        
        Self {
            msg_history: VecDeque::new(),
            successful_calls,
            attempted_recent: HashMap::new(),
            successful_grids,
            last_incoming_for_me: Instant::now().checked_sub(Duration::from_secs(600)).unwrap_or_else(Instant::now),
            last_cq_time: Instant::now().checked_sub(Duration::from_secs(3600)).unwrap_or_else(Instant::now),
            task_queue: VecDeque::new(),
            last_logged_at: HashMap::new(),
            location_engine: loc,
            call_to_grid,
            notion_tx,
            fft_noise_even: Vec::new(),
            fft_noise_odd: Vec::new(),
            noise_acc: Vec::new(),
            noise_count: 0,
            noise_dirty: false,
            last_noise_sec: 99,
            last_update_even: Instant::now().checked_sub(Duration::from_secs(9999)).unwrap_or_else(Instant::now),
            last_update_odd: Instant::now().checked_sub(Duration::from_secs(9999)).unwrap_or_else(Instant::now),
            consecutive_failures: 0,
            last_save_time: Instant::now(),
            is_dirty: false,
        }
    }

    /// 执行存档逻辑 (带节流保护)
    /// force: 是否跳过 5 分钟的时间间隔强制写入 (如在 QSO 成功后)
    pub fn maybe_save(&mut self, force: bool) {
        if !self.is_dirty && !force { return; }
        
        let now = Instant::now();
        if !force && now.duration_since(self.last_save_time) < Duration::from_secs(300) {
            return; // 未达到 5 分钟存档间隔
        }

        if let Ok(j) = serde_json::to_string(&self.successful_calls) { let _ = fs::write(SUCCESSFUL_FILE, j); }
        if let Ok(j) = serde_json::to_string(&self.successful_grids) { let _ = fs::write(GRIDS_FILE, j); }
        if let Ok(j) = serde_json::to_string(&self.call_to_grid) { let _ = fs::write(CALL_GRID_FILE, j); }
        
        self.last_save_time = now;
        self.is_dirty = false;
        log_to_pc("💾 自动存档完成：呼号、网格与历史记录已持久化。");
    }

    /// 接收最新解码结果并更新状态池
    pub fn push_decode(&mut self, res: Ft8DecodeResult, is_even: bool) {
        let now = Instant::now();
        if res.snr != 99 && res.text.contains(config::MY_CALL) { self.last_incoming_for_me = now; }

        if let Some(ref call_raw) = res.sender_call {
            let call: String = clean_call(call_raw);
            if let Some(ref grid) = res.grid {
                // 如果解码到了地理网格，加入映射表
                if res.snr != 99 && call != "Unknown" && grid != "RR73" && grid != "RRR" && grid != "73"{
                    self.call_to_grid.insert(call, grid.clone());
                    self.is_dirty = true; // 标记有新数据需要存档
                }
            }
        }
        
        // 清理超过 30 分钟的陈旧记录
        while self.msg_history.front().map_or(false, |(t, _, _)| now.duration_since(*t) > Duration::from_secs(1800)) {
            self.msg_history.pop_front();
        }
        
        self.msg_history.push_back((now, res, is_even));
    }

    /// 接收我方发射的消息并同步至状态池与日志判定
    pub fn push_tx_decode(&mut self, text: &str, freq: f32, is_even: bool) {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() < 2 { return; }
        
        let receiver = clean_call(parts[0]);
        let sender = clean_call(parts[1]);
        
        let res = Ft8DecodeResult {
            text: text.to_string(),
            snr: 99, // 特殊值标记为我方发射
            freq,
            dt: 0.0,
            sender_call: Some(sender),
            receiver_call: Some(receiver),
            grid: parts.get(2).map(|s| s.to_string()),
            decode_time_ms: 0,
            region: None,
        };
        
        self.push_decode(res.clone(), is_even);
        self.check_and_log_qso(&res);
    }

    /// 通联日志判定与上报逻辑 (处理 Notion 下发)
    pub fn check_and_log_qso(&mut self, res: &Ft8DecodeResult) {
        let sender = res.sender_call.as_deref().map(clean_call).unwrap_or_default();
        let receiver = res.receiver_call.as_deref().map(clean_call).unwrap_or_default();
        
        // 只有涉及我的消息才处理
        if sender != config::MY_CALL && receiver != config::MY_CALL { return; }
        
        // 判定通联成功的标志：只要我方或对方发送了 RR73/RRR/73 就算成功
        let is_end = res.text.contains(" RR73") || res.text.contains(" RRR") || res.text.contains(" 73");
        if !is_end { return; }

        let his_call = if sender == config::MY_CALL { receiver } else { sender };
        if his_call == "UNKNOWN" || his_call.is_empty() { return; }
        let now = Instant::now();
        if let Some(last) = self.last_logged_at.get(&his_call) {
            if now.duration_since(*last) < Duration::from_secs(600) { return; }
        }
        self.last_logged_at.insert(his_call.clone(), now);

        // 成功，加入本地数据库 (采用 CALL:BAND 格式实现波段查重)
        let freq_mhz = {
            let lo = RADIO_LO_FREQ.load(Ordering::SeqCst);
            let s = STATE.get().unwrap().read().unwrap();
            (lo + (s.current_if_hz as u64)) as f64 / 1_000_000.0
        };
        let band = get_band_name(freq_mhz);
        self.successful_calls.insert(format!("{}:{}", his_call, band));
        self.consecutive_failures = 0; 

        // 追溯历史中的 SNR 报告
        let mut my_rcv_snr = String::new();
        let mut his_rcv_snr = String::new();
        for (_, h, _) in self.msg_history.iter().rev() {
            let h_sender = h.sender_call.as_deref().map(clean_call).unwrap_or_default();
            let h_receiver = h.receiver_call.as_deref().map(clean_call).unwrap_or_default();
            
            if h_sender == his_call && h_receiver == config::MY_CALL {
                // 情况 A: 对方发给我的消息。h.snr 是我接收对方的强度。
                if my_rcv_snr.is_empty() && h.snr != 99 {
                    my_rcv_snr = format!("{:+03}", h.snr);
                }
                // 如果消息文本里带报告 (如 R-10)，那是对方接收我的强度。
                if let Some(s) = Self::extract_snr_from_text(&h.text) {
                    if his_rcv_snr.is_empty() { his_rcv_snr = s; }
                }
            }
            if h_sender == config::MY_CALL && h_receiver == his_call {
                // 情况 B: 我发给对方的消息。文本里带的报告是我接收对方的强度。
                if let Some(s) = Self::extract_snr_from_text(&h.text) {
                    if my_rcv_snr.is_empty() { my_rcv_snr = s; }
                }
            }
            if !my_rcv_snr.is_empty() && !his_rcv_snr.is_empty() { break; }
        }
        
        let freq_mhz = {
            let lo = RADIO_LO_FREQ.load(Ordering::SeqCst);
            let s = STATE.get().unwrap().read().unwrap();
            (lo + (s.current_if_hz as u64)) as f64 / 1_000_000.0
        };
        
        let his_grid = self.call_to_grid.get(&his_call).cloned();
        let mut grid_display = his_grid.clone().unwrap_or_else(|| "未知".to_string());
        if let Some(ref g) = his_grid {
            if let (Some(pos1), Some(pos2)) = (grid_to_lat_lon(config::MY_GRID_DETAIL), grid_to_lat_lon(g)) {
                let dist = calculate_distance(pos1.0, pos1.1, pos2.0, pos2.1);
                grid_display = format!("{} [{:.0}km]", g, dist);
            }
            self.successful_grids.insert(g.clone());
        }

        // 推送至异步 Notion 上报队列
        let log = QsoLog {
            time: (chrono::Utc::now() + chrono::Duration::hours(8)).format("%Y-%m-%dT%H:%M:%S+08:00").to_string(),
            freq: format!("{:.3}", freq_mhz),
            call: his_call.clone(),
            my_snr: my_rcv_snr.clone(),
            his_snr: his_rcv_snr.clone(),
            grid: grid_display,
            region: self.location_engine.get_region(&his_call),
        };
        
        // 同时保存 ADIF 到本地
        Self::save_to_adif(&his_call, &his_grid.unwrap_or_default(), &my_rcv_snr, &his_rcv_snr, freq_mhz);

        let tx = self.notion_tx.clone();
        tokio::spawn(async move { let _ = tx.send(log).await; });

        self.is_dirty = true;
        self.maybe_save(true); // 通联成功后强制立即存档一次
    }

    /// 将通联记录保存为标准 ADIF 格式追加到本地文件
    fn save_to_adif(call: &str, grid: &str, rst_sent: &str, rst_rcvd: &str, freq_mhz: f64) {
        let now = chrono::Utc::now() + chrono::Duration::hours(8);
        let date_str = now.format("%Y%m%d").to_string();
        let time_str = now.format("%H%M%S").to_string();
        let band = match freq_mhz {
            1.8..=2.0 => "160M", 3.5..=3.8 => "80M", 7.0..=7.3 => "40M", 10.1..=10.15 => "30M",
            14.0..=14.35 => "20M", 18.068..=18.168 => "17M", 21.0..=21.45 => "15M", 24.89..=24.99 => "12M",
            28.0..=29.7 => "10M", 50.0..=54.0 => "6M", 144.0..=148.0 => "2M", _ => "OTHER",
        };

        let adif = format!(
            "<CALL:{len}>{call} <GRIDSQUARE:{glen}>{grid} <MODE:3>FT8 <RST_SENT:{slen}>{rs} <RST_RCVD:{rlen}>{rr} <QSO_DATE:8>{date} <TIME_ON:6>{time} <BAND:{blen}>{band} <FREQ:{flen}>{freq:.6} <STATION_CALLSIGN:{mlen}>{my} <EOR>\n",
            len = call.len(), call = call, glen = grid.len(), grid = grid,
            slen = rst_sent.len(), rs = rst_sent, rlen = rst_rcvd.len(), rr = rst_rcvd,
            date = date_str, time = time_str, blen = band.len(), band = band,
            flen = format!("{:.6}", freq_mhz).len(), freq = freq_mhz,
            mlen = config::MY_CALL.len(), my = config::MY_CALL
        );

        let log_dir = "logs";
        if !std::path::Path::new(log_dir).exists() { let _ = fs::create_dir(log_dir); }
        let log_path = format!("{}/wsjtx_log.adi", log_dir);
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = file.write_all(adif.as_bytes());
        }
    }
    
    pub fn report_failure(&mut self) { self.consecutive_failures += 1; log_to_pc(&format!("❌ 目标未回复 (当前连续失败: {})", self.consecutive_failures)); }
    pub fn report_any_reply(&mut self) { if self.consecutive_failures > 0 { log_to_pc("✅ 收到回复，重置失败计数"); self.consecutive_failures = 0; } }

    /// 从 FT8 消息文本中提取 SNR 字段（格式如 "+05"、"-12"、"R-07" 等）
    fn extract_snr_from_text(text: &str) -> Option<String> {
        let re = regex::Regex::new(r"R?[+-]\d{1,2}$").ok()?;
        let last = text.split_whitespace().last()?;
        if re.is_match(last) {
            let snr = last.strip_prefix('R').unwrap_or(last);
            Some(snr.to_string())
        } else { None }
    }

    /// 判定是否为 BnCRA 系列特设台 (B0CRA - B9CRA)
    fn is_cra_special(call: &str) -> bool {
        let call = call.to_uppercase();
        if call.len() != 5 { return false; }
        call.starts_with('B') && call.ends_with("CRA") && call.as_bytes()[1].is_ascii_digit()
    }

    /// --- 自动通联核心逻辑 (Mode 3: 自动化全通联处理) ---

    /// 判断呼号所属区域：用于本地优先/DX (远距离) 优先策略 (中日韩印尼定义为本地)
    fn is_local_area(call: &str) -> bool {
        let call = call.to_uppercase();
        if call.len() < 2 { return false; }
        let p2 = &call[0..2];
        let p3 = if call.len() >= 3 { &call[0..3] } else { "" };
        if call.starts_with('B') { match p2 { "BV" | "BU" | "BW" | "BX" => return false, _ => return true } }
        if p3 == "VR2" || p3 == "XX9" { return false; }
        match p2 {
            "JA"|"JB"|"JC"|"JD"|"JE"|"JF"|"JG"|"JH"|"JI"|"JJ"|"JK"|"JL"|"JM"|"JN"|"JO"|"JP"|"JQ"|"JR"|"JS"|"7J"|"7K"|"7L"|"7M"|"7N"|"8J"|"8K"|"8L"|"8M"|"8N" => true,
            "HL"|"DS"|"DT"|"D7"|"D8"|"D9"|"6K"|"6L"|"6M"|"6N" => true,
            "YB"|"YC"|"YD"|"YE"|"YF"|"YG"|"YH" => true,
            _ => false
        }
    }
    
    /// 策略：全宽频自动追踪 (Auto Chase)
    pub fn check_auto_chase(&mut self, is_idle: bool) -> Option<(String, i16, i16, bool)> {
        if !is_idle || Instant::now().duration_since(self.last_incoming_for_me) < Duration::from_secs(30) { return None; }
        let now = Instant::now();

        let current_tx_even = {
            if let Some(state_arc) = STATE.get() {
                if let Ok(s) = state_arc.read() {
                    s.status.tx_window_even == 1
                } else { true }
            } else { true }
        };

        let mut candidates: Vec<(Ft8DecodeResult, bool)> = self.msg_history.iter()
            .filter(|(t, r, _e)| {
                let call_raw = match r.sender_call.as_ref() { Some(s) => s, None => return false };
                let is_recent = now.duration_since(*t) < Duration::from_secs(20);
                let is_far = self.get_distance(&clean_call(call_raw)).unwrap_or(0.0) > 4000.0;
                is_recent || (is_far && now.duration_since(*t) < Duration::from_secs(300))
            })
            .map(|(_, r, e)| (r.clone(), *e))
            .filter(|(r, _e)| {
                let call = clean_call(r.sender_call.as_ref().unwrap());
                if r.sender_call.as_ref().unwrap().contains('<') { return false; }
                let is_standard_trigger = r.text.starts_with("CQ ") || r.text.contains(" RRR") || r.text.contains(" RR73") || r.text.ends_with(" 73");
                let is_rare = !Self::is_local_area(&call);
                let is_dx_aggressive = is_rare && self.call_to_grid.contains_key(&call);
                let is_super_far = self.get_distance(&call).unwrap_or(0.0) > 4000.0;
                is_standard_trigger || is_dx_aggressive || is_super_far
            }).collect();

        candidates.retain(|(r, _e)| {
            let call = clean_call(r.sender_call.as_ref().unwrap()); 
            if call == config::MY_CALL { return false; }
            
            // 波段查重逻辑
            let freq_mhz = {
                let lo = RADIO_LO_FREQ.load(Ordering::SeqCst);
                let s = STATE.get().unwrap().read().unwrap();
                (lo + (s.current_if_hz as u64)) as f64 / 1_000_000.0
            };
            let band = get_band_name(freq_mhz);
            let band_key = format!("{}:{}", call, band);
            
            // 如果该波段已通联，或者该呼号有全局通联记录 (兼容老版本)，则过滤
            if self.successful_calls.contains(&band_key) || self.successful_calls.contains(&call) {
                return false; 
            }

            let wait_time = if !Self::is_local_area(&call) { 600 } else { 1200 };
            if let Some(t) = self.attempted_recent.get(&call) { if now.duration_since(*t) < Duration::from_secs(wait_time) { return false; } }
            true
        });

        // 排序优先级：BnCRA特设台 > 超级远距离 > 稀有地区 (DX) > 窗口一致性 > CQ消息 > 新网格 > 强SNR
        candidates.sort_by(|(a, e_a), (b, e_b)| {
            let ac_raw = a.sender_call.as_ref().unwrap(); let bc_raw = b.sender_call.as_ref().unwrap();
            let ac = clean_call(ac_raw); let bc = clean_call(bc_raw);
            
            // 0. BnCRA 特设台 (最高优先级)
            let acra = Self::is_cra_special(&ac);
            let bcra = Self::is_cra_special(&bc);
            if acra != bcra { return bcra.cmp(&acra); }

            // 1. 超级远距离
            let asf = self.get_distance(&ac).unwrap_or(0.0) > 4000.0; 
            let bsf = self.get_distance(&bc).unwrap_or(0.0) > 4000.0;
            if asf != bsf { return bsf.cmp(&asf); }
            
            // 2. 稀有地区 (DX)
            let ar = !Self::is_local_area(&ac); 
            let br = !Self::is_local_area(&bc);
            if ar != br { return br.cmp(&ar); }

            // 3. 窗口一致性：尽量不变更奇偶窗口 (目标是偶数，则我们需要在奇数发送，反之亦然)
            let a_tx_even = !*e_a;
            let b_tx_even = !*e_b;
            let a_match = a_tx_even == current_tx_even;
            let b_match = b_tx_even == current_tx_even;
            if a_match != b_match { return b_match.cmp(&a_match); }

            // 4. CQ 优先
            let acq = a.text.starts_with("CQ "); let bcq = b.text.starts_with("CQ ");
            if acq != bcq { return bcq.cmp(&acq); }
            
            // 5. 新网格
            let ang = a.grid.as_ref().map_or(false, |g| !self.successful_grids.contains(g));
            let bng = b.grid.as_ref().map_or(false, |g| !self.successful_grids.contains(g));
            if ang != bng { return bng.cmp(&ang); }
            
            // 6. 强 SNR
            b.snr.cmp(&a.snr)
        });

        if let Some((target, e)) = candidates.first() {
            let call = target.sender_call.as_ref().unwrap();
            let tx_even = !*e;
            if now.duration_since(if tx_even { self.last_update_even } else { self.last_update_odd }) > Duration::from_secs(120) {
                crate::utils::log_to_pc(&format!("⚠️ 目标窗口噪声过旧，等待更新...")); return None;
            }
            self.attempted_recent.insert(call.clone(), now);
            log_to_pc(&format!("🎯 策略追踪锁定: {} Freq: {}", call, target.freq));
            Some((format!("{} {} {}", call, config::MY_CALL, config::MY_GRID), self.find_quiet_freq(tx_even), target.freq as i16, tx_even))
        } else { None }
    }

    /// 策略：静默期自动 CQ (Auto CQ)
    /// 当满足冷却条件且波段底噪最新时，在一个干净的寂静频率发起自动呼叫。
    pub fn check_auto_cq(&mut self, is_idle: bool) -> Option<(String, i16, bool)> {
        let now = Instant::now();
        if !is_idle { return None; }
        
        let is_failed_driven = self.consecutive_failures >= 2;
        if !is_failed_driven && now.duration_since(self.last_incoming_for_me) < Duration::from_secs(45) { return None; }
        if !is_failed_driven && now.duration_since(self.last_cq_time) < Duration::from_secs(180) { return None; }
        
        use chrono::Timelike;
        let minute = chrono::Utc::now().minute();
        // 严格的 CQ 奇偶窗口规则：每小时前半小时 (0-29分) 偶数窗口，后半小时 (30-59分) 奇数窗口
        let required_cq_even = minute < 30;

        if now.duration_since(if required_cq_even { self.last_update_even } else { self.last_update_odd }) > Duration::from_secs(120) { return None; }
        
        self.last_cq_time = now;
        self.consecutive_failures = 0; 
        log_to_pc(&format!("🎯 状态机策略触发: 发起自动调优 CQ (指定窗口: {})", if required_cq_even { "偶数" } else { "奇数" }));
        Some((format!("CQ {} {}", config::MY_CALL, config::MY_GRID), self.find_quiet_freq(required_cq_even), required_cq_even))
    }

    /// 智能频率选择：根据采集到的波段能量谱，寻找 50Hz 宽度的最寂静窗。
    fn find_quiet_freq(&self, is_even: bool) -> i16 {
        let v = if is_even { &self.fft_noise_even } else { &self.fft_noise_odd };
        if v.is_empty() { return 1000; }
        let (min_bin, max_bin, win) = ((200.0/2.93) as usize, (2950.0/2.93) as usize, 17);
        let (mut b_f, mut min_score) = (1000, f32::MAX);
        for b in (min_bin..max_bin).step_by(7) {
            if b + win <= v.len() {
                let s: f32 = v[b..b+win].iter().sum();
                let freq = (b as f32 * 2.93) as i16;
                
                // 为照顾部分电台滤波器的带通特性，优先选择 500~2500Hz。
                // 对超出此“黄金频段”的区域施加 1.6 倍底噪惩罚，除非该区域极其安静，否则不予选择。
                let score = if freq >= 500 && freq <= 2700 { s } else { s * 1.6 };
                
                if score < min_score { 
                    min_score = score; 
                    b_f = freq; 
                }
            }
        }
        b_f
    }

    /// 后台底噪监测任务：在非发射期间累加 FFT 能量并更新背景噪声谱。
    pub fn push_fft_noise(&mut self, sec: u32, norms: &[f32], is_txing: bool) {
        let (sw, iv) = (sec % 15, (sec % 30) < 15);
        if sec != self.last_noise_sec {
            if sw == 0 { self.noise_acc.clear(); self.noise_acc.resize(norms.len(), 0.0); self.noise_count = 0; self.noise_dirty = false; }
            if is_txing { self.noise_dirty = true; }
            if sw >= 1 && sw <= 10 && !self.noise_dirty {
                if self.noise_acc.len() == norms.len() { for i in 0..norms.len() { self.noise_acc[i] += norms[i]; } self.noise_count += 1; }
            }
            if sw == 11 && self.noise_count > 0 && !self.noise_dirty {
                let now = Instant::now();
                let avg = self.noise_acc.iter().map(|&x| x / (self.noise_count as f32)).collect();
                if iv { self.fft_noise_even = avg; self.last_update_even = now; } else { self.fft_noise_odd = avg; self.last_update_odd = now; }
            }
            self.last_noise_sec = sec;
        }
    }

    pub fn get_distance(&self, call: &str) -> Option<f64> {
        self.call_to_grid.get(call).and_then(|tg| {
            let p1 = grid_to_lat_lon(config::MY_GRID_DETAIL)?; let p2 = grid_to_lat_lon(tg)?;
            Some(calculate_distance(p1.0, p1.1, p2.0, p2.1))
        })
    }

    /// --- 活跃通联响应逻辑 (Mode 3 专用) ---
    /// 当处于持续通联模式时，处理针对我的回复消息，并推动状态机向前演进。
    pub fn handle_auto_qso_logic(&mut self, decoded_msg_raw: &str, snr: i32, freq: f32, wind_sec: u8) {
        let state_arc = STATE.get().unwrap();
        let mut s = state_arc.write().unwrap();

        // 1. 模式检查：仅处理模式 3 (持续自动通联)
        if s.status.auto_tx_mode != 3 {
            return;
        }

        // 2. 清洗并格式化消息
        let decoded_msg = decoded_msg_raw.trim().to_uppercase();
        if decoded_msg.is_empty() { return; }
        
        if !decoded_msg.contains(config::MY_CALL) { return; }

        let sender_raw = crate::ft8_qso::qso_utils::get_sender_call(&decoded_msg).unwrap_or_else(|| "UNKNOWN".to_string());
        let sender = clean_call(&sender_raw); // 清洗
        let tx_is_even = !((wind_sec % 30) == 0);

        let next_tx = crate::ft8_qso::qso_utils::get_next_tx_msg(&decoded_msg, config::MY_CALL, config::MY_GRID, snr);
        if next_tx.is_empty() {
             log_to_pc("🛑 当前通联已结束 (收到73)，清理缓存等待下一轮采样。");
             s.status.pending_msg = [0u8; 24]; // [关键修复]
             s.status.expect_regex = [0u8; 48];
             s.expect_regex_compiled = None;
             return;
        }

        let current_pending = String::from_utf8_lossy(&s.status.pending_msg).trim_matches(char::from(0)).to_string();

        let is_replaceable = current_pending.is_empty() || current_pending.starts_with("CQ ") || 
            (current_pending.split_whitespace().count() == 3 && current_pending.ends_with(config::MY_GRID));

        if is_replaceable || current_pending.contains(&sender) {
            log_to_pc(&format!("🎯 Mode 3 响应匹配 [%{}] -> 下一条: [{}]", decoded_msg, next_tx));
            
            // 判断是否需要重置重复计数机制 (防止同阶段陷入死循环)
            let mut should_reset_repeat = true;
            if next_tx == current_pending {
                should_reset_repeat = false; // 完全相同的消息不重置计数
            } else if !current_pending.is_empty() {
                let p_parts: Vec<&str> = current_pending.split_whitespace().collect();
                let n_parts: Vec<&str> = next_tx.split_whitespace().collect();
                if p_parts.len() == 3 && n_parts.len() == 3 && p_parts[0] == sender && n_parts[0] == sender {
                    let p_last = p_parts[2];
                    let n_last = n_parts[2];
                    let is_snr = |s: &str| -> bool { s.starts_with('-') || s.starts_with('+') };
                    let is_rsnr = |s: &str| -> bool { s.starts_with("R-") || s.starts_with("R+") };
                    
                    if is_snr(p_last) && is_snr(n_last) {
                        should_reset_repeat = false; // 同属普通信号报告，不重置
                    } else if is_rsnr(p_last) && is_rsnr(n_last) {
                        should_reset_repeat = false; // 同属带 R 确认信号报告，不重置
                    }
                }
            }

            let bytes = next_tx.as_bytes();
            let len = bytes.len().min(24);
            s.status.pending_msg = [0u8; 24];
            s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
            
            if should_reset_repeat {
                s.status.repeat_count = 0; // 发生实质性阶段切换，从头开始，首发寂静频率
            } else {
                // 对方重复发送消息，我们需要无限期陪跑（防止因为达到 4 次上限而断开），
                // 同时我们要**保持频偏交替逻辑**！
                // 此时 repeat_count % 2 的值恰好指示了下一个应该用的频率。
                // 我们直接取模并保留奇偶性，这样既重置了超时机制，又完美对换/延续了双频交替顺序。
                s.status.repeat_count = s.status.repeat_count % 2;
            }
            self.report_any_reply(); // 重置连续失败计数
            
            let quiet_f = self.find_quiet_freq(tx_is_even);
            s.status.pending_offset = quiet_f as u16;
            s.target_offset = freq as u16;
            s.status.tx_window_even = if tx_is_even { 1 } else { 0 };
            
            // 清理掉正则匹配相关遗留字段，以防串联影响
            s.status.expect_regex = [0u8; 48];
            s.expect_regex_compiled = None;
        } else {
            if !self.task_queue.iter().any(|(m, _, _, _)| m.contains(&sender)) {
                log_to_pc(&format!("⏳ 队列等待: {}", sender));
                let quiet_f = self.find_quiet_freq(tx_is_even);
                self.task_queue.push_back((next_tx, quiet_f, freq as i16, tx_is_even));
            }
        }
    }
}
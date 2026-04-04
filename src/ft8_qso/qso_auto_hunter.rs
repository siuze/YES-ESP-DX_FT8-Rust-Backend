use crate::types::{Ft8DecodeResult, STATE, RADIO_LO_FREQ};
use crate::ft8_qso::notion_logger::QsoLog;
use crate::utils::log_to_pc;
use std::collections::{VecDeque, HashMap, HashSet};
use std::time::{Duration, Instant};
use std::sync::atomic::Ordering;
use std::fs; 
use chrono::Timelike;
use crate::ft8_qso::location::LocationEngine; 

use crate::config;

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
    msg_history: VecDeque<(Instant, Ft8DecodeResult)>, // 最近 30 分钟的解码历史
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
            last_incoming_for_me: Instant::now() - Duration::from_secs(600),
            last_cq_time: Instant::now() - Duration::from_secs(3600),
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
            last_update_even: Instant::now() - Duration::from_secs(9999),
            last_update_odd: Instant::now() - Duration::from_secs(9999),
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
    pub fn push_decode(&mut self, res: Ft8DecodeResult) {
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
        while self.msg_history.front().map_or(false, |(t, _)| now.duration_since(*t) > Duration::from_secs(1800)) {
            self.msg_history.pop_front();
        }
        self.msg_history.push_back((now, res));
    }

    /// 通联日志判定与上报逻辑 (处理 Notion 下发)
    pub fn check_and_log_qso(&mut self, res: &Ft8DecodeResult) {
        let receiver = res.receiver_call.as_deref().map(clean_call).unwrap_or_default();
        if receiver != config::MY_CALL { return; }
        
        // 只有收到对方的 RR73/RRR/73 标识通联正式结束，才会记录日志
        let is_end = res.text.contains(" RR73") || res.text.contains(" RRR") || res.text.contains(" 73");
        if !is_end { return; }

        let his_call = match res.sender_call.as_ref() { Some(c) => clean_call(c), None => return };
        let now = Instant::now();
        if let Some(last) = self.last_logged_at.get(&his_call) {
            if now.duration_since(*last) < Duration::from_secs(600) { return; }
        }
        self.last_logged_at.insert(his_call.clone(), now);

        // 成功，加入本地数据库
        self.successful_calls.insert(his_call.clone());
        self.consecutive_failures = 0; 

        // 追溯历史中的 SNR 报告
        let mut my_rcv_snr = String::new();
        let mut his_rcv_snr = String::new();
        for (_, h) in self.msg_history.iter().rev() {
            let h_sender = h.sender_call.as_deref().map(clean_call).unwrap_or_default();
            let h_receiver = h.receiver_call.as_deref().map(clean_call).unwrap_or_default();
            if h_sender == his_call && h_receiver == config::MY_CALL {
                if let Some(s) = Self::extract_snr_from_text(&h.text) { if his_rcv_snr.is_empty() { his_rcv_snr = s; } }
            }
            if h_sender == config::MY_CALL && h_receiver == his_call {
                if let Some(s) = Self::extract_snr_from_text(&h.text) { if my_rcv_snr.is_empty() { my_rcv_snr = s; } }
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
            my_snr: my_rcv_snr,
            his_snr: his_rcv_snr,
            grid: grid_display,
            region: self.location_engine.get_region(&his_call),
        };
        let tx = self.notion_tx.clone();
        tokio::spawn(async move { let _ = tx.send(log).await; });

        self.is_dirty = true;
        self.maybe_save(true); // 通联成功后强制立即存档一次
    }
    
    pub fn report_failure(&mut self) { self.consecutive_failures += 1; log_to_pc(&format!("❌ 目标未回复 (当前连续失败: {})", self.consecutive_failures)); }
    pub fn report_any_reply(&mut self) { if self.consecutive_failures > 0 { log_to_pc("✅ 收到回复，重置失败计数"); self.consecutive_failures = 0; } }

    /// 从 FT8 消息文本中提取 SNR 字段（格式如 "+05"、"-12"、"R-07" 等）
    fn extract_snr_from_text(text: &str) -> Option<String> {
        let re = regex::Regex::new(r"R?[+-]\d{1,2}$").ok()?;
        let last = text.split_whitespace().last()?;
        if re.is_match(last) { Some(last.to_string()) } else { None }
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
    /// 寻找正在叫 CQ 或通联刚结束的“优质”潜在通联目标。优先级：超远距离 > 稀有 DX > 新网格 > SNR 高信号好。
    pub fn check_auto_chase(&mut self, is_idle: bool) -> Option<(String, i16, i16, bool)> {
        if !is_idle || Instant::now().duration_since(self.last_incoming_for_me) < Duration::from_secs(30) { return None; }
        let now = Instant::now();
        let mut candidates: Vec<Ft8DecodeResult> = self.msg_history.iter()
            .filter(|(t, r)| {
                let call_raw = match r.sender_call.as_ref() { Some(s) => s, None => return false };
                let is_recent = now.duration_since(*t) < Duration::from_secs(20);
                let is_far = self.get_distance(&clean_call(call_raw)).unwrap_or(0.0) > 4000.0;
                is_recent || (is_far && now.duration_since(*t) < Duration::from_secs(300))
            })
            .map(|(_, r): &(Instant, Ft8DecodeResult)| r.clone())
            .filter(|r| {
                let call = clean_call(r.sender_call.as_ref().unwrap());
                if r.sender_call.as_ref().unwrap().contains('<') { return false; }
                let is_standard_trigger = r.text.starts_with("CQ ") || r.text.contains(" RRR") || r.text.contains(" RR73") || r.text.ends_with(" 73");
                let is_rare = !Self::is_local_area(&call);
                let is_dx_aggressive = is_rare && self.call_to_grid.contains_key(&call);
                let is_super_far = self.get_distance(&call).unwrap_or(0.0) > 4000.0;
                is_standard_trigger || is_dx_aggressive || is_super_far
            }).collect();

        candidates.retain(|r| {
            let call = clean_call(r.sender_call.as_ref().unwrap()); 
            if self.successful_calls.contains(&call) || call == config::MY_CALL { return false; }
            let wait_time = if !Self::is_local_area(&call) { 600 } else { 1200 };
            if let Some(t) = self.attempted_recent.get(&call) { if now.duration_since(*t) < Duration::from_secs(wait_time) { return false; } }
            true
        });

        // 排序优先级：超级远距离 > 稀有地区 (DX) > CQ消息 > 新网格 > 强SNR
        candidates.sort_by(|a, b| {
            let ac = a.sender_call.as_ref().unwrap(); let bc = b.sender_call.as_ref().unwrap();
            let asf = self.get_distance(ac).unwrap_or(0.0) > 4000.0; let bsf = self.get_distance(bc).unwrap_or(0.0) > 4000.0;
            if asf != bsf { return bsf.cmp(&asf); }
            let ar = !Self::is_local_area(ac); let br = !Self::is_local_area(bc);
            if ar != br { return br.cmp(&ar); }
            let acq = a.text.starts_with("CQ "); let bcq = b.text.starts_with("CQ ");
            if acq != bcq { return bcq.cmp(&acq); }
            let ang = a.grid.as_ref().map_or(false, |g| !self.successful_grids.contains(g));
            let bng = b.grid.as_ref().map_or(false, |g| !self.successful_grids.contains(g));
            if ang != bng { return bng.cmp(&ang); }
            b.snr.cmp(&a.snr)
        });

        if let Some(target) = candidates.first() {
            let call = target.sender_call.as_ref().unwrap();
            let tx_even = !((target.dt.round() as i32 % 30) == 0);
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
        
        let is_even = chrono::Utc::now().second() > 20 && chrono::Utc::now().second() < 50;
        if now.duration_since(if is_even { self.last_update_even } else { self.last_update_odd }) > Duration::from_secs(120) { return None; }
        
        self.last_cq_time = now;
        self.consecutive_failures = 0; 
        log_to_pc("🎯 状态机策略触发: 发起自动调优 CQ");
        Some((format!("CQ {} {}", config::MY_CALL, config::MY_GRID), self.find_quiet_freq(is_even), is_even))
    }

    /// 智能频率选择：根据采集到的波段能量谱，寻找 50Hz 宽度的最寂静窗。
    fn find_quiet_freq(&self, is_even: bool) -> i16 {
        let v = if is_even { &self.fft_noise_even } else { &self.fft_noise_odd };
        if v.is_empty() { return 1000; }
        let (min_bin, max_bin, win) = ((100.0/2.93) as usize, (2950.0/2.93) as usize, 17);
        let (mut b_f, mut min_s) = (1000, f32::MAX);
        for b in (min_bin..max_bin).step_by(7) {
            if b + win <= v.len() {
                let s: f32 = v[b..b+win].iter().sum();
                if s < min_s { min_s = s; b_f = (b as f32 * 2.93) as i16; }
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
    pub fn handle_auto_qso_logic(&mut self, decoded_msg_raw: &str, snr: i32, freq: f32, dt: f32) {
        let state_arc = STATE.get().unwrap();
        let mut s = state_arc.write().unwrap();

        // 1. 模式检查：仅处理模式 3 (持续自动通联)
        if s.status.auto_tx_mode != 3 {
            return;
        }

        // 2. 清洗并格式化消息
        let decoded_msg = decoded_msg_raw.trim().to_uppercase();
        if decoded_msg.is_empty() { return; }
        
        // 3. 正则匹配检查 (防止干扰和误触发)
        let is_match = if let Some(re) = &s.expect_regex_compiled {
            re.is_match(&decoded_msg)
        } else {
            // 如果正则还没设置或丢失，且当前正在发的是针对特定人的回复（非 CQ），
            // 则尝试从字节流恢复，如果还是空，则在 Mode 3 下允许对准包含我呼号的消息
            let current_re_str = String::from_utf8_lossy(&s.status.expect_regex).trim_matches(char::from(0)).to_string();
            if !current_re_str.is_empty() {
                if let Ok(re) = regex::Regex::new(&current_re_str) {
                    let m = re.is_match(&decoded_msg);
                    s.expect_regex_compiled = Some(re);
                    m
                } else { false }
            } else {
                decoded_msg.contains(config::MY_CALL)
            }
        };

        // 3.1 核心修复：多目标排队逻辑 (从备份版本找回)
        // 如果消息是发给我的，但不匹配当前的正则（说明是另外一个人在叫我），将其存入等待队列
        if !is_match && decoded_msg.contains(config::MY_CALL) {
            if let Some(target_call) = crate::ft8_qso::qso_utils::get_sender_call(&decoded_msg) {
                if !self.task_queue.iter().any(|(m, _, _, _)| m.contains(&target_call)) {
                    let next_tx = crate::ft8_qso::qso_utils::get_next_tx_msg(&decoded_msg, config::MY_CALL, config::MY_GRID, snr);
                    if !next_tx.is_empty() {
                        let tx_is_even = !((dt.round() as i32 % 30) == 0);
                        self.task_queue.push_back((next_tx, self.find_quiet_freq(tx_is_even), freq as i16, tx_is_even));
                        log_to_pc(&format!("⏳ 发现新请求，已加入任务队列: {}", target_call));
                    }
                }
            }
            return;
        }

        if !is_match { return; }

        // 4. 生成下一条消息
        let next_tx = crate::ft8_qso::qso_utils::get_next_tx_msg(&decoded_msg, config::MY_CALL, config::MY_GRID, snr);
        if next_tx.is_empty() {
             log_to_pc("🛑 当前通联已结束 (收到73)，清理缓存等待下一轮采样。");
             s.status.pending_msg = [0u8; 24]; // [关键修复]
             s.status.expect_regex = [0u8; 48];
             s.expect_regex_compiled = None;
             return;
        }

        log_to_pc(&format!("🎯 Mode 3 匹配成功 [%{}] -> 下一条: [{}]", decoded_msg, next_tx));
        
        // 5. 更新全局状态，触发发射
        let is_even = (dt.round() as i32 % 30) == 0;
        let tx_is_even = !is_even;
        let now = Instant::now();
        
        // 5.0 噪音时效性检查 (防止在无法监测底噪的情况下盲目发射导致干扰)
        let last_update = if tx_is_even { self.last_update_even } else { self.last_update_odd };
        if now.duration_since(last_update) > Duration::from_secs(120) {
            log_to_pc("⚠️ 活跃通联中断：底噪数据已过期 (>120s)，为避让繁忙频率已停止自动回复。");
            s.status.pending_msg = [0u8; 24];
            return;
        }

        let bytes = next_tx.as_bytes();
        let len = bytes.len().min(24);
        s.status.pending_msg = [0u8; 24];
        s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
        s.status.repeat_count = 0; // 重置重复计数
        self.report_any_reply(); // 重置连续失败计数
        
        // 5.1 频率与窗口同步策略：优先寻找寂静频率 (Split 模式)
        // pending_offset 为我们的发射频率 (Quiet), target_offset 为目标的频率 (用于重试落地方案)
        let quiet_f = self.find_quiet_freq(tx_is_even);
        s.status.pending_offset = quiet_f as u16;
        s.target_offset = freq as u16;
        s.status.tx_window_even = if tx_is_even { 1 } else { 0 };

        // 6. 更新下一波期望的正则
        let next_re_str = crate::ft8_qso::qso_utils::get_next_expect_regex(&next_tx, config::MY_CALL);
        if !next_re_str.is_empty() {
            let re_bytes = next_re_str.as_bytes();
            s.status.expect_regex = [0u8; 48];
            let re_len = re_bytes.len().min(48);
            s.status.expect_regex[..re_len].copy_from_slice(&re_bytes[..re_len]);
            s.expect_regex_compiled = regex::Regex::new(&next_re_str).ok();
        } else {
            s.status.expect_regex = [0u8; 48];
            s.expect_regex_compiled = None;
        }
    }
}
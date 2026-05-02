//! FT8 消息解包模块 (packjt77) - 增强版 (1:1 匹配 WSJTX)
//!
//! 将 77 个信息位解码为可读的 FT8 消息，包含呼号哈希缓存支持

use std::sync::Mutex;
use std::collections::HashMap;

/// FT8 消息类型
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum Ft8MessageType {
    FreeText,
    Standard,
    Dxpedition,
    #[allow(dead_code)]
    Contest,
    Telemetry,
    Unknown,
}

/// FT8 解码消息结构
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
#[allow(dead_code)]
pub struct Ft8Message {
    pub msg_type: Ft8MessageType,
    pub text: String,
    pub call_1: Option<String>,
    pub call_2: Option<String>,
    pub grid: Option<String>,
    pub snr: Option<i32>,
}

const CHARS_38: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
const CHARS_37: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const CHARS_36: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const CHARS_27: &[u8] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const CHARS_10: &[u8] = b"0123456789";

// --- 呼号哈希缓存 (模拟 WSJT-X 全局变量) ---

struct HashCache {
    calls10: HashMap<u32, String>,
    calls12: HashMap<u32, String>,
    calls22: HashMap<u32, String>,
}

static CACHE: Mutex<Option<HashCache>> = Mutex::new(None);

fn get_cache() -> std::sync::MutexGuard<'static, Option<HashCache>> {
    let mut cache = CACHE.lock().unwrap();
    if cache.is_none() {
        *cache = Some(HashCache {
            calls10: HashMap::new(),
            calls12: HashMap::new(),
            calls22: HashMap::new(),
        });
    }
    cache
}

/// 计算呼号哈希 (multiplicative hash, 匹配 WSJTX ihashcall)
pub fn ihashcall(call: &str, bits: u32) -> u32 {
    let mut n8: u64 = 0;
    let call_up = call.to_uppercase();
    let call_fixed = format!("{:<11}", call_up); // 补齐到 11 位
    for c in call_fixed.bytes().take(11) {
        let j = CHARS_38.iter().position(|&b| b == c).unwrap_or(0) as u64;
        n8 = 38 * n8 + j;
    }
    let multiplier: u64 = 8523930129890207281; // 0x764b154a9a5ae631
    let hash_val = n8.wrapping_mul(multiplier) >> (64 - bits);
    hash_val as u32
}

/// 保存呼号到缓存
pub fn save_hash_call(call: &str) {
    if call.is_empty() || call == "<...>" { return; }
    let call = call.trim_matches(|c| c == '<' || c == '>').trim();
    if call.len() < 3 { return; }

    // 保存原始全名
    save_single_call(call);

    // 如果是复合呼号 (如 R5AF/0)，尝试保存基准呼号 (R5AF)
    // 这样后续的 Suffix 模式编码才能通过 12-bit Hash 找回基准名字
    if call.contains('/') {
        let parts: Vec<&str> = call.split('/').collect();
        // 找出最像呼号的那部分 (通常是中间那个，或者最长的那个)
        let mut base = parts[0];
        for p in &parts {
            if p.len() > base.len() { base = p; }
        }
        if base.len() >= 3 && base != call {
            save_single_call(base);
        }
    }
}

fn save_single_call(call: &str) {
    let h10 = ihashcall(call, 10);
    let h12 = ihashcall(call, 12);
    let h22 = ihashcall(call, 22);

    let mut guard = get_cache();
    let cache = guard.as_mut().unwrap();
    
    if cache.calls22.len() < 50000 {
        cache.calls10.insert(h10, call.to_string());
        cache.calls12.insert(h12, call.to_string());
        cache.calls22.insert(h22, call.to_string());
    }
}

fn lookup_hash10(h10: u32) -> String {
    let guard = get_cache();
    guard.as_ref().unwrap().calls10.get(&h10).cloned().map(|c| format!("<{}>", c)).unwrap_or("<...>".to_string())
}

fn lookup_hash12(h12: u32) -> String {
    let guard = get_cache();
    guard.as_ref().unwrap().calls12.get(&h12).cloned().map(|c| format!("<{}>", c)).unwrap_or_else(|| format!("<...{}>", h12))
}

fn lookup_hash22(h22: u32) -> String {
    let guard = get_cache();
    guard.as_ref().unwrap().calls22.get(&h22).cloned().map(|c| format!("<{}>", c)).unwrap_or_else(|| format!("<...{}>", h22))
}

// --- 解包逻辑 ---

/// 将 28 比特整数解码为呼号
fn unpack28(n28: u32) -> (Option<String>, bool) {
    const NTOKENS: u32 = 2063592;
    const MAX22: u32 = 4194304;

    if n28 < NTOKENS {
        let res = match n28 {
            0 => Some("DE".to_string()),
            1 => Some("QRZ".to_string()),
            2 => Some("CQ".to_string()),
            3..=1002 => Some(format!("CQ {:03}", n28 - 3)),
            1003..=532443 => {
                let mut n = n28 - 1003;
                let i1 = n / (27 * 27 * 27); n %= 27 * 27 * 27;
                let i2 = n / (27 * 27); n %= 27 * 27;
                let i3 = n / 27; n %= 27;
                let i4 = n;
                let s = format!(
                    "{}{}{}{}",
                    CHARS_27[i1 as usize] as char,
                    CHARS_27[i2 as usize] as char,
                    CHARS_27[i3 as usize] as char,
                    CHARS_27[i4 as usize] as char
                );
                Some(format!("CQ {}", s.trim()))
            }
            532444..=2063591 => {
                // Callsign with suffix /R, /P, /0-9, /QRP
                let n = n28 - 532444;
                let isuffix = (n % 13) as usize;
                let h12 = n / 13;
                let base_call = lookup_hash12(h12);
                let suffix_str = match isuffix {
                    0 => "/R",
                    1 => "/P",
                    12 => "/QRP",
                    _ => { // 2..11 对应 /0..9
                        let digit = isuffix - 2;
                        return (Some(format!("{}/{}", base_call, digit)), true);
                    }
                };
                return (Some(format!("{}{}", base_call, suffix_str)), true);
            }
            _ => Some(format!("<...{}>", n28)), 
        };
        return (res, true);
    }

    let n28_rel = n28 - NTOKENS;
    if n28_rel < MAX22 {
        return (Some(lookup_hash22(n28_rel)), true);
    }

    // 标准呼号
    let mut n = n28_rel - MAX22;
    let i1 = n / (36 * 10 * 27 * 27 * 27); n %= 36 * 10 * 27 * 27 * 27;
    let i2 = n / (10 * 27 * 27 * 27); n %= 10 * 27 * 27 * 27;
    let i3 = n / (27 * 27 * 27); n %= 27 * 27 * 27;
    let i4 = n / (27 * 27); n %= 27 * 27;
    let i5 = n / 27; n %= 27;
    let i6 = n;

    let call = format!(
        "{}{}{}{}{}{}",
        CHARS_37[i1 as usize] as char,
        CHARS_36[i2 as usize] as char,
        CHARS_10[i3 as usize] as char,
        CHARS_27[i4 as usize] as char,
        CHARS_27[i5 as usize] as char,
        CHARS_27[i6 as usize] as char
    );

    let final_call = call.trim().to_string();
    
    // 校验呼号合法性 (chkcall 逻辑简述)
    let ok = is_valid_base_call(&final_call);
    
    // 自动存入缓存
    save_hash_call(&final_call);
    (Some(final_call), ok)
}

fn is_valid_base_call(bc: &str) -> bool {
    let bc = bc.trim();
    if bc.len() < 3 { return false; }
    if bc == "<...>" { return true; }
    
    // 基础过滤：必须包含至少一个字母和一个数字
    let has_alpha = bc.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = bc.chars().any(|c| c.is_ascii_digit());
    
    // 对于 Type 4 或非标准呼号，WSJTX 允许更多变体
    has_alpha && has_digit
}

/// 解包纯文本消息 (Base 42)
fn unpack_text77(bits: &[u8]) -> String {
    let mut val = 0u128;
    for i in 0..71 {
        if bits[i] != 0 { val |= 1 << (70 - i); }
    }
    let charset = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";
    let mut chars = Vec::new();
    for _ in 0..13 {
        let rem = (val % 42) as usize;
        val /= 42;
        chars.push(charset[rem] as char);
    }
    chars.reverse();
    chars.into_iter().collect::<String>().trim().to_string()
}

/// 主解包函数 - 将 77 个信息位转换为可读文本
pub fn unpack77(bits: &[u8], _nrx: bool) -> Option<Ft8Message> {
    if bits.len() < 77 { return None; }

    let n3 = read_bits(bits, 71, 3) as usize;
    let i3 = read_bits(bits, 74, 3) as usize;

    if i3 > 5 || (i3 == 0 && n3 > 6) { return None; }
    if i3 == 0 && n3 == 2 { return None; }

    let mut msg = Ft8Message {
        msg_type: Ft8MessageType::Unknown,
        text: String::new(),
        call_1: None,
        call_2: None,
        grid: None,
        snr: None,
    };

    let mut success = true;

    if i3 == 0 {
        if n3 == 0 {
            msg.msg_type = Ft8MessageType::FreeText;
            msg.text = unpack_text77(&bits[0..71]);
            if msg.text.is_empty() { success = false; }
            
            // Extract callsigns from free text and stash them for future hash resolutions
            for word in msg.text.split_whitespace() {
                save_hash_call(word);
            }
        } else if n3 == 1 {
            // Type 0.1: Dxpedition (Fox/Hound)
            msg.msg_type = Ft8MessageType::Dxpedition;
            let n28a = read_bits(bits, 0, 28);
            let n28b = read_bits(bits, 28, 28);
            let n10 = read_bits(bits, 56, 10);
            let n5 = read_bits(bits, 66, 5);
            let (call1_opt, ok1) = unpack28(n28a);
            let (call2_opt, ok2) = unpack28(n28b);
            if !ok1 || !ok2 || n28a <= 2 || n28b <= 2 { success = false; }
            
            let call1 = call1_opt.unwrap_or_default();
            let call2 = call2_opt.unwrap_or_default();
            let rpt = (2 * n5 as i32) - 30;
            let call3_hashed = lookup_hash10(n10);
            
            msg.call_1 = Some(call1.clone());
            msg.call_2 = Some(call2.clone());
            msg.snr = Some(rpt);
            msg.text = format!("{} RR73; {} {} {:+03}", call1, call2, call3_hashed, rpt);
        } else if n3 == 5 {
            msg.msg_type = Ft8MessageType::Telemetry;
            msg.text = format!("TELEMETRY: {:018X}", read_bits_u64(bits, 0, 71));
        } else {
            success = false;
        }
    } else if i3 == 1 || i3 == 2 {
        // Type 1 & 2: Standard Message
        msg.msg_type = Ft8MessageType::Standard;
        let n28a = read_bits(bits, 0, 28);
        let ipa = read_bits(bits, 28, 1);
        let n28b = read_bits(bits, 29, 28);
        let ipb = read_bits(bits, 57, 1);
        let ir = read_bits(bits, 58, 1);
        let igrid4 = read_bits(bits, 59, 15);
        
        let (call1_opt, ok1) = unpack28(n28a);
        let (call2_opt, ok2) = unpack28(n28b);
        if !ok1 || !ok2 { success = false; }
        
        let mut call1 = call1_opt.unwrap_or_default();
        let mut call2 = call2_opt.unwrap_or_default();
        
        if call1.starts_with("CQ") && ir == 1 { success = false; }
        
        if ipa == 1 { call1.push_str(if i3 == 1 { "/R" } else { "/P" }); }
        if ipb == 1 { call2.push_str(if i3 == 1 { "/R" } else { "/P" }); }
        
        msg.call_1 = Some(call1.clone());
        msg.call_2 = Some(call2.clone());

        let mut text = format!("{} {}", call1, call2);
        if igrid4 < 32400 {
            // 提取 4 位网格
            let mut n = igrid4;
            let j1 = n / (18 * 10 * 10); n %= 18 * 10 * 10;
            let j2 = n / (10 * 10); n %= 10 * 10;
            let j3 = n / 10; n %= 10;
            let grid = format!("{}{}{}{}", (b'A'+j1 as u8) as char, (b'A'+j2 as u8) as char, (b'0'+j3 as u8) as char, (b'0'+n as u8) as char);
            
            if grid == "RR73" || grid == "RRR" || grid == "73" {
                msg.grid = None; // 索引虽在网格范围内，但语义是指令，不存为网格
            } else {
                msg.grid = Some(grid.clone());
            }
            if ir == 1 { text.push_str(" R "); } else { text.push_str(" "); }
            text.push_str(&grid);
        } else {
            // 提取信号报告 (SNR)
            let irpt = igrid4 - 32400;
            if call1.starts_with("CQ") && irpt >= 2 { success = false; }
            match irpt {
                1 => {}, 
                2 => text.push_str(" RRR"), 
                3 => text.push_str(" RR73"), 
                4 => text.push_str(" 73"),
                _ => {
                    let mut isnr = irpt as i32 - 35;
                    if isnr > 50 { isnr -= 101; }
                    msg.snr = Some(isnr);
                    if ir == 1 { text.push_str(&format!(" R{:+03}", isnr)); } else { text.push_str(&format!(" {:+03}", isnr)); }
                }
            }
        }
        msg.text = text.trim().to_string();
    } else if i3 == 4 {
        // Type 4: Non-standard calls or hashed calls
        msg.msg_type = Ft8MessageType::Standard;
        let n12 = read_bits(bits, 0, 12);
        let n58 = read_bits_u64(bits, 12, 58);
        let iflip = read_bits(bits, 70, 1);
        let nrpt = read_bits(bits, 71, 2);
        let icq = read_bits(bits, 73, 1);
        
        let mut n = n58;
        let mut chars = Vec::new();
        for _ in 0..11 { chars.push(CHARS_38[(n % 38) as usize] as char); n /= 38; }
        chars.reverse();
        let c11 = chars.into_iter().collect::<String>().trim().to_string();
        if c11.is_empty() { success = false; }
        if success {
            save_hash_call(&c11);
        }
        let hash_call = lookup_hash12(n12);
        // iflip 控制呼号顺序
        let (c1, c2) = if iflip == 0 { (hash_call, c11) } else { (c11, hash_call) };
        
        msg.call_1 = Some(c1.clone());
        msg.call_2 = Some(c2.clone());

        if icq == 1 { 
            msg.text = format!("CQ {}", c2); 
        } else {
            let rpt_str = match nrpt { 1 => " RRR", 2 => " RR73", 3 => " 73", _ => "" };
            msg.text = format!("{} {}{}", c1, c2, rpt_str);
        }
    } else if i3 == 5 {
        // Type 5: EU VHF Contest
        let n12 = read_bits(bits, 0, 12);
        let n22 = read_bits(bits, 12, 22);
        let ir = read_bits(bits, 34, 1);
        let irpt = read_bits(bits, 35, 3);
        let iserial = read_bits(bits, 38, 11);
        // igrid6 占 25 bits
        let call1 = lookup_hash12(n12);
        let call2 = lookup_hash22(n22);
        
        msg.call_1 = Some(call1.clone());
        msg.call_2 = Some(call2.clone());
        msg.msg_type = Ft8MessageType::Contest;
        msg.text = format!("{} {} {}{:04} [GRID6]", call1, call2, if ir == 1 { "R " } else { "" }, 5200 + irpt * 1000 + iserial);
    } else {
        success = false;
    }

    if !success { return None; }

    Some(msg)
}

fn read_bits(bits: &[u8], start: usize, len: usize) -> u32 {
    let mut val = 0u32;
    for i in 0..len {
        if start + i < bits.len() && bits[start + i] != 0 {
            val |= 1 << (len - 1 - i);
        }
    }
    val
}

fn read_bits_u64(bits: &[u8], start: usize, len: usize) -> u64 {
    let mut val = 0u64;
    for i in 0..len {
        if start + i < bits.len() && bits[start + i] != 0 {
            val |= 1 << (len - 1 - i);
        }
    }
    val
}
use regex::Regex;
use crate::utils::log_to_pc;
use crate::types::STATE;
use crate::ft8_qso::qso_utils::{get_next_tx_msg, get_next_expect_regex};

use crate::config;

/// Mode 2 自动答复驱动函数
/// 当用户在 Web 端手动选定一个通联目标并开启"自动答复"时，
/// 每次解码到新消息，由此函数自动决定下一条应发的内容并写入 pending_msg。
pub fn handle_auto_reply_logic(decoded_msg_raw: &str, snr: i32, freq: f32, dt: f32) {
    let state_arc = STATE.get().unwrap();
    let mut s = state_arc.write().unwrap();

    // 1. 模式检查：非自动答复模式则跳过
    if s.status.auto_tx_mode != 2 {
        return; 
    }

    // 2. 彻底清洗输入的解码消息
    let decoded_msg = decoded_msg_raw
        .trim_matches(char::from(0))
        .trim()
        .to_uppercase();

    if decoded_msg.is_empty() {
        return;
    }

    // 3. 检查当前消息是否匹配期待的正则表达式 (QSO状态机核心)
    let is_match = if let Some(re) = &s.expect_regex_compiled {
        re.is_match(&decoded_msg)
    } else {
        // 异常恢复：如果已编译正则丢失，尝试从字节数组恢复并匹配
        let current_re_str = String::from_utf8_lossy(&s.status.expect_regex)
            .trim_matches(char::from(0))
            .to_string();
            
        if !current_re_str.is_empty() {
            if let Ok(re) = Regex::new(&current_re_str) {
                let matched = re.is_match(&decoded_msg);
                s.expect_regex_compiled = Some(re);
                log_to_pc(&format!("♻️ 修复：重新编译了丢失的正则: [{}]", current_re_str));
                matched
            } else { false }
        } else {
            // 兜底：如果正则确实为空 (例如首次刚开启模式2)，
            // 检查消息是否包含我的呼号，作为最小必要检查。
            let fallback_match = decoded_msg.contains(config::MY_CALL);
            if fallback_match {
                log_to_pc("⚠️ 模式2正则为空，执行呼号包含兜底匹配。");
            }
            fallback_match
        }
    };

    // 打印匹配结果日志
    let current_re_str = String::from_utf8_lossy(&s.status.expect_regex).trim_matches(char::from(0)).to_string();
    println!("🔍 [匹配尝试] 模式: 2 | 消息: [{}] | 正则: [{}] | 结果: {}", 
        decoded_msg, current_re_str, if is_match { "✅ 成功" } else { "❌ 失败" });

    // 如果不匹配，直接丢弃，不执行后续逻辑
    if !is_match { return; }

    // 4. 匹配成功，生成下一阶段消息
    let next_tx = get_next_tx_msg(&decoded_msg, config::MY_CALL, config::MY_GRID, snr);
    
    if !next_tx.is_empty() {
        log_to_pc(&format!("🎯 触发状态迁徙 -> 下一条消息: [{}]", next_tx));

        // 更新即将发射的消息
        let bytes = next_tx.as_bytes();
        let len = bytes.len().min(24);
        s.status.pending_msg = [0u8; 24];
        s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
        s.status.repeat_count = 0; // 重置重复计数
        
        // 5.1 同步频率与窗口 (Mode 2 同样需要追踪目标位置)
        s.status.pending_offset = freq as u16;
        s.status.tx_window_even = if (dt.round() as i32 % 30) == 0 { 0 } else { 1 };

        // 5. 更新下一条期待正则
        let next_re_str = get_next_expect_regex(&next_tx, config::MY_CALL);
        if !next_re_str.is_empty() {
            let re_bytes = next_re_str.as_bytes();
            s.status.expect_regex = [0u8; 48];
            let re_len = re_bytes.len().min(48);
            s.status.expect_regex[..re_len].copy_from_slice(&re_bytes[..re_len]);
            
            match Regex::new(&next_re_str) {
                Ok(compiled_re) => {
                    s.expect_regex_compiled = Some(compiled_re);
                    log_to_pc(&format!("🛰️ 正则更新: [{}]", next_re_str));
                },
                Err(e) => {
                    s.expect_regex_compiled = None;
                    log_to_pc(&format!("⚠️ 新正则编译失败: {} | 表达式: [{}]", e, next_re_str));
                }
            }
        } else {
            // 通联结束 (例如发了 73)，清空正则
            s.status.expect_regex = [0u8; 48];
            s.expect_regex_compiled = None;
            log_to_pc("🛑 流程结束：无下一条期待正则 (可能发的是73)");
        }
    } else {
        log_to_pc(&format!("🛑 通联顺利结束 (收到73)，已回到空闲状态。"));
        s.status.pending_msg = [0u8; 24]; // [关键核心修复] 必须清空缓存以停止无线电发射
        s.status.auto_tx_mode = 0;       // 单次通联结束后切回手动模式
        s.status.expect_regex = [0u8; 48];
        s.expect_regex_compiled = None;
    }
}
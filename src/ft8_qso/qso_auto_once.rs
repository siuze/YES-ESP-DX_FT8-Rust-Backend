use regex::Regex;
use crate::types::STATE;
use crate::ft8_qso::qso_utils::{get_next_tx_msg, get_next_expect_regex};

use crate::config;

/// Mode 2 自动答复驱动函数
/// 当用户在 Web 端手动选定一个通联目标并开启"自动答复"时，
/// 每次解码到新消息，由此函数自动决定下一条应发的内容并写入 pending_msg。
pub fn handle_auto_reply_logic(decoded_msg_raw: &str, snr: i32) {
    let mode = STATE.get().unwrap().read().unwrap().status.auto_tx_mode;
    if mode != 2 { return; } // 非自动答复模式则跳过

    let next_msg = get_next_tx_msg(decoded_msg_raw, config::MY_CALL, config::MY_GRID, snr);
    if !next_msg.is_empty() {
        let mut s = STATE.get().unwrap().write().unwrap();
        let bytes = next_msg.as_bytes();
        let len = bytes.len().min(24);
        s.status.pending_msg = [0u8; 24];
        s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
        s.status.repeat_count = 0; // 重置重复计数，开始发射新阶段

        // 同步更新期望的正则表达式
        let next_re_str = get_next_expect_regex(&next_msg, config::MY_CALL);
        if !next_re_str.is_empty() {
            let re_bytes = next_re_str.as_bytes();
            s.status.expect_regex = [0u8; 48];
            let re_len = re_bytes.len().min(48);
            s.status.expect_regex[..re_len].copy_from_slice(&re_bytes[..re_len]);
            match Regex::new(&next_re_str) {
                Ok(re) => s.expect_regex_compiled = Some(re),
                Err(_) => s.expect_regex_compiled = None,
            }
        } else {
            s.status.expect_regex = [0u8; 48];
            s.expect_regex_compiled = None;
        }
    }
}

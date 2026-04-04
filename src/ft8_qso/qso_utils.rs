use regex::Regex;

/// 根据收到的消息内容，决定下一条应该发射的 FT8 消息文本
/// 遵循 FT8 标准通联流程：Grid -> SNR -> R+SNR -> RR73/RRR -> 73
pub fn get_next_tx_msg(incoming: &str, my_call: &str, my_grid: &str, snr: i32) -> String {
    let parts: Vec<&str> = incoming.split_whitespace().collect();
    if parts.len() < 3 {
        return String::new();
    }

    let to = parts[0].replace(['>', '<'], "");
    let mut caller = parts[1].replace(['>', '<'], "");
    let mut p = if parts.len() > 2 { parts[2] } else { "" };

    // 处理 CQ 消息格式 (CQ [DX] CALL GRID)
    if to == "CQ" && parts.len() >= 4 {
        caller = parts[2].replace(['>', '<'], "");
        p = parts[3];
    } else if to == "CQ" && parts.len() == 3 {
        caller = parts[1].replace(['>', '<'], "");
        p = parts[2];
    }

    // 基础合法性校验
    if (to.len() < 3 && to != "CQ") || caller.len() < 3 || caller == "..." || to == "..." {
        return String::new();
    }

    // 格式化 SNR 报告
    let snr_str = if snr >= 0 {
        format!("+{:02}", snr)
    } else {
        format!("{:03}", snr)
    };

    // 场景 A: 消息是发给我的
    if to == my_call {
        if is_grid(p) {
            // 对方发来网格 -> 回复我的 SNR
            return format!("{} {} {}", caller, my_call, snr_str);
        }
        if p.starts_with('R') && is_snr(p) {
            // 对方收到我的 SNR 并回传 R+SNR -> 回复 RR73 结束通联
            return format!("{} {} RR73", caller, my_call);
        }
        if is_snr(p) {
            // 对方直接发来 SNR (未带 R) -> 回复 R+SNR
            return format!("{} {} R{}", caller, my_call, snr_str);
        }
        if p == "RR73" || p == "RRR" {
            // 对方已确认收到报告 -> 回复 73
            return format!("{} {} 73", caller, my_call);
        }
        if p == "73" {
            return String::new(); // 通联已彻底结束
        }
    }
    // 场景 B: 对方在叫 CQ
    else if to == "CQ" {
        // 我主动呼叫对方 -> 发送我的呼号和网格
        return format!("{} {} {}", caller, my_call, my_grid);
    }

    String::new()
}

/// 根据即将发射的消息，预测并生成下一轮期望收到的正则表达式
/// 用于在"自动答复"模式下过滤出正确的回应包
pub fn get_next_expect_regex(pending: &str, my_call: &str) -> String {
    let parts: Vec<&str> = pending.split_whitespace().collect();
    if parts.len() < 3 {
        return String::new();
    }

    let target = parts[0].replace(['>', '<'], "");
    if target.len() < 3 && target != "CQ" {
        return String::new();
    }
    let p = parts[2];

    // 如果我正在发 CQ -> 期望收到任何发给我的消息 (带网格或 SNR)
    if target == "CQ" {
        return format!(
            r"^<?{}>?\s+<?[A-Z0-9/]+>?\s+([A-Z][0-9A-Z][0-9]{{2}}|[+-]\d{{1,2}})",
            my_call
        );
    }

    // 通用回复匹配逻辑
    if p == "RR73" || p == "RRR" {
        format!(r"^<?{}>?\s+<?{}>?\s+(RR73|RRR|73)", my_call, target)
    } else if is_grid(p) {
        format!(r"^<?{}>?\s+<?{}>?\s+R?[+-]\d{{1,2}}", my_call, target)
    } else if is_snr(p) {
        if p.starts_with('R') {
            format!(r"^<?{}>?\s+<?{}>?\s+(RR73|RRR|73)", my_call, target)
        } else {
            format!(r"^<?{}>?\s+<?{}>?\s+R?[+-]\d{{1,2}}", my_call, target)
        }
    } else {
        String::new()
    }
}

/// 匹配 FT8 标准 SNR 格式 (+02, -15, R-05 等)
pub fn is_snr(s: &str) -> bool {
    let re_snr = Regex::new(r"^R?[+-]\d{1,2}$").unwrap();
    re_snr.is_match(s)
}

/// 匹配 FT8 标准 4 位网格格式 (OL96, PM95 等)
pub fn is_grid(s: &str) -> bool {
    if s == "RR73" || s == "RRR" || s == "73" {
        return false;
    }
    if s.len() != 4 {
        return false;
    }
    let b = s.as_bytes();
    b[0].is_ascii_alphabetic() && b[1].is_ascii_alphabetic() &&
    b[2].is_ascii_digit() && b[3].is_ascii_digit()
}

/// 提取消息中的发送方呼号
pub fn get_sender_call(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() < 2 { return None; }
    
    let to = parts[0].replace(['>', '<'], "");
    if to == "CQ" {
        if parts.len() >= 3 {
             let caller = parts[2].replace(['>', '<'], "");
             if caller.len() >= 3 { return Some(caller); }
             let caller2 = parts[1].replace(['>', '<'], "");
             if caller2.len() >= 3 { return Some(caller2); }
        }
    } else {
        let caller = parts[1].replace(['>', '<'], "");
        if caller.len() >= 3 { return Some(caller); }
    }
    None
}

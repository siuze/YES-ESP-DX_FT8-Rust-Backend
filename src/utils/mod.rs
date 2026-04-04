use crate::types::GLOBAL_TX;

pub fn decode_24le(b: &[u8]) -> i32 {
    let mut v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
    if v & 0x800000 != 0 {
        v |= !0xffffff;
    }
    v
}

pub fn log_to_pc(msg: &str) {
    println!("{}", msg);
    let mut pkt = Vec::with_capacity(msg.len() + 1);
    pkt.push(0x12);
    pkt.extend_from_slice(msg.as_bytes());
    if let Some(tx) = GLOBAL_TX.get() {
        let _ = tx.send(pkt);
    }
}

/// 将特定的通联活动记录到本地文件 qso_activity.log
/// 格式: [YYYY-MM-DD HH:MM:SS] [TX/RX] Message...
pub fn log_qso_activity(is_tx: bool, text: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    use chrono::Local;

    let time_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let prefix = if is_tx { "TX" } else { "RX" };
    let line = format!("[{}] [{}] {}\n", time_str, prefix, text);

    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("qso_activity.log") 
    {
        let _ = file.write_all(line.as_bytes());
    }
}

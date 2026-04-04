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

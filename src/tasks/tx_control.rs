use tokio::time::{interval, Duration};
use std::sync::{Arc, RwLock};
use chrono::{Utc, Timelike};

use crate::types::{AppState, STATE, AUTO_MGR};
use crate::utils::log_to_pc;

/// 任务 F1: 发射链路定时检查
/// 每隔一定频率检查是否进入了预先设定的发射窗口 (0/15/30/45s)。
pub fn spawn_tx_check_task(state: Arc<RwLock<AppState>>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(100));
        loop {
            ticker.tick().await;
            let now = Utc::now();
            let sec = now.second();

            // 仅在窗口起始的 1 秒内尝试触发发射，防止重复发送
            if (sec == 0 || sec == 15 || sec == 30 || sec == 45) && now.timestamp_subsec_millis() < 500 {
                let mut should_sleep = false;

                {
                    let mut s = state.write().unwrap();
                    
                    // 1. 获取发射参数
                    let is_even_win = sec == 0 || sec == 30;
                    let (pending_msg, mode, max_rep) = {
                        let msg = String::from_utf8_lossy(&s.status.pending_msg).trim_matches(char::from(0)).to_string();
                        (msg, s.status.auto_tx_mode, s.status.max_repeats)
                    };

                    // 判断是否满足发射条件：处于自动发射模式，且窗口匹配
                    if mode > 0 && !pending_msg.is_empty() && (s.status.tx_window_even == 1) == is_even_win {
                        // 2. 确定发射 IP
                        if let Some(target_ip) = s.radio_ip.clone() {
                            // 3. 确定最终频偏 (Homing 策略)
                            let use_f = if mode == 3 && s.status.repeat_count >= 2 && !pending_msg.starts_with("CQ ") {
                                s.target_offset
                            } else {
                                s.status.pending_offset
                            };

                            // 4. 下发发射指令
                            match crate::radio_ctrl::send_ft8_transmit(&target_ip, &pending_msg, use_f) {
                                Ok(_) => {
                                    log_to_pc(&format!("🚀 [发射指令已下发] Msg: {} | Freq: {} Hz | IP: {}", pending_msg, use_f, target_ip));
                                    
                                    // 更新统计
                                    s.status.tx_count += 1;
                                    s.status.repeat_count += 1;

                                    // 如果是单次模式 (Mode 1 或 2)，判定是否需要结束
                                    if mode == 1 || (mode == 2 && (s.status.repeat_count >= max_rep || pending_msg.contains(" 73"))) {
                                        if pending_msg.contains(" 73") || s.status.repeat_count >= max_rep {
                                            s.status.auto_tx_mode = 0;
                                            s.status.pending_msg = [0u8; 24]; // 清空
                                            log_to_pc("🏁 单次任务已完成，回复空闲模式。");
                                        }
                                    }
                                }
                                Err(e) => {
                                    log_to_pc(&format!("❌ [发射失败] UDP 写入错误: {}", e));
                                }
                            }
                        } else {
                            log_to_pc("⚠️ [发射阻塞] 未发现电台 IP，请检查连接或等待自动发现。");
                        }
                        should_sleep = true;
                    }
                }

                if should_sleep {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                }
            }
        }
    });
}

/// 任务 F2: 自动通联管理器状态机心跳 (Mode 3)
/// 每 100ms 检查一次当前通联进度，决定是否需要切换消息、转入重试或开启下一轮。
pub fn spawn_auto_qso_timer_task() {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(100));
        loop {
            ticker.tick().await;
            let (mode, is_idle, repeat_count, current_msg) = {
                let s = STATE.get().unwrap().read().unwrap();
                let msg = String::from_utf8_lossy(&s.status.pending_msg).trim_matches(char::from(0)).to_string();
                (s.status.auto_tx_mode, s.status.pending_msg[0] == 0, s.status.repeat_count, msg)
            };
            if mode != 3 { continue; }

            let now = Utc::now();
            let sec = now.second();
            let ms = now.timestamp_subsec_millis();
            let mut should_sleep = false;

            {
                let mut mgr_locked = AUTO_MGR.get().unwrap().lock().unwrap();
                let mgr = &mut *mgr_locked;
                
                let is_73 = current_msg.contains(" 73") || current_msg.contains(" RR73");
                let is_cq = current_msg.starts_with("CQ ");
                let is_chase = !is_73 && !is_cq;
                
                // 判定当前重复次数是否达到上限，或者是否有其他更高优先级的回复排队中
                let has_others = !mgr.task_queue.is_empty();
                let limit_reached = if is_73 { repeat_count >= 1 } 
                                   else if has_others && repeat_count >= 2 { true } // 若有新任务排队且当前已重复2次，优先切走
                                   else if is_chase { repeat_count >= 3 } 
                                   else { repeat_count >= 4 }; // CQ 重复 4 次

                // 核心逻辑 A: 判定当前任务是否结束 (超时/达成/手动清空) 并尝试拉取新任务
                if (!is_idle && limit_reached) || (is_idle && !mgr.task_queue.is_empty()) {
                    if !is_idle && limit_reached && is_chase { mgr.report_failure(); }
                    
                    let state_arc = STATE.get().unwrap();
                    let mut s = state_arc.write().unwrap();
                    
                    // 只要队列中有任务，就必须优先清空队列
                    if let Some((next_m, next_f, target_f, next_e)) = mgr.task_queue.pop_front() {
                        let bytes = next_m.as_bytes();
                        s.status.pending_msg = [0u8; 24];
                        let len = bytes.len().min(24);
                        s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
                        s.status.pending_offset = next_f as u16;
                        s.target_offset = target_f as u16;
                        s.status.tx_window_even = if next_e { 1 } else { 0 };
                        s.status.repeat_count = 0;
                        log_to_pc(&format!("⏭️ 自动切换任务队列: {}", next_m));
                    } else {
                        s.status.pending_msg = [0u8; 24];
                        if mgr.consecutive_failures >= 2 {
                            log_to_pc("⏭️ 队列已清空，检测到连续失败，准备开启紧急 CQ 或策略刷新");
                        } else {
                            log_to_pc("⏭️ 队列已清空，任务完成回到空闲状态");
                        }
                    }
                }

                // 核心逻辑 B: 在每 15 秒周期的结尾 (14, 29, 44, 59s) 检查是否需要发起新的自动策略
                if (sec == 14 || sec == 29 || sec == 44 || sec == 59) && ms >= 900 {
                    let current_is_idle = STATE.get().unwrap().read().unwrap().status.pending_msg[0] == 0;
                    
                    let mut triggered = false;

                    // 优先级 1: 只有在队列为空的情况下，尝试发起主动 CQ
                    if mgr.task_queue.is_empty() {
                        if let Some((msg, f, e)) = mgr.check_auto_cq(current_is_idle) {
                            let mut s = STATE.get().unwrap().write().unwrap();
                            let bytes = msg.as_bytes();
                            s.status.pending_msg = [0u8; 24];
                            let len = bytes.len().min(24);
                            s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
                            s.status.pending_offset = f as u16;
                            s.target_offset = f as u16;
                            s.status.tx_window_even = if e { 1 } else { 0 };
                            s.status.repeat_count = 0;
                            log_to_pc(&format!("🎯 策略触发 (CQ): {}", msg));
                            triggered = true;
                        }
                    } 
                    
                    // 优先级 2: 如果当前空闲且没有触发 CQ，则检查是否有优质的追逐目标 (Chase)
                    if !triggered {
                        if let Some((msg, f, target_f, e)) = mgr.check_auto_chase(current_is_idle) {
                            let mut s = STATE.get().unwrap().write().unwrap();
                            let bytes = msg.as_bytes();
                            s.status.pending_msg = [0u8; 24];
                            let len = bytes.len().min(24);
                            s.status.pending_msg[..len].copy_from_slice(&bytes[..len]);
                            s.status.pending_offset = f as u16;
                            s.target_offset = target_f as u16;
                            s.status.tx_window_even = if e { 1 } else { 0 };
                            s.status.repeat_count = 0;
                            log_to_pc(&format!("🎯 策略触发 (Chase): {}", msg));
                        }
                    }
                    should_sleep = true;
                }
            }
            if should_sleep { tokio::time::sleep(Duration::from_millis(150)).await; }
        }
    });
}

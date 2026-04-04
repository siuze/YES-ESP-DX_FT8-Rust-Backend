use super::ldpc_constants::*;
use super::ldpc_gen::GEN_HEX;

#[allow(dead_code)]
pub struct DecodeResult {
    pub success: bool,
    pub message_bits: Vec<u8>, // 91 bits (77 message + 14 CRC)
    pub codeword: Vec<u8>,     // 174 bits
    pub nharderror: i32,
    pub iter: usize,
    pub ntype: i32,            // 1: BP, 2: OSD, 0: Fail
    pub dmin: f32,
    pub ncheck: usize,
}

/// 模拟 WSJTX 的 platanh
fn platanh(x: f32) -> f32 {
    let mut isign = 1.0f32;
    let mut z = x;
    if x < 0.0 {
        isign = -1.0;
        z = x.abs();
    }

    if z <= 0.664 {
        x / 0.83
    } else if z <= 0.9217 {
        isign * (z - 0.4064) / 0.322
    } else if z <= 0.9951 {
        isign * (z - 0.8378) / 0.0524
    } else if z <= 0.9998 {
        isign * (z - 0.9914) / 0.0012
    } else {
        isign * 7.0
    }
}
pub fn check_crc14_wsjtx(mc: &[u8]) -> bool {
    let len = mc.len();
    let mut r = vec![0u8; 15];
    let p = [1, 1, 0, 0, 1, 1, 1, 0, 1, 0, 1, 0, 1, 1, 1];

    // 初始化 r = mc(1:15)
    for i in 0..15 {
        r[i] = mc[i];
    }

    // 执行位移除法
    for i in 0..=(len - 15) {
        if i > 0 {
            r[14] = mc[i + 14];
        }
        let r1 = r[0];
        for j in 0..15 {
            r[j] = (r[j] + r1 * p[j]) % 2;
        }
        // 执行 cshift(r, 1) -> 循环左移
        r.rotate_left(1);
    }

    // 检查余数前 14 位是否全为 0
    for i in 0..14 {
        if r[i] != 0 { return false; }
    }
    true
}
fn check_crc_ft8(cw: &[u8]) -> bool {
    let mut m96 = [0u8; 96];
    // m96(1:77) = cw(1:77)
    m96[0..77].copy_from_slice(&cw[0..77]);
    // m96(83:96) = cw(78:91)
    m96[82..96].copy_from_slice(&cw[77..91]);
    
    check_crc14_wsjtx(&m96)
}
pub fn decode_174_91(
    llr: &[f32],
    apmask: &[u8],
    maxosd: i32,
    norder: i32,
) -> DecodeResult {
    let mut tov = [[0.0f32; N]; 3];
    let mut toc = [[0.0f32; M]; 7];
    let mut zn = [0.0f32; N];
    let mut zsum = [0.0f32; N];
    let mut zsave = vec![[0.0f32; N]; 4]; // 对应 Fortran 的 zsave(N,3) + 1个buffer
    let mut cw = [0u8; N];
    
    // let mut nosd = 0;
    let actual_maxosd = maxosd.min(3);
    let max_iterations: usize = 30;

    if actual_maxosd == 0 {
        // nosd = 1;
        zsave[1].copy_from_slice(llr);
    } else if actual_maxosd > 0 {
        // nosd = 3; // 尝试 iteration 1, 5, 10
    }

    // 初始化 toc
    for j in 0..M {
        for i in 0..NRW[j] {
            toc[i][j] = llr[NM[i][j]];
        }
    }

    let mut ncnt = 0;
    let mut nclast = 0;

    // --- BP 迭代循环 ---
    for iter in 0..=max_iterations {
        // 更新 zn
        for i in 0..N {
            if apmask[i] != 1 {
                zn[i] = llr[i] + tov[0][i] + tov[1][i] + tov[2][i];
            } else {
                zn[i] = llr[i];
            }
        }

        // 累加 zsum 并保存位用于 OSD (与 WSJTX 1:1 对齐)
        for i in 0..N { zsum[i] += zn[i]; }
        if actual_maxosd >= 1 && iter > 0 && iter <= actual_maxosd as usize {
            zsave[iter].copy_from_slice(&zsum);
        }

        // 硬判决
        for i in 0..N { cw[i] = if zn[i] > 0.0 { 1 } else { 0 }; }

        // 计算 ncheck
        let mut ncheck = 0;
        for j in 0..M {
            let mut s = 0;
            for i in 0..NRW[j] { s += cw[NM[i][j]]; }
            if s % 2 != 0 { ncheck += 1; }
        }

        // 打印调试信息（与 Fortran 对齐）
        // println!("DEBUG_BP: iter={:>3} ncheck={:>3} zn_sum={:10.2}", iter, ncheck, zn.iter().sum::<f32>());
        // if iter == 0 {
        //     print!("DEBUG_ZN_START: ");
        //     for i in 0..5 { print!("{:10.2}", zn[i]); }
        //     println!();
        // }
        // 检查收敛
        if ncheck == 0 {
            if check_crc14_96(&cw) {
                let nharderror = count_hard_errors(llr, &cw);
                let dmin = calculate_dmin(llr, &cw);
                return DecodeResult {
                    success: true,
                    message_bits: cw[0..91].to_vec(),
                    codeword: cw.to_vec(),
                    nharderror,
                    iter,
                    ntype: 1,
                    dmin,
                    ncheck: 0,
                };
            }
        }

        // 早停逻辑
        if iter > 0 {
            let nd = ncheck as i32 - nclast as i32;
            if nd < 0 { ncnt = 0; } else { ncnt += 1; }
            if ncnt >= 5 && iter >= 10 && ncheck > 15 {
                break; // Exit BP
            }
        }
        nclast = ncheck;

        // 消息传递: Bits to Checks
        for j in 0..M {
            for i in 0..NRW[j] {
                let ibj = NM[i][j];
                let mut val = zn[ibj];
                for kk in 0..3 {
                    if MN[kk][ibj] == j {
                        val -= tov[kk][ibj];
                        break;
                    }
                }
                toc[i][j] = val;
            }
        }

        // 消息传递: Checks to Bits
        let mut tanhtoc = [[0.0f32; M]; 7];
        for j in 0..M {
            for i in 0..7 {
                tanhtoc[i][j] = (-toc[i][j] / 2.0).tanh();
            }
        }
        for j in 0..N {
            for i in 0..3 {
                let ichk = MN[i][j];
                let mut prod = 1.0f32;
                for k in 0..NRW[ichk] {
                    if NM[k][ichk] != j { prod *= tanhtoc[k][ichk]; }
                }
                tov[i][j] = 2.0 * platanh(-prod);
            }
        }
    }

    // --- OSD 尝试循环 ---
    // WSJTX: if(maxosd.ge.1) call osd174_91(zn,apmask,maxosd,norder,cw,ncheck,ntype)
    if actual_maxosd >= 1 {
        // nosd = actual_maxosd
        for i in 1..=actual_maxosd as usize {
            let res = osd174_91(&zsave[i], apmask, norder as usize);
            if res.success {
                let nharderror = count_hard_errors(llr, &res.codeword);
                let dmin = calculate_dmin(llr, &res.codeword);
                return DecodeResult {
                    success: true,
                    message_bits: res.codeword[0..91].to_vec(),
                    codeword: res.codeword.to_vec(),
                    nharderror,
                    iter: 0,
                    ntype: 2, // OSD
                    dmin,
                    ncheck: 0,
                };
            }
        }
    }

    DecodeResult {
        success: false,
        message_bits: vec![],
        codeword: vec![],
        nharderror: -1,
        iter: max_iterations,
        ntype: 0,
        dmin: 0.0,
        ncheck: nclast,
    }
}
/// 使用与 get_crc14.f90 完全一致的位移/异或逻辑
fn check_crc14_96(cw: &[u8]) -> bool {
    let mut m96 = vec![0u8; 96];
    m96[0..77].copy_from_slice(&cw[0..77]);
    m96[82..96].copy_from_slice(&cw[77..91]);
    
    let mut r = [0u8; 15];
    let p = [1, 1, 0, 0, 1, 1, 1, 0, 1, 0, 1, 0, 1, 1, 1];
    
    // 初始化 r = mc(1:15)
    for i in 0..15 { r[i] = m96[i]; }
    
    // 执行位移除法 (与 get_crc14.f90 1:1)
    for i in 0..=81 { // len-15 = 96-15 = 81
        r[14] = m96[i + 14]; // Index 15 to 96 (in 1-indexed Fortran)
        let r1 = r[0];
        if r1 != 0 {
            for j in 0..15 { r[j] = (r[j] + p[j]) % 2; }
        }
        r.rotate_left(1);
    }
    
    // 检查 r(1:14) 是否全为 0
    for i in 0..14 { if r[i] != 0 { return false; } }
    true
}


fn count_hard_errors(llr: &[f32], cw: &[u8]) -> i32 {
    let mut count = 0;
    for i in 0..174 {
        let bit_llr = if llr[i] >= 0.0 { 1 } else { 0 };
        if cw[i] != bit_llr { count += 1; }
    }
    count
}

fn calculate_dmin(llr: &[f32], cw: &[u8]) -> f32 {
    let mut dmin = 0.0;
    for i in 0..174 {
        let bit_llr = if llr[i] >= 0.0 { 1 } else { 0 };
        if bit_llr != cw[i] {
            dmin += llr[i].abs();
        }
    }
    dmin
}

struct OsdResult {
    success: bool,
    codeword: [u8; 174],
}

/// OSD (Ordered Statistics Decoding) 174_91 实现
fn osd174_91(zn: &[f32; 174], _apmask: &[u8], norder: usize) -> OsdResult {
    const K: usize = 91;
    const N: usize = 174;
    
    // 1. 根据可靠性排序 (|zn|)
    let mut reliability: Vec<(usize, f32)> = (0..N).map(|i| (i, zn[i].abs())).collect();
    // 降序排序
    reliability.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    
    let mut indices = [0usize; N];
    for i in 0..N { indices[i] = reliability[i].0; }
    
    // 2. 构造重新排序后的生成矩阵并进行高斯消元
    // 原始 G = [I_91 | GenHex^T]
    let mut g_work = [[0u8; N]; K];
    for i in 0..K {
        // 前 91 列是单位矩阵
        g_work[i][i] = 1;
        // 后 83 列是 GenHex
        for j in 0..83 {
            g_work[i][K + j] = GEN_HEX[i][j];
        }
    }
    
    // 按索引重新排列列
    let mut g_mrb = [[0u8; N]; K];
    for j in 0..N {
        let col_idx = indices[j];
        for i in 0..K {
            g_mrb[i][j] = if col_idx < K {
                if col_idx == i { 1 } else { 0 }
            } else {
                GEN_HEX[i][col_idx - K]
            };
        }
    }
    
    // 3. 高斯消元得到 systematic MRB 形式 [I_K | P]
    let mut pivot_row = 0;
    let mut basis_indices = [0usize; K];
    let mut col_map = [0usize; N];
    for j in 0..N { col_map[j] = j; }

    for j in 0..N {
        if pivot_row >= K { break; }
        
        // 寻找主元
        let mut found = false;
        for r in pivot_row..K {
            if g_mrb[r][j] == 1 {
                g_mrb.swap(pivot_row, r);
                found = true;
                break;
            }
        }
        
        if found {
            // 消去其他行的第 j 列
            for r in 0..K {
                if r != pivot_row && g_mrb[r][j] == 1 {
                    for c in j..N {
                        g_mrb[r][c] ^= g_mrb[pivot_row][c];
                    }
                }
            }
            basis_indices[pivot_row] = j;
            pivot_row += 1;
        }
    }
    
    if pivot_row < K {
        return OsdResult { success: false, codeword: [0; N] };
    }

    // 4. Order-0 编码
    let mut m0 = [0u8; K];
    for i in 0..K {
        m0[i] = if zn[indices[basis_indices[i]]] > 0.0 { 1 } else { 0 };
    }
    
    let mut best_cw = [0u8; N];
    let mut min_dist = f32::MAX;
    let mut found_any = false;

    // 尝试 Order-0, Order-1 和 Order-2 (比特翻转)
    // 我们手动展开循环以支持 Order-0, 1, 2
    
    // Order-0
    check_and_update(m0, &g_mrb, &indices, zn, &mut found_any, &mut min_dist, &mut best_cw);
    
    if found_any && norder == 0 { return OsdResult { success: true, codeword: best_cw }; }

    // Order-1
    if norder >= 1 {
        for i in 0..K {
            let mut m = m0;
            m[i] ^= 1;
            check_and_update(m, &g_mrb, &indices, zn, &mut found_any, &mut min_dist, &mut best_cw);
        }
    }
    
    // Order-2
    if norder >= 2 {
        for i in 0..K {
            for j in (i + 1)..K {
                let mut m = m0;
                m[i] ^= 1;
                m[j] ^= 1;
                check_and_update(m, &g_mrb, &indices, zn, &mut found_any, &mut min_dist, &mut best_cw);
            }
        }
    }

    OsdResult { success: found_any, codeword: best_cw }
}

fn check_and_update(
    m: [u8; 91], 
    g_mrb: &[[u8; 174]; 91], 
    indices: &[usize; 174], 
    zn: &[f32; 174],
    found_any: &mut bool, 
    min_dist: &mut f32, 
    best_cw: &mut [u8; 174]
) {
    const N: usize = 174;
    const K: usize = 91;
    
    // 编码
    let mut cw_mrb = [0u8; N];
    for i in 0..K {
        if m[i] == 1 {
            for j in 0..N {
                cw_mrb[j] ^= g_mrb[i][j];
            }
        }
    }
    
    // 还原原始顺序
    let mut cw_orig = [0u8; N];
    for j in 0..N {
        cw_orig[indices[j]] = cw_mrb[j];
    }
    
    // 检查 CRC
    if check_crc_ft8(&cw_orig) {
        let dist = calculate_dmin(zn, &cw_orig);
        if dist < *min_dist {
            *min_dist = dist;
            *best_cw = cw_orig;
            *found_any = true;
        }
    }
}
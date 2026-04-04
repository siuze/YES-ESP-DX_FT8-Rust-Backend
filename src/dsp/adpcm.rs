pub struct AdpcmState {
    pub valprev: i16,
    pub index: i16,
}

impl AdpcmState {
    pub fn new() -> Self {
        Self { valprev: 0, index: 0 }
    }
}

pub const INDEX_TABLE: [i16; 16] = [
    -1, -1, -1, -1, 2, 4, 6, 8,
    -1, -1, -1, -1, 2, 4, 6, 8,
];

pub const STEP_TABLE: [i16; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
    50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230,
    253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
    1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327,
    3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487,
    12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

pub fn encode_adpcm(sample: i16, state: &mut AdpcmState) -> u8 {
    let step = STEP_TABLE[state.index as usize];
    let mut diff = sample - state.valprev;
    let mut code = 0;

    if diff < 0 {
        code = 8;
        diff = -diff;
    }

    let mut temp_step = step;
    if diff >= temp_step {
        code |= 4;
        diff -= temp_step;
    }
    temp_step >>= 1;
    if diff >= temp_step {
        code |= 2;
        diff -= temp_step;
    }
    temp_step >>= 1;
    if diff >= temp_step {
        code |= 1;
    }

    let mut diffq = step >> 3;
    if (code & 4) != 0 { diffq += step; }
    if (code & 2) != 0 { diffq += step >> 1; }
    if (code & 1) != 0 { diffq += step >> 2; }

    if (code & 8) != 0 {
        state.valprev = state.valprev.saturating_sub(diffq);
    } else {
        state.valprev = state.valprev.saturating_add(diffq);
    }

    state.index += INDEX_TABLE[code as usize];
    state.index = state.index.clamp(0, 88);

    code as u8
}

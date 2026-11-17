// PCG XSH-RR style 32-bit hash. Cheap to evaluate, well-distributed.
fn pcg(state: u32) -> u32 {
    var s: u32 = state * 747796405u + 2891336453u;
    let word: u32 = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (word >> 22u) ^ word;
}

fn unit_f32(x: u32) -> f32 {
    // 1 / 2^32 → maps u32 uniformly into [0, 1).
    return f32(x) * (1.0 / 4294967296.0);
}

// SplitMix64 finalizer truncated to u32. Used to derive a per-pixel,
// per-channel, per-sublayer seed from the user `base_seed`.
fn splitmix32(x: u32) -> u32 {
    var z: u32 = x;
    z = (z ^ (z >> 16u)) * 0x85ebca6bu;
    z = (z ^ (z >> 13u)) * 0xc2b2ae35u;
    return z ^ (z >> 16u);
}

fn next_u32(state: ptr<function, u32>) -> u32 {
    let s = pcg(*state);
    *state = s;
    return s;
}

// Box-Muller standard normal. log(0) guarded with a small epsilon.
fn standard_normal(state: ptr<function, u32>) -> f32 {
    let u1 = unit_f32(next_u32(state));
    let u2 = unit_f32(next_u32(state));
    let r = sqrt(-2.0 * log(max(u1, 1e-7)));
    return r * cos(6.28318530717958647 * u2);
}

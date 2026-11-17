// Initialize the scattering mixture from the original exposure and Gaussian
// core. The three exponential-tail components are accumulated separately.

struct Params {
    n_pixels: u32,
    scatter_amount: f32,
    pad0: u32,
    pad1: u32,
    tail_weight: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> source: array<f32>;
@group(0) @binding(2) var<storage, read> core: array<f32>;
@group(0) @binding(3) var<storage, read_write> mix_buffer: array<f32>;

@compute @workgroup_size(1024)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= params.n_pixels {
        return;
    }
    let base = id.x * 3u;
    let scattered = params.scatter_amount;
    for (var c = 0u; c < 3u; c += 1u) {
        mix_buffer[base + c] = (1.0 - scattered) * source[base + c]
            + scattered * (1.0 - params.tail_weight[c]) * core[base + c];
    }
}

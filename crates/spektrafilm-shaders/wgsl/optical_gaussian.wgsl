// Per-channel film scatter and halation Gaussian pass, using the same
// reflected short FIR and sample-replicated Young–van Vliet IIR as Python.

struct Params {
    width: u32,
    height: u32,
    first_pass: u32,
    pad: u32,
    radii: vec4<i32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> source: array<f32>;
@group(0) @binding(2) var<storage, read> weights: array<f32>;
@group(0) @binding(3) var<storage, read_write> output: array<f32>;

const CHANNEL_STRIDE: u32 = 19u;

fn reflect_edge(index: i32, length: i32) -> u32 {
    if index >= 0 && index < length {
        return u32(index);
    }
    let period = 2 * length;
    let wrap = (index % period + period) % period;
    return u32(min(wrap, period - 1 - wrap));
}

// State: value, first difference, second difference.
fn iir_step(sample_value: f32, state: vec3<f32>, coefficients: vec3<f32>) -> vec3<f32> {
    let second = fma(coefficients.x, sample_value - state.x,
        fma(coefficients.y, state.y, coefficients.z * state.z));
    let first = state.y + second;
    return vec3<f32>(state.x + first, first, second);
}

fn iir_line(head: u32, stride: u32, length: u32, offset: u32) {
    let coeff = vec3<f32>(weights[offset], weights[offset + 1u], weights[offset + 2u]);
    var state = vec3<f32>(source[head], 0.0, 0.0);
    for (var i = 0u; i < length; i += 1u) {
        let index = head + i * stride;
        state = iir_step(source[index], state, coeff);
        output[index] = state.x;
    }
    state = vec3<f32>(state.x, 0.0, 0.0);
    for (var i = length; i > 0u; i -= 1u) {
        let index = head + (i - 1u) * stride;
        state = iir_step(output[index], state, coeff);
        output[index] = state.x;
    }
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    let width = params.width;
    let height = params.height;
    if x >= width || y >= height {
        return;
    }
    let base = (y * width + x) * 3u;
    for (var c = 0u; c < 3u; c += 1u) {
        let radius = params.radii[c];
        let offset = c * CHANNEL_STRIDE;
        if radius < 0 {
            if params.first_pass != 0u && x == 0u {
                iir_line(base + c, 3u, width, offset);
            } else if params.first_pass == 0u && y == 0u {
                iir_line(base + c, width * 3u, height, offset);
            }
        } else if radius == 0 {
            output[base + c] = source[base + c];
        } else {
            // Python's short FIR starts vertically, the IIR starts horizontally.
            let horizontal = params.first_pass == 0u;
            var value = 0.0;
            for (var k = -radius; k <= radius; k += 1) {
                var sample_x = x;
                var sample_y = y;
                if horizontal {
                    sample_x = reflect_edge(i32(x) + k, i32(width));
                } else {
                    sample_y = reflect_edge(i32(y) + k, i32(height));
                }
                let index = (sample_y * width + sample_x) * 3u + c;
                value = fma(source[index], weights[offset + u32(k + radius)], value);
            }
            output[base + c] = value;
        }
    }
}

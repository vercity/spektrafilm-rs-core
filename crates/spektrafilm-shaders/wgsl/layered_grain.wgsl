// Profiled emulsion grain. A developed population obtained by thinning a
// Poisson seed population with binomial probability p is itself Poisson.

struct Params {
    n_pixels: u32,
    mode: u32,
    layer: u32,
    rows: u32,
    direction: f32,
    clump_mu: f32,
    clump_sigma: f32,
    monochrome: u32,
    floor: vec4<f32>,
    maximum: vec4<f32>,
    particles: vec4<f32>,
    uniformity: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> source: array<f32>;
@group(0) @binding(2) var<storage, read> curves: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> result: array<f32>;

fn open_uniform(state: ptr<function, u32>) -> f32 {
    // Midpoints of representable 23-bit bins, excluding both endpoints.
    return (f32(next_u32(state) >> 9u) + 0.5) / 8388608.0;
}

fn log_factorial(k: f32) -> f32 {
    if k < 10.0 {
        var sum = 0.0;
        for (var i = 2.0; i <= k; i += 1.0) {
            sum += log(i);
        }
        return sum;
    }
    let inv = 1.0 / k;
    return (k + 0.5) * log(k) - k + 0.9189385332046727
        + inv * (1.0 / 12.0 - inv * inv / 360.0);
}

fn grain_seed(pixel: u32, stream: u32) -> u32 {
    return splitmix32(pixel ^ ((stream + 1u) * 0x9e3779b9u));
}

fn sample_poisson(lambda: f32, state: ptr<function, u32>) -> f32 {
    // Invalid runtime inputs must terminate even if a rate becomes NaN.
    if !(lambda > 0.0 && lambda <= 3.402823466e38) {
        return 0.0;
    }
    if lambda < 10.0 {
        let minimum = exp(-lambda);
        var product = open_uniform(state);
        var count = 0.0;
        while product > minimum && count < 128.0 {
            product *= open_uniform(state);
            count += 1.0;
        }
        return count;
    }
    // Large populations have negligible standardized Poisson skew and
    // the transformed rejection log likelihood loses precision in f32.
    if lambda >= 4096.0 {
        return max(0.0, round(lambda + sqrt(lambda) * standard_normal(state)));
    }
    // Hörmann transformed rejection for intermediate populations.
    let root = sqrt(lambda);
    let b = 0.931 + 2.53 * root;
    let a = -0.059 + 0.02483 * b;
    let inv_alpha = 1.1239 + 1.1328 / (b - 3.4);
    let vr = 0.9277 - 3.6224 / (b - 2.0);
    let log_lambda = log(lambda);
    for (var attempt = 0u; attempt < 64u; attempt += 1u) {
        let u = open_uniform(state) - 0.5;
        let v = open_uniform(state);
        let us = 0.5 - abs(u);
        let k = floor((2.0 * a / us + b) * u + lambda + 0.43);
        if us >= 0.07 && v <= vr {
            return k;
        }
        if k >= 0.0 && (us >= 0.013 || v <= us) {
            let lhs = log(v * inv_alpha / (a / (us * us) + b));
            let rhs = fma(k, log_lambda, -lambda) - log_factorial(k);
            if lhs <= rhs {
                return k;
            }
        }
    }
    // Bound dispatch time if all proposals miss the rejection test.
    return max(0.0, round(lambda + sqrt(lambda) * standard_normal(state)));
}

fn layer_density(density: f32, channel: u32) -> f32 {
    let start = channel * params.rows;
    let axis = density * params.direction;
    let column = params.layer + 1u;
    let last = start + params.rows - 1u;
    if !(axis > curves[start].x) {
        return curves[start][column];
    }
    if axis >= curves[last].x {
        return curves[last][column];
    }
    var left = start;
    var right = last + 1u;
    while left < right {
        let middle = left + (right - left) / 2u;
        if axis < curves[middle].x {
            right = middle;
        } else {
            left = middle + 1u;
        }
    }
    let lo = curves[left - 1u];
    let hi = curves[left];
    let width = hi.x - lo.x;
    if width == 0.0 {
        return lo[column];
    }
    return mix(lo[column], hi[column], (axis - lo.x) / width);
}

@compute @workgroup_size(1024)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let pixel = id.x;
    if pixel >= params.n_pixels {
        return;
    }
    let base = 3u * pixel;
    for (var channel = 0u; channel < 3u; channel += 1u) {
        let model_channel = select(channel, 0u, params.monochrome != 0u);
        let offset = base + channel;
        if params.mode == 0u {
            let density = layer_density(source[base + model_channel], model_channel);
            let maximum = params.maximum[channel];
            let npp = params.particles[channel];
            let p = clamp((density + params.floor[channel]) / maximum, 1e-6, 1.0 - 1e-6);
            let saturation = 1.0 - p * params.uniformity[channel] * (1.0 - 1e-6);
            var rng = grain_seed(pixel, model_channel + 10u * params.layer);
            let developed = sample_poisson(npp * p / saturation, &rng);
            result[offset] = developed * (maximum / npp) * saturation;
        } else if params.mode == 1u {
            var rng = grain_seed(pixel, 30u + model_channel);
            result[offset] = exp(params.clump_mu + params.clump_sigma * standard_normal(&rng));
        } else {
            var density = result[offset];
            if params.clump_sigma > 0.0 {
                density *= source[offset];
            }
            result[offset] = density - params.floor[channel];
        }
    }
}

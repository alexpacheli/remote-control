// Fullscreen triangle sampling I420 planes, BT.601 limited range -> RGB.

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // (0,0), (2,0), (0,2): one triangle covering the viewport.
    let p = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VsOut;
    out.position = vec4<f32>(p * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.uv = p;
    return out;
}

@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var u_plane: texture_2d<f32>;
@group(0) @binding(2) var v_plane: texture_2d<f32>;
@group(0) @binding(3) var planes: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(y_plane, planes, in.uv).r - 16.0 / 255.0) * 1.164;
    let u = textureSample(u_plane, planes, in.uv).r - 0.5;
    let v = textureSample(v_plane, planes, in.uv).r - 0.5;
    let rgb = vec3<f32>(
        y + 1.596 * v,
        y - 0.392 * u - 0.813 * v,
        y + 2.017 * u,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

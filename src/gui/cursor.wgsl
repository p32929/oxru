// Draws up to `count` solid-colour pixel rectangles (the editor's caret bars)
// directly onto the already-composited surface, in a second pass that runs
// after the normal text blit (see `CaretPostProcessor::process`). A full-screen
// triangle is rasterised and each fragment either falls inside one of the
// rectangles (and is written that rectangle's colour) or is discarded — so
// everywhere but the caret's own pixels is left untouched, preserving whatever
// text was already drawn underneath.

struct Uniforms {
    screen_size: vec4<f32>,
    count: vec4<u32>,
    rects: array<vec4<f32>, 32>,
    colors: array<vec4<f32>, 32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(pos[idx], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag_pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = frag_pos.xy;
    let n = u.count.x;
    for (var i: u32 = 0u; i < n; i = i + 1u) {
        let r = u.rects[i];
        if (p.x >= r.x && p.x < r.x + r.z && p.y >= r.y && p.y < r.y + r.w) {
            return u.colors[i];
        }
    }
    discard;
}

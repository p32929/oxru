//! GPU overlay that draws the editor caret(s) as thin bars on top of the
//! already-rendered text, instead of overwriting a cell's glyph.
//!
//! ratatui's cell model holds exactly one symbol per cell, so a caret drawn
//! *through* the normal buffer (as `▏`) necessarily replaces whatever character
//! was in that cell — hiding it, which is very noticeable next to non-blank
//! text. This [`PostProcessor`] sidesteps that: it runs as a second render
//! pass, after `ratatui-wgpu` has already composited and blitted the text,
//! drawing a handful of solid-colour pixel rectangles straight onto the
//! surface. The character underneath is never touched — `discard` in the
//! fragment shader leaves every pixel outside a caret rectangle exactly as the
//! text pass left it.

use std::num::NonZeroU64;

use ratatui_wgpu::shaders::DefaultPostProcessor;
use ratatui_wgpu::wgpu::*;
use ratatui_wgpu::PostProcessor;

/// Hard cap on simultaneous caret rectangles (primary + secondary multi-cursor
/// carets). Comfortably above any realistic multi-cursor session; extras are
/// silently dropped rather than growing the uniform buffer per frame.
const MAX_CARETS: usize = 32;

/// Mirrors `cursor.wgsl`'s `Uniforms` struct exactly — every field is
/// `vec4`-sized/aligned on both sides so the two layouts can't drift apart.
#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
struct Uniforms {
    screen_size: [f32; 4],
    count: [u32; 4],
    rects: [[f32; 4]; MAX_CARETS],
    colors: [[f32; 4]; MAX_CARETS],
}

impl Default for Uniforms {
    fn default() -> Self {
        Self {
            screen_size: [0.0; 4],
            count: [0; 4],
            rects: [[0.0; 4]; MAX_CARETS],
            colors: [[0.0; 4]; MAX_CARETS],
        }
    }
}

/// The uniform block as the bytes the GPU expects.
///
/// This was a pointer cast — one instruction, but the only `unsafe` in the
/// crate outside [`crate::sys`]. Written out scalar by scalar it needs none,
/// and the cost is nothing that matters: 264 numbers, copied only when the
/// carets actually change (`set_carets` leaves `pending` untouched otherwise,
/// and `needs_update` compares before uploading), not once per frame.
///
/// Native-endian, matching both the cast it replaces and what the driver reads
/// on this machine. `Uniforms` is `repr(C)` and every field is `vec4`-aligned,
/// so writing the fields in declaration order reproduces the struct layout
/// exactly — asserted by `uniform_bytes_match_the_struct_layout`.
fn bytes_of(v: &Uniforms) -> Vec<u8> {
    let mut out = Vec::with_capacity(size_of::<Uniforms>());
    for f in v.screen_size {
        out.extend_from_slice(&f.to_ne_bytes());
    }
    for n in v.count {
        out.extend_from_slice(&n.to_ne_bytes());
    }
    for row in v.rects.iter().chain(v.colors.iter()) {
        for f in row {
            out.extend_from_slice(&f.to_ne_bytes());
        }
    }
    out
}

/// A caret to draw this frame, in physical pixels, with a straight (unblended)
/// `rgba` colour.
#[derive(Clone, Copy)]
pub struct CaretRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
}

/// Wraps the crate's normal text blitter ([`DefaultPostProcessor`]) and adds a
/// second pass that paints caret rectangles on top, so text compositing is
/// completely unchanged and the caret is a strictly additive overlay.
pub struct CaretPostProcessor {
    inner: DefaultPostProcessor,
    pipeline: RenderPipeline,
    uniform_buf: Buffer,
    bind_group: BindGroup,
    pending: Uniforms,
    last_drawn: Uniforms,
}

impl CaretPostProcessor {
    /// Replace this frame's caret list (in physical pixels). Takes effect on
    /// the next [`PostProcessor::process`] call. A call identical to what's
    /// already drawn doesn't mark the backend dirty (see
    /// [`PostProcessor::needs_update`]), so a stationary, non-blinking caret
    /// costs nothing beyond the comparison.
    pub fn set_carets(&mut self, screen_w: f32, screen_h: f32, carets: &[CaretRect]) {
        let mut u = Uniforms {
            screen_size: [screen_w, screen_h, 0.0, 0.0],
            count: [carets.len().min(MAX_CARETS) as u32, 0, 0, 0],
            ..Uniforms::default()
        };
        for (i, c) in carets.iter().take(MAX_CARETS).enumerate() {
            u.rects[i] = [c.x, c.y, c.w, c.h];
            u.colors[i] = c.color;
        }
        self.pending = u;
    }
}

impl PostProcessor for CaretPostProcessor {
    type UserData = ();

    fn compile(
        device: &Device,
        text_view: &TextureView,
        surface_config: &SurfaceConfiguration,
        _user_data: Self::UserData,
    ) -> Self {
        let inner = DefaultPostProcessor::compile(device, text_view, surface_config, ());

        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("Oxru Caret Shader"),
            source: ShaderSource::Wgsl(include_str!("cursor.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("Oxru Caret Bindings"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size_of::<Uniforms>() as u64),
                },
                count: None,
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("Oxru Caret Layout"),
            bind_group_layouts: &[&bind_group_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("Oxru Caret Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: PrimitiveState {
                topology: PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: MultisampleState::default(),
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: PipelineCompilationOptions::default(),
                targets: &[Some(ColorTargetState {
                    format: surface_config.format,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let uniform_buf = device.create_buffer(&BufferDescriptor {
            label: Some("Oxru Caret Uniforms"),
            size: size_of::<Uniforms>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("Oxru Caret Bind Group"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        Self {
            inner,
            pipeline,
            uniform_buf,
            bind_group,
            pending: Uniforms::default(),
            last_drawn: Uniforms::default(),
        }
    }

    fn resize(
        &mut self,
        device: &Device,
        text_view: &TextureView,
        surface_config: &SurfaceConfiguration,
    ) {
        self.inner.resize(device, text_view, surface_config);
        // Caret geometry (in cells) may be numerically unchanged across a
        // resize even though the on-screen pixels moved — force one redraw.
        self.last_drawn.screen_size = [-1.0; 4];
    }

    fn process(
        &mut self,
        encoder: &mut CommandEncoder,
        queue: &Queue,
        text_view: &TextureView,
        surface_config: &SurfaceConfiguration,
        surface_view: &TextureView,
    ) {
        self.inner
            .process(encoder, queue, text_view, surface_config, surface_view);

        let n = self.pending.count[0] as usize;
        if n > 0 {
            queue.write_buffer(&self.uniform_buf, 0, &bytes_of(&self.pending));
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("Oxru Caret Pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: surface_view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Load,
                        store: StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.last_drawn = self.pending;
    }

    fn needs_update(&self) -> bool {
        self.pending != self.last_drawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written serialiser has to produce exactly the struct's own
    /// layout — the shader reads it by offset, so a wrong length or a field in
    /// the wrong place would draw carets at nonsense coordinates.
    #[test]
    fn uniform_bytes_match_the_struct_layout() {
        let mut u = Uniforms {
            screen_size: [1.0, 2.0, 0.0, 0.0],
            count: [3, 0, 0, 0],
            ..Uniforms::default()
        };
        u.rects[0] = [10.0, 20.0, 30.0, 40.0];
        u.colors[MAX_CARETS - 1] = [0.5, 0.25, 0.125, 1.0];

        let bytes = bytes_of(&u);
        assert_eq!(bytes.len(), size_of::<Uniforms>(), "size must match the struct");

        // Spot-check the three regions by their declared offsets.
        let f32_at = |off: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&bytes[off..off + 4]);
            f32::from_ne_bytes(b)
        };
        let u32_at = |off: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&bytes[off..off + 4]);
            u32::from_ne_bytes(b)
        };
        assert_eq!(f32_at(0), 1.0, "screen width leads");
        assert_eq!(u32_at(16), 3, "the count follows the 4-float screen_size");
        assert_eq!(f32_at(32), 10.0, "rects start after screen_size + count");
        let colors_off = 32 + MAX_CARETS * 16;
        assert_eq!(f32_at(colors_off + (MAX_CARETS - 1) * 16), 0.5, "colors follow rects");
    }
}

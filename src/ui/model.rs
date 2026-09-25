//! 3D model rendering for the per-card image area.
//!
//! Loads a Wavefront OBJ file once, optionally pairs it with an MTL
//! companion to extract per-mesh diffuse colors, bakes those colors into
//! a per-vertex attribute, and draws the entire model in a single
//! `draw_indexed` call through an `eframe::egui_wgpu::CallbackTrait`.
//! The shader is a Lambert + soft-rim pass — just enough to read as 3D.
//!
//! Why one draw call: each Metal `drawIndexedPrimitives` triggers a fresh
//! `encodeAndEmitRenderState` cycle on the CPU side. Splitting by
//! material would multiply that fixed cost by the material count for no
//! visual benefit since the material set is tiny and well-separated.
//!
//! Two ways to get pixels on screen: `paint` draws straight into egui's
//! (single-sampled) render pass, while `render_antialiased` +
//! `paint_antialiased` render 4× MSAA offscreen and composite the resolved
//! image. egui's pass can't be multisampled per-callback or toggled at
//! runtime, hence the offscreen detour — which is what lets the app drop
//! back to the direct path when a machine can't keep up.

use eframe::wgpu;
use std::sync::Arc;
use wgpu::util::DeviceExt;

const SHADER_SRC: &str = r#"
struct Transforms {
    mvp: mat4x4<f32>,
    model: mat4x4<f32>,
    light_dir: vec4<f32>,
};
@group(0) @binding(0) var<uniform> t: Transforms;

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) normal:   vec3<f32>,
    @location(2) color:    vec3<f32>,
};
struct VsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world_normal: vec3<f32>,
    @location(1) color: vec3<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_position = t.mvp * vec4<f32>(in.position, 1.0);
    // No non-uniform scale in our model matrix (rotation + translation
    // only), so transforming the normal by the upper-3x3 is sufficient.
    out.world_normal = (t.model * vec4<f32>(in.normal, 0.0)).xyz;
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.world_normal);
    let l = normalize(t.light_dir.xyz);
    let lambert = max(dot(n, l), 0.0);
    let rim = 1.0 - lambert;
    let col = (0.22 + 0.78 * lambert) * in.color + 0.05 * rim;
    return vec4<f32>(col, 1.0);
}
"#;

/// Fullscreen-triangle composite of the resolved offscreen image into the
/// callback's viewport. The resolve leaves edge pixels premultiplied
/// against the transparent clear, so the pipeline blends premultiplied.
const BLIT_SHADER_SRC: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

const MSAA_SAMPLES: u32 = 4;

/// Per-frame transforms — the only uniform written per frame.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Transforms {
    mvp: [[f32; 4]; 4],
    model: [[f32; 4]; 4],
    light_dir: [f32; 4],
}

pub struct ModelRenderer {
    pipeline: wgpu::RenderPipeline,
    /// Present only when built with `antialias`.
    antialias: Option<Antialias>,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    /// Single shared per-frame transforms buffer. Updated by `prepare()`
    /// once per frame; bound by `bind_group`.
    transform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    index_count: u32,
    /// Precomputed `view * translate(-bbox_center)` — constant for the
    /// life of the renderer. Combined with the per-frame rotation matrix
    /// in `prepare()`, this avoids rebuilding the view and centering
    /// translation matrices every frame.
    view_centering: glam::Mat4,
    /// Precomputed normalized light direction. Constant.
    light_dir: glam::Vec3,
}

struct Antialias {
    /// Same shading as `ModelRenderer::pipeline`, at `MSAA_SAMPLES`.
    pipeline: wgpu::RenderPipeline,
    blit_pipeline: wgpu::RenderPipeline,
    blit_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    target_format: wgpu::TextureFormat,
    depth_format: wgpu::TextureFormat,
}

/// Offscreen attachments for one antialiased render at a fixed pixel size.
pub struct AntialiasTarget {
    size: (u32, u32),
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
    /// Single-sampled resolve of `color`, in the renderer's target format.
    pub resolved: wgpu::Texture,
    resolved_view: wgpu::TextureView,
    blit_bind_group: wgpu::BindGroup,
}

impl AntialiasTarget {
    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}

impl ModelRenderer {
    /// Parse OBJ + (optional) MTL bytes and build the GPU resources: an
    /// interleaved vertex buffer (position + normal + color), index buffer,
    /// pipeline, and the per-frame transform bind group. `antialias` also
    /// builds the 4× MSAA path; the caller must have checked that
    /// `target_format` supports 4× multisampling.
    pub fn from_obj_bytes(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        antialias: bool,
        obj_bytes: &[u8],
        mtl_bytes: &[u8],
    ) -> Result<Self, String> {
        let parsed = parse_obj(obj_bytes, mtl_bytes)?;
        let ParsedObj {
            interleaved,
            indices,
            bbox_min,
            bbox_max,
        } = parsed;

        let bbox_center = (bbox_min + bbox_max) * 0.5;
        let bbox_radius = (bbox_max - bbox_min).length() * 0.5;
        let index_count = indices.len() as u32;

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("model.vertices"),
            contents: bytemuck::cast_slice(&interleaved),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("model.indices"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("model.bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("model.pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("model.shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER_SRC)),
        });

        let make_pipeline = |sample_count: u32| device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("model.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: (9 * std::mem::size_of::<f32>()) as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: (3 * std::mem::size_of::<f32>()) as wgpu::BufferAddress,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: (6 * std::mem::size_of::<f32>()) as wgpu::BufferAddress,
                            shader_location: 2,
                        },
                    ],
                }],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
        });
        let pipeline = make_pipeline(1);
        let antialias = antialias.then(|| {
            build_antialias(device, make_pipeline(MSAA_SAMPLES), target_format, depth_format)
        });

        // Single shared transform UBO — written once per frame in prepare().
        let transform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("model.transforms"),
            size: std::mem::size_of::<Transforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("model.bind_group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: transform_buffer.as_entire_binding(),
            }],
        });

        let view = glam::Mat4::look_at_rh(
            glam::vec3(0.0, 0.0, bbox_radius * 2.5),
            glam::Vec3::ZERO,
            glam::Vec3::Y,
        );
        let view_centering = view * glam::Mat4::from_translation(-bbox_center);
        let light_dir = glam::vec3(0.4, 0.7, 0.6).normalize();

        Ok(Self {
            pipeline,
            antialias,
            vertex_buffer,
            index_buffer,
            transform_buffer,
            bind_group,
            index_count,
            view_centering,
            light_dir,
        })
    }

    /// Build this frame's projection/view/model matrices and push them
    /// to the GPU. Material colors are baked into the vertex buffer at
    /// load time, so this is the only `queue.write_buffer` call per frame.
    pub fn prepare(
        &self,
        queue: &wgpu::Queue,
        viewport_w: f32,
        viewport_h: f32,
        angle_rad: f32,
    ) {
        let aspect = (viewport_w / viewport_h.max(1.0)).max(0.001);
        let projection =
            glam::Mat4::perspective_rh_gl(45f32.to_radians(), aspect, 0.01, 100.0);
        let rotation = glam::Mat4::from_rotation_y(angle_rad);
        // Full transform was `projection * view * rotation * translate(-bbox_center)`.
        // `view * translate(-bbox_center)` is precomputed at load time as
        // `view_centering`, so per-frame we only build the rotation and
        // two 4x4 multiplies. The shader's "model" matrix is used only
        // to rotate normals — translation has no effect on directional
        // vectors (w=0), so we just pass the rotation.
        let mvp = projection * self.view_centering * rotation;

        let t = Transforms {
            mvp: mvp.to_cols_array_2d(),
            model: rotation.to_cols_array_2d(),
            light_dir: [self.light_dir.x, self.light_dir.y, self.light_dir.z, 0.0],
        };
        queue.write_buffer(&self.transform_buffer, 0, bytemuck::bytes_of(&t));
    }

    /// Issue draw commands inside the egui render pass. Sets a scissor
    /// rect so the model can't escape its card. `scissor` is in
    /// physical pixels: `(x, y, w, h)` with origin at the top-left.
    pub fn paint<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
        scissor: (u32, u32, u32, u32),
    ) {
        let (sx, sy, sw, sh) = scissor;
        if sw == 0 || sh == 0 {
            return;
        }
        render_pass.set_scissor_rect(sx, sy, sw, sh);
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        render_pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.draw_indexed(0..self.index_count, 0, 0..1);
    }

    pub fn supports_antialias(&self) -> bool {
        self.antialias.is_some()
    }

    /// Allocate offscreen attachments for `render_antialiased` at `size`
    /// physical pixels. `None` when built without `antialias`.
    pub fn antialias_target(&self, device: &wgpu::Device, size: (u32, u32)) -> Option<AntialiasTarget> {
        let aa = self.antialias.as_ref()?;
        let (w, h) = (size.0.max(1), size.1.max(1));
        let texture = |label, samples, format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let color = texture("model.aa.color", MSAA_SAMPLES, aa.target_format, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let depth = texture("model.aa.depth", MSAA_SAMPLES, aa.depth_format, wgpu::TextureUsages::RENDER_ATTACHMENT);
        let resolved = texture(
            "model.aa.resolved",
            1,
            aa.target_format,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
        );
        let resolved_view = view(&resolved);
        let blit_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("model.aa.blit_bind_group"),
            layout: &aa.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&resolved_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&aa.sampler),
                },
            ],
        });
        Some(AntialiasTarget {
            size: (w, h),
            color: view(&color),
            depth: view(&depth),
            resolved,
            resolved_view,
            blit_bind_group,
        })
    }

    /// Record a 4× MSAA render of the model into `target`, resolved into
    /// `target.resolved` over a transparent background. Uses the
    /// transforms from the last `prepare()`.
    pub fn render_antialiased(&self, encoder: &mut wgpu::CommandEncoder, target: &AntialiasTarget) {
        let Some(aa) = self.antialias.as_ref() else {
            return;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("model.aa.pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.color,
                resolve_target: Some(&target.resolved_view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &target.depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&aa.pipeline);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw_indexed(0..self.index_count, 0, 0..1);
    }

    /// Composite `target`'s resolved image into the egui render pass,
    /// filling the viewport egui set for the callback. `scissor` as in
    /// `paint`.
    pub fn paint_antialiased<'rp>(
        &'rp self,
        render_pass: &mut wgpu::RenderPass<'rp>,
        target: &'rp AntialiasTarget,
        scissor: (u32, u32, u32, u32),
    ) {
        let Some(aa) = self.antialias.as_ref() else {
            return;
        };
        let (sx, sy, sw, sh) = scissor;
        if sw == 0 || sh == 0 {
            return;
        }
        render_pass.set_scissor_rect(sx, sy, sw, sh);
        render_pass.set_pipeline(&aa.blit_pipeline);
        render_pass.set_bind_group(0, &target.blit_bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

fn build_antialias(
    device: &wgpu::Device,
    pipeline: wgpu::RenderPipeline,
    target_format: wgpu::TextureFormat,
    depth_format: wgpu::TextureFormat,
) -> Antialias {
    let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("model.aa.blit_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("model.aa.blit_layout"),
        bind_group_layouts: &[&blit_layout],
        push_constant_ranges: &[],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("model.aa.blit_shader"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(BLIT_SHADER_SRC)),
    });
    let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("model.aa.blit"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        // egui's pass carries a depth attachment (depth_buffer = 32), so the
        // pipeline must declare a matching one even though it ignores depth.
        depth_stencil: Some(wgpu::DepthStencilState {
            format: depth_format,
            depth_write_enabled: false,
            depth_compare: wgpu::CompareFunction::Always,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("model.aa.sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    Antialias {
        pipeline,
        blit_pipeline,
        blit_layout,
        sampler,
        target_format,
        depth_format,
    }
}

/// Diffuse for meshes with no resolved material, in sRGB (converted to
/// linear at parse time like MTL diffuse values).
const DEFAULT_COLOR: glam::Vec3 = glam::Vec3::new(0.78, 0.82, 0.88);

/// sRGB → linear (the standard piecewise curve). MTL diffuse values are
/// sRGB-encoded while the shader lights in linear space; skipping this
/// applies gamma twice and grays render visibly too light.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

struct ParsedObj {
    /// Interleaved vertex data: 9 f32 per vertex (position xyz, normal
    /// xyz, color rgb). The diffuse color is baked per-vertex at parse
    /// time so we can render every mesh in a single draw_indexed call.
    interleaved: Vec<f32>,
    indices: Vec<u32>,
    bbox_min: glam::Vec3,
    bbox_max: glam::Vec3,
}

/// Parse OBJ + MTL into a single interleaved vertex buffer (with
/// per-vertex baked color) and a single index buffer. The
/// caller-supplied `mtl_bytes` is consulted first; on top of that we
/// synthesize material entries from any `usemtl` directives whose names
/// follow the Onshape "R_G_B_spec_shininess" convention so the diffuse
/// color comes from the OBJ itself even when no MTL ships.
fn parse_obj(obj_bytes: &[u8], mtl_bytes: &[u8]) -> Result<ParsedObj, String> {
    let mut reader = std::io::Cursor::new(obj_bytes);
    let load_options = tobj::LoadOptions {
        single_index: true,
        triangulate: true,
        ignore_points: true,
        ignore_lines: true,
    };

    let mut combined_mtl: Vec<u8> = mtl_bytes.to_vec();
    if !combined_mtl.is_empty() && !combined_mtl.ends_with(b"\n") {
        combined_mtl.push(b'\n');
    }
    combined_mtl.extend_from_slice(&synthesize_mtl_from_obj(obj_bytes));

    let mtl_loader = move |_p: &std::path::Path| -> tobj::MTLLoadResult {
        if combined_mtl.is_empty() {
            return Err(tobj::LoadError::OpenFileFailed);
        }
        let mut r = std::io::Cursor::new(&combined_mtl);
        tobj::load_mtl_buf(&mut r)
    };

    let (models, materials_result) =
        tobj::load_obj_buf(&mut reader, &load_options, mtl_loader)
            .map_err(|e| format!("OBJ parse: {e}"))?;
    let materials: Vec<tobj::Material> = materials_result.unwrap_or_default();

    let mut interleaved: Vec<f32> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut bbox_min = glam::vec3(f32::INFINITY, f32::INFINITY, f32::INFINITY);
    let mut bbox_max = glam::vec3(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);

    for model in &models {
        let mesh = &model.mesh;
        let base_vertex = (interleaved.len() / 9) as u32;

        let color_srgb = mesh
            .material_id
            .and_then(|id| materials.get(id))
            .and_then(|m| m.diffuse)
            .map(|d| glam::vec3(d[0], d[1], d[2]))
            .unwrap_or(DEFAULT_COLOR);
        let color = glam::vec3(
            srgb_to_linear(color_srgb.x),
            srgb_to_linear(color_srgb.y),
            srgb_to_linear(color_srgb.z),
        );

        // single_index=true means positions and normals share the same
        // index. Missing normals would require recomputation; we rely on
        // the OBJ shipping per-vertex normals (motionhexa does).
        let pos_chunks = mesh.positions.chunks_exact(3);
        let has_normals = !mesh.normals.is_empty();
        for (i, p) in pos_chunks.enumerate() {
            let pv = glam::vec3(p[0], p[1], p[2]);
            bbox_min = bbox_min.min(pv);
            bbox_max = bbox_max.max(pv);
            interleaved.push(p[0]);
            interleaved.push(p[1]);
            interleaved.push(p[2]);
            if has_normals {
                interleaved.push(mesh.normals[i * 3]);
                interleaved.push(mesh.normals[i * 3 + 1]);
                interleaved.push(mesh.normals[i * 3 + 2]);
            } else {
                interleaved.push(0.0);
                interleaved.push(0.0);
                interleaved.push(0.0);
            }
            interleaved.push(color.x);
            interleaved.push(color.y);
            interleaved.push(color.z);
        }
        for &idx in &mesh.indices {
            indices.push(base_vertex + idx);
        }
    }

    if interleaved.is_empty() {
        return Err("OBJ contained no geometry".to_string());
    }
    Ok(ParsedObj {
        interleaved,
        indices,
        bbox_min,
        bbox_max,
    })
}

/// Walk the OBJ source, find every unique `usemtl <name>` whose name
/// looks like Onshape's `R_G_B_specular_shininess` color encoding, and
/// emit minimal MTL definitions (`newmtl … / Kd r g b`) for each. This
/// lets tobj resolve per-mesh diffuse colors directly from the OBJ even
/// when the original MTL file isn't shipped.
fn synthesize_mtl_from_obj(obj_bytes: &[u8]) -> Vec<u8> {
    let s = match std::str::from_utf8(obj_bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = String::new();
    for raw in s.lines() {
        let line = raw.trim();
        let Some(name) = line.strip_prefix("usemtl ") else {
            continue;
        };
        if !seen.insert(name) {
            continue;
        }
        let parts: Vec<&str> = name.split('_').collect();
        if parts.len() < 3 {
            continue;
        }
        let (r, g, b) = match (
            parts[0].parse::<f32>(),
            parts[1].parse::<f32>(),
            parts[2].parse::<f32>(),
        ) {
            (Ok(r), Ok(g), Ok(b)) => (r, g, b),
            _ => continue,
        };
        out.push_str(&format!("newmtl {}\nKd {} {} {}\n", name, r, g, b));
    }
    out.into_bytes()
}

/// Wrapper to share an immutable `ModelRenderer` across egui paint
/// callbacks. Per-frame mutation lives entirely in the GPU-side uniform
/// buffers, written via `queue.write_buffer` (which only needs `&Queue`),
/// so no Mutex is needed.
pub type SharedModel = Arc<ModelRenderer>;

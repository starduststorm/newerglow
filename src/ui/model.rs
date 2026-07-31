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

impl ModelRenderer {
    /// Parse OBJ + (optional) MTL bytes and build the GPU resources: an
    /// interleaved vertex buffer (position + normal + color), index buffer,
    /// pipeline, and the per-frame transform bind group.
    pub fn from_obj_bytes(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
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

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
            multisample: wgpu::MultisampleState::default(),
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

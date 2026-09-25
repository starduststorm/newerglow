//! Pre-render each board's 3D model into a static PNG.
//!
//! Run once after a board's model.obj/model.mtl changes:
//!
//!     cargo run --release --bin bake-static-images --features bake
//!
//! Walks `boards/`, and for every board that ships a `model.obj` writes
//! `boards/<id>/model.png`, which is committed alongside the OBJ. The
//! runtime UI uses these as a graceful fallback when the live wgpu renderer
//! can't be set up or can't sustain 24 fps. Re-running overwrites the file.

use eframe::wgpu;
use newerglow::ui::model::ModelRenderer;
use std::path::PathBuf;

/// Output resolution. ~3× the runtime display size (IMAGE_SIDE × IMAGE_HEIGHT
/// = 234 × 214 logical pixels) so the static fallback looks crisp on
/// HiDPI without bloating the binary.
const OUT_W: u32 = 702;
const OUT_H: u32 = 642;

/// Rotation angle for the static frame. ~36°, a meaningful 3/4-view
/// that reveals enough of the side to read as dimensional.
const STATIC_ANGLE: f32 = std::f32::consts::PI / 5.0;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("no suitable wgpu adapter — bake-static-images needs a GPU");
    log::info!("baking on adapter: {}", adapter.get_info().name);

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("bake.device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
        },
        None,
    ))
    .expect("device request failed");

    let repo_root = locate_repo_root().expect("could not locate repo root");

    let boards_dir = repo_root.join("boards");

    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&boards_dir)
        .unwrap_or_else(|e| panic!("read {}: {}", boards_dir.display(), e))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let id = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let obj_path = dir.join("model.obj");
        if !obj_path.is_file() {
            log::info!("skipping board '{}': no model.obj", id);
            continue;
        }
        let out_path = dir.join("model.png");

        let obj_bytes = std::fs::read(&obj_path)
            .unwrap_or_else(|e| panic!("read {}: {}", obj_path.display(), e));
        let mtl_bytes = std::fs::read(dir.join("model.mtl")).unwrap_or_default();

        let png = render_one(&device, &queue, &obj_bytes, &mtl_bytes)
            .unwrap_or_else(|e| panic!("render {}: {}", id, e));

        std::fs::write(&out_path, &png)
            .unwrap_or_else(|e| panic!("write {}: {}", out_path.display(), e));
        log::info!(
            "wrote {} ({} bytes, {}×{})",
            out_path.display(),
            png.len(),
            OUT_W,
            OUT_H,
        );
    }
}

/// Locate the repo root via CARGO_MANIFEST_DIR (set when invoked through
/// `cargo run`). Falls back to the current working directory.
fn locate_repo_root() -> Option<PathBuf> {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(|| std::env::current_dir().ok())
}

/// Render `obj_bytes` (paired with `mtl_bytes`) headlessly into a PNG
/// byte buffer, through the same 4× MSAA path as the live card. The
/// render target is sRGB Rgba8 with a transparent clear color so the model can be composited over arbitrary card
/// backgrounds at runtime.
fn render_one(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    obj_bytes: &[u8],
    mtl_bytes: &[u8],
) -> Result<Vec<u8>, String> {
    let target_format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let depth_format = wgpu::TextureFormat::Depth32Float;
    let renderer = ModelRenderer::from_obj_bytes(
        device,
        target_format,
        depth_format,
        true,
        obj_bytes,
        mtl_bytes,
    )?;
    let target = renderer
        .antialias_target(device, (OUT_W, OUT_H))
        .ok_or("antialiasing unavailable")?;

    // wgpu requires copy_texture_to_buffer source rows to be padded to
    // wgpu::COPY_BYTES_PER_ROW_ALIGNMENT (256). We strip the padding
    // before encoding the PNG.
    let unpadded_bytes_per_row = OUT_W * 4;
    let padded_bytes_per_row = (unpadded_bytes_per_row + 255) & !255;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("bake.readback"),
        size: (padded_bytes_per_row * OUT_H) as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    renderer.prepare(queue, OUT_W as f32, OUT_H as f32, STATIC_ANGLE);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("bake.encoder"),
    });
    renderer.render_antialiased(&mut encoder, &target);
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &target.resolved,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &readback,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(OUT_H),
            },
        },
        wgpu::Extent3d {
            width: OUT_W,
            height: OUT_H,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv()
        .map_err(|e| format!("readback channel: {e}"))?
        .map_err(|e| format!("readback map: {e}"))?;

    let data = slice.get_mapped_range();
    let mut rgba: Vec<u8> = Vec::with_capacity((unpadded_bytes_per_row * OUT_H) as usize);
    for row in 0..OUT_H {
        let start = (row * padded_bytes_per_row) as usize;
        let end = start + unpadded_bytes_per_row as usize;
        rgba.extend_from_slice(&data[start..end]);
    }
    drop(data);
    readback.unmap();

    let mut png_bytes: Vec<u8> = Vec::new();
    {
        use image::ImageEncoder as _;
        let encoder = image::codecs::png::PngEncoder::new_with_quality(
            &mut png_bytes,
            image::codecs::png::CompressionType::Best,
            image::codecs::png::FilterType::Adaptive,
        );
        encoder
            .write_image(&rgba, OUT_W, OUT_H, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("png encode: {e}"))?;
    }
    Ok(png_bytes)
}

// GUI implementation. Compiled when the `ui` feature is enabled.

mod app;
mod board_assets;
mod card;
mod github;
mod identity;
// `model` is `pub` so the `bake-static-images` dev tool can construct a
// ModelRenderer headlessly. Other UI submodules stay private.
pub mod model;
mod workers;

use eframe::egui;

/// Width of each device card (interior, inside the Frame::group inner_margin).
pub(crate) const CARD_WIDTH: f32 = 250.0;
/// Side of the image area inside the card. Fills the card's inner width.
pub(crate) const IMAGE_SIDE: f32 = CARD_WIDTH - 16.0; // = 234, accounting for Frame margins
/// Vertical extent of the image area. Shorter than the side because the
/// rendered model only fills ~60% of its bbox vertically — the extra
/// pixels read as wasted gap above the device-name label.
pub(crate) const IMAGE_HEIGHT: f32 = IMAGE_SIDE - 20.0; // = 214
/// Horizontal spacing between cards in the device row (item_spacing).
pub(crate) const CARD_GAP: f32 = 8.0;
/// Frame inner margin inside each card (left + right).
pub(crate) const CARD_FRAME_MARGINS: f32 = 16.0;
/// Padding around the card row inside the central panel.
pub(crate) const ROW_PADDING: f32 = 16.0;
/// Maximum number of cards visible without scrolling.
pub(crate) const MAX_VISIBLE_CARDS: f32 = 3.5;
/// Fixed window height. Vertical resize is disabled — we lay out cells to
/// fit exactly within this height. Sized to give roughly the same bottom
/// margin under the card as the left/right ROW_PADDING gutter.
pub(crate) const WINDOW_HEIGHT: f32 = 410.0;

/// Per-card outer footprint = content + frame margins + spacing to next.
const CARD_OUTER: f32 = CARD_WIDTH + CARD_FRAME_MARGINS + CARD_GAP;

/// Compute the desired window inner width for `n` connected devices,
/// clamped between 1 and `MAX_VISIBLE_CARDS` cards' worth of space.
pub(crate) fn desired_inner_width(n: usize) -> f32 {
    let visible = (n.max(1) as f32).min(MAX_VISIBLE_CARDS);
    // Subtract one trailing CARD_GAP because the last card has no following gap.
    visible * CARD_OUTER - CARD_GAP + ROW_PADDING
}

pub fn run() -> eframe::Result<()> {
    let one_card_w = desired_inner_width(1);
    // desired_inner_width already clamps `n` to MAX_VISIBLE_CARDS, so this is
    // exactly the 3.5-card width — no further min() needed.
    let max_w = desired_inner_width(MAX_VISIBLE_CARDS.ceil() as usize);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([one_card_w, WINDOW_HEIGHT])
        // Locking min == max for height disables vertical resize on
        // platforms that respect both bounds (macOS does).
        .with_min_inner_size([one_card_w, WINDOW_HEIGHT])
        .with_max_inner_size([max_w, WINDOW_HEIGHT])
        .with_resizable(true);
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        // wgpu renderer: Metal on macOS, DX12 on Windows, Vulkan on Linux.
        // Sidesteps the macOS GL software-fallback that previously pinned
        // the model at 5 fps and forced cull-only rendering.
        renderer: eframe::Renderer::Wgpu,
        depth_buffer: 32,
        ..Default::default()
    };
    eframe::run_native(
        "Newer Glow",
        options,
        Box::new(|cc| {
            apply_style(&cc.egui_ctx);
            install_fonts(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(app::App::new(cc)))
        }),
    )
}

fn load_window_icon() -> Option<std::sync::Arc<egui::IconData>> {
    let bytes = include_bytes!("../../assets/AppIcon.iconset/icon_512x512.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(std::sync::Arc::new(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }))
}

/// Replace egui's default monospace family ("Hack") with JetBrains Mono.
/// Every existing `.monospace()` callsite (port name, sn, version glyphs)
/// picks up the new font automatically; no per-callsite changes needed.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "jetbrains_mono".to_owned(),
        egui::FontData::from_static(include_bytes!("resources/JetBrainsMono-Regular.ttf")),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "jetbrains_mono".to_owned());
    ctx.set_fonts(fonts);
}

/// Apply a softer, more native-looking style: rounded corners on widgets,
/// roomier button padding.
fn apply_style(ctx: &egui::Context) {
    ctx.style_mut(|style| {
        let r = egui::Rounding::same(6.0);
        for w in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            w.rounding = r;
        }
        style.visuals.window_rounding = egui::Rounding::same(8.0);
        style.visuals.menu_rounding = egui::Rounding::same(6.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    });
}

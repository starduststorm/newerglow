use eframe::egui;

use crate::boards::Registry;
use crate::ui::app::{BoardReleases, DeviceCard, FirmwareSelection, ReleaseLibrary};
use crate::ui::model::SharedModel;
use crate::ui::workers::UpdatePhase;
use crate::ui::{CARD_WIDTH, IMAGE_HEIGHT, IMAGE_SIDE};

/// What the user did on a card this frame.
#[derive(Debug, Clone)]
pub enum Action {
    None,
    Blink(String),
    /// User clicked Update. `anchor` is the screen-space rect of the button
    /// itself, which the confirmation popover uses to position its arrow.
    Update {
        port_name: String,
        anchor: egui::Rect,
    },
    OpenGithub(String),
    /// Refresh one board's releases, or every board's when `None`.
    RefreshBoards(Option<String>),
    BrowseLocal(String), // port_name
    SelectRelease {
        port_name: String,
        board_id: String,
        idx: usize,
    },
    ClearLocal(String), // port_name
}

#[derive(Clone, Debug)]
pub enum Overlay {
    Running(UpdatePhase),
    Done(Result<(), String>),
}

pub struct DrawContext<'a> {
    pub device: &'a DeviceCard,
    pub update_in_progress: bool,
    /// True when a firmware source is resolvable for this device (a
    /// remote release, a local UF2, or an Auto-matched newest release).
    /// When false, the Update button stays disabled — there's nothing to flash.
    pub has_firmware: bool,
    pub overlay: Option<Overlay>,
    pub library: &'a ReleaseLibrary,
    pub registry: &'a Registry,
    pub external_link_tex: &'a egui::TextureHandle,
    pub external_link_hover_tex: &'a egui::TextureHandle,
    /// USB trident silhouette (white pixels) — tint at draw time.
    pub usb_icon_tex: &'a egui::TextureHandle,
    /// 3D renderer for this device's matched board, if any. `None` when the
    /// board ships no model or when the session-wide fallback latch has
    /// engaged because the live path can't sustain ≥24 fps.
    pub model: Option<SharedModel>,
    /// Pre-rendered static fallback image for the device's matched board.
    /// Used in place of the live 3D paint callback when `model` is `None`
    /// but the board ships a baked PNG.
    pub static_texture: Option<&'a egui::TextureHandle>,
    /// The matched board's own icon. Used when the board ships neither a
    /// live model nor a baked PNG.
    pub icon_texture: Option<&'a egui::TextureHandle>,
    /// Elapsed seconds since app start, for driving rotation animation.
    pub elapsed_secs: f32,
}

/// Common widget heights for the firmware row (refresh, dropdown, GH icon, browse).
/// Using a single height keeps them vertically aligned.
const ROW_WIDGET_H: f32 = 26.0;
/// Unified hit-area width for the three flat icons (refresh, GitHub, browse).
const ICON_BTN_W: f32 = 20.0;
/// Action-button (Blink, Update) widths for the bottom row.
const ACTION_BTN_W: f32 = 76.0;
const ACTION_BTN_H: f32 = 28.0;
/// Tint for the flat icon buttons in the firmware row (refresh, browse).
/// Matches the recolored external-link PNG so the row reads as one palette.
const ICON_BLUE: egui::Color32 = egui::Color32::from_rgb(60, 130, 220);
/// Hover tint — slightly brighter, like a hyperlink hover.
pub(crate) const ICON_BLUE_HOVER: egui::Color32 = egui::Color32::from_rgb(110, 170, 240);

pub fn draw(ui: &mut egui::Ui, ctx: &DrawContext) -> Action {
    let mut action = Action::None;

    let response = egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.set_width(CARD_WIDTH);

            // The parent (ui.horizontal_top in draw_device_row) lays children
            // out left-to-right, and Frame::show inherits that direction.
            // Force a vertical stack here so each section appears below the
            // previous one rather than to its right.
            ui.vertical(|ui| {
                // Centered identity stack above the image: heading line
                // (product and software version) and a small subline
                // (hardware version + serial). Both lines always render —
                // when no identity is known, the subline shows "—" so card
                // heights stay aligned across identified and unidentified
                // devices. The port name sits below the image in monospace.
                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                    draw_device_heading(ui, ctx.device.parsed.as_ref());
                    ui.add_space(2.0);
                    draw_device_subline(ui, ctx.device.parsed.as_ref());
                    ui.add_space(4.0);
                    draw_image_or_model(ui, ctx);
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(ctx.device.port_name())
                            .small()
                            .monospace()
                            .weak(),
                    );
                });

                ui.add_space(8.0);

                let row_action = draw_firmware_row(ui, ctx);
                if !matches!(row_action, Action::None) {
                    action = row_action;
                }

                ui.add_space(8.0);

                // Centered Identify + Update buttons on a single row.
                // ui.horizontal fills available width, so naive centering
                // via top_down(Center) doesn't work — instead we pre-allocate
                // a fixed-size strip just wide enough for the buttons.
                let gap = ui.spacing().item_spacing.x;
                let strip_w = if ctx.device.can_blink {
                    ACTION_BTN_W * 2.0 + gap
                } else {
                    ACTION_BTN_W
                };
                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                    ui.allocate_ui(egui::vec2(strip_w, ACTION_BTN_H), |ui| {
                        ui.horizontal(|ui| {
                            if ctx.device.can_blink {
                                let blink_btn = ui.add_enabled(
                                    !ctx.update_in_progress,
                                    egui::Button::new("Identify")
                                        .min_size(egui::vec2(ACTION_BTN_W, ACTION_BTN_H)),
                                );
                                if blink_btn.clicked() {
                                    action = Action::Blink(ctx.device.port_name().to_string());
                                }
                            }
                            let update_btn = ui.add_enabled(
                                !ctx.update_in_progress && ctx.has_firmware,
                                egui::Button::new("Update")
                                    .min_size(egui::vec2(ACTION_BTN_W, ACTION_BTN_H)),
                            );
                            if update_btn.clicked() {
                                action = Action::Update {
                                    port_name: ctx.device.port_name().to_string(),
                                    anchor: update_btn.rect,
                                };
                            }
                        });
                    });
                });
            });
        });

    if let Some(overlay) = &ctx.overlay {
        draw_overlay(ui, response.response.rect, overlay);
    }

    action
}

/// Heading line: `"{product} v{fw}"` when both are known, else just the
/// product name, else a weak "Unidentified" placeholder. Always size 15
/// and strong (or weak for the placeholder).
fn draw_device_heading(
    ui: &mut egui::Ui,
    parsed: Option<&crate::ui::identity::ParsedIdentity>,
) {
    match parsed {
        Some(p) => {
            let text = match p.fw_version.as_deref() {
                Some(fw) => format!("{} v{}", p.product, fw),
                None => p.product.clone(),
            };
            ui.label(egui::RichText::new(text).size(15.0).strong());
        }
        None => {
            ui.label(egui::RichText::new("Unidentified").size(15.0).weak());
        }
    }
}

/// Subline: small, weak `"hw v{hw}, serial {short_sn}"`. Either
/// component may be missing. When neither is known, render an em-dash
/// so the line still occupies vertical space and card heights stay aligned.
fn draw_device_subline(
    ui: &mut egui::Ui,
    parsed: Option<&crate::ui::identity::ParsedIdentity>,
) {
    let hw = parsed.and_then(|p| p.hw_version.as_deref());
    let sn = parsed.and_then(|p| p.serial_number.as_deref());

    let mut parts: Vec<String> = Vec::with_capacity(2);
    if let Some(h) = hw {
        parts.push(format!("hw v{}", h));
    }
    if let Some(s) = sn {
        parts.push(format!("serial {}", s));
    }
    let text = if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(", ")
    };
    ui.label(egui::RichText::new(text).small().weak());
}

/// Slow rotation: one full revolution every 16 seconds.
const MODEL_ANGULAR_SPEED: f32 = std::f32::consts::TAU / 16.0;

/// Per-card 3D paint callback. Bridges egui-wgpu's `prepare`/`paint`
/// trait into the model renderer's two-phase API: `prepare` writes
/// uniform buffers, `paint` records draw commands into egui's render
/// pass.
struct ModelPaintCb {
    model: SharedModel,
    angle: f32,
}

impl eframe::egui_wgpu::CallbackTrait for ModelPaintCb {
    fn prepare(
        &self,
        _device: &eframe::wgpu::Device,
        queue: &eframe::wgpu::Queue,
        screen: &eframe::egui_wgpu::ScreenDescriptor,
        _encoder: &mut eframe::wgpu::CommandEncoder,
        _resources: &mut eframe::egui_wgpu::CallbackResources,
    ) -> Vec<eframe::wgpu::CommandBuffer> {
        // `prepare` sees only the screen descriptor, not the callback rect,
        // so compute the aspect from the card image area — it matches what
        // `paint` will scissor to.
        let ppp = screen.pixels_per_point;
        let w = IMAGE_SIDE * ppp;
        let h = IMAGE_HEIGHT * ppp;
        self.model.prepare(queue, w, h, self.angle);
        Vec::new()
    }

    fn paint<'a>(
        &'a self,
        info: egui::PaintCallbackInfo,
        render_pass: &mut eframe::wgpu::RenderPass<'a>,
        _resources: &'a eframe::egui_wgpu::CallbackResources,
    ) {
        let v = info.viewport_in_pixels();
        let scissor = (
            v.left_px.max(0) as u32,
            v.top_px.max(0) as u32,
            v.width_px.max(0) as u32,
            v.height_px.max(0) as u32,
        );
        self.model.paint(render_pass, scissor);
    }
}

/// Render the device's image, falling back gracefully:
///   1. Live 3D paint callback when a `ModelRenderer` is available.
///   2. Pre-rendered static PNG when 3D is unavailable but the board ships
///      a baked image (e.g., wgpu setup failed, or the FPS guard latched
///      the session into static mode).
///   3. The board's own icon, for boards that ship one but no model.
///   4. USB-trident placeholder as the last resort.
fn draw_image_or_model(ui: &mut egui::Ui, ctx: &DrawContext) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(IMAGE_SIDE, IMAGE_HEIGHT),
        egui::Sense::hover(),
    );

    if let Some(model) = ctx.model.as_ref() {
        let cb = ModelPaintCb {
            model: model.clone(),
            angle: ctx.elapsed_secs * MODEL_ANGULAR_SPEED,
        };
        ui.painter().add(eframe::egui_wgpu::Callback::new_paint_callback(
            rect, cb,
        ));
        return;
    }

    if let Some(tex) = ctx.static_texture.or(ctx.icon_texture) {
        paint_fitted(ui, rect, tex);
        return;
    }

    draw_image_placeholder(ui, rect, ctx.usb_icon_tex);
}

/// Paint a texture into `rect`. Board images are square in spirit but the
/// card image area is slightly wider than tall (234×214), so fit within the
/// rect while preserving the source aspect rather than squashing.
fn paint_fitted(ui: &mut egui::Ui, rect: egui::Rect, tex: &egui::TextureHandle) {
    egui::Image::from_texture(tex)
        .fit_to_exact_size(rect.size())
        .maintain_aspect_ratio(true)
        .paint_at(ui, rect);
}

/// USB-TridentLogo.png is portrait — width / height of the source image.
const USB_ICON_ASPECT: f32 = 576.0 / 1200.0;

fn draw_image_placeholder(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    usb_icon_tex: &egui::TextureHandle,
) {
    ui.painter().rect(
        rect,
        6.0,
        ui.visuals().faint_bg_color,
        egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
    );

    // Fit the trident inside ~50% of the placeholder's smaller dimension,
    // preserving the source aspect ratio. The texture is white pixels;
    // tinting with weak_text_color matches the surrounding stroke.
    let target_h = rect.width().min(rect.height()) * 0.5;
    let target_w = target_h * USB_ICON_ASPECT;
    let icon_rect = egui::Rect::from_center_size(
        rect.center(),
        egui::vec2(target_w, target_h),
    );
    egui::Image::from_texture(usb_icon_tex)
        .tint(ui.visuals().weak_text_color())
        .paint_at(ui, icon_rect);
}

/// Flat, frameless icon button rendering a single glyph in `ICON_BLUE`
/// (or `ICON_BLUE_HOVER` while pointed at). Disabled state dims the
/// glyph and suppresses the hover tint, matching the surrounding row.
fn icon_glyph_button(
    ui: &mut egui::Ui,
    glyph: &str,
    enabled: bool,
    hover_text: &str,
) -> egui::Response {
    let resp = ui
        .add_enabled_ui(enabled, |ui| {
            ui.add_sized(
                [ICON_BTN_W, ROW_WIDGET_H],
                egui::Button::new("").frame(false),
            )
        })
        .inner;
    let color = if !enabled {
        ICON_BLUE.gamma_multiply(0.5)
    } else if resp.hovered() {
        ICON_BLUE_HOVER
    } else {
        ICON_BLUE
    };
    ui.painter().text(
        resp.rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(14.0),
        color,
    );
    resp.on_hover_text(hover_text)
}

/// Refresh button + firmware dropdown + github-link icon + browse icon,
/// in a single horizontal row inside the card. All elements share
/// `ROW_WIDGET_H` height for clean vertical alignment.
fn draw_firmware_row(ui: &mut egui::Ui, ctx: &DrawContext) -> Action {
    let mut action = Action::None;

    // Top-aligned row + every widget allocated into an identically-sized
    // cell. Without this, ComboBox's internal box-model differs slightly
    // from Button's and pixel rounding shifts the trigger 1pt lower than
    // the surrounding icon buttons. Refresh and browse use frame(false)
    // so they read as flat icons (matching the GitHub link), saving
    // horizontal space for the dropdown.
    ui.horizontal_top(|ui| {
        ui.spacing_mut().interact_size = egui::vec2(ICON_BTN_W, ROW_WIDGET_H);
        ui.spacing_mut().button_padding = egui::vec2(8.0, 6.0);
        ui.spacing_mut().item_spacing.x = 4.0;

        let refresh_resp = icon_glyph_button(
            ui,
            "⟳",
            !ctx.update_in_progress,
            "Refresh firmware list",
        );
        if refresh_resp.clicked() {
            // An unmatched device has no board to single out, so refresh all.
            action = Action::RefreshBoards(ctx.device.matched_board.clone());
        }

        // Dropdown — fills the remaining width left over by the icons.
        // Wrapped in a fixed-height cell so its trigger button aligns
        // with the icon buttons regardless of the ComboBox's internal
        // padding.
        let icons_total = ICON_BTN_W * 3.0 + ui.spacing().item_spacing.x * 3.0;
        let dropdown_w = (CARD_WIDTH - icons_total).max(80.0);
        let dropdown_action = ui
            .allocate_ui_with_layout(
                egui::vec2(dropdown_w, ROW_WIDGET_H),
                egui::Layout::top_down(egui::Align::Min),
                |ui| draw_dropdown(ui, ctx, dropdown_w),
            )
            .inner;
        if !matches!(dropdown_action, Action::None) {
            action = dropdown_action;
        }

        // Two pre-tinted textures (normal blue + brighter hover blue) swap
        // on hover, matching the hyperlink-hover feel of the surrounding
        // glyph buttons — Image::tint can't brighten a dark source.
        if let Some(url) = github_url_for_device(ctx) {
            let click_resp = ui.add_sized(
                [ICON_BTN_W, ROW_WIDGET_H],
                egui::Button::new("").frame(false),
            );
            let tex = if click_resp.hovered() {
                ctx.external_link_hover_tex
            } else {
                ctx.external_link_tex
            };
            let img_rect = egui::Rect::from_center_size(
                click_resp.rect.center(),
                egui::vec2(16.0, 16.0),
            );
            egui::Image::from_texture(tex)
                .fit_to_exact_size(egui::vec2(16.0, 16.0))
                .paint_at(ui, img_rect);
            let click_resp = click_resp.on_hover_text("View releases on GitHub");
            if click_resp.clicked() {
                action = Action::OpenGithub(url);
            }
        }

        let browse_resp = icon_glyph_button(
            ui,
            "🔍",
            !ctx.update_in_progress,
            "Browse for local UF2 file",
        );
        if browse_resp.clicked() {
            action = Action::BrowseLocal(ctx.device.port_name().to_string());
        }
    });

    action
}

/// Truncate a string to at most `max_chars` characters, appending "…"
/// if truncated. Used to keep the ComboBox trigger from overflowing.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Pick a GitHub releases URL to link to. Falls back through: matched board
/// → selected release's board → first registered board, so the icon stays
/// visible (the icon row reserves its width unconditionally).
fn github_url_for_device(ctx: &DrawContext) -> Option<String> {
    let board_id = match (&ctx.device.matched_board, &ctx.device.selection) {
        (Some(id), _) => Some(id.as_str()),
        (None, FirmwareSelection::Release { board_id, .. }) => Some(board_id.as_str()),
        _ => None,
    };
    let board = board_id
        .and_then(|id| ctx.registry.get(id))
        .or_else(|| ctx.registry.boards().first())?;
    Some(board.releases_page_url())
}

/// Dropdown showing either the matched board's releases or, when
/// unmatched, every board's releases grouped by disabled section header.
fn draw_dropdown(ui: &mut egui::Ui, ctx: &DrawContext, width: f32) -> Action {
    let mut action = Action::None;
    let device = ctx.device;

    let selected_text = match &device.selection {
        FirmwareSelection::Local(path) => format!(
            "📁 {}",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        ),
        FirmwareSelection::Release {
            board_id,
            release_idx,
        } => match ctx.library.get(board_id) {
            Some(BoardReleases::Loaded(rs)) => rs
                .get(*release_idx)
                .map(|r| r.display_label())
                .unwrap_or_else(|| "(invalid)".to_string()),
            _ => "(loading…)".to_string(),
        },
        FirmwareSelection::Auto => match &device.matched_board {
            Some(id) => match ctx.library.get(id) {
                Some(BoardReleases::Loading) => "Loading…".to_string(),
                Some(BoardReleases::Loaded(rs)) => rs
                    .first()
                    .map(|r| r.display_label())
                    .unwrap_or_else(|| "(no releases)".to_string()),
                Some(BoardReleases::Failed(e)) => format!("⚠ {}", short_err(e)),
                None => "—".to_string(),
            },
            None => "Pick firmware…".to_string(),
        },
    };

    let combo_id = format!("fw-combo-{}", device.port_name());

    // Truncate the trigger text so it doesn't push the icons past the
    // card edge. The full text is still visible in the popup list.
    let display_text = truncate_chars(&selected_text, 22);

    egui::ComboBox::from_id_source(combo_id)
        // Render selected_text at the same size as the icon glyphs so the
        // ComboBox trigger has the same content height as the icon buttons.
        .selected_text(egui::RichText::new(display_text).size(14.0))
        .width(width)
        .show_ui(ui, |ui| {
            if matches!(device.selection, FirmwareSelection::Local(_)) {
                if ui.selectable_label(false, "✕ Clear local file").clicked() {
                    action = Action::ClearLocal(device.port_name().to_string());
                }
                ui.separator();
            }

            match &device.matched_board {
                Some(matched_id) => {
                    draw_board_entries(ui, ctx, matched_id, false, &mut action);
                }
                None => {
                    for (i, board) in ctx.registry.boards().iter().enumerate() {
                        if i > 0 {
                            ui.separator();
                        }
                        draw_board_entries(ui, ctx, &board.id, true, &mut action);
                    }
                }
            }
        });

    action
}

fn draw_board_entries(
    ui: &mut egui::Ui,
    ctx: &DrawContext,
    board_id: &str,
    show_header: bool,
    action: &mut Action,
) {
    let Some(board) = ctx.registry.get(board_id) else {
        return;
    };

    if show_header {
        ui.add_enabled(
            false,
            egui::Label::new(
                egui::RichText::new(format!("— {} —", board.manifest.display_name))
                    .small()
                    .strong(),
            ),
        );
    }

    match ctx.library.get(board_id) {
        Some(BoardReleases::Loading) => {
            ui.add_enabled(false, egui::Label::new("Loading…"));
        }
        Some(BoardReleases::Failed(e)) => {
            ui.add_enabled(
                false,
                egui::Label::new(
                    egui::RichText::new(format!("⚠ {}", short_err(e)))
                        .small()
                        .color(egui::Color32::LIGHT_RED),
                ),
            );
        }
        Some(BoardReleases::Loaded(releases)) => {
            for (i, r) in releases.iter().enumerate() {
                let selected = matches!(
                    &ctx.device.selection,
                    FirmwareSelection::Release { board_id: bid, release_idx } if bid == board_id && *release_idx == i
                );
                if ui.selectable_label(selected, r.display_label()).clicked() {
                    *action = Action::SelectRelease {
                        port_name: ctx.device.port_name().to_string(),
                        board_id: board_id.to_string(),
                        idx: i,
                    };
                }
            }
        }
        None => {
            ui.add_enabled(false, egui::Label::new("(not loaded)"));
        }
    }
}

/// Truncate to ~40 characters for use inside the dropdown popup.
fn short_err(s: &str) -> String {
    truncate_chars(s, 40)
}

fn draw_overlay(ui: &mut egui::Ui, rect: egui::Rect, overlay: &Overlay) {
    let painter = ui.painter();
    painter.rect_filled(rect, 6.0, egui::Color32::from_black_alpha(180));

    let (text, color) = match overlay {
        Overlay::Running(phase) => (phase_label(phase).to_string(), egui::Color32::WHITE),
        Overlay::Done(Ok(())) => ("Update complete ✔".to_string(), egui::Color32::LIGHT_GREEN),
        Overlay::Done(Err(msg)) => (format!("Failed: {}", msg), egui::Color32::LIGHT_RED),
    };

    // Lay out with explicit wrap so long error messages (e.g. "could not
    // open serial port: resource busy ...") wrap inside the card instead
    // of clipping off both edges.
    let wrap_width = (rect.width() - 24.0).max(40.0);
    let galley = painter.layout(text, egui::FontId::proportional(13.0), color, wrap_width);
    let pos = rect.center() - galley.size() / 2.0;
    painter.galley(pos, galley, color);
}

fn phase_label(phase: &UpdatePhase) -> &'static str {
    match phase {
        UpdatePhase::Downloading => "Downloading firmware…",
        UpdatePhase::EnteringBootsel => "Entering bootloader…",
        UpdatePhase::WaitingForVolume => "Waiting for bootloader…",
        UpdatePhase::Flashing => "Flashing…",
        UpdatePhase::Verifying => "Waiting for device…",
    }
}

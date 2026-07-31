use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::boards::Registry;
use crate::device::DeviceInfo;
use crate::ui::board_assets;
use crate::ui::github::Release;
use crate::ui::model::{ModelRenderer, SharedModel};
use crate::ui::workers::{self, Event, FirmwareSource, UpdatePhase};
use crate::ui::{card, desired_inner_width, CARD_WIDTH};

/// Per-board, the release-list state maintained by the App.
#[derive(Clone, Debug)]
pub enum BoardReleases {
    Loading,
    Loaded(Vec<Release>),
    Failed(String),
}

/// Releases for every registered board, keyed by board id.
#[derive(Default, Debug)]
pub struct ReleaseLibrary {
    by_board: HashMap<String, BoardReleases>,
}

impl ReleaseLibrary {
    pub fn get(&self, board_id: &str) -> Option<&BoardReleases> {
        self.by_board.get(board_id)
    }
    pub fn set(&mut self, board_id: String, state: BoardReleases) {
        self.by_board.insert(board_id, state);
    }
}

/// Per-device firmware selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FirmwareSelection {
    /// Default: no explicit pick. UI auto-selects newest release of the
    /// matched project (if any).
    #[default]
    Auto,
    /// User picked a specific release from the dropdown.
    Release { board_id: String, release_idx: usize },
    /// User picked a local UF2 file.
    Local(PathBuf),
}

#[derive(Clone, Debug)]
pub struct DeviceCard {
    pub info: DeviceInfo,
    /// Raw IDENTIFY response (post `ID:` prefix). None when no response yet.
    pub identity: Option<String>,
    /// Parsed structured form of `identity`. None for unidentified devices.
    pub parsed: Option<crate::ui::identity::ParsedIdentity>,
    /// Whether the device responded to IDENTIFY (we can BLINK it).
    pub can_blink: bool,
    /// Id of the board whose identity prefix matched this device's IDENTIFY
    /// response. Set whenever identity changes.
    pub matched_board: Option<String>,
    pub selection: FirmwareSelection,
    /// True when the IDENTIFY probe got EACCES opening the port. Linux
    /// first-run state — clears once the udev rule is installed and the
    /// device replugged. Always false on macOS/Windows.
    pub permission_denied: bool,
}

impl DeviceCard {
    pub fn port_name(&self) -> &str {
        &self.info.port_name
    }
}

pub enum UpdateState {
    Idle,
    /// Confirmation popover is open, anchored to the Update button at `anchor`.
    Confirming {
        port_name: String,
        anchor: egui::Rect,
    },
    Running { port_name: String, phase: UpdatePhase },
    Done { port_name: String, result: Result<(), String>, since: Instant },
}

/// How long the success/failure overlay stays on a card before reverting.
const DONE_DISPLAY_DURATION: Duration = Duration::from_secs(3);

pub struct App {
    registry: Arc<Registry>,
    devices: Vec<DeviceCard>,
    library: ReleaseLibrary,
    update: UpdateState,
    rx: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
    last_sized_count: Option<usize>,
    /// Recolored external-link icon, cached as a texture handle.
    external_link_tex: egui::TextureHandle,
    /// Brighter "hyperlink hover" variant for hover state feedback.
    external_link_hover_tex: egui::TextureHandle,
    /// USB trident silhouette, recolored to white so callers can apply
    /// a runtime tint via `Image::tint`.
    usb_icon_tex: egui::TextureHandle,
    /// Lit 3D model per board (keyed by `board.id`). Populated for boards
    /// that ship a model.obj AND when the wgpu backend is available.
    models: HashMap<String, SharedModel>,
    /// Pre-rendered static fallback texture per board. Used in place of the
    /// live 3D paint callback when the live path either fails to initialize
    /// or can't sustain the frame-rate target.
    static_textures: HashMap<String, egui::TextureHandle>,
    /// Board icon per board, for boards that ship an icon.png. Shown when
    /// neither the live model nor the baked PNG is available.
    board_icons: HashMap<String, egui::TextureHandle>,
    /// Wall-clock origin for model rotation animation.
    start_time: Instant,
    /// Rolling-average FPS guard: when sustained CPU/GPU pressure makes
    /// the live rotation drop below 24 fps, latch into static fallback
    /// for the rest of the session.
    frame_intervals_ns: std::collections::VecDeque<u64>,
    last_frame_time: Option<Instant>,
    slow_frame_streak: u32,
    use_static_fallback: bool,
}

/// Load a pre-rendered RGBA PNG (e.g. a baked static fallback image
/// from `bake-static-images`) into an egui texture, preserving alpha so
/// it composites cleanly over the card. Returns `None` on decode error
/// — the caller treats that as a missing fallback and degrades further.
fn load_png_texture(
    ctx: &egui::Context,
    bytes: &[u8],
    name: &str,
) -> Option<egui::TextureHandle> {
    let img = match image::load_from_memory(bytes) {
        Ok(i) => i,
        Err(e) => {
            log::warn!("failed to decode static fallback '{}': {}", name, e);
            return None;
        }
    };
    let (w, h) = (img.width() as usize, img.height() as usize);
    let rgba = img.into_rgba8().into_raw();
    let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
    Some(ctx.load_texture(format!("static-{name}"), color_image, egui::TextureOptions::LINEAR))
}

/// Load a PNG, replace every opaque pixel's RGB with `target_rgb`, and
/// upload as an egui texture. Used because the bundled icon is black-on-
/// transparent — `Image::tint` multiplies, so it can't tint black to blue.
fn load_recolored_icon(
    ctx: &egui::Context,
    bytes: &[u8],
    target_rgb: [u8; 3],
    name: &str,
) -> egui::TextureHandle {
    let img = image::load_from_memory(bytes).expect("icon decode");
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut rgba = img.into_rgba8().into_raw();
    for chunk in rgba.chunks_exact_mut(4) {
        if chunk[3] > 0 {
            chunk[0] = target_rgb[0];
            chunk[1] = target_rgb[1];
            chunk[2] = target_rgb[2];
        }
    }
    let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
    ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR)
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let registry = Arc::new(Registry::load());
        let (tx, rx) = mpsc::channel();

        workers::spawn_discovery(
            registry.clone(),
            tx.clone(),
            ctx.clone(),
            Duration::from_secs(1),
        );

        let mut library = ReleaseLibrary::default();
        for board in registry.boards() {
            library.set(board.id.clone(), BoardReleases::Loading);
            workers::spawn_fetch_releases(board.clone(), tx.clone(), ctx.clone());
        }

        let external_link_tex = load_recolored_icon(
            &ctx,
            include_bytes!("resources/external_link.png"),
            [60, 130, 220],
            "external_link_blue",
        );
        let external_link_hover_tex = load_recolored_icon(
            &ctx,
            include_bytes!("resources/external_link.png"),
            [110, 170, 240],
            "external_link_blue_hover",
        );
        let usb_icon_tex = load_recolored_icon(
            &ctx,
            include_bytes!("resources/USB-TridentLogo.png"),
            [255, 255, 255],
            "usb_icon_white",
        );

        // Build a 3D model renderer + load the static fallback and icon
        // textures per board that ships them. Any of them may be absent or
        // fail (no GPU, shader compile error, etc.) — the card falls back
        // through: live 3D → baked PNG → board icon → USB placeholder.
        let mut models: HashMap<String, SharedModel> = HashMap::new();
        let mut static_textures: HashMap<String, egui::TextureHandle> = HashMap::new();
        let mut board_icons: HashMap<String, egui::TextureHandle> = HashMap::new();
        let depth_format = eframe::wgpu::TextureFormat::Depth32Float;
        for board in registry.boards() {
            let assets = board_assets::load(board);

            if let (Some(obj), Some(rs)) =
                (assets.model_obj.as_deref(), cc.wgpu_render_state.as_ref())
            {
                // An absent MTL is fine: from_obj_bytes synthesizes one from
                // the OBJ's own material declarations.
                match ModelRenderer::from_obj_bytes(
                    &rs.device,
                    rs.target_format,
                    depth_format,
                    obj,
                    assets.model_mtl.as_deref().unwrap_or(&[]),
                ) {
                    Ok(r) => {
                        models.insert(board.id.clone(), Arc::new(r));
                    }
                    Err(e) => log::error!(
                        "failed to build model renderer for '{}': {} (will use static fallback)",
                        board.id,
                        e
                    ),
                }
            }
            if let Some(png) = assets.model_png.as_deref() {
                if let Some(tex) = load_png_texture(&ctx, png, &board.id) {
                    static_textures.insert(board.id.clone(), tex);
                }
            }
            if let Some(png) = assets.icon_png.as_deref() {
                if let Some(tex) = load_png_texture(&ctx, png, &format!("{}-icon", board.id)) {
                    board_icons.insert(board.id.clone(), tex);
                }
            }
        }

        // Latch into static fallback at startup if either:
        //   - wgpu setup failed for every board that has a baked PNG
        //     (so the card can still show the device, just not animated),
        //   - or the user opts in via `NEWERGLOW_FORCE_FALLBACK=1`,
        //     which is useful for testing the fallback path on a machine
        //     where the live renderer happens to be fast enough.
        let force_fallback = std::env::var("NEWERGLOW_FORCE_FALLBACK")
            .is_ok_and(|v| v == "1");
        let use_static_fallback =
            force_fallback || (models.is_empty() && !static_textures.is_empty());

        Self {
            registry,
            devices: Vec::new(),
            library,
            update: UpdateState::Idle,
            rx,
            tx,
            ctx,
            last_sized_count: None,
            external_link_tex,
            external_link_hover_tex,
            usb_icon_tex,
            models,
            static_textures,
            board_icons,
            start_time: Instant::now(),
            frame_intervals_ns: std::collections::VecDeque::with_capacity(64),
            last_frame_time: None,
            slow_frame_streak: 0,
            use_static_fallback,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::DevicesChanged(infos) => self.merge_devices(infos),
                Event::IdentityResolved {
                    port_name,
                    identity,
                    permission_denied,
                } => {
                    let matched = identity.as_deref().and_then(|s| self.match_board(s));
                    let parsed = identity
                        .as_deref()
                        .map(crate::ui::identity::ParsedIdentity::parse);

                    if let Some(card) = self
                        .devices
                        .iter_mut()
                        .find(|c| c.port_name() == port_name)
                    {
                        card.can_blink = identity.is_some();
                        card.identity = identity;
                        card.parsed = parsed;
                        card.matched_board = matched;
                        card.permission_denied = permission_denied;
                    }
                }
                Event::ReleasesLoaded { board_id, releases } => {
                    let state = if releases.is_empty() {
                        BoardReleases::Failed("no matching releases".to_string())
                    } else {
                        BoardReleases::Loaded(releases)
                    };
                    self.library.set(board_id, state);
                }
                Event::ReleasesFailed { board_id, error } => {
                    self.library.set(board_id, BoardReleases::Failed(error));
                }
                Event::UpdateProgress(phase) => {
                    if let UpdateState::Running { port_name, .. } = &self.update {
                        let port_name = port_name.clone();
                        self.update = UpdateState::Running { port_name, phase };
                    }
                }
                Event::UpdateComplete(result) => {
                    if let UpdateState::Running { port_name, .. } = &self.update {
                        let port_name = port_name.clone();
                        self.update = UpdateState::Done {
                            port_name,
                            result,
                            since: Instant::now(),
                        };
                    }
                }
            }
        }
    }

    fn match_board(&self, identity: &str) -> Option<String> {
        self.registry.match_identity(identity).map(|b| b.id.clone())
    }

    fn merge_devices(&mut self, infos: Vec<DeviceInfo>) {
        let pinned_port: Option<String> = match &self.update {
            UpdateState::Running { port_name, .. } => Some(port_name.clone()),
            _ => None,
        };
        let pinned_card: Option<DeviceCard> = pinned_port.as_ref().and_then(|p| {
            self.devices.iter().find(|c| c.port_name() == p).cloned()
        });

        let mut new_devices: Vec<DeviceCard> = Vec::with_capacity(infos.len() + 1);
        let mut pinned_seen = false;

        for info in infos {
            let prior = self.devices.iter().find(|c| c.port_name() == info.port_name);
            let (identity, parsed, can_blink, matched_board, selection, permission_denied) =
                match prior {
                    Some(p) => (
                        p.identity.clone(),
                        p.parsed.clone(),
                        p.can_blink,
                        p.matched_board.clone(),
                        p.selection.clone(),
                        p.permission_denied,
                    ),
                    None => (None, None, false, None, FirmwareSelection::Auto, false),
                };

            if pinned_port.as_deref() == Some(&info.port_name) {
                pinned_seen = true;
            }

            new_devices.push(DeviceCard {
                info,
                identity,
                parsed,
                can_blink,
                matched_board,
                selection,
                permission_denied,
            });
        }

        if let (false, Some(card)) = (pinned_seen, pinned_card) {
            let prior_idx = self
                .devices
                .iter()
                .position(|c| c.port_name() == card.port_name());
            match prior_idx {
                Some(i) if i <= new_devices.len() => new_devices.insert(i, card),
                _ => new_devices.push(card),
            }
        }

        self.devices = new_devices;
    }

    /// Refresh releases for one board (or all if `board_id` is None).
    fn refresh_releases(&mut self, board_id: Option<&str>) {
        let to_refresh: Vec<_> = self
            .registry
            .boards()
            .iter()
            .filter(|b| board_id.map(|id| id == b.id).unwrap_or(true))
            .cloned()
            .collect();

        for board in to_refresh {
            self.library.set(board.id.clone(), BoardReleases::Loading);
            workers::spawn_fetch_releases(board, self.tx.clone(), self.ctx.clone());
        }
    }

    /// Resolve the firmware source for a device, using its selection or
    /// (for `Auto`) the newest release of its matched board.
    fn firmware_source_for(
        &self,
        device: &DeviceCard,
    ) -> Option<(FirmwareSource, String)> {
        match &device.selection {
            FirmwareSelection::Local(path) => {
                let label = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                Some((FirmwareSource::Local(path.clone()), label))
            }
            FirmwareSelection::Release {
                board_id,
                release_idx,
            } => self.source_from_release(board_id, *release_idx),
            FirmwareSelection::Auto => {
                let board_id = device.matched_board.as_deref()?;
                self.source_from_release(board_id, 0)
            }
        }
    }

    fn source_from_release(
        &self,
        board_id: &str,
        idx: usize,
    ) -> Option<(FirmwareSource, String)> {
        let fw = &self.registry.get(board_id)?.manifest.firmware;
        let releases = match self.library.get(board_id)? {
            BoardReleases::Loaded(rs) => rs,
            _ => return None,
        };
        let release = releases.get(idx)?;
        let asset = release.matching_asset(&fw.asset_pattern)?;
        Some((
            FirmwareSource::Remote {
                owner: fw.github_owner.clone(),
                repo: fw.github_repo.clone(),
                tag: release.tag_name.clone(),
                url: asset.browser_download_url.clone(),
                size: asset.size,
                digest: asset.digest.clone(),
            },
            release.display_label(),
        ))
    }

    fn update_in_progress(&self) -> bool {
        matches!(
            self.update,
            UpdateState::Confirming { .. } | UpdateState::Running { .. }
        )
    }

    fn maybe_dismiss_done(&mut self) {
        if let UpdateState::Done { since, .. } = &self.update {
            if since.elapsed() >= DONE_DISPLAY_DURATION {
                self.update = UpdateState::Idle;
            }
        }
    }

    fn maybe_autosize_window(&mut self, ctx: &egui::Context) {
        let n = self.devices.len();
        if self.last_sized_count == Some(n) {
            return;
        }
        let target_w = desired_inner_width(n);
        let current = ctx.input(|i| i.viewport().inner_rect);
        let height = current.map(|r| r.height()).unwrap_or(crate::ui::WINDOW_HEIGHT);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            target_w, height,
        )));
        self.last_sized_count = Some(n);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.maybe_dismiss_done();
        self.maybe_autosize_window(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            self.draw_device_row(ui);
        });
        self.draw_modal(ctx);

        // Only schedule continuous repaints while the Done overlay is on
        // screen — its auto-dismiss timer needs ticks to fire. All other
        // state changes are driven by user input or worker events (both of
        // which trigger their own repaints), so we can stay idle otherwise.
        if matches!(self.update, UpdateState::Done { .. }) {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        if self.any_visible_live_model() {
            // Aim for 60 fps consistently. The 16 ms floor keeps eframe's
            // loop in WaitUntil mode (vs the busy-Poll mode triggered by
            // immediate request_repaint()), and AutoVsync caps actual
            // presents to the display refresh anyway.
            ctx.request_repaint_after(Duration::from_millis(16));
            self.observe_frame_interval();
        }
    }
}

impl App {
    /// Track frame intervals while the live 3D path is active. After a
    /// short warmup, if the rolling 60-frame average drops under 24 fps
    /// for 90 consecutive frames (~1.5 s), latch into static fallback
    /// for the rest of the session and stop driving the repaint timer.
    fn observe_frame_interval(&mut self) {
        const WINDOW: usize = 60;
        const WARMUP: usize = 30;
        const SLOW_THRESHOLD_FPS: f32 = 24.0;
        const SLOW_STREAK_TO_LATCH: u32 = 90;

        let now = Instant::now();
        if let Some(prev) = self.last_frame_time.replace(now) {
            let interval = now.duration_since(prev).as_nanos() as u64;
            self.frame_intervals_ns.push_back(interval);
            if self.frame_intervals_ns.len() > WINDOW {
                self.frame_intervals_ns.pop_front();
            }
        } else {
            return;
        }

        if self.frame_intervals_ns.len() < WARMUP {
            return;
        }
        let total_ns: u64 = self.frame_intervals_ns.iter().sum();
        let avg_ns = total_ns / self.frame_intervals_ns.len() as u64;
        let avg_fps = if avg_ns == 0 {
            f32::INFINITY
        } else {
            1_000_000_000.0 / avg_ns as f32
        };

        if avg_fps < SLOW_THRESHOLD_FPS {
            self.slow_frame_streak += 1;
            if self.slow_frame_streak >= SLOW_STREAK_TO_LATCH && !self.use_static_fallback {
                log::info!(
                    "live 3D below {} fps for {} frames (avg {:.1}); switching to static fallback",
                    SLOW_THRESHOLD_FPS,
                    self.slow_frame_streak,
                    avg_fps,
                );
                self.use_static_fallback = true;
            }
        } else {
            self.slow_frame_streak = 0;
        }
    }
}

impl App {
    /// True if at least one currently-rendered card is showing the
    /// live 3D paint callback (i.e. has a SharedModel and the
    /// session-wide fallback latch hasn't engaged). Used to gate the
    /// rotation-animation repaint timer.
    fn any_visible_live_model(&self) -> bool {
        if self.use_static_fallback {
            return false;
        }
        self.devices.iter().any(|d| {
            d.matched_board
                .as_deref()
                .map(|bid| self.models.contains_key(bid))
                .unwrap_or(false)
        })
    }
}

impl App {
    fn draw_device_row(&mut self, ui: &mut egui::Ui) {
        self.draw_authorize_banner(ui);

        if self.devices.is_empty() {
            self.draw_empty_state(ui);
            return;
        }

        let update_in_progress = self.update_in_progress();

        let updating_port: Option<String> = match &self.update {
            UpdateState::Running { port_name, .. } | UpdateState::Done { port_name, .. } => {
                Some(port_name.clone())
            }
            _ => None,
        };

        let mut action = card::Action::None;

        // ScrollArea drives a scrollbar fade animation that fires immediate
        // request_repaint() calls, which keep eframe's loop in
        // ControlFlow::Poll between vsyncs (busy-spinning, ~100% CPU). When
        // every card fits in the autosized window we skip ScrollArea entirely
        // and use a plain horizontal layout. Only the rare overflow case
        // (more devices than MAX_VISIBLE_CARDS) keeps the ScrollArea path.
        let needs_scroll = (self.devices.len() as f32) > crate::ui::MAX_VISIBLE_CARDS;
        if needs_scroll {
            egui::ScrollArea::horizontal()
                .auto_shrink([false; 2])
                .show(ui, |ui| self.draw_cards(ui, &mut action, &updating_port, update_in_progress));
        } else {
            self.draw_cards(ui, &mut action, &updating_port, update_in_progress);
        }

        match action {
            card::Action::Blink(port) => workers::spawn_blink(port),
            card::Action::Update { port_name, anchor } if !self.update_in_progress() => {
                let has_source = self
                    .devices
                    .iter()
                    .find(|d| d.port_name() == port_name)
                    .and_then(|d| self.firmware_source_for(d))
                    .is_some();
                if has_source {
                    self.update = UpdateState::Confirming { port_name, anchor };
                }
            }
            card::Action::OpenGithub(url) => {
                self.ctx.open_url(egui::OpenUrl::same_tab(url));
            }
            card::Action::RefreshBoards(board_id) => {
                self.refresh_releases(board_id.as_deref());
            }
            card::Action::BrowseLocal(port) => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("UF2 firmware", &["uf2"])
                    .pick_file()
                {
                    if let Some(d) =
                        self.devices.iter_mut().find(|d| d.port_name() == port)
                    {
                        d.selection = FirmwareSelection::Local(path);
                    }
                }
            }
            _ => {}
        }
    }

    /// Lay out the device cards horizontally and dispatch any per-card
    /// actions. Called from `draw_device_row` either directly (when all
    /// cards fit) or wrapped in a `ScrollArea` (when they don't).
    fn draw_cards(
        &mut self,
        ui: &mut egui::Ui,
        action: &mut card::Action,
        updating_port: &Option<String>,
        update_in_progress: bool,
    ) {
        ui.horizontal_top(|ui| {
            let count = self.devices.len();
            let elapsed_secs = self.start_time.elapsed().as_secs_f32();
            let in_fallback = self.use_static_fallback;
            let registry = self.registry.clone();
            for i in 0..count {
                let overlay = self.overlay_for(i, updating_port);
                let bid = self.devices[i].matched_board.as_deref();
                // When the session-wide fallback latch is engaged, never
                // hand a live model to the card — it'll fall through to
                // the static texture (if any) or the USB placeholder.
                let model = if in_fallback {
                    None
                } else {
                    bid.and_then(|b| self.models.get(b)).cloned()
                };
                let static_texture = bid.and_then(|b| self.static_textures.get(b));
                let icon_texture = bid.and_then(|b| self.board_icons.get(b));
                let has_firmware = self.firmware_source_for(&self.devices[i]).is_some();
                let context = card::DrawContext {
                    device: &self.devices[i],
                    update_in_progress,
                    has_firmware,
                    overlay,
                    library: &self.library,
                    registry: &registry,
                    external_link_tex: &self.external_link_tex,
                    external_link_hover_tex: &self.external_link_hover_tex,
                    usb_icon_tex: &self.usb_icon_tex,
                    model,
                    static_texture,
                    icon_texture,
                    elapsed_secs,
                };
                let a = card::draw(ui, &context);
                match a {
                    card::Action::None => {}
                    card::Action::Blink(_)
                    | card::Action::Update { .. }
                    | card::Action::OpenGithub(_)
                    | card::Action::RefreshBoards(_)
                    | card::Action::BrowseLocal(_) => {
                        *action = a;
                    }
                    card::Action::SelectRelease {
                        ref port_name,
                        ref board_id,
                        idx,
                    } => {
                        if let Some(d) =
                            self.devices.iter_mut().find(|d| d.port_name() == port_name)
                        {
                            d.selection = FirmwareSelection::Release {
                                board_id: board_id.clone(),
                                release_idx: idx,
                            };
                        }
                    }
                    card::Action::ClearLocal(ref port_name) => {
                        if let Some(d) =
                            self.devices.iter_mut().find(|d| d.port_name() == port_name)
                        {
                            d.selection = FirmwareSelection::Auto;
                        }
                    }
                }
            }
        });
    }

    fn overlay_for(&self, idx: usize, updating_port: &Option<String>) -> Option<card::Overlay> {
        let device = &self.devices[idx];
        if updating_port.as_deref() != Some(device.port_name()) {
            return None;
        }
        match &self.update {
            UpdateState::Running { phase, .. } => Some(card::Overlay::Running(phase.clone())),
            UpdateState::Done { result, .. } => Some(card::Overlay::Done(result.clone())),
            _ => None,
        }
    }

    /// Render a slim "Authorize" banner when any device hit EACCES on
    /// IDENTIFY *and* the udev rule isn't already installed on disk.
    /// The disk check makes the banner self-hide after a successful
    /// install even if the device hasn't been replugged yet.
    fn draw_authorize_banner(&mut self, ui: &mut egui::Ui) {
        let any_denied = self.devices.iter().any(|d| d.permission_denied);
        if !any_denied {
            return;
        }
        if crate::install_udev::rule_file_installed(&self.registry.usb_vids()) {
            return;
        }

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(10.0, 4.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("USB device access not configured.")
                            .size(12.0)
                            .weak(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(egui::RichText::new("Authorize").size(12.0))
                            .clicked()
                        {
                            workers::spawn_install_udev(self.registry.clone(), self.tx.clone(), self.ctx.clone());
                        }
                    });
                });
            });
        ui.add_space(4.0);
    }

    fn draw_empty_state(&self, ui: &mut egui::Ui) {
        // Match a real card's outer footprint: same Frame::group, same
        // inner_margin, same content width, stretched to the central
        // panel's full available height. The two messages are vertically
        // centered inside that area by padding above with half the
        // remaining space.
        let avail_h = ui.available_height();
        let inner_margin = 8.0;
        let inner_h = (avail_h - 2.0 * inner_margin).max(0.0);
        // Approximate height of the two stacked labels (font size + leading
        // + explicit add_space). Used only to compute the top pad so the
        // group reads as vertically centered within the card.
        let content_h = 17.0 + 6.0 + 14.0;
        let top_pad = ((inner_h - content_h) / 2.0).max(0.0);
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(inner_margin))
            .show(ui, |ui| {
                ui.set_width(CARD_WIDTH);
                ui.set_min_height(inner_h);
                ui.vertical_centered(|ui| {
                    ui.add_space(top_pad);
                    ui.label(
                        egui::RichText::new("No devices found")
                            .size(14.0)
                            .weak(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Plug in a device via USB")
                            .size(11.0)
                            .weak(),
                    );
                });
            });
    }

    /// In-viewport confirmation popover anchored above the Update button.
    /// Renders a dim overlay over the rest of the UI plus a balloon with a
    /// downward arrow containing a single blue "Confirm Update" button.
    /// Cancellation: click outside, press Escape.
    fn draw_modal(&mut self, ctx: &egui::Context) {
        let UpdateState::Confirming { port_name, anchor } = &self.update else {
            return;
        };
        let port_name = port_name.clone();
        let anchor = *anchor;

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.update = UpdateState::Idle;
            return;
        }

        let mut clicked_confirm = false;
        let mut clicked_outside = false;

        // 1. Dim overlay covering the whole viewport. Catches any click
        //    that doesn't land on the popover (interpreted as cancel).
        let screen_rect = ctx.screen_rect();
        let dim_response = egui::Area::new(egui::Id::new("popover-dim"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen_rect.min)
            .show(ctx, |ui| {
                ui.painter().rect_filled(
                    screen_rect,
                    0.0,
                    egui::Color32::from_black_alpha(140),
                );
                ui.interact(
                    screen_rect,
                    ui.id().with("dim-click"),
                    egui::Sense::click(),
                )
            });
        if dim_response.inner.clicked() {
            clicked_outside = true;
        }

        // 2. Popover content (rounded rect + downward arrow + blue button).
        //    Higher Z-order than the dim, so its hit areas win for clicks
        //    inside the popover bounds.
        const POPOVER_W: f32 = 170.0;
        const POPOVER_H: f32 = 44.0;
        const ARROW_H: f32 = 8.0;
        const ARROW_HALF_W: f32 = 8.0;
        const ARROW_GAP: f32 = 2.0; // gap between arrow tip and button top

        let popover_rect = egui::Rect::from_min_size(
            egui::pos2(
                anchor.center().x - POPOVER_W / 2.0,
                anchor.top() - POPOVER_H - ARROW_H - ARROW_GAP,
            ),
            egui::vec2(POPOVER_W, POPOVER_H),
        );

        let bg_color = ctx.style().visuals.window_fill;

        egui::Area::new(egui::Id::new("popover-content"))
            .order(egui::Order::Tooltip)
            .fixed_pos(popover_rect.min)
            .show(ctx, |ui| {
                // Capture clicks anywhere within the popover or arrow area
                // so they don't fall through to the dim. Drawn first so the
                // button below shadows it for actual button clicks.
                let capture_rect = popover_rect.expand2(egui::vec2(0.0, ARROW_H));
                let _ = ui.interact(
                    capture_rect,
                    ui.id().with("popover-bg"),
                    egui::Sense::click(),
                );

                let painter = ui.painter();
                painter.rect_filled(popover_rect, 8.0, bg_color);

                let cx = anchor.center().x;
                let arrow_tip = egui::pos2(cx, popover_rect.bottom() + ARROW_H);
                let arrow_left = egui::pos2(cx - ARROW_HALF_W, popover_rect.bottom());
                let arrow_right = egui::pos2(cx + ARROW_HALF_W, popover_rect.bottom());
                painter.add(egui::Shape::convex_polygon(
                    vec![arrow_tip, arrow_left, arrow_right],
                    bg_color,
                    egui::Stroke::NONE,
                ));

                let btn_rect = popover_rect.shrink2(egui::vec2(10.0, 8.0));
                let btn = egui::Button::new(
                    egui::RichText::new("Confirm Update")
                        .color(egui::Color32::WHITE)
                        .strong(),
                )
                .fill(egui::Color32::from_rgb(0, 122, 255))
                .min_size(btn_rect.size());
                let response = ui.put(btn_rect, btn);
                if response.clicked() {
                    clicked_confirm = true;
                }
            });

        if clicked_outside {
            self.update = UpdateState::Idle;
            return;
        }
        if clicked_confirm {
            let device = self
                .devices
                .iter()
                .find(|c| c.port_name() == port_name)
                .cloned();
            let serial = device.as_ref().and_then(|d| d.info.serial_number.clone());
            let firmware = device.as_ref().and_then(|d| self.firmware_source_for(d));
            if let Some((src, _)) = firmware {
                self.update = UpdateState::Running {
                    port_name: port_name.clone(),
                    phase: UpdatePhase::Downloading,
                };
                workers::spawn_update(
                    port_name,
                    serial,
                    src,
                    self.registry.clone(),
                    self.tx.clone(),
                    self.ctx.clone(),
                );
            } else {
                // Firmware became unavailable between click and confirm.
                self.update = UpdateState::Idle;
            }
        }
    }
}

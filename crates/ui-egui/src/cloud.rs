//! CadCraft's half of Open and Save against cloud storage.
//!
//! The pages themselves are `craft_ui::cloud`. Every Craft app offers the same
//! ones. What is here is what only CadCraft can answer: a drawing is a DXF or
//! DWG file. These extensions and these content types are what that means.
//! Bytes become a drawing through `open` and go back through `cadcraft_io`.
//!
//! CadCraft holds no provider credential and speaks no provider protocol.
//! Every call goes to the CollabCraft service, which is the single place a
//! provider is configured and permissions are decided. Replacing the provider
//! changes nothing here.
//!
//! Nothing in this file opens a socket. The transport half is a
//! [`CloudBackend`] the host application supplies (see `apps/cadcraft/cloud`).
//! Every CollabCraft call blocks. A blocking read of a large drawing inside a
//! frame would freeze the window for seconds. So the pages state a [`Job`],
//! the host runs it on a worker thread, and the answer arrives on a channel
//! that [`pump`] drains on a later frame.

use std::collections::HashMap;
use std::sync::Arc;

// Re-exported so a host application and a test can name these types without
// depending on the crates they come from.
use cadcraft_doc::Drawing;
pub use craft_cloud::{Capability, Entry, EntryKind};
pub use craft_ui::cloud::{
    Account, Busy, CloudApp, CloudBackend, CloudFiles, Conflict, CopyFormat, Destination, DocumentKind, Event, Fault, Folder, Job, Link, Origin,
    Post, Source, UNAVAILABLE, available, browse, can_converse, conflict_modal, destination_name, has_target, link, load_shared, open_entry,
    open_page, origin, parent_of, prefers_cloud, progress, pump, reload_active, save, save_as, save_page, share_button, shares, sign_in, sign_out,
    start, who, work_together,
};
use egui::{Color32, Painter, Rect, Ui};
use serde_json::{Value as Json, json};

use crate::CadApp;
use crate::theme::Tokens;

/// Extensions CadCraft opens. The same set the desktop file picker offers.
const OPENS: [&str; 4] = ["dwg", "dxf", "dwt", "svg"];

/// Content types CadCraft opens, matched against the start of what the
/// provider reported. Read beside the extension because neither is reliable
/// alone: a WebDAV server often returns `application/octet-stream` for a
/// perfectly good drawing, and a file without an extension may have a correct
/// type.
const MIMES: [&str; 3] = ["application/dxf", "image/vnd.dxf", "image/svg+xml"];

/// What a CadCraft drawing is, as a cloud file list needs to know.
pub const DRAWING: DocumentKind = DocumentKind { extensions: &OPENS, mimes: &MIMES, noun: "drawing", default_extension: "dxf" };

/// The Save a Copy formats the Share menu offers under the share itself.
///
/// Each identifier is the UI command the chosen format runs. Each one writes a
/// copy and leaves the open drawing as it is.
const COPIES: [(&str, &str); 4] = [
    ("app.exportDxf", "Save a Copy as DXF Drawing…"),
    ("app.exportDwg", "Save a Copy as DWG Drawing…"),
    ("app.exportSvg", "Export as SVG…"),
    ("app.exportPdf", "Export as PDF…"),
];

/// The drawing snapshot each save in flight encoded.
///
/// Only CadCraft knows what a version of a drawing is. The pages hold none of
/// these. A save that succeeds clears the dirty flag for exactly the version
/// that reached the server. An edit made while the bytes were on the wire stays
/// unsaved.
pub type Sent = HashMap<u64, Arc<Drawing>>;

/// Which cloud page the toolbar is showing as a modal over the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// The account's own files and the ones other people shared.
    Open,
    /// A folder and a name to write under.
    Save,
}

/// Whether CadCraft can open this entry.
pub fn opens(entry: &Entry) -> bool {
    craft_ui::cloud::opens(entry, DRAWING)
}

/// A typed name as a drawing file name. Folder separators are dropped and a
/// missing extension becomes `.dxf`.
pub fn drawing_name(typed: &str) -> String {
    craft_ui::cloud::document_name(typed, DRAWING)
}

/// The whole of the cloud state, for the control channel and for tests.
pub fn status(app: &CadApp) -> Json {
    craft_ui::cloud::report(app)
}

/// The Share menu's export half, as the control it is drawn in wants it.
pub fn copies() -> Vec<CopyFormat> {
    COPIES.iter().map(|(id, label)| CopyFormat::new(*id, *label)).collect()
}

/// The host part of the service address, for the account menu's first line.
///
/// Read from the environment and compiled in nowhere, because the service is
/// deployment-specific. Somebody with an account on two deployments needs to
/// know which one a drawing is about to land in.
fn service_host() -> String {
    service_url().map(|value| value.trim_start_matches("https://").trim_start_matches("http://").to_owned()).unwrap_or_default()
}

/// Where the CollabCraft service is, as the environment gave it.
///
/// Public because the host application reads the same answer to decide whether
/// to start a worker at all. Two spellings of one setting would be a setting
/// that works in one half of the application.
pub fn service_url() -> Option<String> {
    ["CADCRAFT_CLOUD_URL", "CRAFT_CLOUD_URL", "COLLABCRAFT_URL"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .find(|value| !value.is_empty())
}

impl CloudApp for CadApp {
    fn cloud(&self) -> &CloudFiles {
        &self.cloud
    }

    fn cloud_mut(&mut self) -> &mut CloudFiles {
        &mut self.cloud
    }

    fn kind(&self) -> DocumentKind {
        DRAWING
    }

    fn active(&self) -> Option<u64> {
        // `session.active` is an index; absent when there are no open drawings
        // or the Start tab is showing.
        if self.ui.start_tab || self.session.docs.is_empty() {
            return None;
        }
        self.session.docs.get(self.session.active).map(|d| d.uid)
    }

    fn title(&self, uid: u64) -> String {
        self.session.docs.iter().find(|d| d.uid == uid).map(|d| d.title.clone()).unwrap_or_default()
    }

    fn open_bytes(&mut self, name: &str, bytes: Vec<u8>) -> Result<Option<u64>, String> {
        if let Some(e) = CadApp::open_size_error(name, bytes.len() as u64) {
            return Err(e);
        }
        let data = cadcraft_engine::cmd::file::base64_encode(&bytes);
        let _result = self.run("open", json!({ "data": data, "name": name }))?;
        self.cloud_page = None;
        self.ui.start_tab = false;
        self.canvas.zoom_pending = true;
        Ok(self.session.docs.get(self.session.active).map(|d| d.uid))
    }

    fn encode(&mut self, uid: u64, into: Destination<'_>) -> Result<Vec<u8>, String> {
        let doc = self.session.docs.iter().find(|d| d.uid == uid).ok_or("no drawing open")?;
        let snapshot = Arc::clone(&doc.doc);
        let ext = into.extension();
        // Pick the format the destination asks for. CadCraft writes DXF, DWG,
        // DWT, SVG and PDF; anything else is refused with a clear message.
        // A name with no extension goes out as the user's preferred format
        // (DXF or DWG), the same default Save As applies.
        let name =
            if ext.is_empty() { format!("{}.{}", into.name.trim_end_matches('.'), self.ui.save_format.as_str()) } else { into.name.to_owned() };
        let bytes = cadcraft_io::write(&snapshot, &name).map_err(|e| e.to_string())?;
        self.cloud_sent.insert(uid, snapshot);
        Ok(bytes)
    }

    fn close(&mut self, uid: u64) {
        if let Some(index) = self.session.docs.iter().position(|d| d.uid == uid) {
            // The engine's close command handles canceling the running command
            // and adjusting the active index; running it is safer than a bare
            // Vec::remove.
            let _ = self.run("close", json!({ "index": index }));
        }
    }

    fn mark_saved(&mut self, uid: u64) {
        // The snapshot that reached the server clears the dirty flag for that
        // exact version. An edit made while the bytes were on the wire stays
        // unsaved; only the version that landed marks clean.
        let Some(snapshot) = self.cloud_sent.remove(&uid) else { return };
        if let Some(doc) = self.session.docs.iter_mut().find(|d| d.uid == uid) {
            // `saved` is compared by pointer to `doc`; matching them marks the
            // drawing as no longer dirty for the version that reached the service.
            doc.saved = snapshot;
        }
    }

    fn drop_pending(&mut self, uid: u64) {
        self.cloud_sent.remove(&uid);
    }

    fn sharing(&mut self) -> &mut craft_ui::share::ShareDialog {
        &mut self.share
    }

    fn presence(&mut self) -> &mut craft_ui::presence::PresenceBar {
        &mut self.presence
    }

    fn paint_icon(&self, painter: &Painter, rect: Rect, kind: EntryKind, _colour: Color32) {
        // CadCraft's icon set uses a typed enum with a `dim` flag rather than
        // a per-call colour. Use Open (a folder with an arrow) for folders and
        // files alike; there is no dedicated document icon.
        let icon = if kind == EntryKind::Folder { crate::icons::Icon::Open } else { crate::icons::Icon::Open };
        crate::icons::paint(painter, rect, icon, false);
    }

    fn open_url(&self, url: &str) {
        if let Some(ctx) = &self.egui_ctx {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
    }

    fn say(&mut self, text: String) {
        self.set_status(text.clone());
        self.session.echo(text);
    }

    fn toast(&mut self, text: String) {
        self.set_status(text);
    }

    fn go_to_save_as(&mut self) {
        self.cloud_page = Some(Page::Save);
    }

    fn app_name(&self) -> &str {
        "CadCraft"
    }

    fn tr(&self, text: &str) -> String {
        crate::i18n::t(text).to_owned()
    }

    fn service(&self) -> String {
        service_host()
    }

    fn save_note(&self) -> Option<String> {
        Some(self.tr("The drawing is saved as a DXF or DWG file in the folder above."))
    }

    fn style(&self, _ui: &Ui) -> craft_ui::cloud::CloudStyle {
        let t = Tokens::get();
        craft_ui::cloud::CloudStyle {
            surface: t.panel,
            border: t.border,
            text: t.text,
            dim: t.text_dim,
            hover: t.hover,
            soft: t.select_bg,
            family: egui::FontFamily::Proportional,
            bold: egui::FontFamily::Proportional,
        }
    }
}

// ---------------------------------------------------------------- where the pages are drawn

/// The Cloud section of the Start tab (the Open page).
///
/// Beside the local picker, never in place of it. A build with no service
/// configured draws none of it. An account badge over an empty list reads to a
/// user as a broken window.
pub fn open_section(app: &mut CadApp, ui: &mut Ui) {
    if !available(app) {
        return;
    }
    let t = Tokens::get();
    ui.add_space(22.0);
    ui.label(egui::RichText::new(crate::i18n::t("Cloud")).font(egui::FontId::proportional(15.0)).color(t.text));
    ui.add_space(8.0);
    open_page(app, ui);
}

/// The Open and Save pages as a modal over the drawing canvas.
///
/// Used when File > Open from Cloud or File > Save to Cloud is chosen while a
/// drawing is already open, because the Start tab is not visible then.
pub fn modal(app: &mut CadApp, ctx: &egui::Context) {
    let Some(page) = app.cloud_page else { return };
    let title = match page {
        Page::Open => crate::i18n::t("Open from the Cloud"),
        Page::Save => crate::i18n::t("Save to the Cloud"),
    };
    let mut close = false;
    egui::Modal::new(egui::Id::new("cloud_page")).show(ctx, |ui| {
        let margin = egui::Frame::popup(ui.style()).total_margin().sum();
        let room = (ctx.content_rect().size() - margin - egui::vec2(16.0, 16.0)).max(egui::Vec2::splat(1.0));
        let width = 680.0_f32.min(room.x);
        ui.set_min_width(380.0_f32.min(width));
        ui.set_max_width(width);
        ui.label(egui::RichText::new(title).font(egui::FontId::proportional(16.0)).strong());
        ui.add_space(10.0);
        let body = (room.y - ui.min_size().y - 48.0).max(1.0);
        egui::ScrollArea::vertical().id_salt(("cloud_page", page == Page::Save)).max_height(body).show(ui, |ui| match page {
            Page::Open => open_page(app, ui),
            Page::Save => close = save_body(app, ui),
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(crate::i18n::t("Close")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        });
    });
    if close {
        app.cloud_page = None;
    }
}

/// Save As: where to write, then the page for whichever was picked.
///
/// True once the drawing has gone somewhere and the page is finished with.
fn save_body(app: &mut CadApp, ui: &mut Ui) -> bool {
    let to_cloud = app.save_to_cloud.unwrap_or_else(|| prefers_cloud(app));
    start(app);
    let account = destination_name(app);
    ui.label(crate::i18n::t("Where"));
    ui.add_space(6.0);
    let t = Tokens::get();
    if place_row(ui, &t, crate::icons::Icon::Open, crate::i18n::t("This Computer"), crate::i18n::t("A folder on this computer"), !to_cloud).clicked()
    {
        app.save_to_cloud = Some(false);
    }
    if place_row(ui, &t, crate::icons::Icon::Share, &account, crate::i18n::t("Your cloud account"), to_cloud).clicked() {
        app.save_to_cloud = Some(true);
    }
    ui.add_space(18.0);
    if to_cloud {
        return save_page(app, ui);
    }
    // Local path: delegate to the native Save dialog.
    if ui.button(crate::i18n::t("Browse…")).clicked() {
        let _ = app.run("saveas", json!({}));
        return true;
    }
    false
}

/// One destination row: icon, name, description, selection highlight.
fn place_row(ui: &mut Ui, t: &Tokens, icon: crate::icons::Icon, name: &str, hint: &str, chosen: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, chosen, name));
    if chosen {
        ui.painter().rect_filled(rect, 2.0, t.select_bg);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 2.0, t.hover);
    }
    let glyph = Rect::from_min_size(rect.min + egui::vec2(8.0, 10.0), egui::Vec2::splat(20.0));
    crate::icons::paint(ui.painter(), glyph, icon, false);
    let x = rect.min.x + 36.0;
    ui.painter().text(egui::pos2(x, rect.center().y - 7.0), egui::Align2::LEFT_CENTER, name, egui::FontId::proportional(12.5), t.text);
    ui.painter().text(egui::pos2(x, rect.center().y + 8.0), egui::Align2::LEFT_CENTER, hint, egui::FontId::proportional(11.0), t.text_dim);
    response
}

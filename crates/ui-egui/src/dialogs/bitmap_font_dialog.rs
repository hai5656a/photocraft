//! File › Export › Bitmap Font…: form fields plus a live glyph preview of the export text, and
//! per-character / global offset controls that feed `file.export.bitmapFont`.

use std::sync::Arc;

use photocraft_text::bmfont::{self, FontMetrics, GlyphBitmap};
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::state::DialogKind;
use crate::theme::Tokens;

const MARK: &str = "__bitmapFont";
const COMMAND: &str = "file.export.bitmapFont";
const SELECTED: &str = "__selectedChar";

/// Default size of the resizable Bitmap Font export dialog (compact; user can drag larger).
pub(crate) const DIALOG_SIZE: egui::Vec2 = egui::vec2(540.0, 480.0);
/// Smallest the dialog can be dragged to while keeping OK/Cancel usable.
pub(crate) const DIALOG_MIN: egui::Vec2 = egui::vec2(420.0, 360.0);
/// Preview band height when the body has room.
const PREVIEW_BAND: f32 = 120.0;
/// Label column in the form grids.
const LABEL_W: f32 = 96.0;

pub fn owns(fields: &Map<String, Value>) -> bool {
    fields.contains_key(MARK)
}

/// Open the dialog seeded from the active text layer / all-text group.
pub fn open(app: &mut PhotocraftApp, dir: String, face: String, text: String) -> u64 {
    let mut f = Map::new();
    f.insert(MARK.into(), json!(true));
    f.insert("__form".into(), json!(true));
    f.insert("__command".into(), json!(COMMAND));
    f.insert("__label".into(), json!(tl!("Export Bitmap Font…")));
    f.insert("face".into(), json!(face));
    f.insert("dir".into(), json!(dir));
    f.insert("text".into(), json!(text));
    f.insert("pageWidth".into(), json!(1024));
    f.insert("pageHeight".into(), json!(1024));
    f.insert("padding".into(), json!(1));
    f.insert("offsetX".into(), json!(0));
    f.insert("offsetY".into(), json!(0));
    f.insert("charOffsets".into(), json!({}));
    f.insert(SELECTED.into(), json!(""));
    let orig: Map<String, Value> = f.iter().filter(|(k, _)| !k.starts_with("__")).map(|(k, v)| (k.clone(), v.clone())).collect();
    f.insert("__formOrig".into(), Value::Object(orig));
    app.ui.open_dialog(DialogKind::Command, f)
}

pub fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    // Leave room for the dialog's OK/Cancel row below this body — if we fill all available
    // height, Resize grows past the default size on the first frames.
    let button_chrome = ui.spacing().item_spacing.y + 8.0 + crate::widgets::button_height(ui.ctx()) + 4.0;
    let full = ui.available_size();
    let usable_h = (full.y - button_chrome).max(160.0);
    let preview_h = PREVIEW_BAND.min((usable_h * 0.32).max(88.0)).min((usable_h - 160.0).max(72.0));
    let form_h = (usable_h - preview_h - 16.0).max(100.0);
    let form_w = (full.x - 1.0).max(160.0);
    // Reserve the viewport first, then scroll inside it. ScrollArea's bar must not widen the
    // parent (content+bar > available used to grow the dialog every frame).
    let bar = ui.spacing().scroll.allocated_width();
    let inner_w = (form_w - bar).max(120.0);
    ui.vertical(|ui| {
        ui.set_max_width(form_w);
        let (form_rect, _) = ui.allocate_exact_size(egui::vec2(form_w, form_h), egui::Sense::hover());
        let mut form_ui = ui.new_child(
            egui::UiBuilder::new().max_rect(form_rect).layout(egui::Layout::top_down(egui::Align::Min)),
        );
        form_ui.set_clip_rect(form_rect);
        egui::ScrollArea::vertical()
            .id_salt("bitmap-font-form-scroll")
            .max_height(form_h)
            .auto_shrink([false, false])
            .show(&mut form_ui, |ui| {
                ui.set_width(inner_w);
                ui.set_max_width(inner_w);
                form_fields(ui, f);
                ui.add_space(8.0);
                offset_fields(ui, f);
            });
        ui.add_space(8.0);
        preview_panel(app, ui, f, preview_h);
    });
}

/// Width left for a value control after the label column and grid spacing (must match the grid).
fn value_width(ui: &egui::Ui) -> f32 {
    const GRID_X: f32 = 12.0;
    (ui.available_width() - LABEL_W - GRID_X).max(120.0)
}

fn form_fields(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let mut browse = false;
    let field_w = value_width(ui);
    egui::Grid::new("bitmap-font-form").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        for (key, label) in [("face", tl!("Face")), ("dir", tl!("Dir"))] {
            ui.add_sized([LABEL_W, 18.0], egui::Label::new(label));
            let mut s = f.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            ui.horizontal(|ui| {
                let browse_w = if key == "dir" { 72.0 } else { 0.0 };
                let edit_w = (field_w - browse_w - if key == "dir" { ui.spacing().item_spacing.x } else { 0.0 }).max(120.0);
                ui.add(egui::TextEdit::singleline(&mut s).desired_width(edit_w).clip_text(true)).accessible_name(label);
                if key == "dir" && crate::widgets::secondary_button(ui, tl!("Browse…"), browse_w).clicked() {
                    browse = true;
                }
            });
            f.insert(key.into(), json!(s));
            ui.end_row();
        }

        ui.add_sized([LABEL_W, 18.0], egui::Label::new(tl!("Text")));
        let mut text = f.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
        let response = ui
            .add(egui::TextEdit::multiline(&mut text).desired_rows(3).desired_width(field_w))
            .accessible_name(tl!("Text"))
            .on_hover_text(tl!("Characters to export — the preview updates as you type"));
        if response.has_focus() {
            ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        }
        f.insert("text".into(), json!(text));
        ui.end_row();

        for (key, label) in [("pageWidth", tl!("Page Width")), ("pageHeight", tl!("Page Height")), ("padding", tl!("Padding"))] {
            ui.add_sized([LABEL_W, 18.0], egui::Label::new(label));
            let mut n = f.get(key).and_then(Value::as_i64).unwrap_or(0);
            ui.add(egui::DragValue::new(&mut n).custom_parser(crate::widgets::parse_num)).accessible_name(label);
            f.insert(key.into(), json!(n));
            ui.end_row();
        }
    });
    if browse {
        f.insert(crate::dialogs::BROWSE_FIELD.into(), json!("dir"));
    }
}

fn offset_fields(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    ui.label(egui::RichText::new(tl!("Character Offset")).font(crate::theme::semibold(13.0)));
    ui.add_space(4.0);
    let drop_w = value_width(ui).min(320.0);
    egui::Grid::new("bitmap-font-offsets").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label(tl!("Offset X"));
        let mut ox = f.get("offsetX").and_then(Value::as_i64).unwrap_or(0);
        ui.add(egui::DragValue::new(&mut ox).speed(1.0).custom_parser(crate::widgets::parse_num)).accessible_name(tl!("Offset X"));
        f.insert("offsetX".into(), json!(ox));
        ui.end_row();

        ui.label(tl!("Offset Y"));
        let mut oy = f.get("offsetY").and_then(Value::as_i64).unwrap_or(0);
        ui.add(egui::DragValue::new(&mut oy).speed(1.0).custom_parser(crate::widgets::parse_num)).accessible_name(tl!("Offset Y"));
        f.insert("offsetY".into(), json!(oy));
        ui.end_row();

        let chars = bmfont::charset(f.get("text").and_then(Value::as_str).unwrap_or_default());
        if chars.is_empty() {
            return;
        }
        let mut selected = f.get(SELECTED).and_then(Value::as_str).unwrap_or_default().to_owned();
        if selected.chars().count() != 1 || !chars.contains(&selected.chars().next().unwrap_or('\0')) {
            selected = chars[0].to_string();
        }
        ui.label(tl!("Character"));
        let options: Vec<(String, String)> = chars.iter().map(|c| {
            let s = c.to_string();
            let label = format!("{s}  (U+{:04X})", *c as u32);
            (s, label)
        }).collect();
        let refs: Vec<(String, &str)> = options.iter().map(|(a, b)| (a.clone(), b.as_str())).collect();
        crate::widgets::dropdown_literal(ui, "bitmap-font-char", tl!("Character"), &mut selected, &refs, drop_w);
        f.insert(SELECTED.into(), json!(selected.clone()));
        ui.end_row();

        let ch = selected.chars().next().unwrap_or('?');
        let (mut dx, mut dy) = char_offset(f, ch);
        ui.label(tl!("Char Offset X"));
        if ui.add(egui::DragValue::new(&mut dx).speed(1.0).custom_parser(crate::widgets::parse_num)).accessible_name(tl!("Char Offset X")).changed() {
            set_char_offset(f, ch, dx, dy);
        }
        ui.end_row();
        ui.label(tl!("Char Offset Y"));
        if ui.add(egui::DragValue::new(&mut dy).speed(1.0).custom_parser(crate::widgets::parse_num)).accessible_name(tl!("Char Offset Y")).changed() {
            set_char_offset(f, ch, dx, dy);
        }
        ui.end_row();
    });
}

fn char_offset(f: &Map<String, Value>, ch: char) -> (i32, i32) {
    let key = ch.to_string();
    let Some(obj) = f.get("charOffsets").and_then(Value::as_object) else {
        return (0, 0);
    };
    let Some(v) = obj.get(&key) else {
        return (0, 0);
    };
    (
        v.get("x").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
        v.get("y").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
    )
}

fn set_char_offset(f: &mut Map<String, Value>, ch: char, dx: i32, dy: i32) {
    let key = ch.to_string();
    let mut map = f.get("charOffsets").and_then(Value::as_object).cloned().unwrap_or_default();
    if dx == 0 && dy == 0 {
        map.remove(&key);
    } else {
        map.insert(key, json!({"x": dx, "y": dy}));
    }
    f.insert("charOffsets".into(), Value::Object(map));
}

#[derive(Clone)]
struct PreviewCache {
    signature: String,
    texture: Option<Arc<egui::TextureHandle>>,
    error: Option<String>,
}

fn preview_panel(app: &mut PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>, band_h: f32) {
    ui.label(egui::RichText::new(tl!("Preview")).font(crate::theme::semibold(13.0)));
    ui.add_space(4.0);
    // Stay strictly inside the width we already have (Resize grows if we ask for more).
    let signature = preview_signature(app, f);
    let key = egui::Id::unique(("bitmap-font-preview", f.get("__command").and_then(Value::as_str).unwrap_or(COMMAND)));
    let cached: Option<PreviewCache> = ui.data(|d| d.get_temp(key));
    let preview = if cached.as_ref().is_some_and(|c| c.signature == signature) {
        cached.unwrap()
    } else {
        let built = build_preview(app, f);
        let preview = match built {
            Ok((w, h, rgba)) => {
                let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                let texture = Arc::new(ui.ctx().load_texture("bitmap-font-preview", image, egui::TextureOptions::NEAREST));
                PreviewCache { signature, texture: Some(texture), error: None }
            }
            Err(error) => PreviewCache { signature, texture: None, error: Some(error) },
        };
        ui.data_mut(|d| d.insert_temp(key, preview.clone()));
        preview
    };

    let t = Tokens::get(ui.ctx());
    let pane_h = (band_h - 22.0).max(64.0);
    let pane_w = (ui.available_width() - 1.0).max(1.0);
    let (outer, _) = ui.allocate_exact_size(egui::vec2(pane_w, pane_h), egui::Sense::hover());
    ui.painter().rect_filled(outer, 0.0, t.card);
    ui.painter().rect_stroke(outer, 0.0, egui::Stroke::new(1.0, t.card_border), egui::StrokeKind::Inside);
    let rect = outer.shrink(8.0);
    if let Some(tex) = &preview.texture {
        let size = tex.size_vec2().max(egui::vec2(1.0, 1.0));
        let scale = (rect.width() / size.x).min(rect.height() / size.y).clamp(0.25, 4.0);
        let shown = size * scale;
        let image_rect = egui::Rect::from_center_size(rect.center(), shown);
        ui.painter().image(tex.id(), image_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
    } else if let Some(err) = &preview.error {
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, err, egui::FontId::proportional(13.0), t.danger);
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            tl!("No characters to preview"),
            egui::FontId::proportional(13.0),
            t.text_dim,
        );
    }
}

fn preview_signature(app: &PhotocraftApp, f: &Map<String, Value>) -> String {
    let layer = app.session.active().and_then(|st| st.active_layer.map(|id| id.0)).unwrap_or(0);
    let rev = app.session.active().map(|st| st.revision).unwrap_or(0);
    let text = f.get("text").and_then(Value::as_str).unwrap_or_default();
    let ox = f.get("offsetX").cloned().unwrap_or(json!(0));
    let oy = f.get("offsetY").cloned().unwrap_or(json!(0));
    let offsets = f.get("charOffsets").cloned().unwrap_or(json!({}));
    format!("{layer}:{rev}:{text}:{ox}:{oy}:{offsets}")
}

fn build_preview(app: &PhotocraftApp, f: &Map<String, Value>) -> Result<(u32, u32, Vec<u8>), String> {
    let text = f.get("text").and_then(Value::as_str).unwrap_or_default();
    let chars = bmfont::charset(text);
    if chars.is_empty() {
        return Err(tl!("No characters to preview").to_owned());
    }
    let st = app.session.active().ok_or_else(|| tl!("No document open").to_owned())?;
    let id = st.active_layer.ok_or_else(|| tl!("No layer selected").to_owned())?;
    let layer = st.doc.layer(id).ok_or_else(|| tl!("No layer selected").to_owned())?;
    if !layer.is_bitmap_font_exportable() {
        return Err(tl!("Select a text layer or a group of only text layers").to_owned());
    }
    let (metrics, mut glyphs) = harvest_or_raster(st, layer, &chars)?;
    let ox = f.get("offsetX").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok()).unwrap_or(0);
    let oy = f.get("offsetY").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok()).unwrap_or(0);
    let per = parse_char_offsets_ui(f);
    bmfont::apply_offsets(&mut glyphs, ox, oy, &per);
    bmfont::layout_string(&glyphs, text, metrics).ok_or_else(|| tl!("Preview failed").to_owned())
}

fn harvest_or_raster(
    st: &photocraft_engine::DocState,
    layer: &photocraft_doc::Layer,
    chars: &[char],
) -> Result<(FontMetrics, Vec<GlyphBitmap>), String> {
    match photocraft_engine::bitmap_font_harvest::harvest(&st.doc, layer, chars) {
        Ok((metrics, glyphs)) => Ok((metrics, glyphs)),
        Err(_) => {
            let style = primary_style(layer).ok_or_else(|| tl!("Preview failed").to_owned())?;
            let dpi = if st.doc.resolution_dpi.is_finite() && st.doc.resolution_dpi > 0.0 { st.doc.resolution_dpi } else { 72.0 };
            let mut engine = photocraft_text::shared().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut glyphs = Vec::with_capacity(chars.len());
            for &ch in chars {
                glyphs.push(bmfont::rasterize_char(&mut engine, &style, ch, dpi));
            }
            let size = ((style.size_pt * dpi / 72.0).round() as i32).max(1);
            let metrics = FontMetrics { size, line_height: (size as f32 * 1.25).round() as i32, base: (size as f32 * 0.8).round() as i32 };
            Ok((metrics, glyphs))
        }
    }
}

fn primary_style(layer: &photocraft_doc::Layer) -> Option<photocraft_doc::text::CharStyle> {
    match &layer.content {
        photocraft_doc::LayerContent::Text(t) => Some(
            t.char_runs().into_iter().next().map(|r| r.style).unwrap_or_else(|| photocraft_doc::text::CharStyle {
                font_family: t.font_family.clone(),
                size_pt: t.size_pt,
                color: t.color,
                ..Default::default()
            }),
        ),
        photocraft_doc::LayerContent::Group(_) => {
            let mut best: Option<(f32, photocraft_doc::text::CharStyle)> = None;
            fn walk(layers: &[photocraft_doc::Layer], best: &mut Option<(f32, photocraft_doc::text::CharStyle)>) {
                for l in layers {
                    match &l.content {
                        photocraft_doc::LayerContent::Text(t) => {
                            let style = t.char_runs().into_iter().next().map(|r| r.style).unwrap_or_else(|| photocraft_doc::text::CharStyle {
                                font_family: t.font_family.clone(),
                                size_pt: t.size_pt,
                                color: t.color,
                                ..Default::default()
                            });
                            let sz = style.size_pt;
                            if best.as_ref().is_none_or(|(s, _)| sz > *s) {
                                *best = Some((sz, style));
                            }
                        }
                        photocraft_doc::LayerContent::Group(g) => walk(&g.children, best),
                        _ => {}
                    }
                }
            }
            if let Some(kids) = layer.children() {
                walk(kids, &mut best);
            }
            best.map(|(_, s)| s)
        }
        _ => None,
    }
}

fn parse_char_offsets_ui(f: &Map<String, Value>) -> Vec<(char, i32, i32)> {
    let Some(obj) = f.get("charOffsets").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, v) in obj {
        let ch = match key.chars().count() {
            1 => key.chars().next(),
            _ => key.parse::<u32>().ok().and_then(char::from_u32),
        };
        let Some(ch) = ch else { continue };
        let dx = v.get("x").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok()).unwrap_or(0);
        let dy = v.get("y").and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok()).unwrap_or(0);
        if dx != 0 || dy != 0 {
            out.push((ch, dx, dy));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_seeds_text_and_offset_fields() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 200, "height": 100})).unwrap();
        app.run("type.create", json!({"text": "12.", "size": 24, "x": 10, "y": 40})).unwrap();
        let id = open(&mut app, "/tmp".into(), "Face".into(), "12.".into());
        let d = app.ui.dialog_mut(id).expect("dialog");
        assert!(owns(&d.fields));
        assert_eq!(d.fields.get("text").and_then(Value::as_str), Some("12."));
        assert_eq!(d.fields.get("offsetX").and_then(Value::as_i64), Some(0));
        assert_eq!(d.fields.get("charOffsets"), Some(&json!({})));
    }

    #[test]
    fn char_offset_round_trips_in_fields() {
        let mut f = Map::new();
        f.insert("charOffsets".into(), json!({}));
        set_char_offset(&mut f, '.', 2, -3);
        assert_eq!(char_offset(&f, '.'), (2, -3));
        set_char_offset(&mut f, '.', 0, 0);
        assert_eq!(char_offset(&f, '.'), (0, 0));
    }
}

//! File › Export › Bitmap Font (BMFont): type layer or all-text group → `.fnt` + atlas PNG pages.
//!
//! Prefers document-pixel harvest (Photoshop layer cache + effects, PSD spacing / vertical
//! layout). Characters the document does not paint are filled from the type engine. The UI,
//! CLI and MCP all dispatch [`file.export.bitmapFont`].

use std::sync::PoisonError;

use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType as CSample};
use photocraft_doc::text::CharStyle;
use photocraft_doc::{Layer, LayerContent, TextLayer};
use photocraft_text::bmfont::{self, BmfontError, GlyphBitmap};
use serde_json::{Value, json};

use super::bitmap_font_harvest;
use crate::commands::{CommandSpec, layer_param};
use crate::file_cmds::{join, native_doc, sanitize, str_param, write_file};
use crate::{EngineError, Result, Session};

const CMD: &str = "file.export.bitmapFont";

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: CMD.into(), msg: msg.into() }
}

fn can_export(s: &Session) -> std::result::Result<(), String> {
    native_doc(s)?;
    let d = s.active().ok_or_else(|| "no document open".to_string())?;
    let id = d.active_layer.ok_or_else(|| "no layer selected".to_string())?;
    let layer = d.doc.layer(id).ok_or_else(|| "no layer selected".to_string())?;
    if layer.is_bitmap_font_exportable() {
        Ok(())
    } else {
        Err("select a text layer or a group of only text layers".into())
    }
}

/// Collects (style, charset text, face name) from a text layer or all-text group.
fn source_from_layer(layer: &Layer, text_override: Option<&str>) -> Result<(CharStyle, String, String)> {
    match &layer.content {
        LayerContent::Text(t) => {
            let style = primary_style(t);
            let text = text_override.map(str::to_string).unwrap_or_else(|| t.text.clone());
            Ok((style, text, layer.name.clone()))
        }
        LayerContent::Group(_) if layer.is_all_text_group() => {
            let mut members: Vec<&TextLayer> = Vec::new();
            collect_text_leaves(layer, &mut members);
            if members.is_empty() {
                return Err(bad("the group has no text layers"));
            }
            let mut primary = members[0];
            let mut best = primary_style(primary).size_pt;
            for m in members.iter().skip(1) {
                let sz = primary_style(m).size_pt;
                if sz > best {
                    best = sz;
                    primary = m;
                }
            }
            let style = primary_style(primary);
            let text = if let Some(t) = text_override {
                t.to_string()
            } else {
                let mut s = String::new();
                for m in &members {
                    s.push_str(&m.text);
                }
                s
            };
            Ok((style, text, layer.name.clone()))
        }
        _ => Err(bad("select a text layer or a group of only text layers")),
    }
}

fn collect_text_leaves<'a>(layer: &'a Layer, out: &mut Vec<&'a TextLayer>) {
    match &layer.content {
        LayerContent::Text(t) => out.push(t),
        LayerContent::Group(g) => {
            for c in &g.children {
                collect_text_leaves(c, out);
            }
        }
        _ => {}
    }
}

fn primary_style(t: &TextLayer) -> CharStyle {
    t.char_runs().into_iter().next().map(|r| r.style).unwrap_or_else(|| CharStyle {
        font_family: t.font_family.clone(),
        size_pt: t.size_pt,
        color: t.color,
        ..Default::default()
    })
}

fn u32_param(p: &Value, key: &str, default: u32) -> Result<u32> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => {
            let v = n.as_u64().ok_or_else(|| bad(format!("{key} must be a non-negative integer")))?;
            u32::try_from(v).map_err(|_| bad(format!("{key} is too large")))
        }
        Some(_) => Err(bad(format!("{key} must be a non-negative integer"))),
    }
}

fn i32_param(p: &Value, key: &str, default: i32) -> Result<i32> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n.as_i64().and_then(|v| i32::try_from(v).ok()).ok_or_else(|| bad(format!("{key} must be an integer"))),
        Some(_) => Err(bad(format!("{key} must be an integer"))),
    }
}

/// `charOffsets`: `{"A":{"x":1,"y":-2}, ".":{"x":0,"y":5}}` — keys are one character (or a
/// decimal code point string). Invalid entries are skipped, never panic.
fn parse_char_offsets(p: &Value) -> Vec<(char, i32, i32)> {
    let Some(obj) = p.get("charOffsets").and_then(Value::as_object) else {
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

fn apply_export_offsets(glyphs: &mut [GlyphBitmap], p: &Value) -> Result<()> {
    let ox = i32_param(p, "offsetX", 0)?;
    let oy = i32_param(p, "offsetY", 0)?;
    let per = parse_char_offsets(p);
    if ox != 0 || oy != 0 || !per.is_empty() {
        bmfont::apply_offsets(glyphs, ox, oy, &per);
    }
    Ok(())
}

fn apply_packed_offsets(glyphs: &mut [bmfont::PackedGlyph], p: &Value) -> Result<()> {
    let ox = i32_param(p, "offsetX", 0)?;
    let oy = i32_param(p, "offsetY", 0)?;
    let per = parse_char_offsets(p);
    for g in glyphs.iter_mut() {
        g.xoffset = g.xoffset.saturating_add(ox);
        g.yoffset = g.yoffset.saturating_add(oy);
        if let Some((_, dx, dy)) = per.iter().find(|(c, ..)| *c == g.ch) {
            g.xoffset = g.xoffset.saturating_add(*dx);
            g.yoffset = g.yoffset.saturating_add(*dy);
        }
    }
    Ok(())
}

fn encode_png(page: &bmfont::AtlasPage) -> Result<Vec<u8>> {
    let image = Image::from_raw(page.width, page.height, ChannelLayout::Rgba, CSample::U8, page.rgba.clone())
        .map_err(|e| EngineError::Other(format!("atlas PNG: {e}")))?;
    let opts = EncodeOptions { embed_icc: false, ..Default::default() };
    photocraft_codecs::encode(&image, Format::Png, &opts).map_err(|e| EngineError::Other(format!("atlas PNG: {e}")))
}

fn map_bmfont_err(e: BmfontError) -> EngineError {
    bad(e.to_string())
}

fn metrics_or_from_glyphs(glyphs: &[GlyphBitmap], metrics: bmfont::FontMetrics) -> bmfont::FontMetrics {
    if metrics.size > 0 && metrics.line_height > 0 {
        return metrics;
    }
    let h = glyphs.iter().map(|g| g.height).max().unwrap_or(1) as i32;
    bmfont::FontMetrics { size: h.max(1), line_height: h.max(1), base: (h * 4 / 5).max(0) }
}

/// Builds the BMFont and streams `<face>.fnt` plus `<face>_<page>.png` through `write`.
pub fn export_bitmap_font_with(s: &Session, p: &Value, mut write: impl FnMut(&str, &[u8]) -> Result<()>) -> Result<Value> {
    let id = layer_param(s, p)?;
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let layer = d.doc.layer(id).ok_or(EngineError::NoLayer(id))?;
    if !layer.is_bitmap_font_exportable() {
        return Err(bad("select a text layer or a group of only text layers"));
    }
    let text_override = p.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty());
    let (style, text, default_face) = source_from_layer(layer, text_override)?;
    let face = p.get("face").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&default_face);
    let face_file = sanitize(face);
    let page_w = u32_param(p, "pageWidth", 1024)?;
    let page_h = u32_param(p, "pageHeight", 1024)?;
    let padding = u32_param(p, "padding", 1)?;
    let dpi = d.doc.resolution_dpi;
    let dpi = if dpi.is_finite() && dpi > 0.0 { dpi } else { 72.0 };

    let opts = bmfont::BuildOptions { face: face.to_string(), dpi, page_w, page_h, padding };
    let chars = bmfont::charset(&text);
    // Document pixels first (PSD size, spacing, vertical layout, painted look). Missing chars
    // are filled inside harvest with the primary member's type style + layer effects.
    let font = match bitmap_font_harvest::harvest(&d.doc, layer, &chars) {
        Ok((metrics, mut glyphs)) => {
            apply_export_offsets(&mut glyphs, p)?;
            bmfont::from_bitmaps(opts.face.clone(), metrics_or_from_glyphs(&glyphs, metrics), &glyphs, &opts).map_err(map_bmfont_err)?
        }
        Err(_) => {
            let mut engine = photocraft_text::shared().lock().unwrap_or_else(PoisonError::into_inner);
            let mut font = bmfont::export_memory(&mut engine, &style, &text, &opts).map_err(map_bmfont_err)?;
            apply_packed_offsets(&mut font.glyphs, p)?;
            font
        }
    };

    // Encode everything first so a later write failure does not leave a partial `.fnt` claiming
    // pages that were never written. One page shares the face stem with the `.fnt` (`face.png`);
    // extra pages keep `face_1.png`, `face_2.png`, …
    let mut files = Vec::new();
    let mut page_names = Vec::with_capacity(font.pages.len());
    let mut page_bytes = Vec::with_capacity(font.pages.len());
    for (i, page) in font.pages.iter().enumerate() {
        let name = if font.pages.len() == 1 { format!("{face_file}.png") } else { format!("{face_file}_{i}.png") };
        page_bytes.push(encode_png(page)?);
        page_names.push(name);
    }
    let fnt_name = format!("{face_file}.fnt");
    let fnt = bmfont::format_fnt(&font, |i| {
        page_names.get(i as usize).cloned().unwrap_or_else(|| {
            if font.pages.len() == 1 { format!("{face_file}.png") } else { format!("{face_file}_{i}.png") }
        })
    });

    write(&fnt_name, fnt.as_bytes())?;
    files.push(fnt_name.clone());
    for (name, bytes) in page_names.iter().zip(page_bytes.iter()) {
        write(name, bytes)?;
        files.push(name.clone());
    }

    Ok(json!({
        "files": files,
        "fnt": fnt_name,
        "chars": font.glyphs.len(),
        "pages": font.pages.len(),
        "face": font.face,
        "size": font.metrics.size,
    }))
}

fn export_bitmap_font(s: &mut Session, p: &Value) -> Result<Value> {
    let dir = str_param(p, "dir", CMD).or_else(|_| str_param(p, "output", CMD))?.to_string();
    let report = export_bitmap_font_with(s, p, |name, bytes| write_file(&join(&dir, name), bytes))?;
    // Rewrite relative names as absolute paths under `dir` for the command result.
    let files: Vec<String> = report
        .get("files")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(|n| join(&dir, n)).collect())
        .unwrap_or_default();
    let fnt = report.get("fnt").and_then(Value::as_str).map(|n| join(&dir, n)).unwrap_or_default();
    Ok(json!({
        "files": files,
        "fnt": fnt,
        "chars": report.get("chars").cloned().unwrap_or(json!(0)),
        "pages": report.get("pages").cloned().unwrap_or(json!(0)),
        "face": report.get("face").cloned().unwrap_or(json!("")),
        "size": report.get("size").cloned().unwrap_or(json!(0)),
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: "Bitmap Font…",
        menu: &[],
        shortcut: None,
        params: r##"{"dir":path,"layer":id?,"text":str?,"face":str?,"pageWidth":1024?,"pageHeight":1024?,"padding":1?,"offsetX":0?,"offsetY":0?,"charOffsets":{"A":{"x":0,"y":0}}?} → {files,fnt,chars,pages,face,size}"##,
        enabled: can_export,
        journal: true,
        run: export_bitmap_font,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn session_with_type(text: &str) -> (Session, u64) {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 200, "height": 100})).unwrap();
        let r = s.execute("type.create", json!({"text": text, "size": 24, "x": 20, "y": 40})).unwrap();
        let id = r["layer"].as_u64().unwrap();
        (s, id)
    }

    #[test]
    fn exports_fnt_and_png_for_a_type_layer() {
        let (mut s, id) = session_with_type("AB12");
        let store: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let writer = store.clone();
        let report = export_bitmap_font_with(&s, &json!({"layer": id}), |name, bytes| {
            writer.lock().unwrap().insert(name.to_string(), bytes.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(report["chars"], 4);
        let map = store.lock().unwrap();
        let fnt_name = report["fnt"].as_str().unwrap();
        assert!(fnt_name.ends_with(".fnt"));
        let fnt = String::from_utf8(map.get(fnt_name).unwrap().clone()).unwrap();
        assert!(fnt.contains("chars count=4"));
        assert!(fnt.contains("char id=65 "));
        let pngs: Vec<_> = map.keys().filter(|k| k.ends_with(".png")).collect();
        assert_eq!(pngs.len(), 1);
        let stem = fnt_name.trim_end_matches(".fnt");
        assert_eq!(pngs[0].as_str(), &format!("{stem}.png"), "atlas stem matches .fnt");
        assert!(fnt.contains(&format!("file=\"{stem}.png\"")));
        assert!(map.get(pngs[0]).unwrap().starts_with(b"\x89PNG"));
        // Native path with dir also works.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let r = s.execute(CMD, json!({"dir": path, "layer": id})).unwrap();
        assert!(std::path::Path::new(r["fnt"].as_str().unwrap()).is_file());
    }

    #[test]
    fn exports_group_of_text_layers() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 200, "height": 100})).unwrap();
        let a = s.execute("type.create", json!({"text": "12", "size": 20, "x": 10, "y": 30})).unwrap()["layer"].as_u64().unwrap();
        let b = s.execute("type.create", json!({"text": "AB", "size": 28, "x": 10, "y": 60})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.select", json!({"layer": a})).unwrap();
        s.execute("layer.select", json!({"layer": b, "mode": "add"})).unwrap();
        let g = s.execute("layer.new.groupFromLayers", json!({"name": "Digits"})).unwrap();
        let gid = g.get("layer").and_then(Value::as_u64).or_else(|| {
            s.active().and_then(|d| d.active_layer.map(|l| l.0))
        }).unwrap();
        let store: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let writer = store.clone();
        let report = export_bitmap_font_with(&s, &json!({"layer": gid}), |name, bytes| {
            writer.lock().unwrap().insert(name.to_string(), bytes.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(report["chars"], 4); // 1,2,A,B
        assert_eq!(report["face"], "Digits");
    }

    #[test]
    fn text_override_sets_charset() {
        let (s, id) = session_with_type("AB");
        let store: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let writer = store.clone();
        let report = export_bitmap_font_with(&s, &json!({"layer": id, "text": "XYZ"}), |name, bytes| {
            writer.lock().unwrap().insert(name.to_string(), bytes.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(report["chars"], 3);
        let fnt = String::from_utf8(store.lock().unwrap().get(report["fnt"].as_str().unwrap()).unwrap().clone()).unwrap();
        assert!(fnt.contains("char id=88 ")); // 'X'
        assert!(!fnt.contains("char id=65 ")); // 'A' not in override
    }

    #[test]
    fn rejects_raster_layer_and_empty_charset() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 32, "height": 32})).unwrap();
        s.execute("layer.new.layer", json!({})).unwrap();
        let id = s.active().unwrap().active_layer.unwrap().0;
        let err = export_bitmap_font_with(&s, &json!({"layer": id}), |_, _| Ok(())).unwrap_err().to_string();
        assert!(err.contains("text layer") || err.contains("group"), "{err}");

        let (s, id) = session_with_type("   ");
        let err = export_bitmap_font_with(&s, &json!({"layer": id}), |_, _| Ok(())).unwrap_err().to_string();
        assert!(err.contains("empty") || err.contains("charset"), "{err}");
    }

    #[test]
    fn enabled_only_for_text_sources() {
        let (mut s, _) = session_with_type("Hi");
        assert!(can_export(&s).is_ok());
        s.execute("layer.new.layer", json!({})).unwrap();
        assert!(can_export(&s).is_err());
    }

    #[test]
    fn offset_params_shift_fnt_metrics() {
        let (s, id) = session_with_type("A");
        let store: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let writer = store.clone();
        let base = export_bitmap_font_with(&s, &json!({"layer": id}), |name, bytes| {
            writer.lock().unwrap().insert(name.to_string(), bytes.to_vec());
            Ok(())
        })
        .unwrap();
        let base_fnt = String::from_utf8(store.lock().unwrap().get(base["fnt"].as_str().unwrap()).unwrap().clone()).unwrap();
        let parse_xy = |fnt: &str| -> (i32, i32) {
            let line = fnt.lines().find(|l| l.starts_with("char id=65 ")).unwrap();
            let mut x = 0;
            let mut y = 0;
            for part in line.split_whitespace() {
                if let Some(v) = part.strip_prefix("xoffset=") {
                    x = v.parse().unwrap();
                }
                if let Some(v) = part.strip_prefix("yoffset=") {
                    y = v.parse().unwrap();
                }
            }
            (x, y)
        };
        let (bx, by) = parse_xy(&base_fnt);
        store.lock().unwrap().clear();
        let writer = store.clone();
        let shifted = export_bitmap_font_with(
            &s,
            &json!({"layer": id, "offsetX": 3, "offsetY": -4, "charOffsets": {"A": {"x": 1, "y": 2}}}),
            |name, bytes| {
                writer.lock().unwrap().insert(name.to_string(), bytes.to_vec());
                Ok(())
            },
        )
        .unwrap();
        let shifted_fnt = String::from_utf8(store.lock().unwrap().get(shifted["fnt"].as_str().unwrap()).unwrap().clone()).unwrap();
        let (sx, sy) = parse_xy(&shifted_fnt);
        assert_eq!(sx, bx + 3 + 1);
        assert_eq!(sy, by - 4 + 2);
    }
}

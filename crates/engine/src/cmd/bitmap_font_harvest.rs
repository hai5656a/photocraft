//! Harvest BMFont glyphs from document layer pixels (PSD Photoshop cache + effects).
//!
//! Matches the psd2ui group pipeline: keep the ink as drawn in the document (do **not**
//! re-rasterize with the type engine), measure `xadvance` from neighbour ink spacing, and place
//! glyphs into a shared-height strip so `yoffset` preserves the PSD vertical layout.

use photocraft_doc::{Document, Layer, LayerContent, TextLayer};
use photocraft_geom::Rect;
use photocraft_text::bmfont::{FontMetrics, GlyphBitmap};

use crate::EngineError;
use crate::Result;

const ALPHA_INK: u8 = 4;
const BOTTOM_PAD: i32 = 8;

#[derive(Clone)]
struct Harvested {
    ch: char,
    /// Tight ink RGBA8.
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    /// Document Y of the ink top (before strip normalisation).
    band_top_doc: i32,
    xadvance: i32,
    /// Point size of the source layer (for same-script fill donors).
    size_pt: f32,
    /// Source layer name — used to pick a same-script style donor.
    source: String,
}

/// Build glyph bitmaps from the selected text layer or all-text group. Uses existing layer
/// caches (Photoshop pixels when the PSD stored them) and composites effects via compose.
/// Missing characters are first sought elsewhere in the document; remaining gaps are filled by
/// cloning a **same-script** style donor (CJK→CJK, digit→digit) so supplements keep the PSD
/// size and layer effects of that class — not a digit-sized stand-in for 千/万/亿.
pub fn harvest(doc: &Document, root: &Layer, charset: &[char]) -> Result<(FontMetrics, Vec<GlyphBitmap>)> {
    if charset.is_empty() {
        return Err(EngineError::BadParams { cmd: "file.export.bitmapFont".into(), msg: "charset is empty".into() });
    }
    let mut members: Vec<&Layer> = Vec::new();
    collect_text_layers(root, &mut members);
    if members.is_empty() {
        return Err(EngineError::BadParams { cmd: "file.export.bitmapFont".into(), msg: "no text layers to harvest".into() });
    }

    // Face size = largest member point size (info.size); style donor chosen per script later.
    let mut primary = members[0];
    let mut best = (text_size_pt(primary), has_fx(primary));
    for m in members.iter().skip(1) {
        let key = (text_size_pt(m), has_fx(m));
        if key > best {
            best = key;
            primary = m;
        }
    }
    let best_size = best.0;
    let dpi = if doc.resolution_dpi.is_finite() && doc.resolution_dpi > 0.0 { doc.resolution_dpi } else { 72.0 };
    let size_px = (best_size * dpi / 72.0).round().max(1.0) as i32;

    let canvas = doc.bounds();
    let member_ids: std::collections::BTreeSet<u64> = members.iter().map(|m| m.id.0).collect();
    let mut rendered: Vec<RenderedLayer> = Vec::with_capacity(members.len());
    for m in &members {
        if let Some(r) = render_member(doc, m, canvas) {
            rendered.push(r);
        }
    }

    let mut wanted: std::collections::BTreeSet<char> = charset.iter().copied().collect();
    let mut pool: Vec<Harvested> = Vec::new();
    for r in &rendered {
        slice_layer(r, &mut wanted, &mut pool, size_px);
    }

    // Cross-document harvest for chars the group did not paint (psd2ui specimen path).
    if !wanted.is_empty() {
        for (_path, _depth, layer) in doc.walk() {
            if !layer.visible || member_ids.contains(&layer.id.0) {
                continue;
            }
            if !matches!(layer.content, LayerContent::Text(_)) {
                continue;
            }
            if let Some(r) = render_member(doc, layer, canvas) {
                slice_layer(&r, &mut wanted, &mut pool, size_px);
                if wanted.is_empty() {
                    break;
                }
            }
        }
    }

    let mut by_char: std::collections::HashMap<char, Harvested> = std::collections::HashMap::new();
    for h in pool {
        match by_char.get(&h.ch) {
            Some(prev) if prev.w.saturating_mul(prev.h) >= h.w.saturating_mul(h.h) => {}
            _ => {
                by_char.insert(h.ch, h);
            }
        }
    }

    // Tiny punct harvested from a multi-char slice is often a 4–8 px speck (no glow). Drop it
    // so the fill path re-renders with the digit donor's layer effects — matching `，` / 图片字_1.
    let mut harvested_keys: std::collections::BTreeSet<char> = by_char.keys().copied().collect();
    for ch in charset.iter().copied().filter(|&c| is_tiny_punct(c)) {
        if by_char.get(&ch).is_some_and(|h| !punct_glyph_plausible(h, size_px)) {
            by_char.remove(&ch);
            harvested_keys.remove(&ch);
        }
    }

    // Style donors: one preferred layer per script class (digit / CJK / other).
    let donors = style_donors(&members, &by_char, primary);

    let missing: Vec<char> = charset.iter().copied().filter(|c| !by_char.contains_key(c)).collect();
    for ch in missing {
        let donor = donors.donor_for(ch).unwrap_or(primary);
        let mut filled = render_like_donor(doc, donor, ch, size_px);
        // Tiny punctuation (`.`, `,`) can fail the first donor pass — retry with primary.
        if filled.is_none() && is_tiny_punct(ch) && !std::ptr::eq(donor, primary) {
            filled = render_like_donor(doc, primary, ch, size_px);
        }
        if let Some(mut h) = filled {
            // Snap the fill onto the same baseline as harvested peers of its script (psd2ui
            // normalises supplements to the primary baseline — without this, a CJK fill keeps
            // the donor layer's document Y and floats above the digits in the atlas / runtime).
            if let Some(bot) = script_baseline(&by_char, &harvested_keys, script_class(ch)) {
                h.band_top_doc = bot.saturating_sub(h.h as i32);
            }
            by_char.insert(ch, h);
        }
    }
    let mut ordered: Vec<Harvested> = Vec::new();
    for &ch in charset {
        if let Some(h) = by_char.remove(&ch) {
            ordered.push(h);
        }
    }
    if ordered.is_empty() {
        return Err(EngineError::BadParams { cmd: "file.export.bitmapFont".into(), msg: "no glyphs could be harvested from layer pixels".into() });
    }

    // Digits 0–9: harvested specimens often keep tight PSD tracking (012) while fills use a
    // looser ink+pad formula (3–9). Unify side bearing so runtime spacing matches.
    // Run BEFORE expanding punct cells so xadvance stays based on tight ink, not the pad box.
    normalize_digit_advances(&mut ordered, size_px);

    // ASCII `.` with effects still trims to ~6×6; 图片字_1 keeps a ~40 px cell with the dot
    // near the bottom (yoffset≈50). Expand undersized punct into a peer-sized cell so the
    // atlas and runtime both show the mark on the baseline — not as an apostrophe at the top.
    expand_tiny_punct_cells(&mut ordered, size_px);

    // Shared baseline for the whole face: ink bottoms of harvested digits (fallback: all
    // harvested). Re-seat every glyph — including harvested CJK that sat higher in the PSD
    // specimen — so "1220亿" shares one baseline. Matches 图片字_1 (digits/亿 ≈ same bottom).
    let face_baseline = script_baseline_from_slice(&ordered, &harvested_keys, ScriptClass::Digit)
        .or_else(|| ordered.iter().filter(|h| harvested_keys.contains(&h.ch)).map(|h| h.band_top_doc.saturating_add(h.h as i32)).max())
        .unwrap_or(size_px);
    for h in &mut ordered {
        h.band_top_doc = face_baseline.saturating_sub(h.h as i32);
    }

    let mut strip_top = i32::MAX;
    let mut strip_bot = i32::MIN;
    for h in &ordered {
        strip_top = strip_top.min(h.band_top_doc);
        strip_bot = strip_bot.max(h.band_top_doc.saturating_add(h.h as i32));
    }
    if strip_top == i32::MAX {
        strip_top = 0;
        strip_bot = size_px;
    }
    let canvas_h = (strip_bot - strip_top + BOTTOM_PAD).max(1) as u32;

    // common.base = distance from strip top to the shared baseline.
    let base = (face_baseline - strip_top).clamp(0, canvas_h as i32);

    let mut glyphs = Vec::with_capacity(ordered.len());
    for h in ordered {
        let y_in_strip = (h.band_top_doc - strip_top).max(0) as u32;
        // Expanded punct cells keep intentional transparent top pad — trimming would shrink
        // `.` back to 6×6 and shove yoffset to ~lineHeight (looks like an apostrophe in atlas).
        let (rgba, w, hgt, trim_l, trim_t) = if is_tiny_punct(h.ch) {
            (h.rgba, h.w, h.h, 0_i32, y_in_strip as i32)
        } else {
            place_in_strip(&h.rgba, h.w, h.h, canvas_h, y_in_strip)
        };
        glyphs.push(GlyphBitmap {
            ch: h.ch,
            width: w,
            height: hgt,
            xoffset: trim_l,
            yoffset: trim_t,
            xadvance: h.xadvance.max(1),
            rgba,
        });
    }

    let metrics = FontMetrics { size: size_px, line_height: canvas_h as i32, base };
    Ok((metrics, glyphs))
}

/// Ink-bottom (doc Y) of harvested glyphs in `class`, falling back to digits then any harvested.
fn script_baseline(map: &std::collections::HashMap<char, Harvested>, harvested: &std::collections::BTreeSet<char>, class: ScriptClass) -> Option<i32> {
    let bot = |c: ScriptClass| {
        map.values().filter(|h| harvested.contains(&h.ch) && script_class(h.ch) == c).map(|h| h.band_top_doc.saturating_add(h.h as i32)).max()
    };
    bot(class).or_else(|| bot(ScriptClass::Digit)).or_else(|| map.values().filter(|h| harvested.contains(&h.ch)).map(|h| h.band_top_doc.saturating_add(h.h as i32)).max())
}

fn script_baseline_from_slice(glyphs: &[Harvested], harvested: &std::collections::BTreeSet<char>, class: ScriptClass) -> Option<i32> {
    glyphs.iter().filter(|h| harvested.contains(&h.ch) && script_class(h.ch) == class).map(|h| h.band_top_doc.saturating_add(h.h as i32)).max()
}

fn is_tiny_punct(c: char) -> bool {
    matches!(c, '.' | ',' | '\'' | '’' | '\u{ff0c}' | '\u{3002}' | '\u{ff0e}')
}

/// A plausible punct glyph at `size_px` must cover more than a noise speck.
/// Face size 100 → min edge 10; a 6×6 harvested slice fails and is re-filled.
fn punct_glyph_plausible(h: &Harvested, size_px: i32) -> bool {
    let min_dim = ((size_px as u32) / 10).max(4);
    h.w.max(h.h) >= min_dim
}

/// Grow undersized `.` / `,` into a cell like 图片字_1's period (≈40% of face size, ink at bottom).
/// Prefer a sibling punct that already has effects (e.g. fullwidth `，`) as the height peer.
fn expand_tiny_punct_cells(glyphs: &mut [Harvested], size_px: i32) {
    let default_h = ((size_px as u32) * 2 / 5).max(16);
    let default_w = ((size_px as u32) / 4).max(12);
    let peer_h = glyphs
        .iter()
        .filter(|h| is_tiny_punct(h.ch) && punct_glyph_plausible(h, size_px))
        .map(|h| h.h)
        .max()
        .unwrap_or(default_h)
        .max(default_h);
    let peer_w = glyphs
        .iter()
        .filter(|h| is_tiny_punct(h.ch) && punct_glyph_plausible(h, size_px))
        .map(|h| h.w)
        .max()
        .unwrap_or(default_w)
        .max(default_w);

    for h in glyphs.iter_mut() {
        if !is_tiny_punct(h.ch) {
            continue;
        }
        let th = peer_h.max(h.h);
        let tw = peer_w.max(h.w);
        if tw == h.w && th == h.h {
            continue;
        }
        let mut out = vec![0u8; (tw as usize).saturating_mul(th as usize).saturating_mul(4)];
        let ox = ((tw.saturating_sub(h.w)) / 2) as usize;
        let oy = th.saturating_sub(h.h) as usize;
        let sw = h.w as usize;
        let sh = h.h as usize;
        for row in 0..sh {
            for col in 0..sw {
                let si = (row.saturating_mul(sw).saturating_add(col)).saturating_mul(4);
                let di = ((oy + row).saturating_mul(tw as usize).saturating_add(ox + col)).saturating_mul(4);
                if let (Some(s), Some(d)) = (h.rgba.get(si..si + 4), out.get_mut(di..di + 4)) {
                    d.copy_from_slice(s);
                }
            }
        }
        h.rgba = out;
        h.w = tw;
        h.h = th;
    }
}

fn is_ascii_digit_glyph(c: char) -> bool {
    c.is_ascii_digit()
}

/// Give every ASCII digit the same side bearing: `width + pad`, with `pad` from the face size.
/// Harvested 012 often keep PSD tracking that is much tighter than fills for 3–9.
fn normalize_digit_advances(glyphs: &mut [Harvested], size_px: i32) {
    let pad = (size_px * 8 / 100).max(2);
    let punct_pad = (pad / 2).max(1);
    for h in glyphs.iter_mut() {
        if is_ascii_digit_glyph(h.ch) {
            h.xadvance = (h.w as i32).saturating_add(pad).max(1);
        } else if is_tiny_punct(h.ch) {
            h.xadvance = (h.w as i32).saturating_add(punct_pad).max(1);
        }
    }
}

fn has_fx(layer: &Layer) -> bool {
    layer.effects.enabled && layer.effects.items.iter().any(photocraft_doc::Effect::enabled)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScriptClass {
    Digit,
    Cjk,
    Other,
}

fn script_class(c: char) -> ScriptClass {
    if c.is_ascii_digit() || matches!(c, '.' | ',' | '\u{ff0c}' | '\u{3002}' | '\'' | '’') {
        ScriptClass::Digit
    } else if is_cjk(c) {
        ScriptClass::Cjk
    } else {
        ScriptClass::Other
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2CEAF)
}

struct StyleDonors<'a> {
    digit: Option<&'a Layer>,
    cjk: Option<&'a Layer>,
    other: Option<&'a Layer>,
    primary: &'a Layer,
}

impl<'a> StyleDonors<'a> {
    fn donor_for(&self, ch: char) -> Option<&'a Layer> {
        let pref = match script_class(ch) {
            ScriptClass::Digit => self.digit.or(self.other),
            ScriptClass::Cjk => self.cjk.or(self.other),
            ScriptClass::Other => self.other.or(self.digit),
        };
        pref.or(Some(self.primary))
    }
}

/// Prefer a same-script member that has effects; fall back to a harvested source of that script.
fn style_donors<'a>(members: &[&'a Layer], harvested: &std::collections::HashMap<char, Harvested>, primary: &'a Layer) -> StyleDonors<'a> {
    let pick = |class: ScriptClass| -> Option<&'a Layer> {
        let mut best: Option<(&'a Layer, i32)> = None;
        for m in members {
            let text = match &m.content {
                LayerContent::Text(t) => t.text.as_str(),
                _ => continue,
            };
            if !text.chars().any(|c| script_class(c) == class) {
                continue;
            }
            let score = (has_fx(m) as i32) * 1_000_000 + (text_size_pt(m) * 100.0) as i32;
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((*m, score));
            }
        }
        if best.is_some() {
            return best.map(|(l, _)| l);
        }
        // Fall back: member that produced a harvested glyph of this class (prefer larger size).
        let mut from_harvest: Option<(&'a Layer, i32)> = None;
        for h in harvested.values() {
            if script_class(h.ch) != class {
                continue;
            }
            if let Some(m) = members.iter().find(|m| m.name == h.source) {
                let score = (h.size_pt * 100.0) as i32;
                if from_harvest.is_none_or(|(_, s)| score > s) {
                    from_harvest = Some((*m, score));
                }
            }
        }
        from_harvest.map(|(l, _)| l)
    };
    StyleDonors { digit: pick(ScriptClass::Digit), cjk: pick(ScriptClass::Cjk), other: pick(ScriptClass::Other), primary }
}

/// Clone `donor` (keeping effects / opacity / blend / point size), set its text to one character,
/// re-render type pixels, then composite with effects.
fn render_like_donor(doc: &Document, donor: &Layer, ch: char, face_size_px: i32) -> Option<Harvested> {
    let mut clone = donor.clone();
    let donor_size = text_size_pt(donor);
    let LayerContent::Text(t) = &mut clone.content else { return None };
    // Keep the donor's character style (family, weight, colour) and point size — only swap glyphs.
    let style = t.char_runs().into_iter().next().map(|r| r.style).unwrap_or_else(|| photocraft_doc::text::CharStyle {
        font_family: t.font_family.clone(),
        size_pt: donor_size,
        color: t.color,
        ..Default::default()
    });
    // Fullwidth punct → ASCII so the typeface's common `.` / `,` glyphs are used.
    let draw = match ch {
        '\u{ff0e}' => '.',
        '\u{3002}' => '.',
        '\u{ff0c}' => ',',
        other => other,
    };
    t.text = draw.to_string();
    t.runs = vec![photocraft_doc::text::TextRun { len: t.text.len(), style }];
    t.size_pt = donor_size;
    t.sync_summary();
    t.cache = None;
    t.psd_raw = None;
    crate::type_cmds::refresh(doc, t);
    let canvas = doc.bounds();
    // Inflate bounds — a period's ink is tiny and effects (outer glow) sit outside the type box.
    let area = photocraft_compose::composite_bounds(&clone, canvas)
        .unwrap_or_else(|| photocraft_compose::layer_bounds(&clone, canvas))
        .inflate(face_size_px.max(8));
    let area = area.intersect(&canvas);
    if area.is_empty() {
        return None;
    }
    let buf = photocraft_compose::render_layer_in(doc, &clone, area);
    // Soft AA on a 1–2 px period can sit under ALPHA_INK=4 — retry with threshold 1.
    let (rgba, ink) = buffer_to_tight_rgba8_thresh(&buf, ALPHA_INK).or_else(|| buffer_to_tight_rgba8_thresh(&buf, 1))?;
    if rgba.is_empty() || ink.is_empty() {
        return None;
    }
    let adv = ink.width().max(1) as i32 + (face_size_px * 8 / 100).max(2);
    Some(Harvested {
        ch, // keep the requested codepoint (fullwidth maps still export as the requested id)
        rgba,
        w: ink.width() as u32,
        h: ink.height() as u32,
        band_top_doc: ink.y0,
        xadvance: adv,
        size_pt: donor_size,
        source: donor.name.clone(),
    })
}

struct RenderedLayer {
    name: String,
    text: String,
    size_pt: f32,
    /// Full layer composite as straight RGBA8 over `ink` (tight).
    rgba: Vec<u8>,
    ink: Rect,
}

fn collect_text_layers<'a>(layer: &'a Layer, out: &mut Vec<&'a Layer>) {
    match &layer.content {
        LayerContent::Text(_) => out.push(layer),
        LayerContent::Group(g) => {
            for c in &g.children {
                if c.visible {
                    collect_text_layers(c, out);
                }
            }
        }
        _ => {}
    }
}

fn text_size_pt(layer: &Layer) -> f32 {
    match &layer.content {
        LayerContent::Text(t) => primary_size(t),
        _ => 12.0,
    }
}

fn primary_size(t: &TextLayer) -> f32 {
    t.char_runs().into_iter().next().map(|r| r.style.size_pt).unwrap_or(t.size_pt).max(1.0)
}

fn render_member(doc: &Document, layer: &Layer, canvas: Rect) -> Option<RenderedLayer> {
    let text = match &layer.content {
        LayerContent::Text(t) => t.text.clone(),
        _ => return None,
    };
    let size_pt = text_size_pt(layer);
    // Do not refresh — keep Photoshop's cache when present.
    let area = photocraft_compose::composite_bounds(layer, canvas).unwrap_or_else(|| photocraft_compose::layer_bounds(layer, canvas));
    if area.is_empty() {
        return None;
    }
    let buf = photocraft_compose::render_layer_in(doc, layer, area);
    let (rgba, ink) = buffer_to_tight_rgba8(&buf)?;
    Some(RenderedLayer { name: layer.name.clone(), text, size_pt, rgba, ink })
}

fn buffer_to_tight_rgba8(buf: &photocraft_compose::Buffer) -> Option<(Vec<u8>, Rect)> {
    buffer_to_tight_rgba8_thresh(buf, ALPHA_INK)
}

fn buffer_to_tight_rgba8_thresh(buf: &photocraft_compose::Buffer, alpha_min: u8) -> Option<(Vec<u8>, Rect)> {
    if buf.rect.is_empty() || buf.px.is_empty() {
        return None;
    }
    let w = buf.rect.width() as usize;
    let h = buf.rect.height() as usize;
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            let a = (buf.px[y * w + x][3] * 255.0).round().clamp(0.0, 255.0) as u8;
            if a > alpha_min {
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if !any {
        return None;
    }
    let tw = max_x.saturating_sub(min_x).saturating_add(1);
    let th = max_y.saturating_sub(min_y).saturating_add(1);
    let mut rgba = vec![0u8; tw.saturating_mul(th).saturating_mul(4)];
    for y in 0..th {
        for x in 0..tw {
            let p = buf.px[(min_y + y) * w + (min_x + x)];
            let i = (y * tw + x) * 4;
            rgba[i] = (p[0] * 255.0).round().clamp(0.0, 255.0) as u8;
            rgba[i + 1] = (p[1] * 255.0).round().clamp(0.0, 255.0) as u8;
            rgba[i + 2] = (p[2] * 255.0).round().clamp(0.0, 255.0) as u8;
            rgba[i + 3] = (p[3] * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    let ink = Rect::new(
        buf.rect.x0.saturating_add(min_x as i32),
        buf.rect.y0.saturating_add(min_y as i32),
        buf.rect.x0.saturating_add(min_x as i32).saturating_add(tw as i32),
        buf.rect.y0.saturating_add(min_y as i32).saturating_add(th as i32),
    );
    Some((rgba, ink))
}

fn slice_layer(r: &RenderedLayer, wanted: &mut std::collections::BTreeSet<char>, out: &mut Vec<Harvested>, face_size_px: i32) {
    let chars: Vec<char> = r.text.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.is_empty() {
        return;
    }
    if !chars.iter().any(|c| wanted.contains(c)) {
        return;
    }
    let slices = column_slices(&r.rgba, r.ink.width() as usize, r.ink.height() as usize, chars.len());
    if slices.len() != chars.len() {
        // Fallback: whole layer as one glyph when it is a single wanted char.
        if chars.len() == 1 && wanted.contains(&chars[0]) {
            let adv = r.ink.width().max(1) as i32 + (face_size_px * 8 / 100).max(2);
            out.push(Harvested {
                ch: chars[0],
                rgba: r.rgba.clone(),
                w: r.ink.width() as u32,
                h: r.ink.height() as u32,
                band_top_doc: r.ink.y0,
                xadvance: adv,
                size_pt: r.size_pt,
                source: r.name.clone(),
            });
            wanted.remove(&chars[0]);
        }
        return;
    }
    for (i, &ch) in chars.iter().enumerate() {
        if !wanted.contains(&ch) {
            continue;
        }
        let (x0, x1) = slices[i];
        let tile_w = x1.saturating_sub(x0).saturating_add(1);
        let tile_h = r.ink.height() as usize;
        let mut tile = vec![0u8; tile_w.saturating_mul(tile_h).saturating_mul(4)];
        let src_w = r.ink.width() as usize;
        for y in 0..tile_h {
            for x in 0..tile_w {
                let si = (y * src_w + x0 + x) * 4;
                let di = (y * tile_w + x) * 4;
                if let (Some(s), Some(d)) = (r.rgba.get(si..si + 4), tile.get_mut(di..di + 4)) {
                    d.copy_from_slice(s);
                }
            }
        }
        // Trim vertical transparent rows inside the tile for inkH, but keep full strip placement
        // via band_top; horizontal trim is applied later in place_in_strip's sibling trim.
        let (trim, tw, th, left, top) = trim_rgba(&tile, tile_w as u32, tile_h as u32);
        let xadvance = advance_for_slice(&slices, i, face_size_px);
        out.push(Harvested {
            ch,
            rgba: trim,
            w: tw,
            h: th,
            band_top_doc: r.ink.y0.saturating_add(top),
            xadvance,
            size_pt: r.size_pt,
            source: r.name.clone(),
        });
        // Account for left trim in band — xoffset absorbed at strip stage via re-trim.
        let _ = left;
        wanted.remove(&ch);
    }
}

/// Column ranges `[x0, x1]` inclusive for `n` glyphs inside a tight ink bitmap.
fn column_slices(rgba: &[u8], w: usize, h: usize, n: usize) -> Vec<(usize, usize)> {
    if n == 0 || w == 0 || h == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![(0, w.saturating_sub(1))];
    }
    let mut col_ink = vec![false; w];
    for x in 0..w {
        for y in 0..h {
            let a = rgba.get((y * w + x) * 4 + 3).copied().unwrap_or(0);
            if a > ALPHA_INK {
                col_ink[x] = true;
                break;
            }
        }
    }
    // Contiguous ink runs.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < w {
        if !col_ink[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < w && col_ink[i] {
            i += 1;
        }
        runs.push((start, i.saturating_sub(1)));
    }
    if runs.len() == n {
        return runs;
    }
    // Stuck-together: split the full ink span into n equal parts at projection minima.
    let Some(first) = col_ink.iter().position(|&v| v) else {
        return Vec::new();
    };
    let Some(last) = col_ink.iter().rposition(|&v| v) else {
        return Vec::new();
    };
    let span = last.saturating_sub(first).saturating_add(1);
    if span < n {
        return Vec::new();
    }
    let mut cuts = Vec::with_capacity(n.saturating_add(1));
    cuts.push(first);
    for k in 1..n {
        let ideal = first + span * k / n;
        // Search a window for the sparsest column near the ideal cut.
        let lo = ideal.saturating_sub(span / (n * 2).max(1));
        let hi = (ideal + span / (n * 2).max(1)).min(last);
        let mut best = ideal.clamp(first, last);
        let mut best_score = u32::MAX;
        for x in lo..=hi {
            let mut score = 0u32;
            for y in 0..h {
                score = score.saturating_add(u32::from(rgba.get((y * w + x) * 4 + 3).copied().unwrap_or(0)));
            }
            if score < best_score {
                best_score = score;
                best = x;
            }
        }
        cuts.push(best);
    }
    cuts.push(last.saturating_add(1));
    cuts.sort_unstable();
    cuts.dedup();
    if cuts.len() != n + 1 {
        // Equal split fallback.
        let mut out = Vec::with_capacity(n);
        for k in 0..n {
            let a = first + span * k / n;
            let b = first + span * (k + 1) / n - 1;
            out.push((a, b.max(a)));
        }
        return out;
    }
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let a = cuts[k];
        let b = cuts[k + 1].saturating_sub(1).max(a);
        out.push((a, b));
    }
    out
}

fn advance_for_slice(slices: &[(usize, usize)], i: usize, face_size_px: i32) -> i32 {
    let n = slices.len();
    let (x0, x1) = slices[i];
    let ink_w = (x1.saturating_sub(x0).saturating_add(1)) as i32;
    if n == 1 {
        return ink_w.saturating_add((face_size_px * 8 / 100).max(2));
    }
    if i + 1 < n {
        // Seam midpoint between this and next (psd2ui).
        let next_x0 = slices[i + 1].0;
        let seam = (x1 + 1 + next_x0).saturating_sub(1) / 2;
        return (seam.saturating_add(1).saturating_sub(x0) as i32).max(1);
    }
    // Last: mirror previous gap.
    if i > 0 {
        let (px0, px1) = slices[i - 1];
        let prev_w = (px1.saturating_sub(px0).saturating_add(1)) as i32;
        let gap = x0 as i32 - px0 as i32 - prev_w;
        return ink_w.saturating_add(gap).max(1);
    }
    ink_w.max(1)
}

fn trim_rgba(rgba: &[u8], w: u32, h: u32) -> (Vec<u8>, u32, u32, i32, i32) {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 {
        return (Vec::new(), 0, 0, 0, 0);
    }
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            if rgba.get((y * w + x) * 4 + 3).copied().unwrap_or(0) > ALPHA_INK {
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if !any {
        return (Vec::new(), 0, 0, 0, 0);
    }
    let tw = max_x.saturating_sub(min_x).saturating_add(1);
    let th = max_y.saturating_sub(min_y).saturating_add(1);
    let mut out = vec![0u8; tw.saturating_mul(th).saturating_mul(4)];
    for y in 0..th {
        for x in 0..tw {
            let si = ((min_y + y) * w + (min_x + x)) * 4;
            let di = (y * tw + x) * 4;
            if let (Some(s), Some(d)) = (rgba.get(si..si + 4), out.get_mut(di..di + 4)) {
                d.copy_from_slice(s);
            }
        }
    }
    (out, tw as u32, th as u32, min_x as i32, min_y as i32)
}

/// Paint a glyph into a full-height strip then trim; returns (rgba, w, h, xoffset, yoffset).
fn place_in_strip(src: &[u8], sw: u32, sh: u32, canvas_h: u32, y: u32) -> (Vec<u8>, u32, u32, i32, i32) {
    let y = y.min(canvas_h.saturating_sub(1));
    let mut strip = vec![0u8; (sw as usize).saturating_mul(canvas_h as usize).saturating_mul(4)];
    let copy_h = sh.min(canvas_h.saturating_sub(y));
    for row in 0..copy_h as usize {
        let si = row.saturating_mul(sw as usize).saturating_mul(4);
        let di = (y as usize + row).saturating_mul(sw as usize).saturating_mul(4);
        let n = (sw as usize).saturating_mul(4);
        if let (Some(s), Some(d)) = (src.get(si..si.saturating_add(n)), strip.get_mut(di..di.saturating_add(n))) {
            d.copy_from_slice(s);
        }
    }
    trim_rgba(&strip, sw, canvas_h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_doc::TextLayer;
    use serde_json::json;

    #[test]
    fn harvests_single_char_layers_with_psd_size() {
        let mut s = crate::Session::new();
        s.execute("file.new", json!({"width": 400, "height": 200})).unwrap();
        let a = s.execute("type.create", json!({"text": "A", "size": 48, "x": 10, "y": 80})).unwrap()["layer"].as_u64().unwrap();
        let b = s.execute("type.create", json!({"text": "B", "size": 48, "x": 80, "y": 80})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.select", json!({"layer": a})).unwrap();
        s.execute("layer.select", json!({"layer": b, "mode": "add"})).unwrap();
        let g = s.execute("layer.new.groupFromLayers", json!({"name": "Font"})).unwrap();
        let gid = g.get("layer").and_then(|v| v.as_u64()).or_else(|| s.active().and_then(|d| d.active_layer.map(|l| l.0))).unwrap();
        let d = s.active().unwrap();
        let layer = d.doc.layer(photocraft_doc::LayerId(gid)).unwrap();
        let (metrics, glyphs) = harvest(&d.doc, layer, &['A', 'B']).unwrap();
        assert_eq!(metrics.size, 48);
        assert_eq!(glyphs.len(), 2);
        assert!(glyphs.iter().all(|g| g.width > 0 && g.height > 0 && g.xadvance > 0));
        // Strip yoffsets should be small and similar for same baseline.
        let ys: Vec<_> = glyphs.iter().map(|g| g.yoffset).collect();
        assert!(ys.iter().all(|&y| y >= 0), "{ys:?}");
    }

    #[test]
    fn fills_missing_chars_like_primary_member() {
        let mut s = crate::Session::new();
        s.execute("file.new", json!({"width": 400, "height": 200})).unwrap();
        // Only "0" is painted; export charset also asks for "1".
        s.execute("type.create", json!({"text": "0", "size": 40, "x": 20, "y": 100})).unwrap();
        let id = s.active().unwrap().active_layer.unwrap();
        let d = s.active().unwrap();
        let layer = d.doc.layer(id).unwrap();
        let (metrics, glyphs) = harvest(&d.doc, layer, &['0', '1']).unwrap();
        assert_eq!(metrics.size, 40);
        assert_eq!(glyphs.len(), 2);
        assert!(glyphs.iter().any(|g| g.ch == '0' && g.width > 0));
        assert!(glyphs.iter().any(|g| g.ch == '1' && g.width > 0 && g.height > 0));
    }

    #[test]
    fn cjk_fill_uses_cjk_donor_size_not_digit() {
        let mut s = crate::Session::new();
        s.execute("file.new", json!({"width": 600, "height": 300})).unwrap();
        let digit = s.execute("type.create", json!({"text": "0", "size": 80, "x": 20, "y": 120})).unwrap()["layer"].as_u64().unwrap();
        let cjk = s.execute("type.create", json!({"text": "亿", "size": 48, "x": 120, "y": 120})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.select", json!({"layer": digit})).unwrap();
        s.execute("layer.select", json!({"layer": cjk, "mode": "add"})).unwrap();
        let g = s.execute("layer.new.groupFromLayers", json!({"name": "Units"})).unwrap();
        let gid = g.get("layer").and_then(|v| v.as_u64()).or_else(|| s.active().and_then(|d| d.active_layer.map(|l| l.0))).unwrap();
        let d = s.active().unwrap();
        let layer = d.doc.layer(photocraft_doc::LayerId(gid)).unwrap();
        // 万 is missing — must be filled from the CJK donor (亿 @ 48), not the digit @ 80.
        let (_metrics, glyphs) = harvest(&d.doc, layer, &['0', '亿', '万']).unwrap();
        let yi = glyphs.iter().find(|g| g.ch == '亿').unwrap();
        let wan = glyphs.iter().find(|g| g.ch == '万').unwrap();
        let zero = glyphs.iter().find(|g| g.ch == '0').unwrap();
        // Filled 万 should be near 亿's height, not near the much taller digit.
        let dh = (wan.height as i32 - yi.height as i32).unsigned_abs();
        assert!(dh <= yi.height / 3 + 4, "万 h={} vs 亿 h={} (0 h={})", wan.height, yi.height, zero.height);
        assert!(wan.height + 8 < zero.height, "万 should stay smaller than digit 0");
    }

    #[test]
    fn period_is_filled_and_digits_share_side_bearing() {
        let mut s = crate::Session::new();
        s.execute("file.new", json!({"width": 400, "height": 200})).unwrap();
        // Only 0 and 1 painted tightly; `.` and 2 must be filled — advances unified.
        let a = s.execute("type.create", json!({"text": "0", "size": 50, "x": 10, "y": 100})).unwrap()["layer"].as_u64().unwrap();
        let b = s.execute("type.create", json!({"text": "1", "size": 50, "x": 40, "y": 100})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.select", json!({"layer": a})).unwrap();
        s.execute("layer.select", json!({"layer": b, "mode": "add"})).unwrap();
        let g = s.execute("layer.new.groupFromLayers", json!({"name": "Dots"})).unwrap();
        let gid = g.get("layer").and_then(|v| v.as_u64()).or_else(|| s.active().and_then(|d| d.active_layer.map(|l| l.0))).unwrap();
        let d = s.active().unwrap();
        let layer = d.doc.layer(photocraft_doc::LayerId(gid)).unwrap();
        let (m, glyphs) = harvest(&d.doc, layer, &['0', '1', '2', '.']).unwrap();
        let dot = glyphs.iter().find(|g| g.ch == '.').expect("period glyph");
        assert!(dot.width > 0 && dot.height > 0 && !dot.rgba.is_empty(), "period must have ink");
        // Cell must be a substantial fraction of the face (not a 6×6 speck / apostrophe).
        let min_h = ((m.size as u32) * 2 / 5).max(16);
        assert!(dot.height >= min_h, "period cell h={} < min {min_h} (face {})", dot.height, m.size);
        let zero = glyphs.iter().find(|g| g.ch == '0').unwrap();
        let bottom = |g: &GlyphBitmap| g.yoffset.saturating_add(g.height as i32);
        assert!((bottom(dot) - bottom(zero)).unsigned_abs() <= 6, "period bottom {} vs 0 bottom {}", bottom(dot), bottom(zero));
        assert!(dot.yoffset > zero.yoffset, "period should sit lower in the strip than digit 0");
        let two = glyphs.iter().find(|g| g.ch == '2').unwrap();
        let pad0 = zero.xadvance - zero.width as i32;
        let pad2 = two.xadvance - two.width as i32;
        assert_eq!(pad0, pad2, "digit side bearings unified: 0 pad={pad0} 2 pad={pad2}");
        assert!(pad0 >= 2, "side bearing at least 2, got {pad0}");
    }

    #[test]
    fn undersized_period_speck_is_expanded_to_baseline_cell() {
        // Simulate a bad harvest: only digits present; `.` is filled then expanded.
        let mut s = crate::Session::new();
        s.execute("file.new", json!({"width": 500, "height": 240})).unwrap();
        let a = s.execute("type.create", json!({"text": "9", "size": 80, "x": 20, "y": 140})).unwrap()["layer"].as_u64().unwrap();
        // Fullwidth comma — a large punct peer the expander should match.
        let b = s.execute("type.create", json!({"text": "，", "size": 80, "x": 120, "y": 140})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.select", json!({"layer": a})).unwrap();
        s.execute("layer.select", json!({"layer": b, "mode": "add"})).unwrap();
        let g = s.execute("layer.new.groupFromLayers", json!({"name": "Speck"})).unwrap();
        let gid = g.get("layer").and_then(|v| v.as_u64()).or_else(|| s.active().and_then(|d| d.active_layer.map(|l| l.0))).unwrap();
        let d = s.active().unwrap();
        let layer = d.doc.layer(photocraft_doc::LayerId(gid)).unwrap();
        let (_m, glyphs) = harvest(&d.doc, layer, &['9', '.', '，']).unwrap();
        let dot = glyphs.iter().find(|g| g.ch == '.').expect("period");
        let comma = glyphs.iter().find(|g| g.ch == '，').expect("comma");
        assert!(dot.height + 4 >= comma.height, "period h={} should match comma h={}", dot.height, comma.height);
        let bottom = |g: &GlyphBitmap| g.yoffset.saturating_add(g.height as i32);
        assert!((bottom(dot) - bottom(comma)).unsigned_abs() <= 4);
    }

    #[test]
    fn filled_and_cjk_share_digit_baseline() {
        let mut s = crate::Session::new();
        // Digit low on the canvas; CJK specimen much higher — without baseline snap the fill floats.
        s.execute("file.new", json!({"width": 600, "height": 400})).unwrap();
        let digit = s.execute("type.create", json!({"text": "0", "size": 60, "x": 20, "y": 300})).unwrap()["layer"].as_u64().unwrap();
        let cjk = s.execute("type.create", json!({"text": "亿", "size": 48, "x": 100, "y": 80})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.select", json!({"layer": digit})).unwrap();
        s.execute("layer.select", json!({"layer": cjk, "mode": "add"})).unwrap();
        let g = s.execute("layer.new.groupFromLayers", json!({"name": "Baseline"})).unwrap();
        let gid = g.get("layer").and_then(|v| v.as_u64()).or_else(|| s.active().and_then(|d| d.active_layer.map(|l| l.0))).unwrap();
        let d = s.active().unwrap();
        let layer = d.doc.layer(photocraft_doc::LayerId(gid)).unwrap();
        let (_m, glyphs) = harvest(&d.doc, layer, &['0', '亿', '万']).unwrap();
        let zero = glyphs.iter().find(|g| g.ch == '0').unwrap();
        let yi = glyphs.iter().find(|g| g.ch == '亿').unwrap();
        let wan = glyphs.iter().find(|g| g.ch == '万').unwrap();
        // Bottoms (yoffset + height) should agree within a few px after baseline normalisation.
        let bottom = |g: &GlyphBitmap| g.yoffset.saturating_add(g.height as i32);
        let b0 = bottom(zero);
        assert!((bottom(yi) - b0).unsigned_abs() <= 6, "亿 bottom {} vs 0 bottom {}", bottom(yi), b0);
        assert!((bottom(wan) - b0).unsigned_abs() <= 6, "万 bottom {} vs 0 bottom {}", bottom(wan), b0);
        // Shorter CJK must get a larger yoffset than the digit (not sit at the strip top).
        assert!(yi.yoffset >= zero.yoffset, "亿 yoffset {} < 0 yoffset {}", yi.yoffset, zero.yoffset);
        assert!(wan.yoffset >= zero.yoffset, "万 yoffset {} < 0 yoffset {}", wan.yoffset, zero.yoffset);
    }

    #[test]
    fn column_slices_splits_two_blobs() {
        // 4×2 image: left 2 cols opaque, gap, right 2 cols opaque.
        let mut rgba = vec![0u8; 4 * 2 * 4];
        for y in 0..2 {
            for x in [0usize, 1, 3] {
                let i = (y * 4 + x) * 4;
                rgba[i + 3] = 255;
            }
        }
        // Only two ink runs if col2 empty — wait we set 0,1,3. Runs: [0..=1], [3..=3].
        let s = column_slices(&rgba, 4, 2, 2);
        assert_eq!(s.len(), 2, "{s:?}");
    }

    #[test]
    fn text_layer_helper_compiles() {
        let t = TextLayer { text: "x".into(), size_pt: 12.0, ..Default::default() };
        assert_eq!(primary_size(&t), 12.0);
    }
}

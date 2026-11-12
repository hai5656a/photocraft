//! AngelCode BMFont (text `.fnt` + atlas PNG pages) from a typeface and charset.
//!
//! Phase 1: glyphs come from the type engine (font outlines → coverage), never from document
//! pixels. Callers encode the atlas pages and write the files.

use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::TextLayer;
use photocraft_doc::text::{AntiAlias, CharStyle, TextRun};
use photocraft_geom::Affine;

use crate::TextEngine;

/// Hard caps so adversarial params cannot force huge allocations.
pub const MAX_PAGE: u32 = 4096;
pub const MIN_PAGE: u32 = 64;
pub const MAX_PADDING: u32 = 16;
pub const MAX_CHARS: usize = 4096;

/// One rasterized glyph ready for atlas packing.
#[derive(Clone, Debug)]
pub struct GlyphBitmap {
    pub ch: char,
    pub width: u32,
    pub height: u32,
    /// Distance from the pen to the left of the bitmap (BMFont `xoffset`).
    pub xoffset: i32,
    /// Distance from the line top to the top of the bitmap (BMFont `yoffset`).
    pub yoffset: i32,
    pub xadvance: i32,
    /// Straight RGBA8, row-major (`width * height` pixels). Empty when `width` or `height` is 0.
    pub rgba: Vec<u8>,
}

/// Shared line metrics for a BMFont face at one size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontMetrics {
    /// BMFont `info.size` (pixels per em, rounded).
    pub size: i32,
    pub line_height: i32,
    /// Distance from the line top to the baseline (BMFont `common.base`).
    pub base: i32,
}

/// One packed atlas page (RGBA8).
#[derive(Clone, Debug)]
pub struct AtlasPage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A glyph's placement on an atlas page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedGlyph {
    pub ch: char,
    pub page: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub xoffset: i32,
    pub yoffset: i32,
    pub xadvance: i32,
}

/// Packed BMFont in memory (descriptor + atlas pages). No I/O.
#[derive(Clone, Debug)]
pub struct BitmapFont {
    pub face: String,
    pub metrics: FontMetrics,
    pub padding: u32,
    pub glyphs: Vec<PackedGlyph>,
    pub pages: Vec<AtlasPage>,
}

/// Parameters for [`build`] / [`export_memory`].
#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub face: String,
    pub dpi: f32,
    pub page_w: u32,
    pub page_h: u32,
    pub padding: u32,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self { face: "BitmapFont".into(), dpi: 72.0, page_w: 1024, page_h: 1024, padding: 1 }
    }
}

/// Why building a bitmap font failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BmfontError {
    EmptyCharset,
    TooManyChars(usize),
    InvalidPageSize(u32, u32),
    InvalidPadding(u32),
    GlyphTooLarge { ch: char, width: u32, height: u32, page_w: u32, page_h: u32 },
    NoGlyphs,
}

impl std::fmt::Display for BmfontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCharset => write!(f, "charset is empty after removing whitespace"),
            Self::TooManyChars(n) => write!(f, "charset has {n} characters; at most {MAX_CHARS} are allowed"),
            Self::InvalidPageSize(w, h) => write!(f, "page size {w}×{h} is outside {MIN_PAGE}..={MAX_PAGE}"),
            Self::InvalidPadding(p) => write!(f, "padding {p} is outside 0..={MAX_PADDING}"),
            Self::GlyphTooLarge { ch, width, height, page_w, page_h } => {
                write!(f, "glyph '{ch}' ({width}×{height}) does not fit a {page_w}×{page_h} page")
            }
            Self::NoGlyphs => write!(f, "no glyphs could be rasterized"),
        }
    }
}

/// Unique non-whitespace characters of `text`, first occurrence kept.
pub fn charset(text: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        if seen.insert(c) {
            out.push(c);
        }
    }
    out
}

/// Rasterizes one character with `style` at `dpi` (pixels = `size_pt * dpi / 72`).
pub fn rasterize_char(engine: &mut TextEngine, style: &CharStyle, ch: char, dpi: f32) -> GlyphBitmap {
    let dpi = if dpi.is_finite() && dpi > 0.0 { dpi } else { 72.0 };
    let mut st = style.clone();
    st.underline = false;
    st.strikethrough = false;
    st.tracking = 0.0;
    st.baseline_shift_pt = 0.0;
    st.kern = 0.0;
    let text = ch.to_string();
    let layer = TextLayer {
        runs: vec![TextRun { len: text.len(), style: st }],
        text,
        antialias: AntiAlias::Smooth,
        ..Default::default()
    };
    let layout = engine.layout(&layer, dpi);
    let metrics = line_metrics(&layout);
    let xadvance = layout.clusters.first().map(|c| c.advance.round() as i32).unwrap_or(0).max(0);
    let pen_x = layout.glyphs.first().map(|g| g.x).unwrap_or(0.0);
    let baseline = layout.lines.first().map(|l| l.baseline).unwrap_or(0.0);

    let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::U8, true);
    let rendered = crate::render::rasterize(&layout, &Affine::IDENTITY, fmt, AntiAlias::Smooth);
    let ink = rendered.rect;
    if ink.is_empty() {
        return GlyphBitmap { ch, width: 0, height: 0, xoffset: 0, yoffset: metrics.base, xadvance, rgba: Vec::new() };
    }

    let w = ink.width() as u32;
    let h = ink.height() as u32;
    let mut rgba_px = vec![[0u8; 4]; (w as usize).saturating_mul(h as usize)];
    rendered.surface.read_rgba8_into(ink, &mut rgba_px);
    let rgba: Vec<u8> = rgba_px.into_iter().flat_map(|p| [p[0], p[1], p[2], p[3]]).collect();
    let xoffset = (ink.x0 as f32 - pen_x).round() as i32;
    let line_top = baseline - metrics.base as f32;
    let yoffset = (ink.y0 as f32 - line_top).round() as i32;
    GlyphBitmap { ch, width: w, height: h, xoffset, yoffset, xadvance, rgba }
}

fn line_metrics(layout: &crate::TextLayout) -> FontMetrics {
    let size = layout.styles.first().map(|s| (s.size_pt * layout.px_per_pt).round() as i32).unwrap_or(12).max(1);
    let (ascent, descent) = layout.lines.first().map(|l| (l.ascent, l.descent)).unwrap_or((size as f32 * 0.8, size as f32 * 0.2));
    let base = ascent.round() as i32;
    let line_height = (ascent + descent).round() as i32;
    FontMetrics { size, line_height: line_height.max(1), base: base.max(0) }
}

/// Builds a BMFont from `chars` using `style`. Whitespace in `chars` is ignored; duplicates keep
/// the first occurrence. Page sizes outside [`MIN_PAGE`]..=[`MAX_PAGE`] error.
pub fn build(engine: &mut TextEngine, style: &CharStyle, chars: &[char], opts: &BuildOptions) -> Result<BitmapFont, BmfontError> {
    let page_w = opts.page_w;
    let page_h = opts.page_h;
    let padding = opts.padding;
    let dpi = opts.dpi;
    if !(MIN_PAGE..=MAX_PAGE).contains(&page_w) || !(MIN_PAGE..=MAX_PAGE).contains(&page_h) {
        return Err(BmfontError::InvalidPageSize(page_w, page_h));
    }
    if padding > MAX_PADDING {
        return Err(BmfontError::InvalidPadding(padding));
    }
    let mut unique = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for &c in chars {
        if c.is_whitespace() {
            continue;
        }
        if seen.insert(c) {
            unique.push(c);
        }
    }
    if unique.is_empty() {
        return Err(BmfontError::EmptyCharset);
    }
    if unique.len() > MAX_CHARS {
        return Err(BmfontError::TooManyChars(unique.len()));
    }

    let mut bitmaps = Vec::with_capacity(unique.len());
    let mut metrics = FontMetrics { size: 1, line_height: 1, base: 0 };
    for (i, &ch) in unique.iter().enumerate() {
        let g = rasterize_char(engine, style, ch, dpi);
        if i == 0 {
            // Re-layout once for stable line metrics (same style/dpi).
            let text = ch.to_string();
            let mut st = style.clone();
            st.underline = false;
            st.strikethrough = false;
            let layer = TextLayer {
                runs: vec![TextRun { len: text.len(), style: st }],
                text,
                antialias: AntiAlias::Smooth,
                ..Default::default()
            };
            metrics = line_metrics(&engine.layout(&layer, if dpi.is_finite() && dpi > 0.0 { dpi } else { 72.0 }));
        }
        let need_w = g.width.saturating_add(padding.saturating_mul(2));
        let need_h = g.height.saturating_add(padding.saturating_mul(2));
        if need_w > page_w || need_h > page_h {
            return Err(BmfontError::GlyphTooLarge { ch, width: need_w, height: need_h, page_w, page_h });
        }
        bitmaps.push(g);
    }
    if bitmaps.is_empty() {
        return Err(BmfontError::NoGlyphs);
    }

    from_bitmaps(opts.face.clone(), metrics, &bitmaps, opts)
}

/// Packs already-rasterized glyphs into a BMFont. Used by document-pixel harvest (PSD layer
/// pixels) as well as the type-engine path. When `opts.page_w`/`page_h` equal the defaults
/// (1024) and every glyph fits a tighter content box, the atlas shrinks to the packed content
/// (plus padding), matching psd2ui / snowbBmf rather than leaving a huge empty canvas.
pub fn from_bitmaps(face: String, metrics: FontMetrics, bitmaps: &[GlyphBitmap], opts: &BuildOptions) -> Result<BitmapFont, BmfontError> {
    let page_w = opts.page_w;
    let page_h = opts.page_h;
    let padding = opts.padding;
    if !(MIN_PAGE..=MAX_PAGE).contains(&page_w) || !(MIN_PAGE..=MAX_PAGE).contains(&page_h) {
        return Err(BmfontError::InvalidPageSize(page_w, page_h));
    }
    if padding > MAX_PADDING {
        return Err(BmfontError::InvalidPadding(padding));
    }
    if bitmaps.is_empty() {
        return Err(BmfontError::NoGlyphs);
    }
    for g in bitmaps {
        let need_w = g.width.saturating_add(padding.saturating_mul(2));
        let need_h = g.height.saturating_add(padding.saturating_mul(2));
        if need_w > page_w || need_h > page_h {
            return Err(BmfontError::GlyphTooLarge { ch: g.ch, width: need_w, height: need_h, page_w, page_h });
        }
    }
    let (pages, glyphs) = pack(bitmaps, page_w, page_h, padding)?;
    let (pages, glyphs) = shrink_atlas(pages, glyphs, padding);
    Ok(BitmapFont { face, metrics, padding, glyphs, pages })
}

/// Crop each page to the used content rectangle (drop unused right/bottom padding).
fn shrink_atlas(mut pages: Vec<AtlasPage>, mut glyphs: Vec<PackedGlyph>, padding: u32) -> (Vec<AtlasPage>, Vec<PackedGlyph>) {
    for (pi, page) in pages.iter_mut().enumerate() {
        let page_i = pi as u32;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        for g in glyphs.iter().filter(|g| g.page == page_i) {
            max_x = max_x.max(g.x.saturating_add(g.width).saturating_add(padding));
            max_y = max_y.max(g.y.saturating_add(g.height).saturating_add(padding));
        }
        let new_w = max_x.max(1).min(page.width);
        let new_h = max_y.max(1).min(page.height);
        if new_w == page.width && new_h == page.height {
            continue;
        }
        let mut cropped = blank_page(new_w, new_h);
        for y in 0..new_h as usize {
            let src = y.saturating_mul(page.width as usize).saturating_mul(4);
            let dst = y.saturating_mul(new_w as usize).saturating_mul(4);
            let n = (new_w as usize).saturating_mul(4);
            if let (Some(s), Some(d)) = (page.rgba.get(src..src.saturating_add(n)), cropped.rgba.get_mut(dst..dst.saturating_add(n))) {
                d.copy_from_slice(s);
            }
        }
        *page = cropped;
    }
    // Glyph positions are unchanged; only page dimensions shrink.
    let _ = &mut glyphs;
    (pages, glyphs)
}

fn pack(bitmaps: &[GlyphBitmap], page_w: u32, page_h: u32, padding: u32) -> Result<(Vec<AtlasPage>, Vec<PackedGlyph>), BmfontError> {
    let mut pages: Vec<AtlasPage> = Vec::new();
    let mut glyphs = Vec::with_capacity(bitmaps.len());
    let mut page_idx = 0u32;
    let mut cursor_x = padding;
    let mut cursor_y = padding;
    let mut row_h = 0u32;
    pages.push(blank_page(page_w, page_h));

    for g in bitmaps {
        let gw = g.width;
        let gh = g.height;
        let cell_w = gw.saturating_add(padding);
        let cell_h = gh.saturating_add(padding);
        if cursor_x.saturating_add(gw).saturating_add(padding) > page_w {
            cursor_x = padding;
            cursor_y = cursor_y.saturating_add(row_h).saturating_add(padding);
            row_h = 0;
        }
        if cursor_y.saturating_add(gh).saturating_add(padding) > page_h {
            page_idx = page_idx.saturating_add(1);
            pages.push(blank_page(page_w, page_h));
            cursor_x = padding;
            cursor_y = padding;
            row_h = 0;
        }
        if cursor_y.saturating_add(gh).saturating_add(padding) > page_h || cursor_x.saturating_add(gw).saturating_add(padding) > page_w {
            return Err(BmfontError::GlyphTooLarge { ch: g.ch, width: cell_w.saturating_add(padding), height: cell_h.saturating_add(padding), page_w, page_h });
        }
        if gw > 0 && gh > 0 {
            blit(&mut pages[page_idx as usize], cursor_x, cursor_y, gw, gh, &g.rgba);
        }
        glyphs.push(PackedGlyph {
            ch: g.ch,
            page: page_idx,
            x: cursor_x,
            y: cursor_y,
            width: gw,
            height: gh,
            xoffset: g.xoffset,
            yoffset: g.yoffset,
            xadvance: g.xadvance,
        });
        cursor_x = cursor_x.saturating_add(gw).saturating_add(padding);
        row_h = row_h.max(gh);
    }
    Ok((pages, glyphs))
}

fn blank_page(w: u32, h: u32) -> AtlasPage {
    let n = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    AtlasPage { width: w, height: h, rgba: vec![0u8; n] }
}

fn blit(page: &mut AtlasPage, x: u32, y: u32, w: u32, h: u32, rgba: &[u8]) {
    let pw = page.width as usize;
    let ph = page.height as usize;
    let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
    if w == 0 || h == 0 || x >= pw || y >= ph {
        return;
    }
    let copy_w = w.min(pw.saturating_sub(x));
    let copy_h = h.min(ph.saturating_sub(y));
    for row in 0..copy_h {
        let src = row.saturating_mul(w).saturating_mul(4);
        let dst = (y + row).saturating_mul(pw).saturating_add(x).saturating_mul(4);
        let n = copy_w.saturating_mul(4);
        if let (Some(src_slice), Some(dst_slice)) = (rgba.get(src..src.saturating_add(n)), page.rgba.get_mut(dst..dst.saturating_add(n))) {
            dst_slice.copy_from_slice(src_slice);
        }
    }
}

/// Add `offset_x` / `offset_y` to every glyph, plus any per-character deltas in `per_char`
/// (`ch → (dx, dy)`). Used by the export dialog's offset controls.
pub fn apply_offsets(glyphs: &mut [GlyphBitmap], offset_x: i32, offset_y: i32, per_char: &[(char, i32, i32)]) {
    for g in glyphs.iter_mut() {
        g.xoffset = g.xoffset.saturating_add(offset_x);
        g.yoffset = g.yoffset.saturating_add(offset_y);
        if let Some((_, dx, dy)) = per_char.iter().find(|(c, ..)| *c == g.ch) {
            g.xoffset = g.xoffset.saturating_add(*dx);
            g.yoffset = g.yoffset.saturating_add(*dy);
        }
    }
}

const MAX_PREVIEW: u32 = 2048;

/// Lay out `text` with BMFont metrics into a single RGBA8 image (dark background) for dialog preview.
/// Missing characters are skipped; whitespace advances by ~⅓ em.
pub fn layout_string(glyphs: &[GlyphBitmap], text: &str, metrics: FontMetrics) -> Option<(u32, u32, Vec<u8>)> {
    if text.chars().all(char::is_whitespace) {
        return None;
    }
    let mut by_ch: std::collections::HashMap<char, &GlyphBitmap> = std::collections::HashMap::new();
    for g in glyphs {
        by_ch.entry(g.ch).or_insert(g);
    }
    let space = (metrics.size / 3).max(4);
    let mut pen = 0i32;
    let mut max_r = 1i32;
    let mut max_b = metrics.line_height.max(1);
    for ch in text.chars() {
        if ch.is_whitespace() {
            pen = pen.saturating_add(space);
            max_r = max_r.max(pen);
            continue;
        }
        let Some(g) = by_ch.get(&ch) else { continue };
        let x1 = pen.saturating_add(g.xoffset).saturating_add(g.width as i32);
        let y1 = g.yoffset.saturating_add(g.height as i32);
        max_r = max_r.max(x1).max(pen.saturating_add(g.xadvance.max(1)));
        max_b = max_b.max(y1);
        pen = pen.saturating_add(g.xadvance.max(1));
    }
    let w = (max_r.saturating_add(2)).clamp(1, MAX_PREVIEW as i32) as u32;
    let h = (max_b.saturating_add(2)).clamp(1, MAX_PREVIEW as i32) as u32;
    let n = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    if n == 0 || n > (MAX_PREVIEW as usize).saturating_mul(MAX_PREVIEW as usize).saturating_mul(4) {
        return None;
    }
    // Dark backdrop so bright game-font glyphs read clearly in the dialog.
    let mut rgba = vec![0u8; n];
    for px in rgba.chunks_exact_mut(4) {
        px[0] = 28;
        px[1] = 28;
        px[2] = 28;
        px[3] = 255;
    }
    let mut page = AtlasPage { width: w, height: h, rgba };
    pen = 0;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pen = pen.saturating_add(space);
            continue;
        }
        let Some(g) = by_ch.get(&ch) else { continue };
        let x = pen.saturating_add(g.xoffset).max(0) as u32;
        let y = g.yoffset.max(0) as u32;
        if g.width > 0 && g.height > 0 && !g.rgba.is_empty() {
            blit_over(&mut page, x, y, g.width, g.height, &g.rgba);
        }
        pen = pen.saturating_add(g.xadvance.max(1));
    }
    Some((page.width, page.height, page.rgba))
}

/// Source-over blit so overlapping glyph shadows combine on the preview backdrop.
fn blit_over(page: &mut AtlasPage, x: u32, y: u32, w: u32, h: u32, src: &[u8]) {
    let pw = page.width as usize;
    let ph = page.height as usize;
    let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
    if w == 0 || h == 0 || x >= pw || y >= ph {
        return;
    }
    let copy_w = w.min(pw.saturating_sub(x));
    let copy_h = h.min(ph.saturating_sub(y));
    for row in 0..copy_h {
        for col in 0..copy_w {
            let si = (row.saturating_mul(w).saturating_add(col)).saturating_mul(4);
            let di = (y + row).saturating_mul(pw).saturating_add(x + col).saturating_mul(4);
            let Some(s) = src.get(si..si + 4) else { continue };
            let Some(d) = page.rgba.get_mut(di..di + 4) else { continue };
            let sa = u32::from(s[3]);
            if sa == 0 {
                continue;
            }
            if sa >= 255 {
                d.copy_from_slice(s);
                continue;
            }
            let inv = 255u32.saturating_sub(sa);
            for c in 0..3 {
                let sc = u32::from(s[c]).saturating_mul(sa);
                let dc = u32::from(d[c]).saturating_mul(inv);
                d[c] = ((sc + dc) / 255).min(255) as u8;
            }
            d[3] = (sa + u32::from(d[3]).saturating_mul(inv) / 255).min(255) as u8;
        }
    }
}

/// AngelCode text BMFont descriptor for `font`. Page file names are `page_file(i)`.
pub fn format_fnt(font: &BitmapFont, mut page_file: impl FnMut(u32) -> String) -> String {
    let mut out = String::new();
    let face = escape_face(&font.face);
    let pad = font.padding;
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "info face=\"{face}\" size={} bold=0 italic=0 charset=\"\" unicode=1 stretchH=100 smooth=1 aa=1 padding={pad},{pad},{pad},{pad} spacing=1,1 outline=0\n",
            font.metrics.size
        ),
    );
    let scale_w = font.pages.first().map(|p| p.width).unwrap_or(1);
    let scale_h = font.pages.first().map(|p| p.height).unwrap_or(1);
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "common lineHeight={} base={} scaleW={scale_w} scaleH={scale_h} pages={} packed=0\n",
            font.metrics.line_height,
            font.metrics.base,
            font.pages.len()
        ),
    );
    for i in 0..font.pages.len() as u32 {
        let file = page_file(i);
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("page id={i} file=\"{}\"\n", escape_face(&file)));
    }
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("chars count={}\n", font.glyphs.len()));
    for g in &font.glyphs {
        let id = g.ch as u32;
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(
                "char id={id} x={} y={} width={} height={} xoffset={} yoffset={} xadvance={} page={} chnl=15\n",
                g.x, g.y, g.width, g.height, g.xoffset, g.yoffset, g.xadvance, g.page
            ),
        );
    }
    out
}

fn escape_face(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Convenience: build from a text string without I/O. Atlas pages stay as RGBA8 bytes.
pub fn export_memory(engine: &mut TextEngine, style: &CharStyle, text: &str, opts: &BuildOptions) -> Result<BitmapFont, BmfontError> {
    let chars = charset(text);
    build(engine, style, &chars, opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts::DEFAULT_FAMILY;

    fn style() -> CharStyle {
        CharStyle { font_family: DEFAULT_FAMILY.into(), size_pt: 24.0, ..Default::default() }
    }

    #[test]
    fn charset_strips_whitespace_and_dedupes() {
        assert_eq!(charset("a a\nb"), vec!['a', 'b']);
        assert!(charset(" \t\n").is_empty());
    }

    #[test]
    fn rasterize_letter_has_ink_and_advance() {
        let mut eng = TextEngine::new();
        let g = rasterize_char(&mut eng, &style(), 'A', 72.0);
        assert!(g.width > 0 && g.height > 0, "{}×{}", g.width, g.height);
        assert_eq!(g.rgba.len(), (g.width * g.height * 4) as usize);
        assert!(g.xadvance > 0);
        let ink: u32 = g.rgba.chunks(4).map(|p| u32::from(p[3])).sum();
        assert!(ink > 20 * 255, "{ink}");
    }

    fn opts(face: &str, page: u32, padding: u32) -> BuildOptions {
        BuildOptions { face: face.into(), dpi: 72.0, page_w: page, page_h: page, padding }
    }

    #[test]
    fn build_writes_valid_fnt_and_fits_glyphs() {
        let mut eng = TextEngine::new();
        let font = export_memory(&mut eng, &style(), "AB12", &opts("TestFace", 256, 1)).unwrap();
        assert_eq!(font.glyphs.len(), 4);
        assert!(!font.pages.is_empty());
        let fnt = format_fnt(&font, |i| format!("TestFace_{i}.png"));
        assert!(fnt.contains("info face=\"TestFace\""));
        assert!(fnt.contains("chars count=4"));
        assert!(fnt.contains("char id=65 ")); // 'A'
        assert!(fnt.contains("page id=0 file=\"TestFace_0.png\""));
        for g in &font.glyphs {
            assert!(g.x + g.width <= font.pages[g.page as usize].width);
            assert!(g.y + g.height <= font.pages[g.page as usize].height);
        }
    }

    #[test]
    fn empty_charset_errors() {
        let mut eng = TextEngine::new();
        assert!(matches!(export_memory(&mut eng, &style(), "  \n", &opts("x", 256, 1)), Err(BmfontError::EmptyCharset)));
    }

    #[test]
    fn rejects_bad_page_and_padding() {
        let mut eng = TextEngine::new();
        assert!(matches!(export_memory(&mut eng, &style(), "A", &opts("x", 8, 1)), Err(BmfontError::InvalidPageSize(_, _))));
        assert!(matches!(export_memory(&mut eng, &style(), "A", &opts("x", 256, 99)), Err(BmfontError::InvalidPadding(_))));
    }

    #[test]
    fn packs_onto_multiple_pages_when_needed() {
        let mut eng = TextEngine::new();
        // Tiny page forces several pages for a longer charset.
        let font = export_memory(&mut eng, &style(), "ABCDEFGHIJKLMNOP", &opts("Tiny", 64, 1)).unwrap();
        assert!(font.pages.len() > 1, "pages={}", font.pages.len());
        assert_eq!(font.glyphs.len(), 16);
    }

    #[test]
    fn apply_offsets_shifts_global_and_per_char() {
        let mut glyphs = vec![
            GlyphBitmap { ch: 'A', width: 1, height: 1, xoffset: 2, yoffset: 3, xadvance: 4, rgba: vec![255; 4] },
            GlyphBitmap { ch: 'B', width: 1, height: 1, xoffset: 0, yoffset: 0, xadvance: 4, rgba: vec![255; 4] },
        ];
        apply_offsets(&mut glyphs, 1, -2, &[('B', 5, 6)]);
        assert_eq!(glyphs[0].xoffset, 3);
        assert_eq!(glyphs[0].yoffset, 1);
        assert_eq!(glyphs[1].xoffset, 6);
        assert_eq!(glyphs[1].yoffset, 4);
    }

    #[test]
    fn layout_string_draws_ink_on_dark_backdrop() {
        let mut eng = TextEngine::new();
        let a = rasterize_char(&mut eng, &style(), 'A', 72.0);
        let b = rasterize_char(&mut eng, &style(), 'B', 72.0);
        let metrics = FontMetrics { size: 24, line_height: 30, base: 24 };
        let (w, h, rgba) = layout_string(&[a, b], "AB", metrics).expect("preview");
        assert!(w > 0 && h > 0);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        // Backdrop is (28,28,28,255); any blended glyph pixel differs.
        let ink = rgba.chunks(4).filter(|p| *p != [28, 28, 28, 255]).count();
        assert!(ink > 10, "expected glyph ink over backdrop, ink={ink}");
    }
}

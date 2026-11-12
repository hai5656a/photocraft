//! Equal or per-band spacing between selected layers (`layer.gap.set`).
//!
//! Selected layers are clustered into **columns** (similar x) and **rows** (similar y), so a grid
//! gets one horizontal gap between each pair of columns and one vertical gap between each pair of
//! rows — not a single chain of every layer. A one-row or one-column selection still behaves like
//! neighbour spacing. Setting every gap keeps the first band put; setting `index` shifts that
//! neighbour band and every band beyond it. The Move tool overlay and Properties › Spacing call
//! the same command.

use photocraft_doc::{Document, LayerId};
use photocraft_geom::Rect;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::layer_multi_cmds::{bounded_selection, dedup_moves, move_layers, selected, with_links};
use crate::{EngineError, Result, Session};

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

/// One gap between two neighbouring bands (columns when `horizontal`, else rows).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GapSeg {
    pub horizontal: bool,
    /// Index among gaps on this axis (0 = between the first and second band).
    pub index: usize,
    /// A layer from the leading band (left/top); for diagnostics and stable identity.
    pub before: LayerId,
    /// A layer from the trailing band (right/bottom).
    pub after: LayerId,
    /// Document px from the trailing edge of the leading band to the leading edge of the next.
    pub gap: i32,
    /// Line placement: along the measured axis from `from` to `to`, across-axis at `at`.
    pub from: f64,
    pub to: f64,
    pub at: f64,
    /// Across-axis span used for the drag hit strip (union of both bands).
    pub cross0: f64,
    pub cross1: f64,
    /// Band content unions (`[x0, y0, x1, y1]`).
    pub before_rect: [f64; 4],
    pub after_rect: [f64; 4],
}

/// One row or column of the selection: the layers in it and their union bounds.
#[derive(Clone, Debug)]
struct Band {
    ids: Vec<LayerId>,
    union: Rect,
}

fn can_set_gap(s: &Session) -> std::result::Result<(), String> {
    s.active().ok_or_else(|| String::from("no document open"))?;
    if selected(s).len() < 2 {
        return Err(String::from("select two or more layers"));
    }
    let st = s.active().ok_or_else(|| String::from("no document open"))?;
    if bounded_selection(&st.doc, &selected(s)).len() < 2 {
        return Err(String::from("the selected layers have no content to space"));
    }
    Ok(())
}

fn parse_axis(cmd: &str, p: &Value) -> Result<bool> {
    match p.get("axis").and_then(Value::as_str).unwrap_or("horizontal") {
        "horizontal" | "h" | "x" => Ok(true),
        "vertical" | "v" | "y" => Ok(false),
        other => Err(bad(cmd, format!("`axis` must be horizontal|vertical, not `{other}`"))),
    }
}

/// Inclusive |gap| ceiling for `layer.gap.set` (overlap uses negative values).
const GAP_PX_LIMIT: f64 = 1_000_000.0;

fn parse_gap_px(cmd: &str, p: &Value) -> Result<i32> {
    let v = p.get("gap").ok_or_else(|| bad(cmd, "`gap` is required (document px)"))?;
    let f = v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| bad(cmd, "`gap` must be a finite number"))?;
    if f.abs() > GAP_PX_LIMIT {
        return Err(bad(cmd, format!("`gap` must be within ±{GAP_PX_LIMIT}")));
    }
    Ok(f.round() as i32)
}

fn rect4(r: &Rect) -> [f64; 4] {
    [f64::from(r.x0), f64::from(r.y0), f64::from(r.x1), f64::from(r.y1)]
}

fn center_along(r: &Rect, along_x: bool) -> i64 {
    if along_x {
        i64::from(r.x0) + i64::from(r.x1)
    } else {
        i64::from(r.y0) + i64::from(r.y1)
    }
}

fn extent_along(r: &Rect, along_x: bool) -> (i32, i32) {
    if along_x { (r.x0, r.x1) } else { (r.y0, r.y1) }
}

/// Whether two boxes belong in the same column (`along_x`) or row.
///
/// Centres within ¾ of the smaller extent count as one band (ragged grid rows). A thin projection
/// overlap alone does not: avatar groups often include a name plate that kisses the row below,
/// which used to merge two stacked groups and hide Spacing › V Gap.
fn same_band(a: &Rect, b: &Rect, along_x: bool) -> bool {
    let (a0, a1) = extent_along(a, along_x);
    let (b0, b1) = extent_along(b, along_x);
    let ac = (i64::from(a0) + i64::from(a1)) / 2;
    let bc = (i64::from(b0) + i64::from(b1)) / 2;
    let aw = i64::from(a1.saturating_sub(a0)).max(1);
    let bw = i64::from(b1.saturating_sub(b0)).max(1);
    let thresh = (aw.min(bw) * 3 / 4).max(1);
    if (ac - bc).abs() <= thresh {
        return true;
    }
    // ≥ half the smaller extent overlapping (e.g. a small cell inside a tall neighbour's span).
    let overlap = i64::from(a1.min(b1).saturating_sub(a0.max(b0))).max(0);
    overlap * 2 >= aw.min(bw)
}

/// Cluster the selection into columns (`along_x`) or rows, sorted along that axis.
fn bands(doc: &Document, sel: &[LayerId], along_x: bool) -> Vec<Band> {
    let mut items = bounded_selection(doc, sel);
    if items.is_empty() {
        return Vec::new();
    }
    items.sort_by_key(|(_, r)| (center_along(r, along_x), extent_along(r, along_x).0 as i64));
    let mut groups: Vec<Vec<(LayerId, Rect)>> = Vec::new();
    for (id, r) in items {
        if let Some(last) = groups.last_mut()
            && last.iter().any(|(_, o)| same_band(o, &r, along_x))
        {
            last.push((id, r));
            continue;
        }
        groups.push(vec![(id, r)]);
    }
    // Re-sort bands by their union centre (a late item can widen an earlier band).
    let mut bands: Vec<Band> = groups
        .into_iter()
        .map(|g| {
            let union = g.iter().fold(Rect::EMPTY, |a, (_, r)| a.union(r));
            Band { ids: g.into_iter().map(|(id, _)| id).collect(), union }
        })
        .collect();
    bands.sort_by_key(|b| (center_along(&b.union, along_x), extent_along(&b.union, along_x).0 as i64));
    bands
}

/// Consecutive band gaps on one axis (columns when `horizontal`, else rows).
///
/// A diagonal pair (two columns and two rows) yields both an H gap and a V gap so the canvas and
/// Properties can show both numbers together — same as a 2×2 grid.
pub fn gaps_on_axis(doc: &Document, sel: &[LayerId], horizontal: bool) -> Vec<GapSeg> {
    let bands = bands(doc, sel, horizontal);
    if bands.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(bands.len() - 1);
    for i in 0..bands.len() - 1 {
        let a = &bands[i];
        let b = &bands[i + 1];
        let (a_hi, b_lo, a_c0, a_c1, b_c0, b_c1) = if horizontal {
            (a.union.x1, b.union.x0, a.union.y0, a.union.y1, b.union.y0, b.union.y1)
        } else {
            (a.union.y1, b.union.y0, a.union.x0, a.union.x1, b.union.x0, b.union.x1)
        };
        let cross0 = f64::from(a_c0.min(b_c0));
        let cross1 = f64::from(a_c1.max(b_c1));
        let at = (cross0 + cross1) / 2.0;
        let gap = b_lo - a_hi;
        let from = f64::from(a_hi.min(b_lo));
        let to = f64::from(a_hi.max(b_lo));
        let (Some(&before), Some(&after)) = (a.ids.first(), b.ids.first()) else { continue };
        out.push(GapSeg {
            horizontal,
            index: i,
            before,
            after,
            gap,
            from,
            to,
            at,
            cross0,
            cross1,
            before_rect: rect4(&a.union),
            after_rect: rect4(&b.union),
        });
    }
    out
}

/// Horizontal then vertical band gaps for the selection.
pub fn selection_gaps(doc: &Document, sel: &[LayerId]) -> Vec<GapSeg> {
    let mut out = gaps_on_axis(doc, sel, true);
    out.extend(gaps_on_axis(doc, sel, false));
    out
}

/// One content frame per **selected** layer that has bounds — same set the Layers panel highlights.
///
/// Unlike [`bounded_selection`] (Align / Spacing / Move), this does **not** drop children when a
/// parent group is also selected: otherwise the panel shows two rows lit and the canvas only one
/// box. Spacing still uses `bounded_selection` so a selected group is one band, not group+children.
pub fn selection_frames(doc: &Document, sel: &[LayerId]) -> Vec<(LayerId, Rect)> {
    use crate::layer_multi_cmds::layer_bounds;
    // Document walk order (bottom-to-top), matching `selected_layers()` / the panel.
    let want: std::collections::HashSet<LayerId> = sel.iter().copied().collect();
    doc.walk()
        .into_iter()
        .filter_map(|(_, _, l)| want.contains(&l.id).then_some(l))
        .filter_map(|l| Some((l.id, layer_bounds(l)?)))
        .collect()
}

/// Shared value of every band gap on `horizontal`, or `None` when they differ or there are none.
pub fn uniform_gap(doc: &Document, sel: &[LayerId], horizontal: bool) -> Option<i32> {
    let g = gaps_on_axis(doc, sel, horizontal);
    let first = g.first()?.gap;
    g.iter().all(|s| s.gap == first).then_some(first)
}

fn set_gap(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "layer.gap.set";
    let horizontal = parse_axis(CMD, p)?;
    let gap = parse_gap_px(CMD, p)?;
    let index = match p.get("index") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| bad(CMD, "`index` must be a non-negative integer"))? as usize),
    };
    let sel = selected(s);
    let before = s.active().ok_or(EngineError::NoDocument)?.doc.clone();
    let moved = s.edit("Set Layer Gap", |doc, _| {
        let bands = bands(doc, &sel, horizontal);
        if bands.len() < 2 {
            return Err(EngineError::Other("the selected layers have no spaced bands on that axis".into()));
        }
        let n = bands.len();
        if let Some(i) = index {
            if i >= n - 1 {
                return Err(bad(CMD, format!("`index` must be 0..{}, not {i}", n - 1)));
            }
            let (a_hi, b_lo) = if horizontal {
                (bands[i].union.x1, bands[i + 1].union.x0)
            } else {
                (bands[i].union.y1, bands[i + 1].union.y0)
            };
            let cur = b_lo - a_hi;
            let delta = gap.checked_sub(cur).ok_or_else(|| bad(CMD, "gap delta overflow"))?;
            if delta == 0 {
                return Ok(0usize);
            }
            let mut moves = Vec::new();
            for band in bands.iter().skip(i + 1) {
                let (dx, dy) = if horizontal { (delta, 0) } else { (0, delta) };
                for id in &band.ids {
                    for l in with_links(doc, &[*id]) {
                        moves.push((l, dx, dy));
                    }
                }
            }
            let moves = dedup_moves(doc, moves);
            move_layers(doc, &before, &moves)?;
            Ok(moves.iter().filter(|m| m.1 != 0 || m.2 != 0).count())
        } else {
            // Equal gaps between bands; the first band stays put.
            let mut cursor = if horizontal { bands[0].union.x1 } else { bands[0].union.y1 };
            let mut moves = Vec::new();
            for band in bands.iter().skip(1) {
                let lo = if horizontal { band.union.x0 } else { band.union.y0 };
                let hi = if horizontal { band.union.x1 } else { band.union.y1 };
                let want = cursor.checked_add(gap).ok_or_else(|| bad(CMD, "gap position overflow"))?;
                let d = want.checked_sub(lo).ok_or_else(|| bad(CMD, "gap position overflow"))?;
                if d != 0 {
                    let (dx, dy) = if horizontal { (d, 0) } else { (0, d) };
                    for id in &band.ids {
                        for l in with_links(doc, &[*id]) {
                            moves.push((l, dx, dy));
                        }
                    }
                }
                let width = hi.checked_sub(lo).ok_or_else(|| bad(CMD, "band extent overflow"))?;
                cursor = want.checked_add(width).ok_or_else(|| bad(CMD, "gap position overflow"))?;
            }
            let moves = dedup_moves(doc, moves);
            move_layers(doc, &before, &moves)?;
            Ok(moves.iter().filter(|m| m.1 != 0 || m.2 != 0).count())
        }
    })?;
    Ok(json!({
        "moved": moved,
        "gap": gap,
        "axis": if horizontal { "horizontal" } else { "vertical" },
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "layer.gap.set",
        label: "Set Layer Gap",
        menu: &[],
        shortcut: None,
        params: r##"{"axis":"horizontal|vertical"="horizontal","gap":px (signed; negative = overlap),"index":int?} (bands = columns for horizontal, rows for vertical. omit index to set every band gap equal; keep the first band put. index 0..n-2 moves that neighbour band and everything beyond it)"##,
        enabled: can_set_gap,
        journal: true,
        run: set_gap,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use crate::layer_multi_cmds::layer_bounds;
    use serde_json::json;

    fn three_boxes() -> (Session, [u64; 3]) {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 400, "height": 200})).unwrap();
        let mut ids = [0u64; 3];
        for (i, (x, name)) in [(10, "a"), (80, "b"), (200, "c")].into_iter().enumerate() {
            s.execute("layer.new.layer", json!({"name": name})).unwrap();
            s.execute("select.rect", json!({"x": x, "y": 40, "width": 40, "height": 40})).unwrap();
            s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
            s.execute("select.deselect", json!({})).unwrap();
            ids[i] = s.active().unwrap().active_layer.unwrap().0;
        }
        for &id in &ids[1..] {
            s.execute("layer.select", json!({"layer": id, "mode": "add"})).unwrap();
        }
        s.execute("layer.select", json!({"layer": ids[0], "mode": "add"})).unwrap();
        (s, ids)
    }

    fn bounds(s: &Session, id: u64) -> Rect {
        let st = s.active().unwrap();
        layer_bounds(st.doc.layer(LayerId(id)).unwrap()).unwrap()
    }

    fn select_ids(s: &mut Session, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        s.execute("layer.select", json!({"layer": ids[0]})).unwrap();
        for &id in &ids[1..] {
            s.execute("layer.select", json!({"layer": id, "mode": "add"})).unwrap();
        }
    }

    /// 2×2 grid with uneven gaps: columns 10px / 40px, rows 10px / 30px.
    fn grid_2x2() -> (Session, [[u64; 2]; 2]) {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 400, "height": 400})).unwrap();
        // (x, y): row0 (10,10)/(60,10), row1 (10,60)/(60,90) — widths/heights 40.
        let spots = [[(10, 10), (60, 10)], [(10, 60), (60, 90)]];
        let mut ids = [[0u64; 2]; 2];
        for (r, row) in spots.iter().enumerate() {
            for (c, &(x, y)) in row.iter().enumerate() {
                s.execute("layer.new.layer", json!({"name": format!("r{r}c{c}")})).unwrap();
                s.execute("select.rect", json!({"x": x, "y": y, "width": 40, "height": 40})).unwrap();
                s.execute("edit.fill", json!({"color": "#00ff00"})).unwrap();
                s.execute("select.deselect", json!({})).unwrap();
                ids[r][c] = s.active().unwrap().active_layer.unwrap().0;
            }
        }
        let flat: Vec<u64> = ids.iter().flatten().copied().collect();
        select_ids(&mut s, &flat);
        (s, ids)
    }

    #[test]
    fn set_equal_horizontal_gaps_keeps_the_first_put() {
        let (mut s, ids) = three_boxes();
        s.execute("layer.gap.set", json!({"axis": "horizontal", "gap": 20})).unwrap();
        let a = bounds(&s, ids[0]);
        let b = bounds(&s, ids[1]);
        let c = bounds(&s, ids[2]);
        assert_eq!((a.x0, a.x1), (10, 50));
        assert_eq!(b.x0, 70);
        assert_eq!(c.x0, 130);
        assert_eq!(b.x0 - a.x1, 20);
        assert_eq!(c.x0 - b.x1, 20);
    }

    #[test]
    fn set_one_gap_moves_everything_beyond_it() {
        let (mut s, ids) = three_boxes();
        s.execute("layer.gap.set", json!({"axis": "h", "gap": 50, "index": 0})).unwrap();
        let a = bounds(&s, ids[0]);
        let b = bounds(&s, ids[1]);
        let c = bounds(&s, ids[2]);
        assert_eq!(a.x0, 10);
        assert_eq!(b.x0 - a.x1, 50);
        // The second gap is unchanged in width: both B and C moved by the same delta.
        assert_eq!(c.x0 - b.x1, 80);
    }

    #[test]
    fn vertical_axis_and_coalesce_share_one_history_step() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 200, "height": 300})).unwrap();
        let mut ids = Vec::new();
        for (y, name) in [(10, "a"), (80, "b"), (160, "c")] {
            s.execute("layer.new.layer", json!({"name": name})).unwrap();
            s.execute("select.rect", json!({"x": 20, "y": y, "width": 30, "height": 30})).unwrap();
            s.execute("edit.fill", json!({"color": "#00ff00"})).unwrap();
            s.execute("select.deselect", json!({})).unwrap();
            ids.push(s.active().unwrap().active_layer.unwrap().0);
        }
        select_ids(&mut s, &ids);
        let base = s.active().unwrap().history.entries().len();
        s.execute("layer.gap.set", json!({"axis": "vertical", "gap": 10, "coalesce": "drag"})).unwrap();
        s.execute("layer.gap.set", json!({"axis": "vertical", "gap": 16, "coalesce": "drag"})).unwrap();
        assert_eq!(s.active().unwrap().history.entries().len(), base + 1);
        let a = bounds(&s, ids[0]);
        let b = bounds(&s, ids[1]);
        assert_eq!(b.y0 - a.y1, 16);
    }

    #[test]
    fn bad_params_and_too_few_layers_return_err() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 100, "height": 100})).unwrap();
        assert!(s.execute("layer.gap.set", json!({"gap": 10})).is_err());
        s.execute("layer.new.layer", json!({"name": "a"})).unwrap();
        s.execute("select.rect", json!({"x": 0, "y": 0, "width": 10, "height": 10})).unwrap();
        s.execute("edit.fill", json!({"color": "#ffffff"})).unwrap();
        assert!(s.execute("layer.gap.set", json!({"gap": 10})).is_err());
        s.execute("layer.new.layer", json!({"name": "b"})).unwrap();
        s.execute("select.rect", json!({"x": 40, "y": 0, "width": 10, "height": 10})).unwrap();
        s.execute("edit.fill", json!({"color": "#ffffff"})).unwrap();
        let a = s.active().unwrap().doc.walk().iter().find(|(_, _, l)| l.name == "a").unwrap().2.id.0;
        let b = s.active().unwrap().active_layer.unwrap().0;
        select_ids(&mut s, &[a, b]);
        assert!(s.execute("layer.gap.set", json!({"gap": 2_000_000})).is_err());
        assert!(s.execute("layer.gap.set", json!({"gap": -2_000_000})).is_err());
        assert!(s.execute("layer.gap.set", json!({"gap": 10, "axis": "diagonal"})).is_err());
        assert!(s.execute("layer.gap.set", json!({"gap": 10, "index": 99})).is_err());
        assert!(s.execute("layer.gap.set", json!({"gap": 12})).is_ok());
        assert!(s.execute("layer.gap.set", json!({"gap": -8})).is_ok());
    }

    #[test]
    fn negative_gap_overlaps_neighbour_bands() {
        // Width-40 boxes stay in separate columns while |gap| < ~10 (same_band centre rule).
        let (mut s, ids) = three_boxes();
        s.execute("layer.gap.set", json!({"axis": "horizontal", "gap": -5})).unwrap();
        let a = bounds(&s, ids[0]);
        let b = bounds(&s, ids[1]);
        let c = bounds(&s, ids[2]);
        assert_eq!((a.x0, a.x1), (10, 50));
        assert_eq!(b.x0 - a.x1, -5);
        assert_eq!(c.x0 - b.x1, -5);
        // Per-index drag into a deeper overlap only moves bands beyond that index.
        s.execute("layer.gap.set", json!({"axis": "h", "gap": -8, "index": 0})).unwrap();
        let a = bounds(&s, ids[0]);
        let b = bounds(&s, ids[1]);
        let c = bounds(&s, ids[2]);
        assert_eq!(a.x0, 10);
        assert_eq!(b.x0 - a.x1, -8);
        assert_eq!(c.x0 - b.x1, -5);
    }

    #[test]
    fn selection_gaps_lists_neighbours() {
        let (s, ids) = three_boxes();
        let st = s.active().unwrap();
        let g = gaps_on_axis(&st.doc, &selected(&s), true);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].before.0, ids[0]);
        assert_eq!(g[0].after.0, ids[1]);
        assert_eq!(g[0].gap, 30);
        assert_eq!(g[1].gap, 80);
        assert_eq!(uniform_gap(&st.doc, &selected(&s), true), None);
    }

    #[test]
    fn grid_has_one_gap_per_column_and_row_pair() {
        let (s, _) = grid_2x2();
        let st = s.active().unwrap();
        let sel = selected(&s);
        let h = gaps_on_axis(&st.doc, &sel, true);
        let v = gaps_on_axis(&st.doc, &sel, false);
        assert_eq!(h.len(), 1, "two columns → one column gap: {h:?}");
        assert_eq!(v.len(), 1, "two rows → one row gap: {v:?}");
        // Column gap: right of left col (50) to left of right col (60) = 10.
        assert_eq!(h[0].gap, 10);
        // Row gap: between row0 bottom (50) and row1 top — row1 has y0=60 and y0=90, union y0=60 → 10.
        assert_eq!(v[0].gap, 10);
    }

    #[test]
    fn grid_equal_gaps_move_whole_columns_and_rows() {
        let (mut s, ids) = grid_2x2();
        s.execute("layer.gap.set", json!({"axis": "horizontal", "gap": 24})).unwrap();
        // Left column stays; right column (both rows) moves as a unit.
        assert_eq!(bounds(&s, ids[0][0]).x0, 10);
        assert_eq!(bounds(&s, ids[1][0]).x0, 10);
        assert_eq!(bounds(&s, ids[0][1]).x0, 74); // 50+24
        assert_eq!(bounds(&s, ids[1][1]).x0, 74);
        assert_eq!(bounds(&s, ids[0][1]).x0 - bounds(&s, ids[0][0]).x1, 24);

        s.execute("layer.gap.set", json!({"axis": "vertical", "gap": 16})).unwrap();
        assert_eq!(bounds(&s, ids[0][0]).y0, 10);
        assert_eq!(bounds(&s, ids[0][1]).y0, 10);
        // Row1 union was y0=60..130; after equal gap from row0 bottom 50: y0=66.
        assert_eq!(bounds(&s, ids[1][0]).y0, 66);
        assert_eq!(bounds(&s, ids[1][1]).y0, 96); // kept its offset within the row band (+30)
        assert_eq!(bounds(&s, ids[1][0]).y0 - bounds(&s, ids[0][0]).y1, 16);
    }

    #[test]
    fn grid_index_gap_shifts_only_bands_beyond() {
        let (mut s, ids) = grid_2x2();
        // Widen the only column gap: right column moves, left stays; relative y unchanged.
        s.execute("layer.gap.set", json!({"axis": "h", "gap": 40, "index": 0})).unwrap();
        assert_eq!(bounds(&s, ids[0][0]).x0, 10);
        assert_eq!(bounds(&s, ids[0][1]).x0, 90); // 50 + 40
        assert_eq!(bounds(&s, ids[1][1]).x0, 90);
        assert_eq!(bounds(&s, ids[1][0]).y0, 60);
    }

    /// Vector-only groups (no raster cache) used to yield empty `layer_bounds`, so Properties
    /// never showed Spacing › V Gap for two stacked avatar groups.
    #[test]
    fn two_stacked_uncached_shape_groups_expose_vertical_gap() {
        use photocraft_doc::{LayerContent, LayerId};

        let mut s = Session::new();
        s.execute("file.new", json!({"width": 200, "height": 400})).unwrap();
        let mut groups = Vec::new();
        // `rect` is [x, y, w, h]: tops at y=20..60 and y=140..180 → gap 80.
        for (y, name) in [(20.0_f64, "top"), (140.0, "bot")] {
            let id = s
                .execute("shape.create", json!({"kind": "rect", "rect": [40.0, y, 60.0, 40.0], "fill": "#ff0000"}))
                .unwrap()["layer"]
                .as_u64()
                .unwrap();
            s.edit("clear shape cache", |doc, _| {
                let layer = doc.layer_mut(LayerId(id)).ok_or_else(|| EngineError::NoLayer(LayerId(id)))?;
                if let LayerContent::Shape(sh) = &mut layer.content {
                    sh.cache = None;
                }
                Ok(())
            })
            .unwrap();
            assert!(crate::layer_multi_cmds::layer_bounds(s.active().unwrap().doc.layer(LayerId(id)).unwrap()).is_some());
            s.execute("layer.select", json!({"layer": id})).unwrap();
            let g = s.execute("layer.groupLayers", json!({"name": name})).unwrap()["layer"].as_u64().unwrap();
            groups.push(g);
        }
        select_ids(&mut s, &groups);
        let st = s.active().unwrap();
        let sel = selected(&s);
        assert_eq!(selection_frames(&st.doc, &sel).len(), 2);
        let v = gaps_on_axis(&st.doc, &sel, false);
        assert_eq!(v.len(), 1, "two stacked groups → one V gap: {v:?}");
        assert_eq!(v[0].gap, 80);
        assert!(gaps_on_axis(&st.doc, &sel, true).is_empty(), "same column → no H gap");
        s.execute("layer.gap.set", json!({"axis": "vertical", "gap": 24})).unwrap();
        let st = s.active().unwrap();
        assert_eq!(gaps_on_axis(&st.doc, &selected(&s), false)[0].gap, 24);
    }

    /// Name-plate kiss: y-bounds overlap a little, but centres are clearly on two rows → V Gap.
    #[test]
    fn stacked_pair_with_thin_y_overlap_still_gets_vertical_gap() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 200, "height": 400})).unwrap();
        // Top 40..140, bottom 120..220 — 20 px overlap, centres 90 vs 170 (diff 80 > ¾×100).
        let mut ids = Vec::new();
        for (y, name) in [(40, "top"), (120, "bot")] {
            s.execute("layer.new.layer", json!({"name": name})).unwrap();
            s.execute("select.rect", json!({"x": 40, "y": y, "width": 60, "height": 100})).unwrap();
            s.execute("edit.fill", json!({"color": "#00ff00"})).unwrap();
            s.execute("select.deselect", json!({})).unwrap();
            ids.push(s.active().unwrap().active_layer.unwrap().0);
        }
        select_ids(&mut s, &ids);
        let st = s.active().unwrap();
        let sel = selected(&s);
        let v = gaps_on_axis(&st.doc, &sel, false);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].gap, -20);
        assert!(gaps_on_axis(&st.doc, &sel, true).is_empty());
    }

    /// Local repro for `8奖池-恭喜获得.psd`: two stacked「头像 拷贝」groups must expose V Gap.
    #[test]
    #[ignore = "needs local PSD at PSD2FGUI_PSD"]
    fn local_psd_two_avatar_copy_groups_have_vertical_gap() {
        let path = std::env::var("PSD2FGUI_PSD").unwrap_or_else(|_| r"e:\git\Github\psd2fgui\test\8奖池-恭喜获得.psd".into());
        let mut s = Session::new();
        s.execute("file.openAs", json!({ "path": path })).unwrap();
        // Prefer name lookup: PSD layer ids can shift across imports.
        let st = s.active().unwrap();
        let mut by_name: Vec<(String, LayerId, Option<Rect>)> = Vec::new();
        for (_p, _d, l) in st.doc.walk() {
            if matches!(l.content, photocraft_doc::LayerContent::Group(_))
                && (l.name == "头像 拷贝 4" || l.name == "头像 拷贝 3")
            {
                by_name.push((l.name.clone(), l.id, crate::layer_multi_cmds::layer_bounds(l)));
            }
        }
        eprintln!("named groups: {by_name:?}");
        // Right column: two「头像 拷贝 4」stacked (name plates make y-bounds overlap slightly).
        let copies4: Vec<_> = by_name.iter().filter(|(n, _, b)| n == "头像 拷贝 4" && b.is_some()).collect();
        assert!(copies4.len() >= 2, "need two 头像 拷贝 4 groups: {by_name:?}");
        let mut pair = copies4.clone();
        pair.sort_by_key(|(_, _, b)| b.unwrap().y0);
        // Rightmost column among the upper row, then the copy below it.
        let upper = pair.iter().min_by_key(|(_, _, b)| b.unwrap().y0).unwrap();
        let rightmost_upper = pair
            .iter()
            .filter(|(_, _, b)| (b.unwrap().y0 - upper.2.unwrap().y0).abs() < 40)
            .max_by_key(|(_, _, b)| b.unwrap().x0)
            .unwrap();
        let below = pair
            .iter()
            .filter(|(_, id, b)| {
                *id != rightmost_upper.1 && {
                    let u = rightmost_upper.2.unwrap();
                    let r = b.unwrap();
                    ((u.x0 + u.x1) / 2 - (r.x0 + r.x1) / 2).abs() < 80 && r.y0 > u.y0
                }
            })
            .min_by_key(|(_, _, b)| b.unwrap().y0)
            .expect("stacked copy below");
        let sel = [rightmost_upper.1, below.1];
        eprintln!("pair {:?}/{:?}", rightmost_upper.0, below.0);
        let frames = selection_frames(&st.doc, &sel);
        let h = gaps_on_axis(&st.doc, &sel, true);
        let v = gaps_on_axis(&st.doc, &sel, false);
        eprintln!(
            "frames={frames:?}\nh_gaps={:?}\nv_gaps={:?}",
            h.iter().map(|g| g.gap).collect::<Vec<_>>(),
            v.iter().map(|g| g.gap).collect::<Vec<_>>()
        );
        assert_eq!(frames.len(), 2, "both groups need content bounds: {frames:?}");
        assert_eq!(v.len(), 1, "stacked avatar groups → one V gap; h={h:?} v={v:?}");
        assert!(h.is_empty(), "same column → no H gap: {h:?}");
        // Same-row「拷贝 4」+「拷贝 3」→ H Gap only.
        let c3 = by_name.iter().find(|(n, _, b)| n == "头像 拷贝 3" && b.is_some_and(|r| (r.y0 - rightmost_upper.2.unwrap().y0).abs() < 40));
        if let Some(c3) = c3 {
            let row = [rightmost_upper.1, c3.1];
            assert_eq!(gaps_on_axis(&st.doc, &row, true).len(), 1);
            assert!(gaps_on_axis(&st.doc, &row, false).is_empty());
        }
    }

    /// Panel can still hold group+child via `set_selection`; frames follow every selected id.
    #[test]
    fn selection_frames_match_panel_when_group_and_child_are_both_selected() {
        use photocraft_doc::LayerId;
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 200, "height": 200})).unwrap();
        s.execute("layer.new.layer", json!({"name": "leaf"})).unwrap();
        s.execute("select.rect", json!({"x": 10, "y": 10, "width": 40, "height": 40})).unwrap();
        s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
        s.execute("select.deselect", json!({})).unwrap();
        let leaf = LayerId(s.active().unwrap().active_layer.unwrap().0);
        let g = LayerId(s.execute("layer.groupLayers", json!({"name": "G"})).unwrap()["layer"].as_u64().unwrap());
        crate::layer_multi_cmds::set_selection(&mut s, vec![g, leaf], Some(leaf), Some(g)).unwrap();
        let st = s.active().unwrap();
        let sel = st.selected_layers();
        let frames = selection_frames(&st.doc, &sel);
        assert_eq!(frames.len(), 2, "panel lights group+child → two frames: {frames:?}");
        // Spacing still treats the group as one band (top_level).
        assert_eq!(bounded_selection_len(&st.doc, &sel), 1);
    }

    fn bounded_selection_len(doc: &Document, sel: &[LayerId]) -> usize {
        crate::layer_multi_cmds::bounded_selection(doc, sel).len()
    }

    /// Diagonal pair: two columns and two rows → both H and V gaps (stage shows two numbers).
    #[test]
    fn diagonal_pair_exposes_both_horizontal_and_vertical_gaps() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 300, "height": 300})).unwrap();
        let mut ids = Vec::new();
        for (x, y, name) in [(20, 20, "a"), (120, 100, "b")] {
            s.execute("layer.new.layer", json!({"name": name})).unwrap();
            s.execute("select.rect", json!({"x": x, "y": y, "width": 40, "height": 40})).unwrap();
            s.execute("edit.fill", json!({"color": "#ff00ff"})).unwrap();
            s.execute("select.deselect", json!({})).unwrap();
            ids.push(s.active().unwrap().active_layer.unwrap().0);
        }
        select_ids(&mut s, &ids);
        let st = s.active().unwrap();
        let sel = selected(&s);
        let all = selection_gaps(&st.doc, &sel);
        assert_eq!(gaps_on_axis(&st.doc, &sel, true).len(), 1, "{all:?}");
        assert_eq!(gaps_on_axis(&st.doc, &sel, false).len(), 1, "{all:?}");
        assert_eq!(all.len(), 2);
    }
}

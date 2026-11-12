//! Multi-selection frames, spacing labels and between-band gap drag (Move tool).
//!
//! With two or more content layers selected, each layer gets its own selection frame. Layers are
//! clustered into columns and rows: gap labels sit between column bands and row bands (a grid, not
//! only a single line). Dragging the strip between bands runs `layer.gap.set` with `index` and a
//! coalesce key so one History step covers the gesture. Properties › Spacing sets every band gap
//! on an axis equal (no `index`).

use egui::{Color32, CursorIcon, Pos2, Stroke, pos2, vec2};
use photocraft_engine::layer_gap_cmds::{self, GapSeg};
use serde_json::json;

use crate::PhotocraftApp;
use crate::canvas::{ToolEvent, ViewXform};
use crate::state::Tool;

/// Minimum hit thickness of a gap strip in screen pixels (also used when neighbours touch).
const HIT_PX: f32 = 12.0;
/// Gap between a spacing line and its number badge (screen px).
const LABEL_PAD: f32 = 6.0;

/// An in-progress drag of one neighbour gap.
#[derive(Clone, Debug)]
pub(crate) struct GapDrag {
    pub horizontal: bool,
    pub index: usize,
    /// Pointer position on the measured axis at press (document px).
    pub start_pointer: f64,
    pub start_gap: i32,
    pub coalesce: String,
}

/// Whether the multi-select frames / spacing overlay should show.
fn active(app: &PhotocraftApp) -> bool {
    app.ui.tool == Tool::Move
        && app.ui.transform.is_none()
        && app.guide_drag.is_none()
        && app.session.active().is_some_and(|st| {
            let sel = st.selected_layers();
            sel.len() >= 2 && !layer_gap_cmds::selection_frames(&st.doc, &sel).is_empty()
        })
}

fn segments(app: &PhotocraftApp) -> Vec<GapSeg> {
    let Some(st) = app.session.active() else { return Vec::new() };
    let sel = st.selected_layers();
    if sel.len() < 2 {
        return Vec::new();
    }
    layer_gap_cmds::selection_gaps(&st.doc, &sel)
}

/// Hit the whole strip between two neighbour frames (not only the label's midpoint).
/// Overlapping bands (`gap ≤ 0`) still hit a thickened strip at the shared edge so the user can
/// drag the signed gap (open a positive gap or deepen the overlap).
fn hit_strip(g: &GapSeg, x: f64, y: f64, tol: f64) -> bool {
    // Along the gap: the open space, thickened to at least `tol` when neighbours touch or overlap.
    let (along0, along1) = if g.to > g.from {
        (g.from, g.to)
    } else {
        let mid = (g.from + g.to) / 2.0;
        (mid - tol, mid + tol)
    };
    let pad = if g.to - g.from < tol { (tol - (g.to - g.from)) / 2.0 } else { 0.0 };
    let (a0, a1) = (along0 - pad, along1 + pad);
    let (c0, c1) = (g.cross0.min(g.cross1), g.cross0.max(g.cross1));
    let cross_pad = if c1 - c0 < tol { (tol - (c1 - c0)).max(0.0) / 2.0 } else { 0.0 };
    let (c0, c1) = (c0 - cross_pad, c1 + cross_pad);
    if g.horizontal {
        x >= a0 && x <= a1 && y >= c0 && y <= c1
    } else {
        y >= a0 && y <= a1 && x >= c0 && x <= c1
    }
}

/// Black or white, whichever reads better on `c`.
fn on(c: Color32) -> Color32 {
    let l = 0.299 * f32::from(c.r()) + 0.587 * f32::from(c.g()) + 0.114 * f32::from(c.b());
    if l > 150.0 { Color32::BLACK } else { Color32::WHITE }
}

fn screen_rect(xf: &ViewXform, r: [f64; 4]) -> egui::Rect {
    egui::Rect::from_two_pos(xf.to_screen(r[0] as f32, r[1] as f32), xf.to_screen(r[2] as f32, r[3] as f32))
}

/// Draw per-layer frames, neighbour gap labels, and set the resize cursor over a gap strip.
pub fn draw(app: &mut PhotocraftApp, painter: &egui::Painter, xf: &ViewXform, idx: usize, hover: Option<Pos2>) {
    if app.session.active_index() != Some(idx) || !active(app) {
        return;
    }
    // While ⌘-measuring to another layer, keep that overlay alone.
    let (command, pressed) = painter.ctx().input(|i| (i.modifiers.command, i.pointer.any_down()));
    if command && !pressed && app.layer_gap_drag.is_none() {
        return;
    }
    let Some(st) = app.session.active() else { return };
    let sel = st.selected_layers();
    let frames = layer_gap_cmds::selection_frames(&st.doc, &sel);
    if frames.len() < 2 {
        return;
    }
    let accent = crate::theme::Tokens::get(painter.ctx()).accent;
    let smart = app.ui.view.shows(app.ui.view.show.smart_guides);
    let gap_color = if smart { crate::snap_ui::smart_color(app) } else { accent };
    // One frame per selected layer (MasterGo-style); the Move tool's shared transform box may
    // still outline the union when Show Transform Controls is on.
    for (_, r) in &frames {
        let sr = screen_rect(xf, [f64::from(r.x0), f64::from(r.y0), f64::from(r.x1), f64::from(r.y1)]);
        painter.rect_stroke(sr, 0.0, Stroke::new(1.0, accent), egui::StrokeKind::Middle);
    }
    let segs = layer_gap_cmds::selection_gaps(&st.doc, &sel);
    let (dpi, size) = (f64::from(st.doc.resolution_dpi), st.doc.size);
    let ur = &app.session.prefs().units_and_rulers;
    let stroke = Stroke::new(1.0, gap_color);
    let px = |p: Pos2| pos2(p.x.round() + 0.5, p.y.round() + 0.5);
    let t = crate::theme::Tokens::get(painter.ctx());
    let font = egui::FontId::proportional(11.0);
    let tol = f64::from(HIT_PX) / f64::from(app.point_zoom().max(0.01));
    let mut hover_h = false;
    let mut hover_v = false;
    // Draw lines first, then badges. H/V gutters of a grid/diagonal pair meet at one point — put
    // H numbers above the bar and V numbers beside the bar so both stay readable.
    let mut placed: Vec<egui::Rect> = Vec::with_capacity(segs.len());
    for g in &segs {
        let (a, b) = if g.horizontal {
            (xf.to_screen(g.from as f32, g.at as f32), xf.to_screen(g.to as f32, g.at as f32))
        } else {
            (xf.to_screen(g.at as f32, g.from as f32), xf.to_screen(g.at as f32, g.to as f32))
        };
        let (a, b) = (px(a), px(b));
        if g.gap > 0 {
            painter.line_segment([a, b], stroke);
            let dir = (b - a).normalized();
            if dir.length_sq() > 0.0 {
                let cap = vec2(-dir.y, dir.x) * 4.0;
                for p in [a, b] {
                    painter.line_segment([p - cap, p + cap], stroke);
                }
            }
        }
        let extent = if g.horizontal { size.width } else { size.height } as f64;
        let text = format!("{} {}", ur.format(f64::from(g.gap), dpi, extent), ur.rulers.suffix());
        let galley = painter.layout_no_wrap(text, font.clone(), on(gap_color));
        let badge = galley.size() + vec2(8.0, 4.0);
        let mid = a + (b - a) / 2.0;
        let center = label_center(mid, badge, g.horizontal, &placed);
        let r = egui::Rect::from_center_size(center, badge);
        placed.push(r);
        painter.rect_filled(r, t.radius_sm, gap_color);
        painter.galley(r.center() - galley.size() / 2.0, galley, on(gap_color));
        if let Some(h) = hover.filter(|_| app.layer_gap_drag.is_none()) {
            let d = xf.to_doc(h);
            if hit_strip(g, d[0], d[1], tol) {
                if g.horizontal {
                    hover_h = true;
                } else {
                    hover_v = true;
                }
            }
        }
    }
    if app.layer_gap_drag.as_ref().is_some_and(|d| d.horizontal) || hover_h {
        painter.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    } else if app.layer_gap_drag.as_ref().is_some_and(|d| !d.horizontal) || hover_v {
        painter.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
    }
}

/// Screen centre for a gap number badge: H sits above the bar, V to the right; nudge away from
/// already-placed badges so a grid's H and V numbers never stack on the same pixel.
fn label_center(mid: Pos2, badge: egui::Vec2, horizontal: bool, placed: &[egui::Rect]) -> Pos2 {
    let mut c = if horizontal {
        pos2(mid.x, mid.y - badge.y * 0.5 - LABEL_PAD)
    } else {
        pos2(mid.x + badge.x * 0.5 + LABEL_PAD, mid.y)
    };
    for _ in 0..8 {
        let r = egui::Rect::from_center_size(c, badge);
        let Some(hit) = placed.iter().find(|p| p.intersects(r.expand(2.0))) else { break };
        if horizontal {
            // Push further up (or down if already above the collision).
            let up = hit.top() - badge.y * 0.5 - LABEL_PAD;
            let down = hit.bottom() + badge.y * 0.5 + LABEL_PAD;
            c.y = if (c.y - up).abs() <= (c.y - down).abs() { up } else { down };
        } else {
            let left = hit.left() - badge.x * 0.5 - LABEL_PAD;
            let right = hit.right() + badge.x * 0.5 + LABEL_PAD;
            c.x = if (c.x - right).abs() <= (c.x - left).abs() { right } else { left };
        }
    }
    c
}

/// Pointer input for between-frame gap drags. Returns true when consumed.
pub fn pointer(app: &mut PhotocraftApp, ev: ToolEvent, _mods: egui::Modifiers) -> bool {
    if app.layer_gap_drag.is_some() {
        match ev {
            ToolEvent::Move { x, y, .. } | ToolEvent::Up { x, y } => {
                let Some(drag) = app.layer_gap_drag.clone() else { return true };
                let pointer = if drag.horizontal { x } else { y };
                let delta = (pointer - drag.start_pointer).round() as i32;
                let gap = drag.start_gap.saturating_add(delta).clamp(-1_000_000, 1_000_000);
                let axis = if drag.horizontal { "horizontal" } else { "vertical" };
                let p = json!({
                    "axis": axis,
                    "gap": gap,
                    "index": drag.index,
                    "coalesce": drag.coalesce,
                });
                if let Err(e) = app.run("layer.gap.set", p) {
                    app.ui.status = e;
                    app.ui.status_error = true;
                }
                if matches!(ev, ToolEvent::Up { .. }) {
                    app.layer_gap_drag = None;
                }
                return true;
            }
            ToolEvent::Down { .. } => {}
        }
    }
    if !active(app) || app.drag.is_some() {
        return false;
    }
    let ToolEvent::Down { x, y, .. } = ev else { return false };
    let tol = f64::from(HIT_PX) / f64::from(app.point_zoom().max(0.01));
    let segs = segments(app);
    let Some(g) = segs.iter().find(|g| hit_strip(g, x, y, tol)) else { return false };
    app.layer_gap_drag = Some(GapDrag {
        horizontal: g.horizontal,
        index: g.index,
        start_pointer: if g.horizontal { x } else { y },
        start_gap: g.gap,
        coalesce: format!("layer-gap:{}:{}", if g.horizontal { "h" } else { "v" }, g.index),
    });
    true
}

/// Properties › Spacing: equal-gap fields for the multi-selection.
pub fn properties(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let Some(st) = app.session.active() else { return };
    let sel = st.selected_layers();
    let has_h = !layer_gap_cmds::gaps_on_axis(&st.doc, &sel, true).is_empty();
    let has_v = !layer_gap_cmds::gaps_on_axis(&st.doc, &sel, false).is_empty();
    if !has_h && !has_v {
        return;
    }
    if !crate::props_layout::section(ui, "spacing", tl!("Spacing")) {
        return;
    }
    let u = crate::layer_props_ui::LenUnit::of(app);
    let (dw, dh) = (f64::from(st.doc.size.width), f64::from(st.doc.size.height));
    let h_gap = layer_gap_cmds::uniform_gap(&st.doc, &sel, true);
    let v_gap = layer_gap_cmds::uniform_gap(&st.doc, &sel, false);
    // Full words ("H Gap" / 水平间距), not the 22 pt W/H column: measure labels so they never
    // sit under the value fields on a narrow Properties dock.
    let h_label = tl!("H Gap");
    let v_label = tl!("V Gap");
    let cols = usize::from(has_h) + usize::from(has_v);
    let label_w = gap_label_width(ui, &[has_h.then_some(h_label), has_v.then_some(v_label)].into_iter().flatten().collect::<Vec<_>>());
    let field_w = crate::props_layout::field_width(ui.available_width(), cols, label_w);
    let mut run = Vec::new();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = crate::props_layout::COL_GAP;
        if has_h {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = crate::props_layout::LABEL_GAP;
                if let Some(v) = gap_field(ui, "gap-h", h_label, h_gap, label_w, field_w, u, dw) {
                    run.push(("horizontal", v));
                }
            });
        }
        if has_v {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = crate::props_layout::LABEL_GAP;
                if let Some(v) = gap_field(ui, "gap-v", v_label, v_gap, label_w, field_w, u, dh) {
                    run.push(("vertical", v));
                }
            });
        }
    });
    ui.add_space(crate::theme::ROW_GAP);
    for (axis, gap) in run {
        if let Err(e) = app.run("layer.gap.set", json!({"axis": axis, "gap": gap})) {
            app.ui.status = e;
            app.ui.status_error = true;
        }
    }
}

/// Widest Spacing-row label (at least the short W/H column).
fn gap_label_width(ui: &egui::Ui, labels: &[&str]) -> f32 {
    let t = crate::theme::Tokens::get(ui.ctx());
    let font = egui::FontId::proportional(12.0);
    let text_w = labels
        .iter()
        .map(|s| ui.painter().layout_no_wrap((*s).to_owned(), font.clone(), t.text_dim).size().x)
        .fold(0.0_f32, f32::max)
        .ceil();
    text_w.max(crate::props_layout::LABEL_W)
}

fn gap_field(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    current: Option<i32>,
    label_w: f32,
    field_w: f32,
    u: crate::layer_props_ui::LenUnit,
    extent: f64,
) -> Option<i32> {
    let t = crate::theme::Tokens::get(ui.ctx());
    let (lr, _) = ui.allocate_exact_size(vec2(label_w, 22.0), egui::Sense::hover());
    ui.painter().text(
        pos2(lr.left(), lr.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.0),
        t.text_dim,
    );
    let key = ui.make_persistent_id(("layer-gap-field", id));
    let shown = current.map(|g| u.shown(f64::from(g), extent)).unwrap_or(0.0);
    let mut v = ui.data(|d| d.get_temp::<f32>(key)).unwrap_or(shown);
    let resp = crate::widgets::value_field(ui, &mut v, -300_000.0..=300_000.0, u.unit.suffix(), field_w);
    if current.is_none() {
        let _ = resp.clone().on_hover_text(tl!("Mixed values"));
    }
    if resp.dragged() || resp.has_focus() {
        ui.data_mut(|d| d.insert_temp(key, v));
        return None;
    }
    ui.data_mut(|d| d.remove::<f32>(key));
    if !(resp.drag_stopped() || resp.lost_focus() || resp.changed()) {
        return None;
    }
    let px = u.px(v, extent).round() as i32;
    match current {
        Some(c) if (px - c).abs() < 1 => None,
        _ => Some(px.clamp(-1_000_000, 1_000_000)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app_two_boxes() -> PhotocraftApp {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 400, "height": 200})).unwrap();
        app.run("layer.new.layer", json!({"name": "a"})).unwrap();
        app.run("select.rect", json!({"x": 10, "y": 40, "width": 40, "height": 40})).unwrap();
        app.run("edit.fill", json!({"color": "#ff0000"})).unwrap();
        app.run("select.deselect", json!({})).unwrap();
        let a = app.session.active().unwrap().active_layer.unwrap().0;
        app.run("layer.new.layer", json!({"name": "b"})).unwrap();
        app.run("select.rect", json!({"x": 100, "y": 40, "width": 40, "height": 40})).unwrap();
        app.run("edit.fill", json!({"color": "#0000ff"})).unwrap();
        app.run("select.deselect", json!({})).unwrap();
        let b = app.session.active().unwrap().active_layer.unwrap().0;
        app.run("layer.select", json!({"layer": a})).unwrap();
        app.run("layer.select", json!({"layer": b, "mode": "add"})).unwrap();
        app.ui.tool = Tool::Move;
        app.ui.view.extras = true;
        app.ui.view.show.smart_guides = true;
        app.sync_views();
        app.ui.views[0].zoom = 1.0;
        app.ui.views[0].fit_pending = false;
        app
    }

    #[test]
    fn dragging_a_gap_handle_sets_the_neighbour_spacing() {
        let mut app = app_two_boxes();
        // Anywhere in the strip between the frames (not only the midpoint label).
        assert!(pointer(&mut app, ToolEvent::Down { x: 60.0, y: 55.0, pressure: 1.0 }, egui::Modifiers::NONE), "press in the gap strip");
        assert!(app.layer_gap_drag.is_some());
        assert!(pointer(&mut app, ToolEvent::Move { x: 90.0, y: 55.0, pressure: 1.0 }, egui::Modifiers::NONE));
        assert!(pointer(&mut app, ToolEvent::Up { x: 90.0, y: 55.0 }, egui::Modifiers::NONE));
        assert!(app.layer_gap_drag.is_none());
        let st = app.session.active().unwrap();
        let sel = st.selected_layers();
        let g = layer_gap_cmds::gaps_on_axis(&st.doc, &sel, true);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].gap, 80); // was 50 (50→100), +30
    }

    #[test]
    fn overlapping_neighbours_expose_a_negative_gap() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 400, "height": 100})).unwrap();
        app.run("layer.new.layer", json!({"name": "a"})).unwrap();
        // Wide boxes whose centres stay in different columns, but edges overlap by 30 px.
        app.run("select.rect", json!({"x": 10, "y": 20, "width": 90, "height": 40})).unwrap();
        app.run("edit.fill", json!({"color": "#ff0000"})).unwrap();
        app.run("select.deselect", json!({})).unwrap();
        let a = app.session.active().unwrap().active_layer.unwrap().0;
        app.run("layer.new.layer", json!({"name": "b"})).unwrap();
        app.run("select.rect", json!({"x": 70, "y": 20, "width": 130, "height": 40})).unwrap();
        app.run("edit.fill", json!({"color": "#0000ff"})).unwrap();
        app.run("select.deselect", json!({})).unwrap();
        let b = app.session.active().unwrap().active_layer.unwrap().0;
        app.run("layer.select", json!({"layer": a})).unwrap();
        app.run("layer.select", json!({"layer": b, "mode": "add"})).unwrap();
        let st = app.session.active().unwrap();
        let sel = st.selected_layers();
        let g = layer_gap_cmds::gaps_on_axis(&st.doc, &sel, true);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].gap, -30, "overlap must stay negative, not clamped to 0");
        let h = layer_gap_cmds::uniform_gap(&st.doc, &sel, true);
        assert_eq!(h, Some(-30));
        // Stage label formats the real signed gap (was previously forced to 0).
        let ur = &app.session.prefs().units_and_rulers;
        assert_eq!(ur.format(f64::from(g[0].gap), 72.0, 400.0), "-30");
        // Drag starts from the real gap so opening the overlap is continuous.
        app.ui.tool = Tool::Move;
        assert!(pointer(&mut app, ToolEvent::Down { x: 85.0, y: 40.0, pressure: 1.0 }, egui::Modifiers::NONE));
        assert_eq!(app.layer_gap_drag.as_ref().map(|d| d.start_gap), Some(-30));
        // Dragging further left deepens the overlap (negative gap is accepted).
        assert!(pointer(&mut app, ToolEvent::Move { x: 75.0, y: 40.0, pressure: 1.0 }, egui::Modifiers::NONE));
        assert!(pointer(&mut app, ToolEvent::Up { x: 75.0, y: 40.0 }, egui::Modifiers::NONE));
        let st = app.session.active().unwrap();
        let g = layer_gap_cmds::gaps_on_axis(&st.doc, &st.selected_layers(), true);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].gap, -40);
    }

    #[test]
    fn gap_field_keeps_an_overlap_value_without_committing_zero() {
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(280.0, 60.0))
            .with_accessibility_check(false)
            .build_ui_state(
                |ui, committed: &mut Option<i32>| {
                    let u = crate::layer_props_ui::LenUnit {
                        unit: photocraft_engine::prefs::Unit::Pixels,
                        dpi: 72.0,
                        ppi: 72.0,
                    };
                    if let Some(v) = gap_field(ui, "gap-h-neg", "H Gap", Some(-20), 48.0, 80.0, u, 200.0) {
                        *committed = Some(v);
                    }
                },
                None,
            );
        h.run();
        assert!(h.state().is_none(), "idle paint must not snap overlap to 0");
    }

    #[test]
    fn multi_select_exposes_one_frame_per_layer() {
        let app = app_two_boxes();
        let st = app.session.active().unwrap();
        let frames = layer_gap_cmds::selection_frames(&st.doc, &st.selected_layers());
        assert_eq!(frames.len(), 2);
        assert!(active(&app));
    }

    #[test]
    fn properties_equal_gap_command_is_enabled_for_two_layers() {
        let app = app_two_boxes();
        assert!(app.session.is_enabled("layer.gap.set"));
    }

    #[test]
    fn spacing_labels_reserve_more_than_the_wh_column() {
        let mut h = egui_kittest::Harness::builder().with_size(vec2(280.0, 80.0)).build_ui(|ui| {
            let w = gap_label_width(ui, &["水平间距", "垂直间距"]);
            ui.data_mut(|d| d.insert_temp(egui::Id::new("gap-label-w"), w));
        });
        h.run();
        let w = h.ctx.data(|d| d.get_temp::<f32>(egui::Id::new("gap-label-w"))).unwrap_or(0.0);
        assert!(w > crate::props_layout::LABEL_W, "got {w}");
        assert!(w >= 48.0, "Chinese Spacing labels need room: {w}");
    }

    #[test]
    fn grid_selection_exposes_both_axis_gap_segments_for_the_stage() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 400, "height": 400})).unwrap();
        let spots = [[(10, 10), (80, 10)], [(10, 80), (80, 80)]];
        let mut ids = Vec::new();
        for row in &spots {
            for &(x, y) in row {
                app.run("layer.new.layer", json!({})).unwrap();
                app.run("select.rect", json!({"x": x, "y": y, "width": 40, "height": 40})).unwrap();
                app.run("edit.fill", json!({"color": "#00ff00"})).unwrap();
                app.run("select.deselect", json!({})).unwrap();
                ids.push(app.session.active().unwrap().active_layer.unwrap().0);
            }
        }
        app.run("layer.select", json!({"layer": ids[0]})).unwrap();
        for &id in &ids[1..] {
            app.run("layer.select", json!({"layer": id, "mode": "add"})).unwrap();
        }
        let st = app.session.active().unwrap();
        let sel = st.selected_layers();
        let segs = layer_gap_cmds::selection_gaps(&st.doc, &sel);
        assert!(segs.iter().any(|g| g.horizontal), "stage needs an H number: {segs:?}");
        assert!(segs.iter().any(|g| !g.horizontal), "stage needs a V number: {segs:?}");
    }

    #[test]
    fn h_and_v_gap_labels_do_not_stack_at_the_cross_centre() {
        // Same mid for both axes (grid gutter cross): without an offset only one number shows.
        let mid = pos2(100.0, 100.0);
        let badge = vec2(40.0, 18.0);
        let h = label_center(mid, badge, true, &[]);
        let v = label_center(mid, badge, false, &[egui::Rect::from_center_size(h, badge)]);
        let hr = egui::Rect::from_center_size(h, badge);
        let vr = egui::Rect::from_center_size(v, badge);
        assert!(!hr.intersects(vr), "H={hr:?} must not cover V={vr:?}");
        assert!(h.y < mid.y, "H label sits above the bar");
        assert!(v.x > mid.x, "V label sits to the right of the bar");
    }
}

//! The parts of the live calls (`change`, `selection`, `view`, `cell-box`; craft-web-host) that
//! read the workbook and the grid, kept apart from the browser so they are tested natively:
//! which cells' input changed between two workbooks, each sheet's lasting id, the sheet a host
//! asks about, and whether a range is in view.

use std::collections::HashMap;
use std::sync::Arc;

use craft_web_host::CellChange;
use gridcraft_engine::core::{CellRef, RangeRef};
use gridcraft_engine::model::{Sheet, Workbook};
use gridcraft_ui_egui::grid::Geo;

/// The most cells one comparison reports. Inserting a row above a large table moves every cell
/// below it; a host told about more than this many reads the workbook from a save instead.
pub const MAX_CELLS: usize = 5000;

/// Each sheet's id for the host, in the workbook's order: the `sheetId` the file gave it, which
/// every teammate who opened the same file shares and which stays through renames and moves. A
/// sheet the file did not have (added or copied here) gets `new-<n>`, from this session's own
/// identity for it. When two sheets hold the same file id (a copy keeps its source's), the older
/// one keeps it.
pub fn sheet_ids(wb: &Workbook) -> Vec<String> {
    let mut owner: HashMap<u32, u64> = HashMap::new();
    for sh in &wb.sheets {
        if sh.file_id != 0 {
            let uid = sh.uid.get();
            owner.entry(sh.file_id).and_modify(|o| *o = (*o).min(uid)).or_insert(uid);
        }
    }
    wb.sheets
        .iter()
        .map(|sh| {
            let uid = sh.uid.get();
            if sh.file_id != 0 && owner.get(&sh.file_id) == Some(&uid) { sh.file_id.to_string() } else { format!("new-{uid}") }
        })
        .collect()
}

/// The cells whose input (the formula with its `=`, or the text typed) differs between `old` and
/// `new`, in sheet order; a cell whose value changed only because it was calculated again is not
/// one of them. Sheets are matched by their identity, so a rename or a move is no change; a sheet
/// added since `old` reports every cell with input. At most [`MAX_CELLS`]; `true` when some were
/// left out.
pub fn changed_inputs(old: &Workbook, new: &Workbook) -> (Vec<CellChange>, bool) {
    let ids = sheet_ids(new);
    let mut out = Vec::new();
    for (sh, id) in new.sheets.iter().zip(ids) {
        let before = old.sheets.iter().find(|o| o.uid.get() == sh.uid.get());
        let cells: Vec<CellRef> = match before {
            Some(o) if Arc::ptr_eq(o, sh) => continue,
            Some(o) => o.cells.diff(&sh.cells),
            None => sh.cells.iter().map(|(c, _)| c).collect(),
        };
        let mut cells: Vec<CellRef> = cells
            .into_iter()
            .filter(|c| before.map_or(String::new(), |o| o.input_text(*c)) != sh.input_text(*c))
            .collect();
        cells.sort_by_key(|c| (c.row, c.col));
        cells.dedup();
        for c in cells {
            if out.len() == MAX_CELLS {
                return (out, true);
            }
            out.push(CellChange { sheet_id: id.clone(), sheet: sh.name.clone(), cell: c.a1(), input: sh.input_text(c) });
        }
    }
    (out, false)
}

/// The index of the sheet a host names: by its id first, then by its name (case as Excel ignores
/// it), and the active sheet when it names neither.
pub fn find_sheet(wb: &Workbook, sheet_id: Option<&str>, name: Option<&str>) -> Option<usize> {
    if let Some(id) = sheet_id {
        return sheet_ids(wb).iter().position(|s| s == id);
    }
    if let Some(name) = name {
        return wb.sheets.iter().position(|s| s.name.eq_ignore_ascii_case(name));
    }
    (wb.active_sheet() < wb.sheets.len()).then(|| wb.active_sheet())
}

/// A host's reference (`C8`, `B4:C9`, `$B$4`), or `None` when it is not one.
pub fn parse_ref(text: &str) -> Option<RangeRef> {
    RangeRef::parse(text.trim())
}

/// Where `r` is drawn, in the grid's own coordinates (egui points), and whether any of it is in
/// view: inside the cell area and, for a range wholly past a frozen pane, not under it.
pub fn range_place(geo: &Geo, sh: &Sheet, r: RangeRef) -> (egui::Rect, bool) {
    let mut rect = geo.range_rect(sh, r);
    if geo.rtl {
        rect = geo.mirror_rect(rect);
    }
    let mut seen = rect.intersect(geo.cells);
    if geo.paper.is_none() {
        if geo.fc > 0 && r.start.col >= geo.fc && !geo.rtl {
            seen.min.x = seen.min.x.max(geo.cells.left() + geo.frozen_w);
        }
        if geo.fr > 0 && r.start.row >= geo.fr {
            seen.min.y = seen.min.y.max(geo.cells.top() + geo.frozen_h);
        }
    }
    (rect, seen.width() > 0.0 && seen.height() > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridcraft_engine::core::Value;
    use gridcraft_engine::model::{Cell, Formula, SheetUid};

    fn at(a1: &str) -> CellRef {
        CellRef::parse(a1).unwrap_or_default()
    }

    fn plan() -> Workbook {
        let mut wb = Workbook::new();
        if let Some(sh) = wb.sheets.first_mut() {
            let sh = Arc::make_mut(sh);
            sh.name = "Plan".into();
            sh.file_id = 3;
            sh.cells.set(at("C5"), Cell::value(Value::Number(40.0)));
            sh.cells.set(at("C6"), Cell::value(Value::Number(2.0)));
            sh.cells.set(at("C8"), Cell::formula(Formula::new("=C5*C6")));
        }
        wb
    }

    fn edit(wb: &Workbook, f: impl FnOnce(&mut Sheet)) -> Workbook {
        let mut next = wb.clone();
        if let Some(sh) = next.sheets.first_mut() {
            f(Arc::make_mut(sh));
        }
        next
    }

    fn refs(changes: &[CellChange]) -> Vec<(String, String, String)> {
        changes.iter().map(|c| (c.sheet_id.clone(), c.cell.clone(), c.input.clone())).collect()
    }

    #[test]
    fn a_typed_cell_and_a_new_formula_are_changes_with_their_input() {
        let old = plan();
        let new = edit(&old, |sh| {
            sh.cells.set(at("C6"), Cell::value(Value::Number(3.0)));
            sh.cells.set(at("D9"), Cell::formula(Formula::new("=SUM(C5:C6)")));
        });
        let (changes, cut) = changed_inputs(&old, &new);
        assert!(!cut);
        assert_eq!(refs(&changes), [("3".into(), "C6".into(), "3".into()), ("3".into(), "D9".into(), "=SUM(C5:C6)".into())]);
        assert!(changes.iter().all(|c| c.sheet == "Plan"));
    }

    #[test]
    fn a_value_calculated_again_is_not_a_change() {
        let old = plan();
        let new = edit(&old, |sh| {
            if let Some(c) = sh.cells.get_mut(at("C8")) {
                c.value = Value::Number(80.0);
            }
        });
        assert_eq!(changed_inputs(&old, &new).0, []);
    }

    #[test]
    fn a_cleared_cell_is_a_change_to_nothing_and_an_unchanged_sheet_is_skipped() {
        let old = plan();
        let new = edit(&old, |sh| {
            sh.cells.remove(at("C5"));
        });
        assert_eq!(refs(&changed_inputs(&old, &new).0), [("3".into(), "C5".into(), String::new())]);
        assert_eq!(changed_inputs(&old, &old.clone()).0, []);
    }

    #[test]
    fn a_rename_keeps_the_sheet_id_and_is_no_change() {
        let old = plan();
        let new = edit(&old, |sh| sh.name = "Quarter".into());
        assert_eq!(changed_inputs(&old, &new).0, []);
        assert_eq!(sheet_ids(&new), ["3"]);
        assert_eq!(find_sheet(&new, Some("3"), Some("Plan")), Some(0), "the id wins over an old name");
        assert_eq!(find_sheet(&new, None, Some("quarter")), Some(0));
        assert_eq!(find_sheet(&new, None, Some("Plan")), None);
        assert_eq!(find_sheet(&new, Some("9"), None), None);
        assert_eq!(find_sheet(&new, None, None), Some(0));
    }

    #[test]
    fn a_copied_sheet_gets_its_own_id_and_reports_its_cells() {
        let old = plan();
        let mut new = old.clone();
        let mut copy = (*new.sheets[0]).clone();
        copy.uid = SheetUid::fresh();
        copy.name = "Plan (2)".into();
        // Put first, the copy still does not take the source's id.
        new.sheets.insert(0, Arc::new(copy));
        let ids = sheet_ids(&new);
        assert_eq!(ids[1], "3");
        assert!(ids[0].starts_with("new-"), "{ids:?}");
        let (changes, _) = changed_inputs(&old, &new);
        assert_eq!(changes.len(), 3);
        assert!(changes.iter().all(|c| c.sheet_id == ids[0] && c.sheet == "Plan (2)"));
    }

    #[test]
    fn a_huge_move_is_cut_at_the_limit_and_says_so() {
        let old = plan();
        let new = edit(&old, |sh| {
            for row in 0..(MAX_CELLS as u32 + 10) {
                sh.cells.set(CellRef::new(row + 20, 0), Cell::value(Value::Number(f64::from(row))));
            }
        });
        let (changes, cut) = changed_inputs(&old, &new);
        assert!(cut);
        assert_eq!(changes.len(), MAX_CELLS);
    }

    #[test]
    fn host_references_parse_as_cells_or_ranges() {
        assert_eq!(parse_ref("C8"), Some(RangeRef::cell(at("C8"))));
        assert_eq!(parse_ref(" B4:C9 ").map(|r| r.a1()), Some("B4:C9".into()));
        assert_eq!(parse_ref("$B$4"), Some(RangeRef::cell(at("B4"))));
        for bad in ["", "  ", "Plan!C8", "hello", "C8;D9"] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_range_in_view_is_visible_and_one_scrolled_off_or_under_the_freeze_is_not() {
        let sh = Sheet::new("Plan");
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 400.0));
        let geo = Geo::new(&sh, rect, egui::Vec2::ZERO);
        let (b4, seen) = range_place(&geo, &sh, RangeRef::parse("B4:C9").unwrap_or(RangeRef::cell(at("B4"))));
        assert!(seen);
        assert!(b4.width() > 0.0 && b4.height() > 0.0);
        let (_, far) = range_place(&geo, &sh, RangeRef::cell(at("A5000")));
        assert!(!far, "scrolled out of view");

        // Freeze two rows and scroll down: row 5 now sits under the frozen rows.
        let mut frozen = Sheet::new("Plan");
        frozen.freeze = Some((2, 0));
        let scrolled = Geo::new(&frozen, rect, egui::vec2(0.0, 3.0 * frozen.default_row_height));
        let (_, under) = range_place(&scrolled, &frozen, RangeRef::cell(at("A5")));
        assert!(!under, "under the frozen pane");
        let (_, top) = range_place(&scrolled, &frozen, RangeRef::cell(at("A1")));
        assert!(top, "a frozen cell stays in view");
        let (_, below) = range_place(&scrolled, &frozen, RangeRef::cell(at("A9")));
        assert!(below);
    }
}

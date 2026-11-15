//! What a screen reader is told about the grid while it has the keys (Felipe's fork; prepara-sim-kit
//! `kit/craft-host/CRAFT-A11Y-SCOPE.md`): the grid's AccessKit node is named by the active cell, so the web runner's
//! mirror (the kit's eframe copy) says the cell's address and sheet, what it shows, and its formula, on each move.

use gridcraft_engine::core::CellRef;
use gridcraft_engine::model::{Sheet, Workbook};

/// The grid node's label, value and description for the active cell `c` of `sh`: "B5, Plan"; the text the cell shows,
/// or "blank"; and "formula =B3/B6" when it holds a formula that is not hidden. While the cell is being typed in
/// (`typing`), the value is the text typed and the description says so.
pub fn cell_speech(wb: &Workbook, sh: &Sheet, c: CellRef, typing: Option<&str>) -> (String, String, Option<String>) {
    let label = format!("{}, {}", c.a1(), sh.name);
    if let Some(text) = typing {
        let value = if text.is_empty() { "blank".to_owned() } else { text.to_owned() };
        return (label, value, Some("being typed".to_owned()));
    }
    let shown = gridcraft_engine::display::cell_text(wb, sh, c);
    let value = if shown.trim().is_empty() { "blank".to_owned() } else { shown };
    let input = sh.input_text(c);
    let description = (input.starts_with('=') && !gridcraft_engine::display::formula_hidden(wb, sh, c)).then(|| format!("formula {input}"));
    (label, value, description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridcraft_engine::core::Value;
    use gridcraft_engine::model::{Cell, Formula};
    use std::sync::Arc;

    fn at(a1: &str) -> CellRef {
        CellRef::parse(a1).unwrap_or_default()
    }

    fn plan() -> Workbook {
        let mut wb = Workbook::new();
        if let Some(sh) = wb.sheets.first_mut() {
            let sh = Arc::make_mut(sh);
            sh.name = "Plan".into();
            sh.cells.set(at("B3"), Cell::value(Value::Number(1290.0)));
            sh.cells.set(at("A1"), Cell::value(Value::Text("Bracken Vale Storage".into())));
            let mut f = Cell::formula(Formula::new("=B3*2"));
            f.value = Value::Number(2580.0);
            sh.cells.set(at("B4"), f);
        }
        wb
    }

    #[test]
    fn a_number_text_and_empty_cell_are_said_as_shown() {
        let wb = plan();
        let sh = &wb.sheets[0];
        assert_eq!(cell_speech(&wb, sh, at("B3"), None), ("B3, Plan".into(), "1290".into(), None));
        assert_eq!(cell_speech(&wb, sh, at("A1"), None), ("A1, Plan".into(), "Bracken Vale Storage".into(), None));
        assert_eq!(cell_speech(&wb, sh, at("C9"), None), ("C9, Plan".into(), "blank".into(), None));
    }

    #[test]
    fn a_formula_is_said_with_what_it_shows_and_the_formula() {
        let wb = plan();
        let (label, value, description) = cell_speech(&wb, &wb.sheets[0], at("B4"), None);
        assert_eq!(label, "B4, Plan");
        assert_eq!(value, "2580", "what the cell shows, as the student sees it");
        assert_eq!(description.as_deref(), Some("formula =B3*2"));
    }

    #[test]
    fn a_cell_being_typed_says_what_is_typed() {
        let wb = plan();
        let sh = &wb.sheets[0];
        assert_eq!(cell_speech(&wb, sh, at("B5"), Some("=SUM(")), ("B5, Plan".into(), "=SUM(".into(), Some("being typed".into())));
        assert_eq!(cell_speech(&wb, sh, at("B5"), Some("")).1, "blank");
    }
}

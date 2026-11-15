//! Host modes: a page that runs GridCraft (as an iframe, or in its own document) opens workbooks
//! in it and stores what it saves instead of the browser downloading. The messages and the two
//! transports are `craft-web-host`'s (storytold/craft-libs), the same contract as the other Craft
//! apps' bridges, with the `gridcraft:` prefix. This module is GridCraft's part of it.
//!
//! - `?host=parent` (an `<iframe>`, such as a Nextcloud app) talks to the same-origin parent over
//!   `postMessage`; `data-host="page"` on the canvas (the page's own document, no iframe) uses
//!   DOM `CustomEvent`s on the canvas instead. `?host=parent` wins when both are present. With no
//!   parent, or a parent on another origin, there is no bridge: the normal app with downloads.
//! - `gridcraft:ready` once the app is listening; send a file now. GridCraft reads bytes, so it
//!   says `file: false` and a host sends `bytes`.
//! - `gridcraft:open` (from the host): `{ name, bytes }`. It opens like a workbook picked with
//!   File › Open or dropped on the window; a file over [`MAX_OPEN_BYTES`] is refused before it is
//!   copied.
//! - `gridcraft:save` (to the host): `{ name, saveAs, bytes }` for every file GridCraft would
//!   otherwise download: Save and Save As (an .xlsx workbook) and Export as PDF. The host stores
//!   it and reports failures itself. GridCraft doesn't tell the host which saves make a file the
//!   document, so `saveAs` is always `false`.
//! - `gridcraft:dirty` whenever unsaved changes appear or go away, `gridcraft:failed` when the
//!   app couldn't start.
//!
//! - The live calls, for a page that shows a teammate's work (FelipeMAffonso/prepara-sim-kit
//!   `kit/craft-host/GRIDCRAFT-CALLS-SCOPE.md`): `gridcraft:change` with each edited cell's input
//!   (an edit the host's own commands made says `by: "host"`), `gridcraft:selection`,
//!   `gridcraft:view` when the grid scrolls, zooms, resizes or moves a frozen pane, and an answer
//!   to each `gridcraft:cell-box` ask with the box around a cell or range in CSS pixels of the
//!   frame. None of them holds a computed value: the save stays the file.
//!
//! In both host modes the page starts with an empty workbook instead of a sample, saves and PDF
//! exports are handed to the page rather than downloaded, and `?author=` names the user for new
//! notes and comments.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use craft_web_host::web::{FileData, Host, Message};
use craft_web_host::{
    BoxRect, By, CHANGE_GAP_MS, CellBoxAnswer, CellBoxAsk, ChangeQueue, Pace, Paced, SELECTION_GAP_MS, SelectionReport,
};
use gridcraft_engine::model::Workbook;
use gridcraft_ui_egui::{Inbox, Services, SheetApp};

use crate::hostmode::open_name;
use crate::live;

/// The app id in the host messages (`gridcraft:ready`).
const APP: &str = "gridcraft";

/// Saved files that count as a workbook: the document is then clean under the name it was saved as.
const WORKBOOK_EXTS: &[&str] = &["xlsx"];

/// The largest workbook taken from the host page, refused before it is copied.
const MAX_OPEN_BYTES: u64 = 512 << 20;

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;

#[derive(Clone)]
pub struct Bridge {
    host: Rc<Host>,
    canvas: web_sys::HtmlCanvasElement,
    /// Names of the files handed to the host since the last frame.
    saved: Rc<RefCell<Vec<String>>>,
    live: Rc<RefCell<Live>>,
    /// `cell-box` asks heard since the last frame, answered after it.
    asks: Rc<RefCell<Vec<CellBoxAsk>>>,
}

/// What the live calls remember between frames.
struct Live {
    /// The open workbook as last compared (its document uid and contents).
    base: Option<(u64, Arc<Workbook>)>,
    user: ChangeQueue,
    host: ChangeQueue,
    selection: Paced<SelectionReport>,
    /// The grid's geometry as last reported by a `view`.
    view: Option<Vec<f64>>,
    /// When the bridge started (milliseconds since the epoch): `at` in a change counts from it.
    started: f64,
}

impl Bridge {
    /// The bridge for this page, or `None` when GridCraft isn't hosted: no `?host=parent` and no
    /// `data-host="page"` on the canvas, an unframed `?host=parent` page (it stays the normal app
    /// with downloads), or a parent on another origin.
    pub fn detect(canvas: &web_sys::HtmlCanvasElement) -> Option<Self> {
        let live = Live {
            base: None,
            user: ChangeQueue::new(CHANGE_GAP_MS),
            host: ChangeQueue::new(CHANGE_GAP_MS),
            selection: Paced::new(SELECTION_GAP_MS),
            view: None,
            started: js_sys::Date::now(),
        };
        Some(Self {
            host: Rc::new(Host::connect(APP, canvas)?),
            canvas: canvas.clone(),
            saved: Rc::default(),
            live: Rc::new(RefCell::new(live)),
            asks: Rc::default(),
        })
    }

    /// After the app's per-frame logic, which runs the page's commands: what changed there was the
    /// host's doing (a teammate's value it set), and is reported as `by: "host"`.
    pub fn after_logic(&self, app: &SheetApp) {
        self.compare(app, By::Host);
    }

    /// Queue the cells whose input changed since the last comparison.
    fn compare(&self, app: &SheetApp, by: By) {
        let mut live = self.live.borrow_mut();
        let Some(d) = app.session.active() else {
            live.base = None;
            return;
        };
        if let Some((uid, wb)) = &live.base
            && *uid == d.uid
            && !Arc::ptr_eq(wb, &d.wb)
        {
            let (cells, cut) = live::changed_inputs(wb, &d.wb);
            if cut {
                log::warn!("over {} cells changed at once; the host hears the first {}", live::MAX_CELLS, live::MAX_CELLS);
            }
            let queue = if by == By::Host { &mut live.host } else { &mut live.user };
            for c in cells {
                queue.push(c);
            }
        }
        // Another workbook (one the host opened) is a new start, not a change.
        live.base = Some((d.uid, d.wb.clone()));
    }

    /// The live calls after a frame: changes, the selection, the view, and box answers.
    fn sync_live(&self, app: &SheetApp, ctx: &egui::Context) {
        self.compare(app, By::User);
        let now = js_sys::Date::now();
        let report = selection_report(app);
        let view = view_key(app, ctx);
        let mut wait: Option<f64> = None;
        let mut later = |ms: f64| wait = Some(wait.map_or(ms, |w: f64| w.min(ms)));
        {
            let mut live = self.live.borrow_mut();
            let at = now - live.started;
            for by in [By::User, By::Host] {
                let queue = if by == By::Host { &mut live.host } else { &mut live.user };
                match queue.take(now) {
                    Pace::Send(cells) => {
                        if let Err(e) = self.host.post_change(&cells, by, at) {
                            log::warn!("couldn't post a change: {e}");
                        }
                    }
                    Pace::Wait(ms) => later(ms),
                    Pace::Idle => {}
                }
            }
            if let Some(report) = report {
                live.selection.offer(report);
            }
            match live.selection.take(now) {
                Pace::Send(report) => {
                    if let Err(e) = self.host.post_selection(&report) {
                        log::warn!("couldn't post the selection: {e}");
                    }
                }
                Pace::Wait(ms) => later(ms),
                Pace::Idle => {}
            }
            if view.is_some() && view != live.view {
                live.view = view;
                if let Err(e) = self.host.post_view() {
                    log::warn!("couldn't post a view change: {e}");
                }
            }
        }
        let asks = std::mem::take(&mut *self.asks.borrow_mut());
        for ask in asks {
            let answer = self.cell_box(app, ctx, ask);
            if let Err(e) = self.host.post_cell_box(&answer) {
                log::warn!("couldn't answer a cell-box ask: {e}");
            }
        }
        if let Some(ms) = wait {
            ctx.request_repaint_after(std::time::Duration::from_millis(ms.ceil().max(1.0) as u64));
        }
    }

    /// Where the asked cell or range is drawn on the sheet shown, in CSS pixels of the frame.
    fn cell_box(&self, app: &SheetApp, ctx: &egui::Context, ask: CellBoxAsk) -> CellBoxAnswer {
        let mut answer = CellBoxAnswer {
            id: ask.id,
            sheet_id: ask.sheet_id.clone().unwrap_or_default(),
            sheet: ask.sheet.clone().unwrap_or_default(),
            cell: ask.cell,
            bounds: None,
            visible: false,
        };
        let Some(d) = app.session.active() else { return answer };
        let Some(i) = live::find_sheet(&d.wb, ask.sheet_id.as_deref(), ask.sheet.as_deref()) else { return answer };
        let Some(sh) = d.wb.sheets.get(i) else { return answer };
        answer.sheet_id = live::sheet_ids(&d.wb).get(i).cloned().unwrap_or_default();
        answer.sheet = sh.name.clone();
        // Only the sheet shown is drawn.
        let (Some(r), Some(geo)) = (live::parse_ref(&answer.cell), app.grid.geo.as_ref()) else { return answer };
        if i != d.wb.active_sheet() {
            return answer;
        }
        let (rect, visible) = live::range_place(geo, sh, r);
        let z = f64::from(ctx.zoom_factor());
        let place = self.canvas.get_bounding_client_rect();
        answer.bounds = Some(BoxRect {
            x: place.left() + f64::from(rect.min.x) * z,
            y: place.top() + f64::from(rect.min.y) * z,
            width: f64::from(rect.width()) * z,
            height: f64::from(rect.height()) * z,
        });
        answer.visible = visible;
        answer
    }

    /// Send saved and exported files to the host instead of downloading them, and hear files
    /// from it.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = bridge.post_save(name, bytes) {
                log::error!("couldn't hand {name} to the page: {e}");
            }
        }));
    }

    /// `?author=`: the host's name for the user signs new notes and comments.
    pub fn set_author(app: &mut SheetApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.prefs.user_name = author;
        }
    }

    pub fn post_ready(&self) {
        let _ = self.host.post_ready(env!("CARGO_PKG_VERSION"), false);
    }

    pub fn post_failed(&self, error: &str) {
        let _ = self.host.post_failed(error, None);
    }

    /// After each frame: a workbook handed to the host this frame counts as saved under the
    /// name it was saved as, and the host hears when unsaved changes appear or go away (the
    /// crate does not repeat a state it already reported).
    pub fn sync(&self, app: &mut SheetApp, ctx: &egui::Context) {
        let saved = std::mem::take(&mut *self.saved.borrow_mut());
        if let Some(name) = saved.iter().rev().find(|name| is_workbook(name))
            && let Some(doc) = app.session.active_mut()
        {
            doc.saved = doc.wb.clone();
            if doc.path.is_none() {
                doc.title = name.clone();
            }
        }
        self.host.post_dirty(app.session.documents().iter().any(|d| d.is_dirty()));
        self.sync_live(app, ctx);
    }

    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = file_name(path);
        self.host.post_save(&name, bytes, None, false)?;
        self.saved.borrow_mut().push(name);
        Ok(())
    }

    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        let asks = self.asks.clone();
        self.host.listen(move |message| match message {
            Message::Open { name, data } => deliver_open(&inbox, &ctx, &name, &data),
            Message::CellBox(ask) => {
                asks.borrow_mut().push(ask);
                ctx.request_repaint();
            }
            _ => {}
        });
    }
}

/// What is selected on the sheet shown, and the cell editor's text while it is open there.
fn selection_report(app: &SheetApp) -> Option<SelectionReport> {
    let d = app.session.active()?;
    let i = d.wb.active_sheet();
    let sh = d.wb.sheets.get(i)?;
    let typing = app.editor.as_ref().filter(|e| e.doc == d.uid && e.sheet_uid == sh.uid.get()).map(|e| e.text.clone());
    Some(SelectionReport {
        sheet_id: live::sheet_ids(&d.wb).get(i).cloned().unwrap_or_default(),
        sheet: sh.name.clone(),
        range: d.selection.current().a1(),
        active: d.selection.active.a1(),
        typing,
    })
}

/// The grid's geometry that moves every box: its place, scroll, zoom, frozen panes, the sheet
/// shown and the page's zoom. `None` before the grid is first drawn.
fn view_key(app: &SheetApp, ctx: &egui::Context) -> Option<Vec<f64>> {
    let geo = app.grid.geo.as_ref()?;
    let d = app.session.active()?;
    let sheet = d.wb.sheets.get(d.wb.active_sheet()).map_or(0, |s| s.uid.get());
    Some(vec![
        d.uid as f64,
        sheet as f64,
        f64::from(geo.rect.min.x),
        f64::from(geo.rect.min.y),
        f64::from(geo.rect.max.x),
        f64::from(geo.rect.max.y),
        f64::from(geo.scroll.x),
        f64::from(geo.scroll.y),
        f64::from(geo.z),
        f64::from(geo.fr),
        f64::from(geo.fc),
        f64::from(ctx.zoom_factor()),
    ])
}

/// The file an open message carries, pushed onto the inbox for the next frame, like a dropped
/// file. A host's file is untrusted input: an oversized one is refused before it is copied into
/// the app's memory (the data's own length, not the host's claim).
fn deliver_open(inbox: &Inbox, ctx: &egui::Context, name: &str, data: &FileData) {
    let FileData::Bytes(array) = data else {
        log::warn!("host file {name} came as a File; GridCraft reads bytes only; not opened");
        return;
    };
    if data.size() > MAX_OPEN_BYTES as f64 {
        log::warn!("host file is {} bytes, over the {} MiB host open limit; not opened", data.size(), MAX_OPEN_BYTES >> 20);
        return;
    }
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((open_name(name), array.to_vec()));
    ctx.request_repaint();
}

fn is_workbook(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| WORKBOOK_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

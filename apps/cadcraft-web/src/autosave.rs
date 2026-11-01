//! Crash recovery in the browser: drawings with unsaved changes are kept as DXF copies in
//! IndexedDB (craft-libs' `craft-web-recovery`: one store per tab session, held by a Web Lock so
//! a second tab leaves a running tab's copies alone). The engine's autosaver decides what to
//! write ([`cadcraft_engine::autosave::Autosaver::plan_urgent`]); this carries it out at most
//! every [`EVERY`] seconds and when the tab is hidden. Copies left by tabs that are gone fill
//! Drawing Recovery at start. A copy goes when its drawing is saved or closed, or discarded in
//! Drawing Recovery. The page asks "Leave site?" while a drawing has unsaved changes.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use cadcraft_engine::autosave::{Autosaver, Job, Recoverable};
use cadcraft_ui_egui::CadApp;
use craft_web_recovery::{HiddenHook, Opened, Recovered, Snapshot, Store, UnloadGuard};

/// The least time between two autosave passes (seconds).
const EVERY: f64 = 10.0;
/// The autosave "folder" the engine builds copy names under; only the file name part is used.
const DIR: &str = "browser";

/// The store and the copies earlier tabs left (see [`open`]).
pub struct Startup {
    pub store: Store,
    pub recovered: Vec<Recovered>,
    pub warnings: Vec<String>,
}

pub async fn open() -> Startup {
    let Opened { store, recovered, warnings } = craft_web_recovery::open("cadcraft").await;
    Startup { store, recovered, warnings: warnings.iter().map(ToString::to_string).collect() }
}

/// Put the copies from earlier tabs into Drawing Recovery (newest first) and start autosaving.
pub fn install(app: &mut CadApp, startup: Startup) -> WebAutosave {
    for w in &startup.warnings {
        log::warn!("crash recovery: {w}");
    }
    for r in &startup.recovered {
        app.recovery.items.push(Recoverable {
            path: r.key.clone(),
            drawing: r.name.clone(),
            modified: (r.saved_at / 1000.0) as u64,
            bytes: r.bytes.len() as u64,
        });
        app.recovery.data.insert(r.key.clone(), r.bytes.clone());
    }
    if !startup.recovered.is_empty() {
        let n = startup.recovered.len();
        app.set_status(format!("{n} drawing{} recovered from this browser: see Drawing Recovery.", if n == 1 { "" } else { "s" }));
    }
    app.autosave = Some(Autosaver::new(startup.store.session().to_string()));
    WebAutosave::new(startup.store)
}

pub struct WebAutosave {
    store: Store,
    /// Drawing uid by store key, for the writes in flight.
    keys: HashMap<String, u64>,
    last: f64,
    unsaved: Rc<Cell<bool>>,
    hidden: Rc<Cell<bool>>,
    _guard: Option<UnloadGuard>,
    _hook: Option<HiddenHook>,
}

impl WebAutosave {
    fn new(store: Store) -> Self {
        let unsaved = Rc::new(Cell::new(false));
        let hidden = Rc::new(Cell::new(false));
        let flag = unsaved.clone();
        let _guard = UnloadGuard::new(move || flag.get()).map_err(|e| log::warn!("unload guard: {e}")).ok();
        let flag = hidden.clone();
        let _hook = HiddenHook::new(move || flag.set(true)).map_err(|e| log::warn!("hidden hook: {e}")).ok();
        Self { store, keys: HashMap::new(), last: f64::NEG_INFINITY, unsaved, hidden, _guard, _hook }
    }

    /// Every frame (hidden ticks too): handle Drawing Recovery's discards, take in finished
    /// writes, and write what changed if the interval is up or the tab was just hidden.
    pub fn tick(&mut self, app: &mut CadApp, ctx: &egui::Context) {
        for key in std::mem::take(&mut app.recovery.discarded) {
            self.store.remove(&key);
        }
        let Some(saver) = app.autosave.as_mut() else { return };
        for w in self.store.take_written() {
            if let Err(e) = &w.result {
                log::warn!("recovery copy not written: {e}");
                if let Some(uid) = self.keys.remove(&w.key) {
                    saver.forget_written(uid);
                }
            }
        }
        self.unsaved.set(app.session.docs.iter().any(|d| d.is_dirty()));
        let now = ctx.input(|i| i.time);
        let hidden = self.hidden.replace(false);
        if !hidden && now - self.last < EVERY {
            if self.unsaved.get() {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(EVERY - (now - self.last)));
            }
            return;
        }
        self.last = now;
        for job in saver.plan_urgent(&app.session, craft_web_recovery::now_ms() / 1000.0, DIR) {
            match job {
                Job::Delete { path } => self.store.remove(&key_of(&path)),
                Job::Write { uid, path, doc } => {
                    let key = key_of(&path);
                    let Some(hooks) = cadcraft_engine::cmd::file::io() else { continue };
                    let name = app.session.docs.iter().find(|d| d.uid == uid).map_or_else(|| "Drawing".to_string(), |d| d.title.clone());
                    let result = (hooks.write)(&doc, "autosave.dxf")
                        .and_then(|bytes| self.store.put(&key, 0, Snapshot { name: &name, path: None, meta: None, bytes: &bytes }));
                    match result {
                        Ok(()) => {
                            self.keys.insert(key, uid);
                        }
                        Err(e) => {
                            log::warn!("recovery copy not stored: {e}");
                            saver.forget_written(uid);
                        }
                    }
                }
            }
        }
    }
}

/// The store key of an autosave path: its file name (a copy adopted from Drawing Recovery is
/// already just the key).
fn key_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

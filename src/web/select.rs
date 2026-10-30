//! The printer selector shared by the per-printer tabs (Files, Logs, System). The choice is
//! remembered in `user_prefs.selected_printer_id`.

use askama::Template;

use crate::db::prefs;
use crate::db::printers::{self, Printer};
use crate::error::AppError;
use crate::state::AppState;

#[derive(Template)]
#[template(path = "partials/printer_select.html")]
pub struct PrinterSelect {
    /// The tab's `/ui/...` path; changing the selection reloads it with `?printer=<id>`.
    pub tab_path: &'static str,
    pub options: Vec<(i64, String)>,
    pub selected: i64,
}

impl PrinterSelect {
    fn is_selected(&self, id: &i64) -> bool {
        *id == self.selected
    }
}

pub struct Selection {
    pub printer: Printer,
    pub select: PrinterSelect,
}

/// Picks the printer for a tab: the one asked for (and remember it), else the remembered one,
/// else the first enabled printer. `None` when no printer is enabled.
pub async fn select(
    state: &AppState,
    user_id: i64,
    requested: Option<i64>,
    tab_path: &'static str,
) -> Result<Option<Selection>, AppError> {
    let enabled: Vec<Printer> = printers::list(&state.db)
        .await?
        .into_iter()
        .filter(|p| p.enabled)
        .collect();
    let remembered = prefs::selected_printer(&state.db, user_id).await?;
    let find = |id: Option<i64>| id.and_then(|id| enabled.iter().position(|p| p.id == id));
    let Some(idx) = find(requested)
        .or_else(|| find(remembered))
        .or((!enabled.is_empty()).then_some(0))
    else {
        return Ok(None);
    };
    let printer = enabled[idx].clone();
    if requested == Some(printer.id) && remembered != Some(printer.id) {
        prefs::set_selected_printer(&state.db, user_id, printer.id).await?;
    }
    Ok(Some(Selection {
        select: PrinterSelect {
            tab_path,
            options: enabled.iter().map(|p| (p.id, p.name.clone())).collect(),
            selected: printer.id,
        },
        printer,
    }))
}

/// Loads a printer for a `/p/{id}/...` route: 404 if it doesn't exist, 400 if disabled.
pub async fn enabled_printer(state: &AppState, id: i64) -> Result<Printer, AppError> {
    let p = printers::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    if !p.enabled {
        return Err(AppError::BadRequest("That printer is disabled.".into()));
    }
    Ok(p)
}

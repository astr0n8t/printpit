use super::Db;

#[derive(Debug, Clone, Copy)]
pub struct Prefs {
    pub auto_refresh: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { auto_refresh: true }
    }
}

pub async fn get(db: &Db, user_id: i64) -> Result<Prefs, sqlx::Error> {
    let auto: Option<bool> = sqlx::query_scalar("SELECT auto_refresh FROM user_prefs WHERE user_id = ?")
        .bind(user_id)
        .fetch_optional(db)
        .await?;
    Ok(auto.map_or_else(Prefs::default, |auto_refresh| Prefs { auto_refresh }))
}

pub async fn set_auto_refresh(db: &Db, user_id: i64, on: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO user_prefs (user_id, auto_refresh) VALUES (?, ?)
         ON CONFLICT (user_id) DO UPDATE SET auto_refresh = excluded.auto_refresh",
    )
    .bind(user_id)
    .bind(on)
    .execute(db)
    .await?;
    Ok(())
}

/// The printer last picked on the Files/Logs/System tabs.
pub async fn selected_printer(db: &Db, user_id: i64) -> Result<Option<i64>, sqlx::Error> {
    let id: Option<Option<i64>> =
        sqlx::query_scalar("SELECT selected_printer_id FROM user_prefs WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    Ok(id.flatten())
}

pub async fn set_selected_printer(db: &Db, user_id: i64, printer_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO user_prefs (user_id, selected_printer_id) VALUES (?, ?)
         ON CONFLICT (user_id) DO UPDATE SET selected_printer_id = excluded.selected_printer_id",
    )
    .bind(user_id)
    .bind(printer_id)
    .execute(db)
    .await?;
    Ok(())
}

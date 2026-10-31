//! Temperature presets: named hotend/bed targets that operators can apply to any printer.

use super::{Db, now};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Preset {
    pub id: i64,
    pub name: String,
    pub extruder: Option<f64>,
    pub bed: Option<f64>,
}

pub async fn list(db: &Db) -> Result<Vec<Preset>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, extruder, bed FROM temp_presets
         ORDER BY extruder IS NULL, extruder, bed, name",
    )
    .fetch_all(db)
    .await
}

pub async fn get(db: &Db, id: i64) -> Result<Option<Preset>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, extruder, bed FROM temp_presets WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn name_taken(db: &Db, name: &str, except: Option<i64>) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM temp_presets WHERE name = ? AND id != ?)")
        .bind(name)
        .bind(except.unwrap_or(-1))
        .fetch_one(db)
        .await
}

pub async fn create(
    db: &Db,
    name: &str,
    extruder: Option<f64>,
    bed: Option<f64>,
) -> Result<i64, sqlx::Error> {
    let ts = now();
    sqlx::query_scalar(
        "INSERT INTO temp_presets (name, extruder, bed, created_at, updated_at) VALUES (?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(name)
    .bind(extruder)
    .bind(bed)
    .bind(ts)
    .bind(ts)
    .fetch_one(db)
    .await
}

pub async fn update(
    db: &Db,
    id: i64,
    name: &str,
    extruder: Option<f64>,
    bed: Option<f64>,
) -> Result<bool, sqlx::Error> {
    let res =
        sqlx::query("UPDATE temp_presets SET name = ?, extruder = ?, bed = ?, updated_at = ? WHERE id = ?")
            .bind(name)
            .bind(extruder)
            .bind(bed)
            .bind(now())
            .bind(id)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn delete(db: &Db, id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM temp_presets WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

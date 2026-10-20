use super::{Device, MapDetail, TopologyService};
use crate::error::AppError;
use serde::Serialize;
use sqlx::{Sqlite, Transaction};
#[derive(Serialize, sqlx::FromRow)]
pub struct Outage {
    pub id: i64,
    pub label: String,
    pub ip: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub end_reason: String,
}
impl TopologyService {
    pub async fn initialize_history(&self) -> Result<(), AppError> {
        let maps: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(topology_maps)")
                .fetch_all(&self.pool)
                .await?;
        if !maps.iter().any(|c| c.1 == "edit_revision") {
            sqlx::query(
                "ALTER TABLE topology_maps ADD COLUMN edit_revision INTEGER NOT NULL DEFAULT 1",
            )
            .execute(&self.pool)
            .await?;
        }
        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(topology_nodes)")
                .fetch_all(&self.pool)
                .await?;
        for name in ["last_success_at", "status_since"] {
            if !columns.iter().any(|c| c.1 == name) {
                sqlx::query(&format!(
                    "ALTER TABLE topology_nodes ADD COLUMN {name} INTEGER NOT NULL DEFAULT 0"
                ))
                .execute(&self.pool)
                .await?;
            }
        }
        for statement in [
   "CREATE TABLE IF NOT EXISTS topology_undo(id INTEGER PRIMARY KEY AUTOINCREMENT,map_id INTEGER NOT NULL REFERENCES topology_maps(id) ON DELETE CASCADE,after_revision INTEGER NOT NULL,description TEXT NOT NULL,snapshot TEXT NOT NULL)",
   "CREATE TABLE IF NOT EXISTS device_outages(id INTEGER PRIMARY KEY AUTOINCREMENT,map_id INTEGER NOT NULL,device_key TEXT NOT NULL,label TEXT NOT NULL,ip TEXT NOT NULL,started_at INTEGER NOT NULL,ended_at INTEGER,end_reason TEXT NOT NULL DEFAULT '')",
   "CREATE INDEX IF NOT EXISTS device_outages_lookup ON device_outages(map_id,device_key,id)",
   "UPDATE topology_nodes SET last_success_at=last_checked WHERE last_success_at=0 AND status='up' AND probe_error='' AND latency_ms IS NOT NULL",
   "UPDATE topology_nodes SET status_since=COALESCE((SELECT MAX(occurred_at) FROM network_events WHERE device_id=topology_nodes.id AND status=topology_nodes.status),last_checked) WHERE status_since=0 AND last_checked>0",
   "INSERT INTO device_outages(map_id,device_key,label,ip,started_at) SELECT map_id,cell_id,label,ip,status_since FROM topology_nodes n WHERE status='down' AND status_since>0 AND NOT EXISTS(SELECT 1 FROM device_outages o WHERE o.map_id=n.map_id AND o.device_key=n.cell_id)",
  ]{sqlx::query(statement).execute(&self.pool).await?;}
        Ok(())
    }
    pub(super) async fn snapshot(
        tx: &mut Transaction<'_, Sqlite>,
        id: i64,
        description: &str,
    ) -> Result<(), AppError> {
        let row: Option<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT name,edges_json,version,edit_revision FROM topology_maps WHERE id=?",
        )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
        let (name, edges, version, edit_revision) = row.ok_or(AppError::NotFound)?;
        let nodes: Vec<Device> =
            sqlx::query_as("SELECT * FROM topology_nodes WHERE map_id=? ORDER BY id")
                .bind(id)
                .fetch_all(&mut **tx)
                .await?;
        let detail = MapDetail {
            id,
            name,
            version,
            edit_revision,
            undo_label: String::new(),
            nodes,
            edges: serde_json::from_str(&edges)
                .map_err(|_| AppError::Validation("Invalid stored connections".into()))?,
        };
        let snapshot = serde_json::to_string(&detail)
            .map_err(|_| AppError::Validation("Unable to save edit history".into()))?;
        sqlx::query("INSERT INTO topology_undo(map_id,after_revision,description,snapshot) VALUES (?,?,?,?)").bind(id).bind(edit_revision+1).bind(description).bind(snapshot).execute(&mut **tx).await?;
        sqlx::query("DELETE FROM topology_undo WHERE map_id=? AND id NOT IN (SELECT id FROM topology_undo WHERE map_id=? ORDER BY id DESC LIMIT 20)").bind(id).bind(id).execute(&mut **tx).await?;
        Ok(())
    }
    pub(super) async fn bump_edit(
        tx: &mut Transaction<'_, Sqlite>,
        id: i64,
    ) -> Result<(), AppError> {
        sqlx::query("UPDATE topology_maps SET edit_revision=edit_revision+1 WHERE id=?")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
    pub async fn undo(&self, id: i64, revision: i64) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let current: Option<i64> =
            sqlx::query_scalar("SELECT edit_revision FROM topology_maps WHERE id=?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if current != Some(revision) {
            return Err(AppError::Conflict(
                "Map changed; refresh before undoing".into(),
            ));
        }
        let row:Option<(i64,i64,String)>=sqlx::query_as("SELECT id,after_revision,snapshot FROM topology_undo WHERE map_id=? ORDER BY id DESC LIMIT 1").bind(id).fetch_optional(&mut *tx).await?;
        let (history_id, after, snapshot) =
            row.ok_or_else(|| AppError::Validation("No saved edit to undo".into()))?;
        if after != revision {
            return Err(AppError::Conflict(
                "Edit history changed; refresh first".into(),
            ));
        }
        let before: MapDetail = serde_json::from_str(&snapshot)
            .map_err(|_| AppError::Validation("Invalid edit history".into()))?;
        let existing: Vec<Device> = sqlx::query_as("SELECT * FROM topology_nodes WHERE map_id=?")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        let wanted: std::collections::HashSet<_> = before.nodes.iter().map(|n| n.id).collect();
        for node in existing.iter().filter(|n| !wanted.contains(&n.id)) {
            sqlx::query("DELETE FROM topology_nodes WHERE id=? AND map_id=?")
                .bind(node.id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        for node in &before.nodes {
            sqlx::query("UPDATE device_outages SET ended_at=unixepoch(),end_reason='Address changed by undo' WHERE map_id=? AND device_key=? AND ended_at IS NULL AND EXISTS(SELECT 1 FROM topology_nodes WHERE id=? AND ip<>?)").bind(id).bind(&node.cell_id).bind(node.id).bind(&node.ip).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO topology_nodes(id,map_id,cell_id,label,kind,ip,group_id,enabled,x,y,width,height,version) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET label=excluded.label,kind=excluded.kind,ip=excluded.ip,group_id=excluded.group_id,enabled=excluded.enabled,x=excluded.x,y=excluded.y,width=excluded.width,height=excluded.height,version=topology_nodes.version+1,status=CASE WHEN topology_nodes.ip=excluded.ip THEN topology_nodes.status ELSE 'unknown' END,failures=CASE WHEN topology_nodes.ip=excluded.ip THEN topology_nodes.failures ELSE 0 END,last_checked=CASE WHEN topology_nodes.ip=excluded.ip THEN topology_nodes.last_checked ELSE 0 END,last_success_at=CASE WHEN topology_nodes.ip=excluded.ip THEN topology_nodes.last_success_at ELSE 0 END,status_since=CASE WHEN topology_nodes.ip=excluded.ip THEN topology_nodes.status_since ELSE 0 END,latency_ms=CASE WHEN topology_nodes.ip=excluded.ip THEN topology_nodes.latency_ms ELSE NULL END,probe_error='',next_check=0")
   .bind(node.id).bind(id).bind(&node.cell_id).bind(&node.label).bind(&node.kind).bind(&node.ip).bind(node.group_id).bind(node.enabled).bind(node.x).bind(node.y).bind(node.width).bind(node.height).bind(node.version+1).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE topology_maps SET edges_json=?,version=version+1,edit_revision=edit_revision+1 WHERE id=?").bind(serde_json::to_string(&before.edges).map_err(|_|AppError::Validation("Invalid history edges".into()))?).bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM topology_undo WHERE id=?")
            .bind(history_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE topology_undo SET after_revision=? WHERE id=(SELECT MAX(id) FROM topology_undo WHERE map_id=?)").bind(revision+1).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn outages(&self, id: i64) -> Result<Vec<Outage>, AppError> {
        let node: Option<(i64, String)> =
            sqlx::query_as("SELECT map_id,cell_id FROM topology_nodes WHERE id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        let (map, key) = node.ok_or(AppError::NotFound)?;
        Ok(sqlx::query_as("SELECT id,label,ip,started_at,ended_at,end_reason FROM device_outages WHERE map_id=? AND device_key=? ORDER BY id DESC LIMIT 50").bind(map).bind(key).fetch_all(&self.pool).await?)
    }
}

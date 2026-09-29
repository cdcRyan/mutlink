use crate::model::*;
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::{path::Path, sync::Mutex};

pub struct Store(Mutex<Connection>);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS sessions (
               id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace TEXT NOT NULL,
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS tasks (
               id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
               prompt TEXT NOT NULL, state TEXT NOT NULL, source_device TEXT NOT NULL,
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, error TEXT,
               request_json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS messages (
               seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
               body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events (
               seq INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
               session_id TEXT NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL,
               created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS events_session ON events(session_id, seq);
             CREATE TABLE IF NOT EXISTS approvals (
               id TEXT PRIMARY KEY, task_id TEXT NOT NULL, tool TEXT NOT NULL,
               arguments TEXT NOT NULL, state TEXT NOT NULL,
               created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS packets (
               id TEXT PRIMARY KEY, received_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS remote_replies (
               id TEXT PRIMARY KEY, body TEXT NOT NULL, created_at INTEGER NOT NULL);",
        )?;
        Ok(Self(Mutex::new(db)))
    }

    pub fn recover(&self) -> Result<usize> {
        let db = self.0.lock().unwrap();
        let count = db.execute(
            "UPDATE tasks SET state='unknown', error='Agent restarted; execution must be reconciled before retrying', updated_at=?1
             WHERE state IN ('queued','running','waiting_approval')", [now()])?;
        db.execute(
            "UPDATE approvals SET state='expired' WHERE state='pending'",
            [],
        )?;
        Ok(count)
    }

    pub fn admit(
        &self,
        request: &RunRequest,
        source: &str,
        workspace: &str,
    ) -> Result<(Task, bool)> {
        if request.prompt.trim().is_empty()
            || request.prompt.len() > 64 * 1024
            || request.id.len() > 128
        {
            bail!("Prompt must contain 1–65536 bytes and task ID at most 128 bytes");
        }
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let body = serde_json::to_string(request)?;
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT request_json,source_device FROM tasks WHERE id=?1",
                [&request.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((old, owner)) = prior {
            if old != body || owner != source {
                bail!("Task ID already exists with a different request");
            }
            let task = query_task(&tx, &request.id)?;
            return Ok((task, false));
        }
        let session_id = request.session_id.clone().unwrap_or_else(id);
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [&session_id],
            |r| r.get(0),
        )?;
        if !exists {
            if request.session_id.is_some() {
                bail!("Session does not exist");
            }
            let title: String = request.prompt.chars().take(64).collect();
            tx.execute(
                "INSERT INTO sessions VALUES (?1,?2,?3,?4,?4)",
                params![session_id, title, workspace, now()],
            )?;
        } else {
            let (root, busy): (String, bool) = tx.query_row(
                "SELECT workspace, EXISTS(SELECT 1 FROM tasks WHERE session_id=?1 AND state IN ('queued','running','waiting_approval')) FROM sessions WHERE id=?1",
                [&session_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if root != workspace {
                bail!("Session belongs to a different workspace");
            }
            if busy {
                bail!("This session already has an active task");
            }
            let other: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE session_id=?1 AND source_device<>?2)",
                params![session_id, source],
                |r| r.get(0),
            )?;
            if other {
                bail!("Session belongs to another device");
            }
        }
        let task = Task {
            id: request.id.clone(),
            session_id,
            prompt: request.prompt.clone(),
            state: "queued".into(),
            source_device: source.into(),
            created_at: now(),
            updated_at: now(),
            error: None,
        };
        tx.execute(
            "INSERT INTO tasks VALUES (?1,?2,?3,?4,?5,?6,?6,NULL,?7)",
            params![
                task.id,
                task.session_id,
                task.prompt,
                task.state,
                source,
                task.created_at,
                body
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            params![task.session_id, now()],
        )?;
        tx.commit()?;
        Ok((task, true))
    }

    pub fn task(&self, task_id: &str) -> Result<Task> {
        query_task(&self.0.lock().unwrap(), task_id)
    }

    pub fn tasks(&self) -> Result<Vec<Task>> {
        let db = self.0.lock().unwrap();
        let mut q = db.prepare("SELECT id,session_id,prompt,state,source_device,created_at,updated_at,error FROM tasks ORDER BY created_at DESC LIMIT 100")?;
        let values = q
            .query_map([], task_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(values)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let db = self.0.lock().unwrap();
        let mut q = db.prepare("SELECT id,title,workspace,created_at,updated_at FROM sessions ORDER BY updated_at DESC LIMIT 100")?;
        let values = q
            .query_map([], |r| {
                Ok(Session {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    workspace: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(values)
    }

    pub fn set_state(&self, task_id: &str, state: &str, error: Option<&str>) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE tasks SET state=?2,error=?3,updated_at=?4 WHERE id=?1",
            params![task_id, state, error, now()],
        )?;
        Ok(())
    }

    pub fn event(&self, task: &Task, kind: &str, data: Value) -> Result<Event> {
        let db = self.0.lock().unwrap();
        db.execute(
            "INSERT INTO events(task_id,session_id,kind,data,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![task.id, task.session_id, kind, data.to_string(), now()],
        )?;
        Ok(Event {
            seq: db.last_insert_rowid(),
            task_id: task.id.clone(),
            session_id: task.session_id.clone(),
            kind: kind.into(),
            data,
            created_at: now(),
        })
    }

    pub fn events(&self, session_id: Option<&str>, after: i64) -> Result<Vec<Event>> {
        let db = self.0.lock().unwrap();
        let mut q = db.prepare("SELECT seq,task_id,session_id,kind,data,created_at FROM events WHERE seq>?1 AND (?2 IS NULL OR session_id=?2) ORDER BY seq LIMIT 500")?;
        let values = q
            .query_map(params![after, session_id], |r| {
                let data: String = r.get(4)?;
                Ok(Event {
                    seq: r.get(0)?,
                    task_id: r.get(1)?,
                    session_id: r.get(2)?,
                    kind: r.get(3)?,
                    data: serde_json::from_str(&data).unwrap_or(Value::Null),
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(values)
    }

    pub fn message(&self, session: &str, message: &Value) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO messages(session_id,body) VALUES (?1,?2)",
            params![session, message.to_string()],
        )?;
        Ok(())
    }

    pub fn message_batch(&self, session: &str, messages: &[Value]) -> Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        for message in messages {
            tx.execute(
                "INSERT INTO messages(session_id,body) VALUES (?1,?2)",
                params![session, message.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn messages(&self, session: &str) -> Result<Vec<Value>> {
        let db = self.0.lock().unwrap();
        let mut q = db.prepare("SELECT body FROM messages WHERE session_id=?1 ORDER BY seq")?;
        let rows = q
            .query_map([session], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }

    pub fn create_approval(&self, task_id: &str, tool: &str, arguments: Value) -> Result<Approval> {
        let approval = Approval {
            id: id(),
            task_id: task_id.into(),
            tool: tool.into(),
            arguments,
            state: "pending".into(),
            created_at: now(),
            expires_at: now() + 300,
        };
        self.0.lock().unwrap().execute(
            "INSERT INTO approvals VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                approval.id,
                task_id,
                tool,
                approval.arguments.to_string(),
                approval.state,
                approval.created_at,
                approval.expires_at
            ],
        )?;
        Ok(approval)
    }

    pub fn approvals(&self) -> Result<Vec<Approval>> {
        let db = self.0.lock().unwrap();
        db.execute(
            "UPDATE approvals SET state='expired' WHERE state='pending' AND expires_at<=?1",
            [now()],
        )?;
        let mut q = db.prepare("SELECT id,task_id,tool,arguments,state,created_at,expires_at FROM approvals WHERE state='pending' ORDER BY created_at")?;
        let rows = q
            .query_map([], |r| {
                let arguments: String = r.get(3)?;
                Ok(Approval {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    tool: r.get(2)?,
                    arguments: serde_json::from_str(&arguments).unwrap_or(Value::Null),
                    state: r.get(4)?,
                    created_at: r.get(5)?,
                    expires_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn decide(&self, approval_id: &str, allow: bool) -> Result<()> {
        let changed = self.0.lock().unwrap().execute(
            "UPDATE approvals SET state=?2 WHERE id=?1 AND state='pending' AND expires_at>?3",
            params![
                approval_id,
                if allow { "approved" } else { "rejected" },
                now()
            ],
        )?;
        if changed != 1 {
            bail!("Approval expired, already consumed or does not exist");
        }
        Ok(())
    }

    pub fn consume_approval(&self, approval_id: &str) -> Result<Option<bool>> {
        let db = self.0.lock().unwrap();
        let state: String = db.query_row(
            "SELECT CASE WHEN expires_at<=?2 THEN 'expired' ELSE state END FROM approvals WHERE id=?1",
            params![approval_id, now()], |r| r.get(0))?;
        if state == "pending" {
            return Ok(None);
        }
        if state != "approved" {
            return Ok(Some(false));
        }
        let n = db.execute(
            "UPDATE approvals SET state='consumed' WHERE id=?1 AND state='approved'",
            [approval_id],
        )?;
        Ok(Some(n == 1))
    }

    pub fn accept_packet(&self, packet_id: &str) -> Result<()> {
        let db = self.0.lock().unwrap();
        db.execute("DELETE FROM packets WHERE received_at<?1", [now() - 900])?;
        if db.execute(
            "INSERT OR IGNORE INTO packets VALUES (?1,?2)",
            params![packet_id, now()],
        )? != 1
        {
            bail!("Replay rejected");
        }
        Ok(())
    }
}

fn task_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        session_id: r.get(1)?,
        prompt: r.get(2)?,
        state: r.get(3)?,
        source_device: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        error: r.get(7)?,
    })
}

fn query_task(db: &Connection, task_id: &str) -> Result<Task> {
    Ok(db.query_row("SELECT id,session_id,prompt,state,source_device,created_at,updated_at,error FROM tasks WHERE id=?1",
        [task_id], task_row)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_is_idempotent_and_conflicts_are_rejected() {
        let s = Store::open(Path::new(":memory:")).unwrap();
        let mut r = RunRequest {
            id: id(),
            session_id: None,
            prompt: "hello".into(),
            plan: false,
        };
        let (t, fresh) = s.admit(&r, "alice", "/work").unwrap();
        assert!(fresh);
        assert!(!s.admit(&r, "alice", "/work").unwrap().1);
        assert!(s.admit(&r, "bob", "/work").is_err());
        r.prompt = "different".into();
        assert!(s.admit(&r, "alice", "/work").is_err());
        assert_eq!(s.recover().unwrap(), 1);
        assert_eq!(s.task(&t.id).unwrap().state, "unknown");
    }

    #[test]
    fn approvals_are_consumed_once() {
        let s = Store::open(Path::new(":memory:")).unwrap();
        let a = s
            .create_approval("task", "shell.run", serde_json::json!({"command":"pwd"}))
            .unwrap();
        assert_eq!(s.consume_approval(&a.id).unwrap(), None);
        s.decide(&a.id, true).unwrap();
        assert_eq!(s.consume_approval(&a.id).unwrap(), Some(true));
        assert_eq!(s.consume_approval(&a.id).unwrap(), Some(false));
        assert!(s.decide(&a.id, true).is_err());
    }
}

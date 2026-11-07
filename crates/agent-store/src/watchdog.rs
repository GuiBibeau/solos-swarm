//! The Watchdog's record of what it told the operator. Alert ids are stable
//! for as long as one problem lasts, so a known id is never told again,
//! after a restart too. Only the Watchdog writes these rows.

use crate::{SqliteStore, StoreError};
use rusqlite::{Row, params};
use solos_agent_core::model::{Ms, OutboundMessage, TicketId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertRow {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub ticket: Option<TicketId>,
    /// The Watchdog retries this alert's stalled Assignment.
    pub retry: bool,
    pub raised_ms: Ms,
    /// What goes to the operator; its `MessageId` is the alert id.
    pub message: OutboundMessage,
    /// When the operator's channel took it.
    pub sent_ms: Option<Ms>,
}

const COLUMNS: &str = "id, kind, subject, ticket, retry, raised_ms, message, sent_ms";

type Raw = (
    String,
    String,
    String,
    Option<String>,
    bool,
    Ms,
    String,
    Option<Ms>,
);

fn raw(row: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn parse(
    (id, kind, subject, ticket, retry, raised_ms, message, sent_ms): Raw,
) -> Result<AlertRow, StoreError> {
    Ok(AlertRow {
        id,
        kind,
        subject,
        ticket: ticket.map(TicketId),
        retry,
        raised_ms,
        message: serde_json::from_str(&message)?,
        sent_ms,
    })
}

impl SqliteStore {
    /// Record an alert unless its id is already known. Returns whether it
    /// is new; a known one keeps its row as it was.
    pub fn record_alert(&self, alert: &AlertRow) -> Result<bool, StoreError> {
        let added = self
            .conn
            .prepare_cached(
                "INSERT INTO watchdog_alerts
                     (id, kind, subject, ticket, retry, raised_ms, message, sent_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (id) DO NOTHING",
            )?
            .execute(params![
                alert.id,
                alert.kind,
                alert.subject,
                alert.ticket.as_ref().map(TicketId::as_str),
                alert.retry,
                alert.raised_ms,
                serde_json::to_string(&alert.message)?,
                alert.sent_ms,
            ])?;
        Ok(added > 0)
    }

    /// Alerts the operator's channel has not taken yet, oldest first.
    pub fn unsent_alerts(&self) -> Result<Vec<AlertRow>, StoreError> {
        self.alert_rows(&format!(
            "SELECT {COLUMNS} FROM watchdog_alerts WHERE sent_ms IS NULL ORDER BY raised_ms, id"
        ))
    }

    /// Every alert, oldest first.
    pub fn alerts(&self) -> Result<Vec<AlertRow>, StoreError> {
        self.alert_rows(&format!(
            "SELECT {COLUMNS} FROM watchdog_alerts ORDER BY raised_ms, id"
        ))
    }

    /// Record that the operator's channel took the alert. A repeat changes
    /// nothing.
    pub fn mark_alert_sent(&self, id: &str, at_ms: Ms) -> Result<(), StoreError> {
        self.conn
            .prepare_cached(
                "UPDATE watchdog_alerts SET sent_ms = ?2 WHERE id = ?1 AND sent_ms IS NULL",
            )?
            .execute(params![id, at_ms])?;
        Ok(())
    }

    /// Whether an alert with `id` was ever recorded.
    pub fn has_alert(&self, id: &str) -> Result<bool, StoreError> {
        let found: u32 = self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM watchdog_alerts WHERE id = ?1")?
            .query_row([id], |row| row.get(0))?;
        Ok(found > 0)
    }

    /// Whether the Watchdog has retried one of the Ticket's Assignments.
    pub fn watchdog_retried(&self, ticket: &TicketId) -> Result<bool, StoreError> {
        let found: u32 = self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM watchdog_alerts WHERE ticket = ?1 AND retry = 1")?
            .query_row([ticket.as_str()], |row| row.get(0))?;
        Ok(found > 0)
    }

    fn alert_rows(&self, sql: &str) -> Result<Vec<AlertRow>, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map([], raw)?;
        rows.map(|row| parse(row?)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::AlertRow;
    use crate::SqliteStore;
    use solos_agent_core::model::{ChannelRef, MessageId, MessageKind, OutboundMessage, TicketId};
    use std::collections::BTreeMap;

    fn alert(id: &str, raised_ms: u64, retry: bool) -> AlertRow {
        AlertRow {
            id: id.into(),
            kind: "stalled".into(),
            subject: format!("assignment:as_1:{raised_ms}"),
            ticket: Some(TicketId::new("tk_1")),
            retry,
            raised_ms,
            message: OutboundMessage {
                id: MessageId::new(id),
                channel: ChannelRef {
                    source: "ops".into(),
                    address: BTreeMap::new(),
                },
                kind: MessageKind::Attention,
                text: "stalled".into(),
                ticket: Some(TicketId::new("tk_1")),
                ask: None,
            },
            sent_ms: None,
        }
    }

    #[test]
    fn an_alert_is_recorded_once_and_stays_unsent_until_marked() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.record_alert(&alert("wd_a", 10, false)).unwrap());
        assert!(!store.record_alert(&alert("wd_a", 20, true)).unwrap());
        assert!(store.record_alert(&alert("wd_b", 15, false)).unwrap());
        let unsent: Vec<_> = store
            .unsent_alerts()
            .unwrap()
            .into_iter()
            .map(|a| (a.id, a.raised_ms, a.retry))
            .collect();
        assert_eq!(
            unsent,
            [("wd_a".into(), 10, false), ("wd_b".into(), 15, false)],
            "a known id keeps its first row"
        );
        store.mark_alert_sent("wd_a", 30).unwrap();
        store.mark_alert_sent("wd_a", 40).unwrap();
        let all = store.alerts().unwrap();
        assert_eq!(all[0].sent_ms, Some(30));
        assert_eq!(all[0].message, alert("wd_a", 10, false).message);
        assert_eq!(store.unsent_alerts().unwrap().len(), 1);
    }

    #[test]
    fn a_ticket_counts_as_retried_once_an_alert_retries_it() {
        let store = SqliteStore::open_in_memory().unwrap();
        let ticket = TicketId::new("tk_1");
        store.record_alert(&alert("wd_a", 10, false)).unwrap();
        assert!(!store.watchdog_retried(&ticket).unwrap());
        store.record_alert(&alert("wd_b", 20, true)).unwrap();
        assert!(store.watchdog_retried(&ticket).unwrap());
        assert!(!store.watchdog_retried(&TicketId::new("tk_2")).unwrap());
    }
}

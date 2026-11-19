//! 每应用有界内存事件；SSE 缺口与重启不冒充持久日志。

use super::{PlatformResult, now, opaque};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use tgdrive_contract::events::{HostEvent, SyncRequiredEvent, event_name};
use tgdrive_contract::http::MAX_EVENT_JOURNAL_ENTRIES;
use tokio::sync::broadcast;

struct Journal {
    sequence: u64,
    recent: VecDeque<HostEvent>,
    sender: broadcast::Sender<HostEvent>,
}
pub(crate) struct EventHub {
    epoch: String,
    journals: Mutex<BTreeMap<String, Journal>>,
}
impl EventHub {
    pub fn new() -> Self {
        Self {
            epoch: opaque(),
            journals: Mutex::new(BTreeMap::new()),
        }
    }
    fn journal<'a>(journals: &'a mut BTreeMap<String, Journal>, app: &str) -> &'a mut Journal {
        journals.entry(app.to_owned()).or_insert_with(|| {
            let (sender, _) = broadcast::channel(MAX_EVENT_JOURNAL_ENTRIES as usize);
            Journal {
                sequence: 0,
                recent: VecDeque::new(),
                sender,
            }
        })
    }
    fn publish<T: Serialize>(&self, app: &str, event: &str, data: &T) -> PlatformResult<HostEvent> {
        let data = serde_json::to_value(data)?;
        let mut journals = self.journals.lock().unwrap();
        let journal = Self::journal(&mut journals, app);
        journal.sequence += 1;
        let event = HostEvent {
            cursor: format!("{}-{}", self.epoch, journal.sequence),
            occurred_at: now(),
            event: event.into(),
            data,
        };
        if journal.recent.len() == MAX_EVENT_JOURNAL_ENTRIES as usize {
            journal.recent.pop_front();
        }
        journal.recent.push_back(event.clone());
        let _ = journal.sender.send(event.clone());
        Ok(event)
    }
    pub fn emit<T: Serialize>(&self, app: &str, event: &str, data: &T) -> PlatformResult<()> {
        self.publish(app, event, data).map(|_| ())
    }
    pub fn subscribe(
        &self,
        app: &str,
        last: Option<&str>,
    ) -> PlatformResult<(VecDeque<HostEvent>, broadcast::Receiver<HostEvent>)> {
        let mut journals = self.journals.lock().unwrap();
        let journal = Self::journal(&mut journals, app);
        let receiver = journal.sender.subscribe();
        let position = last
            .and_then(|cursor| cursor.rsplit_once('-'))
            .filter(|(epoch, _)| *epoch == self.epoch)
            .and_then(|(_, seq)| seq.parse::<u64>().ok());
        let minimum = journal
            .recent
            .front()
            .and_then(|event| event.cursor.rsplit_once('-'))
            .and_then(|(_, seq)| seq.parse::<u64>().ok())
            .unwrap_or(journal.sequence + 1);
        if let Some(position) = position
            && position >= minimum.saturating_sub(1)
            && position <= journal.sequence
        {
            return Ok((
                journal
                    .recent
                    .iter()
                    .filter(|event| {
                        event
                            .cursor
                            .rsplit_once('-')
                            .and_then(|(_, seq)| seq.parse::<u64>().ok())
                            .is_some_and(|seq| seq > position)
                    })
                    .cloned()
                    .collect(),
                receiver,
            ));
        }
        journal.sequence += 1;
        let sync = HostEvent {
            cursor: format!("{}-{}", self.epoch, journal.sequence),
            occurred_at: now(),
            event: event_name::SYNC_REQUIRED.into(),
            data: serde_json::to_value(SyncRequiredEvent {
                reason: if last.is_none() {
                    "initial"
                } else if position.is_none() {
                    "host_restarted"
                } else {
                    "cursor_expired"
                }
                .into(),
            })?,
        };
        journal.recent.push_back(sync.clone());
        if journal.recent.len() > MAX_EVENT_JOURNAL_ENTRIES as usize {
            journal.recent.pop_front();
        }
        Ok((VecDeque::from([sync]), receiver))
    }
    pub fn sync(&self, app: &str, reason: &str) -> PlatformResult<HostEvent> {
        self.publish(
            app,
            event_name::SYNC_REQUIRED,
            &SyncRequiredEvent {
                reason: reason.into(),
            },
        )
    }
    pub fn remove(&self, app: &str) {
        self.journals.lock().unwrap().remove(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initial_restart_and_window_gaps_require_sync() {
        let hub = EventHub::new();
        let (events, _) = hub.subscribe("a", None).unwrap();
        assert_eq!(events[0].event, event_name::SYNC_REQUIRED);
        let old = events[0].cursor.clone();
        for _ in 0..MAX_EVENT_JOURNAL_ENTRIES + 1 {
            hub.emit(
                "a",
                event_name::KV_CHANGED,
                &tgdrive_contract::events::KvChangedEvent {
                    key: "k".into(),
                    revision: None,
                },
            )
            .unwrap();
        }
        let (events, _) = hub.subscribe("a", Some(&old)).unwrap();
        assert_eq!(events[0].data["reason"], "cursor_expired");
        let other = EventHub::new();
        let (events, _) = other.subscribe("a", Some(&old)).unwrap();
        assert_eq!(events[0].data["reason"], "host_restarted");
        let (events, _) = hub.subscribe("b", None).unwrap();
        assert_eq!(events.len(), 1);
    }
}

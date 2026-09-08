use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTopic {
    Agent,
    Engine,
    Cache,
    Approval,
    Generation,
    Knowledge,
    Attachment,
    Evaluation,
    Terminal,
    Preview,
    Policy,
    Checkpoint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppEvent {
    pub sequence: u64,
    pub emitted_at: chrono::DateTime<chrono::Utc>,
    pub topic: EventTopic,
    pub action: String,
    pub resource_id: Option<String>,
    pub data: serde_json::Value,
}

pub struct EventBus {
    next_sequence: AtomicU64,
    sender: broadcast::Sender<AppEvent>,
    history: RwLock<VecDeque<AppEvent>>,
    capacity: usize,
    path: Option<PathBuf>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(512)
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> EventBus {
        let (sender, _) = broadcast::channel(capacity);
        EventBus {
            next_sequence: AtomicU64::new(1),
            sender,
            history: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity,
            path: None,
        }
    }

    pub fn open(path: impl Into<PathBuf>, capacity: usize) -> anyhow::Result<EventBus> {
        let path = path.into();
        let document =
            crate::storage::load_versioned::<EventHistory, _>(&path, SCHEMA_VERSION, migrate)?
                .unwrap_or_default();
        let mut events: VecDeque<_> = document.events.into();
        while events.len() > capacity {
            events.pop_front();
        }
        let next = events.back().map_or(1, |event| event.sequence + 1);
        let (sender, _) = broadcast::channel(capacity);
        Ok(EventBus {
            next_sequence: AtomicU64::new(next),
            sender,
            history: RwLock::new(events),
            capacity,
            path: Some(path),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.sender.subscribe()
    }

    pub fn history(
        &self,
        after: u64,
        topic: Option<EventTopic>,
        resource_id: Option<&str>,
        limit: usize,
    ) -> Vec<AppEvent> {
        let limit = limit.clamp(1, self.capacity);
        let matched: Vec<_> = self
            .history
            .read()
            .iter()
            .filter(|event| event.sequence > after)
            .filter(|event| topic.is_none_or(|value| event.topic == value))
            .filter(|event| {
                resource_id.is_none_or(|value| event.resource_id.as_deref() == Some(value))
            })
            .cloned()
            .collect();
        let start = matched.len().saturating_sub(limit);
        matched[start..].to_vec()
    }

    pub fn publish(
        &self,
        topic: EventTopic,
        action: impl Into<String>,
        resource_id: Option<String>,
        data: serde_json::Value,
    ) -> AppEvent {
        let event = AppEvent {
            sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
            emitted_at: chrono::Utc::now(),
            topic,
            action: action.into(),
            resource_id,
            data,
        };
        {
            let mut history = self.history.write();
            history.push_back(event.clone());
            while history.len() > self.capacity {
                history.pop_front();
            }
            if let Some(path) = &self.path {
                let document = EventHistory {
                    events: history.iter().cloned().collect(),
                };
                if let Err(error) = crate::storage::save_versioned(path, SCHEMA_VERSION, &document)
                {
                    tracing::warn!("failed to persist runtime events: {error}");
                }
            }
        }
        let _ = self.sender.send(event.clone());
        event
    }
}

#[derive(Default, Serialize, Deserialize)]
struct EventHistory {
    events: Vec<AppEvent>,
}

fn migrate(version: u32, value: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    match version {
        0 => Ok(value),
        _ => anyhow::bail!("no runtime event migration from schema version {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_ordered_events_to_subscribers() {
        let bus = EventBus::new(4);
        let mut subscriber = bus.subscribe();

        let first = bus.publish(
            EventTopic::Engine,
            "loading",
            Some("model-a".into()),
            serde_json::json!({}),
        );
        let second = bus.publish(
            EventTopic::Engine,
            "ready",
            Some("model-a".into()),
            serde_json::json!({}),
        );

        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert_eq!(subscriber.recv().await.unwrap(), first);
        assert_eq!(subscriber.recv().await.unwrap(), second);
    }

    #[test]
    fn history_returns_recent_events_and_survives_restart() {
        let root = std::env::temp_dir().join(format!("kiralm-events-{}", uuid::Uuid::new_v4()));
        let path = root.join("events.json");
        let bus = EventBus::open(&path, 8).unwrap();
        for index in 0..5 {
            bus.publish(
                EventTopic::Agent,
                format!("step_{index}"),
                Some("chat".to_string()),
                serde_json::json!({}),
            );
        }
        let recent = bus.history(0, Some(EventTopic::Agent), Some("chat"), 2);
        assert_eq!(recent[0].action, "step_3");
        assert_eq!(recent[1].action, "step_4");
        drop(bus);
        let reopened = EventBus::open(&path, 8).unwrap();
        assert_eq!(reopened.history(0, None, None, 8).len(), 5);
        let _ = std::fs::remove_dir_all(root);
    }
}

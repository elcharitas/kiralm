use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTopic {
    Engine,
    Cache,
    Approval,
    Generation,
    Knowledge,
    Attachment,
    Evaluation,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
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
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.sender.subscribe()
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
        let _ = self.sender.send(event.clone());
        event
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
}

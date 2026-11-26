use std::{fmt, sync::Arc};

use tracing::{
    Level, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context};

use super::{Event, Operations};

pub struct Capture(pub(crate) Arc<Operations>);

#[derive(Default)]
struct Message(Option<String>);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = Some(crate::engine::sanitize(&format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = Some(crate::engine::sanitize(value));
        }
    }
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        if !metadata.target().starts_with("restreamer_core") {
            return;
        }
        let mut message = Message::default();
        event.record(&mut message);
        let Some(message) = message.0 else {
            return;
        };
        let level = match *metadata.level() {
            Level::ERROR => "error",
            Level::WARN => "warn",
            Level::INFO => "info",
            Level::DEBUG => "debug",
            Level::TRACE => "trace",
        };
        self.0.publish(Event {
            ts: time::OffsetDateTime::now_utc()
                .unix_timestamp_nanos()
                .div_euclid(1_000_000) as i64,
            category: "application",
            action: "log",
            name: String::new(),
            domain: String::new(),
            message: Some(message),
            level,
        });
    }
}

#![allow(unused)]
use ansi_term::Color::Blue;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::message::Log;

struct StateReader(HashMap<String, ProgressMessage>);

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone)]
struct ProgressMessage {
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
    message: String,
}

impl Log for ProgressMessage {
    fn timestamp(&self) -> DateTime<Utc> {
        self.start
    }
    fn text(&self) -> String {
        self.message.to_string()
    }
    fn style(&self) -> ansi_term::Style {
        Blue.into()
    }
}

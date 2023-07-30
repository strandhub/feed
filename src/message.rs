#![allow(unused)]
use ansi_term::Color::{Black, Blue, Green, Red, Yellow};
use ansi_term::Style;
use chrono::{DateTime, Utc};
use chrono_tz::{Tz, CET};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs::OpenOptions;
use std::io::Write;
use std::str::FromStr;

use crate::reader::log_path;

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct Message {
    pub timestamp: DateTime<Utc>,
    pub status: Status,
    pub message: String,
}

impl Message {
    pub fn new(status: Status, message: &str) -> Self {
        Self {
            timestamp: Utc::now(),
            status,
            message: message.into(),
        }
    }
}

impl Log for Message {
    fn timestamp(&self) -> DateTime<Utc> {
        self.timestamp
    }
    fn text(&self) -> String {
        self.message.to_string()
    }
    fn style(&self) -> Style {
        match self.status {
            Status::Error => Red.into(),
            Status::Success => Green.into(),
            Status::Pending => Style::new().fg(Black).on(Yellow),
        }
    }
}

pub trait Log {
    fn timestamp(&self) -> DateTime<Utc>;
    fn text(&self) -> String;
    fn style(&self) -> Style;
    fn plain(&self) -> String {
        let t = self
            .timestamp()
            .with_timezone(&CET)
            .format("%Y-%m-%d %H:%M:%S");
        format!("[{}] {}", t, self.text())
    }
    fn styled(&self) -> String {
        format!("{}", self.style().paint(self.plain()))
    }
    fn blink(&self) -> String {
        let style = Style::new().fg(Black).on(Yellow);
        format!("{}", style.paint(self.plain()))
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Debug)]
pub enum Status {
    Error,
    Success,
    Pending,
}

impl FromStr for Status {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "success" => Ok(Self::Success),
            "pending" => Ok(Self::Pending),
            _ => Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }
}

impl PartialOrd for Message {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.timestamp.partial_cmp(&other.timestamp)
    }
}
impl Ord for Message {
    fn cmp(&self, other: &Self) -> Ordering {
        self.timestamp.cmp(&other.timestamp).reverse()
    }
}

enum Progress {
    // Find status in separate file. Just ID in the feed
    Progressing,
    Done,
}

pub fn append(message: Message) {
    let mut file = OpenOptions::new().append(true).open(log_path()).unwrap();
    let msg = serde_json::to_string(&message).unwrap();
    writeln!(file, "{}", msg);
}

use chrono::{DateTime, Utc};

use crate::message::{Log, Message};
use std::io::{self, Write};
use std::{collections::BinaryHeap, fs, thread, time::Duration};

const REFRESH_RATE: u64 = 100;
const COLOR_PERIOD: usize = 1000 * 60 * 5;
const BLINK_MILLIS: u64 = 1500;
const N_LINES: usize = 10;
pub const PATH: &str = "./feed.log";

#[derive(Default)]
pub struct ReaderOpts {
    n: Option<usize>,
    blink_millis: Option<u64>,
}

impl ReaderOpts {
    pub fn with_lines(&mut self, n: usize) {
        self.n = Some(n)
    }
    pub fn with_blink_millis(&mut self, blink_millis: u64) {
        self.blink_millis = Some(blink_millis)
    }
    pub fn build(self) -> Reader {
        Reader {
            print_stack: None,
            n: self.n.unwrap_or(N_LINES),
            blink_millis: self.blink_millis.unwrap_or(BLINK_MILLIS),
        }
    }
}

pub struct Reader {
    print_stack: Option<Vec<String>>,
    n: usize,
    blink_millis: u64,
}

impl Reader {
    pub fn parse(&mut self, s: String, history: &mut BinaryHeap<Message>) {
        let mut lines = s.lines().rev().peekable();
        let mut print_stack = Vec::new();

        while let (Some(msg), true) = (validate(lines.next()), print_stack.len() < self.n) {
            history.push(msg.clone());
            print_stack.push(self.format(msg));
        }

        if self.print_stack == Some(print_stack.clone()) {
            return;
        }

        self.print_stack = Some(print_stack.clone());
        clear();
        while let Some(msg) = print_stack.pop() {
            writeln!(io::stdout(), "{}", msg).unwrap();
        }
    }
    pub fn new(n: usize, blink_millis: u64) -> Self {
        Self {
            n,
            print_stack: None,
            blink_millis,
        }
    }
    pub fn listen(&mut self) {
        let mut history: BinaryHeap<Message> = BinaryHeap::new();
        loop {
            let s = fs::read_to_string(PATH).unwrap();
            self.parse(s, &mut history);
            thread::sleep(Duration::from_millis(REFRESH_RATE));
        }
    }
    fn format(&self, msg: Message) -> String {
        use Age::*;
        match age(msg.timestamp, self.blink_millis) {
            Recent => msg.blink(),
            Old => match age(msg.timestamp, COLOR_PERIOD as u64) {
                Recent => msg.styled(),
                Old => msg.plain(),
            },
        }
    }
}

fn validate(s: Option<&str>) -> Option<Message> {
    match s {
        Some(res) => serde_json::from_str(&res).unwrap(),
        None => None,
    }
}

pub fn age(ts: DateTime<Utc>, tol_millis: u64) -> Age {
    match Utc::now() > ts + chrono::Duration::milliseconds(tol_millis as i64) {
        true => Age::Old,
        false => Age::Recent,
    }
}

pub fn clear() {
    print!("{esc}[2J{esc}[1;1H", esc = 27 as char);
}

pub enum Age {
    Recent,
    Old,
}

#![allow(unused)]
use clap::Command;
use feed::message::{append, Message, Status};
use feed::reader::Reader;
use serde::Serialize;

fn main() {
    println!("Hi mom");
    let s = r#"refresh_rate_ms=123
    "#;

    // let serializer = serde::Serializer::serialize_str(s);
    // s.serialize().unwrap()
    // let conf = serde::Serialize::serialize
}

#[derive(Serialize)]
struct Config {
    refresh_rate_ms: u64,
}

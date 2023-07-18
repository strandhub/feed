use std::io::{self, BufRead};

use clap::{Parser, Subcommand};
use feed::{
    message::{append, Message, Status},
    reader::ReaderOpts,
};

fn main() {
    cli()
}

pub fn cli() {
    let args = Arguments::parse();

    match args.command {
        Cmd::Write { msg, status } => {
            let mut stdin = String::new();
            io::stdin().read_line(&mut stdin).unwrap();
            // Hangs if no STDIN!
            // println!("{:?}, {}", stdin, stdin.len());

            let message = match stdin.len() > 0 {
                true => Message::new(Status::Success, &stdin),
                false => Message::new(status, &msg),
            };

            append(message);
        }
        Cmd::Listen {
            lines,
            blink_millis,
        } => {
            let mut opts = ReaderOpts::default();

            if let Some(v) = lines {
                opts.with_lines(v);
            }

            if let Some(v) = blink_millis {
                opts.with_blink_millis(v);
            }

            opts.build().listen();
        }
        Cmd::Read => {
            let mut buf = String::new();
            match io::stdin().read_line(&mut buf) {
                Ok(n) => {
                    println!("{n} bytes read");
                    println!("{buf}");
                }
                Err(error) => println!("error: {error}"),
            }
        }
    };
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Arguments {
    #[clap(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Write {
        msg: String,
        #[arg(short, long)]
        status: Status,
    },
    Listen {
        /// Number of lines to show (DEFAULT: 10)
        #[arg(short = 'n', long)]
        lines: Option<usize>,
        /// How long to blink (DEFAULT: 1500)
        #[arg(long)]
        blink_millis: Option<u64>,
    },
    Read,
}

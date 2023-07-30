use clap::{Parser, Subcommand};
use feed::{
    message::{append, Message, Status},
    reader::ReaderOpts,
};
use std::{fs::File, io, path::PathBuf};

fn main() {
    cli()
}

pub fn cli() {
    let config_base = std::env::var("XDG_CONFIG_HOME").unwrap_or("~/.config".to_string());
    let config_path = vec![config_base, env!("CARGO_PKG_NAME").to_string()].join("/");
    let path = PathBuf::from(config_path);
    let config_file = match path.is_file() {
        true => {
            println!("TRUE");
            File::open(path)
        }
        false => {
            println!("Creating file at {path:?}");
            File::create(path)
        }
    };
    println!("{:?}", config_file);
    // let config = Config::new();
    let args = Arguments::parse();

    match args.command {
        Cmd::Write {
            msg,
            mut status,
            error,
            success,
        } => {
            if error {
                status = Some(Status::Error)
            }
            if success {
                status = Some(Status::Success)
            }
            let status = status.unwrap();
            let message = match msg {
                Some(msg) => Message::new(status, &msg),
                None => {
                    let mut stdin = String::new();
                    io::stdin().read_line(&mut stdin).unwrap();
                    // Hangs if no STDIN!
                    // println!("{:?}, {}", stdin, stdin.len());

                    match stdin.len() > 0 {
                        true => Message::new(Status::Success, &stdin),
                        false => return eprintln!("No stdin"),
                    }
                }
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
        msg: Option<String>,
        #[arg(long)]
        error: bool,
        #[arg(long)]
        success: bool,
        #[arg(short, long)]
        status: Option<Status>,
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

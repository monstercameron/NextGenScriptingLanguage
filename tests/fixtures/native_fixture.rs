//! Owned executable fixtures for native Windows/Unix process contract tests.
use std::io::{Read, Write};
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("") {
        "argv" => serde_json::to_writer(std::io::stdout(), &args[1..]).unwrap(),
        "write" => {
            std::io::stdout().write_all(args[1].as_bytes()).unwrap();
        }
        "copy" => {
            std::io::copy(&mut std::io::stdin(), &mut std::io::stdout()).unwrap();
        }
        "upper" => {
            let mut data = Vec::new();
            std::io::stdin()
                .take(16 * 1024 * 1024)
                .read_to_end(&mut data)
                .unwrap();
            data.make_ascii_uppercase();
            std::io::stdout().write_all(&data).unwrap();
        }
        "exit" => {
            if let Some(data) = args.get(2) {
                print!("{data}");
                std::io::stdout().flush().unwrap();
            }
            eprintln!("fixture failed");
            std::process::exit(args[1].parse().unwrap());
        }
        "produce" => {
            for text in ["one\n", "two\n", "three\n"] {
                print!("{text}");
                std::io::stdout().flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(35));
            }
        }
        "sleep" => std::thread::sleep(std::time::Duration::from_millis(
            args[1].parse::<u64>().unwrap().min(10000),
        )),
        "late-write" => {
            std::thread::sleep(std::time::Duration::from_millis(700));
            std::fs::write(&args[1], b"escaped").unwrap();
        }
        "spawn" => {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["late-write", &args[1]])
                .spawn()
                .unwrap();
            println!("started");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            child.wait().unwrap();
        }
        "increment" => {
            let p = std::path::Path::new(&args[1]);
            let n = std::fs::read_to_string(p)
                .unwrap_or_else(|_| "0".into())
                .parse::<u32>()
                .unwrap()
                + 1;
            std::fs::write(p, n.to_string()).unwrap();
            print!("{n}");
        }
        "stderr-flood" => {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let bytes = [b'e'; 8192];
            let mut sink = std::io::stderr().lock();
            while std::time::Instant::now() < until {
                if sink.write_all(&bytes).is_err() {
                    break;
                }
            }
        }
        "flood" => {
            let bytes = [b'x'; 32768];
            for _ in 0..128 {
                if std::io::stdout().write_all(&bytes).is_err() {
                    break;
                }
            }
        }
        _ => std::process::exit(64),
    }
}

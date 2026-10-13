//! `quorum` — interactive SQL client.

use quorum_proto::{request, ClientRequest, ClientResponse, QueryResult};
use std::io::{BufRead, Write};
use std::net::TcpStream;
use std::process::exit;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn usage() -> ! {
    eprintln!(
        "quorum {}\n\
         Interactive client for the quorum SQL database.\n\n\
         USAGE:\n    quorum [OPTIONS]\n\n\
         OPTIONS:\n    \
         --addr <HOST:PORT>    server address (default 127.0.0.1:7300)\n    \
         -e, --execute <SQL>   run one script and exit\n    \
         -h, --help            print help\n\n\
         In the REPL, terminate statements with ';' and type \\q to quit.",
        VERSION
    );
    exit(0);
}

fn main() {
    let mut addr = "127.0.0.1:7300".to_string();
    let mut one_shot: Option<String> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => usage(),
            "--addr" => {
                i += 1;
                addr = args.get(i).cloned().unwrap_or_else(|| usage());
            }
            "-e" | "--execute" => {
                i += 1;
                one_shot = Some(args.get(i).cloned().unwrap_or_else(|| usage()));
            }
            other => {
                eprintln!("unknown argument: {}", other);
                usage();
            }
        }
        i += 1;
    }

    let mut stream = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot connect to {}: {}", addr, e);
            exit(1);
        }
    };

    if let Some(sql) = one_shot {
        execute(&mut stream, &sql);
        return;
    }

    println!("quorum {} — connected to {}", VERSION, addr);
    println!("terminate statements with ';', type \\q to quit");
    repl(&mut stream);
}

fn repl(stream: &mut TcpStream) {
    let stdin = std::io::stdin();
    let mut buffer = String::new();
    prompt(buffer.is_empty());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            prompt(buffer.is_empty());
            continue;
        }
        if buffer.is_empty() && (trimmed == "\\q" || trimmed.eq_ignore_ascii_case("quit") || trimmed.eq_ignore_ascii_case("exit")) {
            break;
        }
        if trimmed.starts_with('\\') && buffer.is_empty() {
            println!("unknown command: {} (try \\q)", trimmed);
            prompt(true);
            continue;
        }

        buffer.push_str(&line);
        buffer.push('\n');
        if trimmed.ends_with(';') {
            execute(stream, &buffer);
            buffer.clear();
        }
        prompt(buffer.is_empty());
    }
}

fn prompt(fresh: bool) {
    if fresh {
        print!("quorum> ");
    } else {
        print!("    ...> ");
    }
    let _ = std::io::stdout().flush();
}

fn execute(stream: &mut TcpStream, sql: &str) {
    match request(
        stream,
        &ClientRequest::Execute {
            sql: sql.to_string(),
        },
    ) {
        Ok(ClientResponse::Results { results }) => print_results(&results),
        Ok(ClientResponse::Error { message }) => println!("error: {}", message),
        Ok(ClientResponse::Pong) => println!("pong"),
        Err(e) => println!("connection error: {}", e),
    }
}

fn print_results(results: &[QueryResult]) {
    for qr in results {
        if qr.columns.is_empty() {
            if !qr.message.is_empty() {
                println!("{}", qr.message);
            }
            continue;
        }
        print_table(qr);
    }
}

fn print_table(qr: &QueryResult) {
    let cells: Vec<Vec<String>> = qr
        .rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_string()).collect())
        .collect();

    let mut widths: Vec<usize> = qr.columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }

    let border = |widths: &[usize]| {
        let mut s = String::from("+");
        for w in widths {
            s.push_str(&"-".repeat(w + 2));
            s.push('+');
        }
        s
    };

    println!("{}", border(&widths));
    let mut header = String::from("|");
    for (i, c) in qr.columns.iter().enumerate() {
        header.push_str(&format!(" {:width$} |", c, width = widths[i]));
    }
    println!("{}", header);
    println!("{}", border(&widths));
    for row in &cells {
        let mut line = String::from("|");
        for (i, c) in row.iter().enumerate() {
            line.push_str(&format!(" {:width$} |", c, width = widths[i]));
        }
        println!("{}", line);
    }
    println!("{}", border(&widths));
    println!("({} row{})", cells.len(), if cells.len() == 1 { "" } else { "s" });
}

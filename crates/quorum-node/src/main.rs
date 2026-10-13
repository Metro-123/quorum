//! `quorumd` — the quorum database server.

use quorum_node::{start, NodeConfig};
use std::path::PathBuf;
use std::process::exit;

fn usage() -> ! {
    eprintln!(
        "quorumd {}\n\
         A distributed SQL database (single-node server).\n\n\
         USAGE:\n    quorumd [OPTIONS]\n\n\
         OPTIONS:\n    \
         --addr <HOST:PORT>    listen address (default 127.0.0.1:7300)\n    \
         --data <DIR>          data directory (omit for in-memory)\n    \
         --node-id <N>         node id (reserved for clusters)\n    \
         --peers <LIST>        id@host:port,... (reserved for clusters)\n    \
         -h, --help            print help",
        env!("CARGO_PKG_VERSION")
    );
    exit(0);
}

fn main() {
    let mut config = NodeConfig::default();
    let mut data: Option<PathBuf> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => usage(),
            "--addr" => {
                i += 1;
                config.listen = args.get(i).cloned().unwrap_or_else(|| usage());
            }
            "--data" => {
                i += 1;
                data = Some(PathBuf::from(args.get(i).cloned().unwrap_or_else(|| usage())));
            }
            "--node-id" => {
                i += 1; // reserved for clusters
            }
            "--peers" => {
                i += 1; // reserved for clusters
            }
            other => {
                eprintln!("unknown argument: {}", other);
                usage();
            }
        }
        i += 1;
    }

    let data_dir = data.clone();
    config.data_dir = data;

    match start(config) {
        Ok(handle) => {
            println!("quorumd listening on {}", handle.addr());
            match &data_dir {
                Some(dir) => println!("data dir: {}", dir.display()),
                None => println!("storage: in-memory (data will not survive restart)"),
            }
            println!("press Enter to shut down");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            handle.shutdown();
        }
        Err(e) => {
            eprintln!("failed to start quorumd: {}", e);
            exit(1);
        }
    }
}

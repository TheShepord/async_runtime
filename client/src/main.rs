use async_runtime::{runtime, services};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use std::env;
use std::process::exit;
use std::sync::mpsc;
use std::time::Instant;

fn main() {
    const MAX_CHANNELS: usize = 1000;

    const USAGE: &str = "Usage: cargo run -- <number_of_clients>";

    // Get client count from stdin.
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        println!("{}", USAGE);
        exit(1);
    }
    let client_count = match args[1].parse::<usize>() {
        Ok(n) => n,
        Err(_) => {
            println!("{}", USAGE);
            exit(1)
        }
    };

    let (sender, receiver) = mpsc::sync_channel(MAX_CHANNELS);

    let reactor = runtime::Reactor::new(sender.clone());
    let mut executor = runtime::Executor::new(receiver);

    for i in 0..client_count {
        let registrator = reactor.registrator();

        // Create future client that makes server requests.
        let future = async move {
            let addr = format!("127.0.0.1:{}", 3000 + i);
            let request = format!("hello from client {}\n", i);

            let start = Instant::now();

            // Connect to server.
            let mut stream = services::TcpStream::connect(addr, registrator, i)
                .await
                .unwrap_or_else(|e| {
                    println!("{}: failed to connect to server with error {}", i, e);
                    exit(1)
                });
            let connected = start.elapsed();

            // Send message to server.
            stream
                .write_all(request.as_bytes())
                .await
                .unwrap_or_else(|e| {
                    println!("{}: failed to write to server with error {}", i, e);
                    exit(1)
                });
            let wrote = start.elapsed();

            // Read message from server.
            let mut buf = String::new();
            stream.read_to_string(&mut buf).await.unwrap_or_else(|e| {
                println!("{}: failed to read from server with error {}", i, e);
                exit(1)
            });
            let read = start.elapsed();

            println!(
                "{}: connected at {:?}, sent at {:?}, received at {:?}",
                i, connected, wrote, read
            );
            println!("{}: {}", i, buf);

            stream.close().await.unwrap();
        };
        let task = runtime::Task::new(future, sender.clone(), i);
        // Pend task for later execution.
        executor.suspend(task);
    }
    // Run tasks pended for execution.
    executor.run();
}

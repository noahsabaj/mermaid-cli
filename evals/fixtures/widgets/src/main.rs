mod config;

fn main() {
    let addr = format!("127.0.0.1:{}", config::port());
    let listener = std::net::TcpListener::bind(&addr).expect("bind");
    println!("widgets listening on {addr}");
    for stream in listener.incoming().flatten() {
        drop(stream);
    }
}

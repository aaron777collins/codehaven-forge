//! Serves every page from the demo data on one port, for looking at the UI in a browser.
//!
//!   cargo run -p ch-web --example demo            (binds 127.0.0.1:3291)
//!   ssh -L 3291:127.0.0.1:3291 <build-host>       (from your machine, then open http://127.0.0.1:3291)
//!
//! The demo has no real sign-in. Open /login, username `ada`, password `correct horse battery
//! staple`. It binds loopback only.

#[tokio::main]
async fn main() {
    let addr = std::env::var("DEMO_BIND").unwrap_or_else(|_| "127.0.0.1:3291".into());
    if !addr.starts_with("127.0.0.1:") && !addr.starts_with("[::1]:") {
        eprintln!("the demo only binds loopback");
        std::process::exit(2);
    }
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("demo on http://{addr}  (user ada, password {})", ch_web::demo::DEMO_PASSWORD);
    let app = ch_web::demo::router().into_make_service_with_connect_info::<std::net::SocketAddr>();
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("server stopped: {e}");
    }
}

//! `haven`. See `lib.rs`.

#[tokio::main]
async fn main() {
    let env = match haven_cli::Env::from_process() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("haven: {e:#}");
            std::process::exit(1);
        }
    };
    let code = haven_cli::run(std::env::args().collect(), env).await;
    std::process::exit(code);
}

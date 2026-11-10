//! RigInfinity daemon binary — serves durable agent sessions on the
//! local machine (Unix socket + RAP callbacks, loopback only).

use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(false)
        .init();
    if let Err(e) = infinity_daemon::run_daemon(true).await {
        eprintln!("riginfinity-daemon failed: {e}");
        std::process::exit(1);
    }
}

//! `valkvork-controller --config /etc/valkvork/controller.toml`

use clap::Parser;
use std::path::PathBuf;
use valkvork_common::config::ControllerConfig;

#[derive(Parser)]
#[command(
    name = "valkvork-controller",
    version,
    about = "Valkvork control plane"
)]
struct Args {
    /// Path to controller.toml
    #[arg(
        long,
        env = "VALKVORK_CONTROLLER_CONFIG",
        default_value = "/etc/valkvork/controller.toml"
    )]
    config: PathBuf,
    /// Validate the configuration (including token files) and exit
    #[arg(long)]
    check_config: bool,
}

#[tokio::main]
async fn main() {
    valkvork_common::init_tracing();
    let args = Args::parse();
    let result = match ControllerConfig::load(&args.config) {
        Ok(cfg) if args.check_config => check(&cfg),
        Ok(cfg) => valkvork_controller::server::run(cfg).await,
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        tracing::error!(error = %e, "controller exiting");
        eprintln!("valkvork-controller: {e}");
        std::process::exit(1);
    }
}

fn check(cfg: &ControllerConfig) -> Result<(), valkvork_common::ValkvorkError> {
    cfg.resolve_join_token()?;
    cfg.resolve_admin_token()?;
    if let (Some(cert), Some(key)) = (&cfg.tls_cert_file, &cfg.tls_key_file) {
        valkvork_common::tls::server_tls_config(cert, key)?;
    }
    println!("configuration OK");
    Ok(())
}

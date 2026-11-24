//! `redex-backend`: the account service. Configuration from the environment (see config.rs);
//! migrations run at start; SIGTERM or Ctrl+C stop it gracefully.
use redex_backend::{App, MIGRATOR, config::Config, prune, router};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("REDEX_LOG").unwrap_or_else(|_| "info,sqlx=warn".into());
    let json = std::env::var("REDEX_LOG_FORMAT").is_ok_and(|f| f == "json");
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if json { builder.json().init() } else { builder.init() }

    let cfg = Config::from_env().unwrap_or_else(|e| {
        tracing::error!("config: {e}");
        std::process::exit(2);
    });
    tracing::info!(?cfg, "starting");
    // Postgres may come up after us (compose): retry for a minute
    let mut tries = 0;
    let db = loop {
        match sqlx::postgres::PgPoolOptions::new().max_connections(20).acquire_timeout(Duration::from_secs(5)).connect(&cfg.database_url).await {
            Ok(db) => break db,
            Err(e) if tries < 30 => {
                tries += 1;
                tracing::warn!(error = %e, "database not reachable yet; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "database not reachable");
                std::process::exit(1);
            }
        }
    };
    if let Err(e) = MIGRATOR.run(&db).await {
        tracing::error!(error = %e, "migrations failed");
        std::process::exit(1);
    }
    let bind = cfg.bind;
    let app = App::new(db.clone(), cfg).unwrap_or_else(|e| {
        tracing::error!("config: {e}");
        std::process::exit(2);
    });
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(600));
        loop {
            every.tick().await;
            if let Err(e) = prune(&db).await {
                tracing::warn!(error = %e, "prune");
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(bind).await.unwrap_or_else(|e| {
        tracing::error!(error = %e, %bind, "cannot listen");
        std::process::exit(1);
    });
    tracing::info!(%bind, "listening");
    let served = axum::serve(listener, router(app).into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(stopped()).await;
    if let Err(e) = served {
        tracing::error!(error = %e, "server");
        std::process::exit(1);
    }
    tracing::info!("stopped");
}

/// Ctrl+C, or SIGTERM (docker stop) on Unix.
async fn stopped() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}

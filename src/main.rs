use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use clex_mqtt::{
    INSTANCE_LOCK, auth,
    broker::{Broker, BrokerConfig},
    config::Config,
    migrations,
    store::Store,
    web::{self, WebState},
};
use sqlx::{
    ConnectOptions, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    io::{self, BufRead, Read},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "clex-mqtt",
    version,
    about = "MQTT broker and management console"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Start listeners. Does not change the database schema.
    Serve,
    /// Apply embedded database migrations while the broker is stopped.
    Migrate,
    /// Upgrade account MQTT names while the broker is stopped.
    Accounts {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Manage panel administrators.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}
#[derive(Subcommand)]
enum AccountCommand {
    /// Reset legacy account MQTT state and switch to readable names.
    UpgradeNames {
        #[arg(long, required = true)]
        reset_mqtt_state: bool,
    },
}

#[derive(Subcommand)]
enum AdminCommand {
    Create {
        #[arg(long)]
        username: String,
        #[arg(long, required = true)]
        password_stdin: bool,
    },
}

fn main() {
    // Load dotenv before starting any worker threads.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("clex_mqtt=info,tower_http=warn")),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime");
    if let Err(error) = runtime.block_on(run(cli)) {
        tracing::error!(error=%error,"CLEX MQTT stopped");
        std::process::exit(1);
    }
}

async fn pool(config: &Config) -> Result<PgPool> {
    let options: PgConnectOptions = config
        .database_url
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid DATABASE_URL"))?;
    PgPoolOptions::new()
        .max_connections(config.database_connections)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET synchronous_commit = on")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options.disable_statement_logging())
        .await
        .context("Could not connect to PostgreSQL")
}

async fn run(cli: Cli) -> Result<()> {
    let config = Arc::new(Config::from_env()?);
    let pool = pool(&config).await?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Migrate => {
            let mut lock = pool.acquire().await?;
            let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(INSTANCE_LOCK)
                .fetch_one(&mut *lock)
                .await?;
            ensure!(
                acquired,
                "Stop the broker before applying migrations to its database"
            );
            migrations::apply(&pool).await?;
            tracing::info!("Database migrations applied");
            sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(INSTANCE_LOCK)
                .execute(&mut *lock)
                .await?;
        }
        Command::Admin {
            command:
                AdminCommand::Create {
                    username,
                    password_stdin: _,
                },
        } => {
            ensure!(
                !username.is_empty()
                    && username.len() <= 128
                    && username
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_.@-".contains(&c)),
                "Administrator username contains unsupported characters"
            );
            let mut password = String::new();
            io::stdin().lock().take(1024).read_line(&mut password)?;
            let password = password.trim_end_matches(['\r', '\n']);
            auth::validate_password(password)?;
            let hash = auth::hash_password(password)?;
            let result = sqlx::query("INSERT INTO admin_users (id,username,password_hash) VALUES ($1,$2,$3) ON CONFLICT (username) DO NOTHING").bind(Uuid::new_v4()).bind(&username).bind(hash).execute(&pool).await?;
            ensure!(
                result.rows_affected() == 1,
                "Administrator already exists; its password was left unchanged"
            );
            tracing::info!("Panel administrator created");
        }
        Command::Accounts {
            command:
                AccountCommand::UpgradeNames {
                    reset_mqtt_state: _,
                },
        } => {
            let report = clex_mqtt::accounts::upgrade_names(&pool).await?;
            tracing::info!(
                accounts = report.accounts,
                mqtt_users = report.mqtt_users,
                sessions = report.sessions,
                messages = report.messages,
                rules = report.rules,
                subscriptions = report.subscriptions,
                "Account MQTT names upgraded"
            );
        }
        Command::Serve => serve(config, pool).await?,
    }
    Ok(())
}

async fn serve(config: Arc<Config>, pool: PgPool) -> Result<()> {
    ensure!(
        config.database_connections >= 2,
        "Serving requires at least two database connections"
    );
    let mut instance_lock = pool.acquire().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(INSTANCE_LOCK)
        .fetch_one(&mut *instance_lock)
        .await?;
    ensure!(
        acquired,
        "Another CLEX MQTT instance already owns this database"
    );
    let store = Store::from_pool(pool.clone())
        .with_queue_limits(config.max_pending as i64, config.max_pending_bytes as i64);
    store.check_durability().await?;
    clex_mqtt::accounts::require_readable_names(&pool).await?;
    let tcp = TcpListener::bind(config.mqtt_addr)
        .await
        .context("Could not bind MQTT listener")?;
    let http = TcpListener::bind(config.http_addr)
        .await
        .context("Could not bind administration listener")?;
    let tls = match config.tls_addr {
        Some(address) => Some((
            TcpListener::bind(address)
                .await
                .context("Could not bind MQTT TLS listener")?,
            clex_mqtt::tls::acceptor(
                config
                    .tls_cert
                    .as_deref()
                    .context("TLS certificate missing")?,
                config.tls_key.as_deref().context("TLS key missing")?,
                config.tls_client_auth,
                config.tls_client_ca.as_deref(),
            )?,
        )),
        None => None,
    };
    let broker = Broker::new(
        store.clone(),
        BrokerConfig {
            max_packet_size: config.max_packet_size,
            max_connections: config.max_connections,
            max_inflight: config.max_inflight,
            max_pending: config.max_pending,
            max_pending_bytes: config.max_pending_bytes,
            shutdown_timeout: config.shutdown_timeout,
            ..Default::default()
        },
    )
    .await?;
    let stop = CancellationToken::new();
    let mut tasks = JoinSet::new();
    tasks.spawn(clex_mqtt::bridge::run(
        store,
        broker.clone(),
        config.bridge_key.clone(),
        config.max_packet_size,
        stop.clone(),
    ));
    tasks.spawn(tcp_loop(tcp, broker.clone(), stop.clone()));
    if let Some((listener, acceptor)) = tls {
        tasks.spawn(tls_loop(listener, acceptor, broker.clone(), stop.clone()));
    }
    let app = web::router(WebState::new(pool.clone(), broker.clone(), config.clone())?);
    let http_stop = stop.clone();
    tasks.spawn(async move {
        axum::serve(
            http,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(http_stop.cancelled_owned())
        .await
        .context("Administration server failed")
    });
    tracing::info!(mqtt=%config.mqtt_addr,http=%config.http_addr,environment=%config.environment,"CLEX MQTT is ready");
    let result = tokio::select! {
        _ = shutdown_signal() => Ok(()),
        result = tasks.join_next() => match result { Some(Ok(result))=>result, Some(Err(error))=>Err(error.into()), None=>Ok(()) },
        result = async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                sqlx::query("SELECT 1").execute(&mut *instance_lock).await.context("Database instance lock connection was lost; broker ownership is no longer guaranteed")?;
            }
            #[allow(unreachable_code)]
            Ok::<(),anyhow::Error>(())
        } => result,
    };
    stop.cancel();
    let shutdown_result = tokio::time::timeout(config.shutdown_timeout, async {
        broker.shutdown().await?;
        while let Some(task) = tasks.join_next().await {
            task??;
        }
        // Keep the ownership connection until all other database connections
        // are closed. Detaching it prevents pool.close() waiting on itself.
        let ownership = instance_lock.detach();
        pool.close().await;
        sqlx::Connection::close(ownership).await?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("Broker shutdown exceeded its deadline")
    .and_then(|result| result);
    // On a deadline/error, propagate immediately to main's failed process exit.
    // That closes every socket while retaining ownership through graceful work.
    shutdown_result?;
    tracing::info!("CLEX MQTT stopped");
    result
}

async fn tcp_loop(listener: TcpListener, broker: Broker, stop: CancellationToken) -> Result<()> {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            accepted = listener.accept() => {
                let (stream,peer) = accepted?;
                stream.set_nodelay(true)?;
                let broker = broker.clone();
                tasks.spawn(async move { let _ = broker.run_io(stream,peer).await; });
            },
            _ = tasks.join_next(), if !tasks.is_empty() => (),
        }
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

async fn tls_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    broker: Broker,
    stop: CancellationToken,
) -> Result<()> {
    let handshakes = Arc::new(Semaphore::new(64));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            accepted = listener.accept() => {
                let (stream,peer) = accepted?;
                let Ok(permit) = handshakes.clone().try_acquire_owned() else { continue; };
                let acceptor = acceptor.clone();
                let broker = broker.clone();
                tasks.spawn(async move {
                    let accepted = tokio::time::timeout(Duration::from_secs(10),acceptor.accept(stream)).await;
                    drop(permit);
                    if let Ok(Ok(stream))=accepted
                        && let Ok(certificate)=clex_mqtt::tls::verified_client(stream.get_ref().1) {
                        let _=broker.run_io_with_certificate(stream,peer,certificate).await;
                    }
                });
            },
            _ = tasks.join_next(), if !tasks.is_empty() => (),
        }
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _=tokio::signal::ctrl_c()=>(), _=terminate.recv()=>() }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

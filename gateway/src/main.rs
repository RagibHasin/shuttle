use clap::Parser;
use fqdn::FQDN;
use futures::prelude::*;
use instant_acme::{AccountCredentials, ChallengeType};
use shuttle_common::backends::tracing::setup_tracing;
use shuttle_gateway::acme::{AcmeClient, CustomDomain};
use shuttle_gateway::api::latest::{ApiBuilder, SVC_DEGRADED_THRESHOLD};
use shuttle_gateway::args::StartArgs;
use shuttle_gateway::args::{Args, Commands, UseTls};
use shuttle_gateway::proxy::UserServiceBuilder;
use shuttle_gateway::service::{GatewayService, MIGRATIONS};
use shuttle_gateway::task;
use shuttle_gateway::tls::{make_tls_acceptor, ChainAndPrivateKey};
use shuttle_gateway::worker::{Worker, WORKER_QUEUE_SIZE};
use sqlx::migrate::MigrateDatabase;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Sqlite, SqlitePool};
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, info_span, trace, warn, Instrument};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    trace!(args = ?args, "parsed args");

    setup_tracing(tracing_subscriber::registry(), "gateway");

    let db_path = args.state.join("gateway.sqlite");
    let db_uri = db_path.to_str().unwrap();

    if !db_path.exists() {
        Sqlite::create_database(db_uri).await.unwrap();
    }

    info!(
        "state db: {}",
        std::fs::canonicalize(&args.state)
            .unwrap()
            .to_string_lossy()
    );

    let sqlite_options = SqliteConnectOptions::from_str(db_uri)
        .unwrap()
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);

    let db = SqlitePool::connect_with(sqlite_options).await.unwrap();
    MIGRATIONS.run(&db).await.unwrap();

    match args.command {
        Commands::Start(start_args) => start(db, args.state, start_args).await,
    }
}

async fn start(db: SqlitePool, fs: PathBuf, args: StartArgs) -> io::Result<()> {
    let gateway = Arc::new(GatewayService::init(args.context.clone(), db).await);

    let worker = Worker::new();

    let sender = worker.sender();

    for (project_name, _) in gateway
        .iter_projects()
        .await
        .expect("could not list projects")
    {
        gateway
            .clone()
            .new_task()
            .project(project_name)
            .and_then(task::refresh())
            .send(&sender)
            .await
            .ok()
            .unwrap();
    }

    let worker_handle = tokio::spawn(
        worker
            .start()
            .map_ok(|_| info!("worker terminated successfully"))
            .map_err(|err| error!("worker error: {}", err)),
    );

    // Every 60secs go over all `::Ready` projects and check their
    // health
    let ambulance_handle = tokio::spawn({
        let gateway = Arc::clone(&gateway);
        let sender = sender.clone();
        async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await; // first tick is immediate

            loop {
                interval.tick().await;

                if sender.capacity() < WORKER_QUEUE_SIZE - SVC_DEGRADED_THRESHOLD {
                    // if degraded, don't stack more health checks
                    warn!(
                        sender.capacity = sender.capacity(),
                        "skipping health checks"
                    );
                    continue;
                }

                if let Ok(projects) = gateway.iter_projects().await {
                    let span = info_span!(
                        "running health checks",
                        healthcheck.num_projects = projects.len()
                    );

                    let gateway = gateway.clone();
                    let sender = sender.clone();
                    async move {
                        for (project_name, _) in projects {
                            if let Ok(handle) = gateway
                                .new_task()
                                .project(project_name)
                                .and_then(task::check_health())
                                .send(&sender)
                                .await
                            {
                                // we wait for the check to be done before
                                // queuing up the next one
                                handle.await
                            }
                        }
                    }
                    .instrument(span)
                    .await;
                }
            }
        }
    });

    let acme_client = AcmeClient::new();

    let mut api_builder = ApiBuilder::new()
        .with_service(Arc::clone(&gateway))
        .with_sender(sender.clone())
        .binding_to(args.control);

    let mut user_builder = UserServiceBuilder::new()
        .with_service(Arc::clone(&gateway))
        .with_task_sender(sender)
        .with_public(args.context.proxy_fqdn.clone())
        .with_user_proxy_binding_to(args.user)
        .with_bouncer(args.bouncer);

    if let UseTls::Enable = args.use_tls {
        let (resolver, tls_acceptor) = make_tls_acceptor();

        user_builder = user_builder
            .with_acme(acme_client.clone())
            .with_tls(tls_acceptor);

        api_builder = api_builder.with_acme(acme_client.clone(), resolver.clone());

        for CustomDomain {
            fqdn,
            certificate,
            private_key,
            ..
        } in gateway.iter_custom_domains().await.unwrap()
        {
            let mut buf = Vec::new();
            buf.extend(certificate.as_bytes());
            buf.extend(private_key.as_bytes());
            resolver
                .serve_pem(&fqdn.to_string(), Cursor::new(buf))
                .await
                .unwrap();
        }

        tokio::spawn(async move {
            // make sure we have a certificate for ourselves
            let certs = init_certs(fs, args.context.proxy_fqdn.clone(), acme_client.clone()).await;
            resolver.serve_default_der(certs).await.unwrap();
        });
    } else {
        warn!("TLS is disabled in the proxy service. This is only acceptable in testing, and should *never* be used in deployments.");
    };

    let api_handle = api_builder
        .with_default_routes()
        .with_auth_service(args.context.auth_uri)
        .with_default_traces()
        .serve();

    let user_handle = user_builder.serve();

    debug!("starting up all services");

    tokio::select!(
        _ = worker_handle => info!("worker handle finished"),
        _ = api_handle => error!("api handle finished"),
        _ = user_handle => error!("user handle finished"),
        _ = ambulance_handle => error!("ambulance handle finished"),
    );

    Ok(())
}

async fn init_certs<P: AsRef<Path>>(fs: P, public: FQDN, acme: AcmeClient) -> ChainAndPrivateKey {
    let tls_path = fs.as_ref().join("ssl.pem");

    match ChainAndPrivateKey::load_pem(&tls_path) {
        Ok(valid) => valid,
        Err(_) => {
            let creds_path = fs.as_ref().join("acme.json");
            warn!(
                "no valid certificate found at {}, creating one...",
                tls_path.display()
            );

            if !creds_path.exists() {
                panic!(
                    "no ACME credentials found at {}, cannot continue with certificate creation",
                    creds_path.display()
                );
            }

            let creds = std::fs::File::open(creds_path).unwrap();
            let creds: AccountCredentials = serde_json::from_reader(&creds).unwrap();

            let identifier = format!("*.{public}");

            // Use ::Dns01 challenge because that's the only supported
            // challenge type for wildcard domains
            let (chain, private_key) = acme
                .create_certificate(&identifier, ChallengeType::Dns01, creds)
                .await
                .unwrap();

            let mut buf = Vec::new();
            buf.extend(chain.as_bytes());
            buf.extend(private_key.as_bytes());

            let certs = ChainAndPrivateKey::parse_pem(Cursor::new(buf)).unwrap();

            certs.clone().save_pem(&tls_path).unwrap();

            certs
        }
    }
}

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    str::FromStr,
};

use async_trait::async_trait;
use opentelemetry::global;
use portpicker::pick_unused_port;
use shuttle_common::{backends::auth::Claim, project::ProjectName as ServiceName};
use shuttle_service::{
    loader::{LoadedService, Loader},
    Factory, Logger,
};
use tokio::task::JoinError;
use tracing::{debug, debug_span, error, info, instrument, trace, Instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use super::{
    provisioner_factory, runtime_logger, storage_manager::StorageManager, KillReceiver, KillSender,
    RunReceiver, State,
};
use crate::error::{Error, Result};

/// Run a task which takes runnable deploys from a channel and starts them up with a factory provided by the
/// abstract factory and a runtime logger provided by the logger factory
/// A deploy is killed when it receives a signal from the kill channel
pub async fn task(
    mut recv: RunReceiver,
    kill_send: KillSender,
    abstract_factory: impl provisioner_factory::AbstractFactory,
    logger_factory: impl runtime_logger::Factory,
    active_deployment_getter: impl ActiveDeploymentsGetter,
    storage_manager: StorageManager,
) {
    info!("Run task started");

    while let Some(built) = recv.recv().await {
        let id = built.id;

        info!("Built deployment at the front of run queue: {id}");

        let kill_send = kill_send.clone();
        let kill_recv = kill_send.subscribe();
        let storage_manager = storage_manager.clone();

        let port = match pick_unused_port() {
            Some(port) => port,
            None => {
                start_crashed_cleanup(
                    &id,
                    Error::PrepareLoad(
                        "could not find a free port to deploy service on".to_string(),
                    ),
                );
                continue;
            }
        };
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
        let service_name = match ServiceName::from_str(&built.service_name) {
            Ok(name) => name,
            Err(err) => {
                start_crashed_cleanup(&id, err);
                continue;
            }
        };
        let mut factory = match abstract_factory
            .get_factory(
                service_name,
                built.service_id,
                built.id,
                storage_manager.clone(),
                built.claim.clone(),
            )
            .await
        {
            Ok(factory) => factory,
            Err(err) => {
                start_crashed_cleanup(&id, err);
                continue;
            }
        };
        let logger = logger_factory.get_logger(id);

        let old_deployments_killer = kill_old_deployments(
            built.service_id,
            id,
            active_deployment_getter.clone(),
            kill_send,
        );
        let cleanup = move |result: std::result::Result<
            std::result::Result<(), shuttle_service::Error>,
            JoinError,
        >| match result {
            Ok(inner) => match inner {
                Ok(()) => completed_cleanup(&id),
                Err(err) => crashed_cleanup(&id, err),
            },
            Err(err) if err.is_cancelled() => stopped_cleanup(&id),
            Err(err) => start_crashed_cleanup(&id, err),
        };

        tokio::spawn(async move {
            let parent_cx = global::get_text_map_propagator(|propagator| {
                propagator.extract(&built.tracing_context)
            });
            let span = debug_span!("runner");
            span.set_parent(parent_cx);

            async move {
                if let Err(err) = built
                    .handle(
                        addr,
                        storage_manager,
                        &mut factory,
                        logger,
                        kill_recv,
                        old_deployments_killer,
                        cleanup,
                    )
                    .await
                {
                    start_crashed_cleanup(&id, err)
                }

                info!("deployment done");
            }
            .instrument(span)
            .await
        });
    }
}

#[instrument(skip(active_deployment_getter, kill_send))]
async fn kill_old_deployments(
    service_id: Uuid,
    deployment_id: Uuid,
    active_deployment_getter: impl ActiveDeploymentsGetter,
    kill_send: KillSender,
) -> Result<()> {
    for old_id in active_deployment_getter
        .clone()
        .get_active_deployments(&service_id)
        .await
        .map_err(|e| Error::OldCleanup(Box::new(e)))?
        .into_iter()
        .filter(|old_id| old_id != &deployment_id)
    {
        trace!(%old_id, "stopping old deployment");
        kill_send
            .send(old_id)
            .map_err(|e| Error::OldCleanup(Box::new(e)))?;
    }

    Ok(())
}

#[instrument(skip(_id), fields(id = %_id, state = %State::Completed))]
fn completed_cleanup(_id: &Uuid) {
    info!("service finished all on its own");
}

#[instrument(skip(_id), fields(id = %_id, state = %State::Stopped))]
fn stopped_cleanup(_id: &Uuid) {
    info!("service was stopped by the user");
}

#[instrument(skip(_id), fields(id = %_id, state = %State::Crashed))]
fn crashed_cleanup(_id: &Uuid, error: impl std::error::Error + 'static) {
    error!(
        error = &error as &dyn std::error::Error,
        "service encountered an error"
    );
}

#[instrument(skip(_id), fields(id = %_id, state = %State::Crashed))]
fn start_crashed_cleanup(_id: &Uuid, error: impl std::error::Error + 'static) {
    error!(
        error = &error as &dyn std::error::Error,
        "service startup encountered an error"
    );
}

#[async_trait]
pub trait ActiveDeploymentsGetter: Clone + Send + Sync + 'static {
    type Err: std::error::Error + Send;

    async fn get_active_deployments(
        &self,
        service_id: &Uuid,
    ) -> std::result::Result<Vec<Uuid>, Self::Err>;
}

#[derive(Clone, Debug)]
pub struct Built {
    pub id: Uuid,
    pub service_name: String,
    pub service_id: Uuid,
    pub tracing_context: HashMap<String, String>,
    pub claim: Option<Claim>,
}

impl Built {
    #[instrument(skip(self, storage_manager, factory, logger, kill_recv, kill_old_deployments, cleanup), fields(id = %self.id, state = %State::Loading))]
    #[allow(clippy::too_many_arguments)]
    async fn handle(
        self,
        address: SocketAddr,
        storage_manager: StorageManager,
        factory: &mut dyn Factory,
        logger: Logger,
        kill_recv: KillReceiver,
        kill_old_deployments: impl futures::Future<Output = Result<()>>,
        cleanup: impl FnOnce(std::result::Result<std::result::Result<(), shuttle_service::Error>, JoinError>)
            + Send
            + 'static,
    ) -> Result<()> {
        let so_path = storage_manager.deployment_library_path(&self.id)?;
        let service = load_deployment(address, so_path, factory, logger).await?;

        kill_old_deployments.await?;

        info!("got handle for deployment");
        // Execute loaded service
        tokio::spawn(run(self.id, service, address, kill_recv, cleanup));

        Ok(())
    }
}

#[instrument(skip(service, kill_recv, cleanup), fields(address = %_address, state = %State::Running))]
async fn run(
    id: Uuid,
    service: LoadedService,
    _address: SocketAddr,
    mut kill_recv: KillReceiver,
    cleanup: impl FnOnce(std::result::Result<std::result::Result<(), shuttle_service::Error>, JoinError>)
        + Send
        + 'static,
) {
    info!("starting up service");
    let (mut handle, library) = service;
    let result;
    loop {
        tokio::select! {
             Ok(kill_id) = kill_recv.recv() => {
                 if kill_id == id {
                     debug!("deployment '{id}' killed");
                     handle.abort();
                     result = handle.await;
                     break;
                 }
             }
             rsl = &mut handle => {
                 result = rsl;
                 break;
             }
        }
    }

    if let Err(err) = library.close() {
        crashed_cleanup(&id, err);
    } else {
        cleanup(result);
    }
}

#[instrument(skip(addr, so_path, factory, logger))]
async fn load_deployment(
    addr: SocketAddr,
    so_path: PathBuf,
    factory: &mut dyn Factory,
    logger: Logger,
) -> Result<LoadedService> {
    let loader = Loader::from_so_file(so_path)?;

    Ok(loader.load(factory, addr, logger).await?)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        net::{Ipv4Addr, SocketAddr},
        path::PathBuf,
        process::Command,
        time::Duration,
    };

    use shuttle_common::database;
    use shuttle_service::{Factory, Logger};
    use tempfile::Builder;
    use tokio::{
        sync::{broadcast, mpsc, oneshot},
        task::JoinError,
        time::sleep,
    };
    use uuid::Uuid;

    use crate::{deployment::storage_manager::StorageManager, error::Error};

    use super::Built;

    const RESOURCES_PATH: &str = "tests/resources";

    struct StubFactory;

    #[async_trait::async_trait]
    impl Factory for StubFactory {
        async fn get_db_connection_string(
            &mut self,
            _db_type: database::Type,
        ) -> Result<String, shuttle_service::Error> {
            panic!("no run test should get an sql connection");
        }

        async fn get_secrets(
            &mut self,
        ) -> Result<BTreeMap<String, String>, shuttle_service::Error> {
            panic!("no test should get any secrets");
        }

        fn get_service_name(&self) -> shuttle_service::ServiceName {
            panic!("no test should get the service name");
        }

        fn get_environment(&self) -> shuttle_service::Environment {
            panic!("no test should get the environment");
        }

        fn get_build_path(&self) -> Result<PathBuf, shuttle_service::Error> {
            panic!("no test should get the build path");
        }

        fn get_storage_path(&self) -> Result<PathBuf, shuttle_service::Error> {
            panic!("no test should get the storage path");
        }
    }

    fn get_storage_manager() -> StorageManager {
        let tmp_dir = Builder::new().prefix("shuttle_run_test").tempdir().unwrap();
        let path = tmp_dir.into_path();

        StorageManager::new(path)
    }

    fn get_logger(id: Uuid) -> Logger {
        let (tx, mut rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Some(log) = rx.recv().await {
                println!("{log}");
            }
        });

        Logger::new(tx, id)
    }

    async fn kill_old_deployments() -> crate::error::Result<()> {
        Ok(())
    }

    // This test uses the kill signal to make sure a service does stop when asked to
    #[tokio::test]
    async fn can_be_killed() {
        let (built, storage_manager) = make_so_and_built("sleep-async");
        let id = built.id;
        let (kill_send, kill_recv) = broadcast::channel(1);
        let (cleanup_send, cleanup_recv) = oneshot::channel();

        let handle_cleanup = |result: std::result::Result<
            std::result::Result<(), shuttle_service::Error>,
            JoinError,
        >| {
            assert!(
                matches!(result, Err(ref join_error) if join_error.is_cancelled()),
                "handle should have been cancelled: {:?}",
                result
            );
            cleanup_send.send(()).unwrap();
        };
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8001);
        let mut factory = StubFactory;
        let logger = get_logger(built.id);

        built
            .handle(
                addr,
                storage_manager,
                &mut factory,
                logger,
                kill_recv,
                kill_old_deployments(),
                handle_cleanup,
            )
            .await
            .unwrap();

        // Give it some time to start up
        sleep(Duration::from_secs(1)).await;

        // Send kill signal
        kill_send.send(id).unwrap();

        tokio::select! {
            _ = sleep(Duration::from_secs(1)) => panic!("cleanup should have been called"),
            Ok(()) = cleanup_recv => {}
        }
    }

    // This test does not use a kill signal to stop the service. Rather the service decided to stop on its own without errors
    #[tokio::test]
    async fn self_stop() {
        let (built, storage_manager) = make_so_and_built("sleep-async");
        let (_kill_send, kill_recv) = broadcast::channel(1);
        let (cleanup_send, cleanup_recv) = oneshot::channel();

        let handle_cleanup = |result: std::result::Result<
            std::result::Result<(), shuttle_service::Error>,
            JoinError,
        >| {
            let result = result.unwrap();
            assert!(
                result.is_ok(),
                "did not expect error from self stopping service: {}",
                result.unwrap_err()
            );
            cleanup_send.send(()).unwrap();
        };
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8001);
        let mut factory = StubFactory;
        let logger = get_logger(built.id);

        built
            .handle(
                addr,
                storage_manager,
                &mut factory,
                logger,
                kill_recv,
                kill_old_deployments(),
                handle_cleanup,
            )
            .await
            .unwrap();

        tokio::select! {
            _ = sleep(Duration::from_secs(5)) => panic!("cleanup should have been called as service stopped on its own"),
            Ok(()) = cleanup_recv => {},
        }
    }

    // Test for panics in Service::bind
    #[tokio::test]
    async fn panic_in_bind() {
        let (built, storage_manager) = make_so_and_built("bind-panic");
        let (_kill_send, kill_recv) = broadcast::channel(1);
        let (cleanup_send, cleanup_recv): (oneshot::Sender<()>, _) = oneshot::channel();

        let handle_cleanup = |result: std::result::Result<
            std::result::Result<(), shuttle_service::Error>,
            JoinError,
        >| {
            let result = result.unwrap();
            assert!(
                matches!(result, Err(shuttle_service::Error::BindPanic(ref msg)) if msg == "panic in bind"),
                "expected inner error from handle: {:?}",
                result
            );
            cleanup_send.send(()).unwrap();
        };
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8001);
        let mut factory = StubFactory;
        let logger = get_logger(built.id);

        built
            .handle(
                addr,
                storage_manager,
                &mut factory,
                logger,
                kill_recv,
                kill_old_deployments(),
                handle_cleanup,
            )
            .await
            .unwrap();

        tokio::select! {
            _ = sleep(Duration::from_secs(5)) => panic!("cleanup should have been called as service handle stopped after panic"),
            Ok(()) = cleanup_recv => {}
        }
    }

    // Test for panics in the main function
    #[tokio::test]
    async fn panic_in_main() {
        let (built, storage_manager) = make_so_and_built("main-panic");
        let (_kill_send, kill_recv) = broadcast::channel(1);

        let handle_cleanup = |_result| panic!("the service shouldn't even start");
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8001);
        let mut factory = StubFactory;
        let logger = get_logger(built.id);

        let result = built
            .handle(
                addr,
                storage_manager,
                &mut factory,
                logger,
                kill_recv,
                kill_old_deployments(),
                handle_cleanup,
            )
            .await;

        assert!(
            matches!(result, Err(Error::Run(shuttle_service::Error::BuildPanic(ref msg))) if msg == "main panic"),
            "expected inner error from main: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn missing_so() {
        let built = Built {
            id: Uuid::new_v4(),
            service_name: "test".to_string(),
            service_id: Uuid::new_v4(),
            tracing_context: Default::default(),
            claim: None,
        };
        let (_kill_send, kill_recv) = broadcast::channel(1);

        let handle_cleanup = |_result| panic!("no service means no cleanup");
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8001);
        let storage_manager = get_storage_manager();
        let mut factory = StubFactory;
        let logger = get_logger(built.id);

        let result = built
            .handle(
                addr,
                storage_manager,
                &mut factory,
                logger,
                kill_recv,
                kill_old_deployments(),
                handle_cleanup,
            )
            .await;

        assert!(
            matches!(
                result,
                Err(Error::Load(shuttle_service::loader::LoaderError::Load(_)))
            ),
            "expected missing 'so' error: {:?}",
            result
        );
    }

    fn make_so_and_built(crate_name: &str) -> (Built, StorageManager) {
        let crate_dir: PathBuf = [RESOURCES_PATH, crate_name].iter().collect();

        Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(&crate_dir)
            .spawn()
            .unwrap()
            .wait()
            .unwrap();

        let dashes_replaced = crate_name.replace('-', "_");

        let lib_name = if cfg!(target_os = "windows") {
            format!("{}.dll", dashes_replaced)
        } else {
            format!("lib{}.so", dashes_replaced)
        };

        let id = Uuid::new_v4();
        let so_path = crate_dir.join("target/release").join(lib_name);
        let storage_manager = get_storage_manager();
        let new_so_path = storage_manager.deployment_library_path(&id).unwrap();

        std::fs::copy(so_path, new_so_path).unwrap();

        (
            Built {
                id,
                service_name: crate_name.to_string(),
                service_id: Uuid::new_v4(),
                tracing_context: Default::default(),
                claim: None,
            },
            storage_manager,
        )
    }
}

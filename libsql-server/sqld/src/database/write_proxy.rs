use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossbeam::channel::TryRecvError;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use uuid::Uuid;

use crate::error::Error;
use crate::query::{self, QueryResponse, QueryResult};
use crate::query_analysis::{final_state, State};
use crate::replication::client::PeriodicDbUpdater;
use crate::rpc::proxy::rpc::proxy_client::ProxyClient;
use crate::rpc::proxy::rpc::query_result::RowResult;
use crate::rpc::proxy::rpc::{DisconnectMessage, Queries, Query};
use crate::rpc::replication_log::rpc::replication_log_client::ReplicationLogClient;
use crate::Result;

use super::{libsql::LibSqlDb, service::DbFactory, Database};

#[derive(Clone)]
pub struct WriteProxyDbFactory {
    write_proxy: ProxyClient<Channel>,
    db_path: PathBuf,
    /// abort handle: abort db update loop on drop
    _abort_handle: crossbeam::channel::Sender<()>,
}

impl WriteProxyDbFactory {
    pub async fn new(
        addr: &str,
        tls: bool,
        cert_path: Option<PathBuf>,
        key_path: Option<PathBuf>,
        ca_cert_path: Option<PathBuf>,
        db_path: PathBuf,
    ) -> anyhow::Result<(Self, JoinHandle<anyhow::Result<()>>)> {
        let mut endpoint = Channel::from_shared(addr.to_string())?;
        if tls {
            let cert_pem = std::fs::read_to_string(cert_path.unwrap())?;
            let key_pem = std::fs::read_to_string(key_path.unwrap())?;
            let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);

            let ca_cert_pem = std::fs::read_to_string(ca_cert_path.unwrap())?;
            let ca_cert = tonic::transport::Certificate::from_pem(ca_cert_pem);

            let tls_config = tonic::transport::ClientTlsConfig::new()
                .identity(identity)
                .ca_certificate(ca_cert)
                .domain_name("sqld");
            endpoint = endpoint.tls_config(tls_config)?;
        }

        let channel = endpoint.connect_lazy();
        // false positive, `.to_string()` is needed to satisfy lifetime bounds
        #[allow(clippy::unnecessary_to_owned)]
        let uri = tonic::transport::Uri::from_maybe_shared(addr.to_string())?;
        let write_proxy = ProxyClient::with_origin(channel.clone(), uri.clone());
        let logger = ReplicationLogClient::with_origin(channel, uri);

        let mut db_updater =
            PeriodicDbUpdater::new(&db_path, logger, Duration::from_secs(1)).await?;
        let (_abort_handle, receiver) = crossbeam::channel::bounded::<()>(1);

        let handle = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            loop {
                // must abort
                if let Err(TryRecvError::Disconnected) = receiver.try_recv() {
                    tracing::warn!("periodic updater exiting");
                    break Ok(());
                }

                match db_updater.step() {
                    Ok(true) => continue,
                    Ok(false) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        });

        let this = Self {
            write_proxy,
            db_path,
            _abort_handle,
        };
        Ok((this, handle))
    }
}

#[async_trait::async_trait]
impl DbFactory for WriteProxyDbFactory {
    async fn create(&self) -> Result<Arc<dyn Database>> {
        let db = WriteProxyDatabase::new(self.write_proxy.clone(), self.db_path.clone())?;
        Ok(Arc::new(db))
    }
}

pub struct WriteProxyDatabase {
    read_db: LibSqlDb,
    write_proxy: ProxyClient<Channel>,
    state: Mutex<State>,
    client_id: Uuid,
}

impl WriteProxyDatabase {
    fn new(write_proxy: ProxyClient<Channel>, path: PathBuf) -> Result<Self> {
        let read_db = LibSqlDb::new(path, (), false)?;
        Ok(Self {
            read_db,
            write_proxy,
            state: Mutex::new(State::Init),
            client_id: Uuid::new_v4(),
        })
    }
}

#[async_trait::async_trait]
impl Database for WriteProxyDatabase {
    async fn execute(&self, queries: query::Queries) -> Result<(Vec<QueryResult>, State)> {
        let mut state = self.state.lock().await;
        if *state == State::Init
            && queries.iter().all(|q| q.stmt.is_read_only())
            && final_state(*state, queries.iter().map(|s| &s.stmt)) == State::Init
        {
            self.read_db.execute(queries).await
        } else {
            let queries = Queries {
                queries: queries
                    .into_iter()
                    .map(|q| {
                        Ok(Query {
                            stmt: q.stmt.stmt,
                            params: Some(q.params.try_into()?),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                client_id: self.client_id.to_string(),
            };
            let mut client = self.write_proxy.clone();
            match client.execute(queries).await {
                Ok(r) => {
                    let execute_result = r.into_inner();
                    *state = execute_result.state().into();
                    let results = execute_result
                        .results
                        .into_iter()
                        .map(|r| -> QueryResult {
                            let result = r.row_result.unwrap();
                            match result {
                                RowResult::Row(res) => Ok(QueryResponse::ResultSet(res.into())),
                                RowResult::Error(e) => Err(Error::RpcQueryError(e)),
                            }
                        })
                        .collect();

                    Ok((results, *state))
                }
                Err(e) => {
                    // Set state to invalid, so next call is sent to remote, and we have a chance
                    // to recover state.
                    *state = State::Invalid;
                    Err(Error::RpcQueryExecutionError(e))
                }
            }
        }
    }
}

impl Drop for WriteProxyDatabase {
    fn drop(&mut self) {
        // best effort attempt to disconnect
        let mut remote = self.write_proxy.clone();
        let client_id = self.client_id.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = remote.disconnect(DisconnectMessage { client_id }).await;
        });
    }
}

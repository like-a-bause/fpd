use crate::data_sources::DataSources;
use anyhow::{anyhow, Context, Result};
use fp_provider_runtime::spec::types::{QueryInstantOptions, QuerySeriesOptions};
use fp_provider_runtime::Runtime;
use futures::select;
use futures::stream::{SplitSink, SplitStream};
use futures::{sink::SinkExt, FutureExt, StreamExt};
use hyper_tungstenite::tungstenite::Message;
use hyper_tungstenite::WebSocketStream;
use proxy_types::*;
use rmp_serde::Serializer;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tokio::net::TcpStream;
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinError;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, MaybeTlsStream};
use tracing::{debug, error, trace, warn};
use url::Url;
use wasmer::{Singlepass, Store, Universal};

const WS_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(45);

/// This is a mapping from the provider type to the bytes of the wasm module
pub type WasmModuleMap = HashMap<String, Vec<u8>>;

#[derive(Clone, Debug)]
pub struct ProxyService {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    endpoint: Url,
    auth_token: String,
    data_sources: DataSources,
    wasm_modules: WasmModuleMap,
}

impl ProxyService {
    /// Load the provider wasm files from the given directory and create a new Proxy instance
    pub async fn init(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_dir: &Path,
        data_sources: DataSources,
    ) -> Result<Self> {
        let wasm_modules = load_wasm_modules(wasm_dir, &data_sources).await?;
        Ok(ProxyService::new(
            fiberplane_endpoint,
            auth_token,
            wasm_modules,
            data_sources,
        ))
    }

    pub(crate) fn new(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_modules: WasmModuleMap,
        data_sources: DataSources,
    ) -> Self {
        ProxyService {
            inner: Arc::new(Inner {
                endpoint: fiberplane_endpoint,
                auth_token,
                wasm_modules,
                data_sources,
            }),
        }
    }

    pub async fn connect(&self, shutdown: Sender<()>) -> Result<()> {
        // open ws connection
        let request = http::Request::builder()
            .uri(self.inner.endpoint.as_str())
            .header("fp-auth-token", self.inner.auth_token.clone())
            .body(())?;

        let (ws_stream, resp) = connect_async(request).await?;

        let conn_id = resp
            .headers()
            .get("x-fp-conn-id")
            .and_then(|id| id.to_str().map(|hv| hv.to_owned()).ok())
            .ok_or_else(|| anyhow!("no connection id was returned"))?;
        debug!("connection established, connection id: {}", conn_id);

        let (mut write_ws, read_ws) = ws_stream.split();

        // Send the list of data sources to the relay
        let data_sources: SetDataSourcesMessage = self
            .inner
            .data_sources
            .iter()
            .map(|(name, data_source)| (name.clone(), data_source.into()))
            .collect();
        debug!("sending data sources to relay: {:?}", data_sources);
        let message = RelayMessage::SetDataSources(data_sources);
        let message = Message::Binary(message.serialize_msgpack());
        write_ws.send(message).await?;

        let (tx_relay_messages, rx_relay_messages) =
            tokio::sync::mpsc::unbounded_channel::<RelayMessage>();

        // We use a local task set because the Wasmer runtime embedded in the ProxyService
        // cannot be moved across threads (which would be necessary to spawn a task that
        // includes the service)
        let local = tokio::task::LocalSet::new();
        let read_handle = local.run_until(Self::handle_read_loop(
            shutdown.clone(),
            conn_id.clone(),
            read_ws,
            tx_relay_messages,
            self.clone(),
        ));

        let write_handle = tokio::spawn(Self::handle_write_loop(
            shutdown.clone(),
            conn_id.clone(),
            rx_relay_messages,
            write_ws,
        ));

        // keep connection open and handle incoming connections
        let (read, write) = futures::join!(read_handle, write_handle);

        match (read, write) {
            (Ok(read), Ok(write)) => {
                let websocket = read.reunite(write);
                websocket.unwrap().close(None).await.ok();
                trace!(?conn_id, "shutdown web-socket connection successfully");
            }
            (read, write) => {
                error!(
                    ?read,
                    ?write,
                    ?conn_id,
                    "unable to reunite read and write web socket streams"
                );
            }
        };

        Ok(())
    }

    /// Handle any incoming web socket messages (`read_ws`) by sending them
    /// to the service for processing.
    ///
    /// This will block until a message is broadcast on the `shutdown` channel.
    /// It can also exit if an error occurred during receiving or sending a
    /// message from the channel.
    async fn handle_read_loop(
        shutdown: Sender<()>,
        conn_id: String,
        mut read_ws: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        tx_relay_messages: UnboundedSender<RelayMessage>,
        service: ProxyService,
    ) -> Result<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>, JoinError> {
        tokio::task::spawn_local(async move {
            use hyper_tungstenite::tungstenite::Message::*;

            let mut should_shutdown = shutdown.subscribe();
            let conn_id = conn_id;
            loop {
                select! {
                    // Handle a ws message if we receive one from the web-socket
                    // connection.
                    message = read_ws.next().fuse() => {
                        // First make sure we actually received a message or
                        // that the connection is closed.
                        let message = match message {
                            Some(Ok(message)) => message,
                            Some(Err(err)) => {
                                warn!(?err, ?conn_id, "web-socket connection closed unexpectedly");
                                if let Err(e) = shutdown.send(()) {
                                    warn!(?e, "unable to send shutdown signal");
                                };
                                break;
                            }
                            None => {
                                trace!(?conn_id, "web-socket connection did not return a message");
                                if let Err(e) = shutdown.send(()) {
                                    warn!(?e, "unable to send shutdown signal");
                                };
                                break;
                            }
                        };

                        // We are only interested in Binary and Close messages.
                        match message {
                            Text(_) => error!("Received Text"),
                            Binary(msg) => {
                                let message = match ServerMessage::deserialize_msgpack(msg) {
                                    Ok(message) => message,
                                    Err(err) => {
                                        warn!(
                                            ?err,
                                            ?conn_id,
                                            "unable to deserialize msgpack encoded server message"
                                        );
                                        continue;
                                    }
                                };

                                let op_id = message.op_id();
                                let result = service.handle_message(message, tx_relay_messages.clone()).await;
                                if let Err(err) = result {
                                    warn!(
                                        ?err,
                                        ?conn_id,
                                        ?op_id,
                                        "service was unable to handle message"
                                    );
                                };
                            }
                            Ping(_) => trace!(?conn_id, "Received Ping"),
                            Pong(_) => trace!(?conn_id, "Received Pong"),
                            Close(_) => {
                                trace!(?conn_id, "received close message");
                                if let Err(e) = shutdown.send(()) {
                                    warn!(?e, "unable to send shutdown signal");
                                };
                                break;
                            }
                        }
                    }
                    // Stop the loop if we receive a message on the
                    // `should_shutdown` broadcast channel.
                    _ = should_shutdown.recv().fuse() => {
                        trace!(?conn_id, "received shutdown signal, stopping read ws loop");
                        break;
                    }
                }
            }

            read_ws
        })
        .await
    }

    /// Handle any outgoing relay messages (`rx_relay_messages`) by sending them
    /// to the outgoing web socket connection (`write_ws`).
    ///
    /// This will block until a message is broadcast on the `shutdown` channel.
    /// It can also exit if an error occurred during sending or receiving a
    /// message from the channel.
    async fn handle_write_loop(
        shutdown: Sender<()>,
        conn_id: String,
        mut rx_relay_messages: UnboundedReceiver<RelayMessage>,
        mut write_ws: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ) -> SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message> {
        let mut should_shutdown = shutdown.subscribe();

        trace!(?conn_id, "handle_command: creating write_handle");
        loop {
            select! {
                // Handle a outgoing relay message by writing it to the web
                // socket connection.
                message = rx_relay_messages.recv().fuse() => {
                    match message {
                        Some(message) => {
                            trace!(?conn_id, "handle_command: sending message to relay");

                            let op_id = message.op_id();

                            let mut buf = Vec::new();
                            if let Err(err) = message.serialize(&mut Serializer::new(&mut buf)) {
                                error!(?err, ?conn_id, ?op_id, "unable to serialize message to msgpack");
                            };

                            if let Err(err) = write_ws.send(Message::Binary(buf)).await {
                                error!(?err, ?conn_id, ?op_id, "unable to serialize message to msgpack");
                                break;
                            }

                            trace!(?conn_id, "handle_command: sending message to relay complete");
                        },
                        None => {
                            if let Err(e) = shutdown.send(()) {
                                warn!(?e, ?conn_id, "unable to send shutdown signal");
                            };
                            break;
                        }
                    }
                }
                // Stop the loop if we receive a message on the
                // should_shutdown broadcast channel.
                _ = should_shutdown.recv().fuse() => {
                    trace!(?conn_id, "received shutdown signal");
                    break;
                }
                // Send a Ping message to the web socket connection if we have
                // not send anything for some amount of time.
                _ = sleep(WS_INACTIVITY_TIMEOUT).fuse() => {
                    if let Err(err) = write_ws.send(Message::Ping(b"ping".to_vec())).await {
                        warn!(?err, ?conn_id, "unable to send ping to server");
                    };
                }
            }
        }

        write_ws
    }

    async fn handle_message(
        &self,
        message: ServerMessage,
        reply: UnboundedSender<RelayMessage>,
    ) -> Result<()> {
        match message {
            ServerMessage::FetchData(message) => {
                self.handle_relay_query_message(message, reply).await
            }
        }
    }

    async fn handle_relay_query_message(
        &self,
        message: FetchDataMessage,
        reply: UnboundedSender<RelayMessage>,
    ) -> Result<()> {
        let data_source_name = message.data_source_name.as_str();
        debug!(
            "received a relay message for data source {}: {:?}",
            data_source_name, message
        );
        let runtime = self.create_runtime(data_source_name).await?;

        let query = message.query;
        let data_source = self
            .inner
            .data_sources
            .get(data_source_name)
            // TODO send error message back to caller
            .ok_or_else(|| anyhow!(format!("unknown data source: {}", data_source_name)))?
            .clone()
            // convert to the fp_provider_runtime type
            .into();

        // Execute either a series or an instant query
        let query_result = match message.query_type {
            QueryType::Series(time_range) => {
                let options = QuerySeriesOptions {
                    data_source,
                    time_range,
                };
                let result = runtime
                    .fetch_series(query, options)
                    .await
                    .with_context(|| "Wasmer runtime error while running fetch_series query")?;
                QueryResult::Series(result)
            }
            QueryType::Instant(time) => {
                let options = QueryInstantOptions { data_source, time };
                let result = runtime
                    .fetch_instant(query, options)
                    .await
                    .with_context(|| "Wasmer runtime error while running fetch_instant query")?;
                QueryResult::Instant(result)
            }
        };

        // TODO: Better handling of invocation errors. Do we send something back in
        // that case, and/or log to stderr?

        let fetch_data_result_message = FetchDataResultMessage {
            op_id: message.op_id,
            result: query_result,
        };

        reply.send(RelayMessage::FetchDataResult(fetch_data_result_message))?;

        Ok(())
    }

    async fn create_runtime(&self, data_source_name: &str) -> Result<Runtime> {
        let data_source_type = match self.inner.data_sources.get(data_source_name) {
            Some(data_source) => data_source.ty(),
            None => {
                return Err(anyhow!(
                    "received relay message for unknown data source: {}",
                    data_source_name
                ))
            }
        };
        let wasm_module: &[u8] = self
            .inner
            .wasm_modules
            .get(data_source_type)
            .unwrap_or_else(|| {
                panic!(
                    "should have loaded wasm module for provider {}",
                    data_source_type,
                )
            });

        compile_wasm(wasm_module)
    }
}

async fn load_wasm_modules(wasm_dir: &Path, data_sources: &DataSources) -> Result<WasmModuleMap> {
    let data_source_types: HashSet<String> = data_sources
        .0
        .values()
        .map(|d| d.ty().to_string())
        .collect();

    let mut wasm_modules = HashMap::new();
    for data_source_type in data_source_types.into_iter() {
        // Each provider's wasm module is found in the wasm_dir as provider_name.wasm
        let wasm_path = &wasm_dir.join(&format!("{}.wasm", &data_source_type));
        let wasm_module = fs::read(wasm_path)
            .await
            .with_context(|| format!("Error loading wasm file: {}", wasm_path.display()))?;

        // Make sure the wasm file can compile
        compile_wasm(&wasm_module).with_context(|| {
            format!(
                "Error compiling wasm file for provider: {}",
                &data_source_type
            )
        })?;

        debug!("loaded provider: {}", data_source_type);
        wasm_modules.insert(data_source_type, wasm_module);
    }

    Ok(wasm_modules)
}

fn compile_wasm(wasm_module: &[u8]) -> Result<Runtime> {
    // TODO can any of these objects be safely cloned between instances?
    let engine = Universal::new(Singlepass::default()).engine();
    let store = Store::new(&engine);
    let runtime = Runtime::new(store, wasm_module)?;
    Ok(runtime)
}

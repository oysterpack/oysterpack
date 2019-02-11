/*
 * Copyright 2019 OysterPack Inc.
 *
 *    Licensed under the Apache License, Version 2.0 (the "License");
 *    you may not use this file except in compliance with the License.
 *    You may obtain a copy of the License at
 *
 *        http://www.apache.org/licenses/LICENSE-2.0
 *
 *    Unless required by applicable law or agreed to in writing, software
 *    distributed under the License is distributed on an "AS IS" BASIS,
 *    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *    See the License for the specific language governing permissions and
 *    limitations under the License.
 */

//! Provides an ReqRep [Client](type.Client.html) application interface for nng clients.
//! - [register_client](fn.register_client.html) is used to register clients in a global registry
//! - [client](fn.client.html) is used to lookup Clients by ReqRepId
//!
//! - The client is fully async and supports parallelism. The level of parallelism is configured via
//!   [DialerConfig::parallelism()](struct.DialerConfig.html#method.parallelism).
//! - When all [Client(s)](type.Client.html) are unregistered and all references fall out of scope, then
//!   the backend ReqRep service will stop which will:
//!   - unregister its context
//!   - close the nng::Dialer and nng:Socket resources
//!   - close the Aio event loop channels, which will trigger the Aio event loop tasks to exit
//!
//! ## Design
//! The server is designed to leverage nng's async capabilities. The approach is to integrate using
//! [nng:Aio](https://docs.rs/nng/latest/nng/struct.Aio.html) and [nng::Context](https://docs.rs/nng/latest/nng/struct.Context.html).
//! via nng's [callback](https://docs.rs/nng/latest/nng/struct.Aio.html#method.with_callback) mechanism.
//! Parallelism is controlled by the number of [callbacks](https://docs.rs/nng/latest/nng/struct.Aio.html#method.with_callback)
//! that are registered. Each callback is linked to a future's based task via an async channel.
//! The callback's job is to simply forward async IO events to the backend end futures task to process.
//! Think of the futures based task as an AIO event loop that processes an async IO event stream.
//! The AIO event loop forwards messages to an  service backend for the actual message processing.
//!

use crate::concurrent::{
    execution::Executor,
    messaging::reqrep::{self, ReqRep, ReqRepId},
};
use crate::opnng::{self, config::SocketConfigError};
use failure::Fail;
use futures::{
    channel::{mpsc, oneshot},
    future::FutureExt,
    sink::SinkExt,
    stream::StreamExt,
    task::SpawnExt,
};
use lazy_static::lazy_static;
use nng::options::Options;
use oysterpack_log::*;
use oysterpack_uid::ULID;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    num::NonZeroUsize,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

lazy_static! {
     /// Global Client contexts
    static ref CLIENT_CONTEXTS: RwLock<fnv::FnvHashMap<ULID, Arc<NngClientContext>>> = RwLock::new(fnv::FnvHashMap::default());

    /// Global ReqRep nng client registry
    static ref CLIENTS: RwLock<fnv::FnvHashMap<ReqRepId, Client>> = RwLock::new(fnv::FnvHashMap::default());
}

/// Client type alias
pub type Client = ReqRep<nng::Message, Result<nng::Message, RequestError>>;

/// If a client with the same ReqRepId is currently registered, then it will be returned.
/// Otherwise, a new client instance is started and registered.
pub fn register_client(
    reqrep_service_config: reqrep::ReqRepConfig,
    socket_config: Option<SocketConfig>,
    dialer_config: DialerConfig,
    executor: Executor,
) -> Result<Client, NngClientError> {
    let mut clients = CLIENTS.write().unwrap();
    let reqrep = match clients.get(&reqrep_service_config.reqrep_id()) {
        Some(reqrep) => reqrep.clone(),
        None => {
            let nng_client = NngClient::new(socket_config, dialer_config, executor.clone())?;
            let reqrep = reqrep_service_config
                .start_service(nng_client, executor)
                .map_err(|err| NngClientError::ReqRepServiceStartFailed(err.is_shutdown()))?;
            let _ = clients.insert(reqrep.id(), reqrep.clone());
            reqrep
        }
    };
    Ok(reqrep)
}

/// Unregisters the client from the global registry
pub fn unregister_client(reqrep_id: ReqRepId) -> Option<Client> {
    let mut clients = CLIENTS.write().unwrap();
    clients.remove(&reqrep_id)
}

/// Returns the client if it is registered
pub fn client(reqrep_id: ReqRepId) -> Option<Client> {
    CLIENTS.read().unwrap().get(&reqrep_id).cloned()
}

/// Returns set of registered ReqRepId(s)
pub fn registered_client_ids() -> Vec<ReqRepId> {
    CLIENTS.read().unwrap().keys().cloned().collect()
}

/// The context that is required by the NngClient's backend service.
#[derive(Clone)]
struct NngClientContext {
    id: ULID,
    socket: Option<nng::Socket>,
    dialer: Option<nng::Dialer>,
    aio_context_pool_return: mpsc::Sender<mpsc::Sender<Request>>,
}

/// nng client
#[derive(Clone)]
struct NngClient {
    id: ULID,
    borrow: mpsc::Sender<oneshot::Sender<mpsc::Sender<Request>>>,
}

impl NngClient {
    /// constructor
    ///
    /// ## Notes
    /// The Executor is used to spawn tasks for handling the nng request / reply processing.
    /// The parallelism defined by the DialerConfig corresponds to the number of Aio callbacks that
    /// will be registered, which corresponds to the number of Aio Context handler tasks spawned.
    fn new(
        socket_config: Option<SocketConfig>,
        dialer_config: DialerConfig,
        mut executor: Executor,
    ) -> Result<Self, NngClientError> {
        let mut nng_client_executor = executor.clone();
        let id = ULID::generate();
        let parallelism = dialer_config.parallelism();
        let (aio_context_pool_return, mut aio_context_pool_borrow) =
            mpsc::channel::<mpsc::Sender<Request>>(parallelism);

        let create_context = move || {
            let socket = SocketConfig::create_socket(socket_config)
                .map_err(NngClientError::SocketCreateFailure)?;
            let dialer = dialer_config
                .start_dialer(&socket)
                .map_err(NngClientError::DialerStartError)?;

            Ok(NngClientContext {
                id,
                socket: Some(socket),
                dialer: Some(dialer),
                aio_context_pool_return,
            })
        };

        let mut start_workers = move |ctx: &NngClientContext| {
            for i in 0..parallelism {
                // used to notify the workers when an Aio event has occurred, i.e., the Aio callback has been invoked
                let (aio_tx, mut aio_rx) = futures::channel::mpsc::unbounded::<()>();
                // wrap aio_tx within a Mutex in order to make it unwind safe and usable within  Aio callback
                let aio_tx = Mutex::new(aio_tx);
                let context = nng::Context::new(ctx.socket.as_ref().unwrap())
                    .map_err(NngClientError::NngContextCreateFailed)?;
                let callback_ctx = context.clone();
                let aio = nng::Aio::with_callback(move |_aio| {
                    let aio_tx = aio_tx.lock().unwrap();
                    if let Err(err) = aio_tx.unbounded_send(()) {
                        // means the channel has been disconnected because the worker Future task has completed
                        // the server is either being stopped, or the worker has crashed
                        // TODO: we need a way to know if the server is being shutdown
                        warn!("Failed to nofify worker of Aio event. This means the worker is not running. The Aio Context will be closed: {}", err);
                        // TODO: will cloning the Context work ? Context::close() cannot be invoked from the callback because it consumes the Context
                        //       and rust won't allow it because the Context is being referenced by the FnMut closure
                        callback_ctx.clone().close();
                        // TODO: send an alert - if the worker crashed, i.e., panicked, then it may need to be restarted
                    }
                }).map_err(NngClientError::NngAioCreateFailed)?;

                let (req_tx, mut req_rx) = futures::channel::mpsc::channel::<Request>(1);
                let mut aio_context_pool_return = ctx.aio_context_pool_return.clone();
                {
                    let req_tx = req_tx.clone();
                    let mut aio_context_pool_return = aio_context_pool_return.clone();
                    let aio_context_pool_return_send_result = executor
                        .spawn_await(async move { await!(aio_context_pool_return.send(req_tx)) });
                    if aio_context_pool_return_send_result.is_err() {
                        return Err(NngClientError::AioContextPoolChannelClosed);
                    }
                }
                executor.spawn(async move {
                    debug!("[{}-{}] NngClient Aio Context task is running", id, i);
                    while let Some(mut req) = await!(req_rx.next()) {
                        debug!("[{}-{}] NngClient: processing request", id, i);
                        if let Some(msg) = req.msg.take() {
                            // send the request
                            match context.send(&aio, msg) {
                                Ok(_) => {
                                    if await!(aio_rx.next()).is_none() {
                                        debug!("[{}-{}] NngClient Aio callback channel is closed", id, i);
                                        break
                                    }
                                    match aio.result().unwrap() {
                                        Ok(_) => {
                                            // TODO: set a timeout - see Aio::set_timeout()
                                            // receive the reply
                                            match context.recv(&aio) {
                                                Ok(_) => {
                                                    if await!(aio_rx.next()).is_none() {
                                                        debug!("[{}-{}] NngClient Aio callback channel is closed", id, i);
                                                        break
                                                    }
                                                    match aio.result().unwrap() {
                                                        Ok(_) => {
                                                            match aio.get_msg() {
                                                                Some(reply) => {
                                                                    let _ = req.reply_chan.send(Ok(reply));
                                                                },
                                                                None => {
                                                                    let _ = req.reply_chan.send(Err(RequestError::NoReplyMessage));
                                                                }
                                                            }
                                                        }
                                                        Err(err) => {
                                                            let _ = req.reply_chan.send(Err(RequestError::RecvFailed(err)));
                                                            aio.cancel();
                                                        }
                                                    }
                                                },
                                                Err(err) => {
                                                    let _ = req.reply_chan.send(Err(RequestError::RecvFailed(err)));
                                                    aio.cancel();
                                                }
                                            }
                                        },
                                        Err(err) => {
                                            let _ = req.reply_chan.send(Err(RequestError::SendFailed(err)));
                                            aio.cancel();
                                        }
                                    }
                                },
                                Err((_msg, err)) =>  {
                                    let _ = req.reply_chan.send(Err(RequestError::SendFailed(err)));
                                    aio.cancel();
                                }
                            }
                        } else {
                            let _ = req.reply_chan.send(Err(RequestError::InvalidRequest("BUG: Request was received with no nng::Message".to_string())));
                        }
                        // add a request Sender back to the pool, indicating the worker is now available
                        if let Err(err) = await!(aio_context_pool_return.send(req_tx.clone())) {
                            error!("[{}-{}] Failed to return request sender back to the pool: {}",id, i, err)
                        }
                        debug!("[{}-{}] NngClient: request is done", id, i);
                    }
                    debug!("[{}-{}] NngClient Aio Context task is done", id, i);
                }).map_err(|err| NngClientError::AioContextTaskSpawnError(err.is_shutdown()))?;
            }

            Ok(())
        };

        let ctx = create_context()?;
        start_workers(&ctx)?;

        let mut clients = CLIENT_CONTEXTS.write().unwrap();
        clients.insert(ctx.id, Arc::new(ctx));

        let (borrow_tx, mut borrow_rx) = mpsc::channel::<oneshot::Sender<mpsc::Sender<Request>>>(1);
        nng_client_executor.spawn(async move {
            debug!("NngClient Aio Context Pool task is running: {}", id);
            while let Some(reply_chan) = await!(borrow_rx.next()) {
                match await!(aio_context_pool_borrow.next()) {
                    Some(request_sender) => {
                        let _ = reply_chan.send(request_sender);
                    },
                    None => {
                        debug!("`aio_context_pool_borrow` channel is disconnected - thus we are done");
                        break;
                    }
                }
            }
            // drain the aio_context_pool_borrow channel and close the Aio Context handler channels
            // - this is required in order for the AIO Context handler tasks to exit
            while let Some(mut sender) = await!(aio_context_pool_borrow.next()) {
                sender.close_channel();
                debug!("closed Aio Context channel");
            }

            debug!("NngClient Aio Context Pool task is done: {}", id);
        }).map_err(|err| NngClientError::AioContextTaskSpawnError(err.is_shutdown()))?;

        Ok(Self {
            id,
            borrow: borrow_tx,
        })
    }
}

impl fmt::Debug for NngClient {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "NngClient({})", self.id)
    }
}

impl reqrep::Processor<nng::Message, Result<nng::Message, RequestError>> for NngClient {
    fn process(
        &mut self,
        req: nng::Message,
    ) -> reqrep::FutureReply<Result<nng::Message, RequestError>> {
        let mut borrow = self.borrow.clone();

        async move {
            let (borrow_tx, borrow_rx) = oneshot::channel();
            if await!(borrow.send(borrow_tx)).is_err() {
                return Err(RequestError::NngAioContextPoolChannelDisconnected);
            }

            let (tx, rx) = oneshot::channel();
            let request = Request {
                msg: Some(req),
                reply_chan: tx,
            };

            match await!(borrow_rx) {
                Ok(ref mut sender) => match await!(sender.send(request)) {
                    Ok(_) => match await!(rx) {
                        Ok(result) => result,
                        Err(_) => Err(RequestError::ReplyChannelClosed),
                    },
                    Err(err) => Err(RequestError::AioContextChannelDisconnected(err)),
                },
                Err(_) => Err(RequestError::NngAioContextPoolChannelDisconnected),
            }
        }
            .boxed()
    }

    fn destroy(&mut self) {
        debug!("NngClient({}) is being destroyed ...", self.id);
        let mut client_contexts = CLIENT_CONTEXTS.write().unwrap();
        if let Some(mut context) = client_contexts.remove(&self.id) {
            let context = Arc::get_mut(&mut context).unwrap();
            context.dialer.take().unwrap().close();
            debug!("NngClient({}): closed nng::Dialer", self.id);
            context.socket.take().unwrap().close();
            debug!("NngClient({}): closed nng::Socket ", self.id);
            context.aio_context_pool_return.close_channel();
            self.borrow.close_channel();
            debug!("NngClient({}): closed channels", self.id);
        }
        debug!("NngClient({}) is destroyed", self.id);
    }
}

/// NngClient related errors
#[derive(Debug, Fail)]
pub enum NngClientError {
    /// Failed to create Socket
    #[fail(display = "Failed to create Socket: {}", _0)]
    SocketCreateFailure(SocketConfigError),
    /// Failed to start Dialer
    #[fail(display = "Failed to start Dialer: {}", _0)]
    DialerStartError(DialerConfigError),
    /// Failed to create nng::Context
    #[fail(display = "Failed to create nng::Context: {}", _0)]
    NngContextCreateFailed(nng::Error),
    /// Failed to create nng::Aio
    #[fail(display = "Failed to create nng::Aio: {}", _0)]
    NngAioCreateFailed(nng::Error),
    /// The Aio Context pool channel is closed
    #[fail(display = "The Aio Context pool channel is closed")]
    AioContextPoolChannelClosed,
    /// Failed to spawn Aio Context request handler task
    #[fail(
        display = "Failed to spawn Aio Context request handler task: executor is shutdown = {}",
        _0
    )]
    AioContextTaskSpawnError(bool),
    /// Failed to spawn Aio Context request handler task
    #[fail(
        display = "Failed to spawn ReqRep service: executor is shutdown = {}",
        _0
    )]
    ReqRepServiceStartFailed(bool),
}

/// Request related errors
#[derive(Debug, Fail, Clone)]
pub enum RequestError {
    /// The nng Aio Context pool channel is disconnected
    #[fail(display = "The nng Aio Context pool channel is disconnected.")]
    NngAioContextPoolChannelDisconnected,
    /// The nng Aio Context channel is disconnected
    #[fail(display = "The nng Aio Context channel is disconnected: {}", _0)]
    AioContextChannelDisconnected(futures::channel::mpsc::SendError),
    /// Reply channel closed
    #[fail(display = "Reply channel closed")]
    ReplyChannelClosed,
    /// Failed to send the request
    #[fail(display = "Failed to send request: {}", _0)]
    SendFailed(nng::Error),
    /// Failed to receive the reply
    #[fail(display = "Failed to receive reply: {}", _0)]
    RecvFailed(nng::Error),
    /// Empty message
    #[fail(display = "Invalid request: {}", _0)]
    InvalidRequest(String),
    /// No reply message
    #[fail(display = "BUG: No reply message was found - this should never happen")]
    NoReplyMessage,
}

struct Request {
    msg: Option<nng::Message>,
    reply_chan: oneshot::Sender<Result<nng::Message, RequestError>>,
}

/// Socket Settings
#[derive(Debug, Serialize, Deserialize)]
pub struct SocketConfig {
    reconnect_min_time: Option<Duration>,
    reconnect_max_time: Option<Duration>,
    resend_time: Option<Duration>,
    socket_config: Option<opnng::config::SocketConfig>,
}

impl SocketConfig {
    pub(crate) fn create_socket(
        socket_config: Option<SocketConfig>,
    ) -> Result<nng::Socket, SocketConfigError> {
        let mut socket =
            nng::Socket::new(nng::Protocol::Req0).map_err(SocketConfigError::SocketCreateFailed)?;
        socket.set_nonblocking(true);
        match socket_config {
            Some(socket_config) => socket_config.apply(socket),
            None => Ok(socket),
        }
    }

    /// set socket options
    pub(crate) fn apply(&self, socket: nng::Socket) -> Result<nng::Socket, SocketConfigError> {
        let socket = if let Some(settings) = self.socket_config.as_ref() {
            settings.apply(socket)?
        } else {
            socket
        };

        socket
            .set_opt::<nng::options::ReconnectMinTime>(self.reconnect_min_time)
            .map_err(SocketConfigError::ReconnectMinTime)?;

        socket
            .set_opt::<nng::options::ReconnectMaxTime>(self.reconnect_max_time)
            .map_err(SocketConfigError::ReconnectMaxTime)?;

        socket
            .set_opt::<nng::options::protocol::reqrep::ResendTime>(self.resend_time)
            .map_err(SocketConfigError::ResendTime)?;

        Ok(socket)
    }

    /// Socket settings
    pub fn socket_config(&self) -> Option<&opnng::config::SocketConfig> {
        self.socket_config.as_ref()
    }

    /// Amount of time to wait before sending a new request.
    ///
    /// When a new request is started, a timer of this duration is also started. If no reply is
    /// received before this timer expires, then the request will be resent. (Requests are also
    /// automatically resent if the peer to whom the original request was sent disconnects, or if a
    /// peer becomes available while the requester is waiting for an available peer.)
    pub fn resend_time(&self) -> Option<Duration> {
        self.resend_time
    }

    /// The minimum amount of time to wait before attempting to establish a connection after a previous
    /// attempt has failed.
    ///
    /// If set on a Socket, this value becomes the default for new dialers. Individual dialers can
    /// then override the setting.
    pub fn reconnect_min_time(&self) -> Option<Duration> {
        self.reconnect_min_time
    }

    ///The maximum amount of time to wait before attempting to establish a connection after a previous
    /// attempt has failed.
    ///
    /// If this is non-zero, then the time between successive connection attempts will start at the
    /// value of ReconnectMinTime, and grow exponentially, until it reaches this value. If this value
    /// is zero, then no exponential back-off between connection attempts is done, and each attempt
    /// will wait the time specified by ReconnectMinTime. This can be set on a socket, but it can
    /// also be overridden on an individual dialer.
    pub fn reconnect_max_time(&self) -> Option<Duration> {
        self.reconnect_max_time
    }

    /// The minimum amount of time to wait before attempting to establish a connection after a previous
    /// attempt has failed.
    pub fn set_reconnect_min_time(self, reconnect_min_time: Duration) -> Self {
        let mut this = self;
        this.reconnect_min_time = Some(reconnect_min_time);
        this
    }

    ///The maximum amount of time to wait before attempting to establish a connection after a previous
    /// attempt has failed.
    pub fn set_reconnect_max_time(self, reconnect_max_time: Duration) -> Self {
        let mut this = self;
        this.reconnect_max_time = Some(reconnect_max_time);
        this
    }

    /// Amount of time to wait before sending a new request.
    pub fn set_resend_time(self, resend_time: Duration) -> Self {
        let mut this = self;
        this.resend_time = Some(resend_time);
        this
    }

    /// Apply socket settings
    pub fn set_socket_config(self, config: opnng::config::SocketConfig) -> Self {
        let mut this = self;
        this.socket_config = Some(config);
        this
    }
}

/// Dialer Settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialerConfig {
    #[serde(with = "url_serde")]
    url: url::Url,
    parallelism: usize,
    recv_max_size: Option<usize>,
    no_delay: Option<bool>,
    keep_alive: Option<bool>,
    reconnect_min_time: Option<Duration>,
    reconnect_max_time: Option<Duration>,
}

impl DialerConfig {
    /// constructor
    /// - parallelism = number of logical CPUs
    pub fn new(url: url::Url) -> DialerConfig {
        DialerConfig {
            url,
            recv_max_size: None,
            no_delay: None,
            keep_alive: None,
            parallelism: num_cpus::get(),
            reconnect_min_time: None,
            reconnect_max_time: None,
        }
    }

    /// Start a socket dialer.
    ///
    /// Normally, the first attempt to connect to the dialer's address is done synchronously, including
    /// any necessary name resolution. As a result, a failure, such as if the connection is refused,
    /// will be returned immediately, and no further action will be taken.
    ///
    /// However, if nonblocking is specified, then the connection attempt is made asynchronously.
    ///
    /// Furthermore, if the connection was closed for a synchronously dialed connection, the dialer
    /// will still attempt to redial asynchronously.
    ///
    /// The returned handle controls the life of the dialer. If it is dropped, the dialer is shut down
    /// and no more messages will be received on it.
    pub fn start_dialer(self, socket: &nng::Socket) -> Result<nng::Dialer, DialerConfigError> {
        let dialer_options = nng::DialerOptions::new(socket, self.url.as_str())
            .map_err(DialerConfigError::DialerOptionsCreateFailed)?;

        if let Some(recv_max_size) = self.recv_max_size {
            dialer_options
                .set_opt::<nng::options::RecvMaxSize>(recv_max_size)
                .map_err(DialerConfigError::RecvMaxSize)?;
        }

        if let Some(no_delay) = self.no_delay {
            dialer_options
                .set_opt::<nng::options::transport::tcp::NoDelay>(no_delay)
                .map_err(DialerConfigError::TcpNoDelay)?;
        }

        if let Some(keep_alive) = self.keep_alive {
            dialer_options
                .set_opt::<nng::options::transport::tcp::KeepAlive>(keep_alive)
                .map_err(DialerConfigError::TcpKeepAlive)?;
        }

        dialer_options
            .set_opt::<nng::options::ReconnectMinTime>(self.reconnect_min_time)
            .map_err(DialerConfigError::ReconnectMinTime)?;

        dialer_options
            .set_opt::<nng::options::ReconnectMaxTime>(self.reconnect_max_time)
            .map_err(DialerConfigError::ReconnectMaxTime)?;

        dialer_options
            .start(true)
            .map_err(|(_options, err)| DialerConfigError::DialerStartError(err))
    }

    /// the address that the server is listening on
    pub fn url(&self) -> &url::Url {
        &self.url
    }

    /// Max number of async IO operations that can be performed concurrently, which corresponds to the number
    /// of socket contexts that will be created.
    /// - if not specified, then it will default to the number of logical CPUs
    pub fn parallelism(&self) -> usize {
        self.parallelism
    }

    /// The maximum message size that the will be accepted from a remote peer.
    ///
    /// If a peer attempts to send a message larger than this, then the message will be discarded.
    /// If the value of this is zero, then no limit on message sizes is enforced. This option exists
    /// to prevent certain kinds of denial-of-service attacks, where a malicious agent can claim to
    /// want to send an extraordinarily large message, without sending any data. This option can be
    /// set for the socket, but may be overridden for on a per-dialer or per-listener basis.
    pub fn recv_max_size(&self) -> Option<usize> {
        self.recv_max_size
    }

    /// When true (the default), messages are sent immediately by the underlying TCP stream without waiting to gather more data.
    /// When false, Nagle's algorithm is enabled, and the TCP stream may wait briefly in attempt to coalesce messages.
    ///
    /// Nagle's algorithm is useful on low-bandwidth connections to reduce overhead, but it comes at a cost to latency.
    pub fn no_delay(&self) -> Option<bool> {
        self.no_delay
    }

    /// Enable the sending of keep-alive messages on the underlying TCP stream.
    ///
    /// This option is false by default. When enabled, if no messages are seen for a period of time,
    /// then a zero length TCP message is sent with the ACK flag set in an attempt to tickle some traffic
    /// from the peer. If none is still seen (after some platform-specific number of retries and timeouts),
    /// then the remote peer is presumed dead, and the connection is closed.
    ///
    /// This option has two purposes. First, it can be used to detect dead peers on an otherwise quiescent
    /// network. Second, it can be used to keep connection table entries in NAT and other middleware
    /// from being expiring due to lack of activity.
    pub fn keep_alive(&self) -> Option<bool> {
        self.keep_alive
    }

    /// The minimum amount of time to wait before attempting to establish a connection after a previous
    /// attempt has failed.
    ///
    /// If set on a Socket, this value becomes the default for new dialers. Individual dialers can
    /// then override the setting.
    pub fn reconnect_min_time(&self) -> Option<Duration> {
        self.reconnect_min_time
    }

    ///The maximum amount of time to wait before attempting to establish a connection after a previous
    /// attempt has failed.
    ///
    /// If this is non-zero, then the time between successive connection attempts will start at the
    /// value of ReconnectMinTime, and grow exponentially, until it reaches this value. If this value
    /// is zero, then no exponential back-off between connection attempts is done, and each attempt
    /// will wait the time specified by ReconnectMinTime. This can be set on a socket, but it can
    /// also be overridden on an individual dialer.
    pub fn reconnect_max_time(&self) -> Option<Duration> {
        self.reconnect_max_time
    }

    /// Sets the maximum message size that the will be accepted from a remote peer.
    pub fn set_recv_max_size(self, recv_max_size: usize) -> Self {
        let mut settings = self;
        settings.recv_max_size = Some(recv_max_size);
        settings
    }

    /// Sets no delay setting on TCP connection
    pub fn set_no_delay(self, no_delay: bool) -> Self {
        let mut settings = self;
        settings.no_delay = Some(no_delay);
        settings
    }

    /// Sets keep alive setting on TCP connection
    pub fn set_keep_alive(self, keep_alive: bool) -> Self {
        let mut settings = self;
        settings.keep_alive = Some(keep_alive);
        settings
    }

    /// set the max capacity of concurrent async requests
    pub fn set_parallelism(self, count: NonZeroUsize) -> Self {
        let mut settings = self;
        settings.parallelism = count.get();
        settings
    }

    /// The minimum amount of time to wait before attempting to establish a connection after a previous
    /// attempt has failed.
    pub fn set_reconnect_min_time(self, reconnect_min_time: Duration) -> Self {
        let mut this = self;
        this.reconnect_min_time = Some(reconnect_min_time);
        this
    }

    ///The maximum amount of time to wait before attempting to establish a connection after a previous
    /// attempt has failed.
    pub fn set_reconnect_max_time(self, reconnect_max_time: Duration) -> Self {
        let mut this = self;
        this.reconnect_max_time = Some(reconnect_max_time);
        this
    }
}

/// Dialer config related errors
#[derive(Debug, Fail)]
pub enum DialerConfigError {
    /// Failed to create DialerOptions
    #[fail(display = "Failed to create DialerOptions: {}", _0)]
    DialerOptionsCreateFailed(nng::Error),
    /// Failed to set the RecvMaxSize option
    #[fail(display = "Failed to set the RecvMaxSize option: {}", _0)]
    RecvMaxSize(nng::Error),
    /// Failed to set the TcpNoDelay option
    #[fail(display = "Failed to set the TcpNoDelay option: {}", _0)]
    TcpNoDelay(nng::Error),
    /// Failed to set the TcpKeepAlive option
    #[fail(display = "Failed to set the TcpKeepAlive option: {}", _0)]
    TcpKeepAlive(nng::Error),
    /// Failed to set the ReconnectMinTime option
    #[fail(display = "Failed to set the ReconnectMinTime option: {}", _0)]
    ReconnectMinTime(nng::Error),
    /// Failed to set the ReconnectMaxTime option
    #[fail(display = "Failed to set the ReconnectMaxTime option: {}", _0)]
    ReconnectMaxTime(nng::Error),
    /// Failed to start Dialer
    #[fail(display = "Failed to start Dialer: {}", _0)]
    DialerStartError(nng::Error),
}

#[allow(warnings)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::opnng::config::{SocketConfig, SocketConfigError};
    use crate::{
        concurrent::{
            execution::{self, *},
            messaging::reqrep::{self, *},
        },
        configure_logging, metrics,
        opnng::reqrep::server,
    };
    use futures::executor::ThreadPoolBuilder;
    use oysterpack_uid::ULID;
    use oysterpack_uid::*;
    use std::{thread, time::Duration};

    struct EchoService;
    impl Processor<nng::Message, nng::Message> for EchoService {
        fn process(&mut self, req: nng::Message) -> reqrep::FutureReply<nng::Message> {
            async move { req }.boxed()
        }
    }

    const REQREP_ID: ReqRepId = ReqRepId(1871557337320005579010710867531265404);

    fn start_server() -> ReqRep<nng::Message, nng::Message> {
        let timer_buckets = metrics::TimerBuckets::from(
            vec![
                Duration::from_nanos(50),
                Duration::from_nanos(100),
                Duration::from_nanos(150),
                Duration::from_nanos(200),
            ]
            .as_slice(),
        );
        ReqRepConfig::new(REQREP_ID, timer_buckets)
            .start_service(EchoService, global_executor().clone())
            .unwrap()
    }

    fn start_client(url: url::Url) -> (Client, ExecutorId) {
        let timer_buckets = metrics::TimerBuckets::from(
            vec![
                Duration::from_nanos(50),
                Duration::from_nanos(100),
                Duration::from_nanos(150),
                Duration::from_nanos(200),
            ]
            .as_slice(),
        );

        let client_executor_id = ExecutorId::generate();
        let client = super::register_client(
            ReqRepConfig::new(REQREP_ID, timer_buckets),
            None,
            DialerConfig::new(url),
            {
                let mut threadpool_builder = ThreadPoolBuilder::new();
                execution::register(client_executor_id, &mut threadpool_builder).unwrap()
            },
        )
        .unwrap();
        (client, client_executor_id)
    }

    #[test]
    fn nng_client_single_client() {
        configure_logging();
        let mut executor = execution::global_executor();

        // GIVEN: the server is running
        let url = url::Url::parse(&format!("inproc://{}", ULID::generate())).unwrap();
        let server_executor_id = ExecutorId::generate();
        let mut threadpool_builder = ThreadPoolBuilder::new();
        let mut server_handle = server::spawn(
            None,
            server::ListenerConfig::new(url.clone()),
            start_server(),
            execution::register(server_executor_id, &mut threadpool_builder).unwrap(),
        )
        .unwrap();
        assert!(server_handle.ping().unwrap());

        // GIVEN: the NngClient is registered
        let (mut client, client_executor_id) = start_client(url.clone());
        // THEN: the client ReqRepId should match
        assert_eq!(client.id(), REQREP_ID);
        // WHEN: the client is dropped
        drop(client);
        const REQUEST_COUNT: usize = 100;
        let replies: Vec<nng::Message> = executor
            .spawn_await(
                async {
                    // Then: the client can still be retrieved from the global registry
                    let mut client = super::client(REQREP_ID).unwrap();
                    // AND: the client is still functional
                    let mut replies = Vec::with_capacity(REQUEST_COUNT);
                    for _ in 0..REQUEST_COUNT {
                        let reply_receiver: ReplyReceiver<Result<nng::Message, RequestError>> =
                            await!(client.send(nng::Message::new().unwrap())).unwrap();
                        replies.push(await!(reply_receiver.recv()).unwrap().unwrap());
                    }
                    replies
                },
            )
            .unwrap();
        // THEN: all requests were successfully processed
        assert_eq!(replies.len(), REQUEST_COUNT);

        // WHEN: the client is unregistered
        let client = super::unregister_client(REQREP_ID).unwrap();
        assert!(super::unregister_client(REQREP_ID).is_none());
        assert!(super::client(REQREP_ID).is_none());

        // WHEN: the last client reference is dropped
        drop(client);
        thread::yield_now();
        let executor = execution::executor(client_executor_id).unwrap();
        for _ in 0..10 {
            if executor.active_task_count() == 0 {
                info!("all client tasks have completed");
                break;
            }
            info!("waiting for NngClient Aio Context handler tasks to exit: executor.active_task_count() = {}", executor.active_task_count());
            thread::sleep_ms(1);
        }
        assert_eq!(executor.active_task_count(), 0);
    }

    #[test]
    fn nng_client_multithreaded_usage() {
        configure_logging();
        let mut thread_builder = ThreadPoolBuilder::new();
        let mut executor =
            execution::register(ExecutorId::generate(), &mut thread_builder).unwrap();

        // GIVEN: the server is running
        let url = url::Url::parse(&format!("inproc://{}", ULID::generate())).unwrap();
        let server_executor_id = ExecutorId::generate();
        let mut threadpool_builder = ThreadPoolBuilder::new();
        let mut server_handle = server::spawn(
            None,
            server::ListenerConfig::new(url.clone()),
            start_server(),
            execution::register(server_executor_id, &mut threadpool_builder).unwrap(),
        )
        .unwrap();
        assert!(server_handle.ping().unwrap());

        // GIVEN: the NngClient is registered
        let (mut client, client_executor_id) = start_client(url.clone());

        const TASK_COUNT: usize = 10;
        const REQUEST_COUNT: usize = 100;
        let mut handles = Vec::new();
        for _ in 0..TASK_COUNT {
            let handle = executor
                .spawn_with_handle(
                    async {
                        // Then: the client can still be retrieved from the global registry
                        let mut client = super::client(REQREP_ID).unwrap();
                        // AND: the client is still functional
                        let mut replies = Vec::with_capacity(REQUEST_COUNT);
                        for _ in 0..REQUEST_COUNT {
                            let reply_receiver: ReplyReceiver<Result<nng::Message, RequestError>> =
                                await!(client.send(nng::Message::new().unwrap())).unwrap();
                            replies.push(await!(reply_receiver.recv()).unwrap().unwrap());
                        }
                        replies
                    },
                )
                .unwrap();
            handles.push(handle);
        }

        executor
            .spawn_await(
                async move {
                    for handle in handles {
                        let replies: Vec<nng::Message> = await!(handle);
                        assert_eq!(replies.len(), REQUEST_COUNT);
                    }
                },
            )
            .unwrap();
    }
}

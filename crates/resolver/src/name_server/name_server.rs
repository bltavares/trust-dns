// Copyright 2015-2019 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::cmp::Ordering;
use std::fmt::{self, Debug, Formatter};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use futures_util::{future::Future, lock::Mutex};
#[cfg(feature = "tokio-runtime")]
use tokio::runtime::Handle;

use proto::error::{ProtoError, ProtoResult};
#[cfg(feature = "mdns")]
use proto::multicast::MDNS_IPV4;
use proto::op::ResponseCode;
use proto::xfer::{DnsHandle, DnsRequest, DnsResponse};

#[cfg(feature = "mdns")]
use crate::config::Protocol;
use crate::config::{NameServerConfig, ResolverOpts};
use crate::name_server::{ConnectionProvider, NameServerState, NameServerStats};
#[cfg(feature = "tokio-runtime")]
use crate::name_server::{TokioConnection, TokioConnectionProvider};

/// Specifies the details of a remote NameServer used for lookups
#[derive(Clone)]
pub struct NameServer<C: DnsHandle + Send, P: ConnectionProvider<Conn = C> + Send> {
    config: NameServerConfig,
    options: ResolverOpts,
    client: Arc<Mutex<Option<C>>>,
    state: Arc<NameServerState>,
    stats: Arc<NameServerStats>,
    conn_provider: P,
}

impl<C: DnsHandle, P: ConnectionProvider<Conn = C>> Debug for NameServer<C, P> {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "config: {:?}, options: {:?}", self.config, self.options)
    }
}

#[cfg(feature = "tokio-runtime")]
impl NameServer<TokioConnection, TokioConnectionProvider> {
    pub fn new(config: NameServerConfig, options: ResolverOpts, runtime: Handle) -> Self {
        Self::new_with_provider(config, options, TokioConnectionProvider::new(runtime))
    }
}

impl<C: DnsHandle, P: ConnectionProvider<Conn = C>> NameServer<C, P> {
    pub fn new_with_provider(
        config: NameServerConfig,
        options: ResolverOpts,
        conn_provider: P,
    ) -> NameServer<C, P> {
        Self {
            config,
            options,
            client: Arc::new(Mutex::new(None)),
            state: Arc::new(NameServerState::init(None)),
            stats: Arc::new(NameServerStats::default()),
            conn_provider,
        }
    }

    #[doc(hidden)]
    pub fn from_conn(
        config: NameServerConfig,
        options: ResolverOpts,
        client: C,
        conn_provider: P,
    ) -> NameServer<C, P> {
        Self {
            config,
            options,
            client: Arc::new(Mutex::new(Some(client))),
            state: Arc::new(NameServerState::init(None)),
            stats: Arc::new(NameServerStats::default()),
            conn_provider,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_connected(&self) -> bool {
        !self.state.is_failed()
            && if let Some(client) = self.client.try_lock() {
                client.is_some()
            } else {
                // assuming that if someone has it locked it will be or is connected
                true
            }
    }

    /// This will return a mutable client to allows for sending messages.
    ///
    /// If the connection is in a failed state, then this will establish a new connection
    async fn connected_mut_client(&mut self) -> ProtoResult<C> {
        let mut client = self.client.lock().await;

        // if this is in a failure state
        if self.state.is_failed() || client.is_none() {
            debug!("reconnecting: {:?}", self.config);

            // TODO: we need the local EDNS options
            self.state.reinit(None);

            let new_client = self
                .conn_provider
                .new_connection(&self.config, &self.options)
                .await?;

            // establish a new connection
            *client = Some(new_client);
        } else {
            debug!("existing connection: {:?}", self.config);
        }

        Ok((*client)
            .clone()
            .expect("bad state, client should be connected"))
    }

    async fn inner_send<R: Into<DnsRequest> + Unpin + Send + 'static>(
        mut self,
        request: R,
    ) -> Result<DnsResponse, ProtoError> {
        let mut client = self.connected_mut_client().await?;
        let response = client.send(request).await;

        match response {
            Ok(response) => {
                // first we'll evaluate if the message succeeded
                //   see https://github.com/bluejekyll/trust-dns/issues/606
                //   TODO: there are probably other return codes from the server we may want to
                //    retry on. We may also want to evaluate NoError responses that lack records as errors as well
                if self.options.distrust_nx_responses {
                    match response.response_code() {
                        ResponseCode::ServFail => {
                            let note = "Nameserver responded with SERVFAIL";
                            debug!("{}", note);
                            return Err(ProtoError::from(note));
                        }
                        ResponseCode::NXDomain => {
                            let note = "Nameserver responded with NXDomain";
                            debug!("{}", note);
                            return Err(ProtoError::from(note));
                        }
                        ResponseCode::NoError if response.answers().is_empty() => {
                            let note = "Nameserver responded with NoError";
                            debug!("{}", note);
                            return Err(ProtoError::from(note));
                        }
                        _ => (),
                    }
                }

                // TODO: consider making message::take_edns...
                let remote_edns = response.edns().cloned();

                // take the remote edns options and store them
                self.state.establish(remote_edns);

                // record the success
                self.stats.next_success();
                Ok(response)
            }
            Err(error) => {
                debug!("name_server connection failure: {}", error);

                // this transitions the state to failure
                self.state.fail(Instant::now());

                // record the failure
                self.stats.next_failure();

                // These are connection failures, not lookup failures, that is handled in the resolver layer
                Err(error)
            }
        }
    }
}

impl<C, P> DnsHandle for NameServer<C, P>
where
    C: DnsHandle,
    P: ConnectionProvider<Conn = C>,
{
    type Response = Pin<Box<dyn Future<Output = Result<DnsResponse, ProtoError>> + Send>>;

    fn is_verifying_dnssec(&self) -> bool {
        self.options.validate
    }

    // TODO: there needs to be some way of customizing the connection based on EDNS options from the server side...
    fn send<R: Into<DnsRequest> + Unpin + Send + 'static>(&mut self, request: R) -> Self::Response {
        let this = self.clone();
        // if state is failed, return future::err(), unless retry delay expired..
        Box::pin(this.inner_send(request))
    }
}

impl<C: DnsHandle, P: ConnectionProvider<Conn = C>> Ord for NameServer<C, P> {
    /// Custom implementation of Ord for NameServer which incorporates the performance of the connection into it's ranking
    fn cmp(&self, other: &Self) -> Ordering {
        // if they are literally equal, just return
        if self == other {
            return Ordering::Equal;
        }

        // otherwise, run our evaluation to determine the next to be returned from the Heap
        //   this will prefer established connections, we should try other connections after
        //   some number to make sure that all are used. This is more important for when
        //   latency is started to be used.
        match self.state.cmp(&other.state) {
            Ordering::Equal => (),
            o => {
                return o;
            }
        }

        self.stats.cmp(&other.stats)
    }
}

impl<C: DnsHandle, P: ConnectionProvider<Conn = C>> PartialOrd for NameServer<C, P> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<C: DnsHandle, P: ConnectionProvider<Conn = C>> PartialEq for NameServer<C, P> {
    /// NameServers are equal if the config (connection information) are equal
    fn eq(&self, other: &Self) -> bool {
        self.config == other.config
    }
}

impl<C: DnsHandle, P: ConnectionProvider<Conn = C>> Eq for NameServer<C, P> {}

// TODO: once IPv6 is better understood, also make this a binary keep.
#[cfg(feature = "mdns")]
pub(crate) fn mdns_nameserver<C, P>(options: ResolverOpts, conn_provider: P) -> NameServer<C, P>
where
    C: DnsHandle,
    P: ConnectionProvider<Conn = C>,
{
    let config = NameServerConfig {
        socket_addr: *MDNS_IPV4,
        protocol: Protocol::Mdns,
        tls_dns_name: None,
        #[cfg(feature = "dns-over-rustls")]
        tls_config: None,
    };
    NameServer::new_with_provider(config, options, conn_provider)
}

#[cfg(test)]
#[cfg(feature = "tokio-runtime")]
mod tests {
    extern crate env_logger;

    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;

    use futures_util::{future, FutureExt};
    use tokio::runtime::Runtime;

    use proto::op::{Query, ResponseCode};
    use proto::rr::{Name, RecordType};
    use proto::xfer::{DnsHandle, DnsRequestOptions};

    use super::*;
    use crate::config::Protocol;

    #[test]
    fn test_name_server() {
        //env_logger::try_init().ok();

        let config = NameServerConfig {
            socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
            protocol: Protocol::Udp,
            tls_dns_name: None,
            #[cfg(feature = "dns-over-rustls")]
            tls_config: None,
        };
        let mut io_loop = Runtime::new().unwrap();
        let runtime_handle = io_loop.handle().clone();
        let name_server = future::lazy(|_| {
            NameServer::<_, TokioConnectionProvider>::new(
                config,
                ResolverOpts::default(),
                runtime_handle,
            )
        });

        let name = Name::parse("www.example.com.", None).unwrap();
        let response = io_loop
            .block_on(name_server.then(|mut name_server| {
                name_server.lookup(
                    Query::query(name.clone(), RecordType::A),
                    DnsRequestOptions::default(),
                )
            }))
            .expect("query failed");
        assert_eq!(response.response_code(), ResponseCode::NoError);
    }

    #[test]
    fn test_failed_name_server() {
        let mut options = ResolverOpts::default();
        options.timeout = Duration::from_millis(1); // this is going to fail, make it fail fast...
        let config = NameServerConfig {
            socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 252)), 252),
            protocol: Protocol::Udp,
            tls_dns_name: None,
            #[cfg(feature = "dns-over-rustls")]
            tls_config: None,
        };
        let mut io_loop = Runtime::new().unwrap();
        let runtime_handle = io_loop.handle().clone();
        let name_server = future::lazy(|_| {
            NameServer::<_, TokioConnectionProvider>::new(config, options, runtime_handle)
        });

        let name = Name::parse("www.example.com.", None).unwrap();
        assert!(io_loop
            .block_on(name_server.then(|mut name_server| name_server.lookup(
                Query::query(name.clone(), RecordType::A),
                DnsRequestOptions::default()
            )))
            .is_err());
    }
}

//! gRPC server implementation.
//!
//! TODO: - Add GrpcServerError error type and rewrite functions to return <Result<(), GrpcServerError>>, propagating internal errors.
//!       - Add user and password as fields of ProxyClient and use here.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use http::Uri;
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;
use zingo_rpc::{jsonrpc::connector::JsonRpcConnector, primitives::ProxyClient};

/// Configuration data for gRPC server.
pub struct ProxyServer(pub ProxyClient);

impl ProxyServer {
    /// Starts gRPC service.
    pub fn serve(
        self,
        port: impl Into<u16> + Send + Sync + 'static,
        online: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
        tokio::task::spawn(async move {
            let svc = CompactTxStreamerServer::new(self.0);
            let sockaddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), port.into());
            println!("@zingoproxyd: GRPC server listening on: {sockaddr}.");
            while online.load(Ordering::SeqCst) {
                let server = tonic::transport::Server::builder()
                    .add_service(svc.clone())
                    .serve(sockaddr)
                    .await;
                match server {
                    Ok(_) => (),
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
    }

    /// Creates configuration data for gRPC server.
    pub fn new(lightwalletd_uri: http::Uri, zebrad_uri: http::Uri) -> Self {
        ProxyServer(ProxyClient {
            lightwalletd_uri,
            zebrad_uri,
            online: Arc::new(AtomicBool::new(true)),
        })
    }
}

/// Spawns a gRPC server.
pub async fn spawn_server(
    proxy_port: &u16,
    lwd_port: &u16,
    zebrad_port: &u16,
    online: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
    let lwd_uri = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{lwd_port}"))
        .path_and_query("/")
        .build()
        .unwrap();

    // TODO Add user and password as fields of ProxyClient and use here.
    let zebra_uri = JsonRpcConnector::test_and_return_uri(
        zebrad_port,
        Some("xxxxxx".to_string()),
        Some("xxxxxx".to_string()),
    )
    .await
    .unwrap();

    let server = ProxyServer::new(lwd_uri, zebra_uri);
    server.serve(proxy_port.clone(), online)
}

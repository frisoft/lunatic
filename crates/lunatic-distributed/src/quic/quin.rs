use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use dashmap::{mapref::entry, DashMap};
use lunatic_process::{env::Environment, state::ProcessState};
use quinn::{ClientConfig, Connecting, ConnectionError, Endpoint, ServerConfig};
use rustls::server::AllowAnyAuthenticatedClient;
use rustls_pemfile::Item;
use wasmtime::ResourceLimiter;

use crate::{
    distributed::{self},
    DistributedCtx,
};

pub struct SendStream {
    pub stream: quinn::SendStream,
}

impl SendStream {
    pub async fn send(&mut self, data: &mut [Bytes]) -> Result<()> {
        self.stream.write_all_chunks(data).await?;
        Ok(())
    }
}

pub struct RecvStream {
    pub stream: quinn::RecvStream,
}

impl RecvStream {
    pub async fn receive(&mut self) -> Result<Bytes> {
        let mut size = [0u8; 4];
        self.stream.read_exact(&mut size).await?;
        let size = u32::from_le_bytes(size);
        let mut buffer = vec![0u8; size as usize];
        self.stream.read_exact(&mut buffer).await?;
        Ok(buffer.into())
    }

    pub fn id(&self) -> quinn::StreamId {
        self.stream.id()
    }
}

#[derive(Clone)]
pub struct Client {
    inner: Endpoint,
}

impl Client {
    pub async fn _connect(&self, addr: SocketAddr, name: &str) -> Result<quinn::Connection> {
        Ok(self.inner.connect(addr, name)?.await?)
    }

    pub async fn try_connect(
        &self,
        addr: SocketAddr,
        name: &str,
        retry: u32,
    ) -> Result<quinn::Connection> {
        for try_num in 1..(retry + 1) {
            match self._connect(addr, name).await {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    log::error!("Error connecting to {name} at {addr}, try {try_num}. Error: {e}")
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(anyhow!("Failed to connect to {name} at {addr}"))
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
        name: &str,
        retry: u32,
    ) -> Result<(SendStream, RecvStream)> {
        for try_num in 1..(retry + 1) {
            match self.connect_once(addr, name).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    log::error!("Error connecting to {name} at {addr}, try {try_num}. Error: {e}")
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(anyhow!("Failed to connect to {name} at {addr}"))
    }

    async fn connect_once(&self, addr: SocketAddr, name: &str) -> Result<(SendStream, RecvStream)> {
        let conn = self.inner.connect(addr, name)?.await?;
        let (send, recv) = conn.open_bi().await?;
        Ok((SendStream { stream: send }, RecvStream { stream: recv }))
    }
}

pub fn new_quic_client(ca_cert: &str, cert: &str, key: &str) -> Result<Client> {
    let mut ca_cert = ca_cert.as_bytes();
    let ca_cert = rustls_pemfile::read_one(&mut ca_cert)?.unwrap();
    let ca_cert = match ca_cert {
        Item::X509Certificate(ca_cert) => Ok(rustls::Certificate(ca_cert)),
        _ => Err(anyhow!("Not a valid certificate.")),
    }?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(&ca_cert)?;

    let mut cert = cert.as_bytes();
    let mut key = key.as_bytes();
    let pk = rustls_pemfile::read_one(&mut key)?.unwrap();
    let pk = match pk {
        Item::PKCS8Key(key) => Ok(rustls::PrivateKey(key)),
        _ => Err(anyhow!("Not a valid private key.")),
    }?;
    let cert = rustls_pemfile::read_one(&mut cert)?.unwrap();
    let cert = match cert {
        Item::X509Certificate(cert) => Ok(rustls::Certificate(cert)),
        _ => Err(anyhow!("Not a valid certificate")),
    }?;
    let cert = vec![cert];

    let client_crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_single_cert(cert, pk)?;

    let client_config = ClientConfig::new(Arc::new(client_crypto));
    let mut endpoint = Endpoint::client("[::]:0".parse().unwrap())?;
    endpoint.set_default_client_config(client_config);
    Ok(Client { inner: endpoint })
}

pub fn new_quic_server(addr: SocketAddr, cert: &str, key: &str, ca_cert: &str) -> Result<Endpoint> {
    let mut ca_cert = ca_cert.as_bytes();
    let ca_cert = rustls_pemfile::read_one(&mut ca_cert)?.unwrap();
    let ca_cert = match ca_cert {
        Item::X509Certificate(ca_cert) => Ok(rustls::Certificate(ca_cert)),
        _ => Err(anyhow!("Not a valid certificate.")),
    }?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(&ca_cert)?;

    let mut cert = cert.as_bytes();
    let mut key = key.as_bytes();
    let pk = rustls_pemfile::read_one(&mut key)?.unwrap();
    let pk = match pk {
        Item::PKCS8Key(key) => Ok(rustls::PrivateKey(key)),
        _ => Err(anyhow!("Not a valid private key.")),
    }?;
    let cert = rustls_pemfile::read_one(&mut cert)?.unwrap();
    let cert = match cert {
        Item::X509Certificate(cert) => Ok(rustls::Certificate(cert)),
        _ => Err(anyhow!("Not a valid certificate")),
    }?;
    let cert = vec![cert];
    let server_crypto = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_client_cert_verifier(AllowAnyAuthenticatedClient::new(roots))
        .with_single_cert(cert, pk)?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(server_crypto));
    Arc::get_mut(&mut server_config.transport)
        .unwrap()
        .max_concurrent_uni_streams(0_u8.into());

    Ok(quinn::Endpoint::server(server_config, addr)?)
}

pub async fn handle_node_server<T, E>(
    quic_server: &mut Endpoint,
    ctx: distributed::server::ServerCtx<T, E>,
) -> Result<()>
where
    T: ProcessState + ResourceLimiter + DistributedCtx<E> + Send + Sync + 'static,
    E: Environment + 'static,
{
    while let Some(conn) = quic_server.accept().await {
        tokio::spawn(handle_quic_connection_node(ctx.clone(), conn));
    }
    Err(anyhow!("Node server exited"))
}

async fn handle_quic_connection_node<T, E>(
    ctx: distributed::server::ServerCtx<T, E>,
    conn: Connecting,
) -> Result<()>
where
    T: ProcessState + ResourceLimiter + DistributedCtx<E> + Send + Sync + 'static,
    E: Environment + 'static,
{
    log::info!("New node connection");
    let conn = conn.await?;
    log::info!("Remote {} connected", conn.remote_address());
    loop {
        if let Some(reason) = conn.close_reason() {
            log::info!("Connection {} is closed: {reason}", conn.remote_address());
            break;
        }
        let stream = conn.accept_bi().await;
        log::info!("Stream from remote {} accepted", conn.remote_address());
        match stream {
            Ok((s, r)) => {
                let send = SendStream { stream: s };
                let recv = RecvStream { stream: r };
                tokio::spawn(handle_quic_stream_node(ctx.clone(), send, recv));
            }
            Err(ConnectionError::LocallyClosed) => break,
            Err(_) => {}
        }
    }
    log::info!("Connection from remote {} closed", conn.remote_address());
    Ok(())
}

async fn handle_quic_stream_node<T, E>(
    ctx: distributed::server::ServerCtx<T, E>,
    mut send: SendStream,
    recv: RecvStream,
) where
    T: ProcessState + ResourceLimiter + DistributedCtx<E> + Send + Sync + 'static,
    E: Environment + 'static,
{
    let mut recv_ctx = RecvCtx {
        recv: recv.stream,
        chunks: DashMap::new(),
    };
    while let Ok((msg_id, bytes)) = read_next_stream_message(&mut recv_ctx).await {
        if let Ok(request) = rmp_serde::from_slice::<distributed::message::Request>(&bytes) {
            distributed::server::handle_message(ctx.clone(), &mut send, msg_id, request).await;
        } else {
            log::debug!("Error deserializing request");
        }
    }
}

struct Chunk {
    message_id: u64,
    message_size: usize,
    data: Vec<u8>,
}

struct RecvCtx {
    recv: quinn::RecvStream,
    // Map to collect message chunks key: message_id, data: (message_size, data)
    chunks: DashMap<u64, (usize, Vec<u8>)>,
}

async fn read_next_stream_chunk(recv: &mut quinn::RecvStream) -> Result<Chunk> {
    // Read chunk header info
    let mut message_id = [0u8; 8];
    let mut message_size = [0u8; 4];
    let mut chunk_id = [0u8; 8];
    let mut chunk_size = [0u8; 4];
    recv.read_exact(&mut message_id)
        .await
        .map_err(|e| anyhow!("{e} failed to read header message_id"))?;
    recv.read_exact(&mut message_size)
        .await
        .map_err(|e| anyhow!("{e} failed to read header message_size"))?;
    recv.read_exact(&mut chunk_id)
        .await
        .map_err(|e| anyhow!("{e} failed to read header chunk_id"))?;
    recv.read_exact(&mut chunk_size)
        .await
        .map_err(|e| anyhow!("{e} failed to read header chunk_size"))?;
    let message_id = u64::from_le_bytes(message_id);
    let message_size = u32::from_le_bytes(message_size) as usize;
    let chunk_id = u64::from_le_bytes(chunk_id);
    let chunk_size = u32::from_le_bytes(chunk_size) as usize;
    // Read chunk data
    let mut data = vec![0u8; chunk_size];
    recv.read_exact(&mut data)
        .await
        .map_err(|e| anyhow!("{e} failed to read message body"))?;
    log::trace!("read message_id={message_id} chunk_id={chunk_id}");
    Ok(Chunk {
        message_id,
        message_size,
        data,
    })
}

async fn read_next_stream_message(ctx: &mut RecvCtx) -> Result<(u64, Bytes)> {
    loop {
        let new_chunk = read_next_stream_chunk(&mut ctx.recv).await?;
        let message_id = new_chunk.message_id;
        let message_size = new_chunk.message_size;
        if let Some(mut entry) = ctx.chunks.get_mut(&message_id) {
            entry.1.extend(new_chunk.data);
        } else {
            ctx.chunks
                .insert(message_id, (message_size, new_chunk.data));
        };
        let finished = ctx
            .chunks
            .get(&message_id)
            .map(|entry| entry.0 == entry.1.len());
        match finished {
            Some(true) => {
                let (message_id, data) = ctx.chunks.remove(&message_id).unwrap();
                log::trace!("Finished collecting message_id={message_id}");
                return Ok((message_id, Bytes::from(data.1)));
            }
            Some(false) => {
                continue;
            }
            None => unreachable!("Message must exists at all times"),
        }
    }
}

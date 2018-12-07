// Copyright 2018 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate atomicwrites;
extern crate bytes;
extern crate protobuf;
extern crate rustls;
extern crate webpki;
#[macro_use]
extern crate log;
extern crate byteorder;
extern crate messaging;
extern crate mio;
extern crate openssl;
extern crate serde;
extern crate serde_yaml;
extern crate url;
#[macro_use]
extern crate serde_derive;
extern crate crossbeam_channel;
extern crate mio_extras;
#[cfg(test)]
extern crate tempdir;

macro_rules! rwlock_read_unwrap {
    ($lock:expr) => {
        match $lock.read() {
            Ok(d) => d,
            Err(e) => panic!("RwLock error: {:?}", e),
        }
    }
}

macro_rules! rwlock_write_unwrap {
    ($lock:expr) => {
        match $lock.write() {
            Ok(d) => d,
            Err(e) => panic!("RwLock error: {:?}", e),
        }
    }
}

mod async;
pub mod connection;
mod errors;
pub mod mesh;
pub mod storage;
pub mod transport;

use byteorder::{BigEndian, WriteBytesExt};
use rustls::{
    AllowAnyAuthenticatedClient, Certificate, ClientConfig, ClientSession, PrivateKey,
    ServerConfig, ServerSession, SupportedCipherSuite,
};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Write};
use std::net::SocketAddr;
use std::sync::{mpsc, Arc, Mutex};
use std::time;

use messaging::protocol::{
    CircuitCreateRequest, CircuitCreateResponse, CircuitCreateResponse_Status,
    CircuitDestroyRequest, CircuitDestroyResponse, CircuitDestroyResponse_Status, Message,
    MessageType,
};

use async::NoBlock;
use connection::*;
pub use errors::{AddCircuitError, RemoveCircuitError, SplinterError};

/// Shorthand for the transmit half of the message channel.
pub type Tx = mpsc::Sender<Message>;

pub enum DaemonRequest {
    CreateConnection { address: String },
}

/// Used to request that a new connection should be created.
///
///  Consumes tuple (circuit_id, address)
///
/// Connections may receive requests that can result in a
/// new connection needing to be created. This task should
/// be preformed by a damon that owns a Connection, not the
/// connection itself.
pub type DaemonChannel = mpsc::Sender<(DaemonRequest)>;

/// Shorthand for the receive half of the message channel.
pub type Rx = mpsc::Receiver<Message>;

pub struct Shared {
    pub peers: HashMap<SocketAddr, Tx>,
    pub services: HashMap<SocketAddr, Tx>,
    pub circuits: HashMap<String, Circuit>,
}

impl Shared {
    /// Create a new, empty, instance of `Shared`.
    pub fn new() -> Shared {
        Shared {
            peers: HashMap::new(),
            services: HashMap::new(),
            circuits: HashMap::new(),
        }
    }
}

pub struct Circuit {
    pub name: String,
    // service id, node_url
    pub peers: HashMap<String, SocketAddr>,
}

impl Circuit {
    pub fn new(name: String, peers: HashMap<String, SocketAddr>) -> Circuit {
        Circuit { name, peers }
    }

    pub fn add_peer(&mut self, service_id: String, node_url: SocketAddr) {
        self.peers.insert(service_id, node_url);
    }

    pub fn get_peers(&mut self) -> HashMap<String, SocketAddr> {
        self.peers.clone()
    }
}

pub enum ConnectionType {
    Network,
    Service,
}

pub enum ConnectionState {
    Running,
    Closing,
    Closed,
}

/// This is a connection which has been accepted by the server,
/// and is currently being served.
///
/// It has a TCP-level stream, and some
/// other state/metadata.
pub struct ConnectionDriver<T: Connection> {
    state: Arc<Mutex<Shared>>,
    peer_addr: SocketAddr,
    network_addr: SocketAddr,
    connection: T,
    connection_type: ConnectionType,
    rx: Rx,
    daemon_chan: DaemonChannel,
}

impl<T: Connection> ConnectionDriver<T> {
    pub fn new(
        connection: T,
        network_addr: SocketAddr,
        peer_addr: SocketAddr,
        state: Arc<Mutex<Shared>>,
        connection_type: ConnectionType,
        daemon_chan: DaemonChannel,
    ) -> Result<ConnectionDriver<T>, SplinterError> {
        // Create a channel for this peer
        let (tx, rx) = mpsc::channel();
        // Add an entry for this `Peer` in the shared state map.
        match connection_type {
            ConnectionType::Network => {
                state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .peers
                    .insert(peer_addr, tx);
            }
            ConnectionType::Service => {
                state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .services
                    .insert(peer_addr, tx);
            }
        }

        Ok(ConnectionDriver {
            state,
            peer_addr,
            network_addr,
            connection,
            connection_type,
            rx,
            daemon_chan,
        })
    }

    fn read_and_handle_msg(&mut self) -> Result<NoBlock<()>, SplinterError> {
        if let NoBlock::Ready(mut msg) = self.connection.read()? {
            match msg.get_message_type() {
                MessageType::UNSET => {
                    debug!("Received message with an unset message type: {:?}", msg);
                    return Ok(NoBlock::WouldBlock);
                }
                MessageType::HEARTBEAT_REQUEST => {
                    let mut response = Message::new();
                    response.set_message_type(MessageType::HEARTBEAT_RESPONSE);
                    self.respond(response)?;
                }
                MessageType::HEARTBEAT_RESPONSE => (),
                MessageType::CIRCUIT_CREATE_REQUEST => {
                    let circuit_create = msg.take_circuit_create_request();
                    self.add_circuit(circuit_create)?;
                }
                MessageType::CIRCUIT_DESTROY_REQUEST => {
                    let circuit_destroy = msg.take_circuit_destroy_request();
                    self.remove_circuit(circuit_destroy)?;
                }
                _ => self.gossip_message(msg)?,
            };
            Ok(NoBlock::Ready(()))
        } else {
            Ok(NoBlock::WouldBlock)
        }
    }

    fn write_msg(&mut self, msg: &Message) -> Result<NoBlock<()>, SplinterError> {
        Ok(self.connection.write(msg)?)
    }

    fn send_heartbeat(&mut self) -> Result<NoBlock<()>, SplinterError> {
        info!("Sending Heartbeat to {:?}", self.peer_addr);
        let mut msg = Message::new();
        msg.set_message_type(MessageType::HEARTBEAT_REQUEST);
        Ok(self.write_msg(&msg)?)
    }

    pub fn run(&mut self) -> Result<(), SplinterError> {
        loop {
            if let NoBlock::Ready(_) = self.connection.handshake()? {
                break;
            }
        }

        let mut count = 0;
        loop {
            if let NoBlock::Ready(()) = self.read_and_handle_msg()? {
                count = 0;
            }

            if count == 10 {
                self.send_heartbeat()?;
                count = 0
            }
            count = count + 1;

            match self.rx.recv_timeout(time::Duration::from_millis(100)) {
                Ok(msg) => {
                    // need to check if this is succesful and retry if it WouldBlock
                    if let NoBlock::WouldBlock = self.write_msg(&msg)? {
                        // write failed, resubmit the message to the reciever
                        let services = &self
                            .state
                            .lock()
                            .expect("Connection's Shared state lock was poisoned")
                            .services;
                        if let Some(tx) = services.get(&self.peer_addr) {
                            debug!("Retrying {:?}", msg);
                            tx.send(msg)?;
                        }
                    }
                }
                Err(e) if e == mpsc::RecvTimeoutError::Timeout => continue,
                Err(err) => {
                    debug!("Need to handle Error: {:?}", err);
                }
            }
        }
    }

    fn gossip_message(&mut self, msg: Message) -> Result<(), SplinterError> {
        // If message received from service forward to nodes, if from nodes forward to services
        // This needs to eventually handle the message types
        match self.connection_type {
            ConnectionType::Network => {
                let services = &self
                    .state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .services;
                for (addr, tx) in services {
                    //Don't send the message to ourselves
                    if *addr == self.peer_addr {
                        debug!("Service {} {:?}", addr, msg);
                        // The send only fails if the rx half has been
                        // dropped, however this is impossible as the
                        // `tx` half will be removed from the map
                        // before the `rx` is dropped.
                        tx.send(msg.clone())?;
                    }
                }
            }
            ConnectionType::Service => {
                let peers = &self
                    .state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .peers;
                for (addr, tx) in peers {
                    //Don't send the message to ourselves
                    if *addr != self.peer_addr {
                        debug!("Peer {} {:?}", addr, msg);
                        // The send only fails if the rx half has been
                        // dropped, however this is impossible as the
                        // `tx` half will be removed from the map
                        // before the `rx` is dropped.
                        tx.send(msg.clone())?;
                    }
                }
            }
        }
        Ok(())
    }

    fn respond(&mut self, msg: Message) -> Result<(), SplinterError> {
        self.write_msg(&msg)?;
        Ok(())
    }

    fn direct_message(
        &mut self,
        msg: Message,
        addr: &SocketAddr,
        connection_type: ConnectionType,
    ) -> Result<(), SplinterError> {
        match connection_type {
            ConnectionType::Service => {
                let services = &self
                    .state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .services;
                if let Some(tx) = services.get(addr) {
                    debug!("Service {} {:?}", addr, msg);
                    tx.send(msg)?;
                } else {
                    warn!("Cant find Service addr: {}", addr)
                }
            }
            ConnectionType::Network => {
                let peers = &self
                    .state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .peers;
                if let Some(tx) = peers.get(addr) {
                    debug!("Peer {} {:?}", addr, msg);
                    tx.send(msg)?;
                } else {
                    warn!("Cant find Peer addr: {}", addr)
                }
            }
        }
        Ok(())
    }

    fn add_circuit(&mut self, msg: CircuitCreateRequest) -> Result<(), AddCircuitError> {
        info!("Create Circuit request received: {:?}", msg);
        let circuit_name = msg.get_circuit_name();
        let mut circuit = Circuit::new(circuit_name.to_string(), HashMap::new());

        // connecting might fail if the node is not ready to make the connection and will need to
        // be retried later
        let mut circuit_create_response = CircuitCreateResponse::new();
        circuit_create_response.set_circuit_name(circuit_name.to_string());
        circuit_create_response.set_participants(protobuf::RepeatedField::from_vec(
            msg.get_participants().to_vec(),
        ));
        let mut response_message = Message::new();
        response_message.set_message_type(MessageType::CIRCUIT_CREATE_RESPONSE);

        if self
            .state
            .lock()
            .expect("Connection's Shared state lock was poisoned")
            .circuits
            .contains_key(circuit_name)
        {
            debug!(
                "Cannot create Circuit that already exists: {}",
                &circuit_name
            );
            circuit_create_response
                .set_status(CircuitCreateResponse_Status::CIRCUIT_ALREADY_EXISTS);

            circuit_create_response.set_error_message(format!(
                "Cannot CreateCircuit that already exists: {}",
                &circuit_name
            ));
            response_message.set_circuit_create_response(circuit_create_response);
            self.respond(response_message).map_err(|_| {
                AddCircuitError::SendError(format!(
                    "Unable to respond to CircuitCreateRequest from {}",
                    &self.peer_addr
                ))
            })?;
        } else {
            for participant in msg.get_participants().iter() {
                let node_url: SocketAddr = participant.get_network_node_url().parse()?;
                circuit.add_peer(participant.get_service_id().to_string(), node_url);

                // need to rebuild the message to forward
                let mut forward_msg = Message::new();
                forward_msg.set_message_type(MessageType::CIRCUIT_CREATE_REQUEST);
                forward_msg.set_circuit_create_request(msg.clone());

                // if participant is this splinter node, skip forward
                if node_url == self.network_addr {
                    continue;
                }

                if !(self
                    .state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .peers
                    .contains_key(&node_url))
                {
                    info!("sending request to daemon to create connection");

                    let address = {
                        let mut url = String::from("tcp://");
                        url.push_str(participant.get_network_node_url());
                        url
                    };
                    self.daemon_chan
                        .send(DaemonRequest::CreateConnection { address })?;
                } else {
                    self.direct_message(forward_msg.clone(), &node_url, ConnectionType::Network)
                        .map_err(|_| {
                            AddCircuitError::SendError(format!(
                                "Unable to forward CircuitCreateRequest to {}",
                                node_url
                            ))
                        })?;
                }

                circuit_create_response.set_status(CircuitCreateResponse_Status::OK);
            }

            self.state
                .lock()
                .expect("Connection's Shared state lock was poisoned")
                .circuits
                .insert(circuit.name.clone(), circuit);

            response_message.set_circuit_create_response(circuit_create_response);
            self.respond(response_message).map_err(|_| {
                AddCircuitError::SendError(format!(
                    "Unable to respond to CircuitCreateRequest from {}",
                    &self.peer_addr
                ))
            })?;
        }
        Ok(())
    }

    fn remove_circuit(&mut self, msg: CircuitDestroyRequest) -> Result<(), RemoveCircuitError> {
        info!("Destory Circuit request received: {:?}", msg);
        let circuit_name = msg.get_circuit_name();
        let mut circuit_destroy_response = CircuitDestroyResponse::new();
        circuit_destroy_response.set_circuit_name(circuit_name.into());

        let mut response_message = Message::new();
        response_message.set_message_type(MessageType::CIRCUIT_DESTROY_RESPONSE);

        if !(self
            .state
            .lock()
            .expect("Connection's Shared state lock was poisoned")
            .circuits
            .contains_key(circuit_name))
        {
            debug!(
                "Cannot destroy Circuit that does not exist: {}",
                &circuit_name
            );
            circuit_destroy_response
                .set_status(CircuitDestroyResponse_Status::CIRCUIT_DOES_NOT_EXIST);

            circuit_destroy_response.set_error_message(format!(
                "Cannot destroy Circuit that does not exist: {}",
                &circuit_name
            ));

            response_message.set_circuit_destroy_response(circuit_destroy_response);
            self.respond(response_message).map_err(|_| {
                RemoveCircuitError::SendError(format!(
                    "Unable to respond to CircuitDestroyRequest from {}",
                    &self.peer_addr
                ))
            })?;
        } else {
            circuit_destroy_response.set_status(CircuitDestroyResponse_Status::OK);
            // need to rebuild the message to forward
            let mut forward_msg = Message::new();
            forward_msg.set_message_type(MessageType::CIRCUIT_DESTROY_REQUEST);
            forward_msg.set_circuit_destroy_request(msg.clone());

            // Forward the destroy message to other nodes in the circuit
            let peers = if let Some(circuit) = self
                .state
                .lock()
                .expect("Connection's Shared state lock was poisoned")
                .circuits
                .get_mut(circuit_name)
            {
                circuit.get_peers()
            } else {
                HashMap::new()
            };

            for (_, addr) in peers {
                // if participant is this splinter node, skip forward
                if addr == self.network_addr {
                    continue;
                }

                self.direct_message(forward_msg.clone(), &addr, ConnectionType::Network)
                    .map_err(|_| {
                        RemoveCircuitError::SendError(format!(
                            "Unable to forward CircuitDestroyRequest to {}",
                            addr
                        ))
                    })?;
            }

            self.state
                .lock()
                .expect("Connection's Shared state lock was poisoned")
                .circuits
                .remove(circuit_name);

            response_message.set_circuit_destroy_response(circuit_destroy_response);
            self.respond(response_message).map_err(|_| {
                RemoveCircuitError::SendError(format!(
                    "Unable to respond to CircuitDestroyRequest from {}",
                    &self.peer_addr
                ))
            })?;
        }

        Ok(())
    }
}

impl<T: Connection> Drop for ConnectionDriver<T> {
    fn drop(&mut self) {
        match self.connection_type {
            ConnectionType::Network => {
                self.state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .peers
                    .remove(&self.peer_addr);
            }
            ConnectionType::Service => {
                self.state
                    .lock()
                    .expect("Connection's Shared state lock was poisoned")
                    .services
                    .remove(&self.peer_addr);
            }
        }
    }
}

// Loads the private key associated with a cert for creating the tls config
pub fn load_key(file_path: &str) -> Result<PrivateKey, SplinterError> {
    let keyfile = fs::File::open(file_path)?;
    let mut reader = BufReader::new(keyfile);
    let keys = rustls::internal::pemfile::pkcs8_private_keys(&mut reader)
        .map_err(|_| SplinterError::CertificateCreationError)?;

    if keys.len() < 1 {
        Err(SplinterError::PrivateKeyNotFound)
    } else {
        Ok(keys[0].clone())
    }
}

// Loads the certifcate that should be connected to a tls config
pub fn load_cert(file_path: &str) -> Result<Vec<Certificate>, SplinterError> {
    let certfile = fs::File::open(file_path)?;
    let mut reader = BufReader::new(certfile);

    rustls::internal::pemfile::certs(&mut reader)
        .map_err(|_| SplinterError::CertificateCreationError)
}

// Creates a Client config for tls communicating
pub fn create_client_config(
    ca_certs: Vec<Certificate>,
    client_certs: Vec<Certificate>,
    key: PrivateKey,
    cipher_suite: Vec<&'static SupportedCipherSuite>,
) -> Result<ClientConfig, SplinterError> {
    let mut config = rustls::ClientConfig::new();
    for cert in ca_certs {
        config.root_store.add(&cert)?;
    }
    config.set_single_client_cert(client_certs, key);
    config.ciphersuites = cipher_suite;

    Ok(config)
}

// Creates a Client Session from the ClientConfig and dns_name associated with the server to
// connect to
pub fn create_client_session(
    config: ClientConfig,
    dns_name: String,
) -> Result<ClientSession, SplinterError> {
    let dns_name = webpki::DNSNameRef::try_from_ascii_str(&dns_name)
        .map_err(|_| SplinterError::HostNameNotFound)?;

    Ok(ClientSession::new(&Arc::new(config), dns_name))
}

// Creates a Server config for tls communicating
pub fn create_server_config(
    ca_certs: Vec<Certificate>,
    server_certs: Vec<Certificate>,
    key: PrivateKey,
) -> Result<ServerConfig, SplinterError> {
    let mut client_auth_roots = rustls::RootCertStore::empty();
    for cert in ca_certs {
        client_auth_roots.add(&cert)?;
    }

    let auth = AllowAnyAuthenticatedClient::new(client_auth_roots);

    let mut config = ServerConfig::new(auth);
    config.key_log = Arc::new(rustls::KeyLogFile::new());
    config.set_single_cert(server_certs, key)?;

    Ok(config)
}

// Creates a Server Session from the ServerConfig
pub fn create_server_session(config: ServerConfig) -> ServerSession {
    ServerSession::new(&Arc::new(config))
}

pub fn pack_response(msg: &Message) -> Result<Vec<u8>, SplinterError> {
    let raw_msg = protobuf::Message::write_to_bytes(msg)?;
    let mut buff = Vec::new();

    buff.write_u32::<BigEndian>(raw_msg.len() as u32)?;
    buff.write(&raw_msg)?;

    Ok(buff)
}
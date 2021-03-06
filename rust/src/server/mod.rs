//! This module holds a netcode.io server implemenation and all of its related functions.

use std::net::{ToSocketAddrs, SocketAddr, UdpSocket};
use std::io;
#[cfg(test)]
use std::time::Duration;

use common::*;
use packet;
use token;
use crypto;

mod connection;
use server::connection::*;
use socket::*;
use error::*;
use channel::{self, Channel};

/// Errors from creating a server.
#[derive(Debug)]
pub enum CreateError {
    /// Address is already in use.
    AddrInUse,
    /// Address is not available(and probably already bound).
    AddrNotAvailable,
    /// Generic(other) io error occurred.
    GenericIo(io::Error)
}

impl From<io::Error> for CreateError {
    fn from(err: io::Error) -> CreateError {
        CreateError::GenericIo(err)
    }
}

pub type ClientId = u64;

/// Describes event the server receives when calling `next_event(..)`.
#[derive(Debug)]
pub enum ServerEvent {
    /// A client has connected, contains a reference to the client that was just created. `out_packet` contains private user data from token.
    ClientConnect(ClientId),
    /// A client has disconnected, contains the client that was just disconnected.
    ClientDisconnect(ClientId),
    /// Called when client tries to connect but all slots are full.
    ClientSlotFull,
    /// We received a packet, `out_packet` will be filled with data based on `usize`, contains the client id that reieved the packet and length of the packet.
    Packet(ClientId, usize),
    /// We received a keep alive packet with included client id.
    KeepAlive(ClientId),
    /// Client failed connection token validation
    RejectedClient,
    /// Replay detection heard duplicate packet and rejected it.
    ReplayRejected(ClientId)
}

/// UDP based netcode server.
pub type UdpServer = Server<UdpSocket,()>;

/// Netcode server object.
/// # Example
/// ```
/// use netcode::UdpServer;
/// use netcode::ServerEvent;
///
/// const PROTOCOL_ID: u64 = 0xFFEE;
/// const MAX_CLIENTS: usize = 32;
/// let private_key = netcode::generate_key();
/// let mut server = UdpServer::new("127.0.0.1:0", MAX_CLIENTS, PROTOCOL_ID, &private_key).unwrap();
///
/// //loop {
///     server.update(1.0 / 10.0);
///     let mut packet_data = [0; netcode::NETCODE_MAX_PAYLOAD_SIZE];
///     match server.next_event(&mut packet_data) {
///         Ok(Some(e)) => {
///             match e {
///                 ServerEvent::ClientConnect(_id) => {},
///                 ServerEvent::ClientDisconnect(_id) => {},
///                 ServerEvent::Packet(_id,_size) => {},
///                 _ => ()
///             }
///         },
///         Ok(None) => (),
///         Err(err) => Err(err).unwrap()
///     }
/// //}
/// ```
pub struct Server<I,S> {
    #[allow(dead_code)]
    socket_state: S,
    listen_socket: I,
    listen_addr: SocketAddr,
    protocol_id: u64,
    connect_key: [u8; NETCODE_KEY_BYTES],
    //@todo: We could probably use a free list or something smarter here if
    //we find that performance is an issue.
    clients: Vec<Option<Connection>>,
    time: f64,

    challenge_sequence: u64,
    challenge_key: [u8; NETCODE_KEY_BYTES],

    client_event_idx: usize,
}

enum TickResult {
    Noop,
    StateChange(ConnectionState),
    SendKeepAlive
}

impl<I,S> Server<I,S> where I: SocketProvider<I,S> {
    /// Constructs a new Server bound to `local_addr` with `max_clients` and supplied `private_key` for authentication.
    pub fn new<A>(local_addr: A, max_clients: usize, protocol_id: u64, private_key: &[u8; NETCODE_KEY_BYTES]) 
            -> Result<Server<I,S>, CreateError>
            where A: ToSocketAddrs {
        let bind_addr = local_addr.to_socket_addrs().unwrap().next().unwrap();
        let mut socket_state = I::new_state();
        match I::bind(&bind_addr, &mut socket_state) {
            Ok(s) => {
                let mut key_copy: [u8; NETCODE_KEY_BYTES] = [0; NETCODE_KEY_BYTES];
                key_copy.copy_from_slice(private_key);

                let mut clients = Vec::with_capacity(max_clients);
                for _ in 0..max_clients {
                    clients.push(None);
                }

                trace!("Started server on {:?}", s.local_addr().unwrap());

                Ok(Server {
                    socket_state: socket_state,
                    listen_socket: s,
                    listen_addr: bind_addr,
                    protocol_id: protocol_id,
                    connect_key: key_copy,
                    clients: clients,
                    time: 0.0,
                    challenge_sequence: 0,
                    challenge_key: crypto::generate_key(),
                    client_event_idx: 0,
                })
            },
            Err(e) => {
                match e.kind() {
                    io::ErrorKind::AddrInUse => Err(CreateError::AddrInUse),
                    io::ErrorKind::AddrNotAvailable => Err(CreateError::AddrNotAvailable),
                    _ => Err(CreateError::GenericIo(e))
                }
            }
        }
    }

    /// Gets the local port that this server is bound to.
    pub fn get_local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.listen_socket.local_addr()
    }

    /// Sends a packet to `client_id` specified.
    pub fn send(&mut self, client_id: ClientId, packet: &[u8]) -> Result<usize, SendError> {
        if packet.len() == 0 || packet.len() > NETCODE_MAX_PAYLOAD_SIZE {
            return Err(SendError::PacketSize)
        }

        trace!("Sending packet to {} with length {}", client_id, packet.len());

        self.send_packet(client_id, &packet::Packet::Payload(packet.len()), Some(packet))
    }

    /// Updates time elapsed since last server iteration.
    pub fn update(&mut self, elapsed: f64) -> Result<(), io::Error> {
        self.time += elapsed;
        self.client_event_idx = 0;

        Ok(())
    }
    
    /// Checks for incoming packets, client connection and disconnections. Returns `None` when no more events
    /// are pending.
    pub fn next_event(&mut self, out_packet: &mut [u8; NETCODE_MAX_PAYLOAD_SIZE]) -> Result<Option<ServerEvent>, UpdateError> {
        if out_packet.len() < NETCODE_MAX_PAYLOAD_SIZE {
            return Err(UpdateError::PacketBufferTooSmall)
        }

        loop {
            let mut scratch = [0; NETCODE_MAX_PACKET_SIZE];
            let result = match self.listen_socket.recv_from(&mut scratch) {
                Ok((len, addr)) => self.handle_io(&addr, &scratch[..len], out_packet),
                Err(e) => match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(None),
                    _ => Err(RecvError::SocketError(e).into())
                }
            };

            if let Ok(None) = result {
                break;
            } else {
                return result
            }
        }

        loop {
            if self.client_event_idx >= self.clients.len() {
                break;
            }

            let clients = &mut self.clients;
            let result = if let Some(client) = clients[self.client_event_idx].as_mut() {
                Self::tick_client(self.time, client, &mut self.listen_socket)?
            } else {
                TickResult::Noop
            };

            let event = match result {
                TickResult::Noop => None,
                TickResult::StateChange(state) => {
                    match state {
                        ConnectionState::TimedOut |
                        ConnectionState::Disconnected => {
                            let client_id = clients[self.client_event_idx].as_ref().map_or(0, |c| c.client_id);
                            clients[self.client_event_idx] = None;
                            trace!("Client disconnected {}", client_id);
                            Some(ServerEvent::ClientDisconnect(client_id))
                        },
                        _ => {
                            if let Some(client) = clients[self.client_event_idx].as_mut() {
                                client.state = state.clone();
                            }

                            None
                        }
                    }
                },
                TickResult::SendKeepAlive => {
                    let client_id = clients[self.client_event_idx].as_ref().map_or(0, |c| c.client_id);
                    Some(ServerEvent::KeepAlive(client_id))
                }
            };

            self.client_event_idx += 1;

            if event.is_some() {
                return event.map_or(Ok(None), |r| Ok(Some(r)))
            }
        }

        Ok(None)
    }

    fn handle_io(&mut self, addr: &SocketAddr, data: &[u8], out_packet: &mut [u8; NETCODE_MAX_PAYLOAD_SIZE]) -> Result<Option<ServerEvent>, UpdateError> {
        match self.find_client_by_addr(addr) {
            None => {
                trace!("New data on listening socket");

                match packet::decode(data, self.protocol_id, None, out_packet) {
                    Ok(packet) => match packet.1 {
                        packet::Packet::ConnectionRequest(req) => self.handle_client_connect(addr, &req),
                        packet => {
                            trace!("Expected Connection Request but got packet type {}", packet.get_type_id());
                            Ok(None)
                        }
                    },
                    Err(e) => {
                        trace!("Failed to decode connect packet: {:?}", e);
                        Ok(None)
                    }
                }
            },
            Some(client_idx) => {
                //Make sure we aren't still trying to connect
                trace!("New data on client socket {}", client_idx);
                self.handle_packet(client_idx, data, out_packet)
            }
        }
    }

    fn handle_client_connect(&mut self, addr: &SocketAddr, request: &packet::ConnectionRequestPacket) -> Result<Option<ServerEvent>, UpdateError> {
        if let Some(private_data) = Self::validate_client_token(self.protocol_id, &self.listen_addr, &self.connect_key, request) {
            //See if we already have this connection
            if let Some(_) = self.find_client_by_id(private_data.client_id) {
                trace!("Client already exists, skipping socket creation");
            } else {
                //Find open index
                match self.clients.iter().position(|v| v.is_none()) {
                    Some(idx) => {
                        let conn = Connection {
                            client_id: private_data.client_id,
                            state: ConnectionState::PendingResponse,
                            channel: Channel::new(&private_data.server_to_client_key, &private_data.client_to_server_key, addr, self.protocol_id, idx, self.clients.len())
                        };

                        trace!("Accepted connection {:?}", addr);
                        self.clients[idx] = Some(conn);
                    },
                    None => {
                        self.send_denied_packet(&addr, &private_data.server_to_client_key)?;
                        trace!("Tried to accept new client but max clients connected: {}", self.clients.len());
                        return Ok(Some(ServerEvent::ClientSlotFull))
                    }
                }
            }

            self.challenge_sequence += 1;

            trace!("Sending challenge packet");

            let challenge = packet::ChallengePacket::generate(
                private_data.client_id,
                &private_data.user_data,
                self.challenge_sequence,
                &self.challenge_key)?;

            //Send challenge token
            self.send_packet(private_data.client_id, &packet::Packet::Challenge(challenge), None)?;

            Ok(None)
        } else {
            trace!("Failed to accept client connection");
            Ok(Some(ServerEvent::RejectedClient))
        }
    }

    fn send_packet(&mut self, client_id: ClientId, packet: &packet::Packet, payload: Option<&[u8]>) -> Result<usize, SendError> {
        let socket = &mut self.listen_socket;
        let clients = &mut self.clients;

        //Get client idx
        let client = clients.iter().position(|v| v.as_ref().map_or(false, |ref c| c.client_id == client_id))
            //Then turn it into a reference
            .map(|idx| clients[idx].as_mut()).and_then(|c| c);

        if let Some(client) = client {
            return client.channel.send(self.time, packet, payload, socket)
        }

        trace!("Unable to send packet, invalid client id {}", client_id);

        Err(SendError::InvalidClientId)
   }

    fn send_denied_packet(&mut self, addr: &SocketAddr, key: &[u8; NETCODE_KEY_BYTES]) -> Result<(), SendError> {
        //id + sequence
        let mut packet = [0; 1 + 8];
        packet::encode(&mut packet[..], self.protocol_id, &packet::Packet::ConnectionDenied, Some((0, key)), None)?;

        self.listen_socket.send_to(&packet[..], addr).map_err(|e| e.into()).map(|_| ())
    }

    fn validate_client_token(
            protocol_id: u64,
            host: &SocketAddr,
            private_key: &[u8; NETCODE_KEY_BYTES],
            req: &packet::ConnectionRequestPacket) -> Option<token::PrivateData> {
        if req.version != *NETCODE_VERSION_STRING {
            trace!("Version mismatch expected {:?} but got {:?}", 
                NETCODE_VERSION_STRING, req.version);

            return None;
        }

        let now = token::get_time_now();
        if now > req.token_expire {
            trace!("Token expired: {} > {}", now, req.token_expire);
            return None;
        }

        if let Ok(v) = token::PrivateData::decode(&req.private_data, protocol_id, req.token_expire, req.sequence, private_key) {
            let has_host = v.hosts.get().any(|thost| {
                    thost == *host || (host.port() == 0 && thost.ip() == host.ip())
                });

            if !has_host {
                info!("Client connected but didn't contain host's address.");
                None
            } else {
                Some(v)
            }
        } else {
            info!("Unable to decode connection token");
            None
        }
   }

    fn tick_client(time: f64, client: &mut Connection, socket: &mut I) -> Result<TickResult, UpdateError> {
        let state = &client.state;
        let result = match *state {
            ConnectionState::PendingResponse => {
                match client.channel.update(time, socket, false)? {
                    channel::UpdateResult::Expired => TickResult::StateChange(ConnectionState::TimedOut),
                    channel::UpdateResult::SentKeepAlive => TickResult::SendKeepAlive,
                    channel::UpdateResult::Noop => TickResult::Noop
                }
            },
            ConnectionState::Idle => {
                match client.channel.update(time, socket, true)? {
                    channel::UpdateResult::Expired => TickResult::StateChange(ConnectionState::TimedOut),
                    channel::UpdateResult::SentKeepAlive => TickResult::SendKeepAlive,
                    channel::UpdateResult::Noop => TickResult::Noop
                }
            },
            ConnectionState::TimedOut | ConnectionState::Disconnected => TickResult::Noop
        };

        Ok(result)
    }

    fn handle_packet(&mut self,
            client_idx: usize,
            packet: &[u8],
            out_packet: &mut [u8; NETCODE_MAX_PAYLOAD_SIZE])
                -> Result<Option<ServerEvent>, UpdateError> {
        if packet.len() == 0 {
            return Ok(None)
        }

        trace!("Handling packet from client");
        let (client_id, mut state, addr, decoded) = if let Some(client) = self.clients[client_idx].as_mut() {
             let decoded = match client.channel.recv(self.time, packet, out_packet) {
                Ok(packet) => packet,
                Err(RecvError::DuplicateSequence) => return Ok(Some(ServerEvent::ReplayRejected(client.client_id))),
                Err(e) => {
                    info!("Failed to decode packet: {:?}", e);
                    client.state = ConnectionState::Disconnected;

                    return Ok(Some(ServerEvent::ClientDisconnect(client.client_id)))
                }
            };
 
            (client.client_id, client.state.clone(), client.channel.get_addr().clone(), decoded)
        } else {
            return Ok(None)
        };

        //Update client state with any recieved packet
        let event = match state {
            ConnectionState::Idle => {
                match decoded {
                    packet::Packet::Payload(len) => {
                        trace!("Received payload packet from {} with size {}", client_id, len);
                        Some(ServerEvent::Packet(client_id, len))
                    },
                    packet::Packet::KeepAlive(_) => {
                        Some(ServerEvent::KeepAlive(client_id))
                    },
                    packet::Packet::Disconnect => {
                        state = ConnectionState::Disconnected;
                        Some(ServerEvent::ClientDisconnect(client_id))
                    },
                    other => {
                        info!("Unexpected packet type {}", other.get_type_id());
                        None
                    }
                }
             },
            ConnectionState::PendingResponse => {
                match decoded {
                    packet::Packet::Response(resp) => {
                        let token = resp.decode(&self.challenge_key)?;
                        out_packet[..NETCODE_USER_DATA_BYTES].copy_from_slice(&token.user_data);

                        if let Some(client) = self.clients[client_idx].as_mut() {
                            client.channel.send_keep_alive(self.time, &mut self.listen_socket)?;
                        }

                        info!("client response");

                        state = ConnectionState::Idle;
                        Some(ServerEvent::ClientConnect(token.client_id))
                    },
                    packet::Packet::ConnectionRequest(req) => {
                        self.handle_client_connect(&addr, &req)?;
                        None
                    },
                    p => {
                        info!("Unexpected packet type when waiting for response {}", p.get_type_id());
                        None
                    }
                }
            },
            _ => None
        };

        trace!("state {:?}", &state);

        if let Some(client) = self.clients[client_idx].as_mut() {
            client.state = state;
        }

        Ok(event)
    }

    fn find_client_by_id(&self, id: ClientId) -> Option<usize> {
        self.clients.iter().position(|v| v.as_ref().map_or(false, |ref c| c.client_id == id))
    }

    fn find_client_by_addr(&self, addr: &SocketAddr) -> Option<usize> {
        self.clients.iter().position(|v| v.as_ref().map_or(false, |ref c| *c.channel.get_addr() == *addr))
    }

    #[cfg(test)]
    pub fn get_socket_state(&mut self) -> &mut S {
        &mut self.socket_state
    }

    #[cfg(test)]
    pub fn set_read_timeout(&mut self, duration: Option<Duration>) -> Result<(), io::Error> {
        self.listen_socket.set_recv_timeout(duration)
    }
}

#[cfg(test)]
mod test {
    use common::*;
    use packet::*;
    use token;
    use super::*;

    use socket::capi_simulator::*;

    use std::net::UdpSocket;

    const PROTOCOL_ID: u64 = 0xFFCC;
    const MAX_CLIENTS: usize = 256;
    const CLIENT_ID: u64 = 0xFFEEDD;

    struct TestHarness<I,S> where I: SocketProvider<I,S> {
        next_sequence: u64,
        server: Server<I,S>,
        private_key: [u8; NETCODE_KEY_BYTES],
        socket: I,
        connect_token: token::ConnectToken
    }

    impl<S,I> TestHarness<I,S> where I: SocketProvider<I,S> {
        pub fn new(port: Option<u16>) -> TestHarness<I,S> {
            let private_key = crypto::generate_key();

            let addr = format!("127.0.0.1:{}", port.unwrap_or(0));
            let mut server = Server::<I,S>::new(&addr, MAX_CLIENTS, PROTOCOL_ID, &private_key).unwrap();
            server.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
            let socket = I::bind(&Self::str_to_addr(&addr), server.get_socket_state()).unwrap();

            TestHarness {
                next_sequence: 0,
                server: server,
                private_key: private_key,
                socket: socket,
                connect_token: Self::generate_connect_token(&private_key, addr.as_str())
            }
        }

        fn str_to_addr(addr: &str) -> SocketAddr {
            use std::str::FromStr;
            SocketAddr::from_str(addr).unwrap()
        }

        pub fn generate_connect_token(private_key: &[u8; NETCODE_KEY_BYTES], addr: &str) -> token::ConnectToken {
            token::ConnectToken::generate(
                                [Self::str_to_addr(addr)].iter().cloned(),
                                private_key,
                                30, //Expire
                                0,
                                PROTOCOL_ID,
                                CLIENT_ID, //Client Id
                                None).unwrap()
        }

        pub fn replace_connect_token(&mut self, addr: &str, key: Option<&[u8; NETCODE_KEY_BYTES]>) {
            self.connect_token = Self::generate_connect_token(key.unwrap_or(&self.private_key), addr);
        }

        pub fn get_socket_state(&mut self) -> &mut S {
            self.server.get_socket_state()
        }

        pub fn get_connect_token(&mut self) -> &token::ConnectToken {
            &self.connect_token
        }

        fn get_next_sequence(&mut self) -> u64 {
            self.next_sequence += 1;
            self.next_sequence
        }

        pub fn send_connect_packet(&mut self) {
            let mut private_data = [0; NETCODE_CONNECT_TOKEN_PRIVATE_BYTES];
            private_data.copy_from_slice(&self.connect_token.private_data);

            let packet = Packet::ConnectionRequest(ConnectionRequestPacket {
                version: NETCODE_VERSION_STRING.clone(),
                protocol_id: PROTOCOL_ID,
                token_expire: self.connect_token.expire_utc,
                sequence: self.connect_token.sequence,
                private_data: private_data
            });

            let mut data = [0; NETCODE_MAX_PACKET_SIZE];
            let len = packet::encode(&mut data, PROTOCOL_ID, &packet, None, None).unwrap();
            self.socket.send_to(&data[..len], &self.server.get_local_addr().unwrap()).unwrap();
        }
        
        fn validate_challenge(&mut self) {
            let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
            self.server.update(0.0).unwrap();
            self.server.next_event(&mut data).unwrap();
        }

        fn read_challenge(&mut self) -> ChallengePacket {
            let mut data = [0; NETCODE_MAX_PACKET_SIZE];
            self.socket.set_recv_timeout(Some(Duration::from_secs(15))).unwrap();
            let (read, _) = self.socket.recv_from(&mut data).unwrap();

            let mut packet_data = [0; NETCODE_MAX_PAYLOAD_SIZE];
            match packet::decode(&data[..read], PROTOCOL_ID, Some(&self.connect_token.server_to_client_key), &mut packet_data).unwrap() {
                (_, Packet::Challenge(packet)) => {
                    packet
                },
                _ => {
                    assert!(false);
                    ChallengePacket {
                        token_sequence: 0,
                        token_data: [0; NETCODE_CHALLENGE_TOKEN_BYTES]
                    }
                }
            }
        }

        fn send_response(&mut self, token: ChallengePacket) {
            let packet = Packet::Response(ResponsePacket {
                token_sequence: token.token_sequence,
                token_data: token.token_data
            });

            let mut data = [0; NETCODE_MAX_PACKET_SIZE];
            let len = packet::encode(&mut data, PROTOCOL_ID, &packet, Some((self.get_next_sequence(), &self.connect_token.client_to_server_key)), None).unwrap();
            self.socket.send_to(&data[..len], &self.server.get_local_addr().unwrap()).unwrap();
        }

        fn validate_response(&mut self) {
            let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
            self.server.update(0.0).unwrap();
            let event = self.server.next_event(&mut data);

            match event {
                Ok(Some(ServerEvent::ClientConnect(CLIENT_ID))) => (),
                e => assert!(false, "{:?}", e)
            }

            let mut scratch = [0; NETCODE_MAX_PACKET_SIZE];
            let (keep_alive, _) = self.socket.recv_from(&mut scratch).unwrap();
            match packet::decode(&scratch[..keep_alive], PROTOCOL_ID, Some(&self.connect_token.server_to_client_key), &mut data).unwrap() {
                (_, Packet::KeepAlive(_)) => (),
                (_, p) => assert!(false, "{:?}", p.get_type_id())
            }
        }

        fn generate_payload_packet(&mut self, payload: &[u8]) -> (usize, [u8; NETCODE_MAX_PACKET_SIZE]) {
            let packet = Packet::Payload(payload.len());

            let mut data = [0; NETCODE_MAX_PACKET_SIZE];
            let len = packet::encode(&mut data, PROTOCOL_ID, &packet, Some((self.get_next_sequence(), &self.connect_token.client_to_server_key)), Some(payload)).unwrap();

            (len, data)
        }

        fn send_payload(&mut self, payload: &[u8]) {
            let (len, data) = self.generate_payload_packet(payload);
            self.socket.send_to(&data[..len], &self.server.get_local_addr().unwrap()).unwrap();
        }

        fn validate_recv_payload(&mut self, payload: &[u8]) {
            self.server.update(0.0).unwrap();
            let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];

            loop {
                match self.server.next_event(&mut data) {
                    Ok(Some(ServerEvent::Packet(cid, len))) => {
                        assert_eq!(cid, CLIENT_ID);
                        assert_eq!(payload.len(), len);
                        for i in 0..len {
                            assert_eq!(payload[i], data[i]);
                        }

                        break;
                    },
                    Ok(Some(ServerEvent::KeepAlive(cid))) => {
                        assert_eq!(cid, CLIENT_ID);
                    },
                    o => assert!(false, "unexpected {:?}", o)
                }
            }
        }

        fn validate_send_payload(&mut self, payload: &[u8]) {
            let mut data = [0; NETCODE_MAX_PACKET_SIZE];
            self.socket.set_recv_timeout(Some(Duration::from_secs(15))).unwrap();
            let (read,_) = self.socket.recv_from(&mut data).unwrap();

            let mut packet_data = [0; NETCODE_MAX_PAYLOAD_SIZE];
            match packet::decode(&data[..read], PROTOCOL_ID, Some(&self.connect_token.server_to_client_key), &mut packet_data) {
                Ok((sequence, Packet::Payload(len))) => {
                    assert_eq!(sequence, self.next_sequence);
                    assert_eq!(payload.len(), len);
                    for i in 0..len {
                        assert_eq!(packet_data[i], payload[i]);
                    }
                },
                Ok((_,p)) => assert!(false, "unexpected packet type {}", p.get_type_id()),
                Err(o) => assert!(false, "unexpected {:?}", o)
            }
 
        }
    }

    #[allow(dead_code)]
    fn enable_logging() {
        use env_logger::LogBuilder;
        use log::LogLevelFilter;

        LogBuilder::new().filter(None, LogLevelFilter::Trace).init().unwrap();

        use capi::*;
        unsafe {
            netcode_log_level(NETCODE_LOG_LEVEL_DEBUG as i32);
        }
    }

    #[test]
    fn test_connect_api() {
        let mut harness = TestHarness::<UdpSocket,()>::new(None);
        harness.send_connect_packet();
        harness.validate_challenge();
        let challenge = harness.read_challenge();
        harness.send_response(challenge);
        harness.validate_response();
    }

    #[test]
    fn test_connect_bad_host() {
        let mut harness = TestHarness::<UdpSocket,()>::new(None);
        let port = harness.server.get_local_addr().unwrap().port();
        harness.replace_connect_token(format!("0.0.0.0:{}", port).as_str(), None);
        harness.send_connect_packet();

        let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
        harness.server.update(0.0).unwrap();
        match harness.server.next_event(&mut data) {
            Ok(Some(ServerEvent::RejectedClient)) => {},
            _ => assert!(false)
        }
    }

    #[test]
    fn test_connect_bad_key() {
        let mut harness = TestHarness::<UdpSocket,()>::new(None);
        let port = harness.server.get_local_addr().unwrap().port();
        harness.replace_connect_token(format!("127.0.0.1:{}", port).as_str(), Some(&crypto::generate_key()));
        harness.send_connect_packet();

        let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
        harness.server.update(0.0).unwrap();
        match harness.server.next_event(&mut data) {
            Ok(Some(ServerEvent::RejectedClient)) => {},
            _ => assert!(false)
        }
    }

    #[test]
    fn test_replay_protection() {
        let mut harness = TestHarness::<UdpSocket,()>::new(None);
        harness.send_connect_packet();
        harness.validate_challenge();
        let challenge = harness.read_challenge();
        harness.send_response(challenge);
        harness.validate_response();

        let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
        for i in 0..NETCODE_MAX_PAYLOAD_SIZE {
            data[i] = i as u8;
        }

        let (plen, packet) = harness.generate_payload_packet(&data);
        harness.socket.send_to(&packet[..plen], harness.server.get_local_addr().unwrap()).unwrap();
        harness.validate_recv_payload(&data);

        harness.socket.send_to(&packet[..plen], harness.server.get_local_addr().unwrap()).unwrap();
        harness.server.update(0.0).unwrap();
        let mut scratch = [0; NETCODE_MAX_PAYLOAD_SIZE];
        match harness.server.next_event(&mut scratch) {
            Ok(Some(ServerEvent::ReplayRejected(cid))) => assert_eq!(cid, CLIENT_ID),
            o => assert!(false, "unexpected {:?}", o)
        }
    }

    #[test]
    fn test_payload() {
        let mut harness = TestHarness::<UdpSocket,()>::new(None);
        harness.send_connect_packet();
        harness.validate_challenge();
        let challenge = harness.read_challenge();
        harness.send_response(challenge);
        harness.validate_response();

        for s in 1..NETCODE_MAX_PAYLOAD_SIZE {
            let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
            for i in 0..s {
                data[i] = i as u8;
            }

            harness.send_payload(&data[..s]);
            harness.validate_recv_payload(&data[..s]);

            harness.server.send(CLIENT_ID, &data[..s]).unwrap();
            harness.validate_send_payload(&data[..s]);
        }
    }

    #[test]
    fn test_capi_payload() {
        #[allow(unused_variables)]
        let lock = ::common::test::FFI_LOCK.lock().unwrap();

        use capi::*;
        use std::ffi::CString;

        let mut harness = TestHarness::<SimulatedSocket, SimulatorRef>::new(Some(1235));

        unsafe {
            //Establish connection
            netcode_init();

            let addr = CString::new("127.0.0.1:1234").unwrap();
            let sim = harness.get_socket_state().clone();
            let client = netcode_client_create_internal(::std::mem::transmute(addr.as_ptr()), 0.0, sim.borrow_mut().sim);

            let mut connect_token = vec!();
            harness.get_connect_token().write(&mut connect_token).unwrap();

            netcode_client_connect(client, ::std::mem::transmute(connect_token.as_ptr()));

            let mut time = 0.0;
            loop {
                netcode_network_simulator_update(sim.borrow_mut().sim, time);

                harness.server.update(1.0 / 10.0).unwrap();
                loop {
                    let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
                    match harness.server.next_event(&mut data) {
                        Ok(None) => break,
                        Err(e) => assert!(false, "{:?}", e),
                        _ => (),
                    }
                }

                netcode_client_update(client, time);

                if netcode_client_state(client) <= NETCODE_CLIENT_STATE_DISCONNECTED as i32 {
                    break;
                }

                if netcode_client_state(client) == NETCODE_CLIENT_STATE_CONNECTED as i32 {
                    break;
                }

                time += 1.0 / 10.0;
            }

            assert_eq!(netcode_client_state(client), NETCODE_CLIENT_STATE_CONNECTED as i32);

            //Test payloads
            for s in 1..NETCODE_MAX_PAYLOAD_SIZE {
                let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
                for i in 0..s {
                    data[i] = i as u8;
                }

                netcode_client_send_packet(client, data.as_mut_ptr(), s as i32);
                netcode_network_simulator_update(sim.borrow_mut().sim, time);

                harness.validate_recv_payload(&data[..s]);

                harness.server.send(CLIENT_ID, &data[..s]).unwrap();

                netcode_network_simulator_update(sim.borrow_mut().sim, time);
                netcode_client_update(client, 1.0 / 10.0);

                let mut clen: i32 = 0;
                let cpacket = netcode_client_receive_packet(client, &mut clen, ::std::ptr::null_mut());

                assert!(cpacket != ::std::ptr::null_mut());
                assert_eq!(clen, s as i32);

                let cslice: &[u8] = ::std::slice::from_raw_parts(cpacket as *const u8, clen as usize);
                for i in 0..s {
                    assert_eq!(cslice[i], data[i]);
                }

                netcode_client_free_packet(client, cpacket);
            }

            netcode_client_destroy(client);

            netcode_term();
        }
    }

    #[test]
    fn test_capi_connect() {
        #[allow(unused_variables)]
        let lock = ::common::test::FFI_LOCK.lock().unwrap();

        use capi::*;
        use std::ffi::CString;

        let mut harness = TestHarness::<SimulatedSocket, SimulatorRef>::new(Some(1235));

        unsafe {
            netcode_init();

            let addr = CString::new("127.0.0.1:1234").unwrap();
            let sim = harness.get_socket_state().clone();
            let client = netcode_client_create_internal(::std::mem::transmute(addr.as_ptr()), 0.0, sim.borrow_mut().sim);

            let mut connect_token = vec!();
            harness.get_connect_token().write(&mut connect_token).unwrap();

            netcode_client_connect(client, ::std::mem::transmute(connect_token.as_ptr()));

            let mut time = 0.0;
            loop {
                netcode_network_simulator_update(sim.borrow_mut().sim, time);

                harness.server.update(1.0 / 10.0).unwrap();
                loop {
                    let mut data = [0; NETCODE_MAX_PAYLOAD_SIZE];
                    match harness.server.next_event(&mut data) {
                        Ok(None) => break,
                        Err(e) => assert!(false, "{:?}", e),
                        _ => (),
                    }
                }

                netcode_client_update(client, time);

                if netcode_client_state(client) <= NETCODE_CLIENT_STATE_DISCONNECTED as i32 {
                    break;
                }

                if netcode_client_state(client) == NETCODE_CLIENT_STATE_CONNECTED as i32 {
                    break;
                }

                time += 1.0 / 10.0;
            }

            assert_eq!(netcode_client_state(client), NETCODE_CLIENT_STATE_CONNECTED as i32);

            netcode_client_destroy(client);

            netcode_term();
       }
   }
}

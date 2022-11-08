use async_std::fs::write;
use bytes::{BytesMut, BufMut};
use tracing::log::logger;
use super::*;
use quinn_proto::{
    EcnCodepoint, 
    ConnectionHandle, 
    Connection, 
    DatagramEvent,
    EndpointEvent,
    ConnectionEvent, 
    Transmit, 
    Endpoint, 
    Streams, StreamId, SendStream, RecvStream, Datagrams, Dir, coding::BufExt,
};
use mio::net::{UdpSocket};
//use log::{info, warn};
use crate::{
    messaging::{DispatchEnvelope, EventEnvelope, NetMessage, SerialisedFrame},
    net::{
        buffers::{BufferChunk, BufferPool, EncodeBuffer},
        udp_state::UdpState,
        quic_config,
    },
    prelude::{NetworkStatus, SessionId},
    serialisation::ser_helpers::deserialise_chunk_lease,
};
use std::{
    collections::VecDeque,
    collections::HashMap,
    sync::{Arc},
    time::Instant, 
    net::{SocketAddr}, cell::RefCell,
};
use assert_matches::assert_matches;


const MAX_DATAGRAMS: usize = 10;
#[derive(Debug)]
pub struct QuicEndpoint {
    pub endpoint: Endpoint,
    pub addr: SocketAddr,
    timeout: Option<Instant>,
    pub accepted: Option<ConnectionHandle>,
    pub connections: HashMap<ConnectionHandle, Connection>,
    pub conn_events: HashMap<ConnectionHandle, VecDeque<ConnectionEvent>>,
}

impl QuicEndpoint {
    pub fn new(endpoint: Endpoint, 
                addr: SocketAddr, 
               // socket: UdpSocket
            ) -> Self {          
        Self {
            endpoint,
            addr: addr,
            timeout: None,
            //outbound: VecDeque::new(),
            //inbound: VecDeque::new(),
            accepted: None,
            connections: HashMap::default(),
            conn_events: HashMap::default(),
        }
    }
    pub fn conn_mut(&mut self, ch: ConnectionHandle) -> &mut Connection {
        return self.connections.get_mut(&ch).unwrap();
    }
    pub fn streams(&mut self, ch: ConnectionHandle) -> Streams<'_> {
        self.conn_mut(ch).streams()
    }
    pub fn send(&mut self, ch: ConnectionHandle, s: StreamId) -> SendStream<'_> {
        self.conn_mut(ch).send_stream(s)
    }
    pub fn recv(&mut self, ch: ConnectionHandle, s: StreamId) -> RecvStream<'_> {
        self.conn_mut(ch).recv_stream(s)
    }
    pub fn datagrams(&mut self, ch: ConnectionHandle) -> Datagrams<'_> {
        self.conn_mut(ch).datagrams()
    }

    /// start connecting the client
    pub(super) fn connect(&mut self, remote: SocketAddr) -> io::Result<ConnectionHandle> {
        println!("Initiating Connection to ... {}", remote);  
        let (client_ch, client_conn) = self
            .endpoint
            .connect(quic_config::client_config(), remote, "localhost")
            .unwrap();
        self.connections.insert(client_ch, client_conn);       
        
        Ok(client_ch)
    }

    pub(super) fn drive(&mut self, now: Instant, udp_state: &mut UdpState, addr: SocketAddr, n: usize){
        println!("OK in try read");
        let byte_data = udp_state.input_buffer.read_chunk_lease(n);
        let buffer = byte_data.content.to_vec();
        //buf.put_slice(bytes::BufMut);
        if let Some((ch, event)) =
            self.endpoint.handle(now, addr, None, None, buffer.as_slice().into())
        {
            match event {
                DatagramEvent::NewConnection(conn) => {
                    println!("New connection {:?}", ch);
                    self.connections.insert(ch, conn);
                    self.accepted = Some(ch);
                }
                DatagramEvent::ConnectionEvent(event) => {
                    println!("Redirect to existing connection {:?}", ch);
                    self.conn_events
                    .entry(ch)
                    .or_insert_with(VecDeque::new)
                    .push_back(event);
                }
            }
        }
        while let Some(x) = self.endpoint.poll_transmit() {
            println!("endpoint poll transmit in write {:?}", x.contents);
            udp_state.outbound_queue.push_back((x.destination, SerialisedFrame::Vec(x.contents)));
        }
        let mut endpoint_events: Vec<(ConnectionHandle, EndpointEvent)> = vec![];
        for (ch, conn) in self.connections.iter_mut() {
            println!("self connection {:?}", addr);

            if self.timeout.map_or(false, |x| x <= now) {
                println!("timeout {:?}", conn);
                self.timeout = None;
                conn.handle_timeout(now);
            }
            for (_, mut events) in self.conn_events.drain() {
                for event in events.drain(..) {
                    println!("drain connection events {:?}", addr);
                    conn.handle_event(event);
                }
            }

            //Return endpoint-facing events
            while let Some(event) = conn.poll_endpoint_events() {
                println!("poll endpoint events on conn  {:?}", ch);
                    endpoint_events.push((*ch, event))
            }

            //Returns packets to transmit
            while let Some(x) = conn.poll_transmit(now, MAX_DATAGRAMS) {
                println!("push to outbound queue in read {:?}", addr);
                udp_state.outbound_queue.push_back((addr, SerialisedFrame::Vec(x.contents)));

            }
            self.timeout = conn.poll_timeout();
        }
        for (ch, event) in endpoint_events {
            //Process ConnectionEvents generated by the associated Endpoint
            if let Some(event) = self.endpoint.handle_event(ch, event) {
                if let Some(conn) = self.connections.get_mut(&ch) {
                    println!("handle events  {:?}", event);
                    conn.handle_event(event);
                }
            }
        }
    }
    pub(super) fn try_read_quic(&mut self, now: Instant, udp_state: &mut UdpState, buffer_pool: &RefCell<BufferPool>) -> io::Result<()> { 
        //consume icoming packets and connection-generated events via handle and handle_event
        match udp_state.try_read(buffer_pool) {
            Ok((n, addr)) => {
                self.drive(now, udp_state, addr, n);
                return Ok(());
            }
            Err(err) => {
                 return Err(err);
            }
        }
    }

    pub(super) fn try_write_quic(&mut self, now: Instant, udp_state: &mut UdpState) -> io::Result<()>{
        //self.drive(now, udp_state, addr, 5000);
        while let Some(x) = self.endpoint.poll_transmit() {
            println!("endpoint poll transmit in write {:?}", x.contents);
            udp_state.outbound_queue.push_back((x.destination, SerialisedFrame::Vec(x.contents)));
        }
        
        for (ch, conn) in self.connections.iter_mut() {
            //Returns packets to transmit
            while let Some(x) = conn.poll_transmit(now, MAX_DATAGRAMS) {
                println!("push to outbound queue in read {:?}", x.destination);
                udp_state.outbound_queue.push_back((x.destination, SerialisedFrame::Vec(x.contents)));

            }
            self.timeout = conn.poll_timeout();
        }
        match udp_state.try_write() {
            Ok(_) => Ok({}),
            // Other errors we'll consider fatal.
            Err(err) => {
                return Err(err);
            }
        }
        }

    
}
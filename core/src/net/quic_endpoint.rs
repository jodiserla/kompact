use async_std::stream;
use bytes::{Bytes, Buf};
use super::*;
use quinn_proto::{
    ConnectionHandle,
    Connection,
    DatagramEvent,
    EndpointEvent,
    ConnectionEvent,
    Endpoint,
    Streams, SendStream, RecvStream, Datagrams, Dir, Event, StreamId,
};
use quinn_proto::StreamEvent::*;

use crate::{
    messaging::{DispatchEnvelope, EventEnvelope, NetMessage, SerialisedFrame},
    net::{
        buffers::{BufferPool},
        udp_state::UdpState,
        quic_config,
    },
    prelude::{NetworkStatus, SessionId},
};
use std::convert::TryInto;
use std::{
    collections::VecDeque,
    collections::HashMap,
    time::Instant,
    net::{SocketAddr}, cell::RefCell,
};

const MAX_DATAGRAMS: usize = 100;
#[derive(Debug)]
pub struct QuicEndpoint {
    logger: KompactLogger,
    pub endpoint: Endpoint,
    pub addr: SocketAddr,
    timeout: Option<Instant>,
    pub accepted: Option<ConnectionHandle>,
    pub connections: HashMap<ConnectionHandle, Connection>,
    pub connection_handle: Option<ConnectionHandle>,
    pub conn_events: HashMap<ConnectionHandle, VecDeque<ConnectionEvent>>,
    pub(super) incoming_messages: VecDeque<NetMessage>,
    pub stream_id: Option<StreamId>,

}

impl QuicEndpoint {
    pub fn new(endpoint: Endpoint,
                addr: SocketAddr,
                logger: KompactLogger,
            ) -> Self {
        Self {
            logger,
            endpoint,
            addr: addr,
            timeout: None,
            connection_handle: None,
            accepted: None,
            connections: HashMap::default(),
            conn_events: HashMap::default(),
            incoming_messages: VecDeque::new(),
            stream_id: None,
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

    pub fn send_stream_quic(&mut self, data: &[u8]) -> io::Result<()> {
       // info!(self.logger, "send_stream_quic function {:?}", self.connection_handle);

        if let Some(ch) = self.connection_handle.take() {  
          //  info!(self.logger, "within streamid send_stream_quic function");

            if let Some(stream_id) = self.stream_id.take() {
                match self.send(ch, stream_id).write(data) {
                    Ok(_) => {
                        // match self.send(ch, stream_id).finish() {
                        //     Ok(_) => {},
                        //     Err(err) => error!(self.logger, "not able to finish {:?}", err)
                        // }
                    }
                    Err(err) => {
                        error!(self.logger, "Unable to send stream {:?}", err);
                    },                
                }
                self.stream_id = Some(stream_id);
            } else {
                if let Some(stream_id) = self.streams(ch).open(Dir::Bi){
                    self.stream_id = Some(stream_id);

                    match self.send(ch, stream_id).write(data) {
                        Ok(_) => {
                           // info!(self.logger, "Open new stream");
                        }
                        Err(err) => {
                            error!(self.logger, "error {:?}", err);
                        },                
                    }              
                }
            }
            self.connection_handle = Some(ch);
        }
        return Ok(());
    }


    /// start connecting the client
    pub(super) fn connect(&mut self, remote: SocketAddr) -> io::Result<ConnectionHandle> {
        let (ch, client_conn) = self
            .endpoint
            .connect(quic_config::client_config(), remote, "localhost")
            .unwrap();
        self.connections.insert(ch, client_conn);
        self.connection_handle = Some(ch);
        Ok(ch)
    }

    pub(super) fn drive(&mut self, now: Instant, udp_state: &mut UdpState, dispatcher_ref: DispatcherRef){
        while let Some(x) = self.endpoint.poll_transmit() {
            info!(self.logger, "self.endpoint.poll_transmit()");
            udp_state.outbound_queue.push_back((x.destination, SerialisedFrame::Vec(x.contents)));
        }
        let mut endpoint_events: Vec<(ConnectionHandle, EndpointEvent)> = vec![];
         for (ch, conn) in self.connections.iter_mut() {
            if self.timeout.map_or(false, |x| x < now) {
                info!(self.logger, "conn.handle_timeout");
                self.timeout = None;
                conn.handle_timeout(now);
            }
           // 

            for (_, mut events) in self.conn_events.drain() {
                for event in events.drain(..) {
                    info!(self.logger, "conn.handle_event()");
                    conn.handle_event(event);
                }
            }

            //Return endpoint-facing events
            while let Some(event) = conn.poll_endpoint_events() {
                info!(self.logger, "conn.poll_endpoint_events()");
                endpoint_events.push((*ch, event));
            }

            while let Some(x) = conn.poll_transmit(now, MAX_DATAGRAMS) {
                info!(self.logger, "conn.poll_transmit()");
                udp_state.outbound_queue.push_back((x.destination, SerialisedFrame::Vec(x.contents)));
            }
            self.timeout = conn.poll_timeout();
        }

        for (ch, event) in endpoint_events {
            //Process ConnectionEvents generated by the associated Endpoint
            if let Some(event) = self.endpoint.handle_event(ch, event) {
                info!(self.logger, "self.endpoint.handle_event()");
                if let Some(conn) = self.connections.get_mut(&ch) {
                    info!(self.logger, "conn.handle_event from endpoint events");
                    conn.handle_event(event);
                }

            }

        }
        self.process_quic_events(dispatcher_ref.clone());

    }

    pub(super) fn try_read_quic(&mut self, now: Instant, udp_state: &mut UdpState, buffer_pool: &RefCell<BufferPool>, dispatcher_ref: DispatcherRef) -> io::Result<()> {
        //consume icoming packets and connection-generated events via handle and handle_event
       // info!(self.logger, "try_read_quic from quic_endpoint");
        match udp_state.try_read(buffer_pool) {
            Ok((n, addr)) => {
                let data = udp_state.input_buffer.read_chunk_lease(n).content.to_vec().as_slice().into();
                if let Some((ch, event)) =
                    self.endpoint.handle(now, addr, None, None, data)
                {
                    // We either accept new connection or redirect to existing connection
                    match event {
                        DatagramEvent::NewConnection(conn) => {
                           // info!(self.logger, "Accepting new connection from {}", &addr);
                            self.connections.insert(ch, conn);
                            self.connection_handle = Some(ch);
                        }
                        DatagramEvent::ConnectionEvent(event) => {
                           // info!(self.logger, "Redirect to existing connection {}", &addr);
                            self.conn_events
                            .entry(ch)
                            .or_insert_with(VecDeque::new)
                            .push_back(event);
                            self.connection_handle = Some(ch);
                        }
                    }
                }
                self.drive(now, udp_state, dispatcher_ref.clone());
               // self.process_quic_events(dispatcher_ref.clone(), udp_state);

                if !udp_state.outbound_queue.is_empty() {
                    match udp_state.try_write() {
                        Ok(_) => {
                        },
                        // Other errors we'll consider fatal.
                        Err(err) => {
                            error!(self.logger, "error in try_write_quic from read")
                        }
                    }
                }
                Ok(())
            }
            Err(err) => {
                return Err(err);
            }
        }
    }

    pub(super) fn try_write_quic(&mut self, now: Instant, udp_state: &mut UdpState, dispatcher_ref: DispatcherRef) -> io::Result<()>{
       // info!(self.logger, "try_write_quic from endpoint");
        self.drive(now, udp_state, dispatcher_ref.clone());

        match udp_state.try_write() {
            Ok(_) => {},
            // Other errors we'll consider fatal.
            Err(err) => {
                error!(self.logger, "error in try_write_quic")
            }
        };
        Ok(())
    }
 
    pub(super) fn process_quic_events(&mut self, dispatcher_ref: DispatcherRef) {
        info!(self.logger, "process_quic_events");
        if let Some(ch) = self.connection_handle.take() {  
            while let Some(event) = self.conn_mut(ch).poll() {
                match event {
                    Event::HandshakeDataReady => {
                        info!(self.logger, "Handshake Data Ready {:?}", ch);
                    }
                    Event::Connected => {
                        info!(self.logger, "Connected {:?}", ch);
                            dispatcher_ref.tell(DispatchEnvelope::Event(EventEnvelope::Network(NetworkStatus::ConnectionEstablished(
                                SystemPath::with_socket(Transport::Quic, self.conn_mut(ch).remote_address()),
                                SessionId::new_unique()))));
                    }
                    Event::ConnectionLost { reason } => {
                        info!(self.logger, "Lost connection due to {:?}", reason);
                        //TODO fix sessionid
                        dispatcher_ref.tell(DispatchEnvelope::Event(EventEnvelope::Network(NetworkStatus::ConnectionLost(
                            SystemPath::with_socket(Transport::Quic, self.conn_mut(ch).remote_address()),
                            SessionId::new_unique()))));
                    }
                    Event::Stream(Opened { dir: Dir::Bi}) => {
                        info!(self.logger, "Bidirectional stream openend {:?}", ch);
                        if let Some(id) = self.streams(ch).accept(Dir::Bi){
                            let mut inbound: VecDeque<Bytes> = VecDeque::new();
                            let mut receive = self.recv(ch, id);
                            let mut chunks = receive.read(false).unwrap();

                            match chunks.next(usize::MAX){
                            Ok(Some(chunk)) => {
                                    inbound.push_back(chunk.bytes);
                                }
                                Ok(None) => {
                                    println!("stream is finished");

                                }
                                Err(_) => todo!(),

                            }
                            chunks.finalize();

                            for mut byte in inbound.drain(..){
                                byte.advance(FRAME_HEAD_LEN as usize);
                                self.decode_quic_message(self.addr, byte);
                            }
                        }
                    }
                    Event::Stream(Opened { dir: Dir::Uni}) => {
                        // this will not be used 
                        info!(self.logger, "Unidirectional stream opened {:?}", ch);
                    }
                    Event::Stream(Readable { id }) => {
                        info!(self.logger, "Stream readable {:?} on id {:?}", ch, id);
                        let mut inbound: VecDeque<Bytes> = VecDeque::new();
                        let mut receive = self.recv(ch, id);
                        let mut chunks = receive.read(false).unwrap();

                        match chunks.next(usize::MAX){
                        Ok(Some(chunk)) => {
                                inbound.push_back(chunk.bytes);
                            }
                            Ok(None) => {
                                println!("stream is finished");

                            }
                            Err(_) => todo!(),

                        }
                        chunks.finalize();

                        for mut byte in inbound.drain(..){
                            byte.advance(FRAME_HEAD_LEN as usize);
                            self.decode_quic_message(self.addr, byte);
                        }
                    }
                    Event::Stream(Writable { id: StreamId(_) }) => {
                        info!(self.logger, "Stream writeable {:?}", ch);

                    }
                    Event::Stream(Finished { id: StreamId(_) }) => {
                        info!(self.logger, "Stream finished {:?}", ch);


                    }
                    Event::Stream(Stopped { id: StreamId(_), error_code: VarInt }) => {
                        info!(self.logger, "Stream stopped {:?}", ch);

                    }
                    Event::Stream(Available { dir: Dir::Bi }) => {
                        info!(self.logger, "Bidirectional stream available {:?}", ch);

                    }
                    Event::Stream(Available { dir: Dir::Uni }) => {
                        //TODO: we are not expecting uni direction for this implementation
                        info!(self.logger, "Unidirectional stream available {:?}", ch);

                    }
                    Event::DatagramReceived => {
                        //TODO: we are not expecting datagram for this implementation
                        info!(self.logger, "DatagramReceived {:?}", ch);

                    },
                }
            }
            self.connection_handle = Some(ch);
        }
        info!(self.logger, "None");
    }

    pub fn decode_quic_message(&mut self, source: SocketAddr, buf: Bytes) {
        match ser_helpers::deserialise_bytes(buf) {
            Ok(envelope) => {
                self.incoming_messages.push_back(envelope)
            }
            Err(e) => {
                print!(
                    "Could not deserialise Quic messages from {}: {}", source, e
                );
            }
        }
    }

}
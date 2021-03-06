use crate::dns_parser::{self, Name, QueryClass, QueryType, RRData};
use get_if_addrs::get_if_addrs;
use log::{debug, error, trace, warn};
use quick_error::quick_error;
use std::collections::VecDeque;
use std::io;
use std::io::ErrorKind::WouldBlock;
use std::marker::PhantomData;
use std::net::{IpAddr, SocketAddr};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::{net::UdpSocket, stream::Stream, sync::mpsc};

use super::{DEFAULT_TTL, MDNS_PORT};
use crate::address_family::AddressFamily;
use crate::services::{ServiceData, Services};

pub type AnswerBuilder = dns_parser::Builder<dns_parser::Answers>;

#[derive(Clone, Debug)]
pub enum Command {
    SendUnsolicited {
        svc: ServiceData,
        ttl: u32,
        include_ip: bool,
    },
    Shutdown,
}

quick_error! {
#[derive(Debug)]
pub enum Error {
    BufferTooSmall(bytes: usize, size: usize) {
        description("buffer isn't big enough")
        display("Incoming packet {}>{}", bytes, size)
    }
}
}

pub struct FSM<AF: AddressFamily> {
    socket: UdpSocket,
    services: Services,
    commands: mpsc::UnboundedReceiver<Command>,
    outgoing: VecDeque<(Vec<u8>, SocketAddr)>,
    _af: PhantomData<AF>,
}

impl<AF: AddressFamily> FSM<AF> {
    // Will panic if called from outside the context of a runtime
    pub fn new(services: &Services) -> io::Result<(FSM<AF>, mpsc::UnboundedSender<Command>)> {
        let std_socket = AF::bind()?;
        let socket = UdpSocket::from_std(std_socket)?;

        let (tx, rx) = mpsc::unbounded_channel();

        let fsm = FSM {
            socket: socket,
            services: services.clone(),
            commands: rx,
            outgoing: VecDeque::new(),
            _af: PhantomData,
        };

        Ok((fsm, tx))
    }

    fn recv_packets(&mut self, cx: &mut Context) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            let (bytes, addr) = match self.socket.poll_recv_from(cx, &mut buf) {
                Poll::Ready(Ok((bytes, addr))) => (bytes, addr),
                Poll::Ready(Err(err)) => return Err(err),
                Poll::Pending => break,
            };
            // Is moot for certain platforms (Windows will throw a <10040> error from poll_recv)
            if bytes >= buf.len() {
                warn!("buffer too small for packet from {:?}", addr);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    Error::BufferTooSmall(bytes, buf.len()),
                ));
            }
            self.handle_packet(&buf[..bytes], addr);
        }

        Ok(())
    }

    fn handle_packet(&mut self, buffer: &[u8], addr: SocketAddr) {
        trace!("received packet from {:?}", addr);

        let packet = match dns_parser::Packet::parse(buffer) {
            Ok(packet) => packet,
            Err(error) => {
                warn!("couldn't parse packet from {:?}: {}", addr, error);
                return;
            }
        };

        if !packet.header.query {
            trace!("received packet from {:?} with no query", addr);
            return;
        }

        if packet.header.truncated {
            warn!("dropping truncated packet from {:?}", addr);
            return;
        }

        let mut unicast_builder = dns_parser::Builder::new_response(packet.header.id, false, true)
            .move_to::<dns_parser::Answers>();
        let mut multicast_builder =
            dns_parser::Builder::new_response(packet.header.id, false, true)
                .move_to::<dns_parser::Answers>();
        unicast_builder.set_max_size(None);
        multicast_builder.set_max_size(None);

        for question in packet.questions {
            debug!(
                "received question: {:?} {}",
                question.qclass, question.qname
            );

            if question.qclass == QueryClass::IN || question.qclass == QueryClass::Any {
                if question.qu {
                    unicast_builder = self.handle_question(&question, unicast_builder);
                } else {
                    multicast_builder = self.handle_question(&question, multicast_builder);
                }
            }
        }

        if !multicast_builder.is_empty() {
            let response = multicast_builder.build().unwrap_or_else(|x| x);
            let addr = SocketAddr::new(AF::mdns_group(), MDNS_PORT);
            self.outgoing.push_back((response, addr));
        }

        if !unicast_builder.is_empty() {
            let response = unicast_builder.build().unwrap_or_else(|x| x);
            self.outgoing.push_back((response, addr));
        }
    }

    fn handle_question(
        &self,
        question: &dns_parser::Question,
        mut builder: AnswerBuilder,
    ) -> AnswerBuilder {
        let services = self.services.read().unwrap();

        match question.qtype {
            QueryType::A | QueryType::AAAA | QueryType::All
                if question.qname == *services.get_hostname() =>
            {
                builder = self.add_ip_rr(services.get_hostname(), builder, DEFAULT_TTL);
            }
            QueryType::PTR => {
                for svc in services.find_by_type(&question.qname) {
                    builder = svc.add_ptr_rr(builder, DEFAULT_TTL);
                    builder = svc.add_srv_rr(services.get_hostname(), builder, DEFAULT_TTL);
                    builder = svc.add_txt_rr(builder, DEFAULT_TTL);
                    builder = self.add_ip_rr(services.get_hostname(), builder, DEFAULT_TTL);
                }
            }
            QueryType::SRV => {
                if let Some(svc) = services.find_by_name(&question.qname) {
                    builder = svc.add_srv_rr(services.get_hostname(), builder, DEFAULT_TTL);
                    builder = self.add_ip_rr(services.get_hostname(), builder, DEFAULT_TTL);
                }
            }
            QueryType::TXT => {
                if let Some(svc) = services.find_by_name(&question.qname) {
                    builder = svc.add_txt_rr(builder, DEFAULT_TTL);
                }
            }
            _ => (),
        }

        builder
    }

    fn add_ip_rr(&self, hostname: &Name, mut builder: AnswerBuilder, ttl: u32) -> AnswerBuilder {
        let interfaces = match get_if_addrs() {
            Ok(interfaces) => interfaces,
            Err(err) => {
                error!("could not get list of interfaces: {}", err);
                return builder;
            }
        };

        for iface in interfaces {
            if iface.is_loopback() {
                continue;
            }

            trace!("found interface {:?}", iface);
            match iface.ip() {
                IpAddr::V4(ip) if !AF::v6() => {
                    builder = builder.add_answer(hostname, QueryClass::IN, ttl, &RRData::A(ip))
                }
                IpAddr::V6(ip) if AF::v6() => {
                    builder = builder.add_answer(hostname, QueryClass::IN, ttl, &RRData::AAAA(ip))
                }
                _ => (),
            }
        }

        builder
    }

    fn send_unsolicited(&mut self, svc: &ServiceData, ttl: u32, include_ip: bool) {
        let mut builder =
            dns_parser::Builder::new_response(0, false, true).move_to::<dns_parser::Answers>();
        builder.set_max_size(None);

        let services = self.services.read().unwrap();

        builder = svc.add_ptr_rr(builder, ttl);
        builder = svc.add_srv_rr(services.get_hostname(), builder, ttl);
        builder = svc.add_txt_rr(builder, ttl);
        if include_ip {
            builder = self.add_ip_rr(services.get_hostname(), builder, ttl);
        }

        if !builder.is_empty() {
            let response = builder.build().unwrap_or_else(|x| x);
            let addr = SocketAddr::new(AF::mdns_group(), MDNS_PORT);
            self.outgoing.push_back((response, addr));
        }
    }
}

impl<AF: Unpin + AddressFamily> Future for FSM<AF> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        let pinned = Pin::get_mut(self);
        while let Poll::Ready(cmd) = Pin::new(&mut pinned.commands).poll_next(cx) {
            match cmd {
                Some(Command::Shutdown) => return Poll::Ready(()),
                Some(Command::SendUnsolicited {
                    svc,
                    ttl,
                    include_ip,
                }) => {
                    pinned.send_unsolicited(&svc, ttl, include_ip);
                }
                None => {
                    warn!("responder disconnected without shutdown");
                    return Poll::Ready(());
                }
            }
        }

        match pinned.recv_packets(cx) {
            Ok(_) => (),
            Err(e) => error!("ResponderRecvPacket Error: {:?}", e),
        }

        println!("BEFORE: {}", pinned.outgoing.len());
        while let Some(&(ref response, ref addr)) = pinned.outgoing.front() {
            trace!("sending packet to {:?}", addr);

            match pinned.socket.poll_send_to(cx, response, addr) {
                Poll::Ready(Ok(v)) if v == response.len() => break,
                Poll::Ready(Ok(_)) => warn!("failed to send entire datagram"),
                Poll::Ready(Err(ref ioerr)) if ioerr.kind() == WouldBlock => break,
                Poll::Ready(Err(err)) => warn!("error sending packet {:?}", err),
                Poll::Pending => (break),
            }
        }
        pinned.outgoing.pop_front();
        println!("AFTER: {}", pinned.outgoing.len());

        Poll::Pending
    }
}

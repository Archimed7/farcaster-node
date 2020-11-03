// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use amplify::Bipolar;
use std::collections::HashMap;
use std::thread::spawn;

use lnpbp::bitcoin::secp256k1::rand::{self, Rng};
use lnpbp::lnp::{
    message, presentation, transport, zmqsocket, Messages, NodeAddr,
    PeerConnection, PeerSender, SendMessage, TypedEnum, ZmqType, ZMQ_CONTEXT,
};
use lnpbp_services::esb::{self, Handler};
use lnpbp_services::node::TryService;
use lnpbp_services::peer;

use crate::rpc::{Request, ServiceBus};
use crate::{Config, Error, LogStyle, Service, ServiceId};

pub struct MessageFilter {}

pub fn run(
    config: Config,
    connection: PeerConnection,
    id: NodeAddr,
    connect: bool,
) -> Result<(), Error> {
    debug!("Splitting connection into receiver and sender parts");
    let (receiver, sender) = connection.split();

    debug!("Opening bridge between runtime and peer listener threads");
    let tx = ZMQ_CONTEXT.socket(zmq::PAIR)?;
    let rx = ZMQ_CONTEXT.socket(zmq::PAIR)?;
    tx.connect("inproc://bridge")?;
    rx.bind("inproc://bridge")?;

    let identity = ServiceId::Peer(id);

    debug!("Starting thread listening for messages from the remote peer");
    let bridge_handler = ListenerRuntime {
        identity: identity.clone(),
        bridge: esb::Controller::with(
            map! {
                ServiceBus::Bridge => esb::BusConfig {
                    carrier: zmqsocket::Carrier::Socket(tx),
                    router: None,
                    queued: true,
                }
            },
            BridgeHandler,
            ZmqType::Rep,
        )?,
    };
    let listener = peer::Listener::with(receiver, bridge_handler);
    spawn(move || listener.run_or_panic("peerd-listener"));
    // TODO: Use the handle returned by spawn to track the child process

    debug!("Staring main service runtime");
    let runtime = Runtime {
        identity,
        routing: none!(),
        sender,
        connect,
        awaited_pong: None,
    };
    let mut service = Service::service(config, runtime)?;
    service.add_loopback(rx)?;
    service.run_loop()?;
    unreachable!()
}

pub struct BridgeHandler;

impl esb::Handler<ServiceBus> for BridgeHandler {
    type Request = Request;
    type Address = ServiceId;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        ServiceId::Loopback
    }

    fn handle(
        &mut self,
        _senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        _bus: ServiceBus,
        _addr: ServiceId,
        _request: Request,
    ) -> Result<(), Error> {
        // Bridge does not receive replies for now
        Ok(())
    }

    fn handle_err(&mut self, err: esb::Error) -> Result<(), esb::Error> {
        // We simply propagate the error since it's already being reported
        Err(err)?
    }
}

pub struct ListenerRuntime {
    identity: ServiceId,
    bridge: esb::Controller<ServiceBus, Request, BridgeHandler>,
}

impl ListenerRuntime {
    fn send_over_bridge(&mut self, req: Request) -> Result<(), Error> {
        debug!("Forwarding LNPWP message over BRIDGE interface to the runtime");
        self.bridge
            .send_to(ServiceBus::Bridge, self.identity.clone(), req)?;
        Ok(())
    }
}

impl peer::Handler for ListenerRuntime {
    type Error = crate::Error;

    fn handle(&mut self, message: Messages) -> Result<(), Self::Error> {
        // Forwarding all received messages to the runtime
        trace!("LNPWP message details: {:?}", message);
        self.send_over_bridge(Request::PeerMessage(message))
    }

    fn handle_err(&mut self, err: Self::Error) -> Result<(), Self::Error> {
        debug!("Underlying peer interface requested to handle {:?}", err);
        match err {
            Error::Peer(presentation::Error::Transport(
                transport::Error::TimedOut,
            )) => {
                trace!("Time to ping the remote peer");
                // This means socket reading timeout and the fact that we need
                // to send a ping message
                self.send_over_bridge(Request::PingPeer)
            }
            // for all other error types, indicating internal errors, we
            // propagate error to the upper level
            _ => {
                error!("Unrecoverable peer error {:?}, halting", err);
                Err(err)
            }
        }
    }
}

pub struct Runtime {
    identity: ServiceId,
    #[allow(dead_code)]
    routing: HashMap<ServiceId, MessageFilter>,
    sender: PeerSender,
    connect: bool,
    awaited_pong: Option<u16>,
}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = Request;
    type Address = ServiceId;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    fn on_ready(
        &mut self,
        _senders: &mut esb::SenderList<ServiceBus, ServiceId>,
    ) -> Result<(), Error> {
        if self.connect {
            info!("{} with the remote peer", "Initializing connection".promo());

            self.sender.send_message(Messages::Init(message::Init {
                global_features: none!(),
                local_features: none!(),
                assets: none!(),
                unknown_tlvs: none!(),
            }))?;

            self.connect = false;
        }
        Ok(())
    }

    fn handle(
        &mut self,
        senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        bus: ServiceBus,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Self::Error> {
        match bus {
            ServiceBus::Msg => self.handle_rpc_msg(senders, source, request),
            ServiceBus::Ctl => self.handle_rpc_ctl(senders, source, request),
            ServiceBus::Bridge => self.handle_bridge(senders, source, request),
        }
    }

    fn handle_err(&mut self, _: esb::Error) -> Result<(), esb::Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    fn handle_rpc_msg(
        &mut self,
        _senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        _source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match request {
            Request::PeerMessage(message) => {
                // 1. Check permissions
                // 2. Forward to the remote peer
                debug!("Forwarding LN peer message to the remote peer");
                self.sender.send_message(message)?;
            }
            _ => {
                error!(
                    "MSG RPC can be only used for forwarding LNPWP messages"
                );
                return Err(Error::NotSupported(
                    ServiceBus::Msg,
                    request.get_type(),
                ));
            }
        }
        Ok(())
    }

    fn handle_rpc_ctl(
        &mut self,
        _senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        _source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        match request {
            _ => {
                error!("Request is not supported by the CTL interface");
                return Err(Error::NotSupported(
                    ServiceBus::Ctl,
                    request.get_type(),
                ));
            }
        }
    }

    fn handle_bridge(
        &mut self,
        senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        _source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        debug!("BRIDGE RPC request: {}", request);
        match request {
            Request::PingPeer => {
                self.ping()?;
            }

            Request::PeerMessage(Messages::Ping(message::Ping {
                pong_size,
                ..
            })) => {
                self.pong(pong_size)?;
            }

            Request::PeerMessage(Messages::Pong(noise)) => {
                match self.awaited_pong {
                    None => error!("Unexpected pong from the remote peer"),
                    Some(len) if len as usize != noise.len() => warn!(
                        "Pong data size does not match requested with ping"
                    ),
                    _ => trace!("Got pong reply, exiting pong await mode"),
                }
                self.awaited_pong = None;
            }

            Request::PeerMessage(Messages::OpenChannel(_)) => {
                senders.send_to(
                    ServiceBus::Msg,
                    self.identity(),
                    ServiceId::Lnpd,
                    request,
                )?;
            }

            Request::PeerMessage(Messages::AcceptChannel(accept_channel)) => {
                senders.send_to(
                    ServiceBus::Msg,
                    self.identity(),
                    accept_channel.temporary_channel_id.into(),
                    Request::PeerMessage(Messages::AcceptChannel(
                        accept_channel,
                    )),
                )?;
            }

            Request::PeerMessage(message) => {
                // 1. Check permissions
                // 2. Forward to the corresponding daemon
                debug!("Got peer LNPWP message {}", message);
            }

            _ => {
                error!("Request is not supported by the BRIDGE interface");
                return Err(Error::NotSupported(
                    ServiceBus::Bridge,
                    request.get_type(),
                ))?;
            }
        }
        Ok(())
    }

    fn ping(&mut self) -> Result<(), Error> {
        trace!("Sending ping to the remote peer");
        if self.awaited_pong.is_some() {
            return Err(Error::NotResponding);
        }
        let mut rng = rand::thread_rng();
        let len: u16 = rng.gen_range(4, 32);
        let mut noise = vec![0u8; len as usize];
        for i in 0..noise.len() {
            noise[i] = rng.gen();
        }
        let pong_size = rng.gen_range(4, 32);
        self.sender.send_message(Messages::Ping(message::Ping {
            ignored: noise,
            pong_size,
        }))?;
        self.awaited_pong = Some(pong_size);
        Ok(())
    }

    fn pong(&mut self, pong_size: u16) -> Result<(), Error> {
        trace!("Replying with pong to the remote peer");
        let mut noise = vec![0u8; pong_size as usize];
        let mut rng = rand::thread_rng();
        for i in 0..noise.len() {
            noise[i] = rng.gen();
        }
        self.sender.send_message(Messages::Pong(noise))?;
        Ok(())
    }
}

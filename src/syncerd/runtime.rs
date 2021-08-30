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

use crate::syncerd::opts::Coin;
use crate::syncerd::monero_syncer::MoneroSyncer;
use crate::syncerd::bitcoin_syncer::BitcoinSyncer;
use crate::syncerd::bitcoin_syncer::Synclet;
use amplify::Wrapper;
use farcaster_core::blockchain::Network;
use farcaster_core::syncer::Abort;
use farcaster_core::syncer::{Syncer, Task};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::ffi::OsStr;
use std::io;
use std::net::SocketAddr;
use std::process;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use bitcoin::hashes::hex::ToHex;
use bitcoin::secp256k1;
use farcaster_core::swap::SwapId;
use internet2::{
    presentation, transport, zmqsocket, NodeAddr, RemoteSocketAddr, TypedEnum, ZmqType, ZMQ_CONTEXT,
};
use lnp::{message, Messages, TempChannelId as TempSwapId};
use lnpbp::Chain;
use microservices::esb::{self, Handler};
use microservices::rpc::Failure;

use crate::rpc::request::{IntoProgressOrFalure, OptionDetails, SyncerInfo};
use crate::rpc::{request, Request, ServiceBus};
use crate::{Config, Error, LogStyle, Service, ServiceId};

pub struct SyncerdTask {
    pub task: Task,
    pub source: ServiceId,
}

pub fn run(config: Config, coin: Coin) -> Result<(), Error> {
    info!("creating a new syncer");
    let syncer: Option<Box<dyn Synclet>>;
    let (tx, rx): (Sender<SyncerdTask>, Receiver<SyncerdTask>) = std::sync::mpsc::channel();

    let tx_event = ZMQ_CONTEXT.socket(zmq::PAIR)?;
    let rx_event = ZMQ_CONTEXT.socket(zmq::PAIR)?;
    tx_event.connect("inproc://syncerdbridge")?;
    rx_event.bind("inproc://syncerdbridge")?;

    match coin {
        Coin::Monero => {
            syncer = Some(Box::new(MoneroSyncer::new()));
        }
        Coin::Bitcoin => {
            match config.chain {
                Chain::Testnet3 => {
                    syncer = Some(Box::new(BitcoinSyncer::new()));
                }
                _ => {
                    syncer = none!();
                }
            }
        }
    }
    let mut runtime = Runtime {
        identity: ServiceId::Syncer,
        started: SystemTime::now(),
        tasks: none!(),
        syncer: syncer.unwrap(),
        tx,
    };
    runtime.syncer.run(rx, tx_event, runtime.identity().into());

    runtime.syncer.run(rx, tx_event, runtime.identity().into());
    let mut service = Service::service(config, runtime)?;
    service.add_loopback(rx_event)?;
    service.run_loop()?;
    unreachable!()
}

pub struct Runtime {
    identity: ServiceId,
    syncer: Box<dyn Synclet>,
    started: SystemTime,
    tasks: HashSet<u64>, // FIXME
    tx: Sender<SyncerdTask>,
    // spawning_services: HashMap<ServiceId, ServiceId>,
    // senders: HashMap<SwapId, &mut esb::SenderList<ServiceBus, ServiceId>>,
}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = Request;
    type Address = ServiceId;
    type Error = Error;

    fn identity(&self) -> ServiceId {
        self.identity.clone()
    }

    fn handle(
        &mut self,
        senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        bus: ServiceBus,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Self::Error> {
        // self.senders = senders;
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
            Request::Hello => {
                // Ignoring; this is used to set remote identity at ZMQ level
            }

            _ => {
                error!("MSG RPC can be only used for forwarding FWP messages");
                return Err(Error::NotSupported(ServiceBus::Msg, request.get_type()));
            }
        }
        Ok(())
    }
    fn handle_rpc_ctl(
        &mut self,
        senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        let mut notify_cli = None;
        match (&request, &source) {
            (Request::Hello, _) => {
                // Ignoring; this is used to set remote identity at ZMQ level
                info!(
                    "{} daemon is {}",
                    source.bright_green_bold(),
                    "connected".bright_green_bold()
                );
            }
            (Request::SyncerTask(task), _) => {
                self.tx
                    .send(SyncerdTask {
                        task: task.clone(),
                        source,
                    })
                    .unwrap();
            }
            (Request::GetInfo, _) => {
                senders.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Syncer,
                    source,
                    Request::SyncerInfo(SyncerInfo {
                        uptime: SystemTime::now()
                            .duration_since(self.started)
                            .unwrap_or(Duration::from_secs(0)),
                        since: self
                            .started
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or(Duration::from_secs(0))
                            .as_secs(),
                        tasks: self.tasks.iter().cloned().collect(),
                    }),
                )?;
            }

            (Request::ListTasks, ServiceId::Client(_)) => {
                senders.send_to(
                    ServiceBus::Ctl,
                    ServiceId::Syncer,
                    source.clone(),
                    Request::TaskList(self.tasks.iter().cloned().collect()),
                )?;
                let resp = Request::Progress(format!("ListedTasks?",));
                notify_cli = Some((Some(source), resp));
            }

            _ => {
                error!("{}", "Request is not supported by the CTL interface".err());
                return Err(Error::NotSupported(ServiceBus::Ctl, request.get_type()));
            }
        }

        if let Some((Some(respond_to), resp)) = notify_cli {
            senders.send_to(ServiceBus::Ctl, ServiceId::Syncer, respond_to, resp)?;
        }

        Ok(())
    }
    fn handle_bridge(
        &mut self,
        senders: &mut esb::SenderList<ServiceBus, ServiceId>,
        _source: ServiceId,
        request: Request,
    ) -> Result<(), Error> {
        debug!("Syncerd BRIDGE RPC request: {}", request);
        match request {
            Request::SyncerdBridgeEvent(syncerd_bridge_event) => {
                senders.send_to(
                    ServiceBus::Ctl,
                    self.identity(),
                    syncerd_bridge_event.source,
                    Request::SyncerEvent(syncerd_bridge_event.event),
                )?;
            }

            _ => {
                debug!("bridge request {:?} not handled here", request);
            }
        }
        Ok(())
    }
}

//
// Copyright (c) The Holo Core Contributors
//
// SPDX-License-Identifier: MIT
//

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]
#![feature(lazy_cell)]

mod netlink;
pub mod northbound;
mod rib;

use std::collections::BTreeMap;

use derive_new::new;
use holo_northbound::{
    process_northbound_msg, NbDaemonReceiver, NbDaemonSender, NbProviderSender,
    ProviderBase,
};
use holo_protocol::{event_recorder, spawn_protocol_task, InstanceShared};
use holo_utils::ibus::{IbusMsg, IbusReceiver, IbusSender};
use holo_utils::protocol::Protocol;
use holo_utils::sr::SrCfg;
use holo_utils::Database;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::rib::Rib;

pub struct Master {
    // Northbound Tx channel.
    pub nb_tx: NbProviderSender,
    // Internal bus Tx channel.
    pub ibus_tx: IbusSender,
    // Shared data among all protocol instances.
    pub shared: InstanceShared,
    // Event recorder configuration.
    pub event_recorder_config: event_recorder::Config,
    // Netlink socket.
    pub netlink_handle: rtnetlink::Handle,
    // RIB.
    pub rib: Rib,
    // SR configuration data.
    pub sr_config: SrCfg,
    // Protocol instances.
    pub instances: BTreeMap<InstanceId, NbDaemonSender>,
}

#[derive(Debug, Eq, Hash, PartialEq, PartialOrd, new, Ord)]
pub struct InstanceId {
    // Instance protocol.
    pub protocol: Protocol,
    // Instance name.
    pub name: String,
}

// ===== impl Master =====

impl Master {
    async fn run(
        &mut self,
        mut nb_rx: NbDaemonReceiver,
        mut ibus_rx: IbusReceiver,
    ) {
        let mut resources = vec![];

        loop {
            tokio::select! {
                Some(request) = nb_rx.recv() => {
                    process_northbound_msg(
                        self,
                        &mut resources,
                        request,
                    )
                    .await;
                }
                Ok(msg) = ibus_rx.recv() => {
                    process_ibus_msg(self, msg).await;
                }
                Some(_) = self.rib.update_queue_rx.recv() => {
                    self.rib
                        .process_rib_update_queue(
                            &self.netlink_handle,
                            &self.ibus_tx,
                        )
                        .await;
                }
            }
        }
    }
}

// ===== helper functions =====

async fn process_ibus_msg(master: &mut Master, msg: IbusMsg) {
    match msg {
        // Interface address addition notification.
        IbusMsg::InterfaceAddressAdd(msg) => {
            // Add connected route to the RIB.
            master.rib.connected_route_add(msg).await;
        }
        // Interface address delete notification.
        IbusMsg::InterfaceAddressDel(msg) => {
            // Remove connected route from the RIB.
            master.rib.connected_route_del(msg).await;
        }
        IbusMsg::KeychainUpd(keychain) => {
            // Update the local copy of the keychain.
            master
                .shared
                .keychains
                .insert(keychain.name.clone(), keychain.clone());
        }
        IbusMsg::KeychainDel(keychain_name) => {
            // Remove the local copy of the keychain.
            master.shared.keychains.remove(&keychain_name);
        }
        IbusMsg::PolicyMatchSetsUpd(match_sets) => {
            // Update the local copy of the policy match sets.
            master.shared.policy_match_sets = match_sets;
        }
        IbusMsg::PolicyUpd(policy) => {
            // Update the local copy of the policy definition.
            master
                .shared
                .policies
                .insert(policy.name.clone(), policy.clone());
        }
        IbusMsg::PolicyDel(policy_name) => {
            // Remove the local copy of the policy definition.
            master.shared.policies.remove(&policy_name);
        }
        IbusMsg::RouteIpAdd(msg) => {
            // Add route to the RIB.
            master.rib.ip_route_add(msg).await;
        }
        IbusMsg::RouteIpDel(msg) => {
            // Remove route from the RIB.
            master.rib.ip_route_del(msg).await;
        }
        IbusMsg::RouteMplsAdd(msg) => {
            // Add MPLS route to the LIB.
            master.rib.mpls_route_add(msg).await;
        }
        IbusMsg::RouteMplsDel(msg) => {
            // Remove MPLS route from the LIB.
            master.rib.mpls_route_del(msg).await;
        }
        // Ignore other events.
        _ => {}
    }
}

// ===== global functions =====

pub fn start(
    nb_tx: NbProviderSender,
    ibus_tx: IbusSender,
    ibus_rx: IbusReceiver,
    db: Database,
    event_recorder_config: event_recorder::Config,
) -> NbDaemonSender {
    let (nb_daemon_tx, nb_daemon_rx) = mpsc::channel(4);

    tokio::spawn(async move {
        let shared = InstanceShared {
            db: Some(db.clone()),
            ..Default::default()
        };
        let mut master = Master {
            nb_tx,
            ibus_tx,
            shared: shared.clone(),
            event_recorder_config,
            netlink_handle: netlink::init(),
            rib: Default::default(),
            sr_config: Default::default(),
            instances: Default::default(),
        };

        // Start BFD task.
        let name = "main".to_owned();
        let instance_id = InstanceId::new(Protocol::BFD, name.clone());
        let nb_daemon_tx = spawn_protocol_task::<holo_bfd::master::Master>(
            name,
            &master.nb_tx,
            &master.ibus_tx,
            Default::default(),
            shared,
            Some(master.event_recorder_config.clone()),
        );
        master.instances.insert(instance_id, nb_daemon_tx);

        // Run task main loop.
        let span = Master::debug_span("");
        master.run(nb_daemon_rx, ibus_rx).instrument(span).await;
    });

    nb_daemon_tx
}

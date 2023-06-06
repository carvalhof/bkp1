// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use crate::{
    inetstack::protocols::{
        ipv4::Ipv4Header,
        tcp::TcpPeer,
        tcp::established::ControlBlock,
    },
    runtime::{
        fail::Fail,
        memory::DemiBuffer,
        network::{
            config::{
                TcpConfig,
            },
            consts::RECEIVE_BATCH_SIZE,
        },
    },
};
use ::libc::ENOTCONN;
use ::std::{
    net::Ipv4Addr,
};

pub struct Peer<const N: usize> {
    local_ipv4_addr: Ipv4Addr,
    pub tcp: TcpPeer<N>,
}

impl<const N: usize> Peer<N> {
    pub fn new(
        local_ipv4_addr: Ipv4Addr,
        tcp_config: TcpConfig,
    ) -> Result<Self, Fail> {
        let tcp: TcpPeer<N> = TcpPeer::new(
            tcp_config,
        )?;

        Ok(Peer {
            local_ipv4_addr,
            tcp,
        })
    }

    pub fn receive(&mut self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, buf: DemiBuffer) -> Result<(), Fail> {
        let (header, payload) = Ipv4Header::parse(buf)?;
        debug!("Ipv4 received {:?}", header);
        if header.get_dest_addr() != self.local_ipv4_addr && !header.get_dest_addr().is_broadcast() {
            return Err(Fail::new(ENOTCONN, "invalid destination address"));
        }
        self.tcp.receive(cb, &header, payload)
    }
}
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::{
    constants::FALLBACK_MSS,
    established::ControlBlock,
    isn_generator::IsnGenerator,
};
use crate::{
    QDesc,
    inetstack::protocols::{
        ethernet2::{
            EtherType2,
            Ethernet2Header,
        },
        ip::IpProtocol,
        ipv4::Ipv4Header,
        tcp::{
            established::{
                congestion_control,
                congestion_control::CongestionControl,
            },
            segment::{
                TcpHeader,
                TcpOptions2,
                TcpSegment,
            },
            SeqNumber,
        },
    },
    runtime::{
        fail::Fail,
        network::{
            config::TcpConfig,
            NetworkRuntime,
        },
    },
};
use ::libc::{
    EBADMSG,
    ECONNREFUSED,
};
use ::std::{
    cell::RefCell,
    collections::{
        HashMap,
        HashSet,
        VecDeque,
    },
    convert::TryInto,
    net::SocketAddrV4,
    rc::Rc,
    sync::Arc,
    task::{
        Context,
        Poll,
        Waker,
    },
};

struct InflightAccept {
    local_isn: SeqNumber,
    remote_isn: SeqNumber,
    header_window_size: u16,
    remote_window_scale: Option<u8>,
    mss: usize,
}

struct ReadySockets<const N: usize> {
    ready: VecDeque<Result<ControlBlock<N>, Fail>>,
    endpoints: HashSet<SocketAddrV4>,
    waker: Option<Waker>,
}

impl<const N: usize> ReadySockets<N> {
    // fn push_ok(&mut self, cb: ControlBlock<N>) {
    //     assert!(self.endpoints.insert(cb.get_remote()));
    //     self.ready.push_back(Ok(cb));
    //     if let Some(w) = self.waker.take() {
    //         w.wake()
    //     }
    // }

    // fn push_err(&mut self, err: Fail) {
    //     self.ready.push_back(Err(err));
    //     if let Some(w) = self.waker.take() {
    //         w.wake()
    //     }
    // }

    fn poll(&mut self, ctx: &mut Context) -> Poll<Result<ControlBlock<N>, Fail>> {
        let r = match self.ready.pop_front() {
            Some(r) => r,
            None => {
                self.waker.replace(ctx.waker().clone());
                return Poll::Pending;
            },
        };
        if let Ok(ref cb) = r {
            assert!(self.endpoints.remove(&cb.get_remote()));
        }
        Poll::Ready(r)
    }

    fn len(&self) -> usize {
        self.ready.len()
    }
}

pub struct PassiveSocket<const N: usize> {
    inflight: HashMap<SocketAddrV4, InflightAccept>,
    ready: Rc<RefCell<ReadySockets<N>>>,

    max_backlog: usize,
    isn_generator: IsnGenerator,

    local: SocketAddrV4,
    rt: Option<Arc<dyn NetworkRuntime<N>>>,
    tcp_config: TcpConfig,
}

impl<const N: usize> PassiveSocket<N> {
    pub fn new(
        local: SocketAddrV4,
        max_backlog: usize,
        rt: Option<Arc<dyn NetworkRuntime<N>>>,
        tcp_config: TcpConfig,
        nonce: u32,
    ) -> Self {
        let ready = ReadySockets {
            ready: VecDeque::new(),
            endpoints: HashSet::new(),
            waker: None,
        };
        let ready = Rc::new(RefCell::new(ready));
        Self {
            inflight: HashMap::new(),
            ready,
            max_backlog,
            isn_generator: IsnGenerator::new(nonce),
            local,
            rt,
            tcp_config,
        }
    }

    /// Returns the address that the socket is bound to.
    pub fn endpoint(&self) -> SocketAddrV4 {
        self.local
    }

    pub fn poll_accept(&mut self, ctx: &mut Context) -> Poll<Result<ControlBlock<N>, Fail>> {
        self.ready.borrow_mut().poll(ctx)
    }

    pub fn receive(&mut self, qd: QDesc, eth_header: &Ethernet2Header, ip_header: &Ipv4Header, header: &TcpHeader) -> Result<Option<*mut ControlBlock<N>>, Fail> {
        let remote = SocketAddrV4::new(ip_header.get_src_addr(), header.src_port);
        if self.ready.borrow().endpoints.contains(&remote) {
            // TODO: What should we do if a packet shows up for a connection that hasn't been `accept`ed yet?
            return Ok(None);
        }
        let inflight_len = self.inflight.len();

        // If the packet is for an inflight connection, route it there.
        if self.inflight.contains_key(&remote) {
            if !header.ack {
                return Err(Fail::new(EBADMSG, "expeting ACK"));
            }
            debug!("Received ACK: {:?}", header);
            let &InflightAccept {
                local_isn,
                remote_isn,
                header_window_size,
                remote_window_scale,
                mss,
                ..
            } = self.inflight.get(&remote).unwrap();
            if header.ack_num != local_isn + SeqNumber::from(1) {
                return Err(Fail::new(EBADMSG, "invalid SYN+ACK seq num"));
            }

            let (local_window_scale, remote_window_scale) = match remote_window_scale {
                Some(w) => (self.tcp_config.get_window_scale() as u32, w),
                None => (0, 0),
            };
            let remote_window_size = (header_window_size)
                .checked_shl(remote_window_scale as u32)
                .expect("TODO: Window size overflow")
                .try_into()
                .expect("TODO: Window size overflow");
            let local_window_size = (self.tcp_config.get_receive_window_size() as u32)
                .checked_shl(local_window_scale as u32)
                .expect("TODO: Window size overflow");
            info!(
                "Window sizes: local {}, remote {}",
                local_window_size, remote_window_size
            );
            info!(
                "Window scale: local {}, remote {}",
                local_window_scale, remote_window_scale
            );

            self.inflight.remove(&remote);
            let cb = Box::into_raw(Box::new(ControlBlock::new(
                qd,
                self.local,
                remote,
                eth_header.dst_addr(),
                eth_header.src_addr(),
                self.tcp_config.clone(),
                remote_isn + SeqNumber::from(1),
                self.tcp_config.get_ack_delay_timeout(),
                local_window_size,
                local_window_scale,
                local_isn + SeqNumber::from(1),
                remote_window_size,
                remote_window_scale,
                mss,
                congestion_control::None::new,
                None,
            )));
            return Ok(Some(cb));
        }

        // Otherwise, start a new connection.
        if !header.syn || header.ack || header.rst {
            return Err(Fail::new(EBADMSG, "invalid flags"));
        }
        debug!("Received SYN: {:?}", header);
        if inflight_len + self.ready.borrow().len() >= self.max_backlog {
            // TODO: Should we send a RST here?
            return Err(Fail::new(ECONNREFUSED, "connection refused"));
        }
        let local_isn = self.isn_generator.generate(&self.local, &remote);
        let remote_isn = header.seq_num;

        let mut tcp_hdr = TcpHeader::new(header.dst_port, header.src_port);
        tcp_hdr.syn = true;
        tcp_hdr.seq_num = local_isn;
        tcp_hdr.ack = true;
        tcp_hdr.ack_num = remote_isn + SeqNumber::from(1);
        tcp_hdr.window_size = 0xffff as u16;

        let mss = 0xffff as u16;
        tcp_hdr.push_option(TcpOptions2::MaximumSegmentSize(mss));
        info!("Advertising MSS: {}", mss);

        tcp_hdr.push_option(TcpOptions2::WindowScale(0));
        info!("Advertising window scale: {}", 0);

        debug!("Sending SYN+ACK: {:?}", tcp_hdr);
        let segment = TcpSegment {
            ethernet2_hdr: Ethernet2Header::new(eth_header.src_addr(), eth_header.dst_addr(), EtherType2::Ipv4),
            ipv4_hdr: Ipv4Header::new(ip_header.get_dest_addr(), ip_header.get_src_addr(), IpProtocol::TCP),
            tcp_hdr,
            data: None,
            tx_checksum_offload: true,
        };
        
        match &self.rt {
            // Some(rt) => <DPDKRuntime as NetworkRuntime<N>>::transmit(&*rt, Box::new(segment)),
            Some(rt) => rt.transmit(Box::new(segment)),
            None => todo!(),
        }

        let mut remote_window_scale = None;
        let mut mss = FALLBACK_MSS;
        for option in header.iter_options() {
            match option {
                TcpOptions2::WindowScale(w) => {
                    info!("Received window scale: {:?}", w);
                    remote_window_scale = Some(*w);
                },
                TcpOptions2::MaximumSegmentSize(m) => {
                    info!("Received advertised MSS: {}", m);
                    mss = *m as usize;
                },
                _ => continue,
            }
        }
        let accept = InflightAccept {
            local_isn,
            remote_isn,
            header_window_size: header.window_size,
            remote_window_scale,
            mss,
        };
        self.inflight.insert(remote, accept);
        Ok(None)
    }
}

// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//==============================================================================
// Imports
//==============================================================================

use super::{
    // active_open::ActiveOpenSocket,
    established::EstablishedSocket,
    passive_open::PassiveSocket,
};
use crate::{
    inetstack::protocols::{
        ipv4::Ipv4Header,
        tcp::{
            established::ControlBlock,
            operations::{
                PopFuture,
                PushFuture,
            },
            segment::TcpHeader
        },
    },
    runtime::{
        fail::Fail,
        memory::DemiBuffer,
        network::{
            config::TcpConfig,
            consts::RECEIVE_BATCH_SIZE,
        },
        QDesc,
    },
};
use ::std::{
    cell::RefCell,
    net::{
        SocketAddrV4,
    },
    rc::Rc,
    task::{
        Context,
        Poll,
    },
    time::Duration,
};

#[cfg(feature = "profiler")]
use crate::timer;

//==============================================================================
// Enumerations
//==============================================================================

pub enum Socket<const N: usize> {
    Inactive(Option<SocketAddrV4>),
    Listening(PassiveSocket<N>),
    // Connecting(ActiveOpenSocket<N>),
    Established(EstablishedSocket<N>),
    Closing(EstablishedSocket<N>),
}

#[derive(PartialEq, Eq, Hash)]
pub enum SocketId {
    Active(SocketAddrV4, SocketAddrV4),
    Passive(SocketAddrV4),
}

//==============================================================================
// Structures
//==============================================================================

pub struct Inner<const N: usize> {
    tcp_config: TcpConfig,
}

pub struct TcpPeer<const N: usize> {
    pub(super) inner: Rc<RefCell<Inner<RECEIVE_BATCH_SIZE>>>,
}

//==============================================================================
// Associated Functions
//==============================================================================

impl<const N: usize> TcpPeer<N> {
    pub fn new(
        tcp_config: TcpConfig,
    ) -> Result<Self, Fail> {
        let inner = Rc::new(RefCell::new(Inner::new(
            tcp_config,
        )));
        Ok(Self { inner })
    }

    /// Opens a TCP socket.
    pub fn do_socket(&self) -> Result<QDesc, Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    pub fn bind(&self, _qd: QDesc, mut _addr: SocketAddrV4) -> Result<(), Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    pub fn receive(&self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, ip_header: &Ipv4Header, buf: DemiBuffer) -> Result<(), Fail> {
        self.inner.borrow().receive(cb, ip_header, buf)
    }

    // Marks the target socket as passive.
    pub fn listen(&self, _qd: QDesc, _backlog: usize) -> Result<(), Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    // /// Accepts an incoming connection.
    // pub fn do_accept(&self, qd: QDesc) -> (QDesc, AcceptFuture<N>) {
    //     let mut inner_: RefMut<Inner<N>> = self.inner.borrow_mut();
    //     let inner: &mut Inner<N> = &mut *inner_;

    //     let new_qd: QDesc = inner.qtable.borrow_mut().alloc(InetQueue::Tcp(TcpQueue::new()));
    //     (new_qd, AcceptFuture::new(qd, new_qd, self.inner.clone()))
    // }

    /// Handles an incoming connection.
    pub fn poll_accept(
        &self,
        _qd: QDesc,
        _new_qd: QDesc,
        _ctx: &mut Context,
    ) -> Poll<Result<(QDesc, SocketAddrV4), Fail>> {
        Poll::Ready(Err(Fail::new(libc::ENOTSUP, "Not supported")))
    }

    // pub fn connect(&self, _qd: QDesc, _remote: SocketAddrV4) -> Result<ConnectFuture<N>, Fail> {
    //     Err(Fail::new(libc::ENOTSUP, "Not supported"))
    // }

    pub fn poll_recv(&self, cb: *mut ControlBlock<N>, ctx: &mut Context, size: Option<usize>) -> Poll<Result<DemiBuffer, Fail>> {
        unsafe { (*cb).poll_recv(ctx, size) }
    }

    /// TODO: Should probably check for valid queue descriptor before we schedule the future
    pub fn push(&self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, buf: DemiBuffer) -> PushFuture<RECEIVE_BATCH_SIZE>{
        let err: Option<Fail> = match self.send(cb, buf) {
            Ok(()) => None,
            Err(e) => Some(e),
        };
        PushFuture { cb, err }
    }

    /// TODO: Should probably check for valid queue descriptor before we schedule the future
    pub fn pop(&self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, size: Option<usize>) -> PopFuture<RECEIVE_BATCH_SIZE> {
        PopFuture {
            cb,
            size,
            inner: self.inner.clone(),
        }
    }

    fn send(&self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, buf: DemiBuffer) -> Result<(), Fail> {
        unsafe { (*cb).send(buf) }
    }

    /// Closes a TCP socket.
    pub fn do_close(&self, _qd: QDesc) -> Result<(), Fail> {
        Err(Fail::new(libc::ENOTSUP, "Not supported"))
    }

    // /// Closes a TCP socket.
    // pub fn do_async_close(&self, _qd: QDesc) -> Result<CloseFuture<N>, Fail> {
    //     Err(Fail::new(libc::ENOTSUP, "Not supported"))
    // }

    pub fn remote_mss(&self, cb: *mut ControlBlock<N>) -> Result<usize, Fail> {
        unsafe { Ok((*cb).remote_mss()) }
    }

    pub fn current_rto(&self, cb: *mut ControlBlock<N>) -> Result<Duration, Fail> {
        unsafe { Ok((*cb).rto()) }
    }

    pub fn endpoints(&self,  cb: *mut ControlBlock<N>) -> Result<(SocketAddrV4, SocketAddrV4), Fail> {
        unsafe { Ok(((*cb).get_local(), (*cb).get_remote())) }
    }
}

impl<const N: usize> Inner<N> {
    fn new(
        tcp_config: TcpConfig,
    ) -> Self {
        Self {
            tcp_config,
        }
    }

    fn receive(&self, cb: *mut ControlBlock<RECEIVE_BATCH_SIZE>, ip_hdr: &Ipv4Header, buf: DemiBuffer) -> Result<(), Fail> {
        let (mut tcp_hdr, data) = TcpHeader::parse(ip_hdr, buf, self.tcp_config.get_rx_checksum_offload())?;
        debug!("TCP received {:?}", tcp_hdr);
        // let local = SocketAddrV4::new(ip_hdr.get_dest_addr(), tcp_hdr.dst_port);
        let remote = SocketAddrV4::new(ip_hdr.get_src_addr(), tcp_hdr.src_port);

        if remote.ip().is_broadcast() || remote.ip().is_multicast() || remote.ip().is_unspecified() {
            return Err(Fail::new(libc::EINVAL, "invalid address type"));
        }

        unsafe { (*cb).receive(&mut tcp_hdr, data) };

        Ok(())
    }
}

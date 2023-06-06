// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

mod background;
pub mod congestion_control;
mod ctrlblk;
mod rto;
mod sender;

pub use self::ctrlblk::{
    ControlBlock,
    State,
};

use crate::{
    inetstack::protocols::tcp::segment::TcpHeader,
    runtime::{
        fail::Fail,
        memory::DemiBuffer,
        QDesc,
    },
};
use ::std::{
    net::SocketAddrV4,
    task::{
        Context,
        Poll,
    },
    time::Duration,
};

#[derive(Clone)]
pub struct EstablishedSocket<const N: usize> {
    pub cb: *mut ControlBlock<N>,
}

impl<const N: usize> EstablishedSocket<N> {
    pub fn new(cb: *mut ControlBlock<N>, qd: QDesc) -> Self {
        let cb = cb;
        unsafe { (*cb).qd = qd };
        Self { cb, }
    }

    pub fn receive(&self, header: &mut TcpHeader, data: DemiBuffer) {
        unsafe { (*self.cb).receive(header, data) }
    }

    pub fn send(&self, buf: DemiBuffer) -> Result<(), Fail> {
        unsafe { (*self.cb).send(buf) } 
    }

    pub fn poll_recv(&self, ctx: &mut Context, size: Option<usize>) -> Poll<Result<DemiBuffer, Fail>> {
        unsafe { (*self.cb).poll_recv(ctx, size) } 
    }

    pub fn close(&self) -> Result<(), Fail> {
        unsafe { (*self.cb).close() } 
    }

    pub fn poll_close(&self) -> Poll<Result<(), Fail>> {
        unsafe { (*self.cb).poll_close() }
    }

    pub fn remote_mss(&self) -> usize {
        unsafe { (*self.cb).remote_mss() }
    }

    pub fn current_rto(&self) -> Duration {
        unsafe { (*self.cb).rto() }
    }

    pub fn endpoints(&self) -> (SocketAddrV4, SocketAddrV4) {
        unsafe { ((*self.cb).get_local(), (*self.cb).get_remote()) }
    }
}

use super::{
    tcp::queue::TcpQueue,
};
use crate::runtime::queue::{
    IoQueue,
    QType,
};

/// Per-queue metadata: Inet stack Control Block
pub enum InetQueue<const N: usize> {
    Tcp(TcpQueue<N>),
}

impl<const N: usize> IoQueue for InetQueue<N> {
    fn get_qtype(&self) -> QType {
        match self {
            Self::Tcp(_) => QType::TcpSocket,
        }
    }
}

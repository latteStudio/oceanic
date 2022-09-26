use core::{
    iter::FusedIterator,
    sync::atomic::{AtomicBool, Ordering::*},
    time::Duration,
};

use solvent::prelude::{Channel, Object, Packet, EPIPE, SIG_READ};
use solvent_std::sync::Arsc;

use crate::Error;

#[repr(transparent)]
pub struct Server {
    inner: Arsc<Inner>,
}

impl Server {
    pub fn new(channel: Channel) -> Self {
        Server {
            inner: Arsc::new(Inner {
                channel,
                stop: AtomicBool::new(false),
            }),
        }
    }

    #[inline]
    pub fn serve(self, timeout: Option<Duration>) -> (PacketIter, EventSender) {
        (
            PacketIter {
                inner: self.inner.clone(),
                timeout: timeout.unwrap_or(Duration::MAX),
            },
            EventSender { inner: self.inner },
        )
    }
}

impl AsRef<Channel> for Server {
    #[inline]
    fn as_ref(&self) -> &Channel {
        &self.inner.channel
    }
}

impl From<Channel> for Server {
    #[inline]
    fn from(channel: Channel) -> Self {
        Self::new(channel)
    }
}

impl TryFrom<Server> for Channel {
    type Error = Server;

    fn try_from(server: Server) -> Result<Self, Self::Error> {
        match Arsc::try_unwrap(server.inner) {
            Ok(mut inner) => {
                if !*inner.stop.get_mut() {
                    Ok(inner.channel)
                } else {
                    Err(Server {
                        inner: Arsc::new(inner),
                    })
                }
            }
            Err(inner) => Err(Server { inner }),
        }
    }
}

pub struct Request {
    pub packet: Packet,
    pub responder: Responder,
}

pub struct PacketIter {
    inner: Arsc<Inner>,
    timeout: Duration,
}

impl Iterator for PacketIter {
    type Item = Result<Request, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.inner.stop.load(Acquire) {
            return None;
        }

        match self.inner.receive(self.timeout) {
            Err(Error::Disconnected) => None,
            res => Some(res.map(|packet| Request {
                packet,
                responder: Responder(EventSender {
                    inner: self.inner.clone(),
                }),
            })),
        }
    }
}

impl FusedIterator for PacketIter {}

#[repr(transparent)]
pub struct EventSender {
    inner: Arsc<Inner>,
}

impl EventSender {
    #[inline]
    pub fn send(&self, packet: Packet) -> Result<(), Error> {
        if self.inner.stop.load(Acquire) {
            return Err(Error::Disconnected);
        }
        self.inner.send(packet)
    }

    #[inline]
    pub fn close(self) {
        self.inner.stop.store(true, Release);
    }
}

#[repr(transparent)]
pub struct Responder(EventSender);

impl Responder {
    #[inline]
    pub fn send(self, packet: Packet, close: bool) -> Result<(), Error> {
        let ret = self.0.send(packet);
        if close {
            self.0.close();
        }
        ret
    }

    #[inline]
    pub fn close(self) {
        self.0.close()
    }
}

struct Inner {
    channel: Channel,
    stop: AtomicBool,
}

impl Inner {
    fn receive(&self, timeout: Duration) -> Result<Packet, Error> {
        let mut packet = Default::default();
        let res = self.channel.try_wait(timeout, false, SIG_READ);
        let res = res.and_then(|_| self.channel.receive(&mut packet));
        res.map_err(|err| {
            if err == EPIPE {
                self.stop.store(true, Release);
                Error::Disconnected
            } else {
                Error::ServerReceive(err)
            }
        })?;
        Ok(packet)
    }

    fn send(&self, mut packet: Packet) -> Result<(), Error> {
        let res = self.channel.send(&mut packet);
        res.map_err(|err| {
            if err == EPIPE {
                self.stop.store(true, Release);
                Error::Disconnected
            } else {
                Error::ServerSend(err)
            }
        })?;
        Ok(())
    }
}

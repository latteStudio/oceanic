use core::{future::Future, marker::PhantomData};

use futures_lite::StreamExt;
use solvent::prelude::Channel;
use solvent_async::ipc::Channel as AsyncChannel;
use solvent_core::{path::Path, sync::Arsc};
use solvent_rpc::{
    io::{
        entry::{EntryRequest, EntryServer},
        Error, FileType, Metadata, OpenOptions, Permission,
    },
    Server,
};

use crate::{dir::EventTokens, entry::Entry, spawn::Spawner};

pub struct RpcNode<S, G, F>
where
    S: Server + Send + Sync + 'static,
    G: Fn(S, Spawner) -> F + Sync + Send + 'static,
    F: Future<Output = ()> + Sync + Send + 'static,
{
    gen: G,
    _marker: PhantomData<S>,
}

impl<S, G, F> RpcNode<S, G, F>
where
    S: Server + Send + Sync + 'static,
    G: Fn(S, Spawner) -> F + Sync + Send + 'static,
    F: Future<Output = ()> + Sync + Send + 'static,
{
    pub fn new(func: G) -> Arsc<Self> {
        Arsc::new(RpcNode {
            gen: func,
            _marker: PhantomData,
        })
    }

    pub fn open_conn(self: Arsc<Self>, spawner: Spawner, tokens: EventTokens, conn: Channel) {
        let server = EntryServer::from(AsyncChannel::with_disp(conn, spawner.dispatch()));
        let task = handle_rpc(self, spawner.clone(), tokens, server);
        spawner.spawn(task)
    }
}

impl<S, G, F> Entry for RpcNode<S, G, F>
where
    S: Server + Send + Sync + 'static,
    G: Fn(S, Spawner) -> F + Sync + Send + 'static,
    F: Future<Output = ()> + Sync + Send + 'static,
{
    fn open(
        self: Arsc<Self>,
        spawner: Spawner,
        _: EventTokens,
        path: &Path,
        options: OpenOptions,
        conn: Channel,
    ) -> Result<bool, Error> {
        if options - OpenOptions::EXPECT_RPC != OpenOptions::READ | OpenOptions::WRITE {
            return Err(Error::PermissionDenied(options.require()));
        }
        if path != Path::new("") {
            return Err(Error::InvalidPath(path.into()));
        }
        let server = S::from(AsyncChannel::with_disp(conn, spawner.dispatch()));
        let task = (self.gen)(server, spawner.clone());
        spawner.spawn(task);
        Ok(false)
    }

    fn metadata(&self) -> Result<Metadata, Error> {
        Ok(Metadata {
            file_type: FileType::RpcNode,
            perm: Permission::READ | Permission::WRITE,
            len: 0,
        })
    }
}

pub async fn handle_rpc<S, G, F>(
    node: Arsc<RpcNode<S, G, F>>,
    spawner: Spawner,
    tokens: EventTokens,
    server: EntryServer,
) where
    S: Server + Send + Sync + 'static,
    G: Fn(S, Spawner) -> F + Sync + Send + 'static,
    F: Future<Output = ()> + Sync + Send + 'static,
{
    let (mut stream, _) = server.serve();

    while let Some(request) = stream.next().await {
        let request = match request {
            Ok(request) => request,
            Err(err) => {
                log::warn!("RPC receive error: {err}");
                continue;
            }
        };

        let res = match request {
            EntryRequest::CloseConnection { responder } => {
                responder.close();
                break;
            }
            EntryRequest::Open {
                path,
                options,
                conn,
                responder,
            } => responder.send(
                node.clone()
                    .open(spawner.clone(), tokens.clone(), &path, options, conn)
                    .map(drop),
            ),
            EntryRequest::CloneConnection { conn, responder } => {
                node.clone()
                    .open_conn(spawner.clone(), tokens.clone(), conn);
                responder.send(())
            }
            EntryRequest::Metadata { responder } => responder.send(node.metadata()),
            EntryRequest::Unknown(_) => {
                log::warn!("unknown request received");
                continue;
            }
        };

        if let Err(err) = res {
            log::warn!("RPC send error: {err}")
        }
    }
}

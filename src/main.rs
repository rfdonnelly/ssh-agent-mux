//! Run with
//! RUST_LOG=info cargo run -- --target unix://$SSH_AUTH_SOCK --target unix://$SSH_AUTH_SOCK --host unix:///tmp/test.sock
//!
//! Then
//! SSH_AUTH_SOCK=/tmp/test.sock ssh-add -l
//! SSH_AUTH_SOCK=/tmp/test.sock ssh <host>

use clap::Parser;
use futures::future::join_all;
use service_binding::Binding;
use ssh_agent_lib::{
    agent::bind,
    agent::Agent,
    agent::Session,
    async_trait,
    client::connect,
    error::AgentError,
    proto::{Extension, Identity, Request, Response, SignRequest},
};
use ssh_key::Signature;
use std::time::Instant;

struct IndexedIdentity {
    identity: Identity,
    index: usize,
}

struct MuxAgent {
    targets: Vec<Box<dyn Session>>,
    identity_cache: Vec<IndexedIdentity>,
    identity_cache_instant: Option<Instant>,
}

const CACHE_INVALIDATE_TIME: u64 = 1;

impl MuxAgent {
    fn new(targets: Vec<Box<dyn Session>>) -> Self {
        Self {
            targets,
            identity_cache: Vec::new(),
            identity_cache_instant: None,
        }
    }

    async fn update_cache(&mut self) -> Result<(), AgentError> {
        if self.identity_cache_instant.is_some_and(|instant| instant.elapsed().as_secs() <= CACHE_INVALIDATE_TIME) {
            return Ok(())
        }

        let responses = join_all(
            self.targets
            .iter_mut()
            .map(|target| target.request_identities()),
            )
            .await;
        let responses: Result<Vec<_>, _> = responses.into_iter().collect();
        let indexed_identities = responses?.into_iter().enumerate().flat_map(|(index, identities)| {
            identities.into_iter().map(move |identity| IndexedIdentity { identity, index })
        }).collect();

        self.identity_cache_instant = Some(Instant::now());
        self.identity_cache = indexed_identities;

        Ok(())
    }
}

#[async_trait]
impl Session for MuxAgent {
    async fn request_identities(&mut self) -> Result<Vec<Identity>, AgentError> {
        self.update_cache().await?;
        let identities = self.identity_cache.drain(0..).map(|indexed_identity| indexed_identity.identity).collect();
        self.identity_cache_instant = None;
        Ok(identities)
    }

    async fn sign(&mut self, request: SignRequest) -> Result<Signature, AgentError> {
        // TODO: demux based on key
        log::info!("sign request {request:?}");
        let response = self.targets.first_mut().unwrap().sign(request).await?;
        log::info!("sign response {response:?}");
        Ok(response)
    }

    async fn extension(&mut self, request: Extension) -> Result<Option<Extension>, AgentError> {
        log::info!("extension request {request:?}");
        let response = self.targets.first_mut().unwrap().extension(request).await.unwrap_or(None);
        log::info!("extension response {response:?}");
        Ok(response)
    }

    // async fn handle(&mut self, message: Request) -> Result<Response, AgentError> {
    //     log::info!("handle request {message:?}");
    //     match message {
    //         _ => {
    //             let response = self.targets.first_mut().unwrap().handle(message).await?;
    //             log::info!("handle response {response:?}");
    //             Ok(response)
    //         }
    //     }
    // }
}

struct MuxAgentBind {
    targets: Vec<Binding>,
}

#[cfg(unix)]
impl Agent<tokio::net::UnixListener> for MuxAgentBind {
    fn new_session(&mut self, _socket: &tokio::net::UnixStream) -> impl Session {
        self.create_new_session()
    }
}

impl Agent<tokio::net::TcpListener> for MuxAgentBind {
    fn new_session(&mut self, _socket: &tokio::net::TcpStream) -> impl Session {
        self.create_new_session()
    }
}

#[cfg(windows)]
impl Agent<ssh_agent_lib::agent::NamedPipeListener> for MuxAgentBind {
    fn new_session(
        &mut self,
        _socket: &tokio::net::windows::named_pipe::NamedPipeServer,
    ) -> impl Session {
        self.create_new_session()
    }
}

impl MuxAgentBind {
    fn create_new_session(&mut self) -> impl Session {
        let targets = self
            .targets
            .iter()
            .map(|target| connect(target.clone().try_into().unwrap()).unwrap())
            .collect();
        MuxAgent::new(targets)
    }
}

#[derive(Debug, Parser)]
struct Args {
    /// Target SSH agent to which we will proxy all requests.
    #[clap(long="target", num_args=1..)]
    targets: Vec<Binding>,

    /// Source that we will bind to.
    #[clap(long)]
    host: Binding,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = Args::parse();

    bind(
        args.host.try_into()?,
        MuxAgentBind {
            targets: args.targets,
        },
    )
    .await?;

    Ok(())
}

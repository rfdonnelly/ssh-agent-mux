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
    proto::{Extension, Identity, SignRequest},
    // proto::{Request, Response},
};
use ssh_key::{public::KeyData, Signature};

struct IdentityIndex {
    identity: Identity,
    target_index: usize,
}

struct KeyIndex {
    key: KeyData,
    target_index: usize,
}

struct MuxAgent {
    targets: Vec<Box<dyn Session>>,
    key_target_map: Vec<KeyIndex>,
}

impl MuxAgent {
    fn new(targets: Vec<Box<dyn Session>>) -> Self {
        Self {
            targets,
            key_target_map: Vec::new(),
        }
    }

    fn update_indexes(&mut self, identity_indexes: &[IdentityIndex]) {
        self.key_target_map = identity_indexes
            .iter()
            .map(|identity_index| KeyIndex {
                target_index: identity_index.target_index,
                key: identity_index.identity.pubkey.clone(),
            })
            .collect();
    }
}

#[async_trait]
impl Session for MuxAgent {
    async fn request_identities(&mut self) -> Result<Vec<Identity>, AgentError> {
        let responses = join_all(
            self.targets
                .iter_mut()
                .map(|target| target.request_identities()),
        )
        .await;
        let responses: Result<Vec<_>, _> = responses.into_iter().collect();
        let responses = responses?;
        let identity_indexes: Vec<_> = responses
            .into_iter()
            .enumerate()
            .flat_map(|(target_index, identities)| {
                identities.into_iter().map(move |identity| IdentityIndex {
                    identity,
                    target_index,
                })
            })
            .collect();
        self.update_indexes(&identity_indexes);

        let identities = identity_indexes
            .into_iter()
            .map(|identity_index| identity_index.identity)
            .collect();
        Ok(identities)
    }

    async fn sign(&mut self, request: SignRequest) -> Result<Signature, AgentError> {
        log::info!("sign request {request:?}");
        let target_index = self
            .key_target_map
            .iter()
            .find(|key_index| key_index.key == request.pubkey)
            .unwrap()
            .target_index;
        let response = self
            .targets
            .get_mut(target_index)
            .unwrap()
            .sign(request)
            .await?;
        log::info!("sign response {response:?}");
        Ok(response)
    }

    async fn extension(&mut self, request: Extension) -> Result<Option<Extension>, AgentError> {
        log::info!("extension request {request:?}");
        let response = self
            .targets
            .first_mut()
            .unwrap()
            .extension(request)
            .await
            .unwrap_or(None);
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

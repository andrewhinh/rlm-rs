use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::thread;

use serde_json::Value;
use tokio::sync::oneshot;

use crate::pool::SandboxPool;
use crate::protocol::SandboxRunRequest;
use crate::{SandboxHandle, SandboxLauncher};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionErrorKind {
    Overloaded,
    Internal,
}

#[derive(Debug, Clone)]
pub struct SessionError {
    pub kind: SessionErrorKind,
    pub message: String,
}

impl SessionError {
    pub fn overloaded(message: impl Into<String>) -> Self {
        Self {
            kind: SessionErrorKind::Overloaded,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: SessionErrorKind::Internal,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
pub struct SessionRequest {
    pub session_id: String,
    pub reset: bool,
    pub query: String,
    pub context: Option<Value>,
    pub code: Option<String>,
    pub respond_to: oneshot::Sender<Result<SessionResponse, SessionError>>,
}

#[derive(Debug)]
pub struct SessionResponse {
    pub response: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActorState {
    Idle,
    Busy,
    ResetPending,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionConfig {
    pub max_sessions: usize,
    pub ingress_capacity: usize,
    pub sandbox_pool_size: usize,
}

#[derive(Clone)]
pub struct SessionManagerHandle {
    sender: SyncSender<SessionRequest>,
}

impl SessionManagerHandle {
    pub fn try_dispatch(&self, request: SessionRequest) -> Result<(), SessionError> {
        match self.sender.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(SessionError::overloaded(
                "request queue is full; retry later",
            )),
            Err(TrySendError::Disconnected(_)) => {
                Err(SessionError::internal("session manager unavailable"))
            }
        }
    }
}

struct ActorEntry {
    sender: Sender<ActorMessage>,
    pending: usize,
    state: SessionActorState,
}

enum ActorMessage {
    Run(ActorRequest),
}

struct ActorRequest {
    reset: bool,
    query: String,
    context: Option<Value>,
    code: Option<String>,
    respond_to: oneshot::Sender<Result<SessionResponse, SessionError>>,
}

struct ActorFinished {
    session_id: String,
}

enum PoolCommand {
    Acquire {
        respond_to: Sender<Result<Box<dyn SandboxHandle>, String>>,
    },
    Retire {
        handle: Box<dyn SandboxHandle>,
    },
}

pub fn spawn_session_manager(
    config: SessionConfig,
    launcher: Box<dyn SandboxLauncher>,
) -> Result<SessionManagerHandle, String> {
    let pool = SandboxPool::new(launcher, config.sandbox_pool_size)?;
    let pool_sender = spawn_pool_broker(pool)?;
    let (request_sender, request_receiver) =
        mpsc::sync_channel::<SessionRequest>(config.ingress_capacity.max(1));
    let (finished_sender, finished_receiver) = mpsc::channel::<ActorFinished>();

    thread::Builder::new()
        .name("session-manager".to_owned())
        .spawn(move || {
            run_session_manager_loop(
                config,
                request_receiver,
                finished_receiver,
                finished_sender,
                pool_sender,
            );
        })
        .map_err(|err| format!("failed to spawn session manager: {err}"))?;

    Ok(SessionManagerHandle {
        sender: request_sender,
    })
}

fn run_session_manager_loop(
    config: SessionConfig,
    request_receiver: Receiver<SessionRequest>,
    finished_receiver: Receiver<ActorFinished>,
    finished_sender: Sender<ActorFinished>,
    pool_sender: Sender<PoolCommand>,
) {
    let session_capacity = config.max_sessions.max(1);
    let mut actors: HashMap<String, ActorEntry> = HashMap::with_capacity(session_capacity);
    let mut idle_lru: VecDeque<String> = VecDeque::with_capacity(session_capacity);
    let mut idle_index: HashSet<String> = HashSet::with_capacity(session_capacity);

    loop {
        let request = match request_receiver.recv() {
            Ok(request) => request,
            Err(_) => break,
        };
        drain_finished_events(
            &finished_receiver,
            &mut actors,
            &mut idle_lru,
            &mut idle_index,
            4096,
        );
        let SessionRequest {
            session_id,
            reset,
            query,
            context,
            code,
            respond_to,
        } = request;

        if !actors.contains_key(&session_id) {
            if !evict_until_capacity(
                &mut actors,
                &mut idle_lru,
                &mut idle_index,
                config.max_sessions.max(1),
            ) {
                let _ = respond_to.send(Err(SessionError::overloaded(
                    "max sessions reached; no idle session available",
                )));
                continue;
            }

            let actor_sender = match spawn_session_actor(
                session_id.clone(),
                finished_sender.clone(),
                pool_sender.clone(),
            ) {
                Ok(sender) => sender,
                Err(err) => {
                    let _ = respond_to.send(Err(SessionError::internal(err)));
                    continue;
                }
            };
            actors.insert(
                session_id.clone(),
                ActorEntry {
                    sender: actor_sender,
                    pending: 0,
                    state: SessionActorState::Idle,
                },
            );
        }

        let entry = actors
            .get_mut(&session_id)
            .expect("session actor inserted before dispatch");

        remove_from_idle_lru(&mut idle_index, &session_id);
        entry.pending += 1;
        entry.state = if reset {
            SessionActorState::ResetPending
        } else {
            SessionActorState::Busy
        };

        if let Err(err) = entry.sender.send(ActorMessage::Run(ActorRequest {
            reset,
            query,
            context,
            code,
            respond_to,
        })) {
            let ActorMessage::Run(actor_request) = err.0;
            let _ = actor_request
                .respond_to
                .send(Err(SessionError::internal("failed to dispatch to actor")));
            actors.remove(&session_id);
            remove_from_idle_lru(&mut idle_index, &session_id);
        }
        drain_finished_events(
            &finished_receiver,
            &mut actors,
            &mut idle_lru,
            &mut idle_index,
            512,
        );
    }

    actors.clear();
}

fn evict_until_capacity(
    actors: &mut HashMap<String, ActorEntry>,
    idle_lru: &mut VecDeque<String>,
    idle_index: &mut HashSet<String>,
    max_sessions: usize,
) -> bool {
    while actors.len() >= max_sessions {
        if !evict_oldest_idle_actor(actors, idle_lru, idle_index) {
            return false;
        }
    }
    true
}

fn drain_finished_events(
    finished_receiver: &Receiver<ActorFinished>,
    actors: &mut HashMap<String, ActorEntry>,
    idle_lru: &mut VecDeque<String>,
    idle_index: &mut HashSet<String>,
    max_batch: usize,
) {
    let mut drained = 0usize;
    while drained < max_batch {
        let finished = match finished_receiver.try_recv() {
            Ok(finished) => finished,
            Err(_) => break,
        };
        drained += 1;
        let Some(entry) = actors.get_mut(&finished.session_id) else {
            continue;
        };
        entry.pending = entry.pending.saturating_sub(1);
        if entry.pending == 0 {
            entry.state = SessionActorState::Idle;
            if idle_index.insert(finished.session_id.clone()) {
                idle_lru.push_back(finished.session_id);
            }
        } else {
            entry.state = SessionActorState::Busy;
        }
    }
}

fn evict_oldest_idle_actor(
    actors: &mut HashMap<String, ActorEntry>,
    idle_lru: &mut VecDeque<String>,
    idle_index: &mut HashSet<String>,
) -> bool {
    while let Some(session_id) = idle_lru.pop_front() {
        if !idle_index.remove(&session_id) {
            continue;
        }
        let is_idle = actors
            .get(&session_id)
            .is_some_and(|entry| entry.pending == 0);
        if !is_idle {
            continue;
        }
        actors.remove(&session_id);
        return true;
    }
    false
}

fn remove_from_idle_lru(idle_index: &mut HashSet<String>, session_id: &str) {
    idle_index.remove(session_id);
}

fn spawn_pool_broker(mut pool: SandboxPool) -> Result<Sender<PoolCommand>, String> {
    let (sender, receiver) = mpsc::channel::<PoolCommand>();
    thread::Builder::new()
        .name("pool-broker".to_owned())
        .spawn(move || {
            while let Ok(command) = receiver.recv() {
                match command {
                    PoolCommand::Acquire { respond_to } => {
                        let _ = respond_to.send(pool.acquire());
                    }
                    PoolCommand::Retire { handle } => {
                        pool.retire(handle);
                    }
                }
            }
        })
        .map_err(|err| format!("failed to spawn pool broker: {err}"))?;
    Ok(sender)
}

fn spawn_session_actor(
    session_id: String,
    finished_sender: Sender<ActorFinished>,
    pool_sender: Sender<PoolCommand>,
) -> Result<Sender<ActorMessage>, String> {
    let (sender, receiver) = mpsc::channel::<ActorMessage>();
    thread::Builder::new()
        .name(format!("session-actor-{session_id}"))
        .spawn(move || {
            run_session_actor_loop(session_id, receiver, finished_sender, pool_sender);
        })
        .map_err(|err| format!("failed to spawn session actor: {err}"))?;
    Ok(sender)
}

fn run_session_actor_loop(
    session_id: String,
    receiver: Receiver<ActorMessage>,
    finished_sender: Sender<ActorFinished>,
    pool_sender: Sender<PoolCommand>,
) {
    let mut session: Option<(Box<dyn SandboxHandle>, bool)> = None;

    while let Ok(message) = receiver.recv() {
        let ActorMessage::Run(request) = message;
        let _ = run_actor_request(&pool_sender, &mut session, request);
        let _ = finished_sender.send(ActorFinished {
            session_id: session_id.clone(),
        });
    }

    if let Some((handle, _)) = session.take() {
        retire_handle(&pool_sender, handle);
    }
}

fn run_actor_request(
    pool_sender: &Sender<PoolCommand>,
    session: &mut Option<(Box<dyn SandboxHandle>, bool)>,
    request: ActorRequest,
) -> Result<(), SessionError> {
    if request.reset
        && let Some((handle, _)) = session.take()
    {
        retire_handle(pool_sender, handle);
    }

    if session.is_none() {
        let handle = acquire_handle(pool_sender).map_err(SessionError::internal)?;
        *session = Some((handle, false));
    }

    let (handle, initialized) = session.as_mut().expect("session initialized");
    let initialize = !*initialized;
    let run_request = SandboxRunRequest {
        initialize,
        query: request.query,
        context: request.context,
        code: request.code,
    };

    match handle.run(run_request) {
        Ok(result) => {
            if initialize {
                *initialized = true;
            }
            let _ = request.respond_to.send(Ok(SessionResponse {
                response: result.response,
                stdout: result.stdout,
                stderr: result.stderr,
            }));
            Ok(())
        }
        Err(err) => {
            if let Some((failed_handle, _)) = session.take() {
                retire_handle(pool_sender, failed_handle);
            }
            let _ = request
                .respond_to
                .send(Err(SessionError::internal(err.clone())));
            Err(SessionError::internal(err))
        }
    }
}

fn acquire_handle(pool_sender: &Sender<PoolCommand>) -> Result<Box<dyn SandboxHandle>, String> {
    let (respond_to, response) = mpsc::channel();
    pool_sender
        .send(PoolCommand::Acquire { respond_to })
        .map_err(|_| "pool broker unavailable".to_owned())?;
    response
        .recv()
        .map_err(|_| "pool broker acquire response dropped".to_owned())?
}

fn retire_handle(pool_sender: &Sender<PoolCommand>, handle: Box<dyn SandboxHandle>) {
    let _ = pool_sender.send(PoolCommand::Retire { handle });
}

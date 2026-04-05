use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::oneshot;

use crate::pool::SandboxPool;
use crate::protocol::SandboxRunRequest;
use crate::{SandboxHandle, SharedSandboxLauncher};

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
    pub request_id: String,
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
}

enum ActorMessage {
    Run(ActorRequest),
}

struct ActorRequest {
    request_id: String,
    reset: bool,
    query: String,
    context: Option<Value>,
    code: Option<String>,
    respond_to: oneshot::Sender<Result<SessionResponse, SessionError>>,
}

struct ActorFinished {
    session_id: String,
}

struct SessionRuntime {
    handle: Box<dyn SandboxHandle>,
    initialized: bool,
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
    launcher: SharedSandboxLauncher,
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
            request_id,
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
                },
            );
        }

        let entry = actors
            .get_mut(&session_id)
            .expect("session actor inserted before dispatch");

        remove_from_idle_lru(&mut idle_index, &session_id);
        entry.pending += 1;

        if let Err(err) = entry.sender.send(ActorMessage::Run(ActorRequest {
            request_id,
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
        if entry.pending == 0 && idle_index.insert(finished.session_id.clone()) {
            idle_lru.push_back(finished.session_id);
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
    let (launch_result_sender, launch_result_receiver) =
        mpsc::channel::<Result<Box<dyn SandboxHandle>, String>>();
    thread::Builder::new()
        .name("pool-broker".to_owned())
        .spawn(move || {
            let mut waiting: VecDeque<Sender<Result<Box<dyn SandboxHandle>, String>>> =
                VecDeque::new();
            let mut pending_launches = 0usize;
            let mut commands_open = true;
            while commands_open || pending_launches > 0 {
                drain_launch_results(
                    &launch_result_receiver,
                    &mut pool,
                    &mut waiting,
                    &mut pending_launches,
                );
                if commands_open {
                    maybe_spawn_launches(
                        &pool,
                        &mut waiting,
                        &mut pending_launches,
                        &launch_result_sender,
                    );
                }

                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(PoolCommand::Acquire { respond_to }) => {
                        if let Some(handle) = pool.acquire_idle() {
                            let _ = respond_to.send(Ok(handle));
                        } else {
                            waiting.push_back(respond_to);
                        }
                    }
                    Ok(PoolCommand::Retire { handle }) => {
                        let handle_id = handle.identifier();
                        println!("sandbox_retire_enqueue handle={handle_id}");
                        spawn_retire(handle);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        commands_open = false;
                    }
                }
            }

            while let Some(respond_to) = waiting.pop_front() {
                let _ = respond_to.send(Err("pool broker unavailable".to_owned()));
            }
            while let Some(handle) = pool.acquire_idle() {
                spawn_retire(handle);
            }
        })
        .map_err(|err| format!("failed to spawn pool broker: {err}"))?;
    Ok(sender)
}

fn maybe_spawn_launches(
    pool: &SandboxPool,
    waiting: &mut VecDeque<Sender<Result<Box<dyn SandboxHandle>, String>>>,
    pending_launches: &mut usize,
    sender: &Sender<Result<Box<dyn SandboxHandle>, String>>,
) {
    let desired_handles = pool.target_idle().max(waiting.len());
    let mut needed = desired_handles.saturating_sub(pool.idle_len() + *pending_launches);
    while needed > 0 {
        match spawn_launch(pool.launcher(), sender.clone()) {
            Ok(()) => {
                *pending_launches += 1;
                needed -= 1;
            }
            Err(err) => {
                if let Some(respond_to) = waiting.pop_front() {
                    let _ = respond_to.send(Err(err.clone()));
                }
                println!("sandbox_launch_spawn_failed error={err}");
                break;
            }
        }
    }
}

fn spawn_launch(
    launcher: SharedSandboxLauncher,
    sender: Sender<Result<Box<dyn SandboxHandle>, String>>,
) -> Result<(), String> {
    thread::Builder::new()
        .name("sandbox-launch".to_owned())
        .spawn(move || {
            let result = launcher.launch();
            if let Err(err) = sender.send(result)
                && let Ok(handle) = err.0
            {
                println!("sandbox_launch_send_failed");
                terminate_handle_now(handle);
            }
        })
        .map(|_| ())
        .map_err(|err| format!("failed to spawn sandbox launch worker: {err}"))
}

fn drain_launch_results(
    receiver: &Receiver<Result<Box<dyn SandboxHandle>, String>>,
    pool: &mut SandboxPool,
    waiting: &mut VecDeque<Sender<Result<Box<dyn SandboxHandle>, String>>>,
    pending_launches: &mut usize,
) {
    while let Ok(result) = receiver.try_recv() {
        *pending_launches = pending_launches.saturating_sub(1);
        match result {
            Ok(handle) => {
                deliver_handle(pool, waiting, handle);
            }
            Err(err) => {
                if let Some(respond_to) = waiting.pop_front() {
                    let _ = respond_to.send(Err(err.clone()));
                }
                println!("sandbox_launch_failed error={err}");
            }
        }
    }
}

fn spawn_retire(handle: Box<dyn SandboxHandle>) {
    let (sender, receiver) = mpsc::sync_channel::<Box<dyn SandboxHandle>>(1);
    let result = thread::Builder::new()
        .name("sandbox-retire".to_owned())
        .spawn(move || {
            if let Ok(handle) = receiver.recv() {
                terminate_handle_now(handle);
            }
        });
    if let Err(err) = result {
        println!("sandbox_retire_spawn_failed error={err}");
        terminate_handle_now(handle);
        return;
    }
    if let Err(err) = sender.send(handle) {
        println!("sandbox_retire_send_failed");
        terminate_handle_now(err.0);
    }
}

fn terminate_handle_now(handle: Box<dyn SandboxHandle>) {
    let mut handle = handle;
    handle.terminate();
}

fn deliver_handle(
    pool: &mut SandboxPool,
    waiting: &mut VecDeque<Sender<Result<Box<dyn SandboxHandle>, String>>>,
    handle: Box<dyn SandboxHandle>,
) {
    let mut handle = handle;
    while let Some(respond_to) = waiting.pop_front() {
        match respond_to.send(Ok(handle)) {
            Ok(()) => return,
            Err(err) => match err.0 {
                Ok(returned_handle) => {
                    handle = returned_handle;
                }
                Err(_) => {
                    return;
                }
            },
        }
    }
    pool.add_idle(handle);
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
    let mut session: Option<SessionRuntime> = None;

    while let Ok(message) = receiver.recv() {
        let ActorMessage::Run(request) = message;
        let request_id = request.request_id.clone();
        let request_started = Instant::now();
        let result = run_actor_request(&pool_sender, &session_id, &mut session, request);
        println!(
            "session_actor_done request_id={} session_id={} ok={} elapsed_ms={}",
            request_id,
            session_id,
            result.is_ok(),
            request_started.elapsed().as_millis()
        );
        let _ = finished_sender.send(ActorFinished {
            session_id: session_id.clone(),
        });
    }

    if let Some(runtime) = session.take() {
        retire_handle(&pool_sender, runtime.handle);
    }
}

fn run_actor_request(
    pool_sender: &Sender<PoolCommand>,
    session_id: &str,
    session: &mut Option<SessionRuntime>,
    request: ActorRequest,
) -> Result<(), SessionError> {
    let ActorRequest {
        request_id,
        reset,
        query,
        context,
        code,
        respond_to,
    } = request;
    if reset && let Some(runtime) = session.as_mut() {
        let reset_started = Instant::now();
        let handle_id = runtime.handle.identifier();
        println!(
            "session_reset_start request_id={} session_id={} handle={}",
            request_id, session_id, handle_id
        );
        match runtime.handle.reset() {
            Ok(()) => {
                runtime.initialized = false;
                println!(
                    "session_reset_done request_id={} session_id={} handle={} elapsed_ms={}",
                    request_id,
                    session_id,
                    handle_id,
                    reset_started.elapsed().as_millis()
                );
            }
            Err(err) => {
                println!(
                    "session_reset_failed request_id={} session_id={} handle={} error={}",
                    request_id, session_id, handle_id, err
                );
                if let Some(runtime) = session.take() {
                    retire_handle(pool_sender, runtime.handle);
                }
            }
        }
    }

    if session.is_none() {
        let acquire_started = Instant::now();
        let handle = match acquire_handle(pool_sender) {
            Ok(handle) => handle,
            Err(err) => {
                let err = SessionError::internal(err);
                let _ = respond_to.send(Err(err.clone()));
                return Err(err);
            }
        };
        let handle_id = handle.identifier();
        println!(
            "session_acquire_done request_id={} session_id={} handle={} wait_ms={}",
            request_id,
            session_id,
            handle_id,
            acquire_started.elapsed().as_millis()
        );
        *session = Some(SessionRuntime {
            handle,
            initialized: false,
        });
    }

    let runtime = session
        .as_mut()
        .ok_or_else(|| SessionError::internal("session runtime missing after acquire"))?;
    let initialize = !runtime.initialized;
    let run_request = SandboxRunRequest {
        initialize,
        query,
        context,
        code,
    };
    let handle_id = runtime.handle.identifier();
    let run_started = Instant::now();
    println!(
        "session_run_start request_id={} session_id={} handle={} initialize={}",
        request_id, session_id, handle_id, initialize
    );

    match runtime.handle.run(run_request) {
        Ok(result) => {
            if initialize {
                runtime.initialized = true;
            }
            println!(
                "session_run_done request_id={} session_id={} handle={} elapsed_ms={}",
                request_id,
                session_id,
                handle_id,
                run_started.elapsed().as_millis()
            );
            let _ = respond_to.send(Ok(SessionResponse {
                response: result.response,
                stdout: result.stdout,
                stderr: result.stderr,
            }));
            Ok(())
        }
        Err(err) => {
            if let Some(runtime) = session.take() {
                retire_handle(pool_sender, runtime.handle);
            }
            println!(
                "session_run_failed request_id={} session_id={} handle={} elapsed_ms={} error={}",
                request_id,
                session_id,
                handle_id,
                run_started.elapsed().as_millis(),
                err
            );
            let _ = respond_to.send(Err(SessionError::internal(err.clone())));
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
    let handle_id = handle.identifier();
    println!("sandbox_retire_request handle={handle_id}");
    let _ = pool_sender.send(PoolCommand::Retire { handle });
}

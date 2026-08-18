use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::catalog::ResolvedRuntime;
#[cfg(test)]
use crate::runtime::InvocationRuntime;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SessionKey {
    pub(crate) runtime_arn: String,
    pub(crate) qualifier: String,
    pub(crate) runtime_session_id: String,
}

impl SessionKey {
    fn new(runtime: &ResolvedRuntime, runtime_session_id: String) -> Self {
        Self {
            runtime_arn: runtime.runtime_arn.clone(),
            qualifier: runtime.qualifier.clone(),
            runtime_session_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionContainer {
    pub(crate) id: String,
    pub(crate) endpoint: String,
    pub(crate) age: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionHealth {
    Healthy,
    HealthyBusy,
    Unhealthy,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CommandEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited(i64),
    TimedOut,
    Cancelled,
}

pub(crate) struct CommandExecution {
    pub(crate) events: mpsc::Receiver<Result<CommandEvent, String>>,
}

impl CommandExecution {
    pub(crate) async fn recv(&mut self) -> Option<Result<CommandEvent, String>> {
        self.events.recv().await
    }
}

pub(crate) struct SessionCommandExecution {
    execution: CommandExecution,
    cancellation: CancellationToken,
    _lease: SessionLease,
}

impl SessionCommandExecution {
    pub(crate) async fn recv(&mut self) -> Option<Result<CommandEvent, String>> {
        self.execution.recv().await
    }
}

impl Drop for SessionCommandExecution {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[async_trait]
pub(crate) trait SessionBackend: Send + Sync {
    async fn start(
        &self,
        key: &SessionKey,
        runtime: Arc<ResolvedRuntime>,
        cancellation: CancellationToken,
    ) -> Result<SessionContainer, String>;

    async fn ping(&self, container: &SessionContainer) -> SessionHealth;

    async fn stop(&self, container: &SessionContainer) -> Result<(), String>;

    async fn execute_command(
        &self,
        _container: &SessionContainer,
        _command: String,
        _timeout_seconds: Option<u64>,
        _cancellation: CancellationToken,
    ) -> Result<CommandExecution, String> {
        Err("runtime session backend does not support commands".to_owned())
    }
}

#[cfg(test)]
pub(crate) struct InvocationSessionBackend {
    runtime: InvocationRuntime,
}

#[cfg(test)]
impl InvocationSessionBackend {
    pub(crate) fn new(runtime: InvocationRuntime) -> Self {
        Self { runtime }
    }
}

#[cfg(test)]
#[async_trait]
impl SessionBackend for InvocationSessionBackend {
    async fn start(
        &self,
        key: &SessionKey,
        _runtime: Arc<ResolvedRuntime>,
        cancellation: CancellationToken,
    ) -> Result<SessionContainer, String> {
        if cancellation.is_cancelled() {
            return Err("session provisioning was cancelled".to_owned());
        }
        Ok(SessionContainer {
            id: format!(
                "transitional-{}-{}-{}",
                key.runtime_arn, key.qualifier, key.runtime_session_id
            ),
            endpoint: "legacy-invocation-runtime".to_owned(),
            age: Duration::ZERO,
        })
    }

    async fn ping(&self, _container: &SessionContainer) -> SessionHealth {
        if self.runtime.active_count().await == 0 {
            SessionHealth::Healthy
        } else {
            SessionHealth::HealthyBusy
        }
    }

    async fn stop(&self, _container: &SessionContainer) -> Result<(), String> {
        Ok(())
    }

    async fn execute_command(
        &self,
        _container: &SessionContainer,
        command: String,
        _timeout_seconds: Option<u64>,
        _cancellation: CancellationToken,
    ) -> Result<CommandExecution, String> {
        let (sender, events) = mpsc::channel(3);
        let events_to_send = match command.as_str() {
            "exit 7" => vec![Ok(CommandEvent::Exited(7))],
            "timeout" => vec![Ok(CommandEvent::TimedOut)],
            "error" => vec![Err("container secret details".to_owned())],
            _ => vec![
                Ok(CommandEvent::Stdout(command.into_bytes())),
                Ok(CommandEvent::Exited(0)),
            ],
        };
        for event in events_to_send {
            sender
                .send(event)
                .await
                .map_err(|_| "command fixture receiver was dropped".to_owned())?;
        }
        Ok(CommandExecution { events })
    }
}

#[derive(Clone)]
pub(crate) struct SessionManager {
    inner: Arc<SessionManagerInner>,
}

struct SessionManagerInner {
    backend: Arc<dyn SessionBackend>,
    entries: Mutex<HashMap<SessionKey, SessionEntry>>,
    next_generation: Mutex<u64>,
}

struct StartingTransitionGuard {
    inner: Arc<SessionManagerInner>,
    key: SessionKey,
    generation: u64,
    cancellation: CancellationToken,
    armed: bool,
}

impl Drop for StartingTransitionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancellation.cancel();
        let completion = {
            let mut entries = self.inner.entries.lock().expect("session state lock");
            match entries.get(&self.key) {
                Some(SessionEntry::Starting { generation, .. })
                    if *generation == self.generation =>
                {
                    entries.remove(&self.key);
                    None
                }
                Some(SessionEntry::Stopping {
                    generation,
                    notify,
                    outcome,
                    ..
                }) if *generation == self.generation => {
                    let completion = (Arc::clone(notify), Arc::clone(outcome));
                    entries.remove(&self.key);
                    Some(completion)
                }
                _ => None,
            }
        };
        if let Some((notify, outcome)) = completion {
            *outcome.lock().expect("stop outcome lock") = Some(Err(
                "session provisioning task was interrupted before cleanup confirmation".to_owned(),
            ));
            notify.notify_waiters();
        }
    }
}

struct StopTransitionGuard {
    inner: Arc<SessionManagerInner>,
    key: SessionKey,
    generation: u64,
    container: Arc<SessionContainer>,
    cancellation: CancellationToken,
    notify: Arc<Notify>,
    outcome: Arc<Mutex<Option<Result<(), String>>>>,
    armed: bool,
}

impl Drop for StopTransitionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let key = self.key.clone();
        let generation = self.generation;
        let container = Arc::clone(&self.container);
        let cancellation = self.cancellation.clone();
        let notify = Arc::clone(&self.notify);
        let outcome = Arc::clone(&self.outcome);
        cancellation.cancel();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = finish_stop(
                    inner,
                    key,
                    generation,
                    container,
                    cancellation,
                    notify,
                    outcome,
                )
                .await;
            });
        }
    }
}

#[derive(Debug)]
enum SessionEntry {
    Starting {
        generation: u64,
        cancellation: CancellationToken,
    },
    Ready {
        generation: u64,
        container: Arc<SessionContainer>,
        cancellation: CancellationToken,
        started_at: Instant,
        last_activity: Instant,
        active_requests: usize,
        idle_timeout_seconds: u64,
        maximum_lifetime_seconds: u64,
    },
    Stopping {
        generation: u64,
        client_token: Option<String>,
        notify: Arc<Notify>,
        outcome: Arc<Mutex<Option<Result<(), String>>>>,
    },
    Failed {
        generation: u64,
        message: String,
        container: Option<Arc<SessionContainer>>,
        cancellation: CancellationToken,
    },
}

async fn finish_stop(
    inner: Arc<SessionManagerInner>,
    key: SessionKey,
    generation: u64,
    container: Arc<SessionContainer>,
    cancellation: CancellationToken,
    notify: Arc<Notify>,
    outcome: Arc<Mutex<Option<Result<(), String>>>>,
) -> Result<(), SessionError> {
    cancellation.cancel();
    let stopped = inner.backend.stop(&container).await;
    let mut entries = inner.entries.lock().expect("session state lock");
    if matches!(
        entries.get(&key),
        Some(SessionEntry::Stopping {
            generation: current,
            ..
        }) if *current == generation
    ) {
        match &stopped {
            Ok(()) => {
                entries.remove(&key);
            }
            Err(message) => {
                entries.insert(
                    key,
                    SessionEntry::Failed {
                        generation,
                        message: format!("session cleanup must be retried: {message}"),
                        container: Some(container),
                        cancellation,
                    },
                );
            }
        }
    }
    *outcome.lock().expect("stop outcome lock") = Some(stopped.clone());
    notify.notify_waiters();
    stopped.map_err(SessionError::Stopping)
}

impl SessionManager {
    pub(crate) fn new(backend: Arc<dyn SessionBackend>) -> Self {
        let manager = Self {
            inner: Arc::new(SessionManagerInner {
                backend,
                entries: Mutex::new(HashMap::new()),
                next_generation: Mutex::new(0),
            }),
        };
        manager.start_reaper();
        manager
    }

    pub(crate) fn adopt(
        &self,
        runtime: Arc<ResolvedRuntime>,
        key: SessionKey,
        container: SessionContainer,
    ) -> Result<(), SessionError> {
        let generation = self.next_generation();
        let cancellation = CancellationToken::new();
        let now = Instant::now();
        let started_at = now.checked_sub(container.age).unwrap_or(now);
        let mut entries = self.inner.entries.lock().expect("session state lock");
        if entries.contains_key(&key) {
            return Err(SessionError::RetryableConflict(
                "runtime session was already registered".to_owned(),
            ));
        }
        entries.insert(
            key,
            SessionEntry::Ready {
                generation,
                container: Arc::new(container),
                cancellation,
                started_at,
                last_activity: now,
                active_requests: 0,
                idle_timeout_seconds: runtime.lifecycle.idle_timeout_seconds,
                maximum_lifetime_seconds: runtime.lifecycle.maximum_lifetime_seconds,
            },
        );
        Ok(())
    }

    fn start_reaper(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        handle.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                SessionManager { inner }.reap_once().await;
            }
        });
    }

    pub(crate) async fn acquire(
        &self,
        runtime: Arc<ResolvedRuntime>,
        runtime_session_id: String,
    ) -> Result<SessionLease, SessionError> {
        let key = SessionKey::new(&runtime, runtime_session_id);
        let (generation, cancellation) = {
            let mut entries = self.inner.entries.lock().expect("session state lock");
            match entries.get_mut(&key) {
                Some(SessionEntry::Ready {
                    generation,
                    container,
                    cancellation,
                    last_activity,
                    active_requests,
                    ..
                }) => {
                    *active_requests += 1;
                    *last_activity = Instant::now();
                    return Ok(SessionLease {
                        inner: Arc::clone(&self.inner),
                        key,
                        generation: *generation,
                        container: Arc::clone(container),
                        cancellation: cancellation.child_token(),
                    });
                }
                Some(SessionEntry::Starting { .. }) => {
                    return Err(SessionError::RetryableConflict(
                        "runtime session is starting".to_owned(),
                    ));
                }
                Some(SessionEntry::Stopping { .. }) => {
                    return Err(SessionError::RetryableConflict(
                        "runtime session is stopping".to_owned(),
                    ));
                }
                Some(SessionEntry::Failed { message, .. }) => {
                    return Err(SessionError::Provisioning(message.clone()));
                }
                None => {}
            }

            let generation = self.next_generation();
            let cancellation = CancellationToken::new();
            entries.insert(
                key.clone(),
                SessionEntry::Starting {
                    generation,
                    cancellation: cancellation.clone(),
                },
            );
            (generation, cancellation)
        };

        let mut transition = StartingTransitionGuard {
            inner: Arc::clone(&self.inner),
            key: key.clone(),
            generation,
            cancellation: cancellation.clone(),
            armed: true,
        };
        let started = self
            .inner
            .backend
            .start(&key, Arc::clone(&runtime), cancellation.clone())
            .await;
        let mut cleanup = None;
        let result = {
            let mut entries = self.inner.entries.lock().expect("session state lock");
            match entries.get(&key) {
                Some(SessionEntry::Starting {
                    generation: current,
                    ..
                }) if *current == generation => match started {
                    Ok(container) => {
                        let container = Arc::new(container);
                        let request_cancellation = cancellation.child_token();
                        let now = Instant::now();
                        let started_at = now.checked_sub(container.age).unwrap_or(now);
                        entries.insert(
                            key.clone(),
                            SessionEntry::Ready {
                                generation,
                                container: Arc::clone(&container),
                                cancellation,
                                started_at,
                                last_activity: now,
                                active_requests: 1,
                                idle_timeout_seconds: runtime.lifecycle.idle_timeout_seconds,
                                maximum_lifetime_seconds: runtime
                                    .lifecycle
                                    .maximum_lifetime_seconds,
                            },
                        );
                        Ok(SessionLease {
                            inner: Arc::clone(&self.inner),
                            key: key.clone(),
                            generation,
                            container,
                            cancellation: request_cancellation,
                        })
                    }
                    Err(message) => {
                        entries.insert(
                            key.clone(),
                            SessionEntry::Failed {
                                generation,
                                message: message.clone(),
                                container: None,
                                cancellation,
                            },
                        );
                        Err(SessionError::Provisioning(message))
                    }
                },
                Some(SessionEntry::Stopping {
                    generation: current,
                    notify,
                    outcome,
                    ..
                }) if *current == generation => {
                    let notify = Arc::clone(notify);
                    let outcome = Arc::clone(outcome);
                    match started {
                        Ok(container) => cleanup = Some((container, notify, outcome)),
                        Err(message) => {
                            entries.insert(
                                key.clone(),
                                SessionEntry::Failed {
                                    generation,
                                    message: message.clone(),
                                    container: None,
                                    cancellation: CancellationToken::new(),
                                },
                            );
                            *outcome.lock().expect("stop outcome lock") = Some(Err(message));
                            notify.notify_waiters();
                        }
                    }
                    Err(SessionError::StoppedDuringProvisioning)
                }
                _ => {
                    if let Ok(container) = started {
                        cleanup = Some((
                            container,
                            Arc::new(Notify::new()),
                            Arc::new(Mutex::new(None)),
                        ));
                    }
                    Err(SessionError::StoppedDuringProvisioning)
                }
            }
        };

        if let Some((container, notify, outcome)) = cleanup {
            let cleanup = self.inner.backend.stop(&container).await;
            let mut entries = self.inner.entries.lock().expect("session state lock");
            if matches!(
                entries.get(&key),
                Some(SessionEntry::Stopping {
                    generation: current,
                    ..
                }) if *current == generation
            ) {
                match &cleanup {
                    Ok(()) => {
                        entries.remove(&key);
                    }
                    Err(message) => {
                        entries.insert(
                            key.clone(),
                            SessionEntry::Failed {
                                generation,
                                message: format!("session cleanup must be retried: {message}"),
                                container: Some(Arc::new(container)),
                                cancellation: CancellationToken::new(),
                            },
                        );
                    }
                }
            }
            *outcome.lock().expect("stop outcome lock") = Some(cleanup);
            notify.notify_waiters();
        }
        transition.armed = false;
        result
    }

    pub(crate) async fn execute_command(
        &self,
        runtime: Arc<ResolvedRuntime>,
        runtime_session_id: String,
        command: String,
        timeout_seconds: Option<u64>,
    ) -> Result<SessionCommandExecution, SessionError> {
        let lease = self.acquire(runtime, runtime_session_id).await?;
        let cancellation = lease.cancellation();
        let execution = self
            .inner
            .backend
            .execute_command(
                &lease.container,
                command,
                timeout_seconds,
                cancellation.clone(),
            )
            .await
            .map_err(SessionError::Command)?;
        Ok(SessionCommandExecution {
            execution,
            cancellation,
            _lease: lease,
        })
    }

    pub(crate) async fn stop(
        &self,
        runtime: &ResolvedRuntime,
        runtime_session_id: String,
        client_token: Option<String>,
    ) -> Result<(), SessionError> {
        let key = SessionKey::new(runtime, runtime_session_id);
        type SharedOutcome = Arc<Mutex<Option<Result<(), String>>>>;
        enum StopPlan {
            Complete,
            Wait(Pin<Box<dyn Future<Output = ()> + Send>>, SharedOutcome),
            Stop {
                generation: u64,
                container: Arc<SessionContainer>,
                cancellation: CancellationToken,
                notify: Arc<Notify>,
                outcome: SharedOutcome,
            },
        }
        let plan = {
            let mut entries = self.inner.entries.lock().expect("session state lock");
            match entries.remove(&key) {
                None
                | Some(SessionEntry::Failed {
                    container: None, ..
                }) => StopPlan::Complete,
                Some(SessionEntry::Failed {
                    generation,
                    container: Some(container),
                    cancellation,
                    ..
                }) => {
                    cancellation.cancel();
                    let notify = Arc::new(Notify::new());
                    let outcome = Arc::new(Mutex::new(None));
                    entries.insert(
                        key.clone(),
                        SessionEntry::Stopping {
                            generation,
                            client_token,
                            notify: Arc::clone(&notify),
                            outcome: Arc::clone(&outcome),
                        },
                    );
                    StopPlan::Stop {
                        generation,
                        container,
                        cancellation,
                        notify,
                        outcome,
                    }
                }
                Some(SessionEntry::Starting {
                    generation,
                    cancellation,
                }) => {
                    cancellation.cancel();
                    let notify = Arc::new(Notify::new());
                    let outcome = Arc::new(Mutex::new(None));
                    let notified = Box::pin(Arc::clone(&notify).notified_owned());
                    entries.insert(
                        key.clone(),
                        SessionEntry::Stopping {
                            generation,
                            client_token,
                            notify,
                            outcome: Arc::clone(&outcome),
                        },
                    );
                    StopPlan::Wait(notified, outcome)
                }
                Some(SessionEntry::Ready {
                    generation,
                    container,
                    cancellation,
                    ..
                }) => {
                    cancellation.cancel();
                    let notify = Arc::new(Notify::new());
                    let outcome = Arc::new(Mutex::new(None));
                    entries.insert(
                        key.clone(),
                        SessionEntry::Stopping {
                            generation,
                            client_token,
                            notify: Arc::clone(&notify),
                            outcome: Arc::clone(&outcome),
                        },
                    );
                    StopPlan::Stop {
                        generation,
                        container,
                        cancellation,
                        notify,
                        outcome,
                    }
                }
                Some(SessionEntry::Stopping {
                    generation,
                    client_token: active_token,
                    notify,
                    outcome,
                }) => {
                    let notified = Box::pin(Arc::clone(&notify).notified_owned());
                    entries.insert(
                        key.clone(),
                        SessionEntry::Stopping {
                            generation,
                            client_token: active_token.clone(),
                            notify,
                            outcome: Arc::clone(&outcome),
                        },
                    );
                    if active_token == client_token {
                        StopPlan::Wait(notified, outcome)
                    } else {
                        return Err(SessionError::RetryableConflict(
                            "runtime session is already stopping with another client token"
                                .to_owned(),
                        ));
                    }
                }
            }
        };

        match plan {
            StopPlan::Complete => Ok(()),
            StopPlan::Wait(notified, outcome) => {
                notified.await;
                outcome
                    .lock()
                    .expect("stop outcome lock")
                    .clone()
                    .unwrap_or_else(|| Err("stop completed without an outcome".to_owned()))
                    .map_err(SessionError::Stopping)
            }
            StopPlan::Stop {
                generation,
                container,
                cancellation,
                notify,
                outcome,
            } => {
                let mut transition = StopTransitionGuard {
                    inner: Arc::clone(&self.inner),
                    key: key.clone(),
                    generation,
                    container: Arc::clone(&container),
                    cancellation: cancellation.clone(),
                    notify: Arc::clone(&notify),
                    outcome: Arc::clone(&outcome),
                    armed: true,
                };
                let stopped = finish_stop(
                    Arc::clone(&self.inner),
                    key,
                    generation,
                    container,
                    cancellation,
                    notify,
                    outcome,
                )
                .await;
                transition.armed = false;
                stopped
            }
        }
    }

    pub(crate) async fn reap_once(&self) {
        #[derive(Clone)]
        struct Candidate {
            key: SessionKey,
            generation: u64,
            container: Arc<SessionContainer>,
            expired: bool,
            observed_last_activity: Instant,
        }

        let now = Instant::now();
        let candidates = {
            let entries = self.inner.entries.lock().expect("session state lock");
            entries
                .iter()
                .filter_map(|(key, entry)| match entry {
                    SessionEntry::Ready {
                        generation,
                        container,
                        started_at,
                        last_activity,
                        active_requests,
                        idle_timeout_seconds,
                        maximum_lifetime_seconds,
                        ..
                    } if now.duration_since(*started_at).as_secs() >= *maximum_lifetime_seconds
                        || (*active_requests == 0
                            && now.duration_since(*last_activity).as_secs()
                                >= *idle_timeout_seconds) =>
                    {
                        Some(Candidate {
                            key: key.clone(),
                            generation: *generation,
                            container: Arc::clone(container),
                            expired: now.duration_since(*started_at).as_secs()
                                >= *maximum_lifetime_seconds,
                            observed_last_activity: *last_activity,
                        })
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        let failed_cleanups = {
            let entries = self.inner.entries.lock().expect("session state lock");
            entries
                .iter()
                .filter_map(|(key, entry)| match entry {
                    SessionEntry::Failed {
                        generation,
                        container: Some(_),
                        ..
                    } => Some((key.clone(), *generation)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        for candidate in candidates {
            let health = if candidate.expired {
                SessionHealth::Unhealthy
            } else {
                self.inner.backend.ping(&candidate.container).await
            };
            if health == SessionHealth::HealthyBusy {
                let mut entries = self.inner.entries.lock().expect("session state lock");
                if let Some(SessionEntry::Ready { last_activity, .. }) =
                    entries.get_mut(&candidate.key)
                {
                    *last_activity = Instant::now();
                }
                continue;
            }
            let should_stop = {
                let entries = self.inner.entries.lock().expect("session state lock");
                matches!(
                    entries.get(&candidate.key),
                    Some(SessionEntry::Ready {
                        generation,
                        last_activity,
                        active_requests,
                        ..
                    }) if *generation == candidate.generation
                        && (candidate.expired
                            || (*active_requests == 0
                                && *last_activity == candidate.observed_last_activity))
                )
            };
            if should_stop {
                let _ = self.stop_by_key(candidate.key, candidate.generation).await;
            }
        }
        for (key, generation) in failed_cleanups {
            let _ = self.stop_by_key(key, generation).await;
        }
    }

    async fn stop_by_key(
        &self,
        key: SessionKey,
        expected_generation: u64,
    ) -> Result<(), SessionError> {
        let (container, cancellation, notify, outcome) = {
            let mut entries = self.inner.entries.lock().expect("session state lock");
            let (generation, container, cancellation) = match entries.remove(&key) {
                Some(SessionEntry::Ready {
                    generation,
                    container,
                    cancellation,
                    ..
                }) if generation == expected_generation => (generation, container, cancellation),
                Some(SessionEntry::Failed {
                    generation,
                    container: Some(container),
                    cancellation,
                    ..
                }) if generation == expected_generation => (generation, container, cancellation),
                Some(entry) => {
                    entries.insert(key.clone(), entry);
                    return Ok(());
                }
                None => return Ok(()),
            };
            cancellation.cancel();
            let notify = Arc::new(Notify::new());
            let outcome = Arc::new(Mutex::new(None));
            entries.insert(
                key.clone(),
                SessionEntry::Stopping {
                    generation,
                    client_token: Some("lifecycle-reaper".to_owned()),
                    notify: Arc::clone(&notify),
                    outcome: Arc::clone(&outcome),
                },
            );
            (container, cancellation, notify, outcome)
        };
        let mut transition = StopTransitionGuard {
            inner: Arc::clone(&self.inner),
            key: key.clone(),
            generation: expected_generation,
            container: Arc::clone(&container),
            cancellation: cancellation.clone(),
            notify: Arc::clone(&notify),
            outcome: Arc::clone(&outcome),
            armed: true,
        };
        let result = finish_stop(
            Arc::clone(&self.inner),
            key,
            expected_generation,
            container,
            cancellation,
            notify,
            outcome,
        )
        .await;
        transition.armed = false;
        result
    }

    #[cfg(test)]
    pub(crate) fn active_request_count(
        &self,
        runtime: &ResolvedRuntime,
        runtime_session_id: &str,
    ) -> Option<usize> {
        let key = SessionKey::new(runtime, runtime_session_id.to_owned());
        self.inner
            .entries
            .lock()
            .expect("session state lock")
            .get(&key)
            .and_then(|entry| match entry {
                SessionEntry::Ready {
                    active_requests, ..
                } => Some(*active_requests),
                _ => None,
            })
    }

    #[cfg(test)]
    fn state(&self, key: &SessionKey) -> Option<&'static str> {
        self.inner
            .entries
            .lock()
            .expect("session state lock")
            .get(key)
            .map(|entry| match entry {
                SessionEntry::Starting { .. } => "starting",
                SessionEntry::Ready { .. } => "ready",
                SessionEntry::Stopping { .. } => "stopping",
                SessionEntry::Failed { .. } => "failed",
            })
    }

    fn next_generation(&self) -> u64 {
        let mut generation = self
            .inner
            .next_generation
            .lock()
            .expect("session generation lock");
        *generation = generation.wrapping_add(1);
        *generation
    }
}

pub(crate) struct SessionLease {
    inner: Arc<SessionManagerInner>,
    key: SessionKey,
    generation: u64,
    pub(crate) container: Arc<SessionContainer>,
    pub(crate) cancellation: CancellationToken,
}

impl SessionLease {
    #[cfg(test)]
    pub(crate) fn container_id(&self) -> &str {
        &self.container.id
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.container.endpoint
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let mut entries = self.inner.entries.lock().expect("session state lock");
        if let Some(SessionEntry::Ready {
            generation,
            last_activity,
            active_requests,
            ..
        }) = entries.get_mut(&self.key)
            && *generation == self.generation
        {
            *active_requests = active_requests.saturating_sub(1);
            *last_activity = Instant::now();
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum SessionError {
    #[error("retryable session conflict: {0}")]
    RetryableConflict(String),
    #[error("session provisioning failed: {0}")]
    Provisioning(String),
    #[error("session command failed: {0}")]
    Command(String),
    #[error("session was stopped during provisioning")]
    StoppedDuringProvisioning,
    #[error("session stop failed: {0}")]
    Stopping(String),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use async_trait::async_trait;
    use tokio::sync::{Notify, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::{
        CommandEvent, CommandExecution, SessionBackend, SessionContainer, SessionEntry,
        SessionError, SessionHealth, SessionKey, SessionManager,
    };
    use crate::catalog::RuntimeCatalog;

    #[derive(Default)]
    struct FakeBackend {
        starts: Mutex<Vec<String>>,
        stops: Mutex<Vec<String>>,
        health: Mutex<HashMap<String, SessionHealth>>,
        start_gate: Mutex<Option<Arc<Notify>>>,
        stop_gate: Mutex<Option<Arc<Notify>>>,
        stop_failures: Mutex<usize>,
        command_cancellation: Mutex<Option<CancellationToken>>,
    }

    #[async_trait]
    impl SessionBackend for FakeBackend {
        async fn start(
            &self,
            key: &SessionKey,
            _runtime: Arc<crate::catalog::ResolvedRuntime>,
            cancellation: CancellationToken,
        ) -> Result<SessionContainer, String> {
            let gate = { self.start_gate.lock().expect("gate lock").clone() };
            if let Some(gate) = gate {
                tokio::select! {
                    () = gate.notified() => {}
                    () = cancellation.cancelled() => return Err("cancelled".to_owned()),
                }
            }
            let mut starts = self.starts.lock().expect("starts lock");
            let id = format!("container-{}", starts.len() + 1);
            starts.push(key.runtime_session_id.clone());
            self.health
                .lock()
                .expect("health lock")
                .insert(id.clone(), SessionHealth::Healthy);
            Ok(SessionContainer {
                id,
                endpoint: "http://127.0.0.1:8080".to_owned(),
                age: Duration::ZERO,
            })
        }

        async fn ping(&self, container: &SessionContainer) -> SessionHealth {
            self.health
                .lock()
                .expect("health lock")
                .get(&container.id)
                .copied()
                .unwrap_or(SessionHealth::Unhealthy)
        }

        async fn execute_command(
            &self,
            _container: &SessionContainer,
            _command: String,
            _timeout_seconds: Option<u64>,
            cancellation: CancellationToken,
        ) -> Result<CommandExecution, String> {
            *self
                .command_cancellation
                .lock()
                .expect("command cancellation lock") = Some(cancellation.clone());
            let (sender, events) = mpsc::channel(1);
            tokio::spawn(async move {
                cancellation.cancelled().await;
                let _ = sender.send(Ok(CommandEvent::Cancelled)).await;
            });
            Ok(CommandExecution { events })
        }

        async fn stop(&self, container: &SessionContainer) -> Result<(), String> {
            self.stops
                .lock()
                .expect("stops lock")
                .push(container.id.clone());
            let gate = { self.stop_gate.lock().expect("stop gate lock").clone() };
            if let Some(gate) = gate {
                gate.notified().await;
            }
            let should_fail = {
                let mut failures = self.stop_failures.lock().expect("stop failures lock");
                if *failures > 0 {
                    *failures -= 1;
                    true
                } else {
                    false
                }
            };
            if should_fail {
                return Err("fixture cleanup failure".to_owned());
            }
            Ok(())
        }
    }

    fn runtime() -> Arc<crate::catalog::ResolvedRuntime> {
        RuntimeCatalog::test_catalog().default_snapshot()
    }

    #[tokio::test]
    async fn sequential_requests_reuse_one_container_and_other_sessions_do_not() {
        let backend = Arc::new(FakeBackend::default());
        let manager = SessionManager::new(backend.clone());
        let first = manager
            .acquire(runtime(), "session-a".to_owned())
            .await
            .expect("first lease");
        let first_id = first.container.id.clone();
        drop(first);
        let second = manager
            .acquire(runtime(), "session-a".to_owned())
            .await
            .expect("second lease");
        assert_eq!(second.container.id, first_id);
        drop(second);
        let other = manager
            .acquire(runtime(), "session-b".to_owned())
            .await
            .expect("other session lease");
        assert_ne!(other.container.id, first_id);
        assert_eq!(backend.starts.lock().expect("starts lock").len(), 2);
    }

    #[tokio::test]
    async fn dropping_a_command_response_cancels_execution_and_releases_the_lease() {
        let backend = Arc::new(FakeBackend::default());
        let manager = SessionManager::new(backend.clone());
        let deployment = runtime();
        let execution = manager
            .execute_command(
                Arc::clone(&deployment),
                "command-session".to_owned(),
                "sleep 300".to_owned(),
                None,
            )
            .await
            .expect("start command");
        assert_eq!(
            manager.active_request_count(&deployment, "command-session"),
            Some(1)
        );
        drop(execution);
        tokio::task::yield_now().await;
        assert!(
            backend
                .command_cancellation
                .lock()
                .expect("command cancellation lock")
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        );
        assert_eq!(
            manager.active_request_count(&deployment, "command-session"),
            Some(0)
        );
    }

    #[tokio::test]
    async fn provisioning_is_atomic_and_a_second_request_conflicts() {
        let backend = Arc::new(FakeBackend::default());
        let gate = Arc::new(Notify::new());
        *backend.start_gate.lock().expect("gate lock") = Some(Arc::clone(&gate));
        let manager = SessionManager::new(backend);
        let starting_manager = manager.clone();
        let starting = tokio::spawn(async move {
            starting_manager
                .acquire(runtime(), "session-a".to_owned())
                .await
        });
        tokio::task::yield_now().await;
        let conflict = match manager.acquire(runtime(), "session-a".to_owned()).await {
            Ok(_) => panic!("second provisioning request must conflict"),
            Err(error) => error,
        };
        assert!(matches!(conflict, SessionError::RetryableConflict(_)));
        gate.notify_waiters();
        starting.await.expect("start task").expect("first lease");
    }

    #[tokio::test]
    async fn aborted_acquire_and_stop_tasks_do_not_strand_transitional_states() {
        let backend = Arc::new(FakeBackend::default());
        let start_gate = Arc::new(Notify::new());
        *backend.start_gate.lock().expect("start gate lock") = Some(start_gate);
        let manager = SessionManager::new(backend.clone());
        let deployment = runtime();
        let key = SessionKey::new(&deployment, "session-a".to_owned());
        let acquiring_manager = manager.clone();
        let acquiring_runtime = Arc::clone(&deployment);
        let acquiring = tokio::spawn(async move {
            acquiring_manager
                .acquire(acquiring_runtime, "session-a".to_owned())
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(manager.state(&key), Some("starting"));
        acquiring.abort();
        let _ = acquiring.await;
        assert_eq!(manager.state(&key), None);

        *backend.start_gate.lock().expect("start gate lock") = None;
        drop(
            manager
                .acquire(Arc::clone(&deployment), "session-a".to_owned())
                .await
                .expect("ready session"),
        );
        let stop_gate = Arc::new(Notify::new());
        *backend.stop_gate.lock().expect("stop gate lock") = Some(Arc::clone(&stop_gate));
        let stopping_manager = manager.clone();
        let stopping_runtime = Arc::clone(&deployment);
        let stopping = tokio::spawn(async move {
            stopping_manager
                .stop(&stopping_runtime, "session-a".to_owned(), None)
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(manager.state(&key), Some("stopping"));
        stopping.abort();
        let _ = stopping.await;
        tokio::task::yield_now().await;
        stop_gate.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), async {
            while manager.state(&key).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop guard completes stop");
    }

    #[tokio::test]
    async fn stop_then_invoke_provisions_a_fresh_container() {
        let backend = Arc::new(FakeBackend::default());
        let manager = SessionManager::new(backend.clone());
        let first = manager
            .acquire(runtime(), "session-a".to_owned())
            .await
            .expect("first lease");
        let first_id = first.container.id.clone();
        drop(first);
        manager
            .stop(
                &runtime(),
                "session-a".to_owned(),
                Some("stop-token".to_owned()),
            )
            .await
            .expect("stop session");
        let second = manager
            .acquire(runtime(), "session-a".to_owned())
            .await
            .expect("reprovision lease");
        assert_ne!(second.container.id, first_id);
        assert_eq!(
            backend.stops.lock().expect("stops lock").as_slice(),
            [first_id]
        );
    }

    #[tokio::test]
    async fn failed_stop_blocks_reuse_until_cleanup_is_retried() {
        let backend = Arc::new(FakeBackend::default());
        *backend.stop_failures.lock().expect("stop failures lock") = 1;
        let manager = SessionManager::new(backend.clone());
        let deployment = runtime();
        drop(
            manager
                .acquire(Arc::clone(&deployment), "session-a".to_owned())
                .await
                .expect("lease"),
        );
        manager
            .stop(
                &deployment,
                "session-a".to_owned(),
                Some("cleanup".to_owned()),
            )
            .await
            .expect_err("first cleanup fails");
        assert!(matches!(
            manager
                .acquire(Arc::clone(&deployment), "session-a".to_owned())
                .await,
            Err(SessionError::Provisioning(_))
        ));
        manager
            .stop(
                &deployment,
                "session-a".to_owned(),
                Some("cleanup".to_owned()),
            )
            .await
            .expect("cleanup retry succeeds");
        let replacement = manager
            .acquire(deployment, "session-a".to_owned())
            .await
            .expect("fresh session after confirmed cleanup");
        assert_eq!(replacement.container.id, "container-2");
    }

    #[tokio::test]
    async fn concurrent_idempotent_stops_share_cleanup_failure() {
        let backend = Arc::new(FakeBackend::default());
        let gate = Arc::new(Notify::new());
        *backend.stop_gate.lock().expect("stop gate lock") = Some(Arc::clone(&gate));
        *backend.stop_failures.lock().expect("stop failures lock") = 1;
        let manager = SessionManager::new(backend);
        let deployment = runtime();
        drop(
            manager
                .acquire(Arc::clone(&deployment), "session-a".to_owned())
                .await
                .expect("lease"),
        );
        let first_manager = manager.clone();
        let first_runtime = Arc::clone(&deployment);
        let first = tokio::spawn(async move {
            first_manager
                .stop(
                    &first_runtime,
                    "session-a".to_owned(),
                    Some("same-token".to_owned()),
                )
                .await
        });
        tokio::task::yield_now().await;
        let second_manager = manager.clone();
        let second_runtime = Arc::clone(&deployment);
        let second = tokio::spawn(async move {
            second_manager
                .stop(
                    &second_runtime,
                    "session-a".to_owned(),
                    Some("same-token".to_owned()),
                )
                .await
        });
        tokio::task::yield_now().await;
        gate.notify_waiters();
        assert!(matches!(
            first.await.expect("first stop task"),
            Err(SessionError::Stopping(_))
        ));
        assert!(matches!(
            second.await.expect("second stop task"),
            Err(SessionError::Stopping(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_stop_is_idempotent_only_for_the_same_client_token() {
        let backend = Arc::new(FakeBackend::default());
        let gate = Arc::new(Notify::new());
        *backend.stop_gate.lock().expect("stop gate lock") = Some(Arc::clone(&gate));
        let manager = SessionManager::new(backend);
        let deployment = runtime();
        drop(
            manager
                .acquire(Arc::clone(&deployment), "session-a".to_owned())
                .await
                .expect("lease"),
        );

        let stopping_manager = manager.clone();
        let stopping_runtime = Arc::clone(&deployment);
        let first = tokio::spawn(async move {
            stopping_manager
                .stop(
                    &stopping_runtime,
                    "session-a".to_owned(),
                    Some("same-token".to_owned()),
                )
                .await
        });
        tokio::task::yield_now().await;
        let key = SessionKey::new(&deployment, "session-a".to_owned());
        assert_eq!(manager.state(&key), Some("stopping"));
        let conflict = manager
            .stop(
                &deployment,
                "session-a".to_owned(),
                Some("other-token".to_owned()),
            )
            .await
            .expect_err("different stop token conflicts");
        assert!(matches!(conflict, SessionError::RetryableConflict(_)));
        let duplicate_manager = manager.clone();
        let duplicate_runtime = Arc::clone(&deployment);
        let duplicate = tokio::spawn(async move {
            duplicate_manager
                .stop(
                    &duplicate_runtime,
                    "session-a".to_owned(),
                    Some("same-token".to_owned()),
                )
                .await
        });
        tokio::task::yield_now().await;
        gate.notify_waiters();
        first.await.expect("first stop task").expect("first stop");
        duplicate
            .await
            .expect("duplicate stop task")
            .expect("idempotent duplicate stop");
        assert_eq!(manager.state(&key), None);
    }

    #[tokio::test]
    async fn active_request_blocks_idle_reaping_but_not_maximum_lifetime() {
        let backend = Arc::new(FakeBackend::default());
        let manager = SessionManager::new(backend.clone());
        let deployment = runtime();
        let lease = manager
            .acquire(Arc::clone(&deployment), "session-a".to_owned())
            .await
            .expect("lease");
        let key = SessionKey::new(&deployment, "session-a".to_owned());
        {
            let mut entries = manager.inner.entries.lock().expect("session state lock");
            let SessionEntry::Ready {
                idle_timeout_seconds,
                maximum_lifetime_seconds,
                started_at,
                last_activity,
                ..
            } = entries.get_mut(&key).expect("ready session")
            else {
                panic!("session is not ready");
            };
            *idle_timeout_seconds = 0;
            *maximum_lifetime_seconds = 60;
            *started_at = Instant::now();
            *last_activity = Instant::now() - Duration::from_secs(1);
        }
        manager.reap_once().await;
        assert_eq!(manager.state(&key), Some("ready"));
        assert!(backend.stops.lock().expect("stops lock").is_empty());

        {
            let mut entries = manager.inner.entries.lock().expect("session state lock");
            let SessionEntry::Ready {
                maximum_lifetime_seconds,
                started_at,
                ..
            } = entries.get_mut(&key).expect("ready session")
            else {
                panic!("session is not ready");
            };
            *maximum_lifetime_seconds = 0;
            *started_at = Instant::now() - Duration::from_secs(1);
        }
        manager.reap_once().await;
        assert!(lease.cancellation.is_cancelled());
        assert_eq!(manager.state(&key), None);
        assert_eq!(backend.stops.lock().expect("stops lock").len(), 1);
    }

    #[tokio::test]
    async fn lifecycle_reaper_retries_transient_cleanup_failure() {
        let backend = Arc::new(FakeBackend::default());
        *backend.stop_failures.lock().expect("stop failures lock") = 1;
        let manager = SessionManager::new(backend.clone());
        let deployment = runtime();
        drop(
            manager
                .acquire(Arc::clone(&deployment), "session-a".to_owned())
                .await
                .expect("lease"),
        );
        let key = SessionKey::new(&deployment, "session-a".to_owned());
        {
            let mut entries = manager.inner.entries.lock().expect("session state lock");
            let SessionEntry::Ready {
                idle_timeout_seconds,
                last_activity,
                ..
            } = entries.get_mut(&key).expect("ready session")
            else {
                panic!("session is not ready");
            };
            *idle_timeout_seconds = 0;
            *last_activity = Instant::now() - Duration::from_secs(1);
        }
        manager.reap_once().await;
        assert_eq!(manager.state(&key), Some("failed"));
        manager.reap_once().await;
        assert_eq!(manager.state(&key), None);
        assert_eq!(backend.stops.lock().expect("stops lock").len(), 2);
    }

    #[tokio::test]
    async fn idle_busy_session_is_retained_then_removed_when_idle() {
        let backend = Arc::new(FakeBackend::default());
        let manager = SessionManager::new(backend.clone());
        let deployment = runtime();
        let lease = manager
            .acquire(Arc::clone(&deployment), "session-a".to_owned())
            .await
            .expect("lease");
        let key = SessionKey::new(&deployment, "session-a".to_owned());
        backend
            .health
            .lock()
            .expect("health lock")
            .insert(lease.container.id.clone(), SessionHealth::HealthyBusy);
        drop(lease);
        tokio::time::sleep(Duration::from_millis(1)).await;
        {
            let mut entries = manager.inner.entries.lock().expect("session state lock");
            let SessionEntry::Ready {
                idle_timeout_seconds,
                last_activity,
                ..
            } = entries.get_mut(&key).expect("ready session")
            else {
                panic!("session is not ready");
            };
            *idle_timeout_seconds = 0;
            *last_activity = Instant::now() - Duration::from_secs(1);
        }
        manager.reap_once().await;
        assert_eq!(manager.state(&key), Some("ready"));

        let container_id = {
            let entries = manager.inner.entries.lock().expect("session state lock");
            let SessionEntry::Ready { container, .. } = entries.get(&key).expect("ready session")
            else {
                panic!("session is not ready");
            };
            container.id.clone()
        };
        backend
            .health
            .lock()
            .expect("health lock")
            .insert(container_id, SessionHealth::Healthy);
        {
            let mut entries = manager.inner.entries.lock().expect("session state lock");
            let SessionEntry::Ready { last_activity, .. } =
                entries.get_mut(&key).expect("ready session")
            else {
                panic!("session is not ready");
            };
            *last_activity = Instant::now() - Duration::from_secs(1);
        }
        manager.reap_once().await;
        assert_eq!(manager.state(&key), None);
        assert_eq!(backend.stops.lock().expect("stops lock").len(), 1);
    }
}

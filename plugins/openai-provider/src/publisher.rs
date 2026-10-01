//! Invocation-owned output ordering and bounded synchronous callback handoff.
use std::future::Future;
use std::sync::{Arc, Mutex};

use nefor_plugin_sdk::{checked_send, TransportError};
use nefor_protocol::PluginOutgoing;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub const ADMISSION_FAILURE: &str =
    "provider output admission failed; earlier or external effects may have occurred";

#[derive(Debug, thiserror::Error)]
pub enum OutputFailure {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("invocation output publisher full")]
    Full,
}

impl OutputFailure {
    pub fn recoverable(&self) -> bool {
        matches!(
            self,
            Self::Full
                | Self::Transport(
                    TransportError::FrameTooLarge { .. } | TransportError::InvalidFraming
                )
        )
    }
    pub fn category(&self) -> &'static str {
        match self {
            Self::Full => "output_backpressure",
            Self::Transport(_) => "output_admission",
        }
    }
    pub fn into_transport(self) -> TransportError {
        match self {
            Self::Transport(error) => error,
            Self::Full => {
                TransportError::Io(std::io::Error::other("invocation output publisher full"))
            }
        }
    }
}

type Item = (PluginOutgoing, Option<oneshot::Sender<()>>);

#[derive(Clone)]
pub struct Publisher {
    queue: mpsc::Sender<Item>,
    overflow: CancellationToken,
    abort: CancellationToken,
    abandoned: CancellationToken,
}

impl Publisher {
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<Item>) {
        let (queue, rx) = mpsc::channel(capacity);
        (
            Self {
                queue,
                overflow: CancellationToken::new(),
                abort: CancellationToken::new(),
                abandoned: CancellationToken::new(),
            },
            rx,
        )
    }

    pub fn for_invocation(
        capacity: usize,
        parent: &CancellationToken,
        abandoned: &CancellationToken,
    ) -> (Self, mpsc::Receiver<Item>) {
        let (mut publisher, rx) = Self::new(capacity);
        publisher.abort = parent.child_token();
        publisher.abandoned = abandoned.clone();
        (publisher, rx)
    }

    pub fn abandonment_token(&self) -> CancellationToken {
        self.abandoned.clone()
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.abort.clone()
    }

    pub fn try_send(&self, message: PluginOutgoing) -> Result<(), ()> {
        if self.overflow.is_cancelled() || self.queue.try_send((message, None)).is_err() {
            self.overflow.cancel();
            self.abort.cancel();
            Err(())
        } else {
            Ok(())
        }
    }

    pub async fn send(&self, message: PluginOutgoing) -> Result<(), ()> {
        let (tx, rx) = oneshot::channel();
        if self.queue.send((message, Some(tx))).await.is_err() || rx.await.is_err() {
            // The owner handles the failure. Never let a caller ignoring this
            // result advance to a later success after failed admission.
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    pub async fn run<F: Future<Output = ()>>(
        &self,
        wire: &mpsc::Sender<PluginOutgoing>,
        mut rx: mpsc::Receiver<Item>,
        stop: &CancellationToken,
        work: F,
    ) -> Result<(), OutputFailure> {
        tokio::pin!(work);
        let mut completed = false;
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                _ = self.abandoned.cancelled() => return Ok(()),
                _ = self.overflow.cancelled() => return Err(OutputFailure::Full),
                _ = wire.closed() => return Err(TransportError::WriterClosed.into()),
                _ = &mut work, if !completed => {
                    completed = true;
                    rx.close();
                }
                item = rx.recv() => match item {
                    Some((message, ack)) => {
                        tokio::select! {
                            biased;
                            _ = stop.cancelled() => return Ok(()),
                            _ = self.abandoned.cancelled() => return Ok(()),
                            _ = self.overflow.cancelled() => return Err(OutputFailure::Full),
                            result = checked_send(wire, message) => result?,
                        }
                        if let Some(ack) = ack { let _ = ack.send(()); }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

#[derive(Default)]
pub struct Tasks {
    tasks: Mutex<JoinSet<Result<(), TransportError>>>,
    pub stop: CancellationToken,
}

tokio::task_local! { pub static TASKS: Arc<Tasks>; }

pub fn shutdown_token() -> CancellationToken {
    TASKS
        .try_with(|tasks| tasks.stop.clone())
        .unwrap_or_default()
}

pub fn spawn(work: impl Future<Output = Result<(), TransportError>> + Send + 'static) {
    let mut work = Some(work);
    if TASKS
        .try_with(|tasks| match tasks.tasks.lock() {
            Ok(mut set) => {
                if let Some(work) = work.take() {
                    set.spawn(work);
                }
            }
            Err(_) => panic!("provider task registry poisoned"),
        })
        .is_err()
    {
        if let Some(work) = work {
            tokio::spawn(work);
        }
    }
}

pub async fn terminal(
    wire: &mpsc::Sender<PluginOutgoing>,
    stop: &CancellationToken,
    abandoned: &CancellationToken,
    mut message: PluginOutgoing,
    category: &str,
) -> Result<(), TransportError> {
    if let nefor_protocol::Body::Event(body) = &mut message.body {
        body.insert(
            "error_category".into(),
            serde_json::Value::String(category.into()),
        );
    }
    tokio::select! {
        biased;
        _ = stop.cancelled() => Ok(()),
        _ = abandoned.cancelled() => Ok(()),
        result = checked_send(wire, message) => result,
    }
}

impl Tasks {
    pub async fn monitor(&self) -> Result<(), super::error::LlmError> {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(10));
        loop {
            tick.tick().await;
            self.reap()?;
        }
    }

    pub fn reap(&self) -> Result<(), super::error::LlmError> {
        let mut tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(_) => panic!("provider task registry poisoned"),
        };
        while let Some(result) = tasks.try_join_next() {
            result.map_err(|error| super::error::LlmError::Task(error.to_string()))??;
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), super::error::LlmError> {
        self.stop.cancel();
        let mut tasks = match self.tasks.lock() {
            Ok(mut tasks) => std::mem::take(&mut *tasks),
            Err(_) => panic!("provider task registry poisoned"),
        };
        let mut failure = None;
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result
                .map_err(|error| super::error::LlmError::Task(error.to_string()))
                .and_then(|result| result.map_err(Into::into))
            {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

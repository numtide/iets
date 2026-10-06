//! The `Scheduler.Schedule` stream (client/build.cc:`GrpcStore::Run`): a
//! `Want` for each derivation whose inputs are built, answered `Assigned`
//! (build it on that worker), `Cached` or `Unplaceable`. The worker turns a
//! `BuildDerivation` away unless the scheduler announced it.
//!
//! The scheduler keeps soft state only, so a stream that breaks or says
//! `Restarting` is opened again and every unanswered `Want` goes out again
//! (`scheduleUntilDone`).

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::transport::Channel;

use crate::error::{Error, Result};

use super::health::Health;
use super::proto::sched_msg::Msg;
use super::proto::scheduler_client::SchedulerClient;
use super::{Client, GrpcConfig, Headers, c_request, first_line, proto};

pub enum Answer {
    Assigned { worker: String, assign_id: u64 },
    Cached,
}

#[derive(Default)]
struct State {
    tx: Option<UnboundedSender<proto::ClientMsgs>>,
    open: HashMap<String, (proto::Want, mpsc::Sender<Msg>)>,
    fatal: Option<String>,
}

/// One `Schedule` stream, shared by the threads of one batch.
pub struct Sched {
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
    grace: Duration,
    /// What the batch has learnt about the addresses it was assigned.
    pub health: Health,
}

impl Sched {
    pub fn open(c: &Client) -> Result<Sched> {
        let state = Arc::new(Mutex::new(State::default()));
        let task = c.rt.spawn(pump(
            c.sched.clone(),
            c.headers()?,
            c.cfg.clone(),
            Arc::clone(&state),
        ));
        Ok(Sched {
            state,
            task,
            grace: Duration::from_secs(c.cfg.restart_grace),
            health: Health::default(),
        })
    }

    /// `restart-grace`, which also bounds the refusals in `build::build`.
    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// Sends `want` and waits for the scheduler's answer. `Unplaceable` is
    /// advisory: the `Want` stays queued, and fails only once `restart-grace`
    /// passes without an `Assigned`.
    pub fn place(&self, want: proto::Want) -> Result<Answer> {
        let drv = want.drv_path.clone();
        let (tx, rx) = mpsc::channel();
        {
            let mut s = lock(&self.state);
            if let Some(e) = &s.fatal {
                return Err(Error::Protocol(e.clone()));
            }
            if let Some(out) = &s.tx {
                let _ = out.send(batch(vec![want.clone()]));
            }
            s.open.insert(drv.clone(), (want, tx));
        }
        let mut deadline: Option<Instant> = None;
        let mut reason = String::new();
        loop {
            let msg = match deadline {
                None => rx.recv().ok(),
                Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => {
                        lock(&self.state).open.remove(&drv);
                        return Err(Error::Store(format!("{drv}: {reason}")));
                    }
                    Err(RecvTimeoutError::Disconnected) => None,
                },
            };
            match msg {
                Some(Msg::Assigned(a)) => {
                    return Ok(Answer::Assigned {
                        worker: a.worker_addr,
                        assign_id: a.assign_id,
                    });
                }
                Some(Msg::Cached(_)) => return Ok(Answer::Cached),
                Some(Msg::Unplaceable(u)) => {
                    if deadline.is_none() {
                        crate::errln!(
                            "{drv}: {}, waiting up to {}s for a worker",
                            u.reason,
                            self.grace.as_secs()
                        );
                        deadline = Some(Instant::now() + self.grace);
                    }
                    reason = u.reason;
                }
                Some(_) => {}
                None => {
                    let s = lock(&self.state);
                    let e = s.fatal.as_deref().unwrap_or("the scheduler stream closed");
                    return Err(Error::Protocol(e.to_string()));
                }
            }
        }
    }
}

impl Drop for Sched {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn batch(wants: Vec<proto::Want>) -> proto::ClientMsgs {
    proto::ClientMsgs {
        msgs: wants
            .into_iter()
            .map(|w| proto::ClientMsg {
                msg: Some(proto::client_msg::Msg::Want(w)),
            })
            .collect(),
    }
}

enum End {
    Restarting,
    Closed,
    Failed(tonic::Status),
}

/// build.cc:`schedulerGone`: worth reconnecting for, unlike auth or TLS.
fn scheduler_gone(s: &tonic::Status) -> bool {
    matches!(
        s.code(),
        tonic::Code::Unavailable | tonic::Code::Unknown | tonic::Code::Internal
    ) && !s.message().contains("andshake")
        && !s.message().contains("certificate")
}

async fn pump(
    mut stub: SchedulerClient<Channel>,
    headers: Headers,
    cfg: GrpcConfig,
    state: Arc<Mutex<State>>,
) {
    let grace = Duration::from_secs(cfg.restart_grace);
    let first_pause = Duration::from_millis(500);
    let mut pause = first_pause;
    let mut give_up: Option<Instant> = None;
    loop {
        let (tx, rx) = unbounded_channel();
        {
            let mut s = lock(&state);
            let wants: Vec<proto::Want> = s.open.values().map(|(w, _)| w.clone()).collect();
            if !wants.is_empty() {
                let _ = tx.send(batch(wants));
            }
            s.tx = Some(tx);
        }
        let (answered, end) = stream(&mut stub, &headers, rx, &state).await;
        lock(&state).tx = None;
        let status = match end {
            End::Restarting => {
                pause = first_pause;
                give_up = None;
                continue;
            }
            End::Closed => {
                return fail(
                    &state,
                    format!("the scheduler at '{}' closed the stream", cfg.authority),
                );
            }
            End::Failed(s) if scheduler_gone(&s) => s,
            End::Failed(s) => return fail(&state, cfg.status_error(&s, "Schedule").to_string()),
        };
        let now = Instant::now();
        if answered || give_up.is_none() {
            give_up = Some(now + grace);
            pause = first_pause;
        }
        if give_up.is_some_and(|g| now + pause > g) {
            return fail(&state, cfg.status_error(&status, "Schedule").to_string());
        }
        crate::errln!(
            "scheduler at {}: {}, reconnecting",
            cfg.authority,
            first_line(status.message())
        );
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(8));
    }
}

/// One `Schedule` stream until it ends: whether it answered anything, and how.
async fn stream(
    stub: &mut SchedulerClient<Channel>,
    headers: &Headers,
    rx: UnboundedReceiver<proto::ClientMsgs>,
    state: &Mutex<State>,
) -> (bool, End) {
    let req = c_request(UnboundedReceiverStream::new(rx), headers);
    let mut stream = match stub.schedule(req).await {
        Ok(r) => r.into_inner(),
        Err(s) => return (false, End::Failed(s)),
    };
    let mut answered = false;
    loop {
        let msgs = match stream.next().await {
            None => return (answered, End::Closed),
            Some(Err(s)) => return (answered, End::Failed(s)),
            Some(Ok(m)) => m,
        };
        answered = true;
        let mut s = lock(state);
        for msg in msgs.msgs.into_iter().filter_map(|m| m.msg) {
            let drv = match &msg {
                Msg::Restarting(_) => return (answered, End::Restarting),
                Msg::Assigned(a) if a.worker_addr.is_empty() => continue,
                Msg::Assigned(a) => a.drv_path.clone(),
                Msg::Cached(c) => c.drv_path.clone(),
                Msg::Unplaceable(u) => u.drv_path.clone(),
            };
            let settled = !matches!(msg, Msg::Unplaceable(_));
            if let Some((_, tx)) = s.open.get(&drv) {
                let _ = tx.send(msg);
                if settled {
                    s.open.remove(&drv);
                }
            }
        }
    }
}

/// Every waiting `place` returns this error.
fn fail(state: &Mutex<State>, e: String) {
    let mut s = lock(state);
    s.fatal = Some(e);
    s.open.clear();
    s.tx = None;
}

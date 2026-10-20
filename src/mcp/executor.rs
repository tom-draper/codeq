use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::Value;

type Writer = Arc<Mutex<io::Stdout>>;
type Requests = Arc<Mutex<HashMap<String, Arc<Control>>>>;

#[derive(Default)]
struct Control {
    cancelled: AtomicBool,
    interrupt: Mutex<Option<rusqlite::InterruptHandle>>,
}

impl Control {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(handle) = self.interrupt.lock().unwrap().as_ref() {
            handle.interrupt();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Control>>> = const { RefCell::new(None) };
}

pub(crate) fn check_cancelled() -> anyhow::Result<()> {
    if CURRENT.with(|slot| slot.borrow().as_ref().is_some_and(|c| c.is_cancelled())) {
        anyhow::bail!("request cancelled");
    }
    Ok(())
}

pub(crate) fn attach_connection(conn: &rusqlite::Connection) {
    CURRENT.with(|slot| {
        if let Some(control) = slot.borrow().as_ref() {
            *control.interrupt.lock().unwrap() = Some(conn.get_interrupt_handle());
        }
    });
}

fn write_reply(writer: &Writer, value: &Value) {
    let mut out = writer.lock().unwrap();
    // A closed host ends the session; query output never writes to this stream.
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

struct Batch {
    remaining: usize,
    replies: Vec<Value>,
}

#[derive(Clone)]
struct Reply {
    writer: Writer,
    batch: Option<Arc<Mutex<Batch>>>,
}

impl Reply {
    fn complete(&self, value: Option<Value>) {
        if let Some(batch) = &self.batch {
            let mut batch = batch.lock().unwrap();
            batch.remaining -= 1;
            batch.replies.extend(value);
            if batch.remaining == 0 && !batch.replies.is_empty() {
                write_reply(
                    &self.writer,
                    &Value::Array(std::mem::take(&mut batch.replies)),
                );
            }
        } else if let Some(value) = value {
            write_reply(&self.writer, &value);
        }
    }
}

struct Job {
    request: Value,
    key: String,
    control: Arc<Control>,
    reply: Reply,
}

pub(super) struct Executor {
    send: Option<mpsc::SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
    requests: Requests,
    writer: Writer,
    cwd: PathBuf,
    profile: super::ToolProfile,
}

impl Executor {
    pub(super) fn new(cwd: &Path, workers: u8, profile: super::ToolProfile) -> Self {
        let (send, receive) = mpsc::sync_channel::<Job>(32);
        let receive = Arc::new(Mutex::new(receive));
        let requests: Requests = Arc::default();
        let mut handles = Vec::new();
        for _ in 0..workers {
            let receive = receive.clone();
            let requests = requests.clone();
            let cwd = cwd.to_path_buf();
            handles.push(std::thread::spawn(move || loop {
                let job = match receive.lock().unwrap().recv() {
                    Ok(job) => job,
                    Err(_) => return,
                };
                CURRENT.with(|slot| slot.replace(Some(job.control.clone())));
                let reply = if job.control.is_cancelled() {
                    None
                } else {
                    super::respond(&job.request, &cwd, profile)
                };
                CURRENT.with(|slot| slot.replace(None));
                // Serialize cancellation with completion, including removal of the
                // handle, so a late cancel cannot interrupt the worker's next query.
                let mut active = requests.lock().unwrap();
                let reply = reply.filter(|_| !job.control.is_cancelled());
                active.remove(&job.key);
                job.reply.complete(reply);
            }));
        }
        Self {
            send: Some(send),
            workers: handles,
            requests,
            writer: Arc::new(Mutex::new(io::stdout())),
            cwd: cwd.to_path_buf(),
            profile,
        }
    }

    pub(super) fn submit(&self, value: Value) {
        let reply = Reply {
            writer: self.writer.clone(),
            batch: None,
        };
        match value {
            Value::Array(batch) if batch.is_empty() => {
                reply.complete(Some(super::error_reply(
                    Value::Null,
                    -32600,
                    "invalid request: empty batch".into(),
                )));
            }
            Value::Array(batch) => {
                let reply = Reply {
                    batch: Some(Arc::new(Mutex::new(Batch {
                        remaining: batch.len(),
                        replies: Vec::new(),
                    }))),
                    ..reply
                };
                for request in batch {
                    self.submit_one(request, reply.clone());
                }
            }
            request => self.submit_one(request, reply),
        }
    }

    pub(super) fn parse_error(&self, message: String) {
        write_reply(
            &self.writer,
            &super::error_reply(Value::Null, -32700, message),
        );
    }

    fn submit_one(&self, request: Value, reply: Reply) {
        if request["method"] == "notifications/cancelled" && request.get("id").is_none() {
            if let Some(id) = request["params"].get("requestId") {
                if let Some(control) = self.requests.lock().unwrap().get(&id.to_string()) {
                    control.cancel();
                }
            }
            reply.complete(None);
            return;
        }
        if request["method"] != "tools/call" || request.get("id").is_none_or(Value::is_null) {
            reply.complete(super::respond(&request, &self.cwd, self.profile));
            return;
        }
        let id = request["id"].clone();
        let key = id.to_string();
        let control = Arc::new(Control::default());
        {
            let mut active = self.requests.lock().unwrap();
            if active.contains_key(&key) {
                reply.complete(Some(super::error_reply(
                    id,
                    -32600,
                    "request ID is already active".into(),
                )));
                return;
            }
            active.insert(key.clone(), control.clone());
        }
        let job = Job {
            request,
            key: key.clone(),
            control,
            reply,
        };
        if let Err(error) = self.send.as_ref().unwrap().try_send(job) {
            let (mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) = error;
            self.requests.lock().unwrap().remove(&key);
            job.reply.complete(Some(super::error_reply(
                id,
                -32000,
                "query queue is full; retry after an active query completes".into(),
            )));
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        self.send.take();
        // EOF drains accepted requests, which supports one-shot piped clients.
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

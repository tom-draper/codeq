use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use super::Parsed;

struct State {
    items: VecDeque<(Parsed, usize)>,
    bytes: usize,
    senders: usize,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    limit: usize,
}

pub(super) struct Sender(Arc<Shared>);
pub(super) struct Receiver(Arc<Shared>);

pub(super) fn channel(limit: usize) -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            items: VecDeque::new(),
            bytes: 0,
            senders: 1,
            closed: false,
        }),
        changed: Condvar::new(),
        limit,
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl Sender {
    pub(super) fn send(&self, parsed: Parsed) -> bool {
        let bytes = parsed.estimated_bytes();
        let mut state = self.0.state.lock().unwrap();
        while !state.closed
            && (state.items.len() >= 256
                || (state.bytes > 0 && bytes > self.0.limit.saturating_sub(state.bytes)))
        {
            state = self.0.changed.wait(state).unwrap();
        }
        if state.closed {
            return false;
        }
        state.bytes += bytes;
        state.items.push_back((parsed, bytes));
        self.0.changed.notify_all();
        true
    }
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.0.state.lock().unwrap().senders += 1;
        Self(self.0.clone())
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().senders -= 1;
        self.0.changed.notify_all();
    }
}

impl Iterator for Receiver {
    type Item = Parsed;

    fn next(&mut self) -> Option<Parsed> {
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some((parsed, bytes)) = state.items.pop_front() {
                state.bytes -= bytes;
                self.0.changed.notify_all();
                return Some(parsed);
            }
            if state.senders == 0 {
                return None;
            }
            state = self.0.changed.wait(state).unwrap();
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        state.items.clear();
        state.bytes = 0;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn parsed() -> Parsed {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "fn f() { call(\"value\"); }\n").unwrap();
        super::super::parse_one(
            &Default::default(),
            &mut Default::default(),
            "source.rs",
            file.path(),
            28,
            0,
            0,
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn bytes_count_body_and_extracted_payload_capacity() {
        let mut p = parsed();
        assert!(!p.facts.is_empty() && !p.calls.is_empty() && !p.spans.is_empty());
        let initial = p.estimated_bytes();
        let previous = p.facts[0].source.capacity();
        p.facts[0].source.reserve(4096);
        assert_eq!(
            p.estimated_bytes() - initial,
            p.facts[0].source.capacity() - previous
        );
        let initial = p.estimated_bytes();
        let previous = p.body.as_ref().unwrap().capacity();
        p.body.as_mut().unwrap().reserve(8192);
        assert_eq!(
            p.estimated_bytes() - initial,
            p.body.as_ref().unwrap().capacity() - previous
        );
    }

    #[test]
    fn bytes_apply_backpressure_and_allow_one_oversized_file() {
        for limit in [parsed().estimated_bytes(), 1] {
            let (sender, mut receiver) = channel(limit);
            assert!(sender.send(parsed()));
            let second = sender.clone();
            let (done, finished) = std::sync::mpsc::channel();
            let (start, started) = std::sync::mpsc::channel();
            let p = parsed();
            let worker = std::thread::spawn(move || {
                start.send(()).unwrap();
                done.send(second.send(p)).unwrap();
            });
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(finished.recv_timeout(Duration::from_millis(30)).is_err());
            assert!(receiver.next().is_some());
            assert!(finished.recv_timeout(Duration::from_secs(2)).unwrap());
            drop(sender);
            assert!(receiver.next().is_some());
            assert!(receiver.next().is_none());
            worker.join().unwrap();
        }
    }

    #[test]
    fn dropping_receiver_releases_blocked_senders() {
        let (sender, receiver) = channel(1);
        assert!(sender.send(parsed()));
        let (done, finished) = std::sync::mpsc::channel();
        let p = parsed();
        let worker = std::thread::spawn(move || done.send(sender.send(p)).unwrap());
        assert!(finished.recv_timeout(Duration::from_millis(30)).is_err());
        drop(receiver);
        assert!(!finished.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn dropping_last_sender_wakes_an_empty_receiver() {
        let (sender, mut receiver) = channel(1);
        let (done, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || done.send(receiver.next().is_none()).unwrap());
        drop(sender);
        assert!(finished.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }
}

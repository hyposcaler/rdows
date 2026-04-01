use std::collections::VecDeque;

use rdows_core::queue::CompletionQueueEntry;

pub struct CompletionQueue {
    entries: VecDeque<CompletionQueueEntry>,
    capacity: usize,
}

impl CompletionQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, cqe: CompletionQueueEntry) -> bool {
        if self.entries.len() >= self.capacity {
            return false;
        }
        self.entries.push_back(cqe);
        true
    }

    pub fn poll_cq(&mut self, max: usize) -> Vec<CompletionQueueEntry> {
        let n = std::cmp::min(max, self.entries.len());
        self.entries.drain(..n).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }
}

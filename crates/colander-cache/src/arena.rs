use crate::traits::CachedResponse;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Sentinel value indicating "no node" (null pointer equivalent).
pub const NIL: u32 = u32::MAX;

/// A node in the arena-allocated doubly-linked list.
pub struct Node {
    pub key: String,
    pub value: Arc<CachedResponse>,
    pub visited: AtomicBool,
    pub prev: u32,
    pub next: u32,
}

impl Node {
    pub fn new(key: String, value: CachedResponse) -> Self {
        Self {
            key,
            value: Arc::new(value),
            visited: AtomicBool::new(false),
            prev: NIL,
            next: NIL,
        }
    }

    /// Mark this node as visited (lock-free on cache hit).
    #[inline]
    pub fn mark_visited(&self) {
        self.visited.store(true, Ordering::Relaxed);
    }

    /// Check and clear the visited bit. Returns the previous value.
    #[inline]
    pub fn clear_visited(&self) -> bool {
        self.visited.swap(false, Ordering::Relaxed)
    }

    /// Check if this node has been visited without clearing.
    #[inline]
    pub fn is_visited(&self) -> bool {
        self.visited.load(Ordering::Relaxed)
    }
}

/// Arena-allocated doubly-linked list.
///
/// Nodes are stored in a `Vec<Option<Node>>`. Indices (`u32`) serve as pointers.
/// A free-list tracks reclaimed slots for O(1) allocation.
pub struct Arena {
    slots: Vec<Option<Node>>,
    free_list: Vec<u32>,
    pub head: u32,
    pub tail: u32,
    len: usize,
}

impl Arena {
    /// Create a new arena pre-allocated for `capacity` nodes.
    pub fn new(capacity: usize) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        // Pre-allocate all slots as None
        for _ in 0..capacity {
            slots.push(None);
        }
        // All slots start on the free list (in reverse so we pop from the front)
        let free_list: Vec<u32> = (0..capacity as u32).rev().collect();

        Self {
            slots,
            free_list,
            head: NIL,
            tail: NIL,
            len: 0,
        }
    }

    /// Number of active (occupied) nodes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get a reference to the node at `index`.
    #[inline]
    pub fn get(&self, index: u32) -> Option<&Node> {
        self.slots.get(index as usize).and_then(|s| s.as_ref())
    }

    /// Get a mutable reference to the node at `index`.
    #[inline]
    pub fn get_mut(&mut self, index: u32) -> Option<&mut Node> {
        self.slots.get_mut(index as usize).and_then(|s| s.as_mut())
    }

    /// Allocate a new node and insert it at the head of the list.
    /// Returns the index of the new node, or None if no free slots.
    pub fn push_head(&mut self, node: Node) -> Option<u32> {
        let index = self.free_list.pop()?;

        let slot = &mut self.slots[index as usize];
        *slot = Some(node);

        // Link into list at head
        let node = slot.as_mut().unwrap();
        node.prev = NIL;
        node.next = self.head;

        if self.head != NIL {
            self.slots[self.head as usize].as_mut().unwrap().prev = index;
        }

        self.head = index;

        if self.tail == NIL {
            self.tail = index;
        }

        self.len += 1;
        Some(index)
    }

    /// Remove a node from the list and return it. The slot is reclaimed.
    pub fn remove(&mut self, index: u32) -> Option<Node> {
        let node = self.slots[index as usize].take()?;

        // Unlink from list
        let prev = node.prev;
        let next = node.next;

        if prev != NIL {
            self.slots[prev as usize].as_mut().unwrap().next = next;
        } else {
            // Was head
            self.head = next;
        }

        if next != NIL {
            self.slots[next as usize].as_mut().unwrap().prev = prev;
        } else {
            // Was tail
            self.tail = prev;
        }

        self.free_list.push(index);
        self.len -= 1;
        Some(node)
    }

    /// Move an existing node to the head of the list (used by LRU).
    pub fn move_to_head(&mut self, index: u32) {
        if self.head == index {
            return; // Already at head
        }

        let node = self.slots[index as usize].as_ref().unwrap();
        let prev = node.prev;
        let next = node.next;

        // Unlink from current position
        if prev != NIL {
            self.slots[prev as usize].as_mut().unwrap().next = next;
        }

        if next != NIL {
            self.slots[next as usize].as_mut().unwrap().prev = prev;
        } else {
            // Was tail
            self.tail = prev;
        }

        // Link at head
        let node = self.slots[index as usize].as_mut().unwrap();
        node.prev = NIL;
        node.next = self.head;

        if self.head != NIL {
            self.slots[self.head as usize].as_mut().unwrap().prev = index;
        }

        self.head = index;
    }

    /// Remove the tail node and return it.
    pub fn pop_tail(&mut self) -> Option<(u32, Node)> {
        if self.tail == NIL {
            return None;
        }
        let index = self.tail;
        let node = self.remove(index)?;
        Some((index, node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CachedResponse;
    use bytes::Bytes;
    use std::time::{Duration, Instant};

    fn test_response() -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: vec![],
            body: Bytes::from_static(b"test"),
            inserted_at: Instant::now(),
            ttl: Duration::from_secs(60),
        }
    }

    fn test_node(key: &str) -> Node {
        Node::new(key.to_string(), test_response())
    }

    #[test]
    fn empty_arena() {
        let arena = Arena::new(10);
        assert_eq!(arena.len(), 0);
        assert!(arena.is_empty());
        assert_eq!(arena.head, NIL);
        assert_eq!(arena.tail, NIL);
    }

    #[test]
    fn push_single() {
        let mut arena = Arena::new(10);
        let idx = arena.push_head(test_node("a")).unwrap();
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.head, idx);
        assert_eq!(arena.tail, idx);
        assert_eq!(arena.get(idx).unwrap().key, "a");
    }

    #[test]
    fn push_multiple_maintains_order() {
        let mut arena = Arena::new(10);
        let a = arena.push_head(test_node("a")).unwrap();
        let b = arena.push_head(test_node("b")).unwrap();
        let c = arena.push_head(test_node("c")).unwrap();

        // Order should be: head -> c -> b -> a -> tail
        assert_eq!(arena.head, c);
        assert_eq!(arena.tail, a);
        assert_eq!(arena.get(c).unwrap().next, b);
        assert_eq!(arena.get(b).unwrap().next, a);
        assert_eq!(arena.get(a).unwrap().next, NIL);
    }

    #[test]
    fn remove_middle() {
        let mut arena = Arena::new(10);
        let a = arena.push_head(test_node("a")).unwrap();
        let b = arena.push_head(test_node("b")).unwrap();
        let c = arena.push_head(test_node("c")).unwrap();

        let removed = arena.remove(b).unwrap();
        assert_eq!(removed.key, "b");
        assert_eq!(arena.len(), 2);

        // c -> a
        assert_eq!(arena.get(c).unwrap().next, a);
        assert_eq!(arena.get(a).unwrap().prev, c);
    }

    #[test]
    fn remove_head() {
        let mut arena = Arena::new(10);
        let a = arena.push_head(test_node("a")).unwrap();
        let b = arena.push_head(test_node("b")).unwrap();

        arena.remove(b);
        assert_eq!(arena.head, a);
        assert_eq!(arena.tail, a);
    }

    #[test]
    fn remove_tail() {
        let mut arena = Arena::new(10);
        let a = arena.push_head(test_node("a")).unwrap();
        let b = arena.push_head(test_node("b")).unwrap();

        arena.remove(a);
        assert_eq!(arena.head, b);
        assert_eq!(arena.tail, b);
    }

    #[test]
    fn pop_tail() {
        let mut arena = Arena::new(10);
        arena.push_head(test_node("a"));
        arena.push_head(test_node("b"));
        arena.push_head(test_node("c"));

        let (_, node) = arena.pop_tail().unwrap();
        assert_eq!(node.key, "a");
        assert_eq!(arena.len(), 2);
    }

    #[test]
    fn move_to_head() {
        let mut arena = Arena::new(10);
        let a = arena.push_head(test_node("a")).unwrap();
        let b = arena.push_head(test_node("b")).unwrap();
        let c = arena.push_head(test_node("c")).unwrap();

        // Order: c -> b -> a
        arena.move_to_head(a);
        // Order: a -> c -> b

        assert_eq!(arena.head, a);
        assert_eq!(arena.get(a).unwrap().next, c);
        assert_eq!(arena.get(c).unwrap().next, b);
        assert_eq!(arena.get(b).unwrap().next, NIL);
        assert_eq!(arena.tail, b);
    }

    #[test]
    fn move_head_to_head_is_noop() {
        let mut arena = Arena::new(10);
        let a = arena.push_head(test_node("a")).unwrap();
        let b = arena.push_head(test_node("b")).unwrap();

        arena.move_to_head(b);
        assert_eq!(arena.head, b);
        assert_eq!(arena.tail, a);
    }

    #[test]
    fn slot_reclamation() {
        let mut arena = Arena::new(2);
        let a = arena.push_head(test_node("a")).unwrap();
        let _b = arena.push_head(test_node("b")).unwrap();

        // Arena is full
        assert!(arena.push_head(test_node("c")).is_none());

        // Remove one, slot should be reclaimable
        arena.remove(a);
        let c = arena.push_head(test_node("c")).unwrap();
        assert!(arena.get(c).is_some());
        assert_eq!(arena.len(), 2);
    }

    #[test]
    fn visited_bit_operations() {
        let node = test_node("a");
        assert!(!node.is_visited());

        node.mark_visited();
        assert!(node.is_visited());

        let was_visited = node.clear_visited();
        assert!(was_visited);
        assert!(!node.is_visited());
    }
}

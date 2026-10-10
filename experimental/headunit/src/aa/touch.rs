//! Multi-touch tracking: page pointer events → AA `TouchEvent`s.
//!
//! AA follows Android's MotionEvent model: every event lists ALL pointers
//! currently down, `action` says what happened and `action_index` points at
//! the pointer it happened to (openauto `InputSourceService::onTouchEvent`).
//! First finger down = ACTION_DOWN, further fingers = ACTION_POINTER_DOWN,
//! a finger lifted while others stay = ACTION_POINTER_UP (that pointer is
//! still listed), the last finger lifted = ACTION_UP.

use crate::aa::proto::{pointer_action, Pointer, TouchEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchKind {
    Down,
    Move,
    Up,
}

#[derive(Debug, Clone, Copy)]
struct Active {
    page_id: u32,
    aa_id: u32,
    x: u32,
    y: u32,
}

/// At most this many simultaneous pointers are tracked; extra fingers are ignored.
pub const MAX_POINTERS: usize = 10;

#[derive(Debug, Default)]
pub struct TouchTracker {
    active: Vec<Active>,
}

impl TouchTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pointers_down(&self) -> usize {
        self.active.len()
    }

    fn pointers(&self) -> Vec<Pointer> {
        self.active.iter().map(|a| Pointer { x: a.x, y: a.y, pointer_id: a.aa_id }).collect()
    }

    fn free_id(&self) -> u32 {
        (0..).find(|id| !self.active.iter().any(|a| a.aa_id == *id)).unwrap_or(0)
    }

    /// Apply one page event (`x`,`y` already in touchscreen pixels).
    /// Returns the event to send, or `None` when it must be dropped
    /// (move/up of an unknown pointer, too many fingers).
    pub fn event(&mut self, kind: TouchKind, page_id: u32, x: u32, y: u32) -> Option<TouchEvent> {
        let idx = self.active.iter().position(|a| a.page_id == page_id);
        match kind {
            TouchKind::Down => {
                if let Some(i) = idx {
                    // Duplicate down (lost up): treat as a move, never a second pointer.
                    self.active[i].x = x;
                    self.active[i].y = y;
                    return Some(self.make(pointer_action::MOVED, 0));
                }
                if self.active.len() >= MAX_POINTERS {
                    return None;
                }
                let aa_id = self.free_id();
                self.active.push(Active { page_id, aa_id, x, y });
                let i = self.active.len() - 1;
                let action = if i == 0 { pointer_action::DOWN } else { pointer_action::POINTER_DOWN };
                Some(self.make(action, i as u32))
            }
            TouchKind::Move => {
                let i = idx?;
                self.active[i].x = x;
                self.active[i].y = y;
                Some(self.make(pointer_action::MOVED, 0))
            }
            TouchKind::Up => {
                let i = idx?;
                self.active[i].x = x;
                self.active[i].y = y;
                let action = if self.active.len() == 1 { pointer_action::UP } else { pointer_action::POINTER_UP };
                let ev = self.make(action, i as u32);
                self.active.remove(i);
                Some(ev)
            }
        }
    }

    /// Lift every finger (page lost focus, WebSocket closed). Returns the
    /// events that close each pointer cleanly on the phone.
    pub fn cancel_all(&mut self) -> Vec<TouchEvent> {
        let mut out = Vec::new();
        while let Some(a) = self.active.last().copied() {
            if let Some(ev) = self.event(TouchKind::Up, a.page_id, a.x, a.y) {
                out.push(ev);
            }
        }
        out
    }

    fn make(&self, action: i32, action_index: u32) -> TouchEvent {
        TouchEvent { pointer_data: self.pointers(), action_index: Some(action_index), action: Some(action) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ev: &TouchEvent) -> Vec<u32> {
        ev.pointer_data.iter().map(|p| p.pointer_id).collect()
    }

    #[test]
    fn single_tap() {
        let mut t = TouchTracker::new();
        let d = t.event(TouchKind::Down, 7, 10, 20).unwrap();
        assert_eq!((d.action, d.action_index), (Some(pointer_action::DOWN), Some(0)));
        assert_eq!(d.pointer_data, vec![Pointer { x: 10, y: 20, pointer_id: 0 }]);
        let m = t.event(TouchKind::Move, 7, 11, 21).unwrap();
        assert_eq!(m.action, Some(pointer_action::MOVED));
        let u = t.event(TouchKind::Up, 7, 12, 22).unwrap();
        assert_eq!((u.action, u.action_index), (Some(pointer_action::UP), Some(0)));
        assert_eq!(u.pointer_data, vec![Pointer { x: 12, y: 22, pointer_id: 0 }]);
        assert_eq!(t.pointers_down(), 0);
    }

    #[test]
    fn pinch_two_fingers() {
        let mut t = TouchTracker::new();
        t.event(TouchKind::Down, 100, 0, 0).unwrap();
        let d2 = t.event(TouchKind::Down, 200, 50, 50).unwrap();
        assert_eq!((d2.action, d2.action_index), (Some(pointer_action::POINTER_DOWN), Some(1)));
        assert_eq!(ids(&d2), vec![0, 1]);
        // First finger lifts while the second stays: POINTER_UP, index 0, both listed.
        let u1 = t.event(TouchKind::Up, 100, 1, 1).unwrap();
        assert_eq!((u1.action, u1.action_index), (Some(pointer_action::POINTER_UP), Some(0)));
        assert_eq!(ids(&u1), vec![0, 1]);
        // A new finger reuses the freed AA id 0.
        let d3 = t.event(TouchKind::Down, 300, 9, 9).unwrap();
        assert_eq!(ids(&d3), vec![1, 0]);
        assert_eq!(d3.action_index, Some(1));
        let u2 = t.event(TouchKind::Up, 200, 50, 50).unwrap();
        assert_eq!(u2.action, Some(pointer_action::POINTER_UP));
        let u3 = t.event(TouchKind::Up, 300, 9, 9).unwrap();
        assert_eq!(u3.action, Some(pointer_action::UP));
    }

    #[test]
    fn unknown_pointer_move_and_up_are_dropped() {
        let mut t = TouchTracker::new();
        assert!(t.event(TouchKind::Move, 1, 0, 0).is_none());
        assert!(t.event(TouchKind::Up, 1, 0, 0).is_none());
    }

    #[test]
    fn duplicate_down_does_not_add_a_pointer() {
        let mut t = TouchTracker::new();
        t.event(TouchKind::Down, 1, 0, 0).unwrap();
        let again = t.event(TouchKind::Down, 1, 5, 5).unwrap();
        assert_eq!(again.action, Some(pointer_action::MOVED));
        assert_eq!(t.pointers_down(), 1);
    }

    #[test]
    fn cancel_all_lifts_everything() {
        let mut t = TouchTracker::new();
        t.event(TouchKind::Down, 1, 0, 0).unwrap();
        t.event(TouchKind::Down, 2, 0, 0).unwrap();
        let evs = t.cancel_all();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].action, Some(pointer_action::POINTER_UP));
        assert_eq!(evs[1].action, Some(pointer_action::UP));
        assert_eq!(t.pointers_down(), 0);
    }

    #[test]
    fn pointer_cap() {
        let mut t = TouchTracker::new();
        for i in 0..MAX_POINTERS as u32 {
            assert!(t.event(TouchKind::Down, i, 0, 0).is_some());
        }
        assert!(t.event(TouchKind::Down, 99, 0, 0).is_none());
    }
}

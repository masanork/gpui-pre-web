// SPDX-License-Identifier: Apache-2.0
//! Browser touch arbitration. Once a second finger joins, suppress the
//! single-touch stream until every finger is lifted, even after cancellation.
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Phase {
    Started,
    Moved,
    Ended,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pinch {
    pub center: (f32, f32),
    pub delta: f32,
    pub phase: Phase,
}
#[derive(Default)]
pub(crate) struct TouchPinch {
    contacts: BTreeMap<i32, (f32, f32)>,
    pair: Option<(i32, i32)>,
    distance: f32,
    multi: bool,
}
impl TouchPinch {
    pub fn start(&mut self, id: i32, position: (f32, f32)) -> Option<i32> {
        let first = self.contacts.keys().next().copied();
        self.contacts.insert(id, position);
        if !self.multi && self.contacts.len() == 2 {
            let first = first.unwrap();
            self.multi = true;
            self.pair = Some((first, id));
            self.distance = self.geometry().unwrap().1;
            return Some(first);
        }
        None
    }
    pub fn is_multi(&self) -> bool {
        self.multi
    }
    pub fn position(&self, id: i32) -> Option<(f32, f32)> {
        self.contacts.get(&id).copied()
    }
    fn geometry(&self) -> Option<((f32, f32), f32)> {
        let (a, b) = self.pair?;
        let a = *self.contacts.get(&a)?;
        let b = *self.contacts.get(&b)?;
        Some((
            ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5),
            (a.0 - b.0).hypot(a.1 - b.1),
        ))
    }
    pub fn started(&self) -> Option<Pinch> {
        let (center, _) = self.geometry()?;
        Some(Pinch {
            center,
            delta: 0.,
            phase: Phase::Started,
        })
    }
    pub fn moved(&mut self, id: i32, position: (f32, f32)) -> Option<Pinch> {
        *self.contacts.get_mut(&id)? = position;
        let (center, distance) = self.geometry()?;
        if !self.pair.is_some_and(|(a, b)| id == a || id == b) {
            return None;
        }
        // Coincident contacts do not define a scale. Establish a fresh baseline.
        let delta = if self.distance >= 1. && distance >= 1. {
            distance / self.distance - 1.
        } else {
            0.
        };
        self.distance = distance;
        Some(Pinch {
            center,
            delta,
            phase: Phase::Moved,
        })
    }
    pub fn end(&mut self, id: i32, cancelled: bool) -> Option<Pinch> {
        let is_pair = self.pair.is_some_and(|(a, b)| id == a || id == b);
        let result = if is_pair {
            self.geometry().map(|(center, _)| Pinch {
                center,
                delta: 0.,
                phase: if cancelled {
                    Phase::Cancelled
                } else {
                    Phase::Ended
                },
            })
        } else {
            None
        };
        self.contacts.remove(&id);
        if is_pair {
            self.pair = None;
        }
        if self.contacts.is_empty() {
            self.multi = false;
            self.pair = None;
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinch_scales_incrementally_and_suppresses_remaining_touch() {
        let mut t = TouchPinch::default();
        assert_eq!(t.start(1, (0., 0.)), None);
        assert!(t.moved(1, (5., 0.)).is_none());
        assert_eq!(t.position(1), Some((5., 0.)));
        assert_eq!(t.start(2, (105., 0.)), Some(1));
        assert_eq!(t.started().unwrap().phase, Phase::Started);
        let p = t.moved(2, (155., 0.)).unwrap();
        assert_eq!(p.center, (80., 0.));
        assert_eq!(p.delta, 0.5);
        assert!((t.moved(2, (80., 0.)).unwrap().delta + 0.5).abs() < 0.0001);
        assert_eq!(t.end(2, false).unwrap().phase, Phase::Ended);
        assert!(t.is_multi());
        assert!(t.moved(1, (5., 0.)).is_none());
        t.end(1, false);
        assert!(!t.is_multi());
    }
    #[test]
    fn cancellation_and_extra_finger_do_not_restart_gesture() {
        let mut t = TouchPinch::default();
        t.start(1, (0., 0.));
        t.start(2, (0., 0.));
        t.start(3, (2., 0.));
        assert!(t.moved(3, (10., 0.)).is_none());
        assert_eq!(t.moved(2, (10., 0.)).unwrap().delta, 0.);
        assert_eq!(t.end(1, true).unwrap().phase, Phase::Cancelled);
        t.end(2, false);
        assert!(t.is_multi());
        t.end(3, false);
        assert!(!t.is_multi());
    }
}

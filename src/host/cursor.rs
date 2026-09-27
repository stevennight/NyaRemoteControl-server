//! Cursor forwarding: shapes are sent once (by content hash id), positions
//! and visibility as small state updates.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use nya_proto::pb::{self, cursor_msg::Msg};
use nya_win::duplication::{CursorShape, PointerUpdate};

#[derive(Default)]
pub struct CursorTracker {
    sent_shapes: HashSet<u32>,
    shape: Option<(u32, i32, i32)>, // id, hot_x, hot_y
    last_state: Option<pb::CursorState>,
    raw_pos: Option<(i32, i32, bool)>,
}

fn shape_id(s: &CursorShape) -> u32 {
    let mut h = DefaultHasher::new();
    (s.width, s.height, s.hot_x, s.hot_y).hash(&mut h);
    s.rgba.hash(&mut h);
    (h.finish() as u32).max(1)
}

impl CursorTracker {
    /// Turn a DXGI pointer update into messages for the client.
    pub fn update(&mut self, p: PointerUpdate, out: &mut Vec<pb::CursorMsg>) {
        if let Some(s) = p.shape {
            let id = shape_id(&s);
            if self.sent_shapes.insert(id) {
                if self.sent_shapes.len() > 256 {
                    self.sent_shapes.clear();
                    self.sent_shapes.insert(id);
                }
                out.push(pb::CursorMsg {
                    msg: Some(Msg::Shape(pb::CursorShape {
                        id,
                        width: s.width,
                        height: s.height,
                        hot_x: s.hot_x,
                        hot_y: s.hot_y,
                        rgba: s.rgba,
                    })),
                });
            }
            self.shape = Some((id, s.hot_x, s.hot_y));
        }
        if let Some(pos) = p.position {
            self.raw_pos = Some(pos);
        }
        let (Some((id, hx, hy)), Some((x, y, visible))) = (self.shape, self.raw_pos) else { return };
        let state = pb::CursorState { shape_id: id, visible, x: x + hx, y: y + hy };
        if self.last_state.as_ref() != Some(&state) {
            self.last_state = Some(state.clone());
            out.push(pb::CursorMsg { msg: Some(Msg::State(state)) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape() -> CursorShape {
        CursorShape { width: 2, height: 2, hot_x: 1, hot_y: 1, rgba: vec![255; 16] }
    }

    #[test]
    fn shape_sent_once_state_on_change() {
        let mut t = CursorTracker::default();
        let mut out = Vec::new();
        t.update(PointerUpdate { position: Some((10, 20, true)), shape: Some(shape()) }, &mut out);
        assert_eq!(out.len(), 2);
        match &out[1].msg {
            Some(Msg::State(s)) => assert_eq!((s.x, s.y, s.visible), (11, 21, true)),
            _ => panic!(),
        }
        out.clear();
        t.update(PointerUpdate { position: Some((10, 20, true)), shape: Some(shape()) }, &mut out);
        assert!(out.is_empty(), "nothing changed");
        t.update(PointerUpdate { position: Some((12, 20, true)), shape: None }, &mut out);
        assert_eq!(out.len(), 1);
    }
}

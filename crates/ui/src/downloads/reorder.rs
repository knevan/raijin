use std::collections::HashSet;

use raijin_core::download::DownloadId;

struct Drag {
    token: u64,
    original: Vec<DownloadId>,
    dragged: Vec<DownloadId>,
    order: Vec<DownloadId>,
    anchor_ix: usize,
    slot: Option<usize>,
}

struct Pending {
    token: u64,
    original: Vec<DownloadId>,
    order: Vec<DownloadId>,
}

#[derive(Default)]
pub(super) struct ReorderPresentation {
    generation: u64,
    drag: Option<Drag>,
    pending: Option<Pending>,
}

impl ReorderPresentation {
    pub(super) fn reset(&mut self) {
        self.drag = None;
        self.pending = None;
    }

    pub(super) fn begin(
        &mut self,
        items: &[DownloadId],
        dragged: &[DownloadId],
        anchor: DownloadId,
    ) -> Option<u64> {
        if self.pending.is_some() || dragged.is_empty() || !dragged.contains(&anchor) {
            return None;
        }
        let selected: HashSet<_> = dragged.iter().copied().collect();
        if selected.len() != dragged.len() {
            return None;
        }
        let dragged: Vec<_> = items
            .iter()
            .copied()
            .filter(|id| selected.contains(id))
            .collect();
        if dragged.len() != selected.len() {
            return None;
        }
        let anchor_ix = dragged.iter().position(|id| *id == anchor)?;
        self.generation += 1;
        self.drag = Some(Drag {
            token: self.generation,
            original: items.to_vec(),
            dragged,
            order: items.to_vec(),
            anchor_ix,
            slot: None,
        });
        Some(self.generation)
    }

    pub(super) fn update(&mut self, hovered_ix: Option<usize>) -> Option<Vec<DownloadId>> {
        let drag = self.drag.as_mut()?;
        let slot = hovered_ix.map(|ix| {
            ix.saturating_sub(drag.anchor_ix)
                .min(drag.original.len() - drag.dragged.len())
        });
        if drag.slot == slot {
            return None;
        }
        let mut order = drag.original.clone();
        if let Some(slot) = slot {
            order.retain(|id| !drag.dragged.contains(id));
            order.splice(slot..slot, drag.dragged.iter().copied());
        }
        drag.order.clone_from(&order);
        drag.slot = slot;
        Some(order)
    }

    pub(super) fn is_placeholder(&self, id: DownloadId) -> bool {
        self.drag
            .as_ref()
            .is_some_and(|drag| drag.slot.is_some() && drag.dragged.contains(&id))
    }

    pub(super) fn matches(&self, dragged: &[DownloadId]) -> bool {
        self.drag.as_ref().is_some_and(|drag| {
            drag.dragged.len() == dragged.len()
                && dragged.iter().all(|id| drag.dragged.contains(id))
        })
    }

    pub(super) fn cancel_active(&mut self) -> Option<Vec<DownloadId>> {
        let token = self.drag.as_ref()?.token;
        self.cancel(token)
    }

    pub(super) fn cancel(&mut self, token: u64) -> Option<Vec<DownloadId>> {
        if self.drag.as_ref()?.token != token {
            return None;
        }
        Some(self.drag.take()?.original)
    }

    pub(super) fn commit(&mut self) -> Option<(u64, Vec<DownloadId>)> {
        let drag = self.drag.take()?;
        if drag.slot.is_none() || drag.order == drag.original {
            return None;
        }
        let order = drag.order;
        self.pending = Some(Pending {
            token: drag.token,
            original: drag.original,
            order: order.clone(),
        });
        Some((drag.token, order))
    }

    pub(super) fn reject(&mut self, token: u64) -> Option<Vec<DownloadId>> {
        if self.pending.as_ref()?.token != token {
            return None;
        }
        Some(self.pending.take()?.original)
    }

    pub(super) fn project(&mut self, incoming: &[DownloadId]) -> Vec<DownloadId> {
        if let Some(drag) = &self.drag {
            if incoming == drag.original {
                return drag.order.clone();
            }
            self.reset();
        }
        if let Some(pending) = &self.pending {
            if incoming == pending.original {
                return pending.order.clone();
            }
            self.pending = None;
        }
        incoming.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use raijin_core::download::DownloadId;

    use super::ReorderPresentation;

    fn ids(values: &[i64]) -> Vec<DownloadId> {
        values.iter().map(|id| DownloadId::new(*id)).collect()
    }

    #[test]
    fn preview_should_support_both_directions_and_restore_on_cancel() {
        let mut state = ReorderPresentation::default();
        let token = state
            .begin(&ids(&[1, 2, 3]), &ids(&[1]), DownloadId::new(1))
            .unwrap();
        assert_eq!(state.update(Some(2)), Some(ids(&[2, 3, 1])));
        assert!(state.is_placeholder(DownloadId::new(1)));
        assert_eq!(state.project(&ids(&[1, 2, 3])), ids(&[2, 3, 1]));
        assert_eq!(state.update(Some(2)), None);
        assert_eq!(state.cancel(token), Some(ids(&[1, 2, 3])));
        assert!(state.commit().is_none());
        state
            .begin(&ids(&[1, 2, 3]), &ids(&[3]), DownloadId::new(3))
            .unwrap();
        assert_eq!(state.update(Some(0)), Some(ids(&[3, 1, 2])));
        assert_eq!(state.update(None), Some(ids(&[1, 2, 3])));
        assert!(state.commit().is_none());
    }

    #[test]
    fn pending_order_should_survive_stale_snapshots_and_rollback_only_matching_token() {
        let mut state = ReorderPresentation::default();
        state
            .begin(&ids(&[1, 2, 3]), &ids(&[1]), DownloadId::new(1))
            .unwrap();
        state.update(Some(2));
        let (token, order) = state.commit().unwrap();
        assert_eq!(order, ids(&[2, 3, 1]));
        assert_eq!(state.project(&ids(&[1, 2, 3])), order);
        assert!(
            state
                .begin(&order, &ids(&[2]), DownloadId::new(2))
                .is_none()
        );
        assert!(state.reject(token + 1).is_none());
        assert_eq!(state.reject(token), Some(ids(&[1, 2, 3])));
        assert_eq!(state.project(&ids(&[1, 2, 3])), ids(&[1, 2, 3]));
    }

    #[test]
    fn authoritative_changes_should_invalidate_stale_drag_and_completion() {
        let mut state = ReorderPresentation::default();
        let old = state
            .begin(&ids(&[1, 2, 3]), &ids(&[1]), DownloadId::new(1))
            .unwrap();
        state.update(Some(2));
        assert_eq!(state.project(&ids(&[2, 3])), ids(&[2, 3]));
        assert!(state.cancel(old).is_none());
        let new = state
            .begin(&ids(&[2, 3]), &ids(&[2]), DownloadId::new(2))
            .unwrap();
        assert_ne!(old, new);
        assert!(state.cancel(old).is_none());
        state.update(Some(1));
        state.commit().unwrap();
        assert_eq!(state.project(&ids(&[3, 2])), ids(&[3, 2]));
        assert!(state.reject(new).is_none());
        assert!(
            state
                .begin(&ids(&[3, 2]), &ids(&[3]), DownloadId::new(3))
                .is_some()
        );
    }

    #[test]
    fn multi_drag_should_preserve_queue_order_and_reject_invalid_payloads() {
        let mut state = ReorderPresentation::default();
        assert!(
            state
                .begin(&ids(&[1, 2, 3, 4]), &ids(&[8]), DownloadId::new(8))
                .is_none()
        );
        assert!(
            state
                .begin(&ids(&[1, 2]), &ids(&[1, 1]), DownloadId::new(1))
                .is_none()
        );
        state
            .begin(&ids(&[4, 3, 2, 1]), &ids(&[2, 3]), DownloadId::new(3))
            .unwrap();
        assert_eq!(state.update(Some(3)), Some(ids(&[4, 1, 3, 2])));
        assert!(state.matches(&ids(&[2, 3])));
        state.reset();
        assert!(!state.matches(&ids(&[2, 3])));
    }
}

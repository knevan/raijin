use gpui_kit::SharedString;
use gpui_kit::component::searchable_list::SearchableListItem;
use raijin_core::download::QueueId;

#[derive(Debug, Clone)]
pub struct QueueChoice {
    id: QueueId,
    name: SharedString,
}

impl QueueChoice {
    pub fn new(id: QueueId, name: impl Into<SharedString>) -> Self {
        Self {
            id,
            name: name.into(),
        }
    }
}

impl SearchableListItem for QueueChoice {
    type Value = QueueId;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn value(&self) -> &QueueId {
        &self.id
    }
}

#[cfg(test)]
mod tests {
    use super::{QueueChoice, QueueId, SearchableListItem};

    #[test]
    fn duplicate_names_should_keep_distinct_queue_identity() {
        let first = QueueChoice::new(QueueId::MAIN, "Main");
        let second = QueueChoice::new(QueueId::new(1), "Main");
        assert_eq!(first.title(), second.title());
        assert_ne!(first.value(), second.value());
    }
}

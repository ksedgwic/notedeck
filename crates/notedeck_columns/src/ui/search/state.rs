use crate::timeline::TimelineTab;
use notedeck::debouncer::Debouncer;
use std::time::Duration;

#[derive(Debug, Eq, PartialEq)]
pub enum SearchState {
    Typing,
    Searched,
    New,
}

/// Search query state that exists between frames
#[derive(Debug)]
pub struct SearchQueryState {
    /// This holds our search query while we're updating it
    pub string: String,

    /// When the debouncer timer elapses, we execute the search and mark
    /// our state as searchd. This will make sure we don't try to search
    /// again next frames
    pub state: SearchState,

    /// When was the input updated? We use this to debounce searches
    pub debouncer: Debouncer,

    /// The search results
    pub notes: TimelineTab,
}

impl Default for SearchQueryState {
    fn default() -> Self {
        SearchQueryState::new()
    }
}

impl SearchQueryState {
    pub fn new() -> Self {
        Self {
            string: "".to_string(),
            state: SearchState::New,
            notes: TimelineTab::default(),
            debouncer: Debouncer::new(Duration::from_millis(200)),
        }
    }

    pub fn should_search(&self) -> bool {
        self.state == SearchState::Typing && self.debouncer.should_act()
    }

    /// Mark the search as updated. This will update our debouncer and clear
    /// the searched flag, enabling us to search again. This should be
    /// called when the search box changes
    pub fn mark_updated(&mut self) {
        self.state = SearchState::Typing;
        self.debouncer.bounce();
    }

    /// Call this when you are about to do a search so that we don't try
    /// to search again next frame
    pub fn mark_searched(&mut self, state: SearchState) {
        self.state = state;
    }
}

use egui::{vec2, Align, Color32, RichText, Rounding, Stroke, TextEdit};

use super::padding;
use crate::ui::{note::NoteOptions, timeline::TimelineTabView};
use nostrdb::{Filter, Ndb, Transaction};
use notedeck::{Images, MuteFun, NoteCache, NoteRef};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

mod state;

pub use state::{SearchQueryState, SearchState};

pub struct SearchView<'a> {
    query: &'a mut SearchQueryState,
    ndb: &'a Ndb,
    note_options: NoteOptions,
    txn: &'a Transaction,
    note_cache: &'a mut NoteCache,
    img_cache: &'a mut Images,
    is_muted: &'a MuteFun,
}

impl<'a> SearchView<'a> {
    pub fn new(
        ndb: &'a Ndb,
        txn: &'a Transaction,
        note_cache: &'a mut NoteCache,
        img_cache: &'a mut Images,
        is_muted: &'a MuteFun,
        note_options: NoteOptions,
        query: &'a mut SearchQueryState,
    ) -> Self {
        Self {
            ndb,
            txn,
            note_cache,
            img_cache,
            is_muted,
            query,
            note_options,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        padding(8.0, ui, |ui| {
            self.show_impl(ui);
        });
    }

    pub fn show_impl(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 12.0);

        if search_box(self.query, ui) {
            self.execute_search(ui.ctx());
        }

        match self.query.state {
            SearchState::New => {}

            SearchState::Searched | SearchState::Typing => {
                if self.query.state == SearchState::Typing {
                    ui.label(format!("Searching for '{}'", &self.query.string));
                } else {
                    ui.label(format!(
                        "Got {} results for '{}'",
                        self.query.notes.notes.len(),
                        &self.query.string
                    ));
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    let reversed = false;
                    TimelineTabView::new(
                        &self.query.notes,
                        reversed,
                        self.note_options,
                        self.txn,
                        self.ndb,
                        self.note_cache,
                        self.img_cache,
                        self.is_muted,
                    )
                    .show(ui);
                });
            }
        }
    }

    fn execute_search(&mut self, ctx: &egui::Context) {
        if self.query.string.is_empty() {
            return;
        }

        let max_results = 500;
        let filter = Filter::new()
            .search(&self.query.string)
            .kinds([1])
            .limit(max_results)
            .build();

        // TODO: execute in thread

        let before = Instant::now();
        let qrs = self.ndb.query(self.txn, &[filter], max_results as i32);
        let after = Instant::now();
        let duration = after - before;

        if duration > Duration::from_millis(20) {
            warn!(
                "query took {:?}... let's update this to use a thread!",
                after - before
            );
        }

        match qrs {
            Ok(qrs) => {
                info!(
                    "queried '{}' and got {} results",
                    self.query.string,
                    qrs.len()
                );

                let note_refs = qrs.into_iter().map(NoteRef::from_query_result).collect();
                self.query.notes.notes = note_refs;
                self.query.notes.list.borrow_mut().reset();
                ctx.request_repaint();
            }

            Err(err) => {
                error!("fulltext query failed: {err}")
            }
        }
    }
}

fn search_box(query: &mut SearchQueryState, ui: &mut egui::Ui) -> bool {
    ui.horizontal(|ui| {
        // Container for search input and icon
        let search_container = egui::Frame {
            inner_margin: egui::Margin::symmetric(8.0, 0.0),
            outer_margin: egui::Margin::ZERO,
            rounding: Rounding::same(18.0), // More rounded corners
            shadow: Default::default(),
            fill: Color32::from_rgb(30, 30, 30), // Darker background to match screenshot
            stroke: Stroke::new(1.0, Color32::from_rgb(60, 60, 60)),
        };

        search_container
            .show(ui, |ui| {
                // Use layout to align items vertically centered
                ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 0.0);

                    let search_height = 34.0;
                    // Magnifying glass icon
                    ui.add(search_icon(16.0, search_height));

                    let before_len = query.string.len();

                    // Search input field
                    //let font_size = notedeck::fonts::get_font_size(ui.ctx(), &NotedeckTextStyle::Body);
                    ui.add_sized(
                        [ui.available_width(), search_height],
                        TextEdit::singleline(&mut query.string)
                            .hint_text(RichText::new("Search notes...").weak())
                            //.desired_width(available_width - 32.0)
                            //.font(egui::FontId::new(font_size, egui::FontFamily::Proportional))
                            .margin(vec2(0.0, 8.0))
                            .frame(false),
                    );

                    let after_len = query.string.len();

                    let changed = before_len != after_len;
                    if changed {
                        query.mark_updated();
                    }

                    // Execute search after debouncing
                    if query.should_search() {
                        query.mark_searched(SearchState::Searched);
                        true
                    } else {
                        false
                    }
                })
                .inner
            })
            .inner
    })
    .inner
}

/// Creates a magnifying glass icon widget
fn search_icon(size: f32, height: f32) -> impl egui::Widget {
    move |ui: &mut egui::Ui| {
        // Use the provided height parameter
        let desired_size = vec2(size, height);
        let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

        // Calculate center position - this ensures the icon is centered in its allocated space
        let center_pos = rect.center();
        let stroke = Stroke::new(1.5, Color32::from_rgb(150, 150, 150));

        // Draw circle
        let circle_radius = size * 0.35;
        ui.painter()
            .circle(center_pos, circle_radius, Color32::TRANSPARENT, stroke);

        // Draw handle
        let handle_start = center_pos + vec2(circle_radius * 0.7, circle_radius * 0.7);
        let handle_end = handle_start + vec2(size * 0.25, size * 0.25);
        ui.painter()
            .line_segment([handle_start, handle_end], stroke);

        response
    }
}

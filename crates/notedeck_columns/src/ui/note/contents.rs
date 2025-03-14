use crate::gif::{handle_repaint, retrieve_latest_texture};
use crate::ui::images::render_images;
use crate::ui::{
    self,
    note::{NoteOptions, NoteResponse},
};
use crate::{actionbar::NoteAction, images::ImageType, timeline::TimelineKind};
use egui::{Color32, Hyperlink, Image, RichText};
use nostrdb::{BlockType, Mention, Ndb, Note, NoteKey, Transaction};
use tracing::warn;

use notedeck::{supported_mime_hosted_at_url, Images, MediaCacheType, NoteCache};

/// Aggregates dependencies to reduce the number of parameters
/// passed to inner UI elements, minimizing prop drilling.
pub struct NoteContext<'d> {
    pub ndb: &'d Ndb,
    pub img_cache: &'d mut Images,
    pub note_cache: &'d mut NoteCache,
}

pub struct NoteContents<'a, 'd> {
    note_context: &'a mut NoteContext<'d>,
    txn: &'a Transaction,
    note: &'a Note<'a>,
    options: NoteOptions,
    action: Option<NoteAction>,
}

impl<'a, 'd> NoteContents<'a, 'd> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        note_context: &'a mut NoteContext<'d>,
        txn: &'a Transaction,
        note: &'a Note,
        options: ui::note::NoteOptions,
    ) -> Self {
        NoteContents {
            note_context,
            txn,
            note,
            options,
            action: None,
        }
    }

    pub fn action(&self) -> &Option<NoteAction> {
        &self.action
    }
}

impl egui::Widget for &mut NoteContents<'_, '_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let result = render_note_contents(ui, self.note_context, self.txn, self.note, self.options);
        self.action = result.action;
        result.response
    }
}

/// Render an inline note preview with a border. These are used when
/// notes are references within a note
#[allow(clippy::too_many_arguments)]
pub fn render_note_preview(
    ui: &mut egui::Ui,
    note_context: &mut NoteContext,
    txn: &Transaction,
    id: &[u8; 32],
    parent: NoteKey,
    note_options: NoteOptions,
) -> NoteResponse {
    #[cfg(feature = "profiling")]
    puffin::profile_function!();

    let note = if let Ok(note) = note_context.ndb.get_note_by_id(txn, id) {
        // TODO: support other preview kinds
        if note.kind() == 1 {
            note
        } else {
            return NoteResponse::new(ui.colored_label(
                Color32::RED,
                format!("TODO: can't preview kind {}", note.kind()),
            ));
        }
    } else {
        return NoteResponse::new(ui.colored_label(Color32::RED, "TODO: COULD NOT LOAD"));
        /*
        return ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.colored_label(link_color, "@");
                ui.colored_label(link_color, &id_str[4..16]);
            })
            .response;
            */
    };

    egui::Frame::none()
        .fill(ui.visuals().noninteractive().weak_bg_fill)
        .inner_margin(egui::Margin::same(8.0))
        .outer_margin(egui::Margin::symmetric(0.0, 8.0))
        .rounding(egui::Rounding::same(10.0))
        .stroke(egui::Stroke::new(
            1.0,
            ui.visuals().noninteractive().bg_stroke.color,
        ))
        .show(ui, |ui| {
            ui::NoteView::new(note_context, &note, note_options)
                .actionbar(false)
                .small_pfp(true)
                .wide(true)
                .note_previews(false)
                .options_button(true)
                .parent(parent)
                .is_preview(true)
                .show(ui)
        })
        .inner
}

#[allow(clippy::too_many_arguments)]
fn render_note_contents(
    ui: &mut egui::Ui,
    note_context: &mut NoteContext,
    txn: &Transaction,
    note: &Note,
    options: NoteOptions,
) -> NoteResponse {
    #[cfg(feature = "profiling")]
    puffin::profile_function!();

    let note_key = note.key().expect("todo: implement non-db notes");
    let selectable = options.has_selectable_text();
    let mut images: Vec<(String, MediaCacheType)> = vec![];
    let mut note_action: Option<NoteAction> = None;
    let mut inline_note: Option<(&[u8; 32], &str)> = None;
    let hide_media = options.has_hide_media();
    let link_color = ui.visuals().hyperlink_color;

    if !options.has_is_preview() {
        // need this for the rect to take the full width of the column
        let _ = ui.allocate_at_least(egui::vec2(ui.available_width(), 0.0), egui::Sense::click());
    }

    let response = ui.horizontal_wrapped(|ui| {
        let blocks = if let Ok(blocks) = note_context.ndb.get_blocks_by_key(txn, note_key) {
            blocks
        } else {
            warn!("missing note content blocks? '{}'", note.content());
            ui.weak(note.content());
            return;
        };

        ui.spacing_mut().item_spacing.x = 0.0;

        for block in blocks.iter(note) {
            match block.blocktype() {
                BlockType::MentionBech32 => match block.as_mention().unwrap() {
                    Mention::Profile(profile) => {
                        let act = ui::Mention::new(
                            note_context.ndb,
                            note_context.img_cache,
                            txn,
                            profile.pubkey(),
                        )
                        .show(ui)
                        .inner;
                        if act.is_some() {
                            note_action = act;
                        }
                    }

                    Mention::Pubkey(npub) => {
                        let act = ui::Mention::new(
                            note_context.ndb,
                            note_context.img_cache,
                            txn,
                            npub.pubkey(),
                        )
                        .show(ui)
                        .inner;
                        if act.is_some() {
                            note_action = act;
                        }
                    }

                    Mention::Note(note) if options.has_note_previews() => {
                        inline_note = Some((note.id(), block.as_str()));
                    }

                    Mention::Event(note) if options.has_note_previews() => {
                        inline_note = Some((note.id(), block.as_str()));
                    }

                    _ => {
                        ui.colored_label(link_color, format!("@{}", &block.as_str()[4..16]));
                    }
                },

                BlockType::Hashtag => {
                    #[cfg(feature = "profiling")]
                    puffin::profile_scope!("hashtag contents");
                    let resp = ui.colored_label(link_color, format!("#{}", block.as_str()));

                    if resp.clicked() {
                        note_action = Some(NoteAction::OpenTimeline(TimelineKind::Hashtag(
                            block.as_str().to_string(),
                        )));
                    } else if resp.hovered() {
                        ui::show_pointer(ui);
                    }
                }

                BlockType::Url => {
                    let mut found_supported = || -> bool {
                        let url = block.as_str();
                        if let Some(cache_type) =
                            supported_mime_hosted_at_url(&mut note_context.img_cache.urls, url)
                        {
                            images.push((url.to_string(), cache_type));
                            true
                        } else {
                            false
                        }
                    };
                    if hide_media || !found_supported() {
                        #[cfg(feature = "profiling")]
                        puffin::profile_scope!("url contents");
                        ui.add(Hyperlink::from_label_and_url(
                            RichText::new(block.as_str()).color(link_color),
                            block.as_str(),
                        ));
                    }
                }

                BlockType::Text => {
                    #[cfg(feature = "profiling")]
                    puffin::profile_scope!("text contents");
                    if options.has_scramble_text() {
                        ui.add(egui::Label::new(rot13(block.as_str())).selectable(selectable));
                    } else {
                        ui.add(egui::Label::new(block.as_str()).selectable(selectable));
                    }
                }

                _ => {
                    ui.colored_label(link_color, block.as_str());
                }
            }
        }
    });

    let preview_note_action = if let Some((id, _block_str)) = inline_note {
        render_note_preview(ui, note_context, txn, id, note_key, options).action
    } else {
        None
    };

    if !images.is_empty() && !options.has_textmode() {
        ui.add_space(2.0);
        let carousel_id = egui::Id::new(("carousel", note.key().expect("expected tx note")));
        image_carousel(ui, note_context.img_cache, images, carousel_id);
        ui.add_space(2.0);
    }

    let note_action = preview_note_action.or(note_action);

    NoteResponse::new(response.response).with_action(note_action)
}

fn rot13(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                // Rotate lowercase letters
                (((c as u8 - b'a' + 13) % 26) + b'a') as char
            } else if c.is_ascii_uppercase() {
                // Rotate uppercase letters
                (((c as u8 - b'A' + 13) % 26) + b'A') as char
            } else {
                // Leave other characters unchanged
                c
            }
        })
        .collect()
}

fn image_carousel(
    ui: &mut egui::Ui,
    img_cache: &mut Images,
    images: Vec<(String, MediaCacheType)>,
    carousel_id: egui::Id,
) {
    // let's make sure everything is within our area

    let height = 360.0;
    let width = ui.available_size().x;
    let spinsz = if height > width { width } else { height };

    ui.add_sized([width, height], |ui: &mut egui::Ui| {
        egui::ScrollArea::horizontal()
            .id_salt(carousel_id)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (image, cache_type) in images {
                        render_images(
                            ui,
                            img_cache,
                            &image,
                            ImageType::Content(width.round() as u32, height.round() as u32),
                            cache_type,
                            |ui| {
                                ui.allocate_space(egui::vec2(spinsz, spinsz));
                            },
                            |ui, _| {
                                ui.allocate_space(egui::vec2(spinsz, spinsz));
                            },
                            |ui, url, renderable_media, gifs| {
                                let texture = handle_repaint(
                                    ui,
                                    retrieve_latest_texture(&image, gifs, renderable_media),
                                );
                                let img_resp = ui.add(
                                    Image::new(texture)
                                        .max_height(height)
                                        .rounding(5.0)
                                        .fit_to_original_size(1.0),
                                );

                                img_resp.context_menu(|ui| {
                                    if ui.button("Copy Link").clicked() {
                                        ui.ctx().copy_text(url.to_owned());
                                        ui.close_menu();
                                    }
                                });
                            },
                        );
                    }
                })
                .response
            })
            .inner
    });
}

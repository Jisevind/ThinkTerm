//! The window tab row when it does not keep its place: hidden outright, or
//! brought down from the top edge while the pointer rests there.
//!
//! Hidden, the row gives its height back to the terminal. Brought down, it
//! comes in over the window's top and pushes the terminal area down by the
//! same amount -- painted lower, never resized, so nothing reflows and the
//! last line or two drop out of view until it goes again. The hover is the
//! workspace sidebar's machine (`SidebarHoverReveal`): the same dwell, grace
//! and travel, fed a strip along the top instead of the left edge.

use crate::native_settings::NativeWindowTabs;
use crate::quad::{HeapQuadAllocator, QuadClipRect, TripleLayerQuadAllocator};
use crate::termwindow::sidebar_hover::{HoverFrame, HoverInput, PointerZone, SidebarHoverReveal};
use crate::termwindow::ui::tokens::{
    WINDOW_RESIZE_EDGE_HEIGHT, WINDOW_TAB_REVEAL_HOT_ZONE_HEIGHT,
    WINDOW_TAB_REVEAL_STICKY_ZONE_HEIGHT,
};
use crate::termwindow::{
    InlineTabRenameTarget, TermWindow, TermWindowNotif, UIItem, UIItemType,
};
use std::time::Instant;
use window::{WindowDecorations, WindowOps};

#[derive(Debug, Clone, Default)]
pub(crate) struct WindowTabReveal {
    mode: NativeWindowTabs,
    pinned: bool,
    hover: SidebarHoverReveal,
    /// How far out the row is, 0 to 1, as of the last step: what geometry
    /// reads between steps, so a frame and the clicks on it agree.
    shown: f32,
}

impl WindowTabReveal {
    pub(crate) fn new() -> Self {
        let (mode, pinned) = crate::native_settings::window_tabs_choice();
        Self {
            mode,
            pinned,
            ..Default::default()
        }
    }

    /// The row keeps its place in the layout.
    pub(crate) fn in_layout(&self) -> bool {
        row_in_layout(self.mode, self.pinned)
    }

    /// The row carries its pin: it auto-hides, pinned or not.
    pub(crate) fn shows_pin(&self) -> bool {
        self.mode == NativeWindowTabs::AutoHide
    }

    pub(crate) fn pinned(&self) -> bool {
        self.pinned
    }

    /// A dwell, a grace or a travel is under way.
    pub(crate) fn needs_frames(&self) -> bool {
        self.hover.needs_frames()
    }

    /// Focus loss: put the row away without a travel.
    pub(crate) fn cancel(&mut self) {
        self.hover.cancel_immediately();
        self.shown = 0.0;
    }
}

/// Whether the row takes its height in the layout: shown, or auto-hiding
/// with its pin in.
fn row_in_layout(mode: NativeWindowTabs, pinned: bool) -> bool {
    match mode {
        NativeWindowTabs::Show => true,
        NativeWindowTabs::AutoHide => pinned,
        NativeWindowTabs::Hide => false,
    }
}

/// Whether the top edge may bring the row down. A hidden row comes down
/// only while it holds the window's own buttons: they have nowhere else to
/// be, and a window that cannot be closed is worse than a row that shows.
fn row_may_reveal(mode: NativeWindowTabs, pinned: bool, carries_window_buttons: bool) -> bool {
    match mode {
        NativeWindowTabs::Show => false,
        NativeWindowTabs::AutoHide => !pinned,
        NativeWindowTabs::Hide => carries_window_buttons,
    }
}

impl TermWindow {
    /// The height the window tab row takes in the layout: all of it while it
    /// keeps its place, none while it hides. A revealed row takes none
    /// either -- see `window_tab_reveal_offset`.
    pub(crate) fn window_tab_row_layout_height(&self) -> f32 {
        if self.window_tab_reveal.in_layout() {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// How far a row brought down from the top edge has pushed the terminal
    /// area down. Painting and pointer mapping add it; the terminal's size
    /// never does.
    pub(crate) fn window_tab_reveal_offset(&self) -> f32 {
        if self.window_tab_reveal.in_layout() {
            return 0.0;
        }
        self.window_tab_reveal.shown * self.tab_bar_pixel_height().unwrap_or(0.0)
    }

    /// How much of the window's top the row covers as it stands: all of its
    /// height while it keeps its place, as far as it has come down otherwise.
    pub(crate) fn window_tab_row_drawn_height(&self) -> f32 {
        self.window_tab_row_layout_height() + self.window_tab_reveal_offset()
    }

    /// The row is drawn where it always is, with live buttons: it keeps its
    /// place, or it has come all the way down.
    pub(crate) fn window_tab_row_in_place(&self) -> bool {
        self.window_tab_reveal.in_layout() || self.window_tab_reveal.shown >= 1.0
    }

    /// The row is drawn at all this frame: in place, or travelling.
    pub(crate) fn window_tab_row_drawn(&self) -> bool {
        self.window_tab_row_in_place() || self.window_tab_reveal.shown > 0.0
    }

    /// While the row travels: how far above its place it is drawn, as a
    /// negative shift. `None` when it is in place or away.
    pub(crate) fn window_tab_row_travel_shift(&self) -> Option<f32> {
        let shown = self.window_tab_reveal.shown;
        if self.window_tab_row_in_place() || shown <= 0.0 {
            return None;
        }
        Some((shown - 1.0) * self.tab_bar_pixel_height().unwrap_or(0.0))
    }

    /// Paint the row `shift` pixels above its place, cropped to the window:
    /// recorded where it sits, since the painters take unsigned coordinates,
    /// and moved on replay. Nothing on it takes a click until it lands, and
    /// the one target returned keeps a click there from falling through to
    /// the terminal's first row instead.
    pub(crate) fn paint_window_tab_row_travelling(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        shift: f32,
    ) -> anyhow::Result<Vec<UIItem>> {
        let mut recorded = HeapQuadAllocator::default();
        let mut row_layers = TripleLayerQuadAllocator::Heap(&mut recorded);
        self.paint_fancy_tab_bar(&mut row_layers, false)?;
        drop(row_layers);
        let clip = QuadClipRect::from_top_left_pixels(
            0.0,
            0.0,
            self.dimensions.pixel_width as f32,
            self.dimensions.pixel_height as f32,
            &self.dimensions,
        );
        recorded.apply_to_clipped_at(layers, 0.0, shift, clip, 1.0)?;
        let left = self.tab_bar_left_edge();
        Ok(vec![UIItem {
            x: left,
            y: self.get_os_border().top.get() as usize,
            width: (self.terminal_viewport_right() as usize).saturating_sub(left),
            height: self.window_tab_reveal_offset().ceil() as usize,
            item_type: UIItemType::WindowTabsTravelling,
        }])
    }

    /// The window's own minimise, maximise and close buttons are on the row,
    /// as they are with integrated buttons and the right sidebar closed.
    fn window_tab_row_carries_window_buttons(&self) -> bool {
        self.fancy_tab_bar_shows_window_buttons() && self.right_sidebar_width() == 0
    }

    /// A tab of the row is being renamed in place, which needs the row out.
    fn renaming_window_tab(&self) -> bool {
        self.inline_tab_rename
            .as_ref()
            .is_some_and(|rename| matches!(rename.target, InlineTabRenameTarget::WindowTab(_)))
    }

    /// Off macOS, the top edge the system keeps for resizing a window that
    /// draws its own frame. The pointer there never reaches the window, so
    /// the strip that brings the row down reaches past it.
    fn window_tab_resize_edge(&self) -> usize {
        if cfg!(target_os = "macos")
            || self.config.window_decorations.contains(WindowDecorations::TITLE)
            || !self.window_state.can_resize()
        {
            return 0;
        }
        self.ui_px(WINDOW_RESIZE_EDGE_HEIGHT)
    }

    fn window_tab_hover_input(&self) -> HoverInput {
        let overlay_open = self.menu_or_overlay_open();
        let presented = self.window_tab_reveal.hover.is_presented();
        // A row out for a rename goes back the usual way once it is done,
        // whatever the mode. Unlike the sidebars, a terminal that is
        // connecting or offline does not hold the row back: it is the way
        // to another tab.
        let eligible = (row_may_reveal(
            self.window_tab_reveal.mode,
            self.window_tab_reveal.pinned,
            self.window_tab_row_carries_window_buttons(),
        ) || self.renaming_window_tab()
            || presented)
            && self.tab_bar_pixel_height().unwrap_or(0.0) > 0.0
            && !self.content_view_is_full_window()
            && (!overlay_open || presented);
        let pointer = match &self.current_mouse_event {
            None => PointerZone::Away,
            Some(event) => {
                let x = event.coords.x;
                let y = event.coords.y;
                let top = self.get_os_border().top.get() as isize;
                let left = self.tab_bar_left_edge() as isize;
                let right = self.terminal_viewport_right() as isize;
                let row_bottom = top + self.window_tab_reveal_offset().ceil() as isize;
                let edge = top + self.window_tab_resize_edge() as isize;
                if x < left || x >= right {
                    PointerZone::Away
                } else if presented && y < row_bottom {
                    PointerZone::Panel
                } else if y < edge + self.ui_px(WINDOW_TAB_REVEAL_HOT_ZONE_HEIGHT) as isize {
                    PointerZone::HotZone
                } else if y < edge + self.ui_px(WINDOW_TAB_REVEAL_STICKY_ZONE_HEIGHT) as isize {
                    PointerZone::NearHotZone
                } else {
                    PointerZone::Away
                }
            }
        };
        let pinned = !self.current_mouse_buttons.is_empty()
            || self.current_mouse_capture.is_some()
            || self.dragging.is_some()
            || self.pane_tab_drag.is_some()
            || overlay_open;
        HoverInput {
            eligible,
            pointer,
            pinned,
        }
    }

    /// Step the row's hover machine and keep its position for the frame.
    /// From the frame loop, so a resting pointer still brings it down at
    /// dwell expiry and a departed one still sends it back at grace expiry.
    pub(crate) fn advance_window_tab_reveal(&mut self, now: Instant) {
        let input = self.window_tab_hover_input();
        let frame = self.window_tab_reveal.hover.step(input, now);
        self.window_tab_reveal.shown = self.window_tab_reveal.hover.progress(now).unwrap_or(0.0);
        match frame {
            HoverFrame::None => {}
            HoverFrame::Now => {
                if let Some(window) = self.window.as_ref() {
                    window.invalidate();
                }
            }
            HoverFrame::At(due) => self.update_next_frame_time(Some(due)),
        }
    }

    /// Feed a pointer event to the row's hover machine. Never consumes it.
    pub(crate) fn update_window_tab_reveal(&mut self, context: &dyn WindowOps) {
        let input = self.window_tab_hover_input();
        if self.window_tab_reveal.hover.step(input, Instant::now()) != HoverFrame::None {
            // As with the sidebar: the frame's advance registers deadlines.
            context.invalidate();
        }
    }

    /// Take a new mode or pin from Settings: the terminal gets the row's
    /// height back, or gives it up, once.
    pub(crate) fn refresh_window_tabs(&mut self) {
        let before = self.window_tab_reveal.in_layout();
        let (mode, pinned) = crate::native_settings::window_tabs_choice();
        self.window_tab_reveal.mode = mode;
        self.window_tab_reveal.pinned = pinned;
        if !row_may_reveal(mode, pinned, self.window_tab_row_carries_window_buttons()) {
            self.window_tab_reveal.cancel();
        }
        if before != self.window_tab_reveal.in_layout() {
            self.invalidate_fancy_tab_bar();
            if let Some(window) = self.window.as_ref().cloned() {
                let dimensions = self.dimensions;
                self.apply_dimensions(&dimensions, None, &window);
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// A row tab renamed from the keyboard or the palette while the row is
    /// away: bring the row down for the edit, which holds it out until done.
    pub(crate) fn reveal_window_tab_row_for_rename(&mut self) {
        if self.window_tab_reveal.in_layout() || !self.renaming_window_tab() {
            return;
        }
        self.window_tab_reveal.hover.reveal_now(Instant::now());
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// The pin: keep an auto-hiding row out, or let it go again. Saved, so
    /// every window follows and the next launch remembers.
    pub(crate) fn toggle_window_tabs_pinned(&mut self) {
        let pinned = !self.window_tab_reveal.pinned;
        if let Err(err) = crate::native_settings::save_window_tabs_pinned(pinned) {
            log::warn!("failed to save the window tab row's pin: {err:#}");
            return;
        }
        // This window at once: a frame painted before the notice below
        // reaches it would still go by the old pin.
        self.refresh_window_tabs();
        if !pinned {
            // Unpinned under the pointer: it stays out until the pointer
            // leaves, as a row brought down by hover would.
            self.window_tab_reveal.hover.present_immediately();
            self.window_tab_reveal.shown = 1.0;
        }
        if let Some(front_end) = crate::frontend::try_front_end() {
            for gui_window in front_end.gui_windows() {
                gui_window.window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.refresh_window_tabs();
                })));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_shown_or_pinned_row_keeps_its_place() {
        assert!(row_in_layout(NativeWindowTabs::Show, false));
        assert!(row_in_layout(NativeWindowTabs::AutoHide, true));
        assert!(!row_in_layout(NativeWindowTabs::AutoHide, false));
        assert!(!row_in_layout(NativeWindowTabs::Hide, false));
    }

    #[test]
    fn a_hidden_row_comes_down_only_for_the_window_buttons() {
        assert!(row_may_reveal(NativeWindowTabs::AutoHide, false, false));
        assert!(!row_may_reveal(NativeWindowTabs::AutoHide, true, false));
        assert!(!row_may_reveal(NativeWindowTabs::Show, false, true));
        assert!(!row_may_reveal(NativeWindowTabs::Hide, false, false));
        assert!(row_may_reveal(NativeWindowTabs::Hide, false, true));
    }
}

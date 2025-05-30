use crate::wayland::handlers::xdg_activation::ActivationContext;
use crate::{
    backend::render::{
        element::{AsGlowRenderer, FromGlesError},
        BackdropShader,
    },
    shell::{
        layout::{floating::FloatingLayout, tiling::TilingLayout},
        OverviewMode, ANIMATION_DURATION,
    },
    state::State,
    utils::{prelude::*, tween::EaseRectangle},
    wayland::{
        handlers::screencopy::ScreencopySessions,
        protocols::{
            toplevel_info::{toplevel_enter_output, toplevel_leave_output},
            workspace::{WorkspaceHandle, WorkspaceUpdateGuard},
        },
    },
};
use cosmic_comp_config::workspace::{OutputMatch, PinnedWorkspace};

use cosmic::theme::CosmicTheme;
use cosmic_protocols::workspace::v2::server::zcosmic_workspace_handle_v2::TilingState;
use id_tree::Tree;
use indexmap::IndexSet;
use keyframe::{ease, functions::EaseInOutCubic};
use smithay::{
    backend::renderer::{
        element::{
            surface::WaylandSurfaceRenderElement, texture::TextureRenderElement,
            utils::RescaleRenderElement, Element, Id, RenderElement,
        },
        gles::GlesTexture,
        glow::GlowRenderer,
        utils::{DamageSet, OpaqueRegions},
        ImportAll, ImportMem, Renderer,
    },
    desktop::{layer_map_for_output, space::SpaceElement, WindowSurfaceType},
    input::Seat,
    output::Output,
    reexports::wayland_server::{Client, Resource},
    utils::{Buffer as BufferCoords, IsAlive, Logical, Physical, Point, Rectangle, Scale, Size},
    wayland::{
        compositor::{add_blocker, Blocker, BlockerState},
        seat::WaylandFocus,
        xdg_activation::XdgActivationState,
    },
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use wayland_backend::server::ClientId;

use super::{
    element::{
        resize_indicator::ResizeIndicator, stack::CosmicStackRenderElement,
        swap_indicator::SwapIndicator, window::CosmicWindowRenderElement, CosmicMapped,
        MaximizedState,
    },
    focus::{
        target::{KeyboardFocusTarget, PointerFocusTarget, WindowGroup},
        FocusStack, FocusStackMut,
    },
    grabs::ResizeEdge,
    layout::tiling::{Data, MinimizedTilingState, NodeDesc},
    CosmicMappedRenderElement, CosmicSurface, ResizeDirection, ResizeMode,
};

const FULLSCREEN_ANIMATION_DURATION: Duration = Duration::from_millis(200);

// For stable workspace id, generate random 24-bit integer, as a hex string
// Must be compared with existing workspaces work uniqueness.
// TODO: Assign an id to any workspace that is pinned
pub fn random_id() -> String {
    let id = rand::random_range(0..(2 << 24));
    format!("{:x}", id)
}

fn output_match_for_output(output: &Output) -> OutputMatch {
    OutputMatch {
        name: output.name(),
        edid: output.edid().cloned(),
    }
}

// If `disambguate` is true, check that edid *and* connector name match.
// Otherwise, match only edid (if it exists)
fn output_matches(output_match: &OutputMatch, output: &Output, disambiguate: bool) -> bool {
    if output_match.edid.as_ref() != output.edid() {
        false
    } else if disambiguate || output_match.edid.is_none() {
        output_match.name == output.name()
    } else {
        true
    }
}

#[derive(Debug)]
pub struct Workspace {
    pub output: Output,
    pub tiling_layer: TilingLayout,
    pub floating_layer: FloatingLayout,
    pub minimized_windows: Vec<MinimizedWindow>,
    pub tiling_enabled: bool,
    pub fullscreen: Option<FullscreenSurface>,
    pub pinned: bool,

    pub handle: WorkspaceHandle,
    pub focus_stack: FocusStacks,
    pub screencopy: ScreencopySessions,
    output_stack: VecDeque<OutputMatch>,
    pub(super) backdrop_id: Id,
    pub dirty: AtomicBool,
}

#[derive(Debug)]
pub struct MinimizedWindow {
    pub window: CosmicMapped,
    pub previous_state: MinimizedState,
    pub fullscreen: Option<FullscreenSurface>,
    pub output_geo: Rectangle<i32, Global>,
}

#[derive(Debug)]
pub enum MinimizedState {
    Sticky {
        position: Point<i32, Local>,
    },
    Floating {
        position: Point<i32, Local>,
    },
    Tiling {
        tiling_state: Option<MinimizedTilingState>,
        was_maximized: bool,
    },
}

impl MinimizedWindow {
    pub(super) fn unmaximize(&mut self, original_geometry: Rectangle<i32, Local>) {
        self.window.set_maximized(false);
        self.window.configure();

        match &mut self.previous_state {
            MinimizedState::Sticky { position } | MinimizedState::Floating { position } => {
                *position = original_geometry.loc;
            }
            MinimizedState::Tiling { was_maximized, .. } => {
                *was_maximized = false;
            }
        }
    }

    fn unfullscreen(&mut self) -> Option<(ManagedLayer, WorkspaceHandle)> {
        let fullscreen = self.fullscreen.take()?;
        self.window.set_fullscreen(false);
        self.window.set_geometry(fullscreen.original_geometry);
        fullscreen.previously
    }
}

#[derive(Debug, Clone)]
pub struct FullscreenSurface {
    pub surface: CosmicSurface,
    pub previously: Option<(ManagedLayer, WorkspaceHandle)>,
    original_geometry: Rectangle<i32, Global>,
    start_at: Option<Instant>,
    ended_at: Option<Instant>,
    animation_signal: Option<Arc<AtomicBool>>,
}

impl PartialEq for FullscreenSurface {
    fn eq(&self, other: &Self) -> bool {
        self.surface == other.surface
    }
}

struct FullscreenBlocker {
    signal: Arc<AtomicBool>,
}

impl Blocker for FullscreenBlocker {
    fn state(&self) -> BlockerState {
        if self.signal.load(Ordering::SeqCst) {
            BlockerState::Released
        } else {
            BlockerState::Pending
        }
    }
}

impl FullscreenSurface {
    pub fn is_animating(&self) -> bool {
        self.start_at.is_some() || self.ended_at.is_some()
    }
}

impl IsAlive for FullscreenSurface {
    fn alive(&self) -> bool {
        self.surface.alive()
    }
}

/// LIFO stack of focus targets
#[derive(Debug, Default)]
pub struct FocusStacks(HashMap<Seat<State>, IndexSet<CosmicMapped>>);

#[derive(Debug, Clone, PartialEq)]
pub struct ManagedState {
    pub layer: ManagedLayer,
    pub was_fullscreen: Option<FullscreenSurface>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagedLayer {
    Tiling,
    Floating,
    Sticky,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FocusResult {
    None,
    Handled,
    Some(KeyboardFocusTarget),
}

impl FocusResult {
    pub fn or_else<F>(self, f: F) -> FocusResult
    where
        F: FnOnce() -> FocusResult,
    {
        match self {
            FocusResult::None => f(),
            x => x,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MoveResult {
    None,
    Done,
    MoveFurther(KeyboardFocusTarget),
    ShiftFocus(KeyboardFocusTarget),
}

impl MoveResult {
    pub fn or_else<F>(self, f: F) -> MoveResult
    where
        F: FnOnce() -> MoveResult,
    {
        match self {
            MoveResult::None => f(),
            x => x,
        }
    }
}

impl Workspace {
    pub fn new(
        handle: WorkspaceHandle,
        output: Output,
        tiling_enabled: bool,
        theme: cosmic::Theme,
    ) -> Workspace {
        let tiling_layer = TilingLayout::new(theme.clone(), &output);
        let floating_layer = FloatingLayout::new(theme, &output);
        let output_match = output_match_for_output(&output);

        Workspace {
            output,
            tiling_layer,
            floating_layer,
            tiling_enabled,
            minimized_windows: Vec::new(),
            fullscreen: None,
            pinned: false,
            handle,
            focus_stack: FocusStacks::default(),
            screencopy: ScreencopySessions::default(),
            output_stack: {
                let mut queue = VecDeque::new();
                queue.push_back(output_match);
                queue
            },
            backdrop_id: Id::new(),
            dirty: AtomicBool::new(false),
        }
    }

    pub fn from_pinned(
        pinned: &PinnedWorkspace,
        handle: WorkspaceHandle,
        output: Output,
        theme: cosmic::Theme,
    ) -> Self {
        let tiling_layer = TilingLayout::new(theme.clone(), &output);
        let floating_layer = FloatingLayout::new(theme, &output);
        let output_match = output_match_for_output(&output);

        Workspace {
            output,
            tiling_layer,
            floating_layer,
            tiling_enabled: pinned.tiling_enabled,
            minimized_windows: Vec::new(),
            fullscreen: None,
            pinned: true,
            handle,
            focus_stack: FocusStacks::default(),
            screencopy: ScreencopySessions::default(),
            output_stack: {
                let mut queue = VecDeque::new();
                queue.push_back(pinned.output.clone());
                if output_match != pinned.output {
                    queue.push_back(output_match);
                }
                queue
            },
            backdrop_id: Id::new(),
            dirty: AtomicBool::new(false),
        }
    }

    pub fn to_pinned(&self) -> Option<PinnedWorkspace> {
        let output = self.explicit_output().clone();
        if self.pinned {
            Some(PinnedWorkspace {
                output: cosmic_comp_config::workspace::OutputMatch {
                    name: output.name,
                    edid: output.edid,
                },
                tiling_enabled: self.tiling_enabled,
            })
        } else {
            None
        }
    }

    #[profiling::function]
    pub fn refresh(&mut self) {
        // TODO: `Option::take_if` once stabilitized
        if self.fullscreen.as_ref().is_some_and(|w| !w.alive()) {
            let _ = self.fullscreen.take();
        };

        self.floating_layer.refresh();
        self.tiling_layer.refresh();
    }

    fn has_activation_token(&self, xdg_activation_state: &XdgActivationState) -> bool {
        xdg_activation_state.tokens().any(|(_, data)| {
            if let ActivationContext::Workspace(handle) =
                data.user_data.get::<ActivationContext>().unwrap()
            {
                *handle == self.handle
                    && data.timestamp.elapsed() < super::ACTIVATION_TOKEN_EXPIRE_TIME
            } else {
                false
            }
        })
    }

    // Auto-removal of workspaces is allowed if empty, unless blocked by an
    // unused and unexpired activation token, or pinned.
    pub fn can_auto_remove(&self, xdg_activation_state: &XdgActivationState) -> bool {
        self.is_empty() && !self.has_activation_token(xdg_activation_state) && !self.pinned
    }

    pub fn refresh_focus_stack(&mut self) {
        let windows: Vec<CosmicMapped> = self.mapped().cloned().collect();
        for stack in self.focus_stack.0.values_mut() {
            stack.retain(|w| windows.contains(w));
        }
    }

    pub fn animations_going(&self) -> bool {
        self.tiling_layer.animations_going()
            || self.floating_layer.animations_going()
            || self
                .fullscreen
                .as_ref()
                .is_some_and(|f| f.start_at.is_some() || f.ended_at.is_some())
            || self.dirty.swap(false, Ordering::SeqCst)
    }

    pub fn update_animations(&mut self) -> HashMap<ClientId, Client> {
        let mut clients = HashMap::new();

        if let Some(f) = self.fullscreen.as_mut() {
            if let Some(start) = f.start_at.as_ref() {
                let duration_since = Instant::now().duration_since(*start);
                if duration_since > FULLSCREEN_ANIMATION_DURATION {
                    f.start_at.take();
                    self.dirty.store(true, Ordering::SeqCst);
                }
                if duration_since * 2 > FULLSCREEN_ANIMATION_DURATION {
                    if let Some(signal) = f.animation_signal.take() {
                        signal.store(true, Ordering::SeqCst);
                        if let Some(client) =
                            f.surface.wl_surface().as_deref().and_then(Resource::client)
                        {
                            clients.insert(client.id(), client);
                        }
                    }
                }
            }

            if let Some(end) = f.ended_at {
                let duration_since = Instant::now().duration_since(end);
                if duration_since * 2 > FULLSCREEN_ANIMATION_DURATION {
                    if let Some(signal) = f.animation_signal.take() {
                        signal.store(true, Ordering::SeqCst);
                        if let Some(client) =
                            f.surface.wl_surface().as_deref().and_then(Resource::client)
                        {
                            clients.insert(client.id(), client);
                        }
                    }
                }

                if duration_since >= FULLSCREEN_ANIMATION_DURATION {
                    let _ = self.fullscreen.take();
                    self.dirty.store(true, Ordering::SeqCst);
                }
            }
        }

        clients.extend(self.tiling_layer.update_animation_state());
        self.floating_layer.update_animation_state();
        clients
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    /// Output workspace was originally created on, or explicitly moved to by the user
    fn explicit_output(&self) -> &OutputMatch {
        self.output_stack.front().unwrap()
    }

    // Set output the workspace is on
    //
    // If `explicit` is `true`, the user has explicitly moved the workspace
    // to this output, so previous outputs it was on can be forgotten.
    pub fn set_output(&mut self, output: &Output, explicit: bool) {
        self.tiling_layer.set_output(output);
        self.floating_layer.set_output(output);
        for mapped in self.mapped() {
            for (surface, _) in mapped.windows() {
                toplevel_leave_output(&surface, &self.output);
                toplevel_enter_output(&surface, output);
            }
        }
        for window in self.minimized_windows.iter() {
            for (surface, _) in window.window.windows() {
                toplevel_leave_output(&surface, &self.output);
                toplevel_enter_output(&surface, output);
            }
        }
        if explicit {
            self.output_stack.clear();
        }
        if let Some(pos) = self
            .output_stack
            .iter()
            .position(|i| output_matches(i, output, true))
        {
            // Matched edid and connector name
            self.output_stack.truncate(pos + 1);
        } else if let Some(pos) = self
            .output_stack
            .iter()
            .position(|i| output_matches(i, output, false))
        {
            // Matched edid but not connector name; truncate entries that don't match edid,
            // but keep old entry in case we see two outputs with the same edid.
            self.output_stack.truncate(pos + 1);
            self.output_stack.push_back(output_match_for_output(output));
        } else {
            self.output_stack.push_back(output_match_for_output(output));
        }
        self.output = output.clone();
    }

    pub fn prefers_output(&self, output: &Output) -> bool {
        // Disambiguate match by connector name if existing output has same edid
        let disambiguate = output
            .edid()
            .is_some_and(|edid| self.output().edid() == Some(edid));
        self.output_stack
            .iter()
            .any(|i| output_matches(i, output, disambiguate))
    }

    pub fn unmap(&mut self, mapped: &CosmicMapped) -> Option<ManagedState> {
        let mut was_fullscreen = self
            .fullscreen
            .as_ref()
            .filter(|f| f.ended_at.is_none())
            .map(|f| mapped.windows().any(|(w, _)| w == f.surface))
            .unwrap_or(false)
            .then(|| self.fullscreen.take().unwrap());

        if mapped.maximized_state.lock().unwrap().is_some() {
            // If surface is maximized then unmaximize it, so it is assigned to only one layer
            let _ = self.unmaximize_request(mapped);
        }

        let mut was_floating = self.floating_layer.unmap(&mapped).is_some();
        let mut was_tiling = self.tiling_layer.unmap(&mapped);
        if was_floating || was_tiling {
            assert!(was_floating != was_tiling);
        }
        if let Some(pos) = self
            .minimized_windows
            .iter()
            .position(|m| &m.window == mapped)
        {
            let state = self.minimized_windows.remove(pos);
            state.window.set_minimized(false);
            match state.previous_state {
                MinimizedState::Sticky { .. } | MinimizedState::Floating { .. } => {
                    was_floating = true;
                }
                MinimizedState::Tiling { .. } => {
                    was_tiling = true;
                }
            }
            was_fullscreen = state.fullscreen;
        }

        self.focus_stack
            .0
            .values_mut()
            .for_each(|set| set.retain(|m| m != mapped));
        if was_floating {
            Some(ManagedState {
                layer: ManagedLayer::Floating,
                was_fullscreen,
            })
        } else if was_tiling {
            Some(ManagedState {
                layer: ManagedLayer::Tiling,
                was_fullscreen,
            })
        } else {
            None
        }
    }

    fn fullscreen_geometry(&self) -> Option<Rectangle<i32, Local>> {
        self.fullscreen.as_ref().map(|fullscreen| {
            let bbox = fullscreen.surface.bbox().as_local();

            let mut full_geo = Rectangle::from_size(self.output.geometry().size.as_local());
            if bbox != full_geo {
                if bbox.size.w < full_geo.size.w {
                    full_geo.loc.x += (full_geo.size.w - bbox.size.w) / 2;
                    full_geo.size.w = bbox.size.w;
                }
                if bbox.size.h < full_geo.size.h {
                    full_geo.loc.y += (full_geo.size.h - bbox.size.h) / 2;
                    full_geo.size.h = bbox.size.h;
                }
            }

            full_geo
        })
    }

    pub fn element_for_surface<S>(&self, surface: &S) -> Option<&CosmicMapped>
    where
        CosmicSurface: PartialEq<S>,
    {
        self.floating_layer
            .mapped()
            .chain(self.tiling_layer.mapped().map(|(w, _)| w))
            .chain(self.minimized_windows.iter().map(|w| &w.window))
            .find(|e| e.windows().any(|(w, _)| &w == surface))
    }

    pub fn popup_element_under(&self, location: Point<f64, Global>) -> Option<KeyboardFocusTarget> {
        if !self.output.geometry().contains(location.to_i32_round()) {
            return None;
        }
        let location = location.to_local(&self.output);

        if let Some(fullscreen) = self.fullscreen.as_ref() {
            if !fullscreen.is_animating() {
                let geometry = self.fullscreen_geometry().unwrap();
                return fullscreen
                    .surface
                    .0
                    .surface_under(
                        (location + geometry.loc.to_f64()).as_logical(),
                        WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE,
                    )
                    .is_some()
                    .then(|| KeyboardFocusTarget::Fullscreen(fullscreen.surface.clone()));
            }
        }

        self.floating_layer
            .popup_element_under(location)
            .or_else(|| self.tiling_layer.popup_element_under(location))
    }

    pub fn toplevel_element_under(
        &self,
        location: Point<f64, Global>,
    ) -> Option<KeyboardFocusTarget> {
        if !self.output.geometry().contains(location.to_i32_round()) {
            return None;
        }
        let location = location.to_local(&self.output);

        if let Some(fullscreen) = self.fullscreen.as_ref() {
            if !fullscreen.is_animating() {
                let geometry = self.fullscreen_geometry().unwrap();
                return fullscreen
                    .surface
                    .0
                    .surface_under(
                        (location + geometry.loc.to_f64()).as_logical(),
                        WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
                    )
                    .is_some()
                    .then(|| KeyboardFocusTarget::Fullscreen(fullscreen.surface.clone()));
            }
        }

        self.floating_layer
            .toplevel_element_under(location)
            .or_else(|| self.tiling_layer.toplevel_element_under(location))
    }

    pub fn popup_surface_under(
        &self,
        location: Point<f64, Global>,
        overview: OverviewMode,
    ) -> Option<(PointerFocusTarget, Point<f64, Global>)> {
        if !self.output.geometry().contains(location.to_i32_round()) {
            return None;
        }
        let location = location.to_local(&self.output);

        if let Some(fullscreen) = self.fullscreen.as_ref() {
            if !fullscreen.is_animating() {
                let geometry = self.fullscreen_geometry().unwrap();
                return fullscreen
                    .surface
                    .0
                    .surface_under(
                        (location + geometry.loc.to_f64()).as_logical(),
                        WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE,
                    )
                    .map(|(surface, surface_offset)| {
                        (
                            PointerFocusTarget::WlSurface {
                                surface,
                                toplevel: Some(fullscreen.surface.clone().into()),
                            },
                            (geometry.loc + surface_offset.as_local())
                                .to_global(&self.output)
                                .to_f64(),
                        )
                    });
            }
        }

        self.floating_layer
            .popup_surface_under(location)
            .or_else(|| self.tiling_layer.popup_surface_under(location, overview))
            .map(|(m, p)| (m, p.to_global(&self.output)))
    }

    pub fn toplevel_surface_under(
        &self,
        location: Point<f64, Global>,
        overview: OverviewMode,
    ) -> Option<(PointerFocusTarget, Point<f64, Global>)> {
        if !self.output.geometry().contains(location.to_i32_round()) {
            return None;
        }
        let location = location.to_local(&self.output);

        if let Some(fullscreen) = self.fullscreen.as_ref() {
            if !fullscreen.is_animating() {
                let geometry = self.fullscreen_geometry().unwrap();
                return fullscreen
                    .surface
                    .0
                    .surface_under(
                        (location + geometry.loc.to_f64()).as_logical(),
                        WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
                    )
                    .map(|(surface, surface_offset)| {
                        (
                            PointerFocusTarget::WlSurface {
                                surface,
                                toplevel: Some(fullscreen.surface.clone().into()),
                            },
                            (geometry.loc + surface_offset.as_local())
                                .to_global(&self.output)
                                .to_f64(),
                        )
                    });
            }
        }

        self.floating_layer
            .toplevel_surface_under(location)
            .or_else(|| self.tiling_layer.toplevel_surface_under(location, overview))
            .map(|(m, p)| (m, p.to_global(&self.output)))
    }

    pub fn update_pointer_position(
        &mut self,
        location: Option<Point<f64, Local>>,
        overview: OverviewMode,
    ) {
        self.floating_layer.update_pointer_position(location);
        self.tiling_layer
            .update_pointer_position(location, overview);
    }

    pub fn element_geometry(&self, elem: &CosmicMapped) -> Option<Rectangle<i32, Local>> {
        self.floating_layer
            .element_geometry(elem)
            .or_else(|| self.tiling_layer.element_geometry(elem))
    }

    pub fn recalculate(&mut self) {
        self.tiling_layer.recalculate();
        self.floating_layer.recalculate();
    }

    pub fn unmaximize_request(&mut self, elem: &CosmicMapped) -> Option<Size<i32, Logical>> {
        let mut state = elem.maximized_state.lock().unwrap();
        if let Some(state) = state.take() {
            if let Some(minimized) = self
                .minimized_windows
                .iter_mut()
                .find(|m| &m.window == elem)
            {
                minimized.unmaximize(state.original_geometry);
                Some(state.original_geometry.size.as_logical())
            } else {
                match state.original_layer {
                    ManagedLayer::Tiling if self.tiling_enabled => {
                        // should still be mapped in tiling
                        self.floating_layer.unmap(&elem);
                        elem.output_enter(&self.output, elem.bbox());
                        elem.set_maximized(false);
                        elem.set_geometry(state.original_geometry.to_global(&self.output));
                        elem.configure();
                        self.tiling_layer.recalculate();
                        self.tiling_layer
                            .element_geometry(&elem)
                            .map(|geo| geo.size.as_logical())
                    }
                    ManagedLayer::Sticky => unreachable!(),
                    _ => {
                        elem.set_maximized(false);
                        self.floating_layer.map_internal(
                            elem.clone(),
                            Some(state.original_geometry.loc),
                            Some(state.original_geometry.size.as_logical()),
                            None,
                        );
                        Some(state.original_geometry.size.as_logical())
                    }
                }
            }
        } else {
            None
        }
    }

    pub fn minimize(
        &mut self,
        elem: &CosmicMapped,
        to: Rectangle<i32, Local>,
    ) -> Option<MinimizedWindow> {
        let fullscreen = if self
            .get_fullscreen()
            .is_some_and(|s| elem.windows().any(|(w, _)| *s == w))
        {
            let fullscreen_state = self.fullscreen.clone().unwrap();
            {
                let f = self.fullscreen.as_mut().unwrap();
                f.ended_at = Some(
                    Instant::now()
                        - (FULLSCREEN_ANIMATION_DURATION
                            - f.start_at
                                .take()
                                .map(|earlier| {
                                    Instant::now()
                                        .duration_since(earlier)
                                        .min(FULLSCREEN_ANIMATION_DURATION)
                                })
                                .unwrap_or(FULLSCREEN_ANIMATION_DURATION)),
                );
            }
            Some(fullscreen_state)
        } else {
            None
        };

        if self.tiling_layer.mapped().any(|(m, _)| m == elem) {
            let was_maximized = self.floating_layer.unmap(&elem).is_some();
            let tiling_state = self.tiling_layer.unmap_minimize(elem, to);
            Some(MinimizedWindow {
                window: elem.clone(),
                previous_state: MinimizedState::Tiling {
                    tiling_state,
                    was_maximized,
                },
                output_geo: self.output.geometry(),
                fullscreen,
            })
        } else {
            self.floating_layer
                .unmap_minimize(elem, to)
                .map(|(window, position)| MinimizedWindow {
                    window,
                    previous_state: MinimizedState::Floating { position },
                    output_geo: self.output.geometry(),
                    fullscreen,
                })
        }
    }

    pub fn unminimize(
        &mut self,
        window: MinimizedWindow,
        from: Rectangle<i32, Local>,
        seat: &Seat<State>,
    ) -> Option<(CosmicMapped, ManagedLayer, WorkspaceHandle)> {
        match window.previous_state {
            MinimizedState::Floating { mut position } => {
                let current_output_size = self.output.geometry().size.as_logical();
                if current_output_size != window.output_geo.size.as_logical() {
                    position = Point::from((
                        (position.x as f64 / window.output_geo.size.w as f64
                            * current_output_size.w as f64)
                            .floor() as i32,
                        (position.y as f64 / window.output_geo.size.h as f64
                            * current_output_size.h as f64)
                            .floor() as i32,
                    ))
                };
                self.floating_layer
                    .remap_minimized(window.window, from, position);
            }
            MinimizedState::Sticky { .. } => unreachable!(),
            MinimizedState::Tiling {
                tiling_state,
                was_maximized,
            } => {
                if self.tiling_enabled {
                    let focus_stack = self.focus_stack.get(seat);
                    self.tiling_layer.remap_minimized(
                        window.window.clone(),
                        from,
                        tiling_state,
                        Some(focus_stack.iter()),
                    );
                    if was_maximized {
                        let previous_geometry =
                            self.tiling_layer.element_geometry(&window.window).unwrap();
                        self.floating_layer
                            .map_maximized(window.window, previous_geometry, true);
                    }
                } else {
                    if was_maximized {
                        self.floating_layer.map_maximized(window.window, from, true);
                    } else {
                        self.floating_layer.map(window.window.clone(), None);
                        // get the right animation
                        let geometry = self
                            .floating_layer
                            .element_geometry(&window.window)
                            .unwrap();
                        self.floating_layer.remap_minimized(
                            window.window.clone(),
                            from,
                            geometry.loc,
                        );
                    }
                }
            }
        }

        if let Some(mut fullscreen) = window.fullscreen {
            let old_fullscreen = self.remove_fullscreen();
            fullscreen.start_at = Some(Instant::now());
            let geo = self.output.geometry();
            if geo != window.output_geo {
                fullscreen.animation_signal = if let Some(surface) = fullscreen.surface.wl_surface()
                {
                    let signal = Arc::new(AtomicBool::new(false));
                    add_blocker(
                        &surface,
                        FullscreenBlocker {
                            signal: signal.clone(),
                        },
                    );
                    Some(signal)
                } else {
                    None
                };
                fullscreen.surface.set_geometry(geo, 0);
                fullscreen.surface.send_configure();
            }

            self.fullscreen = Some(fullscreen);
            old_fullscreen
        } else {
            None
        }
    }

    pub fn fullscreen_request(
        &mut self,
        window: &CosmicSurface,
        previously: Option<(ManagedLayer, WorkspaceHandle)>,
        from: Rectangle<i32, Local>,
        seat: &Seat<State>,
    ) {
        if self
            .fullscreen
            .as_ref()
            .filter(|f| f.ended_at.is_none())
            .is_some()
        {
            return;
        }

        if let Some(pos) = self
            .minimized_windows
            .iter()
            .position(|m| m.window.windows().any(|(w, _)| &w == window))
        {
            let minimized = self.minimized_windows.remove(pos);
            let _ = self.unminimize(minimized, from, seat);
        }

        window.set_fullscreen(true);
        let geo = self.output.geometry();
        let original_geometry = window.global_geometry().unwrap_or_default();
        let signal = if let Some(surface) = window.wl_surface() {
            let signal = Arc::new(AtomicBool::new(false));
            add_blocker(
                &surface,
                FullscreenBlocker {
                    signal: signal.clone(),
                },
            );
            Some(signal)
        } else {
            None
        };
        window.set_geometry(geo, 0);
        window.send_configure();

        self.fullscreen = Some(FullscreenSurface {
            surface: window.clone(),
            previously,
            original_geometry,
            start_at: Some(Instant::now()),
            ended_at: None,
            animation_signal: signal,
        });
    }

    #[must_use]
    pub fn unfullscreen_request(
        &mut self,
        window: &CosmicSurface,
    ) -> Option<(ManagedLayer, WorkspaceHandle)> {
        if let Some(minimized) = self
            .minimized_windows
            .iter_mut()
            .find(|m| m.fullscreen.as_ref().is_some_and(|f| &f.surface == window))
        {
            minimized.unfullscreen()
        } else if let Some(f) = self
            .fullscreen
            .as_mut()
            .filter(|f| &f.surface == window && f.ended_at.is_none())
        {
            window.set_fullscreen(false);
            window.set_geometry(f.original_geometry, 0);

            self.floating_layer.refresh();
            self.tiling_layer.recalculate();
            self.tiling_layer.refresh();

            let signal = if let Some(surface) = window.wl_surface() {
                let signal = Arc::new(AtomicBool::new(false));
                add_blocker(
                    &surface,
                    FullscreenBlocker {
                        signal: signal.clone(),
                    },
                );
                Some(signal)
            } else {
                None
            };
            window.send_configure();

            f.ended_at = Some(
                Instant::now()
                    - (FULLSCREEN_ANIMATION_DURATION
                        - f.start_at
                            .take()
                            .map(|earlier| {
                                Instant::now()
                                    .duration_since(earlier)
                                    .min(FULLSCREEN_ANIMATION_DURATION)
                            })
                            .unwrap_or(FULLSCREEN_ANIMATION_DURATION)),
            );
            if let Some(new_signal) = signal {
                if let Some(old_signal) = f.animation_signal.replace(new_signal) {
                    old_signal.store(true, Ordering::SeqCst);
                }
            }

            f.previously
        } else {
            None
        }
    }

    #[must_use]
    pub fn remove_fullscreen(&mut self) -> Option<(CosmicMapped, ManagedLayer, WorkspaceHandle)> {
        if let Some(surface) = self.fullscreen.as_ref().map(|f| f.surface.clone()) {
            self.unfullscreen_request(&surface)
                .map(|(l, h)| (self.element_for_surface(&surface).unwrap().clone(), l, h))
        } else {
            None
        }
    }

    /// Returns the content of the current display if it is alive and
    /// not in the process of rendering an animation
    pub fn get_fullscreen(&self) -> Option<&CosmicSurface> {
        self.fullscreen
            .as_ref()
            .filter(|f| f.alive())
            .filter(|f| f.ended_at.is_none())
            .map(|f| &f.surface)
    }

    pub fn resize(
        &mut self,
        focused: &KeyboardFocusTarget,
        direction: ResizeDirection,
        edge: ResizeEdge,
        amount: i32,
    ) -> bool {
        if let Some(toplevel) = focused.toplevel() {
            if self.fullscreen.as_ref().is_some_and(|f| {
                f.ended_at.is_none() && f.surface.wl_surface().as_deref() == Some(&toplevel)
            }) {
                return false;
            }
        }

        if !self.floating_layer.resize(focused, direction, edge, amount) {
            self.tiling_layer.resize(focused, direction, edge, amount)
        } else {
            true
        }
    }

    pub fn toggle_tiling(
        &mut self,
        seat: &Seat<State>,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) {
        self.set_tiling(!self.tiling_enabled, seat, workspace_state)
    }

    pub fn set_tiling(
        &mut self,
        tiling: bool,
        seat: &Seat<State>,
        workspace_state: &mut WorkspaceUpdateGuard<'_, State>,
    ) {
        let mut maximized_windows = Vec::new();
        if tiling {
            let floating_windows = self.floating_layer.mapped().cloned().collect::<Vec<_>>();

            for window in floating_windows.iter().filter(|w| w.is_maximized(false)) {
                let original_geometry = {
                    let state = window.maximized_state.lock().unwrap();
                    state.as_ref().unwrap().original_geometry.clone()
                };
                self.unmaximize_request(&window);
                maximized_windows.push((window.clone(), ManagedLayer::Tiling, original_geometry));
            }

            let focus_stack = self.focus_stack.get(seat);
            for window in floating_windows.into_iter() {
                self.floating_layer.unmap(&window);
                self.tiling_layer
                    .map(window, Some(focus_stack.iter()), None)
            }
            workspace_state.set_workspace_tiling_state(&self.handle, TilingState::TilingEnabled);
            self.tiling_enabled = true;
        } else {
            for window in self
                .tiling_layer
                .mapped()
                .map(|(m, _)| m.clone())
                .collect::<Vec<_>>()
                .into_iter()
            {
                if window.is_maximized(false) {
                    let original_geometry = {
                        let state = window.maximized_state.lock().unwrap();
                        state.as_ref().unwrap().original_geometry.clone()
                    };
                    self.unmaximize_request(&window);
                    maximized_windows.push((
                        window.clone(),
                        ManagedLayer::Floating,
                        original_geometry,
                    ));
                }
                self.tiling_layer.unmap(&window);
                self.floating_layer.map(window, None);
            }
            workspace_state.set_workspace_tiling_state(&self.handle, TilingState::FloatingOnly);
            self.tiling_enabled = false;
        }
        for (window, original_layer, original_geometry) in maximized_windows {
            let mut state = window.maximized_state.lock().unwrap();
            *state = Some(MaximizedState {
                original_geometry,
                original_layer,
            });
            std::mem::drop(state);

            self.floating_layer
                .map_maximized(window, original_geometry, false);
        }
    }

    pub fn toggle_floating_window(&mut self, seat: &Seat<State>, window: &CosmicMapped) {
        if self.tiling_enabled {
            if window.is_maximized(false) {
                self.unmaximize_request(window);
            }
            if self.tiling_layer.mapped().any(|(m, _)| m == window) {
                self.tiling_layer.unmap(window);
                self.floating_layer.map(window.clone(), None);
            } else if self.floating_layer.mapped().any(|w| w == window) {
                let focus_stack = self.focus_stack.get(seat);
                self.floating_layer.unmap(&window);
                self.tiling_layer
                    .map(window.clone(), Some(focus_stack.iter()), None)
            }
        }
    }

    pub fn toggle_floating_window_focused(&mut self, seat: &Seat<State>) {
        let maybe_window = self.focus_stack.get(seat).iter().next().cloned();
        if let Some(window) = maybe_window {
            self.toggle_floating_window(seat, &window);
        }
    }

    pub fn mapped(&self) -> impl Iterator<Item = &CosmicMapped> {
        self.floating_layer
            .mapped()
            .chain(self.tiling_layer.mapped().map(|(w, _)| w))
    }

    pub fn is_empty(&self) -> bool {
        self.floating_layer.mapped().next().is_none()
            && self.tiling_layer.mapped().next().is_none()
            && self.minimized_windows.is_empty()
    }

    pub fn is_fullscreen(&self, mapped: &CosmicMapped) -> bool {
        self.fullscreen
            .as_ref()
            .is_some_and(|f| f.ended_at.is_none() && f.surface == mapped.active_window())
            || self
                .minimized_windows
                .iter()
                .any(|m| &m.window == mapped && m.fullscreen.is_some())
    }

    pub fn is_floating(&self, mapped: &CosmicMapped) -> bool {
        !self.is_fullscreen(mapped)
            && (self.floating_layer.mapped().any(|m| m == mapped)
                || self.minimized_windows.iter().any(|m| {
                    &m.window == mapped
                        && matches!(
                            m.previous_state,
                            MinimizedState::Floating { .. } | MinimizedState::Sticky { .. }
                        )
                }))
    }

    pub fn is_tiled(&self, mapped: &CosmicMapped) -> bool {
        !self.is_fullscreen(mapped)
            && (self.tiling_layer.mapped().any(|(m, _)| m == mapped)
                || self.minimized_windows.iter().any(|m| {
                    &m.window == mapped && matches!(m.previous_state, MinimizedState::Tiling { .. })
                }))
    }

    pub fn node_desc(&self, focus: KeyboardFocusTarget) -> Option<NodeDesc> {
        match focus {
            KeyboardFocusTarget::Element(mapped) => {
                if mapped.is_maximized(false) {
                    return None;
                }
                self.tiling_layer.mapped().find_map(|(m, _)| {
                    if m == &mapped {
                        mapped
                            .tiling_node_id
                            .lock()
                            .unwrap()
                            .clone()
                            .map(|node_id| NodeDesc {
                                handle: self.handle.clone(),
                                node: node_id.clone(),
                                stack_window: if mapped
                                    .stack_ref()
                                    .map(|stack| !stack.whole_stack_focused())
                                    .unwrap_or(false)
                                {
                                    Some(mapped.active_window())
                                } else {
                                    None
                                },
                                focus_stack: vec![node_id],
                            })
                    } else {
                        None
                    }
                })
            }
            KeyboardFocusTarget::Group(WindowGroup {
                node, focus_stack, ..
            }) => Some(NodeDesc {
                handle: self.handle.clone(),
                node,
                stack_window: None,
                focus_stack,
            }),
            _ => None,
        }
    }

    #[profiling::function]
    pub fn render<'a, R>(
        &self,
        renderer: &mut R,
        draw_focus_indicator: Option<&Seat<State>>,
        overview: (OverviewMode, Option<(SwapIndicator, Option<&Tree<Data>>)>),
        resize_indicator: Option<(ResizeMode, ResizeIndicator)>,
        indicator_thickness: u8,
        theme: &CosmicTheme,
    ) -> Result<Vec<WorkspaceRenderElement<R>>, OutputNotMapped>
    where
        R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
        R::TextureId: Send + Clone + 'static,
        CosmicMappedRenderElement<R>: RenderElement<R>,
        CosmicWindowRenderElement<R>: RenderElement<R>,
        CosmicStackRenderElement<R>: RenderElement<R>,
        WorkspaceRenderElement<R>: RenderElement<R>,
    {
        let mut elements = Vec::default();

        let output_scale = self.output.current_scale().fractional_scale();
        let zone = {
            let layer_map = layer_map_for_output(&self.output);
            layer_map.non_exclusive_zone().as_local()
        };

        if let Some(fullscreen) = self.fullscreen.as_ref() {
            // fullscreen window
            let bbox = fullscreen.surface.bbox().as_local();
            let element_geo = Rectangle::new(
                self.element_for_surface(&fullscreen.surface)
                    .and_then(|elem| {
                        self.floating_layer
                            .element_geometry(elem)
                            .or_else(|| self.tiling_layer.element_geometry(elem))
                            .map(|mut geo| {
                                geo.loc -= elem.geometry().loc.as_local();
                                geo
                            })
                    })
                    .unwrap_or(bbox)
                    .loc,
                fullscreen.original_geometry.size.as_local(),
            );

            let mut full_geo = Rectangle::from_size(self.output.geometry().size.as_local());
            if fullscreen.start_at.is_none() {
                if bbox != full_geo {
                    if bbox.size.w < full_geo.size.w {
                        full_geo.loc.x += (full_geo.size.w - bbox.size.w) / 2;
                        full_geo.size.w = bbox.size.w;
                    }
                    if bbox.size.h < full_geo.size.h {
                        full_geo.loc.y += (full_geo.size.h - bbox.size.h) / 2;
                        full_geo.size.h = bbox.size.h;
                    }
                }
            }

            let (target_geo, alpha) = match (fullscreen.start_at, fullscreen.ended_at) {
                (Some(started), _) => {
                    let duration = Instant::now().duration_since(started).as_secs_f64()
                        / FULLSCREEN_ANIMATION_DURATION.as_secs_f64();
                    (
                        ease(
                            EaseInOutCubic,
                            EaseRectangle(element_geo),
                            EaseRectangle(full_geo),
                            duration,
                        )
                        .0,
                        ease(EaseInOutCubic, 0.0, 1.0, duration),
                    )
                }
                (_, Some(ended)) => {
                    let duration = Instant::now().duration_since(ended).as_secs_f64()
                        / FULLSCREEN_ANIMATION_DURATION.as_secs_f64();
                    (
                        ease(
                            EaseInOutCubic,
                            EaseRectangle(full_geo),
                            EaseRectangle(element_geo),
                            duration,
                        )
                        .0,
                        ease(EaseInOutCubic, 1.0, 0.0, duration),
                    )
                }
                (None, None) => (full_geo, 1.0),
            };

            let render_loc = target_geo
                .loc
                .as_logical()
                .to_physical_precise_round(output_scale);
            let scale = Scale {
                x: target_geo.size.w as f64 / bbox.size.w as f64,
                y: target_geo.size.h as f64 / bbox.size.h as f64,
            };

            elements.extend(
                fullscreen
                    .surface
                    .render_elements::<R, CosmicWindowRenderElement<R>>(
                        renderer,
                        render_loc,
                        output_scale.into(),
                        alpha,
                    )
                    .into_iter()
                    .map(|elem| RescaleRenderElement::from_element(elem, render_loc, scale))
                    .map(Into::into),
            );
        }

        if self
            .fullscreen
            .as_ref()
            .map(|f| f.start_at.is_some() || f.ended_at.is_some())
            .unwrap_or(true)
        {
            let focused = draw_focus_indicator
                .filter(|_| !self.fullscreen.is_some())
                .and_then(|seat| self.focus_stack.get(seat).last().cloned());

            // floating surfaces
            let alpha = match &overview.0 {
                OverviewMode::Started(_, started) => {
                    (1.0 - (Instant::now().duration_since(*started).as_millis()
                        / ANIMATION_DURATION.as_millis()) as f32)
                        .max(0.0)
                        * 0.4
                        + 0.6
                }
                OverviewMode::Ended(_, ended) => {
                    ((Instant::now().duration_since(*ended).as_millis()
                        / ANIMATION_DURATION.as_millis()) as f32)
                        * 0.4
                        + 0.6
                }
                OverviewMode::Active(_) => 0.6,
                OverviewMode::None => 1.0,
            };

            elements.extend(
                self.floating_layer
                    .render::<R>(
                        renderer,
                        focused.as_ref(),
                        resize_indicator.clone(),
                        indicator_thickness,
                        alpha,
                        theme,
                    )
                    .into_iter()
                    .map(WorkspaceRenderElement::from),
            );

            let alpha = match &overview.0 {
                OverviewMode::Started(_, start) => Some(
                    (Instant::now().duration_since(*start).as_millis() as f64 / 100.0).min(1.0)
                        as f32,
                ),
                OverviewMode::Active(_) => Some(1.0),
                OverviewMode::Ended(_, ended) => Some(
                    1.0 - (Instant::now().duration_since(*ended).as_millis() as f64 / 100.0)
                        .min(1.0) as f32,
                ),
                OverviewMode::None => None,
            };

            //tiling surfaces
            elements.extend(
                self.tiling_layer
                    .render::<R>(
                        renderer,
                        draw_focus_indicator,
                        zone,
                        overview,
                        resize_indicator,
                        indicator_thickness,
                        theme,
                    )?
                    .into_iter()
                    .map(WorkspaceRenderElement::from),
            );

            if let Some(alpha) = alpha {
                elements.push(
                    Into::<CosmicMappedRenderElement<R>>::into(BackdropShader::element(
                        renderer,
                        self.backdrop_id.clone(),
                        Rectangle::from_size(self.output.geometry().size.as_local()),
                        0.,
                        alpha * 0.85,
                        [0.0, 0.0, 0.0],
                    ))
                    .into(),
                )
            }
        }

        Ok(elements)
    }

    #[profiling::function]
    pub fn render_popups<'a, R>(
        &self,
        renderer: &mut R,
        draw_focus_indicator: Option<&Seat<State>>,
        overview: (OverviewMode, Option<(SwapIndicator, Option<&Tree<Data>>)>),
        theme: &CosmicTheme,
    ) -> Result<Vec<WorkspaceRenderElement<R>>, OutputNotMapped>
    where
        R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
        R::TextureId: Send + Clone + 'static,
        CosmicMappedRenderElement<R>: RenderElement<R>,
        CosmicWindowRenderElement<R>: RenderElement<R>,
        CosmicStackRenderElement<R>: RenderElement<R>,
        WorkspaceRenderElement<R>: RenderElement<R>,
    {
        let mut elements = Vec::default();

        let output_scale = self.output.current_scale().fractional_scale();
        let zone = {
            let layer_map = layer_map_for_output(&self.output);
            layer_map.non_exclusive_zone().as_local()
        };

        if let Some(fullscreen) = self.fullscreen.as_ref() {
            // fullscreen window
            let bbox = fullscreen.surface.bbox().as_local();
            let element_geo = Rectangle::new(
                self.element_for_surface(&fullscreen.surface)
                    .and_then(|elem| {
                        self.floating_layer
                            .element_geometry(elem)
                            .or_else(|| self.tiling_layer.element_geometry(elem))
                            .map(|mut geo| {
                                geo.loc -= elem.geometry().loc.as_local();
                                geo
                            })
                    })
                    .unwrap_or(bbox)
                    .loc,
                fullscreen.original_geometry.size.as_local(),
            );

            let mut full_geo = Rectangle::from_size(self.output.geometry().size.as_local());
            if fullscreen.start_at.is_none() {
                if bbox != full_geo {
                    if bbox.size.w < full_geo.size.w {
                        full_geo.loc.x += (full_geo.size.w - bbox.size.w) / 2;
                        full_geo.size.w = bbox.size.w;
                    }
                    if bbox.size.h < full_geo.size.h {
                        full_geo.loc.y += (full_geo.size.h - bbox.size.h) / 2;
                        full_geo.size.h = bbox.size.h;
                    }
                }
            }

            let (target_geo, alpha) = match (fullscreen.start_at, fullscreen.ended_at) {
                (Some(started), _) => {
                    let duration = Instant::now().duration_since(started).as_secs_f64()
                        / FULLSCREEN_ANIMATION_DURATION.as_secs_f64();
                    (
                        ease(
                            EaseInOutCubic,
                            EaseRectangle(element_geo),
                            EaseRectangle(full_geo),
                            duration,
                        )
                        .0,
                        ease(EaseInOutCubic, 0.0, 1.0, duration),
                    )
                }
                (_, Some(ended)) => {
                    let duration = Instant::now().duration_since(ended).as_secs_f64()
                        / FULLSCREEN_ANIMATION_DURATION.as_secs_f64();
                    (
                        ease(
                            EaseInOutCubic,
                            EaseRectangle(full_geo),
                            EaseRectangle(element_geo),
                            duration,
                        )
                        .0,
                        ease(EaseInOutCubic, 1.0, 0.0, duration),
                    )
                }
                (None, None) => (full_geo, 1.0),
            };

            let render_loc = target_geo
                .loc
                .as_logical()
                .to_physical_precise_round(output_scale);

            elements.extend(
                fullscreen
                    .surface
                    .popup_render_elements::<R, CosmicWindowRenderElement<R>>(
                        renderer,
                        render_loc,
                        output_scale.into(),
                        alpha,
                    )
                    .into_iter()
                    .map(Into::into),
            );
        }

        if self
            .fullscreen
            .as_ref()
            .map(|f| f.start_at.is_some() || f.ended_at.is_some())
            .unwrap_or(true)
        {
            // floating surfaces
            let alpha = match &overview.0 {
                OverviewMode::Started(_, started) => {
                    (1.0 - (Instant::now().duration_since(*started).as_millis()
                        / ANIMATION_DURATION.as_millis()) as f32)
                        .max(0.0)
                        * 0.4
                        + 0.6
                }
                OverviewMode::Ended(_, ended) => {
                    ((Instant::now().duration_since(*ended).as_millis()
                        / ANIMATION_DURATION.as_millis()) as f32)
                        * 0.4
                        + 0.6
                }
                OverviewMode::Active(_) => 0.6,
                OverviewMode::None => 1.0,
            };

            elements.extend(
                self.floating_layer
                    .render_popups::<R>(renderer, alpha)
                    .into_iter()
                    .map(WorkspaceRenderElement::from),
            );

            //tiling surfaces
            elements.extend(
                self.tiling_layer
                    .render_popups::<R>(renderer, draw_focus_indicator, zone, overview, theme)?
                    .into_iter()
                    .map(WorkspaceRenderElement::from),
            );
        }

        Ok(elements)
    }
}

impl FocusStacks {
    pub fn get<'a>(&'a self, seat: &Seat<State>) -> FocusStack<'a> {
        FocusStack(self.0.get(seat))
    }

    pub fn get_mut<'a>(&'a mut self, seat: &Seat<State>) -> FocusStackMut<'a> {
        FocusStackMut(self.0.entry(seat.clone()).or_default())
    }
}

pub struct OutputNotMapped;

pub enum WorkspaceRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
    R::TextureId: 'static,
{
    OverrideRedirect(WaylandSurfaceRenderElement<R>),
    Fullscreen(RescaleRenderElement<CosmicWindowRenderElement<R>>),
    FullscreenPopup(CosmicWindowRenderElement<R>),
    Window(CosmicMappedRenderElement<R>),
    Backdrop(TextureRenderElement<GlesTexture>),
}

impl<R> Element for WorkspaceRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
    R::TextureId: 'static,
{
    fn id(&self) -> &smithay::backend::renderer::element::Id {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.id(),
            WorkspaceRenderElement::Fullscreen(elem) => elem.id(),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.id(),
            WorkspaceRenderElement::Window(elem) => elem.id(),
            WorkspaceRenderElement::Backdrop(elem) => elem.id(),
        }
    }

    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.current_commit(),
            WorkspaceRenderElement::Fullscreen(elem) => elem.current_commit(),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.current_commit(),
            WorkspaceRenderElement::Window(elem) => elem.current_commit(),
            WorkspaceRenderElement::Backdrop(elem) => elem.current_commit(),
        }
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.src(),
            WorkspaceRenderElement::Fullscreen(elem) => elem.src(),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.src(),
            WorkspaceRenderElement::Window(elem) => elem.src(),
            WorkspaceRenderElement::Backdrop(elem) => elem.src(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, smithay::utils::Physical> {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.geometry(scale),
            WorkspaceRenderElement::Fullscreen(elem) => elem.geometry(scale),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.geometry(scale),
            WorkspaceRenderElement::Window(elem) => elem.geometry(scale),
            WorkspaceRenderElement::Backdrop(elem) => elem.geometry(scale),
        }
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, smithay::utils::Physical> {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.location(scale),
            WorkspaceRenderElement::Fullscreen(elem) => elem.location(scale),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.location(scale),
            WorkspaceRenderElement::Window(elem) => elem.location(scale),
            WorkspaceRenderElement::Backdrop(elem) => elem.location(scale),
        }
    }

    fn transform(&self) -> smithay::utils::Transform {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.transform(),
            WorkspaceRenderElement::Fullscreen(elem) => elem.transform(),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.transform(),
            WorkspaceRenderElement::Window(elem) => elem.transform(),
            WorkspaceRenderElement::Backdrop(elem) => elem.transform(),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<smithay::backend::renderer::utils::CommitCounter>,
    ) -> DamageSet<i32, smithay::utils::Physical> {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.damage_since(scale, commit),
            WorkspaceRenderElement::Fullscreen(elem) => elem.damage_since(scale, commit),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.damage_since(scale, commit),
            WorkspaceRenderElement::Window(elem) => elem.damage_since(scale, commit),
            WorkspaceRenderElement::Backdrop(elem) => elem.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, smithay::utils::Physical> {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.opaque_regions(scale),
            WorkspaceRenderElement::Fullscreen(elem) => elem.opaque_regions(scale),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.opaque_regions(scale),
            WorkspaceRenderElement::Window(elem) => elem.opaque_regions(scale),
            WorkspaceRenderElement::Backdrop(elem) => elem.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.alpha(),
            WorkspaceRenderElement::Fullscreen(elem) => elem.alpha(),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.alpha(),
            WorkspaceRenderElement::Window(elem) => elem.alpha(),
            WorkspaceRenderElement::Backdrop(elem) => elem.alpha(),
        }
    }
}

impl<R> RenderElement<R> for WorkspaceRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
    R::TextureId: 'static,
    R::Error: FromGlesError,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, smithay::utils::Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
            WorkspaceRenderElement::Fullscreen(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
            WorkspaceRenderElement::FullscreenPopup(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
            WorkspaceRenderElement::Window(elem) => {
                elem.draw(frame, src, dst, damage, opaque_regions)
            }
            WorkspaceRenderElement::Backdrop(elem) => RenderElement::<GlowRenderer>::draw(
                elem,
                R::glow_frame_mut(frame),
                src,
                dst,
                damage,
                opaque_regions,
            )
            .map_err(FromGlesError::from_gles_error),
        }
    }

    fn underlying_storage(
        &self,
        renderer: &mut R,
    ) -> Option<smithay::backend::renderer::element::UnderlyingStorage> {
        match self {
            WorkspaceRenderElement::OverrideRedirect(elem) => elem.underlying_storage(renderer),
            WorkspaceRenderElement::Fullscreen(elem) => elem.underlying_storage(renderer),
            WorkspaceRenderElement::FullscreenPopup(elem) => elem.underlying_storage(renderer),
            WorkspaceRenderElement::Window(elem) => elem.underlying_storage(renderer),
            WorkspaceRenderElement::Backdrop(elem) => {
                elem.underlying_storage(renderer.glow_renderer_mut())
            }
        }
    }
}

impl<R> From<RescaleRenderElement<CosmicWindowRenderElement<R>>> for WorkspaceRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
    R::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    fn from(elem: RescaleRenderElement<CosmicWindowRenderElement<R>>) -> Self {
        WorkspaceRenderElement::Fullscreen(elem)
    }
}

impl<R> From<CosmicWindowRenderElement<R>> for WorkspaceRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
    R::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    fn from(elem: CosmicWindowRenderElement<R>) -> Self {
        WorkspaceRenderElement::FullscreenPopup(elem)
    }
}

impl<R> From<WaylandSurfaceRenderElement<R>> for WorkspaceRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
    R::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    fn from(elem: WaylandSurfaceRenderElement<R>) -> Self {
        WorkspaceRenderElement::OverrideRedirect(elem)
    }
}

impl<R> From<CosmicMappedRenderElement<R>> for WorkspaceRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
    R::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    fn from(elem: CosmicMappedRenderElement<R>) -> Self {
        WorkspaceRenderElement::Window(elem)
    }
}

impl<R> From<TextureRenderElement<GlesTexture>> for WorkspaceRenderElement<R>
where
    R: Renderer + ImportAll + ImportMem + AsGlowRenderer,
    R::TextureId: 'static,
    CosmicMappedRenderElement<R>: RenderElement<R>,
{
    fn from(elem: TextureRenderElement<GlesTexture>) -> Self {
        WorkspaceRenderElement::Backdrop(elem)
    }
}
